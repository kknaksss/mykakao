//! 스키마 진단(개발용, `WIN_APP_DIAG` 설정 시 실행 후 종료).
//!
//! chatListInfo/TalkUserDB/열린 chatLogs 를 실복호해 **테이블·컬럼명·행수 + 컬럼별 값 프로파일
//! (타입·바이트길이 통계)**만 출력한다. **셀 값(이름/본문)은 절대 출력하지 않는다.**
#![cfg(windows)]

use std::io::Write;
use std::sync::Mutex;
use std::sync::OnceLock;

use rusqlite::types::ValueRef;
use rusqlite::Connection;

use crate::kakao;
use crate::kakao::import::{column_names, table_names};

static OUTF: OnceLock<Option<Mutex<std::fs::File>>> = OnceLock::new();

/// 진단 출력(콘솔 없는 release 대비 파일 동시 기록). 대상: `%TEMP%/win_app_diag.txt`.
fn out(msg: &str) {
    println!("{msg}");
    let f = OUTF.get_or_init(|| {
        let p = std::env::temp_dir().join("win_app_diag.txt");
        std::fs::File::create(p).ok().map(Mutex::new)
    });
    if let Some(m) = f {
        if let Ok(mut file) = m.lock() {
            let _ = writeln!(file, "{msg}");
        }
    }
}

macro_rules! d {
    ($($a:tt)*) => { out(&format!($($a)*)) };
}

pub fn run() {
    let Some(acct) = kakao::discover_account_dir() else {
        d!("[diag] 계정 폴더 없음");
        return;
    };
    d!("[diag] 계정 fp={}", kakao::account_fingerprint(&acct).unwrap_or_default());
    if !kakao::memkey::is_running() {
        d!("[diag] 카톡 미실행 — 키 회수 불가");
        return;
    }
    let Some(cands) = kakao::harvest_candidates(&acct) else {
        d!("[diag] 후보 키 수집 실패");
        return;
    };
    d!("[diag] 후보 키 {}개", cands.len());

    for (label, pathopt) in [
        ("chatListInfo", kakao::chat_list_info_path(&acct)),
        ("TalkUserDB", kakao::talk_user_db_path(&acct)),
    ] {
        d!("\n========== {label} ==========");
        let Some(path) = pathopt else {
            d!("[diag] 경로 없음");
            continue;
        };
        let got = kakao::import::with_decrypted(&path, &cands, dump);
        if got.is_none() {
            d!("[diag] 키 미회수 또는 복호 실패(DB 미개방?)");
        }
    }

    // 열린 chatLogs 하나(작성자 컬럼 확인). 상주 salt = 열린 방.
    let rooms = kakao::list_room_files(&acct);
    if let Some(pid) = kakao::memkey::find_pid(kakao::memkey::PROC_NAME) {
        let salts: Vec<[u8; 16]> = rooms.iter().map(|r| r.salt).collect();
        if let Ok(h) = kakao::memkey::harvest(pid, &salts, false) {
            if let Some(open_room) = rooms.iter().find(|r| h.resident_salts.contains(&r.salt)) {
                d!("\n========== chatLogs(열린 방 1개) ==========");
                let got = kakao::import::with_decrypted(&open_room.path, &cands, dump);
                if got.is_none() {
                    d!("[diag] 열린 방 키 미회수");
                }
            } else {
                d!("\n[diag] 열린(상주) chatLogs 방 없음");
            }
        }
    }
}

fn dump(conn: &Connection) {
    let tables = table_names(conn).unwrap_or_default();
    d!("[diag] 테이블 {}개: {:?}", tables.len(), tables);
    for t in &tables {
        let cols = column_names(conn, t).unwrap_or_default();
        let cnt: i64 = conn
            .query_row(
                &format!("SELECT count(*) FROM \"{}\"", t.replace('"', "")),
                [],
                |r| r.get(0),
            )
            .unwrap_or(-1);
        d!("  ── {t} (rows={cnt}) cols={cols:?}");
        for c in &cols {
            d!("      {c}: {}", col_profile(conn, t, c));
        }
    }
}

/// 컬럼 값 프로파일(타입 히스토그램 + 텍스트/블롭 바이트길이 통계). **값 내용 미출력.**
fn col_profile(conn: &Connection, table: &str, col: &str) -> String {
    let sql = format!(
        "SELECT \"{}\" FROM \"{}\" LIMIT 200",
        col.replace('"', ""),
        table.replace('"', "")
    );
    let Ok(mut stmt) = conn.prepare(&sql) else {
        return "<프로파일 실패>".into();
    };
    let mut n_null = 0u32;
    let mut n_int = 0u32;
    let mut n_real = 0u32;
    let mut n_text = 0u32;
    let mut n_blob = 0u32;
    let mut n_text_empty = 0u32;
    let mut min_len = usize::MAX;
    let mut max_len = 0usize;
    let mut ascii_ok = 0u32; // 텍스트가 UTF-8 로 유효한지(내용 아님, 유효성만)
    let rows = stmt.query_map([], |r| {
        match r.get_ref(0)? {
            ValueRef::Null => n_null += 1,
            ValueRef::Integer(_) => n_int += 1,
            ValueRef::Real(_) => n_real += 1,
            ValueRef::Text(b) => {
                n_text += 1;
                if b.is_empty() {
                    n_text_empty += 1;
                }
                min_len = min_len.min(b.len());
                max_len = max_len.max(b.len());
                if std::str::from_utf8(b).is_ok() {
                    ascii_ok += 1;
                }
            }
            ValueRef::Blob(b) => {
                n_blob += 1;
                min_len = min_len.min(b.len());
                max_len = max_len.max(b.len());
                if std::str::from_utf8(b).is_ok() {
                    ascii_ok += 1;
                }
            }
        }
        Ok(())
    });
    if let Ok(rows) = rows {
        for _ in rows.flatten() {}
    }
    let lenpart = if max_len > 0 {
        format!(" len[{}..{}]", if min_len == usize::MAX { 0 } else { min_len }, max_len)
    } else {
        String::new()
    };
    format!(
        "null={n_null} int={n_int} real={n_real} text={n_text}(empty={n_text_empty},utf8ok={ascii_ok}) blob={n_blob}{lenpart}"
    )
}
