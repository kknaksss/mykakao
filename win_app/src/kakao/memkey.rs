//! 실행 중 KakaoTalk 메모리에서 SQLCipher raw key(32B)를 **passive** 회수한다.
//!
//! spike3 `key_recover.py` 포팅. `ReadProcessMemory` **읽기만**(주입·쓰기 없음 → SAC 무관).
//! PRIVATE·COMMIT·RW 영역에서 고엔트로피 32B 후보를 모으고, 대상 DB 의 page1 로
//! SQLCipher v4 HMAC 검증(→ [`crate::kakao::decrypt::verify_sqlcipher4`])하여 진짜 키를 판정한다.
//!
//! 안전: 키 원값은 반환값(메모리)로만 다루고 로그/에러엔 지문(`kfp`)만 남긴다.

use std::collections::HashSet;

use crate::kakao::decrypt::{high_entropy_32, verify_sqlcipher4};

/// KakaoTalk 프로세스 이름.
pub const PROC_NAME: &str = "KakaoTalk.exe";

/// 회수 후보 raw key 32B.
pub type RawKey = [u8; 32];
/// SQLCipher 파일 salt(page1 첫 16B).
pub type Salt = [u8; 16];

/// 한 번의 메모리 스냅샷 결과.
pub struct Harvest {
    /// 고엔트로피 32B 후보(키 판정 대상). `collect_candidates=false` 면 비어 있다.
    pub candidates: HashSet<RawKey>,
    /// 스냅샷에 상주(=DB 가 열려 있어 키 회수 가능)한 대상 salt 들.
    pub resident_salts: HashSet<Salt>,
    /// 스캔한 바이트 총량(진단용).
    pub scanned: usize,
}

/// 값 노출 없이 로그에 쓸 8-hex 지문(SHA-256 앞 8자리).
#[must_use]
pub fn kfp(b: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let d = Sha256::digest(b);
    hex8(&d[..4])
}

fn hex8(b: &[u8]) -> String {
    let mut s = String::with_capacity(b.len() * 2);
    for x in b {
        s.push_str(&format!("{x:02x}"));
    }
    s
}

/// 후보들 중 대상 DB page1 의 진짜 raw key 를 찾는다(HMAC 검증, 첫 매칭 반환).
#[must_use]
pub fn find_key_for(candidates: &HashSet<RawKey>, page1: &[u8]) -> Option<RawKey> {
    candidates
        .iter()
        .find(|k| verify_sqlcipher4(page1, *k))
        .copied()
}

