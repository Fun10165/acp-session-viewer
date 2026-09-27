"""agent 注册表：从 TOML 读取，缺省使用内置列表。

TOML 结构：

    [[agent]]
    name = "opencode"
    command = ["opencode", "acp"]
    description = "可选说明"
"""

from __future__ import annotations

import tomllib
from dataclasses import dataclass
from pathlib import Path


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
