#!/usr/bin/env python3
"""测试用的最小 ACP agent：原生 JSON-RPC over stdio。

支持 ``initialize``、``session/list``、``session/load``，通过环境变量控制行为：

* ``STUB_AGENT_START_LOG``：每次启动把进程号追加写入该文件，用来核对连接是否被复用；
* ``STUB_AGENT_DIE_ON_SESSION``：加载该会话时直接退出，用来触发连接失效的处理路径。
"""

from __future__ import annotations

import json
import os
import sys

START_LOG = os.environ.get("STUB_AGENT_START_LOG")
DIE_ON_SESSION = os.environ.get("STUB_AGENT_DIE_ON_SESSION")


def _sessions() -> list[dict[str, str]]:
    cwd = os.getcwd()
    return [
        {"sessionId": "stub-1", "cwd": cwd, "title": "桩会话一", "updatedAt": "2026-09-27T00:00:00Z"},
        {"sessionId": "stub-2", "cwd": cwd, "title": "桩会话二", "updatedAt": "2026-09-26T00:00:00Z"},
    ]


def _send(payload: dict) -> None:
    sys.stdout.write(json.dumps(payload, ensure_ascii=False) + "\n")
    sys.stdout.flush()


def _handle(message: dict) -> None:
    method = message.get("method")
    request_id = message.get("id")
    if method == "initialize":
        _send(
            {
                "jsonrpc": "2.0",
                "id": request_id,
                "result": {
                    "protocolVersion": 1,
                    "agentInfo": {"name": "stub-agent", "title": "桩 agent", "version": "0.0.1"},
                    "agentCapabilities": {"loadSession": True, "sessionCapabilities": {"list": {}}},
                    "authMethods": [],
                },
            }
        )
    elif method == "session/list":
        _send({"jsonrpc": "2.0", "id": request_id, "result": {"sessions": _sessions()}})
    elif method == "session/load":
        session_id = message["params"]["sessionId"]
        if DIE_ON_SESSION and session_id == DIE_ON_SESSION:
            os._exit(3)
        for text in (f"{session_id} 的第一条", f"{session_id} 的第二条"):
            _send(
                {
                    "jsonrpc": "2.0",
                    "method": "session/update",
                    "params": {
                        "sessionId": session_id,
                        "update": {
                            "sessionUpdate": "agent_message_chunk",
                            "messageId": "m-1",
                            "content": {"type": "text", "text": text},
                        },
                    },
                }
            )
        _send({"jsonrpc": "2.0", "id": request_id, "result": {}})
    else:
        _send({"jsonrpc": "2.0", "id": request_id, "error": {"code": -32601, "message": f"未实现 {method}"}})


def main() -> int:
    if START_LOG:
        with open(START_LOG, "a", encoding="utf-8") as handle:
            handle.write(f"{os.getpid()}\n")
    for line in sys.stdin:
        line = line.strip()
        if not line:
            continue
        _handle(json.loads(line))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
