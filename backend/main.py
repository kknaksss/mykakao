"""
FastAPI 데모 백엔드 — 복호화된 카톡 DB 를 ORM 으로 읽어 JSON 으로 제공한다.

주의: chatId/logId 는 18자리(>2^53)라 JS Number 정밀도를 넘긴다 → 문자열로 직렬화.
"""
from __future__ import annotations

import os
from datetime import datetime

import json
import time

from fastapi import FastAPI, HTTPException, Query, Request
from fastapi.responses import FileResponse, StreamingResponse
from fastapi.staticfiles import StaticFiles
from pydantic import BaseModel
from sqlalchemy import func, select

from . import db
from . import summarize as sm
from .models import ChatMessage, ChatRoom, User

app = FastAPI(title="mykakao demo")

FRONTEND = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "frontend")


def _ts(v: int | None) -> str | None:
    if not v:
        return None
    try:
        return datetime.fromtimestamp(v).isoformat(sep=" ", timespec="seconds")
    except (OSError, ValueError):
        return None


def _name(chat_name, display, friend, nick) -> str:
    if chat_name and chat_name.strip():
        return chat_name
    return display or friend or nick or "(이름 없음)"


@app.get("/api/stats")
def stats():
    with db.SessionLocal() as s:
        msgs = s.scalar(select(func.count()).select_from(ChatMessage))
        rooms = s.scalar(select(func.count()).select_from(ChatRoom))
    meta = db.public_info()
    return {
        "messages": msgs,
        "rooms": rooms,
        "user_id": meta["user_id"],
        "db_file": os.path.basename(meta["db_path"]),
    }


@app.get("/api/chats")
def chats(limit: int = Query(100, ge=1, le=500)):
    with db.SessionLocal() as s:
        stmt = (
            select(ChatRoom, User)
            .outerjoin(
                User,
                (User.userId == ChatRoom.directChatMemberUserId) & (User.linkId == 0),
            )
            .where(ChatRoom.lastUpdatedAt > 0)
            .order_by(ChatRoom.lastUpdatedAt.desc())
            .limit(limit)
        )
        out = []
        for room, user in s.execute(stmt):
            out.append({
                "chatId": str(room.chatId),
                "name": _name(
                    room.chatName,
                    user.displayName if user else None,
                    user.friendNickName if user else None,
                    user.nickName if user else None,
                ),
                "members": room.activeMembersCount,
                "lastAt": _ts(room.lastUpdatedAt),
                "isGroup": room.type == 1,
            })
    return out


@app.get("/api/messages")
def messages(
    chat_id: str = Query(...),
    limit: int = Query(80, ge=1, le=500),
):
    try:
        cid = int(chat_id)
    except ValueError:
        raise HTTPException(400, "chat_id 가 정수가 아니다")
    with db.SessionLocal() as s:
        stmt = (
            select(ChatMessage, User)
            .outerjoin(
                User,
                (User.userId == ChatMessage.authorId) & (User.linkId == 0),
            )
            .where(ChatMessage.chatId == cid)
            .order_by(ChatMessage.sentAt.desc())
            .limit(limit)
        )
        rows = list(s.execute(stmt))
    me = db.public_info()["user_id"]
    out = []
    for m, user in reversed(rows):  # 오래된 → 최신 순으로 보여줌
        out.append({
            "logId": str(m.logId),
            "sender": (user.displayName or user.friendNickName or user.nickName) if user else None,
            "authorId": str(m.authorId) if m.authorId else None,
            "isMe": m.authorId == me,
            "type": m.type,
            "text": m.message if m.type == 1 else (m.message or f"[type {m.type}]"),
            "at": _ts(m.sentAt),
        })
    return out


@app.get("/api/search")
def search(q: str = Query(..., min_length=1), limit: int = Query(100, ge=1, le=500)):
    with db.SessionLocal() as s:
        stmt = (
            select(ChatMessage, ChatRoom, User)
            .outerjoin(ChatRoom, ChatRoom.chatId == ChatMessage.chatId)
            .outerjoin(
                User,
                (User.userId == ChatMessage.authorId) & (User.linkId == 0),
            )
            .where(ChatMessage.type == 1, ChatMessage.message.like(f"%{q}%"))
            .order_by(ChatMessage.sentAt.desc())
            .limit(limit)
        )
        out = []
        for m, room, user in s.execute(stmt):
            out.append({
                "logId": str(m.logId),
                "chatId": str(m.chatId),
                "room": _name(
                    room.chatName if room else None,
                    None, None, None,
                ) if room else None,
                "sender": (user.displayName or user.friendNickName or user.nickName) if user else None,
                "text": m.message,
                "at": _ts(m.sentAt),
            })
    return out


_STREAM_SQL = """
  SELECT m.logId, m.chatId,
         COALESCE(NULLIF(r.chatName,''), du.displayName, du.friendNickName, du.nickName, '(이름 없음)') AS room,
         m.authorId,
         COALESCE(au.displayName, au.friendNickName, au.nickName) AS sender,
         m.message, m.type, m.sentAt
  FROM NTChatMessage m
  LEFT JOIN NTChatRoom r ON r.chatId = m.chatId
  LEFT JOIN NTUser du ON du.userId = r.directChatMemberUserId AND du.linkId = 0
  LEFT JOIN NTUser au ON au.userId = m.authorId AND au.linkId = 0
  WHERE m.logId > ?
  ORDER BY m.logId ASC LIMIT 200
"""


