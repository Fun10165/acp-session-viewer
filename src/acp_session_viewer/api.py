"""服务层：把只读 ACP 能力包装成可供 HTTP 层使用的报告结构。

这一层只依赖 viewer 的只读接口，不含任何 HTTP 细节，便于单独测试。
所有跨 agent 的操作并行执行，单个 agent 失败只记录到报告里，不影响其他 agent。
"""

from __future__ import annotations

import asyncio
from dataclasses import dataclass
from typing import Sequence

from .config import AgentSpec
from .selection import match_sessions
from .viewer import SessionSummary, ViewerError, list_sessions, load_transcript, probe, select_agents


class ApiError(RuntimeError):
    """服务层错误，携带对应的 HTTP 状态码。"""

    def __init__(self, status: int, message: str) -> None:
        super().__init__(message)
        self.status = status
        self.message = message


@dataclass(frozen=True)
class AgentReport:
    """一个 agent 的能力快照。"""

    name: str
    implementation: str
    description: str
    protocolVersion: int | None
    canList: bool
    canLoad: bool
    sessionCapabilities: list[str]
    error: str | None


@dataclass(frozen=True)
class SessionsReport:
    """会话列表查询结果。"""

    sessions: list[SessionSummary]
    total: int
    errors: list[tuple[str, str]]


@dataclass(frozen=True)
class TranscriptReport:
    """一次会话重放的结果。"""

    agent: str
    session_id: str
    cwd: str
    title: str | None
    matched: int
    note: str | None
    load_response: dict
    updates: list[dict]


async def agents_report(agents: Sequence[AgentSpec], *, timeout: float) -> list[AgentReport]:
    """并行探测每个 agent 的 ACP 能力。"""

    async def one(spec: AgentSpec) -> AgentReport:
        try:
            result = await probe(spec, timeout=timeout)
        except ViewerError as exc:
            return AgentReport(spec.name, "", spec.description, None, False, False, [], str(exc))
        info = result.agent_info
        return AgentReport(
            name=spec.name,
            implementation=str(info.get("title") or info.get("name") or ""),
            description=spec.description,
            protocolVersion=result.protocol_version,
            canList=result.can_list,
            canLoad=result.can_load,
            sessionCapabilities=sorted((result.capabilities.get("sessionCapabilities") or {}).keys()),
            error=None,
        )

    return list(await asyncio.gather(*(one(spec) for spec in agents)))


async def sessions_report(
    agents: Sequence[AgentSpec],
    *,
    query: str | None,
    limit: int,
    timeout: float,
) -> SessionsReport:
    """并行枚举全部会话，按关键词过滤后按更新时间倒序返回。"""

    async def one(spec: AgentSpec) -> tuple[list[SessionSummary], tuple[str, str] | None]:
        try:
            sessions, result = await list_sessions(spec, timeout=timeout)
        except ViewerError as exc:
            return [], (spec.name, str(exc))
        if not result.can_list:
            return [], (spec.name, "不支持会话枚举（缺少 sessionCapabilities.list）")
        return sessions, None

    gathered = await asyncio.gather(*(one(spec) for spec in agents))
    sessions: list[SessionSummary] = []
    errors: list[tuple[str, str]] = []
    for items, error in gathered:
        sessions.extend(items)
        if error is not None:
            errors.append(error)

    if query:
        needle = query.lower()
        sessions = [
            item
            for item in sessions
            if needle in (item.title or "").lower()
            or needle in item.session_id.lower()
            or needle in item.cwd.lower()
            or needle in item.agent.lower()
        ]
    sessions.sort(key=lambda item: item.updated_at or "", reverse=True)
    return SessionsReport(sessions=sessions[:limit], total=len(sessions), errors=errors)


async def transcript_report(
    agents: Sequence[AgentSpec],
    *,
    selector: str,
    agent_names: Sequence[str] | None,
    cwd: str | None,
    timeout: float,
) -> TranscriptReport:
    """按选择器定位一条会话并重放它。"""
    try:
        selected = select_agents(agents, agent_names)
    except ViewerError as exc:
        raise ApiError(400, str(exc)) from exc

    if cwd is not None:
        if len(selected) != 1:
            raise ApiError(400, "使用 cwd 直接加载时必须恰好指定一个 agent")
        if not cwd.startswith("/"):
            raise ApiError(400, f"cwd 必须是绝对路径: {cwd!r}")
        summary = SessionSummary(agent=selected[0].name, session_id=selector, cwd=cwd, title=None, updated_at=None)
        return await _load(selected[0], summary, timeout=timeout, matched=1, note="按 cwd 直接加载，未做枚举")

    async def one(spec: AgentSpec) -> tuple[list[SessionSummary], str | None]:
        try:
            sessions, result = await list_sessions(spec, timeout=timeout)
        except ViewerError as exc:
            return [], str(exc)
        if not result.can_list:
            return [], "不支持会话枚举（缺少 sessionCapabilities.list）"
        return sessions, None

    gathered = await asyncio.gather(*(one(spec) for spec in selected))
    matches: list[tuple[AgentSpec, SessionSummary]] = []
    problems: list[str] = []
    for spec, (sessions, error) in zip(selected, gathered):
        if error is not None:
            problems.append(f"{spec.name}: {error}")
            continue
        matches.extend((spec, item) for item in match_sessions(sessions, selector))

    if not matches:
        detail = f"；已跳过：{'，'.join(problems)}" if problems else ""
        raise ApiError(404, f"没有找到匹配 {selector!r} 的会话{detail}")

    matches.sort(key=lambda pair: pair[1].updated_at or "", reverse=True)
    spec, summary = matches[0]
    if not summary.cwd.startswith("/"):
        raise ApiError(400, f"会话记录的工作目录不是绝对路径: {summary.cwd!r}")
    note = None
    if len(matches) > 1:
        note = f"选择器命中 {len(matches)} 条会话，已取更新时间最新的一条"
    return await _load(spec, summary, timeout=timeout, matched=len(matches), note=note)


async def _load(
    spec: AgentSpec,
    summary: SessionSummary,
    *,
    timeout: float,
    matched: int,
    note: str | None,
) -> TranscriptReport:
    try:
        result, _probe = await load_transcript(spec, summary.session_id, summary.cwd, timeout=timeout)
    except ViewerError as exc:
        raise ApiError(502, f"加载会话 {summary.session_id} 失败: {exc}") from exc
    return TranscriptReport(
        agent=result.agent,
        session_id=result.session_id,
        cwd=summary.cwd,
        title=summary.title,
        matched=matched,
        note=note,
        load_response=result.response,
        updates=result.updates,
    )
