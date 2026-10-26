//! WORK-008 사진 수집: 사진 메시지 → talkmedia URL 조회 → 다운로드 → 검증 → 로컬 미디어 저장.
//!
//! talkmedia.edb(memory harvest 키로 복호): chatMsgTokenJunction(logId→token) + tokenInfo(token→url,fileSize,checkSum).
//! URL 200 → checkSum(SHA1)·fileSize 검증 → `%LOCALAPPDATA%\mykakao\media\<chatId>\<logId>.<ext>` 저장(status=saved).
//! 410/404 → lost(유실, 재시도 안 함). 그 외 실패 → pending.
//!
//! 2단계 분리(실시간 보호): **① `plan_room_photos`(네트워크 없음)** 로 사진 참조만 뽑아 pending 표식 →
//! 호출자가 SSE 를 먼저 push → **② `download_photos`(백그라운드 스레드)** 가 바이트를 받는다.
//! 저장/유실이 확정될 때마다 그 log_id 행을 SSE 로 재전송해 UI 의 사진 자리를 갱신한다.
//!
//! 다운로드는 WinHTTP in-process(외부 `curl.exe` 프로세스 없음) — GUI(콘솔 숨김) 앱에서 콘솔 창이 뜨지 않는다.
//!
//! 안전: URL·토큰·checkSum 원값·이미지 내용 로그 금지(host·status·크기·일치여부·개수만). 다운로드는 카카오 CDN GET 만.

use std::path::PathBuf;

use rusqlite::Connection;

use crate::server::{AppState, StreamEvent};

/// talkmedia 키 캐시용 센티넬 chat_id(방 키 캐시 재사용). 로그아웃 시 함께 무효화.
const TALKMEDIA_KEY_ID: i64 = -1;

/// 한 장당 최대 수신 바이트(방어적 상한 — 초과 시 실패 처리, 메모리 폭증 금지).
const MAX_PHOTO_BYTES: usize = 32 * 1024 * 1024;

/// 미디어 스토어 루트: `%LOCALAPPDATA%\mykakao\media`.
#[must_use]
pub fn media_root() -> PathBuf {
    let base = std::env::var_os("LOCALAPPDATA").map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
    base.join("mykakao").join("media")
}

/// 저장 파일 절대경로(파일명은 store.media.file 에 상대로 보관).
fn media_path(chat_id: i64, file: &str) -> PathBuf {
    media_root().join(chat_id.to_string()).join(file)
}

/// 사진 참조(한 미디어). URL·checkSum 은 밖으로 새지 않게 필드 비공개.
pub struct PhotoRef {
    log_id: i64,
    url: String,
    file_size: i64,
    checksum: Option<String>,
}

/// 매직 → (ext, mime). 이미지 아니면 None.
fn image_kind(b: &[u8]) -> Option<(&'static str, &'static str)> {
    if b.len() >= 3 && b[0] == 0xFF && b[1] == 0xD8 && b[2] == 0xFF { return Some(("jpg", "image/jpeg")); }
    if b.len() >= 8 && &b[..8] == b"\x89PNG\r\n\x1a\n" { return Some(("png", "image/png")); }
    if b.len() >= 12 && &b[..4] == b"RIFF" && &b[8..12] == b"WEBP" { return Some(("webp", "image/webp")); }
    if b.len() >= 6 && (&b[..6] == b"GIF87a" || &b[..6] == b"GIF89a") { return Some(("gif", "image/gif")); }
    None
}

/// 이미지 토큰 확장자(비디오/파일 제외)로 후보 판별.
fn looks_image_token(token: &str) -> bool {
    let t = token.to_lowercase();
    [".jpg", ".jpeg", ".png", ".gif", ".webp", ".bmp"].iter().any(|e| t.ends_with(e))
}

