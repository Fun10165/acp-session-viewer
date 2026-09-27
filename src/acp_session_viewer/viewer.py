"""只读 ACP 客户端：探测能力、枚举会话、重放会话。

只读契约（由代码结构保证）：

* 客户端只发送三个方法：``initialize``、``session/list``、``session/load``。
* 初始化时声明空的能力集，agent 无法要求客户端读写文件或执行命令。
* 服务端发来的任何反向请求一律拒绝：权限申请返回 cancelled，其余报"方法不存在"。
* 不触碰任何 agent 的本地存储文件。
"""

from __future__ import annotations

import asyncio
import os
from dataclasses import dataclass
from typing import Any, Awaitable, Callable, Sequence

import acp
from acp.core import DEFAULT_STDIO_BUFFER_LIMIT_BYTES
from acp.schema import DeniedOutcome, Implementation, RequestPermissionResponse

from .config import AgentSpec


class ViewerError(RuntimeError):
    """与 agent 交互失败：启动失败、超时、协议错误或不支持所需能力。"""


@dataclass(frozen=True)
class SessionSummary:
    """一条会话的元信息。"""

    agent: str
    session_id: str
    cwd: str
    title: str | None
    updated_at: str | None


@dataclass(frozen=True)
class ProbeResult:
    """一次 initialize 之后得到的 agent 能力快照。"""

    agent: AgentSpec
    protocol_version: int
    agent_info: dict[str, Any]
    capabilities: dict[str, Any]
    auth_methods: list[dict[str, Any]]

    @property
    def can_list(self) -> bool:
        session_caps = self.capabilities.get("sessionCapabilities") or {}
        return "list" in session_caps

    @property
    def can_load(self) -> bool:
        return bool(self.capabilities.get("loadSession"))


@dataclass(frozen=True)
class LoadResult:
    """一次会话重放：加载响应与全部 session/update 事件。"""

    agent: str
    session_id: str
    response: dict[str, Any]
    updates: list[dict[str, Any]]
    refused_requests: list[str]


class ReadOnlyClient:
    """记录 session/update 通知，并拒绝所有反向请求。"""

    def __init__(self) -> None:
        self.updates: list[dict[str, Any]] = []
        self.refused_requests: list[str] = []

    async def session_update(self, session_id: str, update: Any, **kwargs: Any) -> None:
        self.updates.append(_dump(update))

    async def request_permission(self, *args: Any, **kwargs: Any) -> RequestPermissionResponse:
        self.refused_requests.append("session/request_permission")
        return RequestPermissionResponse(outcome=DeniedOutcome(outcome="cancelled"))


def _dump(model: Any) -> dict[str, Any]:
    """把 SDK 模型转成接近线上格式的普通字典。"""
    if isinstance(model, dict):
        return model
    if hasattr(model, "model_dump"):
        return model.model_dump(by_alias=True, exclude_none=True)
    raise ViewerError(f"无法序列化的事件类型: {type(model)!r}")


async def _run_with_agent(
    spec: AgentSpec,
    action: Callable[[acp.ClientSideConnection, ProbeResult, ReadOnlyClient], Awaitable[Any]],
    *,
    timeout: float,
    need_list: bool = False,
    need_load: bool = False,
) -> Any:
    """启动 agent、完成 initialize、执行动作，然后关闭进程。"""
    client = ReadOnlyClient()
    try:
        async with asyncio.timeout(timeout):
            async with acp.spawn_agent_process(
                client,
                *spec.command,
                env=dict(os.environ),
                transport_kwargs={"limit": DEFAULT_STDIO_BUFFER_LIMIT_BYTES},
            ) as (connection, _process):
                init = await connection.initialize(
                    protocol_version=acp.PROTOCOL_VERSION,
                    client_capabilities=None,
                    client_info=Implementation(name="acp-session-viewer", title="ACP 会话查看器", version="0.1.0"),
                )
                init_data = _dump(init)
                probe = ProbeResult(
                    agent=spec,
                    protocol_version=int(init_data.get("protocolVersion", acp.PROTOCOL_VERSION)),
                    agent_info=init_data.get("agentInfo") or {},
                    capabilities=init_data.get("agentCapabilities") or {},
                    auth_methods=init_data.get("authMethods") or [],
                )
                if need_list and not probe.can_list:
                    raise ViewerError(f"agent {spec.name} 未声明 sessionCapabilities.list，无法枚举会话")
                if need_load and not probe.can_load:
                    raise ViewerError(f"agent {spec.name} 未声明 loadSession，无法加载会话")
                return await action(connection, probe, client)
    except TimeoutError as exc:
        raise ViewerError(f"agent {spec.name} 在 {timeout:.0f} 秒内没有完成响应") from exc
    except FileNotFoundError as exc:
        raise ViewerError(f"agent {spec.name} 的命令不存在: {' '.join(spec.command)}") from exc


