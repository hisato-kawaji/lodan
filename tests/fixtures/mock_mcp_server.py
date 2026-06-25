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

NOTES_RESOURCE = {
    "uri": "mem://notes",
    "name": "notes",
    "description": "Scratch notes.",
    "mimeType": "text/plain",
}

GET_ROOTS_TOOL = {
    "name": "get_roots",
    "description": "Return the roots reported by the client (server→client roots/list).",
    "inputSchema": {"type": "object", "properties": {}},
}

# Captured from the client's response to our server-initiated roots/list request.
CAPTURED_ROOTS: list = []


def send(obj: dict[str, Any]) -> None:
    sys.stdout.write(json.dumps(obj, separators=(",", ":")) + "\n")
    sys.stdout.flush()


def handle(msg: dict[str, Any]) -> None:
    method = msg.get("method")
    msg_id = msg.get("id")

    # Response to our server→client roots/list request (no method, has result).
    if method is None and isinstance(msg.get("result"), dict):
        roots = msg["result"].get("roots")
        if isinstance(roots, list):
            CAPTURED_ROOTS.clear()
            CAPTURED_ROOTS.extend(roots)
        return

    if method == "initialize":
        send(
            {
                "jsonrpc": "2.0",
                "id": msg_id,
                "result": {
                    "protocolVersion": PROTOCOL_VERSION,
                    "capabilities": {"tools": {}, "prompts": {}, "resources": {}},
                    "serverInfo": {"name": "mock-mcp", "version": "0.0.1"},
                },
            }
        )
        # Exercise the server→client direction: ask the client for its roots.
        send({"jsonrpc": "2.0", "id": 9001, "method": "roots/list"})
        return

    if method == "notifications/initialized":
        return  # notification — no response

    if method == "tools/list":
        send(
            {
                "jsonrpc": "2.0",
                "id": msg_id,
                "result": {"tools": [ECHO_TOOL, GET_ROOTS_TOOL]},
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
        elif name == "get_roots":
            text = json.dumps(CAPTURED_ROOTS, separators=(",", ":"))
            send(
                {
                    "jsonrpc": "2.0",
                    "id": msg_id,
                    "result": {
                        "content": [{"type": "text", "text": text}],
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

    if method == "resources/list":
        send(
            {
                "jsonrpc": "2.0",
                "id": msg_id,
                "result": {"resources": [NOTES_RESOURCE]},
            }
        )
        return

    if method == "resources/read":
        params = msg.get("params") or {}
        uri = params.get("uri")
        if uri == "mem://notes":
            send(
                {
                    "jsonrpc": "2.0",
                    "id": msg_id,
                    "result": {
                        "contents": [
                            {
                                "uri": uri,
                                "mimeType": "text/plain",
                                "text": "remember the milk",
                            }
                        ]
                    },
                }
            )
        else:
            send(
                {
                    "jsonrpc": "2.0",
                    "id": msg_id,
                    "error": {"code": -32602, "message": f"unknown resource: {uri}"},
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