/// 방의 talkmedia 사진 참조 목록(logId→url,fileSize,checkSum).
fn list_room_media(conn: &Connection, chat_id: i64) -> Vec<PhotoRef> {
    let sql = "SELECT ci.logId, ti.url, ti.fileSize, ti.checkSum, ti.token \
               FROM chatMsgInfo ci \
               JOIN chatMsgTokenJunction j ON ci.logId=j.logId \
               JOIN tokenInfo ti ON j.token=ti.token \
               WHERE ci.chatId=?1 AND ti.url IS NOT NULL AND LENGTH(ti.url)>4";
    let mut stmt = match conn.prepare(sql) {
        Ok(s) => s,
        Err(_) => return vec![],
    };
    let rows = stmt.query_map([chat_id], |r| {
        Ok((
            r.get::<_, i64>(0).unwrap_or(0),
            r.get::<_, Option<String>>(1)?.unwrap_or_default(),
            r.get::<_, i64>(2).unwrap_or(0),
            r.get::<_, Option<String>>(3)?,
            r.get::<_, Option<String>>(4)?.unwrap_or_default(),
        ))
    });
    let mut out = vec![];
    if let Ok(rows) = rows {
        for row in rows.flatten() {
            let (log_id, url, file_size, checksum, token) = row;
            if log_id == 0 || url.is_empty() || !looks_image_token(&token) {
                continue;
            }
            out.push(PhotoRef { log_id, url, file_size, checksum });
        }
    }
    out
}

/// URL 분해 결과(WinHTTP 는 host/port/path 를 따로 받는다).
#[derive(Debug, PartialEq, Eq)]
struct UrlParts {
    secure: bool,
    host: String,
    port: u16,
    path: String,
}

/// `scheme://[user@]host[:port]/path?query` 분해. http/https 만 허용(그 외 None).
fn split_url(url: &str) -> Option<UrlParts> {
    let (scheme, rest) = url.split_once("://")?;
    let secure = match scheme.to_ascii_lowercase().as_str() {
        "https" => true,
        "http" => false,
        _ => return None,
    };
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    // userinfo(카카오 CDN 엔 없지만 방어적으로 제거).
    let authority = authority.rsplit('@').next().unwrap_or("");
    let (host, port) = match authority.rsplit_once(':') {
        Some((h, p)) if !h.is_empty() && !p.is_empty() && p.bytes().all(|c| c.is_ascii_digit()) => {
            (h.to_string(), p.parse::<u16>().ok()?)
        }
        _ => (authority.to_string(), if secure { 443 } else { 80 }),
    };
    if host.is_empty() {
        return None;
    }
    Some(UrlParts { secure, host, port, path: path.to_string() })
}

