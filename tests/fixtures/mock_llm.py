#!/usr/bin/env python3
"""
Minimal OpenAI Chat Completions mock for testing lodan end-to-end.

Scripted flow: when the user says anything containing "demo", the mock walks
through Write -> Read -> Edit -> Grep -> Glob -> Bash one tool per turn, then
emits a final summary. Otherwise it returns a greeting.

Supports both stream=false (single JSON) and stream=true (SSE).

Usage: mock_llm.py <port> [<demo_dir>]
"""
import json
import sys
import time
from http.server import BaseHTTPRequestHandler, HTTPServer


def build_steps(demo_dir):
    return [
        ("Write", {"path": f"{demo_dir}/hello.txt", "content": "hi"}),
        ("Read",  {"path": f"{demo_dir}/hello.txt"}),
        ("Edit",  {"path": f"{demo_dir}/hello.txt",
                   "old_string": "hi", "new_string": "hello world"}),
        ("Grep",  {"pattern": "hello", "path": demo_dir}),
        ("Glob",  {"pattern": "**/*.txt", "path": demo_dir}),
        ("Bash",  {"command": f"ls -la {demo_dir}"}),
    ]


def make_handler(demo_dir):
    steps = build_steps(demo_dir)

    def decide(messages):
        last_user = next(
            (m for m in reversed(messages) if m.get("role") == "user"), None
        )
        user_content = (last_user or {}).get("content", "") or ""
        tool_count = sum(1 for m in messages if m.get("role") == "tool")

        if "demo" in user_content.lower():
            if tool_count < len(steps):
                name, args = steps[tool_count]
                return ("tools", [(name, args)])
            return ("text",
                    f"Demo complete: exercised {len(steps)} tools "
                    f"({', '.join(s[0] for s in steps)}).")

        return ("text",
                "Hello from mock LLM. Send 'demo' to run the full tool sequence.")

    def build_message(decision):
        kind, payload = decision
        if kind == "text":
            return {"role": "assistant", "content": payload}
        calls = []
        for i, (name, args) in enumerate(payload):
            calls.append({
                "id": f"call_{int(time.time()*1000)}_{i}",
                "type": "function",
                "function": {"name": name, "arguments": json.dumps(args)},
            })
        return {"role": "assistant", "content": None, "tool_calls": calls}

    def non_streaming_body(decision):
        msg = build_message(decision)
        return {
            "id": f"chatcmpl-mock-{int(time.time())}",
            "object": "chat.completion",
            "created": int(time.time()),
            "model": "mock",
            "choices": [
                {"index": 0, "message": msg,
                 "finish_reason": "tool_calls" if msg.get("tool_calls") else "stop"}
            ],
        }

    def stream_chunks(decision):
        base = {
            "id": f"chatcmpl-mock-{int(time.time())}",
            "object": "chat.completion.chunk",
            "created": int(time.time()),
            "model": "mock",
        }
        kind, payload = decision
        if kind == "text":
            text = payload
            mid = max(1, len(text) // 2)
            for piece in (text[:mid], text[mid:]):
                yield {**base, "choices": [
                    {"index": 0, "delta": {"content": piece}, "finish_reason": None}
                ]}
            yield {**base, "choices": [
                {"index": 0, "delta": {}, "finish_reason": "stop"}
            ]}
            return
        for i, (name, args) in enumerate(payload):
            call_id = f"call_{int(time.time()*1000)}_{i}"
            yield {**base, "choices": [{
                "index": 0,
                "delta": {"tool_calls": [{
                    "index": i,
                    "id": call_id,
                    "type": "function",
                    "function": {"name": name, "arguments": ""},
                }]},
                "finish_reason": None,
            }]}
            yield {**base, "choices": [{
                "index": 0,
                "delta": {"tool_calls": [{
                    "index": i,
                    "function": {"arguments": json.dumps(args)},
                }]},
                "finish_reason": None,
            }]}
        yield {**base, "choices": [
            {"index": 0, "delta": {}, "finish_reason": "tool_calls"}
        ]}

    class Handler(BaseHTTPRequestHandler):
        def log_message(self, fmt, *args):
            sys.stderr.write("[mock] " + (fmt % args) + "\n")

        def do_POST(self):
            if self.path not in ("/v1/chat/completions", "/chat/completions"):
                self.send_error(404)
                return
            length = int(self.headers.get("Content-Length", "0"))
            body = self.rfile.read(length) if length else b"{}"
            try:
                req = json.loads(body)
            except Exception as e:
                self.send_error(400, str(e))
                return

            messages = req.get("messages", [])
            stream = bool(req.get("stream"))
            decision = decide(messages)

            if not stream:
                resp = non_streaming_body(decision)
                data = json.dumps(resp).encode()
                self.send_response(200)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(data)))
                self.end_headers()
                self.wfile.write(data)
                return

            self.send_response(200)
            self.send_header("Content-Type", "text/event-stream")
            self.send_header("Cache-Control", "no-cache")
            self.end_headers()
            for chunk in stream_chunks(decision):
                self.wfile.write(b"data: ")
                self.wfile.write(json.dumps(chunk).encode())
                self.wfile.write(b"\n\n")
                self.wfile.flush()
            self.wfile.write(b"data: [DONE]\n\n")
            self.wfile.flush()

    return Handler


if __name__ == "__main__":
    port = int(sys.argv[1]) if len(sys.argv) > 1 else 8080
    demo_dir = sys.argv[2] if len(sys.argv) > 2 else "/tmp/lodan-mock"
    sys.stderr.write(
        f"[mock] listening on http://127.0.0.1:{port} demo_dir={demo_dir}\n"
    )
    HTTPServer(("127.0.0.1", port), make_handler(demo_dir)).serve_forever()
