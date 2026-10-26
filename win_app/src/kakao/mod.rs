//! KakaoTalk 로컬 경로 탐색 · 대상 DB 열거 · (하위) 키회수/복호/임포트.
//!
//! 계정 폴더는 `%LOCALAPPDATA%\Kakao\KakaoTalk\users\<40hex>` 를 **자동 탐색**한다(하드코딩 금지).
//! 방 DB 는 `chat_data\chatLogs_<chatId>.edb`. 원본은 **읽기 전용**.

pub mod decrypt;
pub mod import;
pub mod memkey;
pub mod photo;

use std::path::{Path, PathBuf};

use crate::kakao::decrypt::{PAGE_SZ, SQLITE_MAGIC};

/// 한 방의 원본 DB 참조.
#[derive(Debug, Clone)]
pub struct RoomFile {
    pub chat_id: i64,
    pub path: PathBuf,
    pub salt: [u8; 16],
}

/// `%LOCALAPPDATA%` 반환(환경변수).
fn local_appdata() -> Option<PathBuf> {
    std::env::var_os("LOCALAPPDATA").map(PathBuf::from)
}

/// `users/` 아래 40-hex 계정 폴더를 자동 탐색(첫 매칭).
#[must_use]
pub fn discover_account_dir() -> Option<PathBuf> {
    let base = local_appdata()?
        .join("Kakao")
        .join("KakaoTalk")
        .join("users");
    let rd = std::fs::read_dir(&base).ok()?;
    for e in rd.flatten() {
        let name = e.file_name();
        let name = name.to_string_lossy();
        if name.len() == 40 && name.bytes().all(|b| b.is_ascii_hexdigit()) {
            let p = e.path();
            if p.is_dir() {
                return Some(p);
            }
        }
    }
    None
}

/// 계정 해시(계정 폴더명)의 마스킹 지문(로그용). 원값은 남기지 않는다.
#[must_use]
pub fn account_fingerprint(acct: &Path) -> Option<String> {
    let name = acct.file_name()?.to_string_lossy().to_string();
    Some(memkey::kfp(name.as_bytes()))
}

/// `chat_data` 디렉토리 경로.
#[must_use]
pub fn chat_data_dir(acct: &Path) -> PathBuf {
    acct.join("chat_data")
}

/// 로그인 계정 이메일: `HKCU\Software\Kakao\KakaoTalk\UserAccounts` 의 (첫) 서브키 이름.
/// 본인 닉네임이 비었을 때 **본인 식별자**로 화면 표시하려는 용도.
/// 레지스트리 **읽기만**(카톡 무변조). 반환 값(이메일)은 **로그 금지**(본인 기기·화면 표시용).
#[must_use]
pub fn account_email() -> Option<String> {
    use windows::core::{PCWSTR, PWSTR};
    use windows::Win32::Foundation::ERROR_SUCCESS;
    use windows::Win32::System::Registry::{
        RegCloseKey, RegEnumKeyExW, RegOpenKeyExW, HKEY, HKEY_CURRENT_USER, KEY_READ,
    };
    let sub: Vec<u16> = "Software\\Kakao\\KakaoTalk\\UserAccounts\0".encode_utf16().collect();
    // SAFETY: 읽기 전용으로 HKCU 서브키를 열고, 첫 서브키 이름만 열거 후 핸들을 닫는다.
    unsafe {
        let mut hk = HKEY::default();
        if RegOpenKeyExW(HKEY_CURRENT_USER, PCWSTR(sub.as_ptr()), 0, KEY_READ, &mut hk)
            != ERROR_SUCCESS
        {
            return None;
        }
        let mut name = [0u16; 256];
        let mut len = name.len() as u32;
        let r = RegEnumKeyExW(
            hk,
            0,
            PWSTR(name.as_mut_ptr()),
            &mut len,
            None,
            PWSTR::null(),
            None,
            None,
        );
        let _ = RegCloseKey(hk);
        if r == ERROR_SUCCESS {
            let s = String::from_utf16_lossy(&name[..len as usize]);
            let t = s.trim();
            if !t.is_empty() {
                return Some(t.to_string());
            }
        }
    }
    None
}

/// 파일명 `chatLogs_<chatId>.edb` 에서 chatId 파싱.
fn parse_chat_id(file_name: &str) -> Option<i64> {
    let stem = file_name.strip_prefix("chatLogs_")?.strip_suffix(".edb")?;
    stem.parse::<i64>().ok()
}

/// 원본 파일 page1(4096B)을 읽는다(읽기 전용). 암호화 파일이 아니면 `None`.
#[must_use]
pub fn read_encrypted_page1(path: &Path) -> Option<[u8; PAGE_SZ]> {
    use std::io::Read;
    let mut f = std::fs::File::open(path).ok()?;
    let mut buf = [0u8; PAGE_SZ];
    f.read_exact(&mut buf).ok()?;
    // 평문 SQLite 는 대상 아님(첫 16B == 매직이면 스킵).
    if &buf[..16] == SQLITE_MAGIC {
        return None;
    }
    Some(buf)
}

/// `chat_data` 의 모든 `chatLogs_*.edb` 를 열거(암호화된 것만, salt 포함).
#[must_use]
pub fn list_room_files(acct: &Path) -> Vec<RoomFile> {
    let dir = chat_data_dir(acct);
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir(&dir) else {
        return out;
    };
    for e in rd.flatten() {
        let fname = e.file_name();
        let fname = fname.to_string_lossy();
        let Some(chat_id) = parse_chat_id(&fname) else {
            continue;
        };
        let path = e.path();
        let Some(page1) = read_encrypted_page1(&path) else {
            continue;
        };
        let mut salt = [0u8; 16];
        salt.copy_from_slice(&page1[..16]);
        out.push(RoomFile {
            chat_id,
            path,
            salt,
        });
    }
    out.sort_by_key(|r| r.chat_id);
    out
}

/// 한 번의 메모리 스냅샷으로 raw key 후보 집합을 수집(import/enrich/watch 공용).
///
/// 키는 상주시키지 않는다 — 호출 때마다 회수하고 반환된 후보는 사용 후 폐기(호출측 스코프).
#[must_use]
pub fn harvest_candidates(acct: &Path) -> Option<std::collections::HashSet<memkey::RawKey>> {
    let pid = memkey::find_pid(memkey::PROC_NAME)?;
    let rooms = list_room_files(acct);
    let salts: Vec<[u8; 16]> = rooms.iter().map(|r| r.salt).collect();
    memkey::harvest(pid, &salts, true).ok().map(|h| h.candidates)
}

/// `TalkUserDB.edb`(userId→nickName) 경로. 닉네임 조인용. 존재하지 않을 수 있다.
#[must_use]
pub fn talk_user_db_path(acct: &Path) -> Option<PathBuf> {
    for cand in [chat_data_dir(acct).join("TalkUserDB.edb"), acct.join("TalkUserDB.edb")] {
        if cand.exists() {
            return Some(cand);
        }
    }
    None
}

/// `chatListInfo.edb`(방 목록/제목) 경로. 존재하지 않을 수 있다.
#[must_use]
pub fn chat_list_info_path(acct: &Path) -> Option<PathBuf> {
    let p = chat_data_dir(acct).join("chatListInfo.edb");
    if p.exists() {
        return Some(p);
    }
    // 일부 버전은 계정 루트에 둔다.
    let p2 = acct.join("chatListInfo.edb");
    p2.exists().then_some(p2)
}
