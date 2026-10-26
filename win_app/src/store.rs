//! 우리 로컬 **평문 SQLite** 축적 저장소(room/message/author). SPEC-003 §Data Contract.
//!
//! 멱등 upsert(message PK=(chat_id,log_id))로 재-import 시 중복이 쌓이지 않는다.
//! 키·대화 본문은 이 DB 에만 로컬 저장하며 외부 전송은 없다(키는 어디에도 저장 안 함).

use std::path::Path;
use std::sync::Mutex;

use anyhow::Result;
use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;

/// 방 목록 응답 행(`/api/rooms`).
#[derive(Debug, Serialize)]
pub struct RoomRow {
    pub chat_id: i64,
    pub title: Option<String>,
    pub member_count: Option<i64>,
    /// 방 종류: "group"(단체) / "direct"(1:1) / "memo"(내 메모) / None(미상).
    pub kind: Option<String>,
    pub selected: bool,
    /// 수집 큐 상태: idle|collecting|waiting|done|error (None=idle).
    pub collect_status: Option<String>,
    /// 수집 완료 시 축적 행수.
    pub collected_rows: Option<i64>,
}

/// 메시지 응답 행(`/api/messages`). `type` 은 예약어라 rename.
/// i64 를 JSON 문자열로(2^53 초과 log_id/chat_id 정밀도 보존 — JS Number 손실 방지).
fn ser_i64_str<S: serde::Serializer>(v: &i64, s: S) -> Result<S::Ok, S::Error> {
    s.serialize_str(&v.to_string())
}

#[derive(Debug, Serialize, Clone)]
pub struct MessageRow {
    #[serde(serialize_with = "ser_i64_str")]
    pub log_id: i64,
    pub author_id: i64,
    pub author_name: Option<String>,
    #[serde(rename = "type")]
    pub msg_type: i64,
    pub sent_at: i64,
    pub text: Option<String>,
    /// WORK-008: 사진 메시지면 "photo"(미디어 행 존재 시). 텍스트면 None.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    /// WORK-008: 사진 미디어 상태 saved|lost|pending. 사진 아니면 None.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub media_status: Option<String>,
}

/// 축적 저장소(스레드 안전 래퍼).
pub struct Store {
    conn: Mutex<Connection>,
}

impl Store {
    /// DB 파일을 열고 스키마를 보장한다.
    pub fn open(path: &Path) -> Result<Self> {
        let conn = Connection::open(path)?;
        Self::from_conn(conn)
    }

    /// 인메모리(테스트).
    #[cfg(test)]
    pub fn open_in_memory() -> Result<Self> {
        Self::from_conn(Connection::open_in_memory()?)
    }

