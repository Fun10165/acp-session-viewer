"""命令行入口：``serve`` 启动服务端，其余子命令作为客户端调用服务端。

客户端约定（便于 agent 调用）：

* 每条命令都有字符上界 ``--budget``，被丢弃的部分会在 stderr 说明；
* 失败时显式报错并以退出码区分：0 成功、1 失败、2 没找到会话；
* ``show`` 的选择器支持会话 ID、ID 前缀、标题子串与 ``last``。
"""

from __future__ import annotations

import argparse
import asyncio
import json
import sys
import unicodedata
from pathlib import Path
from typing import Any, Sequence

from . import __version__
from .client import ClientConfig, ClientError, request_json, request_text, resolve_client_config
from .config import (
    DEFAULT_BIND,
    DEFAULT_BUDGET,
    DEFAULT_LIST_LIMIT,
    DEFAULT_PORT,
    DEFAULT_SERVER_URL,
    AgentConfigError,
    default_token_path,
    load_agents,
    parse_bind,
    resolve_server_token,
)
from .render import apply_budget
from .server import run_server

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


def _format_table(headers: Sequence[str], rows: Sequence[Sequence[str]], gap: str = "  ") -> str:
    widths = [
        max(_display_width(header), *(_display_width(row[index]) for row in rows))
        for index, header in enumerate(headers)
    ]
    lines = [gap.join(_pad(header, widths[index]) for index, header in enumerate(headers))]
    for row in rows:
        lines.append(gap.join(_pad(cell, widths[index]) for index, cell in enumerate(row)))
    return "\n".join(lines)


def _print_bounded(text: str, budget: int) -> None:
    """按字符上界打印，被丢弃的字符数在 stderr 说明。"""
    bounded, dropped = apply_budget(text, budget)
    print(bounded)
    if dropped:
        print(f"（另有 {dropped} 个字符因 --budget 上界未显示）", file=sys.stderr)


async def _cmd_health(_args: argparse.Namespace, config: ClientConfig) -> int:
    payload = await request_json(config, "/health")
    print(json.dumps(payload, ensure_ascii=False))
    return EXIT_OK


async def _cmd_agents(args: argparse.Namespace, config: ClientConfig) -> int:
    payload = await request_json(config, "/agents")
    if args.json:
        print(json.dumps(payload, ensure_ascii=False, indent=2))
        return EXIT_OK
    rows: list[list[str]] = []
    for item in payload.get("agents") or []:
        capability = ("list" if item.get("canList") else "no-list") + " " + ("load" if item.get("canLoad") else "no-load")
        rows.append(
            [
                str(item.get("name", "")),
                str(item.get("implementation") or "未知"),
                capability,
                str(item.get("error") or item.get("description") or ""),
            ]
        )
    _print_bounded(_format_table(["agent", "实现", "能力", "说明"], rows), args.budget)
    return EXIT_OK


async def _cmd_list(args: argparse.Namespace, config: ClientConfig) -> int:
    params: list[tuple[str, str]] = [("limit", str(args.limit))]
    if args.query:
        params.append(("query", args.query))
    for name in args.agent or ():
        params.append(("agent", name))
    payload = await request_json(config, "/sessions", params)
    if args.json:
        print(json.dumps(payload, ensure_ascii=False, indent=2))
        return EXIT_OK

    sessions = payload.get("sessions") or []
    lines = [f"# 共 {payload.get('total', 0)} 条匹配，显示 {len(sessions)} 条"]
    if sessions:
        rows = [
            [
                str(item.get("agent", "")),
                str(item.get("sessionId", "")),
                str(item.get("updatedAt") or ""),
                _ellipsize(str(item.get("title") or "(无标题)"), 60),
                _ellipsize(str(item.get("cwd", "")), 40),
            ]
            for item in sessions
        ]
        lines.append(_format_table(["agent", "会话 ID", "更新时间", "标题", "工作目录"], rows))
    else:
        lines.append("（没有匹配的会话）")
    for error in payload.get("errors") or []:
        lines.append(f"# 跳过 {error.get('agent')}: {error.get('message')}")
    _print_bounded("\n".join(lines), args.budget)
    return EXIT_OK


async def _cmd_show(args: argparse.Namespace, config: ClientConfig) -> int:
    params: list[tuple[str, str]] = [("selector", args.selector)]
    for name in args.agent or ():
        params.append(("agent", name))
    if args.cwd:
        params.append(("cwd", args.cwd))

    if args.json:
        payload = await request_json(config, "/transcript", [*params, ("format", "json")])
        print(json.dumps(payload, ensure_ascii=False, indent=2))
        return EXIT_OK

    text = await request_text(
        config,
        "/transcript",
        [
            *params,
            ("format", "text"),
            ("no_thoughts", "1" if args.no_thoughts else "0"),
            ("meta", "1" if args.meta else "0"),
            ("truncate", str(args.truncate)),
            ("budget", str(args.budget)),
        ],
    )
    print(text)
    return EXIT_OK


async def _dispatch(args: argparse.Namespace, config: ClientConfig) -> int:
    handlers = {
        "health": _cmd_health,
        "agents": _cmd_agents,
        "list": _cmd_list,
        "show": _cmd_show,
    }
    handler = handlers.get(args.command)
    if handler is None:
        raise ClientError(f"未知子命令: {args.command}")
    return await handler(args, config)