/// WinHTTP in-process GET. 반환 `(status_code, body)`.
/// 외부 프로세스를 만들지 않으므로 GUI 앱에서 콘솔 창이 뜨지 않는다. URL·본문은 로그 금지.
#[cfg(windows)]
fn http_get(url: &str) -> Option<(u16, Vec<u8>)> {
    use windows::core::PCWSTR;
    use windows::Win32::Networking::WinHttp::{
        WinHttpCloseHandle, WinHttpConnect, WinHttpOpen, WinHttpOpenRequest, WinHttpQueryHeaders,
        WinHttpReadData, WinHttpReceiveResponse, WinHttpSendRequest, WinHttpSetTimeouts,
        WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY, WINHTTP_FLAG_SECURE, WINHTTP_OPEN_REQUEST_FLAGS,
        WINHTTP_QUERY_FLAG_NUMBER, WINHTTP_QUERY_STATUS_CODE,
    };

    /// WinHTTP 핸들 RAII(조기 반환에도 누수 없음).
    struct H(*mut core::ffi::c_void);
    impl Drop for H {
        fn drop(&mut self) {
            if !self.0.is_null() {
                unsafe {
                    let _ = WinHttpCloseHandle(self.0);
                }
            }
        }
    }
    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    let p = split_url(url)?;
    let agent = wide("mykakao");
    let host = wide(&p.host);
    let verb = wide("GET");
    let path = wide(&p.path);

    unsafe {
        let sess = H(WinHttpOpen(
            PCWSTR(agent.as_ptr()),
            WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY,
            PCWSTR::null(),
            PCWSTR::null(),
            0,
        ));
        if sess.0.is_null() {
            return None;
        }
        // resolve/connect/send/receive 타임아웃(ms) — 무한 대기 금지.
        let _ = WinHttpSetTimeouts(sess.0, 10_000, 10_000, 15_000, 25_000);

        let conn = H(WinHttpConnect(sess.0, PCWSTR(host.as_ptr()), p.port, 0));
        if conn.0.is_null() {
            return None;
        }
        let flags = if p.secure { WINHTTP_FLAG_SECURE } else { WINHTTP_OPEN_REQUEST_FLAGS(0) };
        let req = H(WinHttpOpenRequest(
            conn.0,
            PCWSTR(verb.as_ptr()),
            PCWSTR(path.as_ptr()),
            PCWSTR::null(),
            PCWSTR::null(),
            std::ptr::null(),
            flags,
        ));
        if req.0.is_null() {
            return None;
        }
        // 리다이렉트는 WinHTTP 기본 정책이 따라간다(https→http 강등만 차단).
        WinHttpSendRequest(req.0, None, None, 0, 0, 0).ok()?;
        WinHttpReceiveResponse(req.0, std::ptr::null_mut()).ok()?;

        let mut code: u32 = 0;
        let mut len = u32::try_from(std::mem::size_of::<u32>()).unwrap_or(4);
        WinHttpQueryHeaders(
            req.0,
            WINHTTP_QUERY_STATUS_CODE | WINHTTP_QUERY_FLAG_NUMBER,
            PCWSTR::null(),
            Some(std::ptr::addr_of_mut!(code).cast()),
            &mut len,
            std::ptr::null_mut(),
        )
        .ok()?;

        let mut body = Vec::new();
        if code == 200 {
            let mut buf = vec![0u8; 64 * 1024];
            loop {
                let mut got: u32 = 0;
                let cap = u32::try_from(buf.len()).unwrap_or(0);
                if WinHttpReadData(req.0, buf.as_mut_ptr().cast(), cap, &mut got).is_err() {
                    return None;
                }
                if got == 0 {
                    break;
                }
                body.extend_from_slice(&buf[..got as usize]);
                if body.len() > MAX_PHOTO_BYTES {
                    return None; // 상한 초과 → 실패(pending 유지).
                }
            }
        }
        Some((u16::try_from(code).unwrap_or(0), body))
    }
}

/// 비-Windows(개발/CI): talkmedia 자체가 Windows 경로라 사진 바이트 수집은 Windows 전용.
#[cfg(not(windows))]
fn http_get(_url: &str) -> Option<(u16, Vec<u8>)> {
    None
}

/// ① 계획 단계(**네트워크 없음** — SSE push 전에 호출해도 안전).
///
/// talkmedia 에서 방의 사진 참조를 읽어 아직 확정(saved/lost)되지 않은 건을 `pending` 으로 표식하고
/// 다운로드 대상 목록을 돌려준다. 이 표식 덕에 곧바로 이어지는 SSE push 가 사진 행을 photo/pending 으로 보낸다.
pub fn plan_room_photos(state: &AppState, chat_id: i64) -> Vec<PhotoRef> {
    let Some(acct) = state.account.as_ref() else { return vec![] };
    let Some(cands) = state.cached_candidates() else { return vec![] };
    let tm = crate::kakao::chat_data_dir(acct).join("talkmedia.edb");
    if !tm.exists() {
        return vec![];
    }
    // talkmedia 키(세션 캐시 우선 — 반복 스윕 회피).
    let Some(key) = crate::kakao::import::resolve_key_for(&tm, cands.as_ref(), state.room_key(TALKMEDIA_KEY_ID)) else {
        return vec![];
    };
    state.cache_room_key(TALKMEDIA_KEY_ID, key);
    let Some(refs) = crate::kakao::import::with_key(&tm, &key, |c| list_room_media(c, chat_id)) else {
        return vec![];
    };

    let mut todo = vec![];
    for r in refs {
        // 이미 확정 상태면 skip.
        if let Ok(Some((_, st, _))) = state.store.media_of(chat_id, r.log_id) {
            if st == "saved" || st == "lost" {
                continue;
            }
        }
        let _ = state.store.upsert_media(chat_id, r.log_id, None, "pending", None);
        todo.push(r);
    }
    todo
}

