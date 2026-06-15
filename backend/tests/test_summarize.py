"""summarize 순수 함수 단위 테스트 (DB·redis 비의존).

실행: mykakao 루트에서 `python -m pytest backend/tests -q`
(pytest 는 런타임 의존 아님 — 검증용으로만 설치)
"""
from datetime import datetime

from backend import summarize as sm


def _epoch(y, mo, d, h, mi):
    return int(datetime(y, mo, d, h, mi).timestamp())


def test_day_range_boundaries():
    start, end = sm.day_range("2026-06-15")
    assert start == int(datetime(2026, 6, 15, 0, 0).timestamp())
    assert end == int(datetime(2026, 6, 16, 0, 0).timestamp())
    assert end - start in (86400, 82800, 90000)  # 정상/DST 경계 허용


def test_day_range_bad_format():
    import pytest

    with pytest.raises(ValueError):
        sm.day_range("2026/06/15")


def test_assemble_prompt_format():
    msgs = [
        {"sentAt": _epoch(2026, 6, 15, 9, 5), "type": 1, "message": "안녕", "sender": "철수"},
        {"sentAt": _epoch(2026, 6, 15, 14, 30), "type": 2, "message": "(binary)", "sender": "영희"},
    ]
    out = sm.assemble_prompt("오늘 요약해줘", "우리방", "2026-06-15", msgs)
    lines = out.split("\n")
    assert lines[0] == "오늘 요약해줘"
    assert lines[1] == ""
    assert lines[2] == "---"
    assert lines[3] == "[우리방 / 2026-06-15]"
    assert lines[4] == "09:05 철수: 안녕"
    # 비텍스트(type≠1)는 placeholder 로 치환 (원문 누출 없음)
    assert lines[5] == f"14:30 영희: {sm.NONTEXT_PLACEHOLDER}"
    assert "(binary)" not in out


def test_assemble_prompt_strips_user_prompt():
    out = sm.assemble_prompt("  공백포함  ", "방", "2026-06-15", [])
    assert out.split("\n")[0] == "공백포함"


def test_apply_cap_under_limit():
    msgs = [{"sentAt": i, "type": 1, "message": str(i), "sender": "a"} for i in range(10)]
    kept, total, truncated = sm.apply_cap(msgs, 500)
    assert kept == msgs and total == 10 and truncated is False


def test_apply_cap_truncates_oldest():
    msgs = [{"sentAt": i, "type": 1, "message": str(i), "sender": "a"} for i in range(10)]
    kept, total, truncated = sm.apply_cap(msgs, 3)
    assert total == 10 and truncated is True
    assert len(kept) == 3
    # 오래된 것부터 잘리고 최근 3개(7,8,9)만 남는다
    assert [m["sentAt"] for m in kept] == [7, 8, 9]


def test_truncation_notice():
    assert sm.truncation_notice(10, 3) == "…(일부 생략됨, 총 10개 중 3개 표시)"


def test_line_missing_sender_and_empty_text():
    line = sm._line({"sentAt": _epoch(2026, 6, 15, 0, 0), "type": 1, "message": None, "sender": None})
    assert line == "00:00 (이름 없음): "
