"""agent 配置加载的行为测试。"""

from __future__ import annotations

from pathlib import Path

import pytest

from acp_session_viewer.config import (
    DEFAULT_AGENTS,
    AgentConfigError,
    AgentSpec,
    find_agent,
    load_agents,
)


def _write(tmp_path: Path, content: str) -> Path:
    path = tmp_path / "agents.toml"
    path.write_text(content, encoding="utf-8")
    return path


def test_缺省时返回内置列表() -> None:
    assert load_agents(None) == DEFAULT_AGENTS


def test_从_toml_读取多个_agent(tmp_path: Path) -> None:
    path = _write(
        tmp_path,
        """
[[agent]]
name = "one"
command = ["echo", "1"]
description = "第一个"

[[agent]]
name = "two"
command = ["echo", "2"]
""",
    )
    agents = load_agents(path)
    assert [spec.name for spec in agents] == ["one", "two"]
    assert agents[0].command == ("echo", "1")
    assert agents[0].description == "第一个"
    assert agents[1].description == ""


def test_配置文件不存在时显式报错(tmp_path: Path) -> None:
    with pytest.raises(AgentConfigError, match="不存在"):
        load_agents(tmp_path / "missing.toml")


def test_缺少_agent_段时报错(tmp_path: Path) -> None:
    with pytest.raises(AgentConfigError, match="至少一个"):
        load_agents(_write(tmp_path, "[other]\nkey = 1\n"))


def test_command_类型错误时报错(tmp_path: Path) -> None:
    path = _write(tmp_path, '[[agent]]\nname = "x"\ncommand = "echo 1"\n')
    with pytest.raises(AgentConfigError, match="command"):
        load_agents(path)


def test_command_为空时报错(tmp_path: Path) -> None:
    path = _write(tmp_path, '[[agent]]\nname = "x"\ncommand = []\n')
    with pytest.raises(AgentConfigError, match="command"):
        load_agents(path)


def test_缺少_name_时报错(tmp_path: Path) -> None:
    path = _write(tmp_path, '[[agent]]\ncommand = ["echo"]\n')
    with pytest.raises(AgentConfigError, match="name"):
        load_agents(path)


def test_重名时报错(tmp_path: Path) -> None:
    path = _write(
        tmp_path,
        '[[agent]]\nname = "dup"\ncommand = ["echo"]\n\n[[agent]]\nname = "dup"\ncommand = ["echo"]\n',
    )
    with pytest.raises(AgentConfigError, match="重名"):
        load_agents(path)


def test_空名称的_AgentSpec_被拒绝() -> None:
    with pytest.raises(AgentConfigError, match="名称不能为空"):
        AgentSpec(name="", command=("echo",))


def test_find_agent_找到与找不到() -> None:
    agents = (AgentSpec("a", ("echo",)), AgentSpec("b", ("echo",)))
    assert find_agent(agents, "b").name == "b"
    with pytest.raises(AgentConfigError, match="未知 agent"):
        find_agent(agents, "c")
