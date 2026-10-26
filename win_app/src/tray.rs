//! P3 트레이 상주 앱(Windows 전용) — **별도 crate 없이 `windows` crate 로 직접 구현**.
//!
//! `Shell_NotifyIconW`(NOTIFYICONDATAW) 로 트레이 아이콘 등록 + 숨은 메시지 윈도우(WndProc)로
//! 콜백 수신. 좌클릭 또는 우클릭 메뉴 「설정 열기」 → `ShellExecuteW` 로 기본 브라우저에 localhost.
//! 우클릭 → `TrackPopupMenu`(「설정 열기」·「종료」). 「종료」 → 창 파괴 → 메시지 루프 종료 → 아이콘 제거.
//!
//! 이벤트 대기는 `GetMessageW` **블로킹**(busy-loop 없음). SAC 아래서도 새 서명없는 산출물이 없다.

#![cfg(windows)]

use std::sync::OnceLock;

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{COLORREF, HINSTANCE, HWND, LPARAM, LRESULT, POINT, SIZE, WPARAM};
use windows::Win32::Graphics::Gdi::{
    CreateFontIndirectW, CreatePen, CreateSolidBrush, DeleteObject, Ellipse, GetDC, GetSysColor,
    GetTextExtentPoint32W, ReleaseDC, SelectObject, SetBkMode, SetTextColor, TextOutW,
    COLOR_MENUTEXT, HGDIOBJ, PS_SOLID, TRANSPARENT,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Shell::{
    ShellExecuteW, Shell_NotifyIconW, NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE,
    NOTIFYICONDATAW,
};
use windows::Win32::UI::Controls::{DRAWITEMSTRUCT, MEASUREITEMSTRUCT, ODT_MENU};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyMenu, DestroyWindow,
    DispatchMessageW, GetCursorPos, GetMessageW, LoadIconW, PostQuitMessage,
    RegisterClassW, SetForegroundWindow, SystemParametersInfoW, TrackPopupMenu, TranslateMessage,
    IDI_APPLICATION, MF_DISABLED, MF_OWNERDRAW, MF_SEPARATOR, MF_STRING, MSG,
    NONCLIENTMETRICSW, SPI_GETNONCLIENTMETRICS, SW_SHOWNORMAL, SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS,
    TPM_LEFTALIGN, TPM_RIGHTBUTTON, WINDOW_EX_STYLE, WINDOW_STYLE, WM_APP, WM_COMMAND, WM_DESTROY,
    WM_DRAWITEM, WM_LBUTTONUP, WM_MEASUREITEM, WM_RBUTTONUP, WNDCLASSW,
};

use crate::server::{Shared, LIFE_IN};

/// 트레이 콜백 메시지(WM_APP 오프셋).
const WM_TRAY: u32 = WM_APP + 1;
/// 메뉴 항목 ID(클릭 동작).
const ID_OPEN: usize = 1;
const ID_QUIT: usize = 2;
/// 오너드로우 정보 항목 ID(비클릭).
const ID_INFO_HEAD: usize = 10;
const ID_INFO_USER: usize = 11;
const ID_INFO_STAT: usize = 12;
/// 상태 점 색(COLORREF 0x00BBGGRR). 초록 #17B26A / 빨강 #E5484D.
const DOT_GREEN: u32 = 0x006A_B217;
const DOT_RED: u32 = 0x004D_48E5;
/// 오너드로우 좌측 패딩.
const OWNER_PAD: i32 = 22;

/// 브라우저로 열 URL(널종단 UTF-16). WndProc 전역 접근용.
static URL_W: OnceLock<Vec<u16>> = OnceLock::new();
/// 트레이 메뉴가 상태를 읽기 위한 공유 상태(로그인상태·본인닉네임).
static TRAY_STATE: OnceLock<Shared> = OnceLock::new();

