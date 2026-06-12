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
from sqlalchemy import func, select

from . import db
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


@app.get("/")
def index():
    return FileResponse(os.path.join(FRONTEND, "index.html"))


app.mount("/", StaticFiles(directory=FRONTEND), name="static")
