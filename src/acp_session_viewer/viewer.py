"""只读 ACP 客户端：探测能力、枚举会话、重放会话。

只读契约（由代码结构保证）：

* 客户端只发送三个方法：``initialize``、``session/list``、``session/load``。
* 初始化时声明空的能力集，agent 无法要求客户端读写文件或执行命令。
* 服务端发来的任何反向请求一律拒绝：权限申请返回 cancelled，其余报"方法不存在"。
* 不触碰任何 agent 的本地存储文件。

``AgentConnection`` 是一条可复用的长连接（持有子进程与 ACP 连接），
连接复用与空闲回收由 ``agent_pool.AgentPool`` 负责。
``probe`` / ``list_sessions`` / ``load_transcript`` 三个函数是冷启动一次的便捷封装。
"""

from __future__ import annotations

import asyncio
import os
from contextlib import AsyncExitStack, contextmanager
from dataclasses import dataclass
from typing import Any, Iterator, Sequence

import acp
from acp.core import DEFAULT_STDIO_BUFFER_LIMIT_BYTES
from acp.schema import DeniedOutcome, Implementation, RequestPermissionResponse

from . import __version__
from .config import AgentSpec

DEFAULT_TIMEOUT = 180.0
DEFAULT_QUIET_PERIOD = 0.3
MAX_LIST_PAGES = 20


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


@contextmanager
def _translate_errors(agent_name: str, operation: str, timeout: float) -> Iterator[None]:
    """把超时与传输层中断统一翻译成 ViewerError，避免它们以裸异常逃到上层。"""
    try:
        yield
    except TimeoutError as exc:
        raise ViewerError(f"agent {agent_name} {operation}超时（{timeout:.0f} 秒）") from exc
    except (ConnectionError, EOFError, OSError) as exc:
        raise ViewerError(f"agent {agent_name} 的连接中断: {exc}") from exc


