#!/usr/bin/env python3
"""最小 stdio MCP server，仅用于离线验证 rmcp 客户端。

实现 initialize / tools/list / tools/call / ping，其余请求回 -32601。
消息为换行分隔的 JSON-RPC 2.0。
"""

import json
import sys

TOOLS = [
    {
        "name": "echo",
        "description": "回显传入的 text。",
        "inputSchema": {
            "type": "object",
            "properties": {"text": {"type": "string"}},
            "required": ["text"],
        },
    },
    {
        "name": "add",
        "description": "返回两个整数之和。",
        "inputSchema": {
            "type": "object",
            "properties": {"a": {"type": "integer"}, "b": {"type": "integer"}},
            "required": ["a", "b"],
        },
    },
    {
        "name": "fail",
        "description": "总是返回工具级错误（isError=true）。",
        "inputSchema": {"type": "object", "properties": {}},
    },
]


def send(message):
    sys.stdout.write(json.dumps(message, ensure_ascii=False) + "\n")
    sys.stdout.flush()


def result(req_id, payload):
    send({"jsonrpc": "2.0", "id": req_id, "result": payload})


def error(req_id, code, message):
    send({"jsonrpc": "2.0", "id": req_id, "error": {"code": code, "message": message}})


def main():
    for line in sys.stdin:
        line = line.strip()
        if not line:
            continue
        try:
            message = json.loads(line)
        except json.JSONDecodeError:
            continue

        method = message.get("method")
        req_id = message.get("id")
        params = message.get("params") or {}

        # 通知没有 id，不需要响应。
        if req_id is None:
            continue

        if method == "initialize":
            result(
                req_id,
                {
                    "protocolVersion": params.get("protocolVersion", "2025-06-18"),
                    "capabilities": {"tools": {"listChanged": False}},
                    "serverInfo": {"name": "fake-mcp", "version": "0.0.1"},
                },
            )
        elif method == "tools/list":
            result(req_id, {"tools": TOOLS})
        elif method == "tools/call":
            name = params.get("name")
            arguments = params.get("arguments") or {}
            if name == "echo":
                text = str(arguments.get("text", ""))
                result(
                    req_id,
                    {
                        "content": [{"type": "text", "text": f"echo: {text}"}],
                        "isError": False,
                    },
                )
            elif name == "add":
                total = int(arguments.get("a", 0)) + int(arguments.get("b", 0))
                result(
                    req_id,
                    {
                        "content": [{"type": "text", "text": str(total)}],
                        "isError": False,
                    },
                )
            elif name == "fail":
                result(
                    req_id,
                    {
                        "content": [{"type": "text", "text": "boom"}],
                        "isError": True,
                    },
                )
            else:
                result(
                    req_id,
                    {
                        "content": [{"type": "text", "text": f"unknown tool: {name}"}],
                        "isError": True,
                    },
                )
        elif method == "ping":
            result(req_id, {})
        else:
            error(req_id, -32601, f"method not found: {method}")


if __name__ == "__main__":
    main()
