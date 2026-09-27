"""HTTP 服务端：把只读 ACP 能力暴露为带令牌鉴权的接口。

接口一览（全部需要 ``Authorization: Bearer <token>``）：

* ``GET /health`` —— 健康检查，附带连接池状态
* ``GET /agents`` —— 各 agent 的 ACP 能力
* ``GET /sessions?agent=&query=&limit=`` —— 会话列表
* ``GET /transcript?selector=&agent=&cwd=&format=json|text`` —— 会话重放

agent 连接由连接池维护，跨请求复用，空闲超过阈值后回收；每个请求都会打印一行访问记录。
"""

from __future__ import annotations

import secrets
import time
from dataclasses import asdict
from typing import Any, Sequence

from aiohttp import web

from . import __version__
from .agent_pool import DEFAULT_IDLE_TIMEOUT, DEFAULT_MAX_USES, AgentPool
from .api import ApiError, agents_report, sessions_report, transcript_report
from .config import AgentSpec
from .render import RenderOptions, apply_budget, render_transcript
from .viewer import SessionSummary, ViewerError, select_agents

MAX_BUDGET = 10**9

TOKEN_KEY: web.AppKey[str] = web.AppKey("token", str)
AGENTS_KEY: web.AppKey[tuple[AgentSpec, ...]] = web.AppKey("agents", tuple)
POOL_KEY: web.AppKey[AgentPool] = web.AppKey("pool", AgentPool)
LOG_REQUESTS_KEY: web.AppKey[bool] = web.AppKey("log_requests", bool)


def _error(status: int, message: str) -> web.Response:
    return web.json_response({"error": message}, status=status)


@web.middleware
async def _access_log(request: web.Request, handler: Any) -> web.StreamResponse:
    started = time.perf_counter()
    try:
        response = await handler(request)
    except ApiError as exc:
        response = _error(exc.status, exc.message)
    except web.HTTPException as exc:
        response = _error(exc.status, exc.reason or "")
    except Exception:
        _log(request, started, 500)
        raise
    _log(request, started, response.status)
    return response


def _log(request: web.Request, started: float, status: int) -> None:
    if not request.app[LOG_REQUESTS_KEY]:
        return
    elapsed_ms = (time.perf_counter() - started) * 1000
    stamp = time.strftime("%Y-%m-%dT%H:%M:%S")
    print(f"[{stamp}] {request.remote} {request.method} {request.path_qs} → {status} {elapsed_ms:.0f}ms", flush=True)


@web.middleware
async def _require_token(request: web.Request, handler: Any) -> web.StreamResponse:
    header = request.headers.get("Authorization", "")
    prefix = "Bearer "
    provided = header[len(prefix) :] if header.startswith(prefix) else ""
    # 比较字节串：compare_digest 不接受含非 ASCII 字符的字符串，否则会抛异常返回 500。
    if not provided or not secrets.compare_digest(provided.encode("utf-8"), request.app[TOKEN_KEY].encode("utf-8")):
        return _error(401, "缺少或错误的令牌：请提供 Authorization: Bearer <token>")
    return await handler(request)


def _int_param(request: web.Request, name: str, default: int, *, minimum: int, maximum: int) -> int:
    raw = request.query.get(name)
    if raw is None or raw == "":
        return default
    try:
        value = int(raw)
    except ValueError:
        raise ApiError(400, f"参数 {name} 必须是整数: {raw!r}") from None
    if not minimum <= value <= maximum:
        raise ApiError(400, f"参数 {name} 超出范围 {minimum}-{maximum}: {value}")
    return value


def _bool_param(request: web.Request, name: str, default: bool) -> bool:
    raw = request.query.get(name)
    if raw is None:
        return default
    lowered = raw.strip().lower()
    if lowered in ("1", "true", "yes", "on"):
        return True
    if lowered in ("0", "false", "no", "off"):
        return False
    raise ApiError(400, f"参数 {name} 必须是布尔值: {raw!r}")


async def _start_pool(app: web.Application) -> None:
    await app[POOL_KEY].start()


async def _stop_pool(app: web.Application) -> None:
    await app[POOL_KEY].close()


async def _handle_health(request: web.Request) -> web.Response:
    return web.json_response(
        {
            "ok": True,
            "version": __version__,
            "connections": request.app[POOL_KEY].status(),
        }
    )


