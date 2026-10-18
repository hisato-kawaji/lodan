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

OAuth (MOCK_MCP_OAUTH=1, #83): the MCP endpoint answers 401 with
`WWW-Authenticate: Bearer resource_metadata=...` unless the request carries a
valid `Authorization: Bearer` token. The same server is the authorization server:
protected resource metadata, authorization server metadata, dynamic client
registration (/register), /authorize (redirects straight back with a code — the
"browser" in tests is curl) and /token (authorization_code with PKCE S256 and
refresh_token with rotation). Tokens are at-1, at-2, … and rt-1, rt-2, ….
- tools/call `expire_tokens`: every access token issued so far stops working
  (the refresh token still does).
- MOCK_MCP_LOG=<path>: append one line per authorized MCP request:
  `<token> <method>[ <tool name>]`.

Usage: mock_mcp_http_server.py <port>
"""
from __future__ import annotations

import base64
import hashlib
import json
import os
import sys
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import parse_qs, urlencode, urlparse

PROTOCOL_VERSION = "2025-06-18"
SESSION_ID = "test-session-123"

ECHO_TOOL = {
    "name": "echo",
    "description": "Return the input arguments as text.",
    "inputSchema": {"type": "object", "properties": {"msg": {"type": "string"}}},
}
EXPIRE_TOOL = {
    "name": "expire_tokens",
    "description": "Invalidate every access token issued so far (OAuth mode only).",
    "inputSchema": {"type": "object"},
}
EXTRA_TOOL = {
    "name": "extra",
    "description": "Appears after trigger_list_changed.",
    "inputSchema": {"type": "object"},
}

lock = threading.Condition()
state = {"extra": False, "outbox": [], "responses": {}}
OAUTH = bool(os.environ.get("MOCK_MCP_OAUTH"))
# codes: code -> request; access: valid access tokens; refresh: refresh token -> client_id
auth = {"n": 0, "tok": 0, "clients": {}, "codes": {}, "access": set(), "refresh": {}}


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
        return {"tools": [ECHO_TOOL] + ([EXPIRE_TOOL] if OAUTH else [])
                + ([EXTRA_TOOL] if extra else [])}
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
        if name == "expire_tokens":
            with lock:
                auth["access"].clear()
            return text_result("expired")
    return {"error": {"code": -32601, "message": f"method not found: {method}"}}


def issue_tokens(client_id: str) -> dict:
    with lock:
        auth["tok"] += 1
        n = auth["tok"]
        auth["access"].add(f"at-{n}")
        auth["refresh"][f"rt-{n}"] = client_id
    return {"access_token": f"at-{n}", "refresh_token": f"rt-{n}",
            "token_type": "Bearer", "expires_in": 3600}


class Handler(BaseHTTPRequestHandler):
    def log_message(self, *args):  # silence stderr noise
        pass

    def base(self) -> str:
        return f"http://127.0.0.1:{self.server.server_address[1]}"

    def json_reply(self, status: int, body: dict, headers: dict | None = None) -> None:
        payload = json.dumps(body).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(payload)))
        for k, v in (headers or {}).items():
            self.send_header(k, v)
        self.end_headers()
        self.wfile.write(payload)

    def bearer(self) -> str | None:
        value = self.headers.get("Authorization") or ""
        return value[7:] if value.startswith("Bearer ") else None

    def authorized(self) -> bool:
        """MCP endpoint guard. Answers 401 itself when the token is missing or stale."""
        if not OAUTH:
            return True
        token = self.bearer()
        with lock:
            ok = token in auth["access"]
        if not ok:
            meta = f"{self.base()}/.well-known/oauth-protected-resource"
            self.send_response(401)
            self.send_header("WWW-Authenticate", f'Bearer resource_metadata="{meta}"')
            self.send_header("Content-Length", "0")
            self.end_headers()
        return ok

    def log_request_line(self, msg: dict) -> None:
        path = os.environ.get("MOCK_MCP_LOG")
        if not path or not OAUTH or msg.get("method") is None:
            return
        line = f"{self.bearer()} {msg['method']}"
        if msg["method"] == "tools/call":
            line += f" {(msg.get('params') or {}).get('name')}"
        with lock, open(path, "a") as fh:
            fh.write(line + "\n")

    def oauth_get(self, url) -> bool:
        """Authorization-server GET routes. True if handled."""
        if url.path == "/.well-known/oauth-protected-resource":
            self.json_reply(200, {"resource": f"{self.base()}/mcp",
                                  "authorization_servers": [self.base()]})
            return True
        if url.path == "/.well-known/oauth-authorization-server":
            self.json_reply(200, {
                "issuer": self.base(),
                "authorization_endpoint": f"{self.base()}/authorize",
                "token_endpoint": f"{self.base()}/token",
                "registration_endpoint": f"{self.base()}/register",
                "code_challenge_methods_supported": ["S256"],
            })
            return True
        if url.path == "/authorize":
            q = {k: v[0] for k, v in parse_qs(url.query).items()}
            ok = (q.get("response_type") == "code"
                  and q.get("code_challenge_method") == "S256"
                  and q.get("resource") == f"{self.base()}/mcp"
                  and auth["clients"].get(q.get("client_id")) == q.get("redirect_uri"))
            if not ok:
                self.json_reply(400, {"error": "invalid_request"})
                return True
            with lock:
                auth["n"] += 1
                code = f"code-{auth['n']}"
                auth["codes"][code] = q
            self.send_response(302)
            target = q["redirect_uri"] + "?" + urlencode({"code": code, "state": q["state"]})
            self.send_header("Location", target)
            self.send_header("Content-Length", "0")
            self.end_headers()
            return True
        return False

    def oauth_post(self, url, raw: bytes) -> bool:
        """Authorization-server POST routes. True if handled."""
        if url.path == "/register":
            body = json.loads(raw)
            uris = body.get("redirect_uris") or []
            if len(uris) != 1 or not uris[0].startswith("http://127.0.0.1:"):
                self.json_reply(400, {"error": "invalid_redirect_uri"})
                return True
            with lock:
                client_id = f"client-{len(auth['clients']) + 1}"
                auth["clients"][client_id] = uris[0]
            self.json_reply(201, {"client_id": client_id, "redirect_uris": uris})
            return True
        if url.path == "/token":
            f = {k: v[0] for k, v in parse_qs(raw.decode()).items()}
            if f.get("resource") != f"{self.base()}/mcp":
                self.json_reply(400, {"error": "invalid_target"})
                return True
            if f.get("grant_type") == "authorization_code":
                with lock:
                    req = auth["codes"].pop(f.get("code"), None)
                digest = hashlib.sha256((f.get("code_verifier") or "").encode()).digest()
                challenge = base64.urlsafe_b64encode(digest).rstrip(b"=").decode()
                if (not req or req["code_challenge"] != challenge
                        or req["redirect_uri"] != f.get("redirect_uri")
                        or req["client_id"] != f.get("client_id")):
                    self.json_reply(400, {"error": "invalid_grant"})
                    return True
                self.json_reply(200, issue_tokens(f["client_id"]))
                return True
            if f.get("grant_type") == "refresh_token":
                with lock:
                    client = auth["refresh"].pop(f.get("refresh_token"), None)
                if client is None or client != f.get("client_id"):
                    self.json_reply(400, {"error": "invalid_grant"})
                    return True
                self.json_reply(200, issue_tokens(client))
                return True
            self.json_reply(400, {"error": "unsupported_grant_type"})
            return True
        return False

    def sse(self, msg: dict) -> None:
        self.wfile.write(f"data: {json.dumps(msg)}\n\n".encode())
        self.wfile.flush()

    def do_GET(self):
        if OAUTH and self.oauth_get(urlparse(self.path)):
            return
        if not self.authorized():
            return
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
        if OAUTH and self.oauth_post(urlparse(self.path), raw):
            return
        if not self.authorized():
            return
        try:
            msg = json.loads(raw)
        except json.JSONDecodeError:
            self.send_response(400)
            self.end_headers()
            return

        self.log_request_line(msg)

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
