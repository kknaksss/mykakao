"""
mykakao 추출 코어 — 자체 키 유도 + user_id 복구.

복호화 자체는 표준 sqlcipher(파이썬 sqlcipher3 드라이버)가 하고,
이 모듈은 그 sqlcipher 에 넣을 '키'를 우리 손으로 만든다.
kakaocli 런타임 의존 없음 (kakaocli 는 파생식을 알아낸 reference 일 뿐).

키 = f(device UUID, user_id). user_id 는 plist 에서 복구한다.
"""
from __future__ import annotations

import base64
import hashlib
import json
import os
import plistlib
import re
import subprocess
import sys
import tempfile

HOME = os.path.expanduser("~")
CONTAINER = (
    f"{HOME}/Library/Containers/com.kakao.KakaoTalkMac/Data/Library/"
    f"Application Support/com.kakao.KakaoTalkMac"
)
PREFS = (
    f"{HOME}/Library/Containers/com.kakao.KakaoTalkMac/Data/Library/"
    f"Preferences/com.kakao.KakaoTalkMac.plist"
)
CONFIG = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "config.json")
EMPTY_SHA512 = hashlib.sha512(b"0").hexdigest()  # 빈 계정 기본 해시


# ── 기기/파일 탐색 ────────────────────────────────────────────────

def platform_uuid() -> str:
    """ioreg 에서 IOPlatformUUID 를 읽는다."""
    out = subprocess.check_output(
        ["/usr/sbin/ioreg", "-rd1", "-c", "IOPlatformExpertDevice"]
    ).decode(errors="ignore")
    m = re.search(r'"IOPlatformUUID"\s*=\s*"([0-9A-F-]+)"', out)
    if not m:
        raise RuntimeError("IOPlatformUUID 를 찾지 못했다")
    return m.group(1)


def find_db() -> str:
    """컨테이너에서 78자리 hex 메시지 DB 파일을 찾는다."""
    for f in os.listdir(CONTAINER):
        if re.fullmatch(r"[0-9a-f]{78}", f):
            return os.path.join(CONTAINER, f)
    raise RuntimeError(f"메시지 DB 를 찾지 못했다: {CONTAINER}")


# ── 키 유도 (정본: kakaocli KeyDerivation.swift 와 동일) ──────────

def _hashed_uuid(uuid: str) -> str:
    d = uuid.encode()
    return base64.b64encode(hashlib.sha1(d).digest() + hashlib.sha256(d).digest()).decode()


def db_name(user_id: int, uuid: str) -> str:
    """이 (user_id, uuid) 가 만들어야 할 DB 파일명. 검증에 쓴다."""
    hawawa = ".".join([".", "F", str(user_id), "A", "F", uuid[::-1], ".", "|"])
    salt = _hashed_uuid(uuid)[::-1]
    digest = hashlib.pbkdf2_hmac("sha256", hawawa.encode(), salt.encode(), 100_000, 128).hex()
    return digest[28:28 + 78]


def secure_key(user_id: int, uuid: str) -> str:
    """SQLCipher 에 passphrase 로 넣을 256-hex 키."""
    hashed = _hashed_uuid(uuid)
    parts = ["A", hashed, "|", "F", uuid[:5], "H", str(user_id), "|", uuid[7:]]
    hawawa = "F".join(parts)
    salt = uuid[int(len(uuid) * 0.3):]
    return hashlib.pbkdf2_hmac("sha256", hawawa[::-1].encode(), salt.encode(), 100_000, 128).hex()


# ── user_id 복구 ─────────────────────────────────────────────────

def _active_account_hashes() -> list[str]:
    """plist 의 *REVISION:<sha512> 키 중 값이 0 이 아닌(활성 계정) 해시들."""
    with open(PREFS, "rb") as f:
        pl = plistlib.load(f)
    out = []
    for k, v in pl.items():
        m = re.search(r"REVISION:([0-9a-f]{128})$", k)
        if not m or m.group(1) == EMPTY_SHA512:
            continue
        try:
            iv = int(v)
        except (TypeError, ValueError):
            iv = 0
        if iv != 0:
            out.append(m.group(1))
    return out


def _sha512_preimage(hexhash: str, max_id: int = 1_000_000_000) -> int | None:
    """SHA512(str(i)) == hexhash 인 정수 i 를 찾는다. C 가속, 실패 시 파이썬."""
    # 빠른 경로: C(CommonCrypto) 컴파일해서 brute-force (~15초)
    try:
        import shutil
        cc = shutil.which("cc") or shutil.which("clang")
        if cc:
            src = r'''
#include <stdio.h>
#include <string.h>
#include <stdlib.h>
#include <CommonCrypto/CommonDigest.h>
int main(int argc,char**argv){
  unsigned char t[64]; for(int i=0;i<64;i++){unsigned v;sscanf(argv[1]+i*2,"%2x",&v);t[i]=v;}
  long mx=atol(argv[2]); char b[24]; unsigned char h[64];
  for(long i=0;i<mx;i++){int n=sprintf(b,"%ld",i);CC_SHA512(b,n,h);
    if(!memcmp(h,t,64)){printf("%ld",i);return 0;}}
  return 1;}
'''
            d = tempfile.mkdtemp()
            cpath, bpath = os.path.join(d, "bf.c"), os.path.join(d, "bf")
            with open(cpath, "w") as f:
                f.write(src)
            if subprocess.call([cc, "-O3", "-o", bpath, cpath]) == 0:
                r = subprocess.run([bpath, hexhash, str(max_id)], capture_output=True, text=True)
                if r.returncode == 0 and r.stdout.strip().isdigit():
                    return int(r.stdout.strip())
                return None
    except Exception:
        pass
    # 느린 경로: 순수 파이썬
    tgt = bytes.fromhex(hexhash)
    for i in range(max_id):
        if hashlib.sha512(str(i).encode()).digest() == tgt:
            return i
    return None


# ── 부팅: 설정 확보 (uuid + user_id + db_path) ───────────────────

def resolve(force: bool = False) -> dict:
    """uuid / user_id / db_path / key 를 확보한다. config.json 에 캐시한다."""
    uuid = platform_uuid()
    db_path = find_db()
    target = os.path.basename(db_path)

    # 1) 캐시 검증
    if not force and os.path.exists(CONFIG):
        try:
            cfg = json.load(open(CONFIG))
            uid = int(cfg["user_id"])
            if cfg.get("uuid") == uuid and db_name(uid, uuid) == target:
                return _bundle(uid, uuid, db_path)
        except Exception:
            pass

    # 2) plist 활성 계정 해시 → SHA512 preimage → DB 파일명 일치 검증
    for h in _active_account_hashes():
        uid = _sha512_preimage(h)
        if uid is not None and db_name(uid, uuid) == target:
            json.dump({"uuid": uuid, "user_id": uid}, open(CONFIG, "w"), indent=2)
            return _bundle(uid, uuid, db_path)

    raise RuntimeError(
        "user_id 복구 실패. KakaoTalk 로그인 상태와 Full Disk Access 권한을 확인하라."
    )


def _bundle(user_id: int, uuid: str, db_path: str) -> dict:
    return {
        "user_id": user_id,
        "uuid": uuid,
        "db_path": db_path,
        "key": secure_key(user_id, uuid),
    }


if __name__ == "__main__":
    info = resolve(force="--force" in sys.argv)
    masked = dict(info)
    masked["key"] = info["key"][:12] + "…(생략)"
    print(json.dumps(masked, ensure_ascii=False, indent=2))