class AgentConnection:
    """一条到 agent 的长连接：持有子进程与 ACP 连接，可以复用。"""

    def __init__(self, spec: AgentSpec) -> None:
        self.spec = spec
        self.probe: ProbeResult | None = None
        self.load_count = 0
        self._client = ReadOnlyClient()
        self._stack: AsyncExitStack | None = None
        self._connection: acp.ClientSideConnection | None = None
        self._process: Any = None

    @property
    def pid(self) -> int | None:
        return None if self._process is None else self._process.pid

    @property
    def is_open(self) -> bool:
        return self._connection is not None

    @property
    def alive(self) -> bool:
        return self._process is not None and self._process.returncode is None

    async def ensure_open(self, *, timeout: float = DEFAULT_TIMEOUT) -> ProbeResult:
        """建立连接并完成初始化；已建立时直接返回能力快照。"""
        if self.probe is not None:
            return self.probe

        stack = AsyncExitStack()
        try:
            connection, process = await stack.enter_async_context(
                acp.spawn_agent_process(
                    self._client,
                    *self.spec.command,
                    env=dict(os.environ),
                    transport_kwargs={"limit": DEFAULT_STDIO_BUFFER_LIMIT_BYTES},
                )
            )
            async with asyncio.timeout(timeout):
                init = _dump(
                    await connection.initialize(
                        protocol_version=acp.PROTOCOL_VERSION,
                        client_capabilities=None,
                        client_info=Implementation(
                            name="acp-session-viewer",
                            title="ACP 会话查看器",
                            version=__version__,
                        ),
                    )
                )
        except TimeoutError as exc:
            await stack.aclose()
            raise ViewerError(f"agent {self.spec.name} 在 {timeout:.0f} 秒内没有完成初始化") from exc
        except FileNotFoundError as exc:
            await stack.aclose()
            raise ViewerError(f"agent {self.spec.name} 的命令不存在: {' '.join(self.spec.command)}") from exc
        except Exception:
            await stack.aclose()
            raise

        self._stack = stack
        self._connection = connection
        self._process = process
        self.probe = ProbeResult(
            agent=self.spec,
            protocol_version=int(init.get("protocolVersion", acp.PROTOCOL_VERSION)),
            agent_info=init.get("agentInfo") or {},
            capabilities=init.get("agentCapabilities") or {},
            auth_methods=init.get("authMethods") or [],
        )
        return self.probe

    async def close(self) -> None:
        """关闭连接与子进程。"""
        stack, self._stack = self._stack, None
        self._connection = None
        self._process = None
        self.probe = None
        if stack is not None:
            await stack.aclose()

    async def _require_connection(self) -> acp.ClientSideConnection:
        if self._connection is None:
            raise ViewerError(f"agent {self.spec.name} 的连接尚未建立")
        return self._connection

    async def list_sessions(
        self,
        *,
        timeout: float = DEFAULT_TIMEOUT,
        max_pages: int = MAX_LIST_PAGES,
    ) -> list[SessionSummary]:
        """分页枚举该 agent 的全部会话。"""
        if self.probe is None or not self.probe.can_list:
            raise ViewerError(f"agent {self.spec.name} 未声明 sessionCapabilities.list，无法枚举会话")
        connection = await self._require_connection()
        summaries: list[SessionSummary] = []
        seen_cursors: set[str] = set()
        cursor: str | None = None
        with _translate_errors(self.spec.name, "枚举会话", timeout):
            async with asyncio.timeout(timeout):
                for _ in range(max_pages):
                    response = _dump(await connection.list_sessions(cursor=cursor))
                    for item in response.get("sessions") or []:
                        summaries.append(
                            SessionSummary(
                                agent=self.spec.name,
                                session_id=item["sessionId"],
                                cwd=item.get("cwd", ""),
                                title=item.get("title"),
                                updated_at=item.get("updatedAt"),
                            )
                        )
                    next_cursor = response.get("nextCursor")
                    if not next_cursor:
                        return summaries
                    if next_cursor in seen_cursors:
                        raise ViewerError(f"agent {self.spec.name} 返回了重复的翻页游标，已停止以避免死循环")
                    seen_cursors.add(next_cursor)
                    cursor = next_cursor
        raise ViewerError(f"agent {self.spec.name} 的会话翻页超过 {max_pages} 页，已停止")

    async def load_transcript(
        self,
        session_id: str,
        cwd: str,
        *,
        timeout: float = DEFAULT_TIMEOUT,
        quiet_period: float = DEFAULT_QUIET_PERIOD,
    ) -> LoadResult:
        """加载一条会话并收集重放的全部事件。"""
        if self.probe is None or not self.probe.can_load:
            raise ViewerError(f"agent {self.spec.name} 未声明 loadSession，无法加载会话")
        connection = await self._require_connection()
        self._client.updates.clear()
        self._client.refused_requests.clear()
        with _translate_errors(self.spec.name, "加载会话", timeout):
            async with asyncio.timeout(timeout):
                response = _dump(await connection.load_session(cwd=cwd, session_id=session_id))
                await _wait_for_replay_tail(self._client, quiet_period=quiet_period)
        self.load_count += 1
        return LoadResult(
            agent=self.spec.name,
            session_id=session_id,
            response=response,
            updates=list(self._client.updates),
            refused_requests=list(self._client.refused_requests),
        )


async def probe(spec: AgentSpec, *, timeout: float = DEFAULT_TIMEOUT) -> ProbeResult:
    """冷启动一次，只做初始化。"""
    connection = AgentConnection(spec)
    try:
        return await connection.ensure_open(timeout=timeout)
    finally:
        await connection.close()


async def list_sessions(
    spec: AgentSpec,
    *,
    timeout: float = DEFAULT_TIMEOUT,
    max_pages: int = MAX_LIST_PAGES,
) -> tuple[list[SessionSummary], ProbeResult]:
    """冷启动一次并枚举会话。"""
    connection = AgentConnection(spec)
    try:
        result = await connection.ensure_open(timeout=timeout)
        sessions = await connection.list_sessions(timeout=timeout, max_pages=max_pages)
        return sessions, result
    finally:
        await connection.close()


async def load_transcript(
    spec: AgentSpec,
    session_id: str,
    cwd: str,
    *,
    timeout: float = DEFAULT_TIMEOUT,
    quiet_period: float = DEFAULT_QUIET_PERIOD,
) -> tuple[LoadResult, ProbeResult]:
    """冷启动一次并重放一条会话。"""
    connection = AgentConnection(spec)
    try:
        result = await connection.ensure_open(timeout=timeout)
        transcript = await connection.load_transcript(session_id, cwd, timeout=timeout, quiet_period=quiet_period)
        return transcript, result
    finally:
        await connection.close()


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
