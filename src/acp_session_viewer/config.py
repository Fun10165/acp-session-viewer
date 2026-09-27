"""agent 注册表：从 TOML 读取，缺省使用内置列表。

TOML 结构：

    [[agent]]
    name = "opencode"
    command = ["opencode", "acp"]
    description = "可选说明"
"""

from __future__ import annotations

import os
import secrets
import tomllib
from dataclasses import dataclass
from pathlib import Path

DEFAULT_SERVER_URL = "http://127.0.0.1:8787"
DEFAULT_BIND = "127.0.0.1:8787"
DEFAULT_PORT = 8787
DEFAULT_BUDGET = 12000
DEFAULT_LIST_LIMIT = 30
TOKEN_ENVIRONMENT_VARIABLE = "ACP_VIEW_TOKEN"
SERVER_ENVIRONMENT_VARIABLE = "ACP_VIEW_SERVER"


class AgentConfigError(RuntimeError):
    """配置文件缺失、语法错误或结构不符合约定。"""


@dataclass(frozen=True)
class AgentSpec:
    """一个 ACP 服务端的启动方式。"""

    name: str
    command: tuple[str, ...]
    description: str = ""

    def __post_init__(self) -> None:
        if not self.name:
            raise AgentConfigError("agent 名称不能为空")
        if not self.command:
            raise AgentConfigError(f"agent {self.name!r} 的命令不能为空")


DEFAULT_AGENTS: tuple[AgentSpec, ...] = (
    AgentSpec("omp", ("omp", "acp"), "omp 自带的 ACP 服务端"),
    AgentSpec("opencode", ("opencode", "acp"), "OpenCode 自带的 ACP 服务端"),
    AgentSpec("codex", ("npx", "-y", "@agentclientprotocol/codex-acp"), "Codex 官方 ACP 适配器（首次运行需要联网下载）"),
    AgentSpec("cursor", ("cursor-agent", "acp"), "Cursor Agent 的 ACP 服务端"),
    AgentSpec("gemini", ("gemini", "--acp"), "Gemini CLI 的 ACP 模式（不支持会话枚举）"),
)


def load_agents(path: Path | None) -> tuple[AgentSpec, ...]:
    """读取 agent 配置；path 为 None 时返回内置列表。"""
    if path is None:
        return DEFAULT_AGENTS
    if not path.is_file():
        raise AgentConfigError(f"配置文件不存在: {path}")

    data = tomllib.loads(path.read_text(encoding="utf-8"))
    raw_agents = data.get("agent")
    if not isinstance(raw_agents, list) or not raw_agents:
        raise AgentConfigError(f"{path} 必须包含至少一个 [[agent]] 段")

    agents: list[AgentSpec] = []
    for index, raw in enumerate(raw_agents, start=1):
        if not isinstance(raw, dict):
            raise AgentConfigError(f"{path} 第 {index} 个 [[agent]] 不是表")
        name = raw.get("name")
        command = raw.get("command")
        description = raw.get("description", "")
        if not isinstance(name, str) or not name:
            raise AgentConfigError(f"{path} 第 {index} 个 [[agent]] 缺少 name")
        if not isinstance(command, list) or not command or not all(isinstance(item, str) for item in command):
            raise AgentConfigError(f"agent {name!r} 的 command 必须是非空字符串数组")
        if not isinstance(description, str):
            raise AgentConfigError(f"agent {name!r} 的 description 必须是字符串")
        agents.append(AgentSpec(name=name, command=tuple(command), description=description))

    names = [spec.name for spec in agents]
    if len(set(names)) != len(names):
        raise AgentConfigError(f"{path} 中存在重名 agent: {names}")
    return tuple(agents)


def find_agent(agents: tuple[AgentSpec, ...], name: str) -> AgentSpec:
    """按名称查找 agent，找不到时显式报错并列出可用名称。"""
    for spec in agents:
        if spec.name == name:
            return spec
    known = ", ".join(spec.name for spec in agents)
    raise AgentConfigError(f"未知 agent {name!r}；可用: {known}")


def default_token_path() -> Path:
    """默认的令牌文件位置。"""
    return Path.home() / ".config" / "acp-session-viewer" / "token"


def parse_bind(value: str, *, default_port: int = DEFAULT_PORT) -> tuple[str, int]:
    """解析 HOST、HOST:PORT、[IPv6]:PORT 三种绑定写法。"""
    if not value:
        raise AgentConfigError("绑定地址不能为空")
    if value.startswith("["):
        host, separator, rest = value[1:].partition("]")
        if not separator:
            raise AgentConfigError(f"IPv6 绑定地址缺少右方括号: {value!r}")
        if rest == "":
            address = (host, default_port)
        elif rest.startswith(":"):
            address = (host, _parse_port(rest[1:], value))
        else:
            raise AgentConfigError(f"绑定地址格式错误: {value!r}")
    elif value.count(":") == 1:
        host, port_text = value.split(":")
        address = (host, _parse_port(port_text, value))
    elif ":" in value:
        raise AgentConfigError(f"IPv6 地址请写成 [地址]:端口 的形式: {value!r}")
    else:
        address = (value, default_port)
    if not address[0]:
        raise AgentConfigError(f"绑定地址缺少主机部分: {value!r}")
    return address


def _parse_port(text: str, source: str) -> int:
    if not text.isdigit():
        raise AgentConfigError(f"端口必须是数字: {source!r}")
    port = int(text)
    if not 1 <= port <= 65535:
        raise AgentConfigError(f"端口超出 1-65535: {source!r}")
    return port


def generate_token() -> str:
    """生成一个新的访问令牌。"""
    return secrets.token_urlsafe(32)


def load_token(path: Path) -> str | None:
    """读取令牌文件；文件不存在或内容为空时返回 None。"""
    if not path.is_file():
        return None
    token = path.read_text(encoding="utf-8").strip()
    return token or None


def save_token(path: Path, token: str) -> None:
    """写入令牌文件，权限固定为 0600。"""
    path.parent.mkdir(parents=True, exist_ok=True)
    descriptor = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
    with os.fdopen(descriptor, "w", encoding="utf-8") as handle:
        handle.write(token + "\n")


def resolve_token(explicit: str | None, path: Path) -> str:
    """客户端取令牌：参数优先，其次环境变量，最后令牌文件。"""
    if explicit:
        return explicit
    from_environment = os.environ.get(TOKEN_ENVIRONMENT_VARIABLE)
    if from_environment:
        return from_environment
    stored = load_token(path)
    if stored:
        return stored
    raise AgentConfigError(
        f"没有可用的令牌：请用 --token 指定，或设置 {TOKEN_ENVIRONMENT_VARIABLE} 环境变量，"
        f"或先由服务端生成 {path}"
    )


def resolve_server_token(explicit: str | None, path: Path) -> tuple[str, str]:
    """服务端取令牌，取不到时新生成并写入令牌文件，返回（令牌, 来源说明）。"""
    if explicit:
        return explicit, "--token 参数"
    from_environment = os.environ.get(TOKEN_ENVIRONMENT_VARIABLE)
    if from_environment:
        return from_environment, f"环境变量 {TOKEN_ENVIRONMENT_VARIABLE}"
    stored = load_token(path)
    if stored:
        return stored, f"令牌文件 {path}"
    generated = generate_token()
    save_token(path, generated)
    return generated, f"新生成并写入 {path}"