    fn from_conn(conn: Connection) -> Result<Self> {
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.execute_batch(SCHEMA)?;
        // 마이그레이션(중복이면 에러 무시).
        let _ = conn.execute("ALTER TABLE room ADD COLUMN kind TEXT", []);
        let _ = conn.execute("ALTER TABLE room ADD COLUMN collect_status TEXT", []);
        let _ = conn.execute("ALTER TABLE room ADD COLUMN collected_rows INTEGER", []);
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// 방 존재 보장 + (있으면) 제목/멤버수 갱신.
    pub fn upsert_room(
        &self,
        chat_id: i64,
        title: Option<&str>,
        member_count: Option<i64>,
        kind: Option<&str>,
    ) -> Result<()> {
        let c = self.lock();
        c.execute(
            "INSERT INTO room(chat_id, title, member_count, kind, updated_at)
             VALUES(?1, ?2, ?3, ?4, datetime('now'))
             ON CONFLICT(chat_id) DO UPDATE SET
               title=COALESCE(excluded.title, room.title),
               member_count=COALESCE(excluded.member_count, room.member_count),
               kind=COALESCE(excluded.kind, room.kind),
               updated_at=datetime('now')",
            params![chat_id, title, member_count, kind],
        )?;
        Ok(())
    }

    /// 수집 대상 방 설정: 주어진 집합만 selected=1, 나머지는 0. 반환 = 선택된 chat_id 정렬본.
    pub fn set_selected(&self, chat_ids: &[i64]) -> Result<Vec<i64>> {
        let mut c = self.lock();
        let tx = c.transaction()?;
        // 대상 방이 없으면 먼저 생성.
        for id in chat_ids {
            tx.execute(
                "INSERT INTO room(chat_id, updated_at) VALUES(?1, datetime('now'))
                 ON CONFLICT(chat_id) DO NOTHING",
                params![id],
            )?;
        }
        tx.execute("UPDATE room SET selected=0", [])?;
        for id in chat_ids {
            tx.execute(
                "UPDATE room SET selected=1, updated_at=datetime('now') WHERE chat_id=?1",
                params![id],
            )?;
        }
        tx.commit()?;
        let mut v = chat_ids.to_vec();
        v.sort_unstable();
        v.dedup();
        Ok(v)
    }

    /// 선택된 방 chat_id 목록.
    pub fn selected_ids(&self) -> Result<Vec<i64>> {
        let c = self.lock();
        let mut stmt = c.prepare("SELECT chat_id FROM room WHERE selected=1 ORDER BY chat_id")?;
        let rows = stmt.query_map([], |r| r.get::<_, i64>(0))?;
        Ok(rows.filter_map(Result::ok).collect())
    }

    /// 저장된 방 전체(제목/선택/멤버수).
    pub fn list_rooms(&self) -> Result<Vec<RoomRow>> {
        let c = self.lock();
        let mut stmt = c.prepare(
            "SELECT chat_id, title, member_count, kind, selected, collect_status, collected_rows \
             FROM room ORDER BY chat_id",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(RoomRow {
                chat_id: r.get(0)?,
                title: r.get(1)?,
                member_count: r.get(2)?,
                kind: r.get(3)?,
                selected: r.get::<_, i64>(4)? != 0,
                collect_status: r.get(5)?,
                collected_rows: r.get(6)?,
            })
        })?;
        Ok(rows.filter_map(Result::ok).collect())
    }

    /// 수집 큐 상태 갱신(+행수). rows=None 이면 행수 유지.
    /// 방의 현재 수집 상태(collect_status). 없으면 None.
    pub fn collect_status_of(&self, chat_id: i64) -> Result<Option<String>> {
        let c = self.lock();
        let v = c
            .query_row(
                "SELECT collect_status FROM room WHERE chat_id=?1",
                params![chat_id],
                |r| r.get::<_, Option<String>>(0),
            )
            .optional()?
            .flatten();
        Ok(v)
    }

    pub fn set_collect(&self, chat_id: i64, status: &str, rows: Option<i64>) -> Result<()> {
        let c = self.lock();
        match rows {
            Some(n) => c.execute(
                "UPDATE room SET collect_status=?2, collected_rows=?3, updated_at=datetime('now') WHERE chat_id=?1",
                params![chat_id, status, n],
            )?,
            None => c.execute(
                "UPDATE room SET collect_status=?2, updated_at=datetime('now') WHERE chat_id=?1",
                params![chat_id, status],
            )?,
        };
        Ok(())
    }

