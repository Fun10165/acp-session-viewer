"""连接池的行为测试：复用、轮换、空闲回收、进程退出后的恢复。

测试对象是连接池本身，桩 agent 只是被观察的环境：它把每次启动的进程号
追加写入 ``STUB_AGENT_START_LOG``，因此「启动了几次」可以被直接核对。
"""

from __future__ import annotations

import asyncio
import os
import signal
import sys
from pathlib import Path
from typing import Any

import pytest

from acp_session_viewer.agent_pool import AgentPool
from acp_session_viewer.config import AgentSpec
from acp_session_viewer.viewer import LoadResult, SessionSummary, ViewerError

STUB = Path(__file__).parent / "fixtures" / "stub_agent.py"
CWD = Path(__file__).resolve().parent.as_posix()


def _spec() -> AgentSpec:
    return AgentSpec("stub", (sys.executable, str(STUB)), "测试桩")


def _starts(path: Path) -> int:
    if not path.exists():
        return 0
    return len([line for line in path.read_text(encoding="utf-8").splitlines() if line.strip()])


async def _list(pool: AgentPool, spec: AgentSpec) -> list[SessionSummary]:
    return await pool.use(spec, lambda connection: connection.list_sessions(timeout=10))


async def _load(pool: AgentPool, spec: AgentSpec, session_id: str) -> LoadResult:
    return await pool.use(
        spec,
        lambda connection: connection.load_transcript(session_id, CWD, timeout=10),
    )


def _run(scenario_factory) -> Any:
    return asyncio.run(scenario_factory())


def test_同一_agent_的连接被复用(monkeypatch: pytest.MonkeyPatch, tmp_path: Path) -> None:
    start_log = tmp_path / "starts.log"
    monkeypatch.setenv("STUB_AGENT_START_LOG", str(start_log))
    spec = _spec()

    async def scenario() -> dict[str, Any]:
        pool = AgentPool(timeout=10, idle_timeout=60, max_uses_per_connection=8)
        await pool.start()
        try:
            assert len(await _list(pool, spec)) == 2
            assert len(await _list(pool, spec)) == 2
            return pool.status()[0]
        finally:
            await pool.close()

    status = _run(scenario)
    assert status["agent"] == "stub"
    assert status["connected"] is True
    assert status["uses"] == 2
    assert _starts(start_log) == 1


def test_达到使用上限后轮换连接(monkeypatch: pytest.MonkeyPatch, tmp_path: Path) -> None:
    start_log = tmp_path / "starts.log"
    monkeypatch.setenv("STUB_AGENT_START_LOG", str(start_log))
    spec = _spec()

    async def scenario() -> None:
        pool = AgentPool(timeout=10, idle_timeout=60, max_uses_per_connection=2)
        await pool.start()
        try:
            for _ in range(3):
                await _list(pool, spec)
        finally:
            await pool.close()

    _run(scenario)
    assert _starts(start_log) == 2


def test_空闲超时后回收连接(monkeypatch: pytest.MonkeyPatch, tmp_path: Path) -> None:
    start_log = tmp_path / "starts.log"
    monkeypatch.setenv("STUB_AGENT_START_LOG", str(start_log))
    spec = _spec()

    async def scenario() -> list[dict[str, Any]]:
        pool = AgentPool(timeout=10, idle_timeout=0.2, max_uses_per_connection=8, sweep_interval=0.05)
        await pool.start()
        try:
            await _list(pool, spec)
            await asyncio.sleep(0.5)
            await _list(pool, spec)
            return pool.status()
        finally:
            await pool.close()

    status = _run(scenario)
    assert _starts(start_log) == 2
    assert status[0]["connected"] is True


def test_进程退出后自动换新连接(monkeypatch: pytest.MonkeyPatch, tmp_path: Path) -> None:
    start_log = tmp_path / "starts.log"
    monkeypatch.setenv("STUB_AGENT_START_LOG", str(start_log))
    spec = _spec()

    async def scenario() -> list[SessionSummary]:
        pool = AgentPool(timeout=10, idle_timeout=60, max_uses_per_connection=8)
        await pool.start()
        try:
            await _list(pool, spec)
            pid = pool.status()[0]["pid"]
            assert isinstance(pid, int)
            os.kill(pid, signal.SIGKILL)
            await asyncio.sleep(0.3)
            return await _list(pool, spec)
        finally:
            await pool.close()

    sessions = _run(scenario)
    assert len(sessions) == 2
    assert _starts(start_log) == 2


def test_进程中途退出时重试一次后上报(monkeypatch: pytest.MonkeyPatch, tmp_path: Path) -> None:
    """桩 agent 加载 stub-2 时会直接退出：连接失效后换新连接重试一次，仍失败就上报。"""
    start_log = tmp_path / "starts.log"
    monkeypatch.setenv("STUB_AGENT_START_LOG", str(start_log))
    monkeypatch.setenv("STUB_AGENT_DIE_ON_SESSION", "stub-2")
    spec = _spec()

    async def scenario() -> None:
        pool = AgentPool(timeout=5, idle_timeout=60, max_uses_per_connection=8)
        await pool.start()
        try:
            with pytest.raises(ViewerError):
                await _load(pool, spec, "stub-2")
            assert pool.status()[0]["connected"] is False
        finally:
            await pool.close()

    _run(scenario)
    assert _starts(start_log) == 2


def test_加载会话能读出桩事件(monkeypatch: pytest.MonkeyPatch, tmp_path: Path) -> None:
    monkeypatch.setenv("STUB_AGENT_START_LOG", str(tmp_path / "starts.log"))
    spec = _spec()

    async def scenario() -> LoadResult:
        pool = AgentPool(timeout=10, idle_timeout=60, max_uses_per_connection=8)
        await pool.start()
        try:
            return await _load(pool, spec, "stub-1")
        finally:
            await pool.close()

    result = _run(scenario)
    assert result.session_id == "stub-1"
    texts = [update["content"]["text"] for update in result.updates]
    assert texts == ["stub-1 的第一条", "stub-1 的第二条"]
