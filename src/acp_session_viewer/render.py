"""把 ACP 的 session/update 事件渲染成人类可读文本。

全部为纯函数：输入事件字典序列，输出文本，不产生副作用。
"""

from __future__ import annotations

import json
from dataclasses import dataclass
from typing import Any, Sequence

_CHUNK_LABELS = {
    "user_message_chunk": "用户",
    "agent_message_chunk": "助手",
    "agent_thought_chunk": "思考",
}


@dataclass(frozen=True)
class RenderOptions:
    """渲染选项。"""

    show_thoughts: bool = True
    show_meta: bool = False
    truncate: int = 400


def apply_budget(text: str, budget: int) -> tuple[str, int]:
    """把文本压到 budget 个字符以内，返回（结果, 丢弃字符数）。budget 为 0 或负数表示不限制。

    截断提示行本身也计入上界，因此结果的长度按构造不超过 budget。
    """
    if budget <= 0 or len(text) <= budget:
        return text, 0
    template = "\n…（已截断，丢弃 {dropped} 字符；调大 --budget 或改用 --json 查看完整内容）"
    reserved = len(template.format(dropped=len(text)))
    if budget <= reserved:
        marker = "…（预算过小，无法显示内容）"
        return marker[:budget], len(text)
    keep = budget - reserved
    dropped = len(text) - keep
    return text[:keep] + template.format(dropped=dropped), dropped


def _shorten(text: str, limit: int) -> str:
    if limit <= 0 or len(text) <= limit:
        return text
    return f"{text[:limit]}…（共 {len(text)} 字符）"


def _compact_json(value: Any, limit: int) -> str:
    return _shorten(json.dumps(value, ensure_ascii=False, sort_keys=True), limit)


def content_block_text(block: Any, limit: int) -> str:
    """把单个 ContentBlock 转成一行文本。"""
    if isinstance(block, str):
        return block
    if not isinstance(block, dict):
        return f"[无法识别的内容块] {block!r}"
    kind = block.get("type")
    if kind == "text":
        return str(block.get("text", ""))
    if kind == "image":
        return f"[图片 {block.get('mimeType', '未知类型')}]"
    if kind == "audio":
        return f"[音频 {block.get('mimeType', '未知类型')}]"
    if kind == "resource_link":
        return f"[资源链接] {block.get('name') or block.get('uri')}"
    if kind == "resource":
        inner = block.get("resource") or {}
        return f"[内嵌资源] {inner.get('uri', '未知位置')}"
    if kind is None:
        return f"[缺少 type 的内容块] {_compact_json(block, limit)}"
    return f"[{kind} 内容]"


def _block_line(label: str, body: str) -> str:
    """把一段正文渲染成带标签的文本，多行时后续行缩进对齐。"""
    stripped = body.strip()
    if not stripped:
        return ""
    lines = stripped.splitlines()
    if len(lines) == 1:
        return f"[{label}] {lines[0]}"
    indented = "\n".join(f"    {line}" for line in lines[1:])
    return f"[{label}] {lines[0]}\n{indented}"


def _plan_lines(entries: Any, limit: int) -> list[str]:
    if not isinstance(entries, list):
        return [f"[计划] {_compact_json(entries, limit)}"]
    lines = ["[计划]"]
    for index, entry in enumerate(entries, start=1):
        if isinstance(entry, dict):
            status = entry.get("status", "未知状态")
            content = entry.get("content", "")
            lines.append(f"    {index}. ({status}) {content}")
        else:
            lines.append(f"    {index}. {entry}")
    return lines


def _tool_call_lines(titles: dict[str, str], update: dict[str, Any], limit: int) -> list[str]:
    kind = update.get("sessionUpdate")
    tool_call_id = str(update.get("toolCallId", "未知"))
    title = update.get("title") or titles.get(tool_call_id) or tool_call_id
    titles[tool_call_id] = str(title)
    status = update.get("status")
    lines: list[str] = []
    if kind == "tool_call":
        lines.append(f"[工具] {title}" + (f"  ({status})" if status else ""))
    else:
        parts = [f"[工具] {title}"]
        if status:
            parts.append(f"→ {status}")
        lines.append(" ".join(parts))
    if update.get("rawInput") is not None:
        lines.append(f"    输入: {_compact_json(update['rawInput'], limit)}")
    if update.get("rawOutput") is not None:
        lines.append(f"    输出: {_compact_json(update['rawOutput'], limit)}")
    for block in update.get("content") or []:
        if isinstance(block, dict) and block.get("type") == "content":
            inner = block.get("content")
            if isinstance(inner, dict):
                lines.append(f"    内容: {_shorten(content_block_text(inner, limit), limit)}")
            else:
                lines.append(f"    内容: {_compact_json(inner, limit)}")
        else:
            lines.append(f"    内容: {_compact_json(block, limit)}")
    return lines


def render_transcript(updates: Sequence[dict[str, Any]], options: RenderOptions = RenderOptions()) -> str:
    """把事件序列渲染成文本。"""
    lines: list[str] = []
    titles: dict[str, str] = {}
    pending_kind: str | None = None
    pending_message: str | None = None
    pending_parts: list[str] = []

    def flush_chunks() -> None:
        nonlocal pending_kind, pending_message, pending_parts
        if pending_kind is not None:
            # 先合并同一消息的所有片段再截断，避免把流式消息按片段切断。
            body = "".join(pending_parts).strip()
            if body:
                lines.append(_block_line(_CHUNK_LABELS[pending_kind], _shorten(body, options.truncate)))
        pending_kind, pending_message, pending_parts = None, None, []

    for update in updates:
        if not isinstance(update, dict):
            flush_chunks()
            lines.append(f"[无法识别的事件] {update!r}")
            continue
        kind = update.get("sessionUpdate")
        if kind in _CHUNK_LABELS:
            if kind == "agent_thought_chunk" and not options.show_thoughts:
                continue
            message_id = update.get("messageId")
            if pending_kind != kind or pending_message != message_id:
                flush_chunks()
                pending_kind, pending_message = kind, message_id
            pending_parts.append(content_block_text(update.get("content"), options.truncate))
            continue

        flush_chunks()
        if kind in ("tool_call", "tool_call_update"):
            lines.extend(_tool_call_lines(titles, update, options.truncate))
        elif kind == "plan":
            lines.extend(_plan_lines(update.get("entries"), options.truncate))
        elif kind == "available_commands_update":
            if options.show_meta:
                commands = [item.get("name", "?") for item in update.get("availableCommands") or []]
                lines.append(f"[元信息] 可用命令: {', '.join(commands)}")
        elif kind == "current_mode_update":
            if options.show_meta:
                lines.append(f"[元信息] 当前模式: {update.get('currentModeId')}")
        elif kind == "config_option_update":
            if options.show_meta:
                lines.append(f"[元信息] 配置项: {_compact_json(update.get('configOptions'), options.truncate)}")
        elif kind == "session_info_update":
            if options.show_meta:
                lines.append(f"[元信息] 会话信息: {_compact_json(update, options.truncate)}")
        elif kind == "usage_update":
            if options.show_meta:
                lines.append(f"[元信息] 用量: {_compact_json(update, options.truncate)}")
        elif kind is None:
            lines.append(f"[缺少 sessionUpdate 字段的事件] {_compact_json(update, options.truncate)}")
        else:
            lines.append(f"[未知事件 {kind}] {_compact_json(update, options.truncate)}")

    flush_chunks()
    return "\n".join(lines)