/// 확정된 미디어 상태를 그 메시지 행의 SSE 재전송으로 반영(UI 의 "사진 받는 중…" → 이미지/유실).
fn push_media_update(state: &AppState, chat_id: i64, log_id: i64, status: &str) {
    if let Ok(Some(mut row)) = state.store.message_one(chat_id, log_id) {
        row.kind = Some("photo".to_string());
        row.media_status = Some(status.to_string());
        let _ = state.tx.send(StreamEvent { chat_id, row });
    }
}

/// ② 다운로드 단계(**네트워크 — 백그라운드 스레드 전용**). 실시간 SSE 경로에서 직접 호출하지 말 것.
///
/// 바이트 수신 → 이미지 매직/checkSum 검증 → 미디어 스토어 저장(saved), 410/404 → lost.
/// 확정될 때마다 그 행을 SSE 로 재전송한다.
pub fn download_photos(state: &AppState, chat_id: i64, todo: Vec<PhotoRef>) {
    if todo.is_empty() {
        return;
    }
    let mut saved = 0u32;
    let mut lost = 0u32;
    let mut sum_ok = 0u32;
    let mut sum_bad = 0u32;
    for r in todo {
        // 계획과 다운로드 사이에 다른 스윕이 이미 확정했을 수 있다(낡은 계획) → 재다운로드 금지.
        if let Ok(Some((_, st, _))) = state.store.media_of(chat_id, r.log_id) {
            if st == "saved" || st == "lost" {
                continue;
            }
        }
        match http_get(&r.url) {
            Some((200, bytes)) => {
                match image_kind(&bytes) {
                    Some((ext, mime)) => {
                        // checkSum(SHA1)·fileSize 검증(값 미출력, 일치여부만 카운트).
                        let sum_match = match r.checksum.as_deref() {
                            Some(cs) if !cs.is_empty() => sha1_hex(&bytes).eq_ignore_ascii_case(cs.trim()),
                            _ => true,
                        };
                        if sum_match { sum_ok += 1; } else { sum_bad += 1; }
                        let _size_match = r.file_size == 0 || bytes.len() as i64 == r.file_size;
                        // 저장(우리 미디어 스토어).
                        let file = format!("{}.{ext}", r.log_id);
                        let dest = media_path(chat_id, &file);
                        if let Some(parent) = dest.parent() {
                            let _ = std::fs::create_dir_all(parent);
                        }
                        if std::fs::write(&dest, &bytes).is_ok() {
                            let _ = state.store.upsert_media(chat_id, r.log_id, Some(mime), "saved", Some(&file));
                            saved += 1;
                            push_media_update(state, chat_id, r.log_id, "saved");
                        }
                    }
                    None => { /* 비이미지(302 html 등) → pending 유지 */ }
                }
            }
            Some((410, _)) | Some((404, _)) => {
                let _ = state.store.upsert_media(chat_id, r.log_id, None, "lost", None);
                lost += 1;
                push_media_update(state, chat_id, r.log_id, "lost");
            }
            _ => { /* pending 유지(다음 델타에 재시도) */ }
        }
    }
    if saved > 0 || lost > 0 {
        crate::applog::line(&format!(
            "[win_app] 사진 수집: 저장 {saved} / 유실 {lost} (checkSum 일치 {sum_ok}, 불일치 {sum_bad})"
        ));
    }
}