def _session_json(item: SessionSummary) -> dict[str, Any]:
    """会话摘要的线上表示，字段名与 ACP 线格式保持一致（驼峰命名）。"""
    return {
        "agent": item.agent,
        "sessionId": item.session_id,
        "cwd": item.cwd,
        "title": item.title,
        "updatedAt": item.updated_at,
    }


async def _handle_agents(request: web.Request) -> web.Response:
    reports = await agents_report(request.app[AGENTS_KEY], pool=request.app[POOL_KEY])
    return web.json_response({"agents": [asdict(report) for report in reports]})


async def _handle_sessions(request: web.Request) -> web.Response:
    names = request.query.getall("agent", None)
    limit = _int_param(request, "limit", 30, minimum=1, maximum=10000)
    query = request.query.get("query") or None
    try:
        selected = select_agents(request.app[AGENTS_KEY], names)
    except ViewerError as exc:
        raise ApiError(400, str(exc)) from exc
    report = await sessions_report(selected, query=query, limit=limit, pool=request.app[POOL_KEY])
    return web.json_response(
        {
            "sessions": [_session_json(item) for item in report.sessions],
            "total": report.total,
            "errors": [{"agent": agent, "message": message} for agent, message in report.errors],
        }
    )


async def _handle_transcript(request: web.Request) -> web.Response:
    selector = request.query.get("selector")
    if not selector:
        raise ApiError(400, "缺少 selector 参数")
    output_format = request.query.get("format", "json")
    if output_format not in ("json", "text"):
        raise ApiError(400, f"未知 format: {output_format!r}，可用 json 或 text")
    names = request.query.getall("agent", None)
    cwd = request.query.get("cwd") or None
    try:
        selected = select_agents(request.app[AGENTS_KEY], names)
    except ViewerError as exc:
        raise ApiError(400, str(exc)) from exc

    report = await transcript_report(
        selected,
        selector=selector,
        agent_names=None,
        cwd=cwd,
        pool=request.app[POOL_KEY],
    )

    if output_format == "json":
        return web.json_response(
            {
                "agent": report.agent,
                "sessionId": report.session_id,
                "cwd": report.cwd,
                "title": report.title,
                "matched": report.matched,
                "note": report.note,
                "loadResponse": report.load_response,
                "updates": report.updates,
            }
        )

    options = RenderOptions(
        show_thoughts=not _bool_param(request, "no_thoughts", False),
        show_meta=_bool_param(request, "meta", False),
        truncate=_int_param(request, "truncate", 400, minimum=0, maximum=10**6),
    )
    text = render_transcript(report.updates, options)
    header = f"# {report.agent} / {report.session_id} / {report.cwd}（{len(report.updates)} 条事件）"
    if report.note:
        header += f"\n# {report.note}"
    body = f"{header}\n{text}" if text else f"{header}\n（该会话没有可渲染的事件）"
    bounded, _dropped = apply_budget(body, _int_param(request, "budget", 12000, minimum=0, maximum=MAX_BUDGET))
    return web.Response(text=bounded, content_type="text/plain", charset="utf-8")


def build_app(
    *,
    token: str,
    agents: Sequence[AgentSpec],
    pool: AgentPool,
    log_requests: bool = True,
) -> web.Application:
    """构造 aiohttp 应用，便于测试时单独装配。"""
    app = web.Application(middlewares=[_access_log, _require_token])
    app[TOKEN_KEY] = token
    app[AGENTS_KEY] = tuple(agents)
    app[POOL_KEY] = pool
    app[LOG_REQUESTS_KEY] = log_requests
    app.router.add_get("/health", _handle_health)
    app.router.add_get("/agents", _handle_agents)
    app.router.add_get("/sessions", _handle_sessions)
    app.router.add_get("/transcript", _handle_transcript)
    app.on_startup.append(_start_pool)
    app.on_cleanup.append(_stop_pool)
    return app


def run_server(
    *,
    host: str,
    port: int,
    token: str,
    agents: Sequence[AgentSpec],
    timeout: float,
    idle_timeout: float = DEFAULT_IDLE_TIMEOUT,
    max_uses_per_connection: int = DEFAULT_MAX_USES,
) -> None:
    """启动服务，直到收到中断信号。"""
    pool = AgentPool(
        timeout=timeout,
        idle_timeout=idle_timeout,
        max_uses_per_connection=max_uses_per_connection,
        log=lambda message: print(message, flush=True),
    )
    app = build_app(token=token, agents=agents, pool=pool)
    web.run_app(app, host=host, port=port, print=None, access_log=None)
