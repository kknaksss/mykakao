//! 과거 히스토리 import: 키회수 → 페이지 복호(사본) → chatLogs 행 읽기 → 우리 SQLite upsert.
//!
//! 원본은 **읽기 전용**. 복호는 **임시 사본**(작업 후 자동 삭제, RAII). 키·본문은 로그에 남기지 않는다.
//! chatLogs 실제 컬럼(OQ-3)은 **런타임 introspection** 으로 유연 매핑한다.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use rusqlite::types::ValueRef;
use rusqlite::Connection;

use crate::kakao::decrypt::{decrypt_database_with_wal, PAGE_SZ};
use crate::kakao::memkey::RawKey;
use crate::kakao::{read_encrypted_page1, RoomFile};
use crate::store::{MessageRow, Store};

/// 복호된 임시 평문 DB — drop 시 파일 삭제(사본 잔존 금지).
struct TempPlainDb {
    path: PathBuf,
}

impl TempPlainDb {
    fn create(bytes: &[u8]) -> Result<Self> {
        let dir = std::env::temp_dir().join("win_app_scratch");
        std::fs::create_dir_all(&dir)?;
        let name = format!("dec_{}.sqlite", unique_token());
        let path = dir.join(name);
        std::fs::write(&path, bytes)?;
        Ok(Self { path })
    }
}

impl Drop for TempPlainDb {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
        // rusqlite 가 남긴 -wal/-shm 도 정리.
        for ext in ["-wal", "-shm"] {
            let mut p = self.path.clone().into_os_string();
            p.push(ext);
            let _ = std::fs::remove_file(PathBuf::from(p));
        }
    }
}

fn unique_token() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static CTR: AtomicU64 = AtomicU64::new(0);
    let n = CTR.fetch_add(1, Ordering::Relaxed);
    let t = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{t:x}_{n:x}")
}

/// 원본 SQLCipher DB 를 raw key 로 복호해 임시 평문 SQLite 로 떨구고 연다.
fn decrypt_and_open(src: &Path, raw_key: &RawKey) -> Result<(TempPlainDb, Connection)> {
    let enc = std::fs::read(src).with_context(|| "원본 DB 읽기 실패")?;
    if enc.len() < PAGE_SZ {
        return Err(anyhow!("원본 DB 가 너무 작음"));
    }
    // 활발한 방은 최근 메시지가 -wal 에 상주(아직 main 으로 checkpoint 안 됨) → main+WAL 합쳐 복호.
    // 원본 -wal 은 읽기만.
    let mut wal_path = src.as_os_str().to_owned();
    wal_path.push("-wal");
    let wal = std::fs::read(PathBuf::from(&wal_path)).ok();
    let plain = decrypt_database_with_wal(&enc, wal.as_deref(), raw_key)
        .context("페이지 복호 실패")?;
    let tmp = TempPlainDb::create(&plain)?;
    let conn = Connection::open(&tmp.path).context("평문 사본 열기 실패")?;
    Ok((tmp, conn))
}

/// 대상 DB 의 키를 후보에서 찾아 복호·오픈한 뒤 `f` 에 Connection 을 넘긴다.
/// 키 없음/복호 실패면 `None`. 임시 사본은 `f` 반환 후 RAII 삭제.
pub fn with_decrypted<T>(
    path: &Path,
    candidates: &HashSet<RawKey>,
    f: impl FnOnce(&Connection) -> T,
) -> Option<T> {
    let page1 = read_encrypted_page1(path)?;
    let key = crate::kakao::memkey::find_key_for(candidates, &page1)?;
    let (_tmp, conn) = decrypt_and_open(path, &key).ok()?;
    Some(f(&conn))
}

pub(crate) fn table_names(conn: &Connection) -> Result<Vec<String>> {
    let mut stmt =
        conn.prepare("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name")?;
    let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
    Ok(rows.filter_map(Result::ok).collect())
}

