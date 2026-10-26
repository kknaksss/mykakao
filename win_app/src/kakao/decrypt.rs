//! SQLCipher v4 페이지 복호 — **순수 Rust 크립토**(OpenSSL 회피).
//!
//! spike3 `key_recover.py` / `KEY_REPORT.md` 의 파라미터 정본을 Rust 로 포팅한다.
//! 회수한 raw key(32B)로 각 페이지를 AES-256-CBC 복호해 **평문 SQLite** 바이트로 재구성한다.
//! 재구성한 평문은 예약영역(reserve 80B)을 유지하므로 `rusqlite`(bundled, 평문)로 그대로 열린다.
//!
//! 파라미터(정본): compat4 · page 4096 · reserve 80(IV16 + HMAC-SHA512 64) ·
//! HMAC key = PBKDF2-HMAC-SHA512(raw_key, salt⊕0x3a, 2, 32) · page IV = 페이지 reserve 앞 16B.
//!
//! 안전: 키·본문 값을 로그/에러에 남기지 않는다. 순수 함수라 합성 픽스처로 테스트한다.

use std::collections::HashMap;

use aes::Aes256;
use cbc::cipher::block_padding::NoPadding;
use cbc::cipher::{BlockDecryptMut, KeyIvInit};
use hmac::{Hmac, Mac};
use sha2::Sha512;

/// SQLCipher v4 페이지 크기(바이트).
pub const PAGE_SZ: usize = 4096;
/// 각 페이지 끝 예약영역: IV(16) + HMAC-SHA512(64).
pub const RESERVE: usize = 80;
/// 페이지 IV 길이.
pub const IV_SZ: usize = 16;
/// HMAC-SHA512 길이.
pub const HMAC_SZ: usize = 64;
/// 평문 SQLite 파일 매직(page1 첫 16B — 암호화 파일에선 salt 가 있던 자리).
pub const SQLITE_MAGIC: &[u8; 16] = b"SQLite format 3\x00";

type Aes256CbcDec = cbc::Decryptor<Aes256>;
type HmacSha512 = Hmac<Sha512>;

/// 복호 관련 에러(값을 노출하지 않는 얇은 표현).
#[derive(Debug, thiserror::Error)]
pub enum DecryptError {
    #[error("입력 파일이 SQLCipher 페이지 배수가 아님(len={0})")]
    BadLength(usize),
    #[error("빈 파일(page1 없음)")]
    Empty,
    #[error("HMAC 검증 실패(page {0}) — 키 불일치 또는 손상")]
    HmacMismatch(u32),
    #[error("AES 블록 정렬 오류(page {0})")]
    BadBlock(u32),
}

/// 고엔트로피 32B 윈도우 판정(메모리에서 raw key 후보 필터). spike3 `high_entropy_32` 포팅.
///
/// 조건: 정확히 32B · 서로 다른 바이트 ≥25 · 0x00 연속 런 <4.
#[must_use]
pub fn high_entropy_32(w: &[u8]) -> bool {
    if w.len() != 32 {
        return false;
    }
    let mut seen = [false; 256];
    let mut distinct = 0usize;
    for &b in w {
        if !seen[b as usize] {
            seen[b as usize] = true;
            distinct += 1;
        }
    }
    if distinct < 25 {
        return false;
    }
    let mut run = 0u32;
    let mut mx = 0u32;
    for &b in w {
        run = if b == 0 { run + 1 } else { 0 };
        if run > mx {
            mx = run;
        }
    }
    mx < 4
}

/// SQLCipher v4 HMAC key 유도: PBKDF2-HMAC-SHA512(raw_key, salt⊕0x3a, 2, 32).
#[must_use]
pub fn derive_hmac_key(raw_key: &[u8], salt: &[u8]) -> [u8; 32] {
    let hmac_salt: Vec<u8> = salt.iter().map(|b| b ^  0x3a).collect();
    let mut out = [0u8; 32];
    pbkdf2::pbkdf2_hmac::<Sha512>(raw_key, &hmac_salt, 2, &mut out);
    out
}

