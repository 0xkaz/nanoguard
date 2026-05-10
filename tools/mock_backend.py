#!/usr/bin/env python3
"""Minimal OpenAI-compatible backend used by tools/e2e.sh.

Echoes the most recent user message back as the assistant's reply, so
tests can verify that nanoguard masked the prompt before forwarding
(by inspecting `[mock] received` log lines on stderr) and that the
output deanonymizer restored placeholders before returning to client.

For SSE tests, set `?stream=1` or pass `"stream": true` in the body —
the backend emits a series of `data: {...}` chunks then `data: [DONE]`.
"""
import json
import sys
import time
from http.server import BaseHTTPRequestHandler, HTTPServer


def extract_user_text(req):
    user_msg = ""
    for m in req.get("messages", []):
        if m.get("role") == "user":
            content = m.get("content")
            if isinstance(content, str):
                user_msg = content
            elif isinstance(content, list):
                user_msg = " ".join(
                    p.get("text", "") for p in content if p.get("type") == "text"
                )
    return user_msg


class MockHandler(BaseHTTPRequestHandler):
    def log_message(self, fmt, *args):
        sys.stderr.write("[mock] " + fmt % args + "\n")

    def do_POST(self):
        length = int(self.headers.get("Content-Length", "0"))
        body = self.rfile.read(length)
        try:
            req = json.loads(body)
        except Exception:
            self.send_response(400)
            self.end_headers()
            return
        sys.stderr.write(f"[mock] received: {json.dumps(req)[:400]}\n")
        sys.stderr.flush()

        user_msg = extract_user_text(req)
        echo = f"You said: {user_msg}"

        if req.get("stream"):
            self.handle_stream(req, echo)
        else:
            self.handle_unary(req, echo)

    def handle_unary(self, req, echo):
        resp = {
            "id": "chatcmpl-mock",
            "object": "chat.completion",
            "model": req.get("model", "mock-model"),
            "choices": [
                {
                    "index": 0,
                    "message": {"role": "assistant", "content": echo},
                    "finish_reason": "stop",
                }
            ],
            "usage": {"prompt_tokens": 10, "completion_tokens": 10, "total_tokens": 20},
        }
        body = json.dumps(resp).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def handle_stream(self, req, echo):
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Cache-Control", "no-cache")
        self.end_headers()

        # Split echo into deliberately small chunks to exercise SSE buffering.
        chunks = []
        i = 0
        while i < len(echo):
            chunks.append(echo[i : i + 5])
            i += 5

        for c in chunks:
            payload = {
                "id": "chatcmpl-mock",
                "object": "chat.completion.chunk",
                "model": req.get("model", "mock-model"),
                "choices": [{"index": 0, "delta": {"content": c}}],
            }
            line = f"data: {json.dumps(payload)}\n\n".encode()
            try:
                self.wfile.write(line)
                self.wfile.flush()
            except BrokenPipeError:
                return
            time.sleep(0.01)

        # Final usage chunk if include_usage requested
        opts = req.get("stream_options") or {}
        if opts.get("include_usage"):
            usage_payload = {
                "id": "chatcmpl-mock",
                "object": "chat.completion.chunk",
                "model": req.get("model", "mock-model"),
                "choices": [],
                "usage": {
                    "prompt_tokens": 10,
                    "completion_tokens": 10,
                    "total_tokens": 20,
                },
            }
            self.wfile.write(f"data: {json.dumps(usage_payload)}\n\n".encode())

        self.wfile.write(b"data: [DONE]\n\n")
        try:
            self.wfile.flush()
        except BrokenPipeError:
            pass


if __name__ == "__main__":
    port = int(sys.argv[1]) if len(sys.argv) > 1 else 11500
    print(f"[mock] listening on :{port}", file=sys.stderr)
    HTTPServer(("127.0.0.1", port), MockHandler).serve_forever()
