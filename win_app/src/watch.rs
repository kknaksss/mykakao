//! P2 실시간 축적 — 선택 방 `chat_data` 파일 감시 → 델타 복호 → SQLite append → SSE push.
//!
//! `notify`(내부 ReadDirectoryChangesW)로 `chat_data` 디렉토리를 감시하고,
//! `chatLogs_<chatId>.edb`(및 `-wal`/`-shm`) 변경 이벤트를 chat_id 로 디바운스 코얼레싱한다.
//! due 시 P1 import 로직(키회수→복호→커서 이후 행→upsert)을 **그대로 재사용**해 델타만 처리한다.
//!
//! 자원 규칙(상주 앱): 워처 핸들 1개(디렉토리 감시)로 선택 방만 처리(핸들 누적 없음),
//! 델타 작업은 직렬(중첩 방지), 임시 사본은 RAII 삭제(P1), 키는 이벤트마다 회수·폐기(비상주).
//! 종료 시 watcher drop → 콜백 채널 닫힘 → 처리 태스크 clean stop.

use std::collections::HashSet;
use std::time::Duration;

use notify::{RecursiveMode, Watcher};
use tokio::sync::mpsc;

use crate::server::{Shared, StreamEvent};

/// 감시 핸들 — drop 되면 감시 중단(watcher + 콜백 채널 정리). 서버 수명 동안 보관.
pub struct WatchHandle {
    _watcher: notify::RecommendedWatcher,
}

/// 파일명 `chatLogs_<chatId>.edb[-wal|-shm]` 에서 chatId 파싱.
#[must_use]
pub fn parse_chatlog_chat_id(file_name: &str) -> Option<i64> {
    let rest = file_name.strip_prefix("chatLogs_")?;
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    if digits.is_empty() {
        None
    } else {
        digits.parse::<i64>().ok()
    }
}

/// 계정 폴더의 로그인/로그아웃 신호 파일(chatListInfo/TalkUserDB)인지.
fn is_account_db(file_name: &str) -> bool {
    let f = file_name.to_lowercase();
    f.starts_with("talkuserdb.edb") || f.starts_with("chatlistinfo.edb")
}

/// 감시 시작. 계정 없으면 `None`(감시 없이 정상 동작).
pub fn spawn(state: Shared) -> Option<WatchHandle> {
    let acct = state.account.clone()?;
    if !acct.is_dir() {
        return None;
    }

    // chatLogs 변경 → chat_id 채널(델타). 계정 DB 변경 → login 채널(상태 재판정).
    let (tx_ev, rx_ev) = mpsc::channel::<i64>(1024);
    let (login_tx, login_rx) = mpsc::channel::<()>(64);

    let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        if let Ok(ev) = res {
            for path in ev.paths {
                if let Some(name) = path.file_name().and_then(|s| s.to_str()) {
                    if let Some(cid) = parse_chatlog_chat_id(name) {
                        let _ = tx_ev.try_send(cid);
                    } else if is_account_db(name) {
                        let _ = login_tx.try_send(());
                    }
                }
            }
        }
    })
    .ok()?;
    // 계정 폴더 전체를 재귀 감시(chat_data 하위 + 계정 DB).
    watcher.watch(&acct, RecursiveMode::Recursive).ok()?;

    tokio::spawn(process_loop(state.clone(), rx_ev));

    // WORK-006: 로그인 상태 엔진(3상태 감지 + 로그인 전이 시 자동 재조정). Windows 전용.
    #[cfg(windows)]
    tokio::spawn(crate::state::run(state.clone(), login_rx));
    #[cfg(not(windows))]
    let _ = login_rx;

    // 실시간 견고화(핵심): 추적 방 델타 폴링을 **기본 ON(3s)**. notify(ReadDirectoryChangesW)는
    // KakaoTalk 의 -wal in-place 쓰기를 놓칠 수 있어(메모리맵/flush 타이밍) 폴링으로 보정한다.
    // 폴링은 방별 키 캐시로 저렴(전체 harvest 아님) + 델타 없으면 상태 무변(플리커 없음).
    // WIN_APP_RESYNC_SECS 로 주기 조정(0=끔).
    let resync_secs = std::env::var("WIN_APP_RESYNC_SECS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(3);
    if resync_secs > 0 {
        tokio::spawn(resync_loop(state, Duration::from_secs(resync_secs)));
        crate::applog::line(&format!("[win_app] 실시간 델타 폴링 {resync_secs}s 간격 활성"));
    }

    crate::applog::line("[win_app] 파일 감시 시작 — 선택 방 델타 실시간 축적");
    Some(WatchHandle { _watcher: watcher })
}