/// SQLCipher v4 page-1 HMAC-SHA512 검증. 맞는 raw key 면 `true`.
///
/// spike3 `verify_sqlcipher4` 포팅: mac_in = page1\[16 .. PAGE_SZ-HMAC_SZ] ++ page_no(1) LE4.
#[must_use]
pub fn verify_sqlcipher4(page1: &[u8], key: &[u8]) -> bool {
    if page1.len() < PAGE_SZ || key.len() != 32 {
        return false;
    }
    let salt = &page1[..16];
    let hk = derive_hmac_key(key, salt);
    let Ok(mut mac) = HmacSha512::new_from_slice(&hk) else {
        return false;
    };
    mac.update(&page1[16..PAGE_SZ - HMAC_SZ]);
    mac.update(&1u32.to_le_bytes());
    mac.verify_slice(&page1[PAGE_SZ - HMAC_SZ..PAGE_SZ]).is_ok()
}

/// 페이지가 전부 0 인지(SQLCipher 는 미사용 free page 를 0 으로 둔다 → 복호하지 않고 통과).
fn is_zero_page(enc_page: &[u8]) -> bool {
    enc_page.iter().all(|&b| b == 0)
}

/// 단일 페이지 복호 → 평문 페이지(항상 PAGE_SZ 바이트). `page_no` 는 1-기반.
///
/// - page1 은 앞 16B 가 salt(평문) → 출력에선 SQLite 매직으로 치환.
/// - 예약영역(reserve 80B)은 평문 출력에서 0 으로 채운다(SQLite 는 예약영역을 읽지 않음).
pub fn decrypt_page(
    enc_page: &[u8],
    raw_key: &[u8],
    salt: &[u8],
    page_no: u32,
) -> Result<Vec<u8>, DecryptError> {
    if enc_page.len() != PAGE_SZ {
        return Err(DecryptError::BadLength(enc_page.len()));
    }
    if is_zero_page(enc_page) {
        return Ok(vec![0u8; PAGE_SZ]);
    }

    let content_start = if page_no == 1 { 16 } else { 0 };
    let iv_off = PAGE_SZ - RESERVE; // 4016
    let hmac_off = PAGE_SZ - HMAC_SZ; // 4032

    // HMAC 검증: mac_in = enc_page[content_start .. hmac_off] ++ page_no LE4.
    let hk = derive_hmac_key(raw_key, salt);
    let mut mac =
        HmacSha512::new_from_slice(&hk).map_err(|_| DecryptError::HmacMismatch(page_no))?;
    mac.update(&enc_page[content_start..hmac_off]);
    mac.update(&page_no.to_le_bytes());
    if mac
        .verify_slice(&enc_page[hmac_off..PAGE_SZ])
        .is_err()
    {
        return Err(DecryptError::HmacMismatch(page_no));
    }

    // AES-256-CBC 복호(패딩 없음): cipher = enc_page[content_start .. iv_off], iv = enc_page[iv_off .. iv_off+16].
    let iv = &enc_page[iv_off..iv_off + IV_SZ];
    let mut buf = enc_page[content_start..iv_off].to_vec();
    if buf.len() % 16 != 0 {
        return Err(DecryptError::BadBlock(page_no));
    }
    let dec = Aes256CbcDec::new(raw_key.into(), iv.into());
    let plain = dec
        .decrypt_padded_mut::<NoPadding>(&mut buf)
        .map_err(|_| DecryptError::BadBlock(page_no))?;

    // 평문 페이지 재구성.
    let mut out = Vec::with_capacity(PAGE_SZ);
    if page_no == 1 {
        out.extend_from_slice(SQLITE_MAGIC);
    }
    out.extend_from_slice(plain);
    out.resize(PAGE_SZ, 0); // 예약영역(reserve)은 0 으로.
    debug_assert_eq!(out.len(), PAGE_SZ);
    Ok(out)
}

/// 암호화된 SQLCipher v4 DB 바이트 전체 → 평문 SQLite 바이트.
///
/// salt 는 page1 첫 16B 에서 취한다(파일 고유). 모든 페이지 동일 salt·raw key 사용.
pub fn decrypt_database(enc: &[u8], raw_key: &[u8]) -> Result<Vec<u8>, DecryptError> {
    if enc.len() < PAGE_SZ {
        return Err(DecryptError::Empty);
    }
    if enc.len() % PAGE_SZ != 0 {
        return Err(DecryptError::BadLength(enc.len()));
    }
    let salt = enc[..16].to_vec();
    let n_pages = enc.len() / PAGE_SZ;
    let mut out = Vec::with_capacity(enc.len());
    for i in 0..n_pages {
        let page_no = (i + 1) as u32;
        let enc_page = &enc[i * PAGE_SZ..(i + 1) * PAGE_SZ];
        let plain = decrypt_page(enc_page, raw_key, &salt, page_no)?;
        out.extend_from_slice(&plain);
    }
    Ok(out)
}

