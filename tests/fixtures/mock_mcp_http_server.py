#!/usr/bin/env python3
"""Minimal Streamable HTTP MCP server for lodan integration testing.

Implements just enough for: initialize → notifications/initialized →
tools/list → tools/call(echo), over a single HTTP endpoint.

- Client POSTs newline-free JSON-RPC; we reply with application/json.
- initialize response carries an `Mcp-Session-Id` header; later requests echo it.
- Notifications (no `id`) get 202 Accepted with no body.

Usage: mock_mcp_http_server.py <port>
"""
from __future__ import annotations

import json
import sys
from http.server import BaseHTTPRequestHandler, HTTPServer

PROTOCOL_VERSION = "2025-06-18"
SESSION_ID = "test-session-123"

ECHO_TOOL = {
    "name": "echo",
    "description": "Return the input arguments as text.",
    "inputSchema": {"type": "object", "properties": {"msg": {"type": "string"}}},
}


def result_for(msg: dict) -> dict | None:
    """Return the JSON-RPC result object, or None for notifications."""
    method = msg.get("method")
    if method == "initialize":
        return {
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": {"tools": {}},
            "serverInfo": {"name": "mock-http-mcp", "version": "0.0.1"},
        }
    if method == "tools/list":
        return {"tools": [ECHO_TOOL]}
    if method == "tools/call":
        params = msg.get("params") or {}
        if params.get("name") == "echo":
            text = json.dumps(params.get("arguments") or {}, separators=(",", ":"))
            return {"content": [{"type": "text", "text": f"echo:{text}"}], "isError": False}
    return {"error": {"code": -32601, "message": f"method not found: {method}"}}


class Handler(BaseHTTPRequestHandler):
    def log_message(self, *args):  # silence stderr noise
        pass

    def do_POST(self):
        length = int(self.headers.get("Content-Length", 0))
        raw = self.rfile.read(length)
        try:
            msg = json.loads(raw)
        except json.JSONDecodeError:
            self.send_response(400)
            self.end_headers()
            return

        # Notification (no id) → 202 Accepted, no body.
        if msg.get("id") is None and "method" in msg:
            self.send_response(202)
            self.send_header("Mcp-Session-Id", SESSION_ID)
            self.end_headers()
            return

        res = result_for(msg)
        if "error" in (res or {}):
            body = {"jsonrpc": "2.0", "id": msg.get("id"), "error": res["error"]}
        else:
            body = {"jsonrpc": "2.0", "id": msg.get("id"), "result": res}
        payload = json.dumps(body, separators=(",", ":")).encode()

        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Mcp-Session-Id", SESSION_ID)
        self.send_header("Content-Length", str(len(payload)))
        self.end_headers()
        self.wfile.write(payload)


def main() -> None:
    port = int(sys.argv[1])
    HTTPServer(("127.0.0.1", port), Handler).serve_forever()


if __name__ == "__main__":
    main()