pub(crate) fn column_names(conn: &Connection, table: &str) -> Result<Vec<String>> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info(\"{}\")", table.replace('"', "")))?;
    let rows = stmt.query_map([], |r| r.get::<_, String>(1))?;
    Ok(rows.filter_map(Result::ok).collect())
}

/// 후보 이름들 중 실제 컬럼(대소문자 무시 정확 일치)을 우선순위대로 찾는다.
fn pick<'a>(cols: &'a [String], candidates: &[&str]) -> Option<&'a str> {
    for cand in candidates {
        if let Some(c) = cols.iter().find(|c| c.eq_ignore_ascii_case(cand)) {
            return Some(c.as_str());
        }
    }
    None
}

fn q(col: &str) -> String {
    format!("\"{}\"", col.replace('"', ""))
}

fn as_i64(v: ValueRef<'_>) -> i64 {
    match v {
        ValueRef::Integer(i) => i,
        ValueRef::Real(r) => r as i64,
        ValueRef::Text(t) => std::str::from_utf8(t)
            .ok()
            .and_then(|s| s.trim().parse::<i64>().ok())
            .unwrap_or(0),
        _ => 0,
    }
}

fn as_text(v: ValueRef<'_>) -> Option<String> {
    match v {
        ValueRef::Text(t) => Some(String::from_utf8_lossy(t).into_owned()),
        ValueRef::Blob(b) => Some(String::from_utf8_lossy(b).into_owned()),
        ValueRef::Integer(i) => Some(i.to_string()),
        ValueRef::Real(r) => Some(r.to_string()),
        ValueRef::Null => None,
    }
}

/// 복호된 DB 에서 `chatLogs` 행을 읽는다(logId > after). 컬럼은 introspection 매핑.
fn read_chatlogs(conn: &Connection, after: i64) -> Result<Vec<MessageRow>> {
    let tables = table_names(conn)?;
    let table = tables
        .iter()
        .find(|t| t.eq_ignore_ascii_case("chatLogs"))
        .or_else(|| tables.iter().find(|t| t.to_lowercase().contains("chatlog")))
        .cloned()
        .ok_or_else(|| anyhow!("chatLogs 테이블 없음(테이블 {}개)", tables.len()))?;

    let cols = column_names(conn, &table)?;
    let log_col = pick(&cols, &["logId", "_id", "id"])
        .ok_or_else(|| anyhow!("logId 컬럼 매핑 실패"))?;
    let log_expr = q(log_col);
    let auth_expr = pick(&cols, &["userId", "authorId", "author_id", "user_id"])
        .map_or_else(|| "NULL".to_string(), q);
    let type_expr = pick(&cols, &["type", "msg_type"]).map_or_else(|| "NULL".to_string(), q);
    let sent_expr = pick(&cols, &["sendAt", "createdAt", "sent_at", "created_at", "date"])
        .map_or_else(|| "NULL".to_string(), q);
    let text_expr = pick(&cols, &["message", "text", "content"]).map_or_else(|| "NULL".to_string(), q);

    let sql = format!(
        "SELECT {log} AS log_id, {auth} AS author_id, {typ} AS msg_type, \
         {sent} AS sent_at, {text} AS text FROM \"{tbl}\" \
         WHERE {log} > ?1 ORDER BY {log} ASC",
        log = log_expr,
        auth = auth_expr,
        typ = type_expr,
        sent = sent_expr,
        text = text_expr,
        tbl = table.replace('"', "")
    );

    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map([after], |r| {
        Ok(MessageRow {
            log_id: as_i64(r.get_ref(0)?),
            author_id: as_i64(r.get_ref(1)?),
            author_name: None,
            msg_type: as_i64(r.get_ref(2)?),
            sent_at: as_i64(r.get_ref(3)?),
            text: as_text(r.get_ref(4)?),
            kind: None,
            media_status: None,
        })
    })?;
    Ok(rows.filter_map(Result::ok).collect())
}