/// SQLite WAL 헤더/프레임헤더 크기.
pub const WAL_HDR: usize = 32;
pub const WAL_FRAME_HDR: usize = 24;
const WAL_MAGIC_LE: u32 = 0x377f_0682;
const WAL_MAGIC_BE: u32 = 0x377f_0683;

fn be32(b: &[u8]) -> u32 {
    u32::from_be_bytes([b[0], b[1], b[2], b[3]])
}

/// SQLite WAL 누적 체크섬(walChecksumBytes). `data` 길이는 8 배수. `big`=데이터 워드를 BE 로 읽음.
fn wal_checksum(big: bool, mut s0: u32, mut s1: u32, data: &[u8]) -> (u32, u32) {
    let mut i = 0;
    while i + 8 <= data.len() {
        let (x0, x1) = if big {
            (be32(&data[i..i + 4]), be32(&data[i + 4..i + 8]))
        } else {
            (
                u32::from_le_bytes([data[i], data[i + 1], data[i + 2], data[i + 3]]),
                u32::from_le_bytes([data[i + 4], data[i + 5], data[i + 6], data[i + 7]]),
            )
        };
        s0 = s0.wrapping_add(x0).wrapping_add(s1);
        s1 = s1.wrapping_add(x1).wrapping_add(s0);
        i += 8;
    }
    (s0, s1)
}