#[cfg(windows)]
mod win {
    use super::*;
    use std::ffi::c_void;
    use windows::Win32::Foundation::{CloseHandle, HANDLE};
    use windows::Win32::System::Diagnostics::Debug::ReadProcessMemory;
    use windows::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
        TH32CS_SNAPPROCESS,
    };
    use windows::Win32::System::Memory::{
        VirtualQueryEx, MEMORY_BASIC_INFORMATION, MEM_COMMIT, MEM_PRIVATE, PAGE_GUARD,
    };
    use windows::Win32::System::Threading::{
        OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_VM_READ,
    };

    const CHUNK: usize = 8 * 1024 * 1024;
    const MAX_ADDR: usize = 0x7FFF_FFFF_0000;

    /// 이름으로 프로세스 PID 조회(첫 매칭). Toolhelp 스냅샷.
    pub fn find_pid(name: &str) -> Option<u32> {
        // SAFETY: 스냅샷 핸들은 성공 시 유효하며 아래에서 CloseHandle 한다.
        let snap = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) }.ok()?;
        let mut entry = PROCESSENTRY32W {
            dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
            ..Default::default()
        };
        let mut found = None;
        // SAFETY: entry 는 dwSize 설정 완료, snap 은 유효.
        unsafe {
            if Process32FirstW(snap, &mut entry).is_ok() {
                loop {
                    let exe = String::from_utf16_lossy(
                        &entry.szExeFile[..entry
                            .szExeFile
                            .iter()
                            .position(|&c| c == 0)
                            .unwrap_or(entry.szExeFile.len())],
                    );
                    if exe.eq_ignore_ascii_case(name) {
                        found = Some(entry.th32ProcessID);
                        break;
                    }
                    if Process32NextW(snap, &mut entry).is_err() {
                        break;
                    }
                }
            }
            let _ = CloseHandle(snap);
        }
        found
    }

    /// KakaoTalk 실행 여부.
    pub fn is_running() -> bool {
        find_pid(PROC_NAME).is_some()
    }

    /// PRIVATE·COMMIT·RW(PAGE_READWRITE|PAGE_WRITECOPY, GUARD 제외) 영역만 읽어
    /// 후보 32B 수집 + 대상 salt 상주 여부 판정. **읽기 전용**.
    pub fn harvest(
        pid: u32,
        target_salts: &[Salt],
        collect_candidates: bool,
    ) -> anyhow::Result<Harvest> {
        // SAFETY: 접근권한 = QUERY_LIMITED_INFORMATION|VM_READ (읽기 전용). 핸들은 아래서 닫는다.
        let handle: HANDLE = unsafe {
            OpenProcess(
                PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_VM_READ,
                false,
                pid,
            )
        }
        .map_err(|e| anyhow::anyhow!("OpenProcess 실패: {e}"))?;

        // salt 빠른 프리필터용 first-byte 테이블 + 정확 매칭용 set.
        let mut first_byte = [false; 256];
        let mut salt_set: HashSet<Salt> = HashSet::new();
        for s in target_salts {
            first_byte[s[0] as usize] = true;
            salt_set.insert(*s);
        }

        let mut candidates: HashSet<RawKey> = HashSet::new();
        let mut resident_salts: HashSet<Salt> = HashSet::new();
        let mut scanned: usize = 0;
        let mut buf = vec![0u8; CHUNK];

        let mut addr: usize = 0;
        while addr < MAX_ADDR {
            let mut mbi = MEMORY_BASIC_INFORMATION::default();
            // SAFETY: handle 유효, mbi 는 스택 버퍼, 크기 정확.
            let got = unsafe {
                VirtualQueryEx(
                    handle,
                    Some(addr as *const c_void),
                    &mut mbi,
                    std::mem::size_of::<MEMORY_BASIC_INFORMATION>(),
                )
            };
            if got == 0 {
                break;
            }
            let base = mbi.BaseAddress as usize;
            let region = if mbi.RegionSize == 0 { 0x1000 } else { mbi.RegionSize };

            let protect = mbi.Protect.0;
            let low = protect & 0xFF;
            let is_rw = low == 0x04 || low == 0x08; // PAGE_READWRITE | PAGE_WRITECOPY
            let no_guard = (protect & PAGE_GUARD.0) == 0;
            if mbi.State == MEM_COMMIT && mbi.Type == MEM_PRIVATE && is_rw && no_guard {
                let mut off = 0usize;
                while off < region {
                    let n = std::cmp::min(CHUNK, region - off);
                    let mut read: usize = 0;
                    // SAFETY: 대상 주소는 위 필터로 COMMIT 확인됨. 실패 시 Err → 스킵.
                    let ok = unsafe {
                        ReadProcessMemory(
                            handle,
                            (base + off) as *const c_void,
                            buf.as_mut_ptr() as *mut c_void,
                            n,
                            Some(&mut read),
                        )
                    }
                    .is_ok();
                    if ok && read > 0 {
                        scan_chunk(
                            &buf[..read],
                            collect_candidates,
                            &first_byte,
                            &salt_set,
                            &mut candidates,
                            &mut resident_salts,
                        );
                        scanned += read;
                    }
                    off += n;
                }
            }
            addr = base.saturating_add(region);
            if addr == 0 {
                break;
            }
        }

        // SAFETY: handle 유효.
        unsafe {
            let _ = CloseHandle(handle);
        }
        Ok(Harvest {
            candidates,
            resident_salts,
            scanned,
        })
    }

    fn scan_chunk(
        b: &[u8],
        collect_candidates: bool,
        first_byte: &[bool; 256],
        salt_set: &HashSet<Salt>,
        candidates: &mut HashSet<RawKey>,
        resident_salts: &mut HashSet<Salt>,
    ) {
        // 후보 32B(4B step).
        if collect_candidates && b.len() >= 32 {
            let mut i = 0usize;
            let end = b.len() - 32;
            while i <= end {
                let w = &b[i..i + 32];
                if high_entropy_32(w) {
                    let mut k = [0u8; 32];
                    k.copy_from_slice(w);
                    candidates.insert(k);
                }
                i += 4;
            }
        }
        // salt 상주(모든 offset, first-byte 프리필터).
        if !salt_set.is_empty() && b.len() >= 16 {
            let end = b.len() - 16;
            for i in 0..=end {
                if first_byte[b[i] as usize] {
                    let w = &b[i..i + 16];
                    let mut s = [0u8; 16];
                    s.copy_from_slice(w);
                    if salt_set.contains(&s) {
                        resident_salts.insert(s);
                    }
                }
            }
        }
    }
}

#[cfg(windows)]
pub use win::{find_pid, harvest, is_running};

// 비-Windows(개발/CI) 스텁 — 컴파일만 통과. 실기동은 Windows 전용.
#[cfg(not(windows))]
pub fn find_pid(_name: &str) -> Option<u32> {
    None
}
#[cfg(not(windows))]
pub fn is_running() -> bool {
    false
}
#[cfg(not(windows))]
pub fn harvest(
    _pid: u32,
    _target_salts: &[Salt],
    _collect_candidates: bool,
) -> anyhow::Result<Harvest> {
    anyhow::bail!("메모리 키 회수는 Windows 전용입니다")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kfp_is_stable_and_masked() {
        let a = kfp(&[1u8; 32]);
        assert_eq!(a.len(), 8);
        assert_eq!(a, kfp(&[1u8; 32]));
        assert_ne!(a, kfp(&[2u8; 32]));
    }
}
