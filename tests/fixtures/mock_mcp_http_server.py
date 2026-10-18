#!/usr/bin/env python3
"""Minimal Streamable HTTP MCP server for lodan integration testing.

Implements just enough for: initialize → notifications/initialized →
tools/list → tools/call(echo), over a single HTTP endpoint.

- Client POSTs newline-free JSON-RPC; we reply with application/json.
- initialize response carries an `Mcp-Session-Id` header; later requests echo it.
- Notifications (no `id`) get 202 Accepted with no body.

Server → client messages (#83):

- GET with `Accept: text/event-stream` (and the session header) opens the server
  stream. Messages queued by the tools below are written to it as SSE events.
  With MOCK_MCP_NO_GET=1 the GET is answered with 405 (a server without a stream).
- tools/call `trigger_list_changed`: adds the `extra` tool and sends
  `notifications/tools/list_changed` on the GET stream.
- tools/call `ask_roots`: answers with an SSE stream that first carries a
  `roots/list` request, waits for the client to POST the response, then sends the
  tools/call result with that response as its text.
- tools/call `ping_via_get`: sends a `roots/list` request (id 902) on the GET
  stream and returns at once. tools/call `last_response` {id} waits for the
  client's response to that id and returns it as text.
- Client responses (an `id` and no `method`) are recorded and get 202 Accepted.

Usage: mock_mcp_http_server.py <port>
"""
from __future__ import annotations

import json
import os
import sys
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

PROTOCOL_VERSION = "2025-06-18"
SESSION_ID = "test-session-123"

ECHO_TOOL = {
    "name": "echo",
    "description": "Return the input arguments as text.",
    "inputSchema": {"type": "object", "properties": {"msg": {"type": "string"}}},
}
EXTRA_TOOL = {
    "name": "extra",
    "description": "Appears after trigger_list_changed.",
    "inputSchema": {"type": "object"},
}

lock = threading.Condition()
state = {"extra": False, "outbox": [], "responses": {}}


def push(msg: dict) -> None:
    """Queue a message for the GET stream."""
    with lock:
        state["outbox"].append(msg)
        lock.notify_all()


def wait_response(rid: int, timeout: float = 5.0) -> dict | None:
    with lock:
        lock.wait_for(lambda: rid in state["responses"], timeout=timeout)
        return state["responses"].get(rid)


def text_result(text: str) -> dict:
    return {"content": [{"type": "text", "text": text}], "isError": False}


def result_for(msg: dict) -> dict | None:
    """Return the JSON-RPC result object, or None for notifications."""
    method = msg.get("method")
    if method == "initialize":
        return {
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": {"tools": {"listChanged": True}},
            "serverInfo": {"name": "mock-http-mcp", "version": "0.0.1"},
        }
    if method == "tools/list":
        with lock:
            extra = state["extra"]
        return {"tools": [ECHO_TOOL] + ([EXTRA_TOOL] if extra else [])}
    if method == "tools/call":
        params = msg.get("params") or {}
        name = params.get("name")
        args = params.get("arguments") or {}
        if name == "echo":
            text = json.dumps(args, separators=(",", ":"))
            return text_result(f"echo:{text}")
        if name == "trigger_list_changed":
            with lock:
                state["extra"] = True
            push({"jsonrpc": "2.0", "method": "notifications/tools/list_changed"})
            return text_result("ok")
        if name == "ping_via_get":
            push({"jsonrpc": "2.0", "id": 902, "method": "roots/list"})
            return text_result("sent")
        if name == "last_response":
            got = wait_response(int(args.get("id", 0)))
            return text_result(json.dumps(got) if got else "no response")
    return {"error": {"code": -32601, "message": f"method not found: {method}"}}


class Handler(BaseHTTPRequestHandler):
    def log_message(self, *args):  # silence stderr noise
        pass

    def sse(self, msg: dict) -> None:
        self.wfile.write(f"data: {json.dumps(msg)}\n\n".encode())
        self.wfile.flush()

    def do_GET(self):
        if os.environ.get("MOCK_MCP_NO_GET"):
            self.send_response(405)
            self.end_headers()
            return
        if "text/event-stream" not in (self.headers.get("Accept") or ""):
            self.send_response(406)
            self.end_headers()
            return
        if self.headers.get("Mcp-Session-Id") != SESSION_ID:
            self.send_response(400)
            self.end_headers()
            return
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.end_headers()
        self.wfile.flush()
        try:
            while True:
                with lock:
                    lock.wait_for(lambda: state["outbox"], timeout=0.5)
                    batch, state["outbox"] = state["outbox"], []
                for msg in batch:
                    self.sse(msg)
        except (BrokenPipeError, ConnectionResetError):
            return

    def do_POST(self):
        length = int(self.headers.get("Content-Length", 0))
        raw = self.rfile.read(length)
        try:
            msg = json.loads(raw)
        except json.JSONDecodeError:
            self.send_response(400)
            self.end_headers()
            return

        # Client response to a server request → record it, 202 Accepted.
        if "method" not in msg and msg.get("id") is not None:
            with lock:
                state["responses"][msg["id"]] = msg
                lock.notify_all()
            self.send_response(202)
            self.end_headers()
            return

        # Notification (no id) → 202 Accepted, no body.
        if msg.get("id") is None and "method" in msg:
            self.send_response(202)
            self.send_header("Mcp-Session-Id", SESSION_ID)
            self.end_headers()
            return

        params = msg.get("params") or {}
        if msg.get("method") == "tools/call" and params.get("name") == "ask_roots":
            # Server request first, in the same SSE stream as the eventual response.
            self.send_response(200)
            self.send_header("Content-Type", "text/event-stream")
            self.send_header("Mcp-Session-Id", SESSION_ID)
            self.end_headers()
            self.sse({"jsonrpc": "2.0", "id": 901, "method": "roots/list"})
            got = wait_response(901)
            self.sse({
                "jsonrpc": "2.0",
                "id": msg.get("id"),
                "result": text_result(json.dumps(got) if got else "no response"),
            })
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
    server = ThreadingHTTPServer(("127.0.0.1", port), Handler)
    server.daemon_threads = True
    server.serve_forever()


if __name__ == "__main__":
    main()
