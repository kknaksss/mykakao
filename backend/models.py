"""
ORM 모델 — 카톡이 만든 read-only 스키마를 매핑한다 (우리가 만든 스키마 아님).
필요한 컬럼만 선언한다.
"""
from __future__ import annotations

from sqlalchemy import BigInteger, Integer, Text
from sqlalchemy.orm import DeclarativeBase, Mapped, mapped_column


class Base(DeclarativeBase):
    pass


class ChatRoom(Base):
    __tablename__ = "NTChatRoom"

    chatId: Mapped[int] = mapped_column(BigInteger, primary_key=True)
    type: Mapped[int] = mapped_column(Integer)
    chatName: Mapped[str | None] = mapped_column(Text)
    activeMembersCount: Mapped[int | None] = mapped_column(Integer)
    lastUpdatedAt: Mapped[int | None] = mapped_column(BigInteger)
    lastLogId: Mapped[int | None] = mapped_column(BigInteger)
    directChatMemberUserId: Mapped[int | None] = mapped_column(BigInteger)


class ChatMessage(Base):
    __tablename__ = "NTChatMessage"

    # 복합키지만 ORM 식별엔 logId 면 충분
    logId: Mapped[int] = mapped_column(BigInteger, primary_key=True)
    chatId: Mapped[int] = mapped_column(BigInteger)
    authorId: Mapped[int | None] = mapped_column(BigInteger)
    type: Mapped[int | None] = mapped_column(Integer)
    message: Mapped[str | None] = mapped_column(Text)
    sentAt: Mapped[int | None] = mapped_column(BigInteger)


class User(Base):
    __tablename__ = "NTUser"

    userId: Mapped[int] = mapped_column(BigInteger, primary_key=True)
    linkId: Mapped[int] = mapped_column(BigInteger, primary_key=True)
    displayName: Mapped[str | None] = mapped_column(Text)
    friendNickName: Mapped[str | None] = mapped_column(Text)
    nickName: Mapped[str | None] = mapped_column(Text)
    profileImageUrl: Mapped[str | None] = mapped_column(Text)