/// 이벤트 디바운스 루프. 이벤트가 700ms 잠잠해지면 모아서 한 번 처리.
async fn process_loop(state: Shared, mut rx: mpsc::Receiver<i64>) {
    let debounce = Duration::from_millis(700);
    let mut pending: HashSet<i64> = HashSet::new();
    loop {
        tokio::select! {
            maybe = rx.recv() => match maybe {
                Some(cid) => { pending.insert(cid); }
                None => break, // 콜백 채널 닫힘(watcher drop) → 종료.
            },
            _ = tokio::time::sleep(debounce), if !pending.is_empty() => {
                let batch: Vec<i64> = pending.drain().collect();
                let st = state.clone();
                // 델타 작업(회수+복호)은 blocking·직렬 처리(중첩 방지 → CPU/임시본 폭증 방지).
                let _ = tokio::task::spawn_blocking(move || process_delta(&st, &batch)).await;
            }
        }
    }
}

/// 저빈도 재동기: 선택된 방 전체를 주기적으로 델타 처리(감시 누락 보정).
async fn resync_loop(state: Shared, period: Duration) {
    let mut tick = tokio::time::interval(period);
    tick.tick().await; // 즉시 1틱 소비(시작 직후 실행 방지).
    loop {
        tick.tick().await;
        let selected = match state.store.selected_ids() {
            Ok(s) => s,
            Err(_) => continue,
        };
        if selected.is_empty() {
            continue;
        }
        let st = state.clone();
        let _ = tokio::task::spawn_blocking(move || process_delta(&st, &selected)).await;
    }
}

/// 델타 처리(blocking): 대상 = 이벤트 chat_id ∩ 선택 방. **세션 키캐시** 재사용(harvest 반복 회피)
/// → 방별 델타 복호 → SSE push. (WORK-006 재조정도 이 함수를 selected 전체로 호출해 재사용.)
pub(crate) fn process_delta(state: &Shared, chat_ids: &[i64]) {
    let Some(acct) = state.account.as_ref() else {
        return;
    };
    let selected: HashSet<i64> = match state.store.selected_ids() {
        Ok(s) => s.into_iter().collect(),
        Err(_) => return,
    };
    let want: HashSet<i64> = chat_ids.iter().copied().collect();
    let targets: Vec<crate::kakao::RoomFile> = crate::kakao::list_room_files(acct)
        .into_iter()
        .filter(|r| want.contains(&r.chat_id) && selected.contains(&r.chat_id))
        .collect();
    if targets.is_empty() {
        return;
    }

    // 세션 키캐시(같은 pid 면 harvest 재실행 안 함; miss 면 1회 harvest 후 캐싱). 키는 RAM only.
    let Some(mut cands_arc) = state.cached_candidates() else {
        return;
    };

    // 캐시가 형성된 뒤 열린 방의 키는 캐시에 없을 수 있다 → 방당 최대 1회 재harvest(전체 1회).
    let mut refreshed = false;
    for room in &targets {
        // 백그라운드 경로(force=false): 잠금 프로브가 "열림"인 방만 후보 스윕(3s 폴링 폭주 방지).
        let _ = process_room(state, acct, room, &mut cands_arc, &mut refreshed, false);
    }
}

/// 능동 수집 결과(사용자 대화방 클릭 → `POST /api/collect`).
pub(crate) struct CollectOutcome {
    /// "done"(수집됨) / "waiting"(닫힘·키 미회수) / "collecting"(이미 진행 중) /
    /// "not_tracked"(추적 방 아님) / "error".
    pub status: &'static str,
    /// 이 방의 저장 총 행수(status=done 일 때 의미 있음).
    pub rows: i64,
}

