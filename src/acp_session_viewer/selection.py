"""会话选择器的匹配规则（纯函数）。

支持三种写法，按此顺序尝试：

1. 会话 ID 完整匹配
2. 会话 ID 前缀匹配
3. 标题、会话 ID、工作目录、agent 名称的子串匹配（大小写不敏感）
4. 特殊值 ``last``：取更新时间最新的一条

命中多条时按更新时间倒序返回，调用方取第一条并说明命中数量。
"""

from __future__ import annotations

from typing import Sequence

from .viewer import SessionSummary


class SelectionError(RuntimeError):
    """选择器没有命中任何会话。"""


def _newest_first(sessions: Sequence[SessionSummary]) -> list[SessionSummary]:
    return sorted(sessions, key=lambda item: item.updated_at or "", reverse=True)


def match_sessions(sessions: Sequence[SessionSummary], selector: str) -> list[SessionSummary]:
    """返回命中的会话，按更新时间从新到旧排序。"""
    if not selector:
        raise SelectionError("选择器不能为空")
    if selector == "last":
        return _newest_first(sessions)[:1]

    exact = [item for item in sessions if item.session_id == selector]
    if exact:
        return exact

    lowered = selector.lower()
    prefixed = [item for item in sessions if item.session_id.lower().startswith(lowered)]
    if prefixed:
        return _newest_first(prefixed)

    return _newest_first(
        [
            item
            for item in sessions
            if lowered in (item.title or "").lower()
            or lowered in item.session_id.lower()
            or lowered in item.cwd.lower()
            or lowered in item.agent.lower()
        ]
    )
