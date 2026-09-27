"""会话选择器匹配规则的行为测试。"""

from __future__ import annotations

import pytest

from acp_session_viewer.selection import SelectionError, match_sessions
from acp_session_viewer.viewer import SessionSummary

SESSIONS = [
    SessionSummary(
        agent="omp",
        session_id="01a0e148-3751-719a",
        cwd="/Users/x/tmp",
        title="Implement ACP session mutual viewing",
        updated_at="2026-09-27T05:38:05Z",
    ),
    SessionSummary(
        agent="omp",
        session_id="01a0e148-9999-0000",
        cwd="/Users/x/tmp",
        title="其它任务",
        updated_at="2026-09-26T05:00:00Z",
    ),
    SessionSummary(
        agent="opencode",
        session_id="ses_28a0188b9ffe",
        cwd="/Users/x",
        title="Clash 代理规则",
        updated_at="2026-04-21T08:30:10Z",
    ),
]


def test_完整_id_精确命中() -> None:
    matches = match_sessions(SESSIONS, "ses_28a0188b9ffe")
    assert [item.session_id for item in matches] == ["ses_28a0188b9ffe"]


def test_前缀命中多条时按时间倒序() -> None:
    matches = match_sessions(SESSIONS, "01a0e148")
    assert [item.session_id for item in matches] == ["01a0e148-3751-719a", "01a0e148-9999-0000"]


def test_标题子串匹配且大小写不敏感() -> None:
    assert [item.session_id for item in match_sessions(SESSIONS, "acp session")] == ["01a0e148-3751-719a"]
    assert [item.session_id for item in match_sessions(SESSIONS, "CLASH")] == ["ses_28a0188b9ffe"]


def test_按_agent_名匹配() -> None:
    assert [item.session_id for item in match_sessions(SESSIONS, "opencode")] == ["ses_28a0188b9ffe"]


def test_按工作目录匹配() -> None:
    matches = match_sessions(SESSIONS, "/Users/x/tmp")
    assert [item.session_id for item in matches] == ["01a0e148-3751-719a", "01a0e148-9999-0000"]


def test_last_取最新一条() -> None:
    assert [item.session_id for item in match_sessions(SESSIONS, "last")] == ["01a0e148-3751-719a"]


def test_没有命中时返回空列表() -> None:
    assert match_sessions(SESSIONS, "不存在的东西") == []


def test_空选择器显式报错() -> None:
    with pytest.raises(SelectionError, match="不能为空"):
        match_sessions(SESSIONS, "")


def test_没有会话时_last_返回空() -> None:
    assert match_sessions([], "last") == []