async def probe(spec: AgentSpec, *, timeout: float = 60.0) -> ProbeResult:
    """只做 initialize，返回能力快照。"""

    async def action(_connection: acp.ClientSideConnection, result: ProbeResult, _client: ReadOnlyClient) -> ProbeResult:
        return result

    return await _run_with_agent(spec, action, timeout=timeout)


async def list_sessions(
    spec: AgentSpec,
    *,
    timeout: float = 60.0,
    max_pages: int = 20,
) -> tuple[list[SessionSummary], ProbeResult]:
    """分页枚举该 agent 的全部会话。"""
    seen_cursors: set[str] = set()

    async def action(
        connection: acp.ClientSideConnection, result: ProbeResult, _client: ReadOnlyClient
    ) -> tuple[list[SessionSummary], ProbeResult]:
        summaries: list[SessionSummary] = []
        cursor: str | None = None
        for _ in range(max_pages):
            response = _dump(await connection.list_sessions(cursor=cursor))
            for item in response.get("sessions") or []:
                summaries.append(
                    SessionSummary(
                        agent=spec.name,
                        session_id=item["sessionId"],
                        cwd=item.get("cwd", ""),
                        title=item.get("title"),
                        updated_at=item.get("updatedAt"),
                    )
                )
            next_cursor = response.get("nextCursor")
            if not next_cursor:
                return summaries, result
            if next_cursor in seen_cursors:
                raise ViewerError(f"agent {spec.name} 返回了重复的翻页游标，已停止以避免死循环")
            seen_cursors.add(next_cursor)
            cursor = next_cursor
        raise ViewerError(f"agent {spec.name} 的会话翻页超过 {max_pages} 页，已停止")

    return await _run_with_agent(spec, action, timeout=timeout, need_list=True)


async def load_transcript(
    spec: AgentSpec,
    session_id: str,
    cwd: str,
    *,
    timeout: float = 120.0,
    quiet_period: float = 0.3,
) -> tuple[LoadResult, ProbeResult]:
    """加载一条会话并收集重放的全部事件。"""

    async def action(
        connection: acp.ClientSideConnection, result: ProbeResult, client: ReadOnlyClient
    ) -> tuple[LoadResult, ProbeResult]:
        response = _dump(await connection.load_session(cwd=cwd, session_id=session_id))
        await _wait_for_replay_tail(client, quiet_period=quiet_period)
        load_result = LoadResult(
            agent=spec.name,
            session_id=session_id,
            response=response,
            updates=list(client.updates),
            refused_requests=list(client.refused_requests),
        )
        return load_result, result

    return await _run_with_agent(spec, action, timeout=timeout, need_load=True)


async def _wait_for_replay_tail(client: ReadOnlyClient, *, quiet_period: float, max_wait: float = 5.0) -> None:
    """等重放事件全部到达：静默 quiet_period 秒后认为结束。

    session/load 的响应可能早于最后几条通知处理完成，因此需要这段等待。
    """
    loop = asyncio.get_running_loop()
    deadline = loop.time() + max_wait
    while True:
        seen = len(client.updates)
        await asyncio.sleep(quiet_period)
        if len(client.updates) == seen or loop.time() >= deadline:
            return


def select_agents(agents: Sequence[AgentSpec], names: Sequence[str] | None) -> list[AgentSpec]:
    """按命令行给出的名称筛选 agent；未给出时返回全部。"""
    if not names:
        return list(agents)
    index = {spec.name: spec for spec in agents}
    missing = [name for name in names if name not in index]
    if missing:
        known = ", ".join(index)
        raise ViewerError(f"未知 agent {missing}；可用: {known}")
    return [index[name] for name in names]