@app.get("/api/stream")
def stream(request: Request, after: str | None = None):
    """SSE — DB 를 1초마다 폴링해 새 logId 가 적재되면 push 한다(사실상 실시간 후킹)."""
    me = db.USER_ID

    def gen():
        con = db.raw_ro_connection()
        cur = con.cursor()
        if after and after.isdigit():
            last = int(after)
        else:
            cur.execute("SELECT COALESCE(MAX(logId), 0) FROM NTChatMessage")
            last = cur.fetchone()[0]
        yield ": connected\n\n"
        idle = 0
        try:
            while True:
                cur.execute(_STREAM_SQL, (last,))
                rows = cur.fetchall()
                for logId, chatId, room, authorId, sender, message, mtype, sentAt in rows:
                    last = logId
                    payload = {
                        "logId": str(logId),
                        "chatId": str(chatId),
                        "room": room,
                        "authorId": str(authorId) if authorId else None,
                        "isMe": authorId == me,
                        "sender": sender,
                        "text": message if mtype == 1 else (message or f"[type {mtype}]"),
                        "type": mtype,
                        "at": _ts(sentAt),
                    }
                    yield f"data: {json.dumps(payload, ensure_ascii=False)}\n\n"
                idle = 0 if rows else idle + 1
                if idle and idle % 10 == 0:
                    yield ": ping\n\n"  # keep-alive
                time.sleep(1)
        finally:
            con.close()

    return StreamingResponse(
        gen(),
        media_type="text/event-stream",
        headers={"Cache-Control": "no-cache", "X-Accel-Buffering": "no"},
    )


class SummarizeRequest(BaseModel):
    chat_id: str
    date: str
    prompt: str


@app.post("/api/summarize")
async def summarize(req: SummarizeRequest):
    """그 방·그날 메시지를 OQ-2 템플릿으로 조립(+cap) 후 codex submit → {task_id}.

    SPEC-002 BE Contract / Case Matrix:
    - 빈 프롬프트 / 잘못된 chat_id·date → 400
    - 그날 0건 → 거부 아님, empty 플래그로 빈 결과 반환(요약 미실행)
    - cap 초과 → 오래된 것부터 truncate + 고지 메타(에러 아님)
    - redis/워커 미가동 → 503
    """
    prompt = (req.prompt or "").strip()
    if not prompt:
        raise HTTPException(400, "프롬프트가 비어 있다")
    try:
        cid = int(req.chat_id)
    except (TypeError, ValueError):
        raise HTTPException(400, "chat_id 가 정수가 아니다")
    try:
        start, end = sm.day_range(req.date)
    except ValueError:
        raise HTTPException(400, "date 형식이 잘못됐다 (YYYY-MM-DD)")

    room_name = sm.resolve_room_name(cid)
    if room_name is None:
        raise HTTPException(400, "존재하지 않는 chat_id")

    messages = sm.fetch_day_messages(cid, start, end)
    if not messages:
        return {"task_id": None, "empty": True, "total": 0, "shown": 0, "notice": None}

    kept, total, truncated = sm.apply_cap(messages, sm.MAX_MESSAGES)
    assembled = sm.assemble_prompt(prompt, room_name, req.date, kept)
    notice = sm.truncation_notice(total, len(kept)) if truncated else None

    try:
        client = await sm.get_client()
        task_id = await client.submit(
            assembled, provider=sm.PROVIDER, model=sm.MODEL, queue=sm.QUEUE
        )
    except Exception as e:  # redis 미가동/연결 실패 등
        raise HTTPException(503, f"요약 서버(워커) 미가동: {e}")

    return {
        "task_id": task_id,
        "empty": False,
        "total": total,
        "shown": len(kept),
        "notice": notice,
    }


def _sse(payload: dict) -> str:
    return f"data: {json.dumps(payload, ensure_ascii=False)}\n\n"


@app.get("/api/summarize/stream")
async def summarize_stream(task_id: str = Query(...)):
    """open_kknaks text 이벤트를 SSE 릴레이 + 종료 후 TaskStatus 기반 terminal 합성.

    StreamEvent 엔 종료/에러 전용 이벤트가 없다(WORK-002 grounding). 토큰은 type=="text"
    의 .text. 스트림 종료 후 task 상태(done/failed/cancelled)+error 로 done/error 를 합성한다.
    SSE 이벤트 키: {type:"text",text} / {type:"done",status} / {type:"error",error,status}.
    """

    async def gen():
        try:
            client = await sm.get_client()
        except Exception as e:
            yield _sse({"type": "error", "error": f"요약 서버(워커) 미가동: {e}"})
            return
        yield ": connected\n\n"
        try:
            async for ev in client.stream(
                task_id, event_types={"text"}, timeout=sm.STREAM_TIMEOUT_S
            ):
                if ev.text:
                    yield _sse({"type": "text", "text": ev.text})
            # 스트림 종료 → 이미 terminal 이므로 즉시 반환된다.
            task = await client.result(task_id, timeout=1)
            status = task.status if task else None
            if status == "done":
                yield _sse({"type": "done", "status": status})
            else:
                err = (task.error if task and task.error else None) or "요약 생성 실패"
                yield _sse({"type": "error", "error": err, "status": status})
        except Exception as e:
            yield _sse({"type": "error", "error": str(e)})

    return StreamingResponse(
        gen(),
        media_type="text/event-stream",
        headers={"Cache-Control": "no-cache", "X-Accel-Buffering": "no"},
    )


@app.get("/")
def index():
    return FileResponse(os.path.join(FRONTEND, "index.html"))


app.mount("/", StaticFiles(directory=FRONTEND), name="static")
