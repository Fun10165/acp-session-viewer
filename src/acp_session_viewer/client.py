"""HTTP 客户端：解析服务地址与令牌，并调用服务端接口。"""

from __future__ import annotations

import json
import os
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Sequence

import aiohttp

from .config import (
    DEFAULT_SERVER_URL,
    SERVER_ENVIRONMENT_VARIABLE,
    default_token_path,
    resolve_token,
)


class ClientError(RuntimeError):
    """调用服务端失败：连不上、鉴权失败或服务端返回错误。"""

    def __init__(self, message: str, *, status: int | None = None, not_found: bool = False) -> None:
        super().__init__(message)
        self.status = status
        self.not_found = not_found


@dataclass(frozen=True)
class ClientConfig:
    """客户端连接参数。"""

    base_url: str
    token: str


def resolve_client_config(
    server: str | None,
    token: str | None,
    token_file: Path | None,
) -> ClientConfig:
    """按 参数 → 环境变量 → 默认值 的顺序确定服务地址与令牌。"""
    base_url = server or os.environ.get(SERVER_ENVIRONMENT_VARIABLE) or DEFAULT_SERVER_URL
    base_url = base_url.strip().rstrip("/")
    if not base_url.startswith(("http://", "https://")):
        base_url = f"http://{base_url}"
    return ClientConfig(base_url=base_url, token=resolve_token(token, token_file or default_token_path()))


def _decode(text: str) -> Any:
    try:
        return json.loads(text)
    except json.JSONDecodeError:
        return None


async def _request(
    config: ClientConfig,
    path: str,
    params: Sequence[tuple[str, str]] | None,
) -> tuple[int, str]:
    url = f"{config.base_url}{path}"
    headers = {"Authorization": f"Bearer {config.token}"}
    timeout = aiohttp.ClientTimeout(total=600)
    try:
        async with aiohttp.ClientSession(headers=headers, timeout=timeout) as session:
            async with session.get(url, params=list(params) if params else None) as response:
                return response.status, await response.text()
    except aiohttp.ClientConnectorError as exc:
        raise ClientError(f"无法连接服务端 {config.base_url}（{exc}）；请先运行 acp-view serve") from exc
    except aiohttp.ClientError as exc:
        raise ClientError(f"与服务端 {config.base_url} 通信失败：{exc}") from exc


def _raise_for_status(status: int, text: str) -> None:
    if status == 200:
        return
    payload = _decode(text)
    message = payload.get("error") if isinstance(payload, dict) else text[:200]
    raise ClientError(f"服务端返回 {status}: {message}", status=status, not_found=status == 404)


async def request_json(
    config: ClientConfig,
    path: str,
    params: Sequence[tuple[str, str]] | None = None,
) -> dict[str, Any]:
    """调用接口并解析 JSON 响应。"""
    status, text = await _request(config, path, params)
    _raise_for_status(status, text)
    payload = _decode(text)
    if not isinstance(payload, dict):
        raise ClientError(f"服务端返回的不是 JSON 对象: {text[:200]}")
    return payload


async def request_text(
    config: ClientConfig,
    path: str,
    params: Sequence[tuple[str, str]] | None = None,
) -> str:
    """调用接口并返回纯文本响应。"""
    status, text = await _request(config, path, params)
    _raise_for_status(status, text)
    return text