/// 임의 DB 파일의 raw key 해석(캐시 우선). talkmedia 등 방 아닌 DB 용.
#[must_use]
pub fn resolve_key_for(path: &Path, candidates: &HashSet<RawKey>, cached: Option<RawKey>) -> Option<RawKey> {
    let page1 = read_encrypted_page1(path)?;
    if let Some(k) = cached {
        if crate::kakao::decrypt::verify_sqlcipher4(&page1, &k) {
            return Some(k);
        }
    }
    crate::kakao::memkey::find_key_for(candidates, &page1)
}

/// 알려진 key 로 DB 복호·오픈 후 `f` 실행(임시 사본 RAII).
pub fn with_key<T>(path: &Path, key: &RawKey, f: impl FnOnce(&Connection) -> T) -> Option<T> {
    let (_tmp, conn) = decrypt_and_open(path, key).ok()?;
    Some(f(&conn))
}

/// 한 방 import 결과.
pub struct ImportResult {
    pub chat_id: i64,
    /// 새로 upsert 되어 변경된 행 수.
    pub imported: usize,
    /// 이번 델타로 읽힌 행들(커서 이후). P2 SSE push 원천.
    pub rows: Vec<MessageRow>,
}

/// 방의 raw key 를 해석한다. **캐시된 키가 page1 에 유효하면 후보 스윕 없이 그대로 재사용**(폴링 저렴).
/// 캐시가 없거나 무효면 `allow_sweep` 일 때만 후보 스윕. (page1 1회 읽기.)
/// - `cached`: 이전에 이 방에서 찾은 키(있으면 검증만).
/// - `allow_sweep`: 열린 방 등, 후보 전체 스윕을 허용할지(닫힌 미수집 방은 false → 스윕 회피).
#[must_use]
pub fn resolve_room_key(
    room: &RoomFile,
    candidates: &HashSet<RawKey>,
    cached: Option<RawKey>,
    allow_sweep: bool,
) -> Option<RawKey> {
    let page1 = read_encrypted_page1(&room.path)?;
    if let Some(k) = cached {
        if crate::kakao::decrypt::verify_sqlcipher4(&page1, &k) {
            return Some(k);
        }
    }
    if allow_sweep {
        crate::kakao::memkey::find_key_for(candidates, &page1)
    } else {
        None
    }
}

/// 이미 해석된 키로 방 델타를 import 한다(커서 이후 행만): 복호 → 읽기 → upsert → 커서 전진.
/// 신규 0 행이면 `imported=0`(상태는 호출측이 판단).
pub fn import_room_with_key(store: &Store, room: &RoomFile, key: &RawKey) -> Result<ImportResult> {
    let after = store.cursor(room.chat_id)?;
    let (_tmp, conn) = decrypt_and_open(&room.path, key)?;
    let rows = read_chatlogs(&conn, after)?;

    let max_log = rows.iter().map(|m| m.log_id).max().unwrap_or(after);
    let inserted = store.upsert_messages(room.chat_id, &rows)?;
    store.upsert_room(room.chat_id, None, None, None)?;
    if max_log > after {
        store.set_cursor(room.chat_id, max_log)?;
    }
    // _tmp drop → 사본 삭제.
    Ok(ImportResult {
        chat_id: room.chat_id,
        imported: inserted,
        rows,
    })
}

/// 한 방을 import 한다: 후보에서 키 찾기 → 복호 → chatLogs 읽기 → upsert → 커서 전진.
///
/// 키를 못 찾으면(그 순간 DB 미개방) `Ok(None)` 반환(에러 아님).
pub fn import_room(
    store: &Store,
    room: &RoomFile,
    candidates: &HashSet<RawKey>,
) -> Result<Option<ImportResult>> {
    match resolve_room_key(room, candidates, None, true) {
        Some(key) => import_room_with_key(store, room, &key).map(Some),
        None => Ok(None),
    }
}

/// 방 종류 분류: type 문자열 + directChatMemberId 로 group/direct/memo 판정.
fn classify_kind(type_str: &str, direct_member: i64) -> &'static str {
    let t = type_str.to_lowercase();
    if t.contains("memo") {
        "memo"
    } else if direct_member != 0 || t.contains("direct") {
        "direct"
    } else {
        "group"
    }
}

