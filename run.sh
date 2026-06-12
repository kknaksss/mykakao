#!/bin/bash
# mykakao 데모 실행 — venv 준비 → 의존성 설치 → 키 복구 → 서버 기동
set -e
cd "$(dirname "$0")"

# sqlcipher3 는 소스 빌드라 brew sqlcipher 헤더/라이브러리 경로가 필요하다.
PFX="$(brew --prefix sqlcipher 2>/dev/null)"
if [ -z "$PFX" ]; then echo "sqlcipher 가 없다. 'brew install sqlcipher' 후 다시 실행."; exit 1; fi
export CFLAGS="-I$PFX/include" LDFLAGS="-L$PFX/lib"
export C_INCLUDE_PATH="$PFX/include" LIBRARY_PATH="$PFX/lib"

if [ ! -d .venv ]; then python3 -m venv .venv; fi
source .venv/bin/activate
pip install -q --upgrade pip
pip install -q -r requirements.txt

echo "→ 키 복구/검증 중…"
python -m backend.extract

PORT="${PORT:-8765}"
echo "→ http://127.0.0.1:$PORT  (Ctrl+C 로 종료)"
exec python -m uvicorn backend.main:app --host 127.0.0.1 --port "$PORT"
