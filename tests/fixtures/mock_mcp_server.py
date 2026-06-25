#!/usr/bin/env python3
"""Minimal stdio MCP server for lodan integration testing.

Implements just enough of the protocol for one round-trip:
  initialize → notifications/initialized → tools/list → tools/call(echo).

Wire: newline-delimited JSON-RPC 2.0 on stdin/stdout.
"""
from __future__ import annotations

import json
import sys
from typing import Any

PROTOCOL_VERSION = "2025-06-18"

ECHO_TOOL = {
    "name": "echo",
    "description": "Return the input arguments as text.",
    "inputSchema": {
        "type": "object",
        "properties": {"msg": {"type": "string"}},
    },
}

GREET_PROMPT = {
    "name": "greet",
    "description": "Greet someone by name.",
    "arguments": [
        {"name": "who", "description": "the person", "required": True},
    ],
}


def send(obj: dict[str, Any]) -> None:
    sys.stdout.write(json.dumps(obj, separators=(",", ":")) + "\n")
    sys.stdout.flush()


def handle(msg: dict[str, Any]) -> None:
    method = msg.get("method")
    msg_id = msg.get("id")

    if method == "initialize":
        send(
            {
                "jsonrpc": "2.0",
                "id": msg_id,
                "result": {
                    "protocolVersion": PROTOCOL_VERSION,
                    "capabilities": {"tools": {}, "prompts": {}},
                    "serverInfo": {"name": "mock-mcp", "version": "0.0.1"},
                },
            }
        )
        return

    if method == "notifications/initialized":
        return  # notification — no response

    if method == "tools/list":
        send(
            {
                "jsonrpc": "2.0",
                "id": msg_id,
                "result": {"tools": [ECHO_TOOL]},
            }
        )
        return

    if method == "tools/call":
        params = msg.get("params") or {}
        name = params.get("name")
        args = params.get("arguments") or {}
        if name == "echo":
            text = json.dumps(args, separators=(",", ":"))
            send(
                {
                    "jsonrpc": "2.0",
                    "id": msg_id,
                    "result": {
                        "content": [{"type": "text", "text": f"echo:{text}"}],
                        "isError": False,
                    },
                }
            )
        else:
            send(
                {
                    "jsonrpc": "2.0",
                    "id": msg_id,
                    "error": {"code": -32601, "message": f"unknown tool: {name}"},
                }
            )
        return

    if method == "prompts/list":
        send(
            {
                "jsonrpc": "2.0",
                "id": msg_id,
                "result": {"prompts": [GREET_PROMPT]},
            }
        )
        return

    if method == "prompts/get":
        params = msg.get("params") or {}
        name = params.get("name")
        args = params.get("arguments") or {}
        if name == "greet":
            who = args.get("who", "world")
            send(
                {
                    "jsonrpc": "2.0",
                    "id": msg_id,
                    "result": {
                        "description": "greeting",
                        "messages": [
                            {
                                "role": "user",
                                "content": {
                                    "type": "text",
                                    "text": f"Say hello to {who}.",
                                },
                            }
                        ],
                    },
                }
            )
        else:
            send(
                {
                    "jsonrpc": "2.0",
                    "id": msg_id,
                    "error": {"code": -32601, "message": f"unknown prompt: {name}"},
                }
            )
        return

    # Unknown method — respond with JSON-RPC method-not-found if it has an id.
    if msg_id is not None:
        send(
            {
                "jsonrpc": "2.0",
                "id": msg_id,
                "error": {"code": -32601, "message": f"method not found: {method}"},
            }
        )


def main() -> None:
    for line in sys.stdin:
        line = line.strip()
        if not line:
            continue
        try:
            msg = json.loads(line)
        except json.JSONDecodeError as e:
            print(f"mock_mcp_server: bad JSON: {e}", file=sys.stderr)
            continue
        try:
            handle(msg)
        except Exception as e:
            print(f"mock_mcp_server: handler error: {e}", file=sys.stderr)


if __name__ == "__main__":
    main()