/// `chatListInfo.edb`(chatRoomList)에서 방 제목·종류·멤버수를 실복호해 store 에 반영.
///
/// 실스키마(진단 확인): 테이블 `chatRoomList`, 컬럼 `chatId/type/chatRoomTitle/activeMembersCount/directChatMemberId`.
/// 1:1(direct) 방 제목이 비면 상대(directChatMemberId)의 닉네임(`nick_map`)으로 대체.
pub fn enrich_rooms(
    store: &Store,
    chat_list_path: &Path,
    candidates: &HashSet<RawKey>,
    nick_map: &HashMap<i64, String>,
) -> Result<usize> {
    let done = with_decrypted(chat_list_path, candidates, |conn| -> Result<usize> {
        let tables = table_names(conn)?;
        let table = tables
            .iter()
            .find(|t| t.eq_ignore_ascii_case("chatRoomList"))
            .or_else(|| {
                tables
                    .iter()
                    .find(|t| t.to_lowercase().contains("chatroom") || t.to_lowercase().contains("room"))
            })
            .cloned()
            .ok_or_else(|| anyhow!("chatRoomList 테이블 없음(테이블 {}개)", tables.len()))?;
        let cols = column_names(conn, &table)?;
        let id_col = pick(&cols, &["chatId", "id", "chat_id"])
            .ok_or_else(|| anyhow!("chatId 컬럼 없음"))?;
        let title_col = pick(&cols, &["chatRoomTitle", "title", "name", "displayName"]);
        let type_col = pick(&cols, &["type"]);
        let member_col = pick(&cols, &["activeMembersCount", "memberCount", "members", "memberCnt"]);
        let direct_col = pick(&cols, &["directChatMemberId"]);

        let sql = format!(
            "SELECT {id} AS chat_id, {title} AS title, {typ} AS rtype, \
             {mem} AS member_count, {dm} AS direct_member FROM \"{tbl}\"",
            id = q(id_col),
            title = title_col.map_or_else(|| "NULL".to_string(), q),
            typ = type_col.map_or_else(|| "NULL".to_string(), q),
            mem = member_col.map_or_else(|| "NULL".to_string(), q),
            dm = direct_col.map_or_else(|| "NULL".to_string(), q),
            tbl = table.replace('"', "")
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map([], |r| {
            Ok((
                as_i64(r.get_ref(0)?),
                as_text(r.get_ref(1)?),
                as_text(r.get_ref(2)?),
                match r.get_ref(3)? {
                    ValueRef::Null => None,
                    v => Some(as_i64(v)),
                },
                as_i64(r.get_ref(4)?),
            ))
        })?;
        let mut n = 0usize;
        for row in rows.filter_map(Result::ok) {
            let (chat_id, title, rtype, member, direct_member) = row;
            if chat_id == 0 {
                continue;
            }
            let kind = classify_kind(rtype.as_deref().unwrap_or(""), direct_member);
            let mut title = title.map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
            // 1:1 방 제목이 비면 상대 닉네임으로.
            if title.is_none() && direct_member != 0 {
                title = nick_map.get(&direct_member).cloned();
            }
            store.upsert_room(chat_id, title.as_deref(), member, Some(kind))?;
            n += 1;
        }
        Ok(n)
    });
    done.unwrap_or(Ok(0))
}

/// TalkUserDB 에서 userId→nickName 매핑을 best-effort 로 읽는다(스키마 불확실 → 실패 시 빈 맵).
///
/// 원본 읽기전용·임시 사본 RAII·키 비상주(P1 방식). 닉네임 **원값은 로그에 남기지 않는다**.
pub fn load_nick_map(talk_user_path: &Path, candidates: &HashSet<RawKey>) -> HashMap<i64, String> {
    with_decrypted(talk_user_path, candidates, |conn| {
        let mut map: HashMap<i64, String> = HashMap::new();
        let Ok(tables) = table_names(conn) else {
            return map;
        };
        // talkUser: userId → friendNickName(별칭) 우선, 없으면 nickName.
        if let Some(table) = tables
            .iter()
            .find(|t| t.eq_ignore_ascii_case("talkUser"))
            .or_else(|| {
                tables
                    .iter()
                    .find(|t| t.to_lowercase().contains("user") && !t.to_lowercase().contains("global"))
            })
            .cloned()
        {
            if let Ok(cols) = column_names(conn, &table) {
                let id_col = pick(&cols, &["userId", "id", "user_id"]);
                let nick_col = pick(&cols, &["nickName", "nick_name", "name", "nickname"]);
                let friend_col = pick(&cols, &["friendNickName"]);
                if let (Some(id_col), Some(nick_col)) = (id_col, nick_col) {
                    let sql = format!(
                        "SELECT {id} AS uid, {nick} AS nick, {fr} AS fr FROM \"{tbl}\"",
                        id = q(id_col),
                        nick = q(nick_col),
                        fr = friend_col.map_or_else(|| "NULL".to_string(), q),
                        tbl = table.replace('"', "")
                    );
                    if let Ok(mut stmt) = conn.prepare(&sql) {
                        if let Ok(rows) = stmt.query_map([], |r| {
                            Ok((
                                as_i64(r.get_ref(0)?),
                                as_text(r.get_ref(1)?),
                                as_text(r.get_ref(2)?),
                            ))
                        }) {
                            for row in rows.filter_map(std::result::Result::ok) {
                                let (uid, nick, fr) = row;
                                if uid == 0 {
                                    continue;
                                }
                                let name = fr
                                    .filter(|s| !s.trim().is_empty())
                                    .or(nick)
                                    .map(|s| s.trim().to_string())
                                    .filter(|s| !s.is_empty());
                                if let Some(name) = name {
                                    map.insert(uid, name);
                                }
                            }
                        }
                    }
                }
            }
        }
        // Profile: 내 userId → "나"(내 메시지 표시용, 이미 이름 있으면 유지).
        if let Some(ptbl) = tables.iter().find(|t| t.eq_ignore_ascii_case("Profile")).cloned() {
            if let Ok(cols) = column_names(conn, &ptbl) {
                if let Some(id_col) = pick(&cols, &["userId", "id"]) {
                    let sql = format!(
                        "SELECT {} AS uid FROM \"{}\" LIMIT 1",
                        q(id_col),
                        ptbl.replace('"', "")
                    );
                    if let Ok(uid) = conn.query_row(&sql, [], |r| Ok(as_i64(r.get_ref(0)?))) {
                        if uid != 0 {
                            map.entry(uid).or_insert_with(|| "나".to_string());
                        }
                    }
                }
            }
        }
        map
    })
    .unwrap_or_default()
}

/// 방의 author_id 들에 대해 nick_map 으로 닉네임을 채운다(있는 것만). 반환 = 채운 수.
pub fn apply_author_names(
    store: &Store,
    chat_id: i64,
    author_ids: &[i64],
    nick_map: &HashMap<i64, String>,
) -> Result<usize> {
    let mut n = 0usize;
    for aid in author_ids {
        if let Some(nick) = nick_map.get(aid) {
            store.upsert_author(chat_id, *aid, Some(nick))?;
            n += 1;
        }
    }
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{MessageRow, Store};

    fn mk(log_id: i64, aid: i64) -> MessageRow {
        MessageRow {
            log_id,
            author_id: aid,
            author_name: None,
            msg_type: 1,
            sent_at: 100 + log_id,
            text: Some("x".into()),
            kind: None,
            media_status: None,
        }
    }

    #[test]
    fn author_names_backfill_and_join() {
        let s = Store::open_in_memory().unwrap();
        s.upsert_messages(9, &[mk(1, 7), mk(2, 8), mk(3, 7)]).unwrap();

        let mut ids = s.distinct_author_ids(9).unwrap();
        ids.sort_unstable();
        assert_eq!(ids, vec![7, 8]);

        // nick_map 은 7 만 가짐 → 8 은 이름 없이 남는다.
        let mut nick = HashMap::new();
        nick.insert(7i64, "alice".to_string());
        let filled = apply_author_names(&s, 9, &ids, &nick).unwrap();
        assert_eq!(filled, 1);

        let rows = s.messages(9, 0, 10).unwrap();
        let a7 = rows.iter().find(|m| m.author_id == 7).unwrap();
        let a8 = rows.iter().find(|m| m.author_id == 8).unwrap();
        assert_eq!(a7.author_name.as_deref(), Some("alice"));
        assert_eq!(a8.author_name, None);
    }
}

/// TalkUserDB 에서 **본인 닉네임**을 best-effort 로 해석한다(값은 로그 금지, 표시용).
///
/// Profile(1행)의 userId=본인 → nickName(있으면). 비면 talkUser 에서 그 userId 의 nickName.
/// 못 구하면 `None`("-"). 원본 읽기전용·임시본 RAII·키 비상주.
/// 본인 프로필: (userId, nickname). userId 는 Profile 에서 신뢰 가능(닉네임 비어도).
/// nickname 은 Profile.nickname → 없으면 talkUser[userId] → 못 구하면 None.
pub fn my_profile(
    talk_user_path: &Path,
    candidates: &HashSet<RawKey>,
) -> Option<(i64, Option<String>)> {
    with_decrypted(talk_user_path, candidates, |conn| {
        let tables = table_names(conn).ok()?;
        let ptbl = tables.iter().find(|t| t.eq_ignore_ascii_case("Profile")).cloned()?;
        let pcols = column_names(conn, &ptbl).ok()?;
        let id_col = pick(&pcols, &["userId", "id"])?;
        let nick_col = pick(&pcols, &["nickname", "nickName", "name"]);
        let sql = format!(
            "SELECT {id} AS uid, {nick} AS nick FROM \"{tbl}\" LIMIT 1",
            id = q(id_col),
            nick = nick_col.map_or_else(|| "NULL".to_string(), q),
            tbl = ptbl.replace('"', "")
        );
        let (my_id, my_nick): (i64, Option<String>) = conn
            .query_row(&sql, [], |r| Ok((as_i64(r.get_ref(0)?), as_text(r.get_ref(1)?))))
            .ok()?;
        let nick = resolve_nick(conn, &tables, my_id, my_nick);
        Some((my_id, nick))
    })
    .flatten()
}

/// Profile.nickname → talkUser[userId].nickName 순으로 본인 닉네임 해석.
fn resolve_nick(
    conn: &Connection,
    tables: &[String],
    my_id: i64,
    my_nick: Option<String>,
) -> Option<String> {
    if let Some(n) = my_nick.as_ref() {
        let t = n.trim();
        if !t.is_empty() {
            return Some(t.to_string());
        }
    }
    {
        // Profile.nickname 이 비면 talkUser 에서 본인 userId 조회.
        if my_id != 0 {
            if let Some(utbl) = tables.iter().find(|t| t.eq_ignore_ascii_case("talkUser")).cloned() {
                if let Ok(ucols) = column_names(conn, &utbl) {
                    if let (Some(uid_col), Some(unick)) = (
                        pick(&ucols, &["userId", "id"]),
                        pick(&ucols, &["nickName", "nickname", "name"]),
                    ) {
                        let sql2 = format!(
                            "SELECT {nick} FROM \"{tbl}\" WHERE {id}=?1 LIMIT 1",
                            nick = q(unick),
                            tbl = utbl.replace('"', ""),
                            id = q(uid_col)
                        );
                        if let Ok(nick) =
                            conn.query_row(&sql2, [my_id], |r| Ok(as_text(r.get_ref(0)?)))
                        {
                            if let Some(n) = nick {
                                let t = n.trim();
                                if !t.is_empty() {
                                    return Some(t.to_string());
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    None
}

/// (호환) 본인 닉네임만.
pub fn my_nickname(talk_user_path: &Path, candidates: &HashSet<RawKey>) -> Option<String> {
    my_profile(talk_user_path, candidates).and_then(|(_, n)| n)
}
