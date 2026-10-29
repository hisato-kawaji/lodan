#!/usr/bin/env python3
"""MCP server protected by OAuth 2.1, plus its own authorization server (#164).

Resource server (any POST path except the ones below): requires `Authorization: Bearer <valid>`,
otherwise 401 with `WWW-Authenticate: Bearer resource_metadata="<base>/.well-known/oauth-protected-resource"`.
Records which Authorization header each path saw (GET /admin/seen).

Authorization server:
- GET /.well-known/oauth-protected-resource, /.well-known/oauth-authorization-server
- POST /register (dynamic client registration) → client_id
- GET /authorize → 302 to redirect_uri with code + state (checks client_id, S256, resource)
- POST /token: authorization_code (checks PKCE, redirect_uri, resource) / refresh_token
- POST /admin/revoke: invalidates the current access token (forces a refresh)

Usage: mock_mcp_oauth_server.py <port> [bad-issuer|bad-resource]
  bad-issuer   the AS metadata names a different issuer than the one it was fetched for
  bad-resource the protected-resource metadata describes a different resource
"""
from __future__ import annotations

import base64
import hashlib
import json
import sys
import threading
import urllib.parse
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

LOCK = threading.Lock()
MODE = sys.argv[2] if len(sys.argv) > 2 else ""
STATE = {
    "clients": {},  # client_id -> redirect_uris
    "codes": {},  # code -> (client_id, redirect_uri, challenge, resource)
    "valid": set(),  # valid access tokens
    "refresh": {},  # refresh token -> client_id
    "n": 0,
    "seen": {},  # path -> list of Authorization headers
}
ECHO_TOOL = {"name": "echo", "description": "echo", "inputSchema": {"type": "object"}}


class Handler(BaseHTTPRequestHandler):
    def log_message(self, *args):
        pass

    @property
    def base(self) -> str:
        return f"http://127.0.0.1:{self.server.server_address[1]}"

    def reply(self, status: int, body=None, headers=None):
        payload = b"" if body is None else json.dumps(body).encode()
        self.send_response(status)
        for k, v in (headers or {}).items():
            self.send_header(k, v)
        if body is not None:
            self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(payload)))
        self.end_headers()
        self.wfile.write(payload)

    def body(self) -> bytes:
        return self.rfile.read(int(self.headers.get("Content-Length", 0)))

    def do_GET(self):
        url = urllib.parse.urlparse(self.path)
        q = dict(urllib.parse.parse_qsl(url.query))
        if url.path == "/.well-known/oauth-protected-resource":
            resource = "https://elsewhere.example/mcp" if MODE == "bad-resource" else self.base + "/mcp"
            return self.reply(200, {"resource": resource, "authorization_servers": [self.base]})
        if url.path == "/.well-known/oauth-authorization-server":
            return self.reply(200, {
                "issuer": self.base + "/impostor" if MODE == "bad-issuer" else self.base,
                "authorization_endpoint": self.base + "/authorize",
                "token_endpoint": self.base + "/token",
                "registration_endpoint": self.base + "/register",
                "code_challenge_methods_supported": ["S256"],
            })
        if url.path == "/authorize":
            with LOCK:
                uris = STATE["clients"].get(q.get("client_id"))
                ok = (
                    uris is not None
                    and q.get("redirect_uri") in uris
                    and q.get("code_challenge_method") == "S256"
                    and q.get("response_type") == "code"
                    and q.get("resource") == self.base + "/mcp"
                )
                if not ok:
                    return self.reply(400, {"error": "invalid_request", "q": q})
                STATE["n"] += 1
                code = f"code-{STATE['n']}"
                STATE["codes"][code] = (q["client_id"], q["redirect_uri"], q["code_challenge"], q["resource"])
            loc = q["redirect_uri"] + "?" + urllib.parse.urlencode({"code": code, "state": q.get("state", "")})
            self.send_response(302)
            self.send_header("Location", loc)
            self.send_header("Content-Length", "0")
            self.end_headers()
            return
        if url.path == "/admin/seen":
            with LOCK:
                return self.reply(200, STATE["seen"])
        return self.reply(405)

    def issue(self, client_id: str) -> dict:
        STATE["n"] += 1
        at, rt = f"at-{STATE['n']}", f"rt-{STATE['n']}"
        STATE["valid"].add(at)
        STATE["refresh"][rt] = client_id
        return {"access_token": at, "token_type": "Bearer", "expires_in": 3600, "refresh_token": rt}

    def do_POST(self):
        url = urllib.parse.urlparse(self.path)
        raw = self.body()
        if url.path == "/register":
            meta = json.loads(raw)
            with LOCK:
                STATE["n"] += 1
                cid = f"client-{STATE['n']}"
                STATE["clients"][cid] = meta.get("redirect_uris", [])
            return self.reply(201, {"client_id": cid, **meta})
        if url.path == "/token":
            form = dict(urllib.parse.parse_qsl(raw.decode()))
            with LOCK:
                if form.get("grant_type") == "authorization_code":
                    entry = STATE["codes"].pop(form.get("code"), None)
                    if entry is None:
                        return self.reply(400, {"error": "invalid_grant"})
                    cid, redirect, challenge, resource = entry
                    digest = hashlib.sha256(form.get("code_verifier", "").encode()).digest()
                    expected = base64.urlsafe_b64encode(digest).rstrip(b"=").decode()
                    if (form.get("client_id"), form.get("redirect_uri"), form.get("resource")) != (cid, redirect, resource) \
                            or expected != challenge:
                        return self.reply(400, {"error": "invalid_grant", "why": "pkce/redirect/resource"})
                    return self.reply(200, self.issue(cid))
                if form.get("grant_type") == "refresh_token":
                    cid = STATE["refresh"].pop(form.get("refresh_token"), None)
                    if cid is None or cid != form.get("client_id"):
                        return self.reply(400, {"error": "invalid_grant"})
                    return self.reply(200, self.issue(cid))
            return self.reply(400, {"error": "unsupported_grant_type"})
        if url.path == "/admin/revoke":
            with LOCK:
                STATE["valid"].clear()
            return self.reply(204)

        # MCP resource server.
        auth = self.headers.get("Authorization")
        with LOCK:
            STATE["seen"].setdefault(url.path, []).append(auth)
            authorized = auth is not None and auth.startswith("Bearer ") and auth[7:] in STATE["valid"]
        if not authorized:
            return self.reply(401, {"error": "unauthorized"}, {
                "WWW-Authenticate": f'Bearer resource_metadata="{self.base}/.well-known/oauth-protected-resource"',
            })
        msg = json.loads(raw)
        if msg.get("id") is None:
            return self.reply(202)
        method = msg.get("method")
        if method == "initialize":
            result = {"protocolVersion": "2025-06-18", "capabilities": {"tools": {}},
                      "serverInfo": {"name": "mock-oauth", "version": "0"}}
        elif method == "tools/list":
            result = {"tools": [ECHO_TOOL]}
        else:
            return self.reply(200, {"jsonrpc": "2.0", "id": msg["id"],
                                    "error": {"code": -32601, "message": method}})
        return self.reply(200, {"jsonrpc": "2.0", "id": msg["id"], "result": result})


def main() -> None:
    server = ThreadingHTTPServer(("127.0.0.1", int(sys.argv[1])), Handler)
    server.daemon_threads = True
    server.serve_forever()


if __name__ == "__main__":
    main()
