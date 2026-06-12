"""
SQLAlchemy 엔진 — 표준 sqlcipher3 드라이버에 우리가 만든 키를 주입한다.

핵심 결정:
- `mode=ro` (NOT `immutable`): immutable 은 WAL 을 무시해 연 시점 스냅샷에 고정된다.
  카톡이 새 메시지를 WAL 에 쌓으므로, 라이브 반영하려면 mode=ro 로 열어야 한다.
- NullPool: 요청마다 새 연결 → 매 요청이 최신 커밋을 본다(+동시 요청 충돌 없음).
  연결당 PRAGMA key(KDF)는 1회. 개인 데모 수준에선 비용 무시 가능.
"""
from __future__ import annotations

import sqlcipher3
from sqlalchemy import create_engine
from sqlalchemy.orm import sessionmaker
from sqlalchemy.pool import NullPool

from . import extract

_INFO = extract.resolve()
DB_PATH = _INFO["db_path"]
_KEY = _INFO["key"]
USER_ID = _INFO["user_id"]


def raw_ro_connection():
    """복호화된 read-only sqlcipher3 연결. SSE 워처가 직접 폴링에 쓴다."""
    con = sqlcipher3.connect(f"file:{DB_PATH}?mode=ro", uri=True, check_same_thread=False)
    con.execute("PRAGMA cipher_default_compatibility=3")  # 카톡 v26.x
    con.execute(f"PRAGMA key='{_KEY}'")
    con.execute("PRAGMA busy_timeout=3000")
    con.execute("SELECT count(*) FROM sqlite_master")  # 키 검증 (틀리면 예외)
    return con


engine = create_engine(
    "sqlite://",
    creator=raw_ro_connection,
    poolclass=NullPool,
    connect_args={"check_same_thread": False},
)
SessionLocal = sessionmaker(bind=engine, autoflush=False, autocommit=False)


def public_info() -> dict:
    """프론트에 노출해도 되는 메타."""
    return {"user_id": USER_ID, "db_path": DB_PATH}
