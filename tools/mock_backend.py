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
import os
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
        # Hook: if the user message starts with "TOOL:", interpret the rest
        # as a JSON `tool_calls` array and emit it as the assistant's reply.
        # Used by tool-gate e2e tests to deterministically simulate an LLM
        # that decided to call a tool. Works in both streaming and
        # non-streaming modes; in streaming mode the tool_calls are emitted
        # as a sequence of partial deltas keyed by index, then a final
        # finish_reason chunk.
        tool_calls = None
        if user_msg.startswith("TOOL:"):
            try:
                tool_calls = json.loads(user_msg[len("TOOL:"):])
            except Exception:
                tool_calls = None

        # `BACKEND_LABEL` lets the e2e script run two mock backends
        # on different ports and tell them apart in the response. The
        # default reproduces the historical "You said: ..." shape so
        # existing scenarios keep matching their assertions verbatim.
        label = os.environ.get("BACKEND_LABEL", "")
        echo = f"[{label}] You said: {user_msg}" if label else f"You said: {user_msg}"

        if req.get("stream"):
            if tool_calls is not None:
                self.handle_stream_tool_calls(req, tool_calls)
            else:
                self.handle_stream(req, echo)
        else:
            self.handle_unary(req, echo, tool_calls)

    def handle_unary(self, req, echo, tool_calls=None):
        message = {"role": "assistant", "content": echo}
        if tool_calls is not None:
            message["content"] = None
            message["tool_calls"] = tool_calls
        finish = "tool_calls" if tool_calls else "stop"
        resp = {
            "id": "chatcmpl-mock",
            "object": "chat.completion",
            "model": req.get("model", "mock-model"),
            "choices": [
                {
                    "index": 0,
                    "message": message,
                    "finish_reason": finish,
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

    def handle_stream_tool_calls(self, req, tool_calls):
        """Emit tool_calls as partial deltas, then a finish_reason terminator.

        This exercises the streaming Tool Gate accumulator: the function
        name arrives in one delta, the arguments arrive split across two,
        and `finish_reason: "tool_calls"` triggers evaluation.
        """
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Cache-Control", "no-cache")
        self.end_headers()

        for idx, tc in enumerate(tool_calls):
            func = tc.get("function", {})
            name = func.get("name", "unknown")
            args = func.get("arguments", "{}")
            tc_id = tc.get("id", f"call_{idx}")

            # Delta 1: name + id only.
            payload = {
                "id": "chatcmpl-mock",
                "object": "chat.completion.chunk",
                "model": req.get("model", "mock-model"),
                "choices": [{
                    "index": 0,
                    "delta": {"tool_calls": [{
                        "index": idx,
                        "id": tc_id,
                        "type": "function",
                        "function": {"name": name},
                    }]},
                }],
            }
            self._emit_sse(payload)

            # Deltas 2+: arguments split into two halves.
            mid = max(1, len(args) // 2)
            for chunk in (args[:mid], args[mid:]):
                if not chunk:
                    continue
                payload = {
                    "id": "chatcmpl-mock",
                    "object": "chat.completion.chunk",
                    "model": req.get("model", "mock-model"),
                    "choices": [{
                        "index": 0,
                        "delta": {"tool_calls": [{
                            "index": idx,
                            "function": {"arguments": chunk},
                        }]},
                    }],
                }
                self._emit_sse(payload)

        # Finish reason → triggers the streaming Tool Gate evaluation.
        finish_payload = {
            "id": "chatcmpl-mock",
            "object": "chat.completion.chunk",
            "model": req.get("model", "mock-model"),
            "choices": [{"index": 0, "delta": {}, "finish_reason": "tool_calls"}],
        }
        self._emit_sse(finish_payload)

        # Optional usage chunk.
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
            self._emit_sse(usage_payload)

        try:
            self.wfile.write(b"data: [DONE]\n\n")
            self.wfile.flush()
        except BrokenPipeError:
            pass

    def _emit_sse(self, payload):
        line = f"data: {json.dumps(payload)}\n\n".encode()
        try:
            self.wfile.write(line)
            self.wfile.flush()
        except BrokenPipeError:
            pass
        time.sleep(0.01)

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
