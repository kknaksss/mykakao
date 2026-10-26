//! WORK-006 로그인 상태 트래킹 + 자동 재조정 엔진.
//!
//! 3상태(DOWN / UP_LOGGED_OUT / UP_LOGGED_IN)를 **cheap passive 신호**로 감지한다:
//!  - 프로세스 존재(가벼움) + 계정 DB(TalkUserDB/chatListInfo) **잠금 여부**(deny-all open 프로브).
//!  - OQ-1 실측: 로그인 시 계정 DB 가 deny-all open 에서 SHARING_VIOLATION(32) → "열림=로그인".
//!    로그아웃 시 닫히면 open 성공 → "미열림=로그아웃". 파일 close 이벤트에 의존하지 않고 잠금을 직접 본다.
//!  - harvest(메모리 키회수)는 **로그인/방열림 이벤트에서만**(재조정), 상태 폴링엔 안 씀.
//!
//! 안전: 원본 DB **읽기 전용**(deny-all open 은 잠김이면 즉시 실패=간섭0, 미잠김이면 즉시 닫음). 카톡 무변조.
//! 키·닉네임·식별자 원값 로그 금지(존재/개수만).

#![cfg(windows)]

use std::collections::HashSet;
use std::path::Path;
use std::time::Duration;

use tokio::sync::mpsc;

use crate::server::{Shared, LIFE_DOWN, LIFE_IN, LIFE_OUT};

/// win32 ERROR_SHARING_VIOLATION 의 HRESULT.
const HR_SHARING_VIOLATION: i32 = 0x8007_0020u32 as i32;

fn wide(p: &Path) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    p.as_os_str().encode_wide().chain(std::iter::once(0)).collect()
}

/// deny-all(FILE_SHARE 0) 로 열어본다. 카톡이 잡고 있으면 SHARING_VIOLATION → `true`(열림/잠김).
/// 성공하면 즉시 닫고 `false`(미잠김). 파일 없음/기타 오류 → `false`. **읽기 전용·간섭0.**
pub fn is_locked(path: &Path) -> bool {
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::Storage::FileSystem::{
        CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_MODE, OPEN_EXISTING,
    };
    if !path.exists() {
        return false;
    }
    let w = wide(path);
    // SAFETY: 읽기 권한·deny-all·기존파일 열기. 실패면 Err(간섭 없음), 성공하면 즉시 CloseHandle.
    unsafe {
        match CreateFileW(
            PCWSTR(w.as_ptr()),
            0x8000_0000, // GENERIC_READ
            FILE_SHARE_MODE(0),
            None,
            OPEN_EXISTING,
            FILE_ATTRIBUTE_NORMAL,
            None,
        ) {
            Ok(h) => {
                let _ = CloseHandle(h);
                false
            }
            Err(e) => e.code().0 == HR_SHARING_VIOLATION,
        }
    }
}

/// 계정 DB(TalkUserDB 또는 chatListInfo)가 열려 있는가(=로그인).
pub fn account_db_open(acct: &Path) -> bool {
    let talk = acct.join("TalkUserDB.edb");
    if is_locked(&talk) {
        return true;
    }
    let cli = crate::kakao::chat_data_dir(acct).join("chatListInfo.edb");
    is_locked(&cli)
}

/// 3상태 감지(cheap). DOWN/UP_LOGGED_OUT/UP_LOGGED_IN 코드(LIFE_*).
pub fn detect(acct: Option<&Path>) -> u8 {
    if !crate::kakao::memkey::is_running() {
        return LIFE_DOWN;
    }
    match acct {
        Some(a) if account_db_open(a) => LIFE_IN,
        _ => LIFE_OUT,
    }
}

/// 지금 열린(키 회수 가능한) 방 chat_id 들 — chatLogs 파일 잠금 프로브(메모리 스캔 없이 cheap).
pub fn open_rooms(acct: &Path) -> Vec<i64> {
    let mut out: Vec<i64> = crate::kakao::list_room_files(acct)
        .into_iter()
        .filter(|r| is_locked(&r.path))
        .map(|r| r.chat_id)
        .collect();
    out.sort_unstable();
    out
}

/// 로그인 전이 시 1회(blocking): 세션 키캐시 워밍 → 본인 닉네임 해석 → 추적 방 델타 재조정.
fn on_login(state: &Shared) {
    let Some(acct) = state.account.as_ref() else {
        return;
    };
    // 본인 식별자 폴백: 로그인 계정 이메일(레지스트리 읽기, 키 불필요). 값은 로그 금지.
    let email = crate::kakao::account_email();
    // 세션 캐시(있으면 재사용, 없으면 harvest 후 캐싱).
    let Some(cands) = state.cached_candidates() else {
        crate::applog::line("[win_app] 로그인 재조정: 키 회수 실패(후보 없음)");
        // 키가 없어도 계정 이메일로 본인 식별자는 표시.
        state.set_me(email);
        return;
    };
    // 본인 닉네임(TalkUserDB) — 값은 로그 금지(존재/소스만).
    let mut nick: Option<String> = None;
    if let Some(p) = crate::kakao::talk_user_db_path(acct) {
        if let Some((uid, n)) = crate::kakao::import::my_profile(&p, cands.as_ref()) {
            state.set_my_uid(uid);
            nick = n;
        }
    }
    // 본인 식별자 우선순위: ① TalkUserDB 닉네임 → ② 계정 이메일 → ③ 없음("(이름 없음)").
    let src = if nick.is_some() {
        "닉네임"
    } else if email.is_some() {
        "이메일"
    } else {
        "없음"
    };
    crate::applog::line(&format!(
        "[win_app] 본인 프로필: userId={}, 식별자 {}",
        if state.my_uid() != 0 { "확보" } else { "미상" },
        src
    ));
    state.set_me(nick.or(email));
    // 추적 방(selected) 델타 재조정 — process_delta 재사용(내부에서 열린 방만 실제 import).
    if let Ok(selected) = state.store.selected_ids() {
        if !selected.is_empty() {
            crate::watch::process_delta(state, &selected);
        }
    }
}

