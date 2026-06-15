"""AI 요약 — 그 방·그날 메시지를 OQ-2 템플릿으로 조립해 codex(open_kknaks)로 요약한다.

SPEC-002 / WORK-002 (W-1) 계약 구현.
- 메시지 소스: 기존 read-only sqlcipher 연결(`db`) 재사용.
- LLM 호출: `open_kknaks` (provider=codex, model=gpt-5.5). redis broker 경유 (ADR-04 계열).
- 조립/cap/직렬화는 순수 함수로 분리해 DB·redis 없이 단위 테스트한다.

고정한 구현 파라미터 (spec→work 이월):
- cap 상한: 메시지 수 기준 (`SUMMARY_MAX_MESSAGES`, 기본 500). 초과 시 오래된 것부터 truncate.
- 비텍스트(type≠1) placeholder: `[기타]`.
- namespace=`mykakao`, queue=`default` — T-006 worker .env 와 반드시 일치해야 픽업된다.
"""
from __future__ import annotations

import asyncio
import os
from datetime import datetime, timedelta

# open_kknaks 2.0.2 는 AgentClient, 2.0.0 은 ClaudeClient 로 노출 → 버전 hedge.
try:
    from open_kknaks import AgentClient as _Client  # >=2.0.2
except ImportError:  # pragma: no cover
    from open_kknaks import ClaudeClient as _Client  # 2.0.0 호환

from open_kknaks import RedisBroker
from sqlalchemy import select

from .models import ChatMessage, ChatRoom, User

# ─── 고정 파라미터 ───
MODEL = os.environ.get("SUMMARY_MODEL", "gpt-5.5")
PROVIDER = "codex"
REDIS_URL = os.environ.get("REDIS_URL", "redis://localhost:6379")
NAMESPACE = os.environ.get("OPEN_KKNAKS_NAMESPACE", "mykakao")
QUEUE = os.environ.get("OPEN_KKNAKS_QUEUE", "default")
MAX_MESSAGES = int(os.environ.get("SUMMARY_MAX_MESSAGES", "500"))
STREAM_TIMEOUT_S = int(os.environ.get("SUMMARY_STREAM_TIMEOUT", "300"))
NONTEXT_PLACEHOLDER = "[기타]"


# ─── 순수 함수 (DB·redis 비의존, 단위 테스트 대상) ───

def day_range(date_str: str) -> tuple[int, int]:
    """`YYYY-MM-DD` → 그날 로컬 [00:00, 24:00) 의 unix epoch(초) 경계.

    DST 등으로 하루가 86400초가 아닐 수 있으므로 '다음날 자정'으로 end 를 계산한다.
    형식이 틀리면 ValueError.
    """
    d = datetime.strptime(date_str, "%Y-%m-%d").date()
    nxt = d + timedelta(days=1)
    start = int(datetime(d.year, d.month, d.day).timestamp())
    end = int(datetime(nxt.year, nxt.month, nxt.day).timestamp())
    return start, end


def _name(chat_name, display, friend, nick) -> str:
    """SPEC-001 표시명 규약 재사용 (main.py `_name` 미러)."""
    if chat_name and chat_name.strip():
        return chat_name
    return display or friend or nick or "(이름 없음)"


def _line(msg: dict) -> str:
    """한 메시지 → `{HH:MM} {발신자}: {본문}`. 비텍스트는 placeholder."""
    hhmm = datetime.fromtimestamp(msg["sentAt"]).strftime("%H:%M")
    sender = msg.get("sender") or "(이름 없음)"
    if msg.get("type") == 1:
        body = msg.get("message") or ""
    else:
        body = NONTEXT_PLACEHOLDER
    return f"{hhmm} {sender}: {body}"


def assemble_prompt(user_prompt: str, room_name: str, date_str: str, messages: list[dict]) -> str:
    """OQ-2 조립 템플릿. 한 줄 = 한 메시지 (시각 오름차순 전제)."""
    header = f"[{room_name} / {date_str}]"
    lines = [user_prompt.strip(), "", "---", header]
    lines.extend(_line(m) for m in messages)
    return "\n".join(lines)


def apply_cap(messages: list[dict], max_messages: int = MAX_MESSAGES) -> tuple[list[dict], int, bool]:
    """cap 적용. 초과 시 오래된 것부터 잘라 최근 max_messages 개만 남긴다.

    returns (kept, total, truncated). messages 는 시각 오름차순 전제.
    """
    total = len(messages)
    if total <= max_messages:
        return messages, total, False
    return messages[-max_messages:], total, True


def truncation_notice(total: int, shown: int) -> str:
    """cap 생략 고지 문구 (SPEC-002 Data Contract)."""
    return f"…(일부 생략됨, 총 {total}개 중 {shown}개 표시)"


# ─── DB 조회 (db 모듈은 import 시 키 복구를 트리거하므로 함수 내부에서 lazy import) ───

def resolve_room_name(chat_id: int) -> str | None:
    """추출된 방 이름. 존재하지 않으면 None (chat_id 검증 겸용)."""
    from . import db

    with db.SessionLocal() as s:
        stmt = (
            select(ChatRoom, User)
            .outerjoin(
                User,
                (User.userId == ChatRoom.directChatMemberUserId) & (User.linkId == 0),
            )
            .where(ChatRoom.chatId == chat_id)
        )
        row = s.execute(stmt).first()
    if row is None:
        return None
    room, user = row
    return _name(
        room.chatName,
        user.displayName if user else None,
        user.friendNickName if user else None,
        user.nickName if user else None,
    )


def fetch_day_messages(chat_id: int, start: int, end: int) -> list[dict]:
    """그 방·그날 [start, end) 메시지를 시각 오름차순으로 조회."""
    from . import db

    with db.SessionLocal() as s:
        stmt = (
            select(ChatMessage, User)
            .outerjoin(
                User,
                (User.userId == ChatMessage.authorId) & (User.linkId == 0),
            )
            .where(
                ChatMessage.chatId == chat_id,
                ChatMessage.sentAt >= start,
                ChatMessage.sentAt < end,
            )
            .order_by(ChatMessage.sentAt.asc())
        )
        rows = list(s.execute(stmt))
    out = []
    for m, user in rows:
        out.append({
            "sentAt": m.sentAt,
            "type": m.type,
            "message": m.message,
            "sender": (user.displayName or user.friendNickName or user.nickName) if user else None,
        })
    return out


# ─── open_kknaks client (lazy singleton — backend 는 redis 없이도 기동되어야 한다) ───

_client = None
_broker = None
_lock = asyncio.Lock()


async def get_client():
    """연결된 client 싱글톤. 최초 호출에서만 broker 연결. redis 미가동 시 예외 전파."""
    global _client, _broker
    if _client is not None:
        return _client
    async with _lock:
        if _client is None:
            broker = RedisBroker(url=REDIS_URL, namespace=NAMESPACE)
            await broker.connect()
            _broker = broker
            _client = _Client(broker=broker)
    return _client
