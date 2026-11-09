#!/usr/bin/env python3
"""Minimal OpenAI-compatible upstream for the e2e probe. Records requests."""
import json
import sys
from http.server import BaseHTTPRequestHandler, HTTPServer

LOG = sys.argv[1]


class H(BaseHTTPRequestHandler):
    def do_POST(self):
        n = int(self.headers.get("content-length") or 0)
        raw = self.rfile.read(n).decode("utf-8", "replace")
        with open(LOG, "a") as f:
            f.write(json.dumps({
                "path": self.path,
                "auth": self.headers.get("authorization"),
                "body": raw[:4000],
            }) + "\n")
        if self.path.rstrip("/").endswith("/chat/completions"):
            body = {
                "id": "chatcmpl-mock-1",
                "object": "chat.completion",
                "created": 1700000000,
                "model": "mock-model",
                "choices": [{
                    "index": 0,
                    "message": {"role": "assistant", "content": "hello from mock upstream"},
                    "finish_reason": "stop",
                }],
                "usage": {"prompt_tokens": 3, "completion_tokens": 5, "total_tokens": 8},
            }
        else:
            body = {"error": {"message": "mock: unexpected path " + self.path, "type": "invalid_request_error"}}
        payload = json.dumps(body).encode()
        self.send_response(200)
        self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(payload)))
        self.end_headers()
        self.wfile.write(payload)

    def log_message(self, *_a):
        pass


HTTPServer(("127.0.0.1", int(sys.argv[2])), H).serve_forever()