/// 상태 전이 처리: life 갱신 + (로그인)재조정 / (로그아웃·다운)세션 무효화.
fn apply(state: &Shared, prev: Option<u8>, new: u8) {
    state.set_life(new);
    if prev == Some(new) {
        return;
    }
    match new {
        LIFE_IN => {
            crate::applog::line("[win_app] 상태→UP_LOGGED_IN: 추적 방 자동 재조정");
            let st = state.clone();
            std::thread::spawn(move || on_login(&st));
        }
        LIFE_OUT | LIFE_DOWN => {
            // 로그아웃/다운 → 세션 키·닉네임 캐시 무효화(다음 로그인 때 델타로 메꿈).
            state.invalidate_session();
            crate::applog::line(&format!(
                "[win_app] 상태→{}: 세션 캐시 무효화",
                if new == LIFE_DOWN { "DOWN" } else { "UP_LOGGED_OUT" }
            ));
        }
        _ => {}
    }
}

/// 상태 엔진 루프(비동기). 계정 파일 이벤트(login_rx) + 저빈도 detect(3s)로 전이 감지.
/// harvest 는 로그인 전이에서만(폴링 아님).
pub async fn run(state: Shared, mut login_rx: mpsc::Receiver<()>) {
    let acct = state.account.clone();
    // 시작: 즉시 상태 판정 + (로그인이면) 재조정.
    let mut cur = detect(acct.as_deref());
    crate::applog::line(&format!("[win_app] 초기 상태: {}", crate::server::life_str(cur)));
    apply(&state, None, cur);

    // 방 열림 edge 추적: 지금 "열린 추적 방" 집합. 새로 열린 방만 수집을 트리거한다.
    let mut prev_open: HashSet<i64> = HashSet::new();
    if cur == LIFE_IN {
        prev_open = open_tracked(&state, acct.as_deref());
    }

    // 상태 히스테리시스: 하강(DOWN/OUT)은 **연속 CONFIRM_N회** 확인해야 확정한다.
    // find_pid/잠금프로브의 단발 실패로 상태가 IN↔DOWN↔OUT 진동하며 재수집·"수집 중" 반복하던 문제 제거.
    // 상승(IN)은 즉시 확정(로그인 반응성).
    const CONFIRM_N: u32 = 3;
    let mut down_cand: u8 = cur;
    let mut down_streak: u32 = 0;

    let mut tick = tokio::time::interval(Duration::from_secs(3));
    tick.tick().await; // 즉시 틱 소비.
    loop {
        tokio::select! {
            m = login_rx.recv() => {
                if m.is_none() { break; } // 채널 닫힘 → 종료.
                // 계정 파일 변화 → 잠깐 디바운스 후 재판정.
                tokio::time::sleep(Duration::from_millis(500)).await;
                // 대기 중 쌓인 이벤트 흡수.
                while login_rx.try_recv().is_ok() {}
            }
            _ = tick.tick() => {}
        }
        let raw = detect(acct.as_deref());
        // 히스테리시스 적용 → 확정 상태 new 산출.
        let new = if raw == cur {
            down_streak = 0;
            cur
        } else if raw == LIFE_IN {
            down_streak = 0;
            LIFE_IN
        } else {
            // 하강 후보(DOWN/OUT): 같은 값이 연속일 때만 누적, 값이 바뀌면(진동) 리셋.
            if raw == down_cand {
                down_streak += 1;
            } else {
                down_cand = raw;
                down_streak = 1;
            }
            if down_streak >= CONFIRM_N {
                down_streak = 0;
                raw // 확정
            } else {
                cur // 미확정 → 이전 상태 유지(단발 miss·진동 흡수)
            }
        };
        if new != cur {
            crate::applog::line(&format!(
                "[win_app] 상태 전이 {} → {}",
                crate::server::life_str(cur),
                crate::server::life_str(new)
            ));
            apply(&state, Some(cur), new);
            cur = new;
        }
        // 방 열림 감지(로그인 상태에서 매 틱): 카톡에서 방을 새로 열면 그 추적 방 수집을 트리거.
        // (파일 변경 이벤트가 없어도 잠금 프로브로 열림 전이를 잡는다.)
        if cur == LIFE_IN {
            let cur_open = open_tracked(&state, acct.as_deref());
            let newly: Vec<i64> = cur_open.difference(&prev_open).copied().collect();
            if !newly.is_empty() {
                crate::applog::line(&format!(
                    "[win_app] 방 열림 감지: 추적 방 {}개 수집 트리거",
                    newly.len()
                ));
                let st = state.clone();
                let _ =
                    tokio::task::spawn_blocking(move || crate::watch::process_delta(&st, &newly));
            }
            prev_open = cur_open;
        } else {
            prev_open.clear();
        }
    }
}

/// 지금 "열려 있는 추적(selected) 방" 집합(잠금 프로브 ∩ selected). 없으면 빈 집합.
fn open_tracked(state: &Shared, acct: Option<&Path>) -> HashSet<i64> {
    let Some(a) = acct else {
        return HashSet::new();
    };
    let Ok(sel) = state.store.selected_ids() else {
        return HashSet::new();
    };
    if sel.is_empty() {
        return HashSet::new();
    }
    let sel_set: HashSet<i64> = sel.into_iter().collect();
    open_rooms(a).into_iter().filter(|id| sel_set.contains(id)).collect()
}
