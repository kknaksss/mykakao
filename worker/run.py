"""mykakao codex worker (WORK-002 W-3) — open_kknaks examples/worker/run.py 미러.

env (.env, docker-compose env_file):
  REDIS_URL   = redis://redis:6379   (compose 내부 네트워크)
  NAMESPACE   = mykakao              (★ host backend summarize.NAMESPACE 와 일치 필수)
  QUEUES      = default              (★ host submit queue 와 일치 필수)
  CONCURRENCY = 2
  WORK_DIR    = /project             (codex cwd)
codex 인증: CODEX_HOME=/codex-home (호스트 ~/.codex 복사본 바인드 마운트).
codex CLI: PATH=/codex-tools/node_modules/.bin (setup.sh 가 설치).
"""

import asyncio
import os

from open_kknaks.broker.redis import RedisBroker
from open_kknaks.config import ClaudeConfig
from open_kknaks.middleware.cost import CostMiddleware
from open_kknaks.middleware.logging import LoggingMiddleware
from open_kknaks.middleware.retries import RetriesMiddleware
from open_kknaks.middleware.timeout import TimeoutMiddleware
from open_kknaks.worker.worker import ClaudeWorker


async def main() -> None:
    # codex_adapter 는 ClaudeConfig.work_dir 를 무시하고(`del config`) 워커 프로세스의 cwd 를
    # codex exec 의 cwd 로 상속시킨다. 따라서 codex 가 trusted git repo(/project)에서 돌도록
    # 워커 cwd 를 WORK_DIR 로 옮긴다. (/project trust 는 .codex-home/config.toml 에 등록.)
    work_dir = os.environ.get("WORK_DIR", "/project")
    if os.path.isdir(work_dir):
        os.chdir(work_dir)

    broker = RedisBroker(
        url=os.environ.get("REDIS_URL", "redis://redis:6379"),
        namespace=os.environ.get("NAMESPACE", "mykakao"),
    )
    await broker.connect()

    config = ClaudeConfig(
        work_dir=os.environ.get("WORK_DIR", "/project"),
    )

    worker = ClaudeWorker(
        broker=broker,
        config=config,
        queues=os.environ.get("QUEUES", "default").split(","),
        concurrency=int(os.environ.get("CONCURRENCY", "2")),
        middleware=[
            LoggingMiddleware(),
            RetriesMiddleware(max_retries=2),
            TimeoutMiddleware(),
            CostMiddleware(
                worker_budget_usd=5.0,
                global_budget_usd=20.0,
            ),
        ],
    )

    print(f"Worker starting: ns={broker._namespace}, queues={worker.queues}, concurrency={worker.concurrency}")

    try:
        await worker.run()
    finally:
        await broker.close()


if __name__ == "__main__":
    asyncio.run(main())
