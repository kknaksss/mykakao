#!/bin/bash
# mykakao AI 요약 인프라 셋업 (WORK-002 W-3) — open_kknaks examples/setup.sh 미러 (codex 전용).
#
# 하는 일:
#   1) Linux용 codex CLI 를 .codex-tools/ 에 설치 (docker node 이미지로 native linux 바이너리 fetch)
#   2) 호스트 ~/.codex 인증/config 를 .codex-home/ 으로 복사 (컨테이너 바인드 마운트용)
#   3) .env 생성 (NAMESPACE=mykakao / QUEUES=default — host backend 와 일치)
#   4) docker compose up -d --build (redis + codex worker)
#
# 사전조건: 호스트 codex 로그인 완료(~/.codex 존재), docker 설치.
set -e

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
CODEX_TOOLS_DIR="${SCRIPT_DIR}/.codex-tools"
CODEX_HOME_DIR="${SCRIPT_DIR}/.codex-home"
NODE_VERSION="22.16.0"

# ── 1. Linux용 codex CLI 설치 (.codex-tools/) ──
echo "=== Codex CLI 설치 (Docker/Linux용) ==="
if ! command -v docker &> /dev/null; then
    echo "ERROR: docker 가 필요합니다."
    exit 1
fi
mkdir -p "${CODEX_TOOLS_DIR}"
docker run --rm \
    -v "${CODEX_TOOLS_DIR}:/codex-tools" \
    "node:${NODE_VERSION}-bookworm-slim" \
    npm install --prefix /codex-tools @openai/codex
echo "Codex CLI 설치 완료: .codex-tools/"

# ── 2. 호스트 codex 인증/config 복사 (.codex-home/) ──
echo ""
echo "=== Codex 인증/config 복사 ==="
if [ -d "${HOME}/.codex" ]; then
    mkdir -p "${CODEX_HOME_DIR}"
    cp -R "${HOME}/.codex/." "${CODEX_HOME_DIR}/"
    echo "복사 완료: .codex-home/"
else
    echo "ERROR: ${HOME}/.codex 가 없습니다. 먼저 호스트에서 codex 로그인하세요."
    exit 1
fi

# 컨테이너 cwd(/project)를 codex trusted 로 등록.
# worker(run.py)가 /project 로 chdir 후 codex exec → trust 없으면 codex 가 실행 거부한다.
if ! grep -q '\[projects."/project"\]' "${CODEX_HOME_DIR}/config.toml" 2>/dev/null; then
    printf '\n[projects."/project"]\ntrust_level = "trusted"\n' >> "${CODEX_HOME_DIR}/config.toml"
    echo "trust 등록: /project → trusted (.codex-home/config.toml)"
fi

# ── 3. .env 생성 ──
echo ""
echo "=== .env 생성 ==="
cat > "${SCRIPT_DIR}/.env" << 'EOF'
REDIS_URL=redis://redis:6379
NAMESPACE=mykakao
QUEUES=default
CONCURRENCY=2
WORK_DIR=/project
EOF
echo ".env 생성 완료"

# ── 4. docker compose 기동 ──
echo ""
echo "=== docker compose up (redis + codex worker) ==="
cd "${SCRIPT_DIR}"
docker compose up -d --build

echo ""
echo "=== 완료 ==="
echo "  redis:  localhost:6379"
echo "  worker: docker (codex, ns=mykakao, queue=default)"
echo ""
echo "이제 호스트 backend 를 기동하세요:  ./run.sh"
echo "  (backend → redis://localhost:6379 로 submit → worker 픽업)"
