"""渲染器与字符上界的行为测试，以及只读客户端的拒绝行为。"""

from __future__ import annotations

import asyncio

import pytest

from acp_session_viewer.render import RenderOptions, apply_budget, content_block_text, render_transcript
from acp_session_viewer.viewer import ReadOnlyClient


def _user(text: str, message_id: str = "m1") -> dict:
    return {"sessionUpdate": "user_message_chunk", "messageId": message_id, "content": {"type": "text", "text": text}}


def _agent(text: str, message_id: str = "m2") -> dict:
    return {"sessionUpdate": "agent_message_chunk", "messageId": message_id, "content": {"type": "text", "text": text}}


def _thought(text: str, message_id: str = "m3") -> dict:
    return {"sessionUpdate": "agent_thought_chunk", "messageId": message_id, "content": {"type": "text", "text": text}}


def test_同一消息的多个片段合并成一段() -> None:
    text = render_transcript([_agent("你"), _agent("好"), _agent("呀")])
    assert text == "[助手] 你好呀"


def test_不同消息之间会断开() -> None:
    text = render_transcript([_agent("第一句", "a"), _agent("第二句", "b")])
    assert text.splitlines() == ["[助手] 第一句", "[助手] 第二句"]


def test_用户与助手分别成段() -> None:
    text = render_transcript([_user("问题"), _agent("答案")])
    assert text.splitlines() == ["[用户] 问题", "[助手] 答案"]


def test_思考片段可以隐藏() -> None:
    updates = [_thought("内部推理"), _agent("最终回答")]
    assert render_transcript(updates).splitlines() == ["[思考] 内部推理", "[助手] 最终回答"]
    hidden = render_transcript(updates, RenderOptions(show_thoughts=False))
    assert hidden == "[助手] 最终回答"


def test_多行内容缩进对齐() -> None:
    text = render_transcript([_agent("第一行\n第二行")])
    assert text == "[助手] 第一行\n    第二行"


def test_工具调用与更新都会渲染状态() -> None:
    updates = [
        {
            "sessionUpdate": "tool_call",
            "toolCallId": "t1",
            "title": "读取文件",
            "status": "in_progress",
            "rawInput": {"path": "a.py"},
        },
        {"sessionUpdate": "tool_call_update", "toolCallId": "t1", "status": "completed", "rawOutput": "ok"},
    ]
    text = render_transcript(updates)
    assert "[工具] 读取文件  (in_progress)" in text
    assert '输入: {"path": "a.py"}' in text
    assert "[工具] 读取文件 → completed" in text
    assert '输出: "ok"' in text


def test_工具更新在没有标题时沿用首次标题() -> None:
    updates = [
        {"sessionUpdate": "tool_call", "toolCallId": "t9", "title": "编译", "status": "pending"},
        {"sessionUpdate": "tool_call_update", "toolCallId": "t9", "status": "failed"},
    ]
    text = render_transcript(updates)
    assert "[工具] 编译 → failed" in text


def test_计划条目按序编号() -> None:
    updates = [
        {
            "sessionUpdate": "plan",
            "entries": [
                {"content": "读取配置", "status": "completed"},
                {"content": "修改代码", "status": "pending"},
            ],
        }
    ]
    text = render_transcript(updates)
    assert text.splitlines() == ["[计划]", "    1. (completed) 读取配置", "    2. (pending) 修改代码"]


def test_元信息默认隐藏_打开后显示() -> None:
    updates = [
        {"sessionUpdate": "available_commands_update", "availableCommands": [{"name": "init"}, {"name": "review"}]},
        {"sessionUpdate": "current_mode_update", "currentModeId": "plan"},
    ]
    assert render_transcript(updates) == ""
    shown = render_transcript(updates, RenderOptions(show_meta=True))
    assert "[元信息] 可用命令: init, review" in shown
    assert "[元信息] 当前模式: plan" in shown


def test_未知事件显式标出_不静默丢弃() -> None:
    text = render_transcript([{"sessionUpdate": "品牌新事件", "value": 1}])
    assert text.startswith("[未知事件 品牌新事件]")


def test_缺少_sessionUpdate_的事件被明确指出() -> None:
    text = render_transcript([{"value": 1}])
    assert "缺少 sessionUpdate 字段" in text


def test_截断生效且标注原始长度() -> None:
    long_text = "甲" * 50
    text = render_transcript([_agent(long_text)], RenderOptions(truncate=10))
    assert text == f"[助手] {'甲' * 10}…（共 50 字符）"
    assert render_transcript([_agent(long_text)], RenderOptions(truncate=0)) == f"[助手] {long_text}"


def test_各种内容块的文字表示() -> None:
    assert content_block_text({"type": "text", "text": "hi"}, 100) == "hi"
    assert content_block_text({"type": "image", "mimeType": "image/png"}, 100) == "[图片 image/png]"
    assert content_block_text({"type": "resource_link", "uri": "file:///a", "name": "a"}, 100) == "[资源链接] a"


def test_只读客户端拒绝权限申请() -> None:
    client = ReadOnlyClient()
    response = asyncio.run(client.request_permission(session_id="s", tool_call={}, options=[]))
    assert response.outcome.outcome == "cancelled"
    assert client.refused_requests == ["session/request_permission"]


def test_预算内与不限预算时原样返回() -> None:
    assert apply_budget("abc", 10) == ("abc", 0)
    assert apply_budget("abc", 0) == ("abc", 0)
    assert apply_budget("", 5) == ("", 0)


def test_恰好等于上界时不截断() -> None:
    text = "甲" * 100
    assert apply_budget(text, 100) == (text, 0)


@pytest.mark.parametrize("budget", [1, 10, 45, 60, 100, 1000, 4999])
def test_截断后长度不超过上界且说明丢弃量(budget: int) -> None:
    text = "甲" * 5000
    bounded, dropped = apply_budget(text, budget)
    assert len(bounded) <= budget
    assert 0 < dropped <= len(text)
    if budget > 60:
        assert f"丢弃 {dropped} 字符" in bounded