/// 수집 점유 RAII — 조기 반환·패닉에도 in-flight 표식이 남지 않게 한다.
struct CollectGuard<'a> {
    state: &'a Shared,
    chat_id: i64,
}
impl Drop for CollectGuard<'_> {
    fn drop(&mut self) {
        self.state.collect_end(self.chat_id);
    }
}

/// 사용자가 대화방을 클릭했을 때의 **즉시 수집**(blocking).
///
/// 백그라운드 재조정과 다른 점: 잠금 프로브가 "닫힘"이라고 해도 후보 스윕과 1회 재harvest 를
/// 허용한다(`force=true`). 세션 키캐시에 그 방 키가 없어 앱이 닫힘으로 오판한 방도, 실제로
/// 카톡에서 열려 있으면 이 경로로 수집된다. 진짜 닫힌 방은 키가 안 나와 `waiting` 으로 끝난다.
pub(crate) fn collect_room_now(state: &Shared, chat_id: i64) -> CollectOutcome {
    let mut out = CollectOutcome { status: "waiting", rows: 0 };
    let Some(acct) = state.account.as_ref() else {
        out.status = "error";
        return out;
    };
    // 추적(selected) 방만 수집한다 — 추적하지 않는 방의 본문은 저장하지 않는다.
    match state.store.selected_ids() {
        Ok(s) if s.contains(&chat_id) => {}
        Ok(_) => {
            out.status = "not_tracked";
            return out;
        }
        Err(_) => {
            out.status = "error";
            return out;
        }
    }
    // 연타/중복 요청 안전: 같은 방 수집이 이미 돌고 있으면 즉시 반환(호출측이 잠시 뒤 재시도).
    if !state.collect_begin(chat_id) {
        out.status = "collecting";
        return out;
    }
    let _guard = CollectGuard { state, chat_id };

    let Some(room) = crate::kakao::list_room_files(acct)
        .into_iter()
        .find(|r| r.chat_id == chat_id)
    else {
        // 로컬 chatLogs 파일 자체가 없음 → 수집 불가(대기).
        return out;
    };
    let Some(mut cands_arc) = state.cached_candidates() else {
        return out;
    };
    let mut refreshed = false;
    out.status = process_room(state, acct, &room, &mut cands_arc, &mut refreshed, true);
    out.rows = state.store.message_count(chat_id).unwrap_or(0);
    if out.status == "waiting" {
        // 세션 캐시에 그 방 키가 없다. 카톡 메모리 재harvest 로 찾을 수 있지만 수십 초가 걸리므로
        // **응답을 막지 않고** 백그라운드로 돌린다 → 호출측이 잠시 뒤 재요청하면 그때 수집된다.
        // 최근에 이미 재harvest 했는데도 없으면 진짜 닫힌 방 → waiting 그대로.
        if state.harvest_in_flight() || (state.harvest_due() && spawn_harvest(state)) {
            out.status = "collecting";
        }
    }
    out
}

/// 카톡 메모리 키 재harvest 를 백그라운드 스레드 1개로 띄운다(이미 돌고 있거나 스폰 실패면 false).
/// 수십 초 걸리는 작업이라 HTTP 응답 경로에서 인라인으로 돌리지 않는다.
fn spawn_harvest(state: &Shared) -> bool {
    if !state.harvest_begin() {
        return false;
    }
    let st = state.clone();
    let spawned = std::thread::Builder::new()
        .name("win_app-harvest".into())
        .spawn(move || {
            crate::applog::line("[win_app] 방 키 미회수 → 백그라운드 세션 키 재harvest");
            let _ = st.refresh_candidates();
            st.harvest_end();
        })
        .is_ok();
    if !spawned {
        state.harvest_end();
    }
    spawned
}

