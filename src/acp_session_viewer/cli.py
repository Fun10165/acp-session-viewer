"""命令行入口：agents / list / show 三个子命令。"""

from __future__ import annotations

import argparse
import asyncio
import json
import sys
import unicodedata
from pathlib import Path
from typing import Any, Sequence

from . import __version__
from .config import AgentConfigError, AgentSpec, load_agents
from .render import RenderOptions, render_transcript
from .viewer import ProbeResult, ViewerError, load_transcript, list_sessions, probe, select_agents

EXIT_OK = 0
EXIT_ERROR = 1
EXIT_NOT_FOUND = 2


def _display_width(text: str) -> int:
    return sum(2 if unicodedata.east_asian_width(char) in "WF" else 1 for char in text)


def _pad(text: str, width: int) -> str:
    return text + " " * max(0, width - _display_width(text))


def _ellipsize(text: str, limit: int) -> str:
    """把单元格压成单行并按显示宽度截断，避免超长标题撑爆表格。"""
    single_line = " ".join(text.split())
    if _display_width(single_line) <= limit:
        return single_line
    kept: list[str] = []
    width = 0
    for char in single_line:
        char_width = 2 if unicodedata.east_asian_width(char) in "WF" else 1
        if width + char_width > limit - 1:
            break
        kept.append(char)
        width += char_width
    return "".join(kept) + "…"


def _print_table(headers: Sequence[str], rows: Sequence[Sequence[str]], gap: str = "  ") -> None:
    widths = [max(_display_width(header), *(_display_width(row[index]) for row in rows)) for index, header in enumerate(headers)]
    print(gap.join(_pad(header, widths[index]) for index, header in enumerate(headers)))
    for row in rows:
        print(gap.join(_pad(cell, widths[index]) for index, cell in enumerate(row)))


def _capability_text(result: ProbeResult) -> str:
    marks: list[str] = []
    marks.append("list" if result.can_list else "no-list")
    marks.append("load" if result.can_load else "no-load")
    session_caps = sorted((result.capabilities.get("sessionCapabilities") or {}).keys())
    if session_caps:
        marks.append("[" + ",".join(session_caps) + "]")
    return " ".join(marks)


async def _probe_all(agents: Sequence[AgentSpec], timeout: float) -> list[tuple[AgentSpec, ProbeResult | str]]:
    outcomes: list[tuple[AgentSpec, ProbeResult | str]] = []
    for spec in agents:
        try:
            outcomes.append((spec, await probe(spec, timeout=timeout)))
        except ViewerError as exc:
            outcomes.append((spec, str(exc)))
    return outcomes


async def _cmd_agents(agents: Sequence[AgentSpec], args: argparse.Namespace) -> int:
    outcomes = await _probe_all(agents, args.timeout)
    rows: list[list[str]] = []
    for spec, outcome in outcomes:
        if isinstance(outcome, str):
            rows.append([spec.name, "启动失败", outcome, ""])
        else:
            info = outcome.agent_info.get("title") or outcome.agent_info.get("name") or "未知"
            rows.append([spec.name, info, _capability_text(outcome), spec.description])
    _print_table(["agent", "实现", "能力", "说明"], rows)
    return EXIT_OK


async def _cmd_list(agents: Sequence[AgentSpec], args: argparse.Namespace) -> int:
    selected = select_agents(agents, args.agent)
    payload: list[dict[str, Any]] = []
    rows: list[list[str]] = []
    failures: list[str] = []
    for spec in selected:
        try:
            sessions, result = await list_sessions(spec, timeout=args.timeout)
        except ViewerError as exc:
            failures.append(f"{spec.name}: {exc}")
            continue
        if not result.can_list:
            failures.append(f"{spec.name}: 不支持会话枚举（缺少 sessionCapabilities.list）")
            continue
        sessions.sort(key=lambda item: item.updated_at or "", reverse=True)
        for item in sessions[: args.limit]:
            rows.append(
                [
                    item.agent,
                    item.session_id,
                    item.updated_at or "",
                    _ellipsize(item.title or "(无标题)", 60),
                    _ellipsize(item.cwd, 40),
                ]
            )
        payload.append(
            {
                "agent": spec.name,
                "count": len(sessions),
                "shown": min(len(sessions), args.limit),
                "sessions": [item.__dict__ for item in sessions],
            }
        )
    if args.json:
        print(json.dumps(payload, ensure_ascii=False, indent=2))
    elif rows:
        _print_table(["agent", "会话 ID", "更新时间", "标题", "工作目录"], rows)
    else:
        print("没有可显示的会话")
    for message in failures:
        print(f"跳过 {message}", file=sys.stderr)
    return EXIT_OK if payload else EXIT_NOT_FOUND