/// 암호화된 main `.edb` + (있으면) `-wal` 을 합쳐 **현재 상태**를 평문 SQLite 로 복호한다.
///
/// 활발한 방은 최근 메시지가 WAL 프레임에 상주(아직 main 으로 checkpoint 안 됨)하므로, main 만
/// 읽으면 0행이 된다. 여기서 유효한(체크섬 통과) WAL 프레임을 마지막 commit 까지 재생(replay)해
/// 페이지별 최신본을 반영한다. SQLCipher v4 는 WAL 프레임의 페이지도 main 과 동일하게 암호화돼 있다.
///
/// 원본 WAL 은 **읽기만**(호출측이 사본). 유효하지 않은 WAL 은 무시하고 main 만 쓴다.
pub fn decrypt_database_with_wal(
    main: &[u8],
    wal: Option<&[u8]>,
    raw_key: &[u8],
) -> Result<Vec<u8>, DecryptError> {
    if main.len() < PAGE_SZ {
        return Err(DecryptError::Empty);
    }
    if main.len() % PAGE_SZ != 0 {
        return Err(DecryptError::BadLength(main.len()));
    }
    let salt = main[..16].to_vec();
    let main_n = main.len() / PAGE_SZ;

    // WAL 재생: 유효 프레임을 마지막 commit 까지 모아 페이지별 최신 복호본.
    let mut wal_pages: HashMap<u32, Vec<u8>> = HashMap::new();
    let mut final_size = 0usize;
    if let Some(w) = wal {
        if w.len() >= WAL_HDR {
            let magic = be32(&w[0..4]);
            let ps = be32(&w[8..12]) as usize;
            if (magic == WAL_MAGIC_LE || magic == WAL_MAGIC_BE) && ps == PAGE_SZ {
                let big = magic == WAL_MAGIC_BE;
                let salt1 = &w[16..20];
                let salt2 = &w[20..24];
                let mut s0 = be32(&w[24..28]);
                let mut s1 = be32(&w[28..32]);
                let frame_sz = WAL_FRAME_HDR + ps;

                // (page_no, dbsize, data_offset) 순서대로.
                let mut frames: Vec<(u32, u32, usize)> = Vec::new();
                let mut last_commit: Option<usize> = None;
                let mut off = WAL_HDR;
                while off + frame_sz <= w.len() {
                    let fh = &w[off..off + WAL_FRAME_HDR];
                    // salt 불일치 = 이전 체크포인트 잔재/torn → 유효 로그 끝.
                    if &fh[8..12] != salt1 || &fh[12..16] != salt2 {
                        break;
                    }
                    let data = &w[off + WAL_FRAME_HDR..off + frame_sz];
                    let (c0, c1) = wal_checksum(big, s0, s1, &fh[0..8]);
                    let (c0, c1) = wal_checksum(big, c0, c1, data);
                    // 저장된 프레임 체크섬(BE). 불일치 = torn/미완 → 끝.
                    if c0 != be32(&fh[16..20]) || c1 != be32(&fh[20..24]) {
                        break;
                    }
                    s0 = c0;
                    s1 = c1;
                    let page_no = be32(&fh[0..4]);
                    let dbsize = be32(&fh[4..8]);
                    frames.push((page_no, dbsize, off + WAL_FRAME_HDR));
                    if dbsize != 0 {
                        last_commit = Some(frames.len() - 1);
                    }
                    off += frame_sz;
                }

                if let Some(lc) = last_commit {
                    final_size = frames[lc].1 as usize;
                    // 마지막 commit 까지의 프레임을 순서대로 적용(뒤 프레임이 앞을 덮음).
                    for &(page_no, _ds, doff) in frames.iter().take(lc + 1) {
                        let enc_page = &w[doff..doff + PAGE_SZ];
                        // WAL 프레임 복호 실패는 비치명적(해당 페이지는 main 폴백).
                        if let Ok(plain) = decrypt_page(enc_page, raw_key, &salt, page_no) {
                            wal_pages.insert(page_no, plain);
                        }
                    }
                }
            }
        }
    }

    let n = if final_size > 0 { final_size } else { main_n };
    let mut out = Vec::with_capacity(n * PAGE_SZ);
    for page_no in 1..=n as u32 {
        if let Some(p) = wal_pages.get(&page_no) {
            out.extend_from_slice(p);
        } else {
            let idx = (page_no - 1) as usize;
            if idx < main_n {
                let enc = &main[idx * PAGE_SZ..(idx + 1) * PAGE_SZ];
                out.extend_from_slice(&decrypt_page(enc, raw_key, &salt, page_no)?);
            } else {
                out.extend(std::iter::repeat(0u8).take(PAGE_SZ));
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use cbc::cipher::block_padding::NoPadding as EncNoPadding;
    use cbc::cipher::BlockEncryptMut;

    type Aes256CbcEnc = cbc::Encryptor<Aes256>;

    /// 테스트 전용: 평문 페이지 → SQLCipher v4 암호화 페이지(복호의 역).
    /// `plain_content` 는 (PAGE_SZ - RESERVE) 바이트(page1 은 앞 16B 가 매직이라 4000B 만 암호화).
    fn encrypt_page_for_test(
        plain_content: &[u8],
        raw_key: &[u8],
        salt: &[u8],
        iv: &[u8; 16],
        page_no: u32,
    ) -> Vec<u8> {
        let content_start = if page_no == 1 { 16 } else { 0 };
        let cipher_len = (PAGE_SZ - RESERVE) - content_start;
        assert_eq!(plain_content.len(), cipher_len);

        let mut buf = plain_content.to_vec();
        let enc = Aes256CbcEnc::new(raw_key.into(), iv.into());
        let ct = enc
            .encrypt_padded_mut::<EncNoPadding>(&mut buf, cipher_len)
            .unwrap()
            .to_vec();

        let mut page = Vec::with_capacity(PAGE_SZ);
        if page_no == 1 {
            page.extend_from_slice(&salt[..16]); // page1 앞 16B = salt
        }
        page.extend_from_slice(&ct); // ciphertext
        page.extend_from_slice(iv); // IV(16)
        assert_eq!(page.len(), PAGE_SZ - HMAC_SZ);

        // HMAC-SHA512 over page[content_start .. PAGE_SZ-HMAC_SZ] ++ page_no LE4.
        let hk = derive_hmac_key(raw_key, salt);
        let mut mac = HmacSha512::new_from_slice(&hk).unwrap();
        mac.update(&page[content_start..]);
        mac.update(&page_no.to_le_bytes());
        page.extend_from_slice(&mac.finalize().into_bytes());
        assert_eq!(page.len(), PAGE_SZ);
        page
    }

    #[test]
    fn high_entropy_filters() {
        assert!(!high_entropy_32(&[0u8; 32])); // distinct 부족
        assert!(!high_entropy_32(&[1u8; 16])); // 길이 아님
        // 32개 서로 다른 바이트 → 통과.
        let w: Vec<u8> = (0u8..32).collect();
        assert!(high_entropy_32(&w));
        // 0x00 4연속 런 포함 → 거부.
        let mut w2: Vec<u8> = (10u8..42).collect();
        w2[5] = 0;
        w2[6] = 0;
        w2[7] = 0;
        w2[8] = 0;
        assert!(!high_entropy_32(&w2));
    }

    #[test]
    fn roundtrip_page1_correct_key() {
        let raw_key = [0x11u8; 32];
        let salt = [0xAB_u8; 16];
        let iv = [0x22u8; 16];
        // page1 평문 content(4000B) — offset 20(=content[4])에 reserve=80 을 흉내낼 필요는 없음(복호 라운드트립만 확인).
        let content: Vec<u8> = (0..(PAGE_SZ - RESERVE - 16)).map(|i| (i % 251) as u8).collect();
        let enc = encrypt_page_for_test(&content, &raw_key, &salt, &iv, 1);

        assert!(verify_sqlcipher4(&enc, &raw_key));
        let plain = decrypt_page(&enc, &raw_key, &salt, 1).unwrap();
        assert_eq!(plain.len(), PAGE_SZ);
        assert_eq!(&plain[..16], SQLITE_MAGIC);
        assert_eq!(&plain[16..PAGE_SZ - RESERVE], &content[..]);
        assert!(plain[PAGE_SZ - RESERVE..].iter().all(|&b| b == 0));
    }

    #[test]
    fn roundtrip_page_n() {
        let raw_key = [0x33u8; 32];
        let salt = [0x5Au8; 16];
        let iv = [0x44u8; 16];
        let content: Vec<u8> = (0..(PAGE_SZ - RESERVE)).map(|i| (i * 7 % 253) as u8).collect();
        let enc = encrypt_page_for_test(&content, &raw_key, &salt, &iv, 2);

        let plain = decrypt_page(&enc, &raw_key, &salt, 2).unwrap();
        assert_eq!(&plain[..PAGE_SZ - RESERVE], &content[..]);
        assert!(plain[PAGE_SZ - RESERVE..].iter().all(|&b| b == 0));
    }

    #[test]
    fn wrong_key_rejected() {
        let raw_key = [0x11u8; 32];
        let salt = [0xAB_u8; 16];
        let iv = [0x22u8; 16];
        let content: Vec<u8> = vec![7u8; PAGE_SZ - RESERVE - 16];
        let enc = encrypt_page_for_test(&content, &raw_key, &salt, &iv, 1);

        let wrong = [0x99u8; 32];
        assert!(!verify_sqlcipher4(&enc, &wrong));
        assert!(matches!(
            decrypt_page(&enc, &wrong, &salt, 1),
            Err(DecryptError::HmacMismatch(1))
        ));
    }

    #[test]
    fn zero_page_passthrough() {
        let raw_key = [0x11u8; 32];
        let salt = [0xAB_u8; 16];
        let zero = vec![0u8; PAGE_SZ];
        let out = decrypt_page(&zero, &raw_key, &salt, 5).unwrap();
        assert!(out.iter().all(|&b| b == 0));
    }

    #[test]
    fn full_database_roundtrip() {
        let raw_key = [0x24u8; 32];
        let salt = [0x77u8; 16];
        let iv1 = [0x01u8; 16];
        let iv2 = [0x02u8; 16];
        let c1: Vec<u8> = (0..(PAGE_SZ - RESERVE - 16)).map(|i| (i % 200) as u8).collect();
        let c2: Vec<u8> = (0..(PAGE_SZ - RESERVE)).map(|i| ((i + 5) % 200) as u8).collect();
        let mut enc = encrypt_page_for_test(&c1, &raw_key, &salt, &iv1, 1);
        enc.extend_from_slice(&encrypt_page_for_test(&c2, &raw_key, &salt, &iv2, 2));

        let plain = decrypt_database(&enc, &raw_key).unwrap();
        assert_eq!(plain.len(), 2 * PAGE_SZ);
        assert_eq!(&plain[..16], SQLITE_MAGIC);
        assert_eq!(&plain[16..PAGE_SZ - RESERVE], &c1[..]);
        assert_eq!(&plain[PAGE_SZ..PAGE_SZ + (PAGE_SZ - RESERVE)], &c2[..]);
    }

    /// 테스트 전용: 암호화된 프레임 페이지들로 유효한 SQLite WAL 을 구성(체크섬 포함).
    /// frames: (page_no, dbsize, enc_page). LE 매직 사용(big=false).
    fn build_wal(frames: &[(u32, u32, Vec<u8>)]) -> Vec<u8> {
        let big = false;
        let salt1 = [0xA1u8; 4];
        let salt2 = [0xB2u8; 4];
        let mut w = Vec::new();
        w.extend_from_slice(&WAL_MAGIC_LE.to_be_bytes());
        w.extend_from_slice(&3_007_000u32.to_be_bytes());
        w.extend_from_slice(&(PAGE_SZ as u32).to_be_bytes());
        w.extend_from_slice(&0u32.to_be_bytes());
        w.extend_from_slice(&salt1);
        w.extend_from_slice(&salt2);
        let (h0, h1) = wal_checksum(big, 0, 0, &w[0..24]);
        w.extend_from_slice(&h0.to_be_bytes());
        w.extend_from_slice(&h1.to_be_bytes());
        let (mut s0, mut s1) = (h0, h1);
        for (page_no, dbsize, enc_page) in frames {
            let mut fh = Vec::new();
            fh.extend_from_slice(&page_no.to_be_bytes());
            fh.extend_from_slice(&dbsize.to_be_bytes());
            fh.extend_from_slice(&salt1);
            fh.extend_from_slice(&salt2);
            let (c0, c1) = wal_checksum(big, s0, s1, &fh[0..8]);
            let (c0, c1) = wal_checksum(big, c0, c1, enc_page);
            fh.extend_from_slice(&c0.to_be_bytes());
            fh.extend_from_slice(&c1.to_be_bytes());
            s0 = c0;
            s1 = c1;
            w.extend_from_slice(&fh);
            w.extend_from_slice(enc_page);
        }
        w
    }

    #[test]
    fn wal_replay_overwrites_committed_pages() {
        let key = [0x5Au8; 32];
        let salt = [0x11u8; 16];
        let c1: Vec<u8> = (0..(PAGE_SZ - RESERVE - 16)).map(|i| (i % 97) as u8).collect();
        let c2_old: Vec<u8> = (0..(PAGE_SZ - RESERVE)).map(|i| (i % 89) as u8).collect();
        let mut main = encrypt_page_for_test(&c1, &key, &salt, &[1u8; 16], 1);
        main.extend_from_slice(&encrypt_page_for_test(&c2_old, &key, &salt, &[2u8; 16], 2));

        let c2_new: Vec<u8> = (0..(PAGE_SZ - RESERVE)).map(|i| ((i * 3 + 7) % 251) as u8).collect();
        let enc2new = encrypt_page_for_test(&c2_new, &key, &salt, &[3u8; 16], 2);

        // 커밋(dbsize=2)된 WAL → page2 는 WAL 신본.
        let wal = build_wal(&[(2u32, 2u32, enc2new.clone())]);
        let plain = decrypt_database_with_wal(&main, Some(&wal), &key).unwrap();
        assert_eq!(plain.len(), 2 * PAGE_SZ);
        assert_eq!(&plain[16..PAGE_SZ - RESERVE], &c1[..]); // page1 그대로
        assert_eq!(&plain[PAGE_SZ..PAGE_SZ + (PAGE_SZ - RESERVE)], &c2_new[..]); // page2=WAL
        assert_ne!(&plain[PAGE_SZ..PAGE_SZ + (PAGE_SZ - RESERVE)], &c2_old[..]);

        // WAL 없음 → main 만(page2=old).
        let p0 = decrypt_database_with_wal(&main, None, &key).unwrap();
        assert_eq!(&p0[PAGE_SZ..PAGE_SZ + (PAGE_SZ - RESERVE)], &c2_old[..]);

        // 미커밋(dbsize=0) 프레임만 → 무시(main 유지).
        let wal_nc = build_wal(&[(2u32, 0u32, enc2new)]);
        let pnc = decrypt_database_with_wal(&main, Some(&wal_nc), &key).unwrap();
        assert_eq!(&pnc[PAGE_SZ..PAGE_SZ + (PAGE_SZ - RESERVE)], &c2_old[..]);
    }

    #[test]
    fn wal_extends_db_with_new_page() {
        let key = [0x42u8; 32];
        let salt = [0x22u8; 16];
        let c1: Vec<u8> = (0..(PAGE_SZ - RESERVE - 16)).map(|i| (i % 71) as u8).collect();
        let main = encrypt_page_for_test(&c1, &key, &salt, &[1u8; 16], 1); // 1 페이지 DB

        // WAL 이 page2 를 추가하고 dbsize=2 로 커밋.
        let c2: Vec<u8> = (0..(PAGE_SZ - RESERVE)).map(|i| ((i + 9) % 200) as u8).collect();
        let enc2 = encrypt_page_for_test(&c2, &key, &salt, &[7u8; 16], 2);
        let wal = build_wal(&[(2u32, 2u32, enc2)]);
        let plain = decrypt_database_with_wal(&main, Some(&wal), &key).unwrap();
        assert_eq!(plain.len(), 2 * PAGE_SZ); // WAL 이 DB 를 2페이지로 확장
        assert_eq!(&plain[PAGE_SZ..PAGE_SZ + (PAGE_SZ - RESERVE)], &c2[..]);
    }
}
