"""agent 配置与令牌处理的行为测试。"""

from __future__ import annotations

import stat
from pathlib import Path

import pytest

from acp_session_viewer.config import (
    DEFAULT_AGENTS,
    DEFAULT_PORT,
    TOKEN_ENVIRONMENT_VARIABLE,
    AgentConfigError,
    AgentSpec,
    find_agent,
    load_agents,
    load_token,
    parse_bind,
    resolve_server_token,
    resolve_token,
    save_token,
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


def test_解析绑定地址() -> None:
    assert parse_bind("0.0.0.0") == ("0.0.0.0", DEFAULT_PORT)
    assert parse_bind("127.0.0.1:9000") == ("127.0.0.1", 9000)
    assert parse_bind("[::1]:8080") == ("::1", 8080)
    assert parse_bind("[::1]") == ("::1", DEFAULT_PORT)
    assert parse_bind("example.com", default_port=1) == ("example.com", 1)


@pytest.mark.parametrize("value", ["", "host:port", "host:0", "host:70000", "a:b:c", "[::1", "[::1]x"])
def test_绑定地址非法时显式报错(value: str) -> None:
    with pytest.raises(AgentConfigError):
        parse_bind(value)


def test_客户端令牌按参数_环境_文件顺序解析(monkeypatch: pytest.MonkeyPatch, tmp_path: Path) -> None:
    path = tmp_path / "token"
    monkeypatch.delenv(TOKEN_ENVIRONMENT_VARIABLE, raising=False)
    with pytest.raises(AgentConfigError, match="没有可用的令牌"):
        resolve_token(None, path)

    save_token(path, "来自文件")
    assert resolve_token(None, path) == "来自文件"

    monkeypatch.setenv(TOKEN_ENVIRONMENT_VARIABLE, "来自环境")
    assert resolve_token(None, path) == "来自环境"
    assert resolve_token("来自参数", path) == "来自参数"


def test_服务端令牌会生成并写回文件(monkeypatch: pytest.MonkeyPatch, tmp_path: Path) -> None:
    path = tmp_path / "nested" / "token"
    monkeypatch.delenv(TOKEN_ENVIRONMENT_VARIABLE, raising=False)

    token, source = resolve_server_token(None, path)
    assert token
    assert "新生成" in source
    assert load_token(path) == token

    again, again_source = resolve_server_token(None, path)
    assert again == token
    assert "令牌文件" in again_source


def test_令牌文件权限是_0600(tmp_path: Path) -> None:
    path = tmp_path / "token"
    save_token(path, "x")
    assert stat.S_IMODE(path.stat().st_mode) == 0o600