/// 한 방 델타 처리(키 회수 → import → SSE push → 사진 백그라운드). 결과 상태를 돌려준다.
///
/// `force` = 사용자가 직접 요청한 방. 잠금 프로브 결과와 무관하게 후보 스윕과 1회 재harvest 를
/// 허용하고, 최초 수집이면 `collecting` 을 표시한다. `false`(백그라운드 폴링)면 기존대로
/// "열린 방만" 스윕 — 3s 폴링이 전체 후보 스윕을 반복하지 않게 하기 위함이다.
fn process_room(
    state: &Shared,
    acct: &std::path::Path,
    room: &crate::kakao::RoomFile,
    cands_arc: &mut std::sync::Arc<HashSet<crate::kakao::memkey::RawKey>>,
    refreshed: &mut bool,
    force: bool,
) -> &'static str {
    let cur = state.store.collect_status_of(room.chat_id).ok().flatten();
    let was_done = cur.as_deref() == Some("done");
    let is_open = crate::state::is_locked(&room.path);
    // 후보 스윕 허용 조건: 열린 방(백그라운드) 또는 사용자가 직접 클릭한 방(force).
    // 단, 같은 후보집합(세대)으로 이미 스윕해 실패한 방은 다시 돌지 않는다 — 스윕은 방당 수 초가
    // 걸리므로(후보마다 PBKDF2 검증) 연타·폴링이 그 비용을 반복하면 안 된다.
    let allow_sweep = (force || is_open) && !state.sweep_failed_now(room.chat_id);

    // 방 키 해석: 방별 캐시 우선(스윕 없음). 캐시 무효 시엔 위 조건에서만 후보 스윕.
    let cached = state.room_key(room.chat_id);
    let mut key =
        crate::kakao::import::resolve_room_key(room, cands_arc.as_ref(), cached, allow_sweep);
    // 스윕으로도 못 찾으면 세션 후보가 낡은 것(재로그인·새 방 오픈으로 키가 새로 로드됨).
    // force(사용자 클릭)는 동기 재harvest 로 즉시 시도한다. 백그라운드 폴링은 재harvest 를
    // **워커 스레드**로 돌려 이 배치(다른 방들의 실시간)를 막지 않는다 — 이번 틱은 키 없이
    // 넘기고, harvest 가 끝나면 다음 틱이 갱신된 후보로 자동 해결한다(수동 재로그인 불필요).
    if key.is_none() && is_open && !*refreshed && state.harvest_due() {
        if force {
            crate::applog::line("[win_app] 방 키 미회수 → 세션 키 재harvest");
            if let Some(fresh) = state.refresh_candidates() {
                *cands_arc = fresh;
                *refreshed = true;
                key = crate::kakao::import::resolve_room_key(room, cands_arc.as_ref(), None, true);
            }
        } else {
            // 논블로킹: 백그라운드 1스레드로 재harvest(spawn_harvest 내부에서 로그·동시가드).
            spawn_harvest(state);
            *refreshed = true; // 이 배치의 다른 방이 중복 스폰하지 않게.
        }
    }
    let Some(key) = key else {
        // 이 세대 후보로는 이 방 키가 없다고 표식(다음 harvest 로 후보가 바뀌면 자동 해제).
        if allow_sweep {
            state.mark_sweep_failed(room.chat_id);
        }
        // 키 미회수: done 은 유지(다운그레이드·플리커 금지), 그 외에는 waiting(값 같으면 무변).
        if !was_done && cur.as_deref() != Some("waiting") {
            let _ = state.store.set_collect(room.chat_id, "waiting", None);
        }
        return if was_done { "done" } else { "waiting" };
    };
    state.cache_room_key(room.chat_id, key);

    // 최초 수집(아직 done 아님)이고 실제로 다룰 수 있는 방만 'collecting' 표시(폴링 플리커 방지).
    if allow_sweep && !was_done {
        let _ = state.store.set_collect(room.chat_id, "collecting", None);
    }
    match crate::kakao::import::import_room_with_key(&state.store, room, &key) {
        Ok(res) => {
            let has_new = res.imported > 0;
            // 신규 델타가 있거나 최초 수집일 때만 done 갱신. 델타 0 & 이미 done → 상태 무변(플리커 금지).
            if has_new || !was_done {
                let total = state.store.message_count(room.chat_id).unwrap_or(0);
                let _ = state.store.set_collect(room.chat_id, "done", Some(total));
            }
            // WORK-008: 사진 **계획만**(talkmedia 조회 + pending 표식) — 네트워크 없음.
            // 바이트 다운로드를 여기서 하면 그만큼 SSE push 가 밀려 실시간이 깨진다.
            let plan = if state.photo_sweep_due(room.chat_id, has_new || !was_done) {
                crate::kakao::photo::plan_room_photos(state, room.chat_id)
            } else {
                Vec::new()
            };
            if has_new && !res.rows.is_empty() {
                crate::applog::line(&format!("[win_app] 실시간 델타 +{}행 push", res.rows.len()));
                // 닉네임 맵은 세션 1회만 로드(캐시). 델타 있을 때만 필요.
                let nm = state.nick_map_cached(|| {
                    crate::kakao::talk_user_db_path(acct)
                        .map(|p| crate::kakao::import::load_nick_map(&p, cands_arc.as_ref()))
                        .unwrap_or_default()
                });
                if !nm.is_empty() {
                    if let Ok(ids) = state.store.distinct_author_ids(room.chat_id) {
                        let _ = crate::kakao::import::apply_author_names(
                            &state.store,
                            room.chat_id,
                            &ids,
                            &nm,
                        );
                    }
                }
                // 사진 표식(kind/media_status) 부착용 상태맵(계획 단계의 pending 까지 반영됨).
                let media = state.store.media_status_map(room.chat_id).unwrap_or_default();
                for mut row in res.rows {
                    if row.author_name.is_none() {
                        row.author_name = nm.get(&row.author_id).cloned();
                    }
                    if let Some(st) = media.get(&row.log_id) {
                        row.kind = Some("photo".to_string());
                        row.media_status = Some(st.clone());
                    }
                    let _ = state.tx.send(StreamEvent {
                        chat_id: room.chat_id,
                        row,
                    });
                }
            }
            // SSE 를 보낸 **뒤에** 사진 바이트를 백그라운드로 받는다(방당 1스레드).
            // 저장/유실이 확정되면 download_photos 가 그 행을 SSE 로 재전송해 UI 가 갱신된다.
            if !plan.is_empty() && state.photo_begin(room.chat_id) {
                let st = state.clone();
                let cid = room.chat_id;
                let spawned = std::thread::Builder::new()
                    .name("win_app-photo".into())
                    .spawn(move || {
                        crate::kakao::photo::download_photos(&st, cid, plan);
                        st.photo_end(cid);
                    })
                    .is_ok();
                if !spawned {
                    state.photo_end(room.chat_id);
                }
            }
            "done"
        }
        Err(_) => {
            let _ = state.store.set_collect(room.chat_id, "error", None);
            "error"
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn account_db_signal_files() {
        assert!(is_account_db("TalkUserDB.edb"));
        assert!(is_account_db("TalkUserDB.edb-wal"));
        assert!(is_account_db("chatListInfo.edb"));
        assert!(is_account_db("chatlistinfo.edb-shm"));
        assert!(!is_account_db("chatLogs_123.edb"));
        assert!(!is_account_db("CalendarDB.edb"));
    }

    #[test]
    fn life_str_contract() {
        use crate::server::{life_str, LIFE_DOWN, LIFE_IN, LIFE_OUT};
        assert_eq!(life_str(LIFE_DOWN), "DOWN");
        assert_eq!(life_str(LIFE_OUT), "UP_LOGGED_OUT");
        assert_eq!(life_str(LIFE_IN), "UP_LOGGED_IN");
    }

    #[test]
    fn parse_wal_and_edb_names() {
        assert_eq!(parse_chatlog_chat_id("chatLogs_123.edb"), Some(123));
        assert_eq!(parse_chatlog_chat_id("chatLogs_18332626660210452.edb-wal"), Some(18332626660210452));
        assert_eq!(parse_chatlog_chat_id("chatLogs_456.edb-shm"), Some(456));
        assert_eq!(parse_chatlog_chat_id("chatListInfo.edb"), None);
        assert_eq!(parse_chatlog_chat_id("TalkUserDB.edb-wal"), None);
        assert_eq!(parse_chatlog_chat_id("chatLogs_.edb"), None);
    }
}