async def _cmd_show(agents: Sequence[AgentSpec], args: argparse.Namespace) -> int:
    selected = select_agents(agents, args.agent)
    matched: tuple[AgentSpec, str] | None = None
    notes: list[str] = []
    for spec in selected:
        if args.cwd:
            matched = (spec, args.cwd)
            break
        try:
            sessions, result = await list_sessions(spec, timeout=args.timeout)
        except ViewerError as exc:
            notes.append(f"{spec.name}: {exc}")
            continue
        if not result.can_list:
            notes.append(f"{spec.name}: 不支持会话枚举，需用 --cwd 指定工作目录")
            continue
        for item in sessions:
            if item.session_id == args.session_id:
                matched = (spec, item.cwd)
                break
        if matched:
            break
    for note in notes:
        print(f"跳过 {note}", file=sys.stderr)
    if matched is None:
        print(f"没有找到会话 {args.session_id}；可以用 --agent 与 --cwd 直接指定", file=sys.stderr)
        return EXIT_NOT_FOUND

    spec, cwd = matched
    if not Path(cwd).is_absolute():
        print(f"会话记录中的工作目录不是绝对路径: {cwd!r}", file=sys.stderr)
        return EXIT_ERROR
    try:
        result, _probe_result = await load_transcript(spec, args.session_id, cwd, timeout=args.timeout)
    except ViewerError as exc:
        print(f"加载失败: {exc}", file=sys.stderr)
        return EXIT_ERROR

    if args.json:
        print(
            json.dumps(
                {
                    "agent": result.agent,
                    "sessionId": result.session_id,
                    "cwd": cwd,
                    "loadResponse": result.response,
                    "updates": result.updates,
                },
                ensure_ascii=False,
                indent=2,
            )
        )
    else:
        options = RenderOptions(show_thoughts=not args.no_thoughts, show_meta=args.meta, truncate=args.truncate)
        text = render_transcript(result.updates, options)
        print(f"# {spec.name} / {result.session_id} / {cwd}（{len(result.updates)} 条事件）")
        print(text if text else "（该会话没有可渲染的事件）")
    for refusal in result.refused_requests:
        print(f"已拒绝 agent 的反向请求: {refusal}", file=sys.stderr)
    return EXIT_OK


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(prog="acp-view", description="用 ACP 协议只读查看各家 agent 的会话")
    parser.add_argument("--version", action="version", version=f"acp-session-viewer {__version__}")
    parser.add_argument("--agents", metavar="FILE", help="agent 配置文件（TOML），缺省使用内置列表")
    subparsers = parser.add_subparsers(dest="command", required=True)

    agents_parser = subparsers.add_parser("agents", help="探测每个 agent 的 ACP 能力")
    agents_parser.add_argument("--timeout", type=float, default=60.0, help="每个 agent 的超时秒数")

    list_parser = subparsers.add_parser("list", help="枚举会话")
    list_parser.add_argument("--agent", action="append", help="只查指定 agent，可重复")
    list_parser.add_argument("--limit", type=int, default=30, help="每个 agent 最多显示多少条")
    list_parser.add_argument("--json", action="store_true", help="输出 JSON")
    list_parser.add_argument("--timeout", type=float, default=60.0)

    show_parser = subparsers.add_parser("show", help="重放一条会话")
    show_parser.add_argument("session_id", help="会话 ID")
    show_parser.add_argument("--agent", action="append", help="限定 agent，可重复")
    show_parser.add_argument("--cwd", help="直接指定会话的工作目录，跳过枚举")
    show_parser.add_argument("--json", action="store_true", help="输出原始事件 JSON")
    show_parser.add_argument("--no-thoughts", action="store_true", help="不显示思考片段")
    show_parser.add_argument("--meta", action="store_true", help="显示元信息事件")
    show_parser.add_argument("--truncate", type=int, default=400, help="单段内容截断长度，0 表示不截断")
    show_parser.add_argument("--timeout", type=float, default=120.0)
    return parser


async def _dispatch(args: argparse.Namespace, agents: Sequence[AgentSpec]) -> int:
    if args.command == "agents":
        return await _cmd_agents(agents, args)
    if args.command == "list":
        return await _cmd_list(agents, args)
    if args.command == "show":
        return await _cmd_show(agents, args)
    raise ViewerError(f"未知子命令: {args.command}")


def main(argv: Sequence[str] | None = None) -> int:
    parser = build_parser()
    args = parser.parse_args(argv)
    try:
        agents = load_agents(Path(args.agents) if args.agents else None)
    except (AgentConfigError, OSError) as exc:
        print(f"配置错误: {exc}", file=sys.stderr)
        return EXIT_ERROR
    try:
        return asyncio.run(_dispatch(args, agents))
    except (ViewerError, AgentConfigError) as exc:
        print(f"执行失败: {exc}", file=sys.stderr)
        return EXIT_ERROR
    except KeyboardInterrupt:
        return 130


if __name__ == "__main__":
    raise SystemExit(main())
