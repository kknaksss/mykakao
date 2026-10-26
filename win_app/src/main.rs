//! mykakao Windows V2 — win_app (P1+P2+P3).
//!
//! 트레이 상주 앱: 아이콘 클릭 → 기본 브라우저로 localhost 설정 페이지. 선택 방의 과거
//! 히스토리를 실행 중 카톡 메모리 키로 복호·축적(P1), `chat_data` 파일감시로 새 메시지를
//! 델타 복호·SSE 스트리밍(P2), 트레이 + 로그인 상태 상시화 + 닉네임 조인(P3).
//!
//! 안전(불변): 카톡 원본 읽기만·프로세스 무변조, 키/본문/닉네임 원값 비노출, 복호는 임시 사본(자동 삭제).
//!
//! release 빌드에선 콘솔을 숨기고(로그는 파일) 트레이로만 상호작용한다. debug 는 콘솔 유지(개발 편의).
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod applog;
#[cfg(windows)]
mod diag;
mod kakao;
mod server;
#[cfg(windows)]
mod state;
mod store;
#[cfg(windows)]
mod tray;
mod watch;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;

use crate::server::AppState;
use crate::store::Store;

/// 우리 축적 DB 경로: `%LOCALAPPDATA%\mykakao\accumulate.sqlite`(없으면 CWD).
fn store_path() -> PathBuf {
    let base = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    let dir = base.join("mykakao");
    let _ = std::fs::create_dir_all(&dir);
    dir.join("accumulate.sqlite")
}

fn resolve_port() -> u16 {
    std::env::var("WIN_APP_PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(8765)
}

fn main() -> Result<()> {
    // 스키마 진단 모드(개발용): 실복호로 테이블·컬럼·행수·값프로파일만 출력 후 종료.
    #[cfg(windows)]
    if std::env::var_os("WIN_APP_DIAG").is_some() {
        diag::run();
        return Ok(());
    }

    let db_path = std::env::var_os("WIN_APP_DB")
        .map(PathBuf::from)
        .unwrap_or_else(store_path);
    let store = Arc::new(Store::open(&db_path)?);

    let account = kakao::discover_account_dir();
    match &account {
        Some(a) => applog::line(&format!(
            "[win_app] 계정 폴더 탐색됨(fp={})",
            kakao::account_fingerprint(a).unwrap_or_default()
        )),
        None => applog::line("[win_app] 계정 폴더 미탐색 — 카톡 로그인 확인 필요"),
    }
    applog::line(&format!(
        "[win_app] KakaoTalk 실행: {}",
        if kakao::memkey::is_running() { "예" } else { "아니오" }
    ));

    let state = Arc::new(AppState::new(store, account));
    let port = resolve_port();

    // 서버(axum+watch)는 백그라운드 스레드의 tokio 런타임에서. 트레이 이벤트 루프는 메인 스레드.
    let server_state = state.clone();
    std::thread::Builder::new()
        .name("win_app-server".into())
        .spawn(move || {
            let rt = match tokio::runtime::Builder::new_multi_thread().enable_all().build() {
                Ok(rt) => rt,
                Err(e) => {
                    applog::line(&format!("[win_app] 런타임 생성 실패: {e}"));
                    return;
                }
            };
            rt.block_on(async move {
                // P2: 파일감시 시작(핸들은 이 async 스코프 = 서버 수명 동안 보관).
                let _watch = watch::spawn(server_state.clone());
                let app = server::router(server_state);
                let addr = SocketAddr::from(([127, 0, 0, 1], port));
                match tokio::net::TcpListener::bind(addr).await {
                    Ok(listener) => {
                        applog::line(&format!("[win_app] http://{addr}"));
                        if let Err(e) = axum::serve(listener, app).await {
                            applog::line(&format!("[win_app] 서버 종료: {e}"));
                        }
                    }
                    Err(e) => applog::line(&format!("[win_app] 포트 바인드 실패({port}): {e}")),
                }
            });
        })?;

    // WIN_APP_NO_TRAY 설정 시 트레이 없이 서버만 유지(헤드리스/서버 전용 모드).
    let no_tray = std::env::var_os("WIN_APP_NO_TRAY").is_some();

    // 메인 스레드: 트레이(블로킹). 종료 선택 시 반환 → 프로세스 종료(서버 스레드 정리).
    #[cfg(windows)]
    {
        if no_tray {
            applog::line("[win_app] NO_TRAY 모드 — 서버만 유지(프로세스 종료로 정지)");
            std::thread::park();
        } else {
            tray::run(state, port)?;
        }
    }
    // 비-Windows(개발/CI): 트레이 없이 서버만 유지.
    #[cfg(not(windows))]
    {
        let _ = no_tray;
        applog::line("[win_app] 비-Windows: 트레이 없이 서버 유지(Ctrl+C 종료)");
        std::thread::park();
    }
    Ok(())
}