    /// 저장 시 큐 동기화: 추적 아닌 방은 상태 비움, 추적된 방 중 idle/error/NULL 은 'waiting' 으로 enqueue
    /// (collecting/done/waiting 은 유지 — 재저장으로 리셋 안 함).
    pub fn enqueue_tracked(&self, tracked: &[i64]) -> Result<()> {
        let mut c = self.lock();
        let tx = c.transaction()?;
        tx.execute(
            "UPDATE room SET collect_status=NULL, collected_rows=NULL WHERE selected=0",
            [],
        )?;
        for id in tracked {
            tx.execute(
                "UPDATE room SET collect_status='waiting' WHERE chat_id=?1 \
                 AND (collect_status IS NULL OR collect_status='idle' OR collect_status='error')",
                params![id],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// 아직 완료 안 된 추적 방(수집 대상): selected=1 AND collect_status IN (collecting,waiting) 또는 NULL.
    pub fn rooms_to_collect(&self) -> Result<Vec<i64>> {
        let c = self.lock();
        let mut stmt = c.prepare(
            "SELECT chat_id FROM room WHERE selected=1 \
             AND (collect_status IS NULL OR collect_status IN ('collecting','waiting','error')) \
             ORDER BY chat_id",
        )?;
        let rows = stmt.query_map([], |r| r.get::<_, i64>(0))?;
        Ok(rows.filter_map(Result::ok).collect())
    }

    /// 방의 마지막 동기화 커서(last_synced_id).
    pub fn cursor(&self, chat_id: i64) -> Result<i64> {
        let c = self.lock();
        let v: Option<i64> = c
            .query_row(
                "SELECT last_synced_id FROM room WHERE chat_id=?1",
                params![chat_id],
                |r| r.get(0),
            )
            .optional()?;
        Ok(v.unwrap_or(0))
    }

    /// 커서 갱신(전진만).
    pub fn set_cursor(&self, chat_id: i64, log_id: i64) -> Result<()> {
        let c = self.lock();
        c.execute(
            "INSERT INTO room(chat_id, last_synced_id, updated_at)
             VALUES(?1, ?2, datetime('now'))
             ON CONFLICT(chat_id) DO UPDATE SET
               last_synced_id=MAX(room.last_synced_id, excluded.last_synced_id),
               updated_at=datetime('now')",
            params![chat_id, log_id],
        )?;
        Ok(())
    }

    /// 메시지 배치 멱등 upsert. 반환 = 새로 삽입된(변경된) 행 수.
    pub fn upsert_messages(&self, chat_id: i64, rows: &[MessageRow]) -> Result<usize> {
        let mut c = self.lock();
        let tx = c.transaction()?;
        let mut n = 0usize;
        {
            let mut stmt = tx.prepare(
                "INSERT INTO message(chat_id, log_id, author_id, msg_type, sent_at, text)
                 VALUES(?1, ?2, ?3, ?4, ?5, ?6)
                 ON CONFLICT(chat_id, log_id) DO UPDATE SET
                   author_id=excluded.author_id, msg_type=excluded.msg_type,
                   sent_at=excluded.sent_at, text=excluded.text",
            )?;
            for m in rows {
                let changed = stmt.execute(params![
                    chat_id, m.log_id, m.author_id, m.msg_type, m.sent_at, m.text
                ])?;
                n += changed;
            }
        }
        tx.commit()?;
        Ok(n)
    }

    /// 작성자 닉네임 upsert.
    pub fn upsert_author(&self, chat_id: i64, author_id: i64, nickname: Option<&str>) -> Result<()> {
        let c = self.lock();
        c.execute(
            "INSERT INTO author(chat_id, author_id, nickname) VALUES(?1, ?2, ?3)
             ON CONFLICT(chat_id, author_id) DO UPDATE SET
               nickname=COALESCE(excluded.nickname, author.nickname)",
            params![chat_id, author_id, nickname],
        )?;
        Ok(())
    }

    /// 축적본 메시지 조회(`/api/messages`). author_name 은 author 조인.
    pub fn messages(&self, chat_id: i64, after: i64, limit: i64) -> Result<Vec<MessageRow>> {
        let c = self.lock();
        let mut stmt = c.prepare(
            "SELECT m.log_id, m.author_id, a.nickname, m.msg_type, m.sent_at, m.text
             FROM message m
             LEFT JOIN author a ON a.chat_id=m.chat_id AND a.author_id=m.author_id
             WHERE m.chat_id=?1 AND m.log_id>?2
             ORDER BY m.log_id ASC
             LIMIT ?3",
        )?;
        let rows = stmt.query_map(params![chat_id, after, limit], |r| {
            Ok(MessageRow {
                log_id: r.get(0)?,
                author_id: r.get(1)?,
                author_name: r.get(2)?,
                msg_type: r.get(3)?,
                sent_at: r.get(4)?,
                text: r.get(5)?,
                kind: None,
                media_status: None,
            })
        })?;
        Ok(rows.filter_map(Result::ok).collect())
    }

    // ── WORK-008: 미디어(사진) 상태 ──────────────────────────────
    /// 미디어 행 upsert(수집 상태·mime·저장파일명).
    pub fn upsert_media(
        &self,
        chat_id: i64,
        log_id: i64,
        mime: Option<&str>,
        status: &str,
        file: Option<&str>,
    ) -> Result<()> {
        let c = self.lock();
        c.execute(
            "INSERT INTO media(chat_id, log_id, mime, status, file, updated_at)
             VALUES(?1,?2,?3,?4,?5,datetime('now'))
             ON CONFLICT(chat_id, log_id) DO UPDATE SET
               mime=COALESCE(excluded.mime, media.mime),
               status=excluded.status,
               file=COALESCE(excluded.file, media.file),
               updated_at=datetime('now')",
            params![chat_id, log_id, mime, status, file],
        )?;
        Ok(())
    }

    /// 한 미디어 행: (mime, status, file).
    pub fn media_of(
        &self,
        chat_id: i64,
        log_id: i64,
    ) -> Result<Option<(Option<String>, String, Option<String>)>> {
        let c = self.lock();
        let v = c
            .query_row(
                "SELECT mime, status, file FROM media WHERE chat_id=?1 AND log_id=?2",
                params![chat_id, log_id],
                |r| Ok((r.get::<_, Option<String>>(0)?, r.get::<_, String>(1)?, r.get::<_, Option<String>>(2)?)),
            )
            .optional()?;
        Ok(v)
    }

    /// 방의 (log_id → status) 맵 — /api/messages 표식 부착용.
    pub fn media_status_map(&self, chat_id: i64) -> Result<std::collections::HashMap<i64, String>> {
        let c = self.lock();
        let mut stmt = c.prepare("SELECT log_id, status FROM media WHERE chat_id=?1")?;
        let rows = stmt.query_map(params![chat_id], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))?;
        Ok(rows.filter_map(Result::ok).collect())
    }

    /// 한 메시지 행(사진 확정 후 SSE 재전송용). kind/media_status 는 호출자가 붙인다.
    pub fn message_one(&self, chat_id: i64, log_id: i64) -> Result<Option<MessageRow>> {
        let c = self.lock();
        let v = c
            .query_row(
                "SELECT m.log_id, m.author_id, a.nickname, m.msg_type, m.sent_at, m.text
                 FROM message m
                 LEFT JOIN author a ON a.chat_id=m.chat_id AND a.author_id=m.author_id
                 WHERE m.chat_id=?1 AND m.log_id=?2",
                params![chat_id, log_id],
                |r| {
                    Ok(MessageRow {
                        log_id: r.get(0)?,
                        author_id: r.get(1)?,
                        author_name: r.get(2)?,
                        msg_type: r.get(3)?,
                        sent_at: r.get(4)?,
                        text: r.get(5)?,
                        kind: None,
                        media_status: None,
                    })
                },
            )
            .optional()?;
        Ok(v)
    }

    /// 방에 등장하는 서로 다른 author_id 목록(닉네임 조인 백필용).
    pub fn distinct_author_ids(&self, chat_id: i64) -> Result<Vec<i64>> {
        let c = self.lock();
        let mut stmt =
            c.prepare("SELECT DISTINCT author_id FROM message WHERE chat_id=?1")?;
        let rows = stmt.query_map(params![chat_id], |r| r.get::<_, i64>(0))?;
        Ok(rows.filter_map(Result::ok).collect())
    }

    /// 방의 저장 행수(진단/검증용).
    pub fn message_count(&self, chat_id: i64) -> Result<i64> {
        let c = self.lock();
        Ok(c.query_row(
            "SELECT count(*) FROM message WHERE chat_id=?1",
            params![chat_id],
            |r| r.get(0),
        )?)
    }
}

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS room(
  chat_id        INTEGER PRIMARY KEY,
  title          TEXT,
  member_count   INTEGER,
  kind           TEXT,
  selected       INTEGER DEFAULT 0,
  last_synced_id INTEGER DEFAULT 0,
  collect_status TEXT,
  collected_rows INTEGER,
  updated_at     TEXT
);
CREATE TABLE IF NOT EXISTS message(
  chat_id    INTEGER,
  log_id     INTEGER,
  author_id  INTEGER,
  msg_type   INTEGER,
  sent_at    INTEGER,
  text       TEXT,
  PRIMARY KEY(chat_id, log_id)
);
CREATE TABLE IF NOT EXISTS author(
  chat_id   INTEGER,
  author_id INTEGER,
  nickname  TEXT,
  PRIMARY KEY(chat_id, author_id)
);
CREATE TABLE IF NOT EXISTS media(
  chat_id    INTEGER,
  log_id     INTEGER,
  mime       TEXT,
  status     TEXT,
  file       TEXT,
  updated_at TEXT,
  PRIMARY KEY(chat_id, log_id)
);
CREATE INDEX IF NOT EXISTS idx_message_chat_log ON message(chat_id, log_id);
"#;

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(log_id: i64, text: &str) -> MessageRow {
        MessageRow {
            log_id,
            author_id: 7,
            author_name: None,
            msg_type: 1,
            sent_at: 1000 + log_id,
            text: Some(text.to_string()),
            kind: None,
            media_status: None,
        }
    }

    #[test]
    fn idempotent_upsert_and_query() {
        let s = Store::open_in_memory().unwrap();
        s.upsert_room(42, Some("room"), None, None).unwrap();
        let n1 = s
            .upsert_messages(42, &[msg(1, "a"), msg(2, "b"), msg(3, "c")])
            .unwrap();
        assert_eq!(n1, 3);
        // 재-import: 같은 log_id → 중복 안 쌓임(count 그대로).
        let _ = s.upsert_messages(42, &[msg(2, "b"), msg(3, "c")]).unwrap();
        assert_eq!(s.message_count(42).unwrap(), 3);

        let got = s.messages(42, 1, 100).unwrap();
        assert_eq!(got.len(), 2); // log_id > 1
        assert_eq!(got[0].log_id, 2);
    }

    #[test]
    fn selection_replaces_set() {
        let s = Store::open_in_memory().unwrap();
        let sel = s.set_selected(&[1, 2, 3]).unwrap();
        assert_eq!(sel, vec![1, 2, 3]);
        assert_eq!(s.selected_ids().unwrap(), vec![1, 2, 3]);
        // 새 집합으로 교체.
        s.set_selected(&[2]).unwrap();
        assert_eq!(s.selected_ids().unwrap(), vec![2]);
    }

    #[test]
    fn cursor_moves_forward_only() {
        let s = Store::open_in_memory().unwrap();
        s.set_cursor(9, 100).unwrap();
        s.set_cursor(9, 50).unwrap(); // 뒤로 못감
        assert_eq!(s.cursor(9).unwrap(), 100);
        s.set_cursor(9, 200).unwrap();
        assert_eq!(s.cursor(9).unwrap(), 200);
    }

    #[test]
    fn message_row_json_keys_match_contract() {
        // SSE(/api/stream)·/api/messages 페이로드 계약 키 고정.
        let m = MessageRow {
            log_id: 5,
            author_id: 7,
            author_name: Some("n".into()),
            msg_type: 1,
            sent_at: 123,
            text: Some("t".into()),
            kind: None,
            media_status: None,
        };
        let v: serde_json::Value = serde_json::to_value(&m).unwrap();
        let mut keys: Vec<&str> = v.as_object().unwrap().keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            vec!["author_id", "author_name", "log_id", "sent_at", "text", "type"]
        );
        // `type` 은 msg_type 의 rename.
        assert_eq!(v["type"], serde_json::json!(1));
    }

    #[test]
    fn author_join_in_messages() {
        let s = Store::open_in_memory().unwrap();
        s.upsert_messages(1, &[msg(10, "hi")]).unwrap();
        s.upsert_author(1, 7, Some("nick")).unwrap();
        let got = s.messages(1, 0, 10).unwrap();
        assert_eq!(got[0].author_name.as_deref(), Some("nick"));
    }

    #[test]
    fn message_one_exact_row_or_none() {
        let s = Store::open_in_memory().unwrap();
        s.upsert_messages(1, &[msg(10, "hi"), msg(11, "there")]).unwrap();
        s.upsert_author(1, 7, Some("nick")).unwrap();
        let got = s.message_one(1, 11).unwrap().expect("row 11");
        assert_eq!(got.log_id, 11);
        assert_eq!(got.text.as_deref(), Some("there"));
        assert_eq!(got.author_name.as_deref(), Some("nick"));
        // 다음 행으로 밀리지 않고 없는 log_id 는 None.
        assert!(s.message_one(1, 12).unwrap().is_none());
        assert!(s.message_one(2, 11).unwrap().is_none());
    }
}