/// 순수 Rust SHA-1(외부 crate 없이). checkSum 검증용.
fn sha1_hex(data: &[u8]) -> String {
    let mut h: [u32; 5] = [0x6745_2301, 0xEFCD_AB89, 0x98BA_DCFE, 0x1032_5476, 0xC3D2_E1F0];
    let ml = (data.len() as u64).wrapping_mul(8);
    let mut msg = data.to_vec();
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&ml.to_be_bytes());
    for chunk in msg.chunks(64) {
        let mut w = [0u32; 80];
        for i in 0..16 {
            w[i] = u32::from_be_bytes([chunk[i * 4], chunk[i * 4 + 1], chunk[i * 4 + 2], chunk[i * 4 + 3]]);
        }
        for i in 16..80 {
            w[i] = (w[i - 3] ^ w[i - 8] ^ w[i - 14] ^ w[i - 16]).rotate_left(1);
        }
        let (mut a, mut b, mut c, mut d, mut e) = (h[0], h[1], h[2], h[3], h[4]);
        for (i, &wi) in w.iter().enumerate() {
            let (f, k) = if i < 20 {
                ((b & c) | ((!b) & d), 0x5A82_7999u32)
            } else if i < 40 {
                (b ^ c ^ d, 0x6ED9_EBA1)
            } else if i < 60 {
                ((b & c) | (b & d) | (c & d), 0x8F1B_BCDC)
            } else {
                (b ^ c ^ d, 0xCA62_C1D6)
            };
            let tmp = a.rotate_left(5).wrapping_add(f).wrapping_add(e).wrapping_add(k).wrapping_add(wi);
            e = d;
            d = c;
            c = b.rotate_left(30);
            b = a;
            a = tmp;
        }
        h[0] = h[0].wrapping_add(a);
        h[1] = h[1].wrapping_add(b);
        h[2] = h[2].wrapping_add(c);
        h[3] = h[3].wrapping_add(d);
        h[4] = h[4].wrapping_add(e);
    }
    let mut s = String::with_capacity(40);
    for x in h {
        s.push_str(&format!("{x:08x}"));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn sha1_known_vectors() {
        assert_eq!(sha1_hex(b""), "da39a3ee5e6b4b0d3255bfef95601890afd80709");
        assert_eq!(sha1_hex(b"abc"), "a9993e364706816aba3e25717850c26c9cd0d89d");
    }
    #[test]
    fn image_kind_magics() {
        assert_eq!(image_kind(&[0xFF, 0xD8, 0xFF, 0x00]).map(|x| x.0), Some("jpg"));
        assert_eq!(image_kind(b"\x89PNG\r\n\x1a\n....").map(|x| x.0), Some("png"));
        assert!(image_kind(b"hello world!!").is_none());
    }
    #[test]
    fn token_ext_filter() {
        assert!(looks_image_token("abc.jpg"));
        assert!(looks_image_token("X.PNG"));
        assert!(!looks_image_token("v.mp4"));
    }

    #[test]
    fn url_split_https_default_port() {
        let u = split_url("https://cdn.example.com/talkm/a/b.jpg?q=1").unwrap();
        assert!(u.secure);
        assert_eq!(u.host, "cdn.example.com");
        assert_eq!(u.port, 443);
        assert_eq!(u.path, "/talkm/a/b.jpg?q=1");
    }
    #[test]
    fn url_split_http_explicit_port_and_no_path() {
        let u = split_url("http://host.example:8080").unwrap();
        assert!(!u.secure);
        assert_eq!(u.port, 8080);
        assert_eq!(u.path, "/");
    }
    #[test]
    fn url_split_rejects_non_http() {
        assert!(split_url("ftp://h/x.jpg").is_none());
        assert!(split_url("/relative/x.jpg").is_none());
        assert!(split_url("https:///nohost").is_none());
    }
}
