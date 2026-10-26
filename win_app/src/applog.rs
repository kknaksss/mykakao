//! 얇은 파일 로거 — 트레이 앱은 콘솔이 숨겨질 수 있어(release) 로그를 파일로 남긴다.
//!
//! 경로: `%LOCALAPPDATA%\mykakao\win_app.log`(append). debug 빌드에선 stderr 에도 출력.
//! **키·대화 본문·닉네임 원값·계정 식별자 원값을 절대 넘기지 마라** — 호출측이 마스킹/카운트만.

use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

static FILE: OnceLock<Option<Mutex<std::fs::File>>> = OnceLock::new();

fn log_path() -> PathBuf {
    let base = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    let dir = base.join("mykakao");
    let _ = std::fs::create_dir_all(&dir);
    dir.join("win_app.log")
}

/// 한 줄 로그(파일 + debug 시 stderr). 값 노출 금지는 호출측 책임.
pub fn line(msg: &str) {
    #[cfg(debug_assertions)]
    eprintln!("{msg}");

    let f = FILE.get_or_init(|| {
        OpenOptions::new()
            .create(true)
            .append(true)
            .open(log_path())
            .ok()
            .map(Mutex::new)
    });
    if let Some(m) = f {
        if let Ok(mut file) = m.lock() {
            let _ = writeln!(file, "{msg}");
        }
    }
}
