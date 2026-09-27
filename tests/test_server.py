"""服务端鉴权、路由与错误映射的行为测试（不启动真实 agent）。"""

from __future__ import annotations

import asyncio

from aiohttp.test_utils import TestClient, TestServer

from acp_session_viewer.agent_pool import AgentPool
from acp_session_viewer.server import _session_json, build_app
from acp_session_viewer.viewer import SessionSummary

TOKEN = "test-token-123"


def _client():
    pool = AgentPool(timeout=5, idle_timeout=60, max_uses_per_connection=8)
    app = build_app(token=TOKEN, agents=(), pool=pool, log_requests=False)
    return TestClient(TestServer(app))


def _auth() -> dict[str, str]:
    return {"Authorization": f"Bearer {TOKEN}"}


def test_缺少令牌时返回401() -> None:
    async def scenario() -> None:
        async with _client() as client:
            response = await client.get("/health")
            assert response.status == 401
            payload = await response.json()
            assert "令牌" in payload["error"]

    asyncio.run(scenario())


def test_令牌错误时返回401() -> None:
    async def scenario() -> None:
        async with _client() as client:
            response = await client.get("/health", headers={"Authorization": "Bearer 错的"})
            assert response.status == 401

    asyncio.run(scenario())


def test_健康检查在正确令牌下返回版本() -> None:
    async def scenario() -> None:
        async with _client() as client:
            response = await client.get("/health", headers=_auth())
            assert response.status == 200
            payload = await response.json()
            assert payload["ok"] is True
            assert "version" in payload

    asyncio.run(scenario())


def test_未知路径返回_json_错误() -> None:
    async def scenario() -> None:
        async with _client() as client:
            response = await client.get("/不存在的路径", headers=_auth())
            assert response.status == 404
            payload = await response.json()
            assert "error" in payload

    asyncio.run(scenario())


def test_缺少选择器时返回400() -> None:
    async def scenario() -> None:
        async with _client() as client:
            response = await client.get("/transcript", headers=_auth())
            assert response.status == 400
            payload = await response.json()
            assert "selector" in payload["error"]

    asyncio.run(scenario())


def test_未知输出格式时返回400() -> None:
    async def scenario() -> None:
        async with _client() as client:
            response = await client.get("/transcript", params={"selector": "x", "format": "xml"}, headers=_auth())
            assert response.status == 400

    asyncio.run(scenario())


def test_未知_agent_时返回400() -> None:
    async def scenario() -> None:
        async with _client() as client:
            response = await client.get("/sessions", params={"agent": "不存在"}, headers=_auth())
            assert response.status == 400
            payload = await response.json()
            assert "未知 agent" in payload["error"]

    asyncio.run(scenario())


def test_非整数参数时返回400() -> None:
    async def scenario() -> None:
        async with _client() as client:
            response = await client.get("/sessions", params={"limit": "abc"}, headers=_auth())
            assert response.status == 400
            payload = await response.json()
            assert "limit" in payload["error"]

    asyncio.run(scenario())


def test_会话摘要使用驼峰字段名() -> None:
    payload = _session_json(
        SessionSummary(
            agent="omp",
            session_id="01a0e148",
            cwd="/Users/x/tmp",
            title="标题",
            updated_at="2026-09-27T05:38:05Z",
        )
    )
    assert payload == {
        "agent": "omp",
        "sessionId": "01a0e148",
        "cwd": "/Users/x/tmp",
        "title": "标题",
        "updatedAt": "2026-09-27T05:38:05Z",
    }
