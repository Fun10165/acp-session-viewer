"""按 agent 维护长连接池：避免每次请求都付出冷启动开销。

设计要点：

* 同一 agent 的操作串行执行（每个 agent 一把锁），不同 agent 互不影响；
* 连接空闲超过 ``idle_timeout`` 秒后由回收任务关闭，用完即走的场景不会留下常驻进程；
* 单条连接使用次数达到 ``max_uses_per_connection`` 后轮换，避免进程长期累积状态；
* 只有连接确实失效（进程退出或连接关闭）时才丢弃连接并重试一次；
  操作层面的失败（例如 agent 不支持某个能力）不动连接，直接上报。
"""

from __future__ import annotations

import asyncio
import time
from dataclasses import dataclass, field
from typing import Any, Awaitable, Callable, TypeVar

from .config import AgentSpec
from .viewer import DEFAULT_TIMEOUT, AgentConnection

T = TypeVar("T")

DEFAULT_IDLE_TIMEOUT = 300.0
DEFAULT_MAX_USES = 64
DEFAULT_SWEEP_INTERVAL = 30.0


@dataclass
class _Slot:
    """一个 agent 的连接槽位。"""

    spec: AgentSpec
    lock: asyncio.Lock = field(default_factory=asyncio.Lock)
    connection: AgentConnection | None = None
    last_used: float = 0.0
    uses: int = 0


class AgentPool:
    """长连接池。"""

    def __init__(
        self,
        *,
        timeout: float = DEFAULT_TIMEOUT,
        idle_timeout: float = DEFAULT_IDLE_TIMEOUT,
        max_uses_per_connection: int = DEFAULT_MAX_USES,
        sweep_interval: float = DEFAULT_SWEEP_INTERVAL,
        log: Callable[[str], None] | None = None,
    ) -> None:
        self._timeout = timeout
        self._idle_timeout = idle_timeout
        self._max_uses = max_uses_per_connection
        self._sweep_interval = sweep_interval
        self._log = log if log is not None else (lambda _message: None)
        self._slots: dict[str, _Slot] = {}
        self._sweeper: asyncio.Task[None] | None = None

    @property
    def timeout(self) -> float:
        """单次操作的超时秒数。"""
        return self._timeout

    async def start(self) -> None:
        """启动空闲回收任务。"""
        if self._sweeper is None:
            self._sweeper = asyncio.create_task(self._sweep_loop())

    async def close(self) -> None:
        """停止回收任务并关闭全部连接。"""
        if self._sweeper is not None:
            self._sweeper.cancel()
            try:
                await self._sweeper
            except asyncio.CancelledError:
                pass
            self._sweeper = None
        for slot in list(self._slots.values()):
            await self._discard(slot, reason="关闭连接池")
        self._slots.clear()

    async def use(self, spec: AgentSpec, action: Callable[[AgentConnection], Awaitable[T]]) -> T:
        """在指定 agent 的长连接上执行一次操作。"""
        slot = self._slots.get(spec.name)
        if slot is None:
            slot = _Slot(spec=spec)
            self._slots[spec.name] = slot

        async with slot.lock:
            for attempt in (1, 2):
                connection = await self._ensure(slot)
                try:
                    result = await action(connection)
                except Exception:
                    if connection.alive and connection.is_open:
                        # 连接仍然健康，属于操作层面的失败（例如不支持该能力），保留连接。
                        raise
                    await self._discard(slot, reason="连接已失效")
                    if attempt == 1:
                        # 换一条新连接重试一次；本工具的操作都是只读的，重试没有副作用。
                        continue
                    raise
                slot.last_used = time.monotonic()
                slot.uses += 1
                if slot.uses >= self._max_uses:
                    await self._discard(slot, reason=f"达到 {self._max_uses} 次使用上限，轮换连接")
                return result
        raise AssertionError("重试循环不应走到这里")

    async def _ensure(self, slot: _Slot) -> AgentConnection:
        connection = slot.connection
        if connection is not None and (not connection.is_open or not connection.alive):
            await self._discard(slot, reason="连接已失效")
            connection = None
        if connection is None:
            connection = AgentConnection(slot.spec)
            await connection.ensure_open(timeout=self._timeout)
            slot.connection = connection
            slot.uses = 0
            self._log(f"[pool] 建立 {slot.spec.name} 连接（pid={connection.pid}）")
        return connection

    async def _discard(self, slot: _Slot, *, reason: str) -> None:
        connection, slot.connection = slot.connection, None
        slot.uses = 0
        if connection is None:
            return
        self._log(f"[pool] 关闭 {slot.spec.name} 连接（{reason}）")
        await connection.close()

    async def _sweep_loop(self) -> None:
        while True:
            await asyncio.sleep(self._sweep_interval)
            now = time.monotonic()
            for slot in list(self._slots.values()):
                if slot.connection is None or slot.lock.locked():
                    continue
                if now - slot.last_used < self._idle_timeout:
                    continue
                async with slot.lock:
                    if slot.connection is not None and now - slot.last_used >= self._idle_timeout:
                        await self._discard(slot, reason=f"空闲超过 {self._idle_timeout:.0f} 秒")

    def status(self) -> list[dict[str, Any]]:
        """各 agent 的连接状态，供调试与健康检查展示。"""
        now = time.monotonic()
        return [
            {
                "agent": slot.spec.name,
                "connected": slot.connection is not None,
                "pid": slot.connection.pid if slot.connection is not None else None,
                "idleSeconds": round(now - slot.last_used, 1) if slot.last_used else None,
                "uses": slot.uses,
            }
            for slot in self._slots.values()
        ]