fn wide_nul(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn to_wide_null(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// URL 을 기본 브라우저로 연다(ShellExecuteW). 실패는 로그만.
fn open_settings() {
    if let Some(u) = URL_W.get() {
        // SAFETY: u 는 널종단 UTF-16, 호출 동안 유효. 반환 HINSTANCE 는 성공/실패 코드일 뿐 핸들 아님.
        unsafe {
            ShellExecuteW(
                None,
                w!("open"),
                PCWSTR(u.as_ptr()),
                PCWSTR::null(),
                PCWSTR::null(),
                SW_SHOWNORMAL,
            );
        }
    }
}

/// 현재 (로그인여부, 본인닉네임).
fn cur_state() -> (bool, Option<String>) {
    TRAY_STATE
        .get()
        .map(|s| (s.life() == LIFE_IN, s.me()))
        .unwrap_or((false, None))
}

/// 오너드로우 항목의 표시 텍스트(상태 항목은 점 뒤 라벨만).
fn info_text(id: usize) -> String {
    let (logged_in, me) = cur_state();
    match id {
        ID_INFO_HEAD => "로그인 상태".to_string(),
        ID_INFO_USER => format!(
            "   로그인 유저 : {}",
            if logged_in { me.as_deref().unwrap_or("(이름 없음)") } else { "-" }
        ),
        ID_INFO_STAT => format!(
            "   상태 :    {}",
            if logged_in { "로그인" } else { "로그아웃" }
        ),
        _ => String::new(),
    }
}

/// 캐시된 메뉴 폰트(시스템 lfMenuFont).
fn menu_font() -> windows::Win32::Graphics::Gdi::HFONT {
    use windows::Win32::Graphics::Gdi::HFONT;
    static F: OnceLock<isize> = OnceLock::new();
    let h = *F.get_or_init(|| {
        // SAFETY: NONCLIENTMETRICSW 를 채우고 lfMenuFont 로 폰트 생성.
        unsafe {
            let mut ncm = NONCLIENTMETRICSW {
                cbSize: std::mem::size_of::<NONCLIENTMETRICSW>() as u32,
                ..Default::default()
            };
            let ok = SystemParametersInfoW(
                SPI_GETNONCLIENTMETRICS,
                ncm.cbSize,
                Some(&mut ncm as *mut _ as *mut _),
                SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
            )
            .is_ok();
            if ok {
                CreateFontIndirectW(&ncm.lfMenuFont).0 as isize
            } else {
                0
            }
        }
    });
    HFONT(h as *mut _)
}

/// 우클릭 팝업 메뉴 — 오너드로우 정보 3항목(까만 글씨·색 점·비클릭) + 환경설정/종료(WORK-007).
unsafe fn show_menu(hwnd: HWND) {
    let Ok(menu) = CreatePopupMenu() else {
        return;
    };
    // 정보 항목: MF_OWNERDRAW(직접 그림) + MF_DISABLED(비클릭·비하이라이트). ID 로 항목 구분.
    let od = MF_OWNERDRAW | MF_DISABLED;
    let _ = AppendMenuW(menu, od, ID_INFO_HEAD, PCWSTR::null());
    let _ = AppendMenuW(menu, od, ID_INFO_USER, PCWSTR::null());
    let _ = AppendMenuW(menu, od, ID_INFO_STAT, PCWSTR::null());
    // 구분선 + 동작 항목(일반·클릭).
    let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
    let _ = AppendMenuW(menu, MF_STRING, ID_OPEN, w!("환경설정"));
    let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
    let _ = AppendMenuW(menu, MF_STRING, ID_QUIT, w!("종료"));

    let mut pt = POINT::default();
    let _ = GetCursorPos(&mut pt);
    // 메뉴가 포커스를 잃으면 닫히도록 전경 설정(고전 워크어라운드).
    let _ = SetForegroundWindow(hwnd);
    let _ = TrackPopupMenu(
        menu,
        TPM_LEFTALIGN | TPM_RIGHTBUTTON,
        pt.x,
        pt.y,
        0,
        hwnd,
        None,
    );
    let _ = DestroyMenu(menu);
}

fn to16(s: &str) -> Vec<u16> {
    s.encode_utf16().collect()
}

/// WM_MEASUREITEM: 오너드로우 정보 항목 크기 산정(메뉴 폰트 기준).
unsafe fn measure_item(mis: &mut MEASUREITEMSTRUCT) {
    if mis.CtlType != ODT_MENU {
        return;
    }
    let text = to16(&info_text(mis.itemID as usize));
    let hdc = GetDC(None);
    let font = menu_font();
    let old = SelectObject(hdc, HGDIOBJ(font.0));
    let mut sz = SIZE::default();
    let _ = GetTextExtentPoint32W(hdc, &text, &mut sz);
    SelectObject(hdc, old);
    ReleaseDC(None, hdc);
    let extra = if mis.itemID as usize == ID_INFO_STAT { 6 } else { 0 };
    mis.itemWidth = (OWNER_PAD + sz.cx + extra + 18) as u32;
    mis.itemHeight = (sz.cy.max(18) + 6) as u32;
}

/// WM_DRAWITEM: 정보 항목을 시스템 메뉴색(까만/다크대응) 글씨로 그림 + 상태 점(초록/빨강).
unsafe fn draw_item(dis: &DRAWITEMSTRUCT) {
    if dis.CtlType != ODT_MENU {
        return;
    }
    let id = dis.itemID as usize;
    let hdc = dis.hDC;
    let rc = dis.rcItem;
    let font = menu_font();
    let old = SelectObject(hdc, HGDIOBJ(font.0));
    SetBkMode(hdc, TRANSPARENT);
    SetTextColor(hdc, COLORREF(GetSysColor(COLOR_MENUTEXT)));

    let text = to16(&info_text(id));
    let mut sz = SIZE::default();
    let _ = GetTextExtentPoint32W(hdc, &text, &mut sz);
    let x = rc.left + OWNER_PAD;
    let y = rc.top + (rc.bottom - rc.top - sz.cy) / 2;
    let _ = TextOutW(hdc, x, y, &text);

    if id == ID_INFO_STAT {
        // 점을 "   상태 :  " 뒤 gap 에 그린다.
        let (logged_in, _) = cur_state();
        let prefix = to16("   상태 :  ");
        let mut psz = SIZE::default();
        let _ = GetTextExtentPoint32W(hdc, &prefix, &mut psz);
        let cx = rc.left + OWNER_PAD + psz.cx + 4;
        let cy = rc.top + (rc.bottom - rc.top) / 2;
        let r = 4;
        let color = COLORREF(if logged_in { DOT_GREEN } else { DOT_RED });
        let brush = CreateSolidBrush(color);
        let pen = CreatePen(PS_SOLID, 1, color);
        let ob = SelectObject(hdc, HGDIOBJ(brush.0));
        let op = SelectObject(hdc, HGDIOBJ(pen.0));
        let _ = Ellipse(hdc, cx - r, cy - r, cx + r, cy + r);
        SelectObject(hdc, ob);
        SelectObject(hdc, op);
        let _ = DeleteObject(HGDIOBJ(brush.0));
        let _ = DeleteObject(HGDIOBJ(pen.0));
    }
    SelectObject(hdc, old);
}

/// 숨은 윈도우 프로시저: 트레이 콜백·메뉴 커맨드·오너드로우·파괴 처리.
unsafe extern "system" fn wndproc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_MEASUREITEM => {
            let mis = &mut *(lparam.0 as *mut MEASUREITEMSTRUCT);
            measure_item(mis);
            LRESULT(1)
        }
        WM_DRAWITEM => {
            let dis = &*(lparam.0 as *const DRAWITEMSTRUCT);
            draw_item(dis);
            LRESULT(1)
        }
        WM_TRAY => {
            // lParam 하위 워드 = 마우스 이벤트.
            let evt = (lparam.0 as u32) & 0xFFFF;
            if evt == WM_LBUTTONUP {
                open_settings();
            } else if evt == WM_RBUTTONUP {
                show_menu(hwnd);
            }
            LRESULT(0)
        }
        WM_COMMAND => {
            let id = wparam.0 & 0xFFFF;
            if id == ID_OPEN {
                open_settings();
            } else if id == ID_QUIT {
                let _ = DestroyWindow(hwnd); // → WM_DESTROY
            }
            LRESULT(0)
        }
        WM_DESTROY => {
            PostQuitMessage(0);
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

fn set_tip(nid: &mut NOTIFYICONDATAW, tip: &str) {
    let wide: Vec<u16> = tip.encode_utf16().take(nid.szTip.len() - 1).collect();
    for (i, c) in wide.iter().enumerate() {
        nid.szTip[i] = *c;
    }
    nid.szTip[wide.len()] = 0;
}

/// 트레이 실행(메인 스레드, 블로킹). 종료 시 반환(프로세스 종료).
pub fn run(state: Shared, port: u16) -> anyhow::Result<()> {
    let _ = URL_W.set(to_wide_null(&format!("http://127.0.0.1:{port}")));
    let _ = TRAY_STATE.set(state);

    // SAFETY: 아래 Win32 호출은 유효 인자로만 수행. 핸들 수명은 프로세스 종료까지.
    unsafe {
        let hmodule = GetModuleHandleW(None)
            .map_err(|e| anyhow::anyhow!("GetModuleHandleW 실패: {e}"))?;
        let hinstance = HINSTANCE(hmodule.0);
        let class_name = w!("mykakao_tray_wnd");

        let wc = WNDCLASSW {
            lpfnWndProc: Some(wndproc),
            hInstance: hinstance,
            lpszClassName: class_name,
            ..Default::default()
        };
        RegisterClassW(&wc); // 재등록 실패는 무시(이미 등록됨).

        let hwnd = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            class_name,
            w!("mykakao"),
            WINDOW_STYLE(0),
            0,
            0,
            0,
            0,
            None,
            None,
            hinstance,
            None,
        )
        .map_err(|e| anyhow::anyhow!("CreateWindowExW 실패: {e}"))?;

        let hicon = LoadIconW(None, IDI_APPLICATION)
            .map_err(|e| anyhow::anyhow!("LoadIconW 실패: {e}"))?;

        let mut nid = NOTIFYICONDATAW {
            cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
            hWnd: hwnd,
            uID: 1,
            uFlags: NIF_MESSAGE | NIF_ICON | NIF_TIP,
            uCallbackMessage: WM_TRAY,
            hIcon: hicon,
            ..Default::default()
        };
        set_tip(&mut nid, "mykakao — Windows 대화 축적 (클릭: 설정 열기)");

        if !Shell_NotifyIconW(NIM_ADD, &nid).as_bool() {
            crate::applog::line("[win_app] 트레이 아이콘 등록 실패");
        }

        // 서버 바인드 여유 후 최초 1회 자동 오픈(편의).
        std::thread::sleep(std::time::Duration::from_millis(800));
        open_settings();
        crate::applog::line("[win_app] 트레이 상주 — 좌클릭/우클릭 메뉴로 설정 열기");

        // 메시지 루프(블로킹, busy-loop 아님). WM_QUIT 시 종료.
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, HWND::default(), 0, 0).0 > 0 {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }

        // 정리: 트레이 아이콘 제거.
        let _ = Shell_NotifyIconW(NIM_DELETE, &nid);
        crate::applog::line("[win_app] 트레이 종료 — 아이콘 제거");
    }
    Ok(())
}