def _run_serve(args: argparse.Namespace) -> int:
    try:
        agents = load_agents(Path(args.agents) if args.agents else None)
        host, port = parse_bind(args.bind, default_port=DEFAULT_PORT)
        if args.port is not None:
            if not 1 <= args.port <= 65535:
                raise AgentConfigError(f"端口超出 1-65535: {args.port}")
            port = args.port
        token, source = resolve_server_token(args.token, args.token_file or default_token_path())
    except AgentConfigError as exc:
        print(f"配置错误: {exc}", file=sys.stderr)
        return EXIT_ERROR

    print(f"acp-view 服务端启动: http://{host}:{port}")
    print(f"令牌来源: {source}")
    print(f"可用 agent: {', '.join(spec.name for spec in agents)}")
    if host not in ("127.0.0.1", "localhost", "::1"):
        print("警告: 绑定在非回环地址，凡是能访问该端口并持有令牌的人都可以读取全部会话内容")
    try:
        run_server(host=host, port=port, token=token, agents=agents, timeout=args.timeout)
    except KeyboardInterrupt:
        return 130
    return EXIT_OK


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        prog="acp-view",
        description="用 ACP 协议只读查看各家 agent 的会话（服务端 + 客户端）",
    )
    parser.add_argument("--version", action="version", version=f"acp-session-viewer {__version__}")
    subparsers = parser.add_subparsers(dest="command", required=True)

    common = argparse.ArgumentParser(add_help=False)
    common.add_argument("--server", help=f"服务端地址（默认 {DEFAULT_SERVER_URL}，也可用 ACP_VIEW_SERVER）")
    common.add_argument("--token", help="访问令牌（也可用 ACP_VIEW_TOKEN 或令牌文件）")
    common.add_argument("--token-file", type=Path, help="令牌文件位置")

    serve = subparsers.add_parser("serve", help="启动查询服务")
    serve.add_argument("--bind", default=DEFAULT_BIND, help=f"绑定地址，HOST 或 HOST:PORT（默认 {DEFAULT_BIND}）")
    serve.add_argument("--port", type=int, help="覆盖绑定地址中的端口")
    serve.add_argument("--token", help="访问令牌；未提供时依次查找环境变量与令牌文件，都没有就新生成并写入")
    serve.add_argument("--token-file", type=Path, help="令牌文件位置")
    serve.add_argument("--agents", metavar="FILE", help="agent 配置文件（TOML）")
    serve.add_argument("--timeout", type=float, default=180.0, help="每个 agent 操作的超时秒数")

    health = subparsers.add_parser("health", parents=[common], help="检查服务端是否可用")  # noqa: F841

    agents = subparsers.add_parser("agents", parents=[common], help="查看各 agent 的 ACP 能力")
    agents.add_argument("--json", action="store_true", help="输出 JSON")
    agents.add_argument("--budget", type=int, default=DEFAULT_BUDGET, help=f"输出字符上界（默认 {DEFAULT_BUDGET}，0 表示不限）")

    listing = subparsers.add_parser("list", parents=[common], help="列出会话")
    listing.add_argument("--agent", action="append", help="只查指定 agent，可重复")
    listing.add_argument("--query", help="按关键词过滤（匹配标题、会话 ID、工作目录、agent 名）")
    listing.add_argument("--limit", type=int, default=DEFAULT_LIST_LIMIT, help=f"最多显示多少条（默认 {DEFAULT_LIST_LIMIT}）")
    listing.add_argument("--json", action="store_true", help="输出 JSON")
    listing.add_argument("--budget", type=int, default=DEFAULT_BUDGET, help=f"输出字符上界（默认 {DEFAULT_BUDGET}，0 表示不限）")

    show = subparsers.add_parser("show", parents=[common], help="重放一条会话")
    show.add_argument("selector", help="会话 ID、ID 前缀、标题子串，或 last")
    show.add_argument("--agent", action="append", help="限定 agent，可重复")
    show.add_argument("--cwd", help="按该工作目录直接加载（需配合唯一的 --agent，跳过枚举）")
    show.add_argument("--json", action="store_true", help="输出原始事件 JSON（不受 --budget 限制）")
    show.add_argument("--no-thoughts", action="store_true", help="不显示思考片段")
    show.add_argument("--meta", action="store_true", help="显示元信息事件")
    show.add_argument("--truncate", type=int, default=400, help="单段内容截断长度，0 表示不截断")
    show.add_argument("--budget", type=int, default=DEFAULT_BUDGET, help=f"输出字符上界（默认 {DEFAULT_BUDGET}，0 表示不限）")
    return parser


def main(argv: Sequence[str] | None = None) -> int:
    parser = build_parser()
    args = parser.parse_args(argv)

    if args.command == "serve":
        return _run_serve(args)

    try:
        config = resolve_client_config(args.server, args.token, args.token_file)
    except AgentConfigError as exc:
        print(f"配置错误: {exc}", file=sys.stderr)
        return EXIT_ERROR

    try:
        return asyncio.run(_dispatch(args, config))
    except ClientError as exc:
        print(f"调用失败: {exc}", file=sys.stderr)
        return EXIT_NOT_FOUND if exc.not_found else EXIT_ERROR
    except KeyboardInterrupt:
        return 130


if __name__ == "__main__":
    raise SystemExit(main())
