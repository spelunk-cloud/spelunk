#!/usr/bin/env python3
"""Minimal stand-in for the inference server that pre-0.9 binaries embed through.

Releases before 0.9.0 send index chunks to a spelunk-server's
POST /v1/projects/{id}/index/embed and store the returned vectors. That era's
server shipped a 768-dimension embedder that no longer exists, and a current
spelunk-server answers on a wire shape those binaries cannot parse, so the
768-dimension wing of the upgrade corpus cannot be captured against either one.

This serves that era's two endpoints so the real old binary can complete a real
index run. Only the vector *values* come from here; the database file, its
schema, its vec0 table declaration and every row in it are written by the
released binary itself, which is what the corpus exists to preserve. Vector
values are irrelevant to a migration test: the dimension-upgrade path discards
them wholesale.

Vectors are derived from a hash of the chunk text, so a regeneration run
produces byte-identical output and does not churn the checked-in fixture.

Two response wires are supported because the era boundary runs through them:
0.8.x reads a JSON `{"chunks":[{"chunk_id","vector"}]}` body, 0.9.x reads raw
little-endian f32 bytes, one dim-float vector per request chunk in request
order.

Usage: embed_stub.py <port> <dim> <json|f32le>
"""

import hashlib
import json
import struct
import sys
from http.server import BaseHTTPRequestHandler, HTTPServer


class Handler(BaseHTTPRequestHandler):
    dim = 768
    wire = "json"

    def log_message(self, *args):
        pass

    def _send(self, payload, code=200):
        body = json.dumps(payload).encode()
        self.send_response(code)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def _send_bytes(self, body):
        self.send_response(200)
        self.send_header("Content-Type", "application/octet-stream")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_GET(self):
        if self.path.rstrip("/").endswith("/v1/health"):
            # `embedding_dim` and `embedder.state` were added mid-0.9: without
            # them a 0.9.x client decides the server has no embedder and
            # indexes text-only, leaving a wing with no vectors to migrate.
            # Releases that pre-date the fields ignore them.
            self._send(
                {
                    "status": "ok",
                    # Union of the capability vocabularies across the captured
                    # releases: a name a given binary does not know is ignored,
                    # a name it needs and does not see disables embedding.
                    "capabilities": [
                        "index.embed",
                        "search.semantic",
                        "memory",
                        "explore",
                        "plan",
                        "embed",
                        "search",
                        "llm",
                    ],
                    "instance_id": "upgrade-corpus-stub",
                    "embedding_dim": self.dim,
                    "embedder": {"state": "ready", "detail": None},
                }
            )
        else:
            self._send({"error": "not found"}, 404)

    def do_POST(self):
        length = int(self.headers.get("Content-Length", 0))
        request = json.loads(self.rfile.read(length) or b"{}")
        if not self.path.endswith("/index/embed"):
            self._send({"error": "not found"}, 404)
            return
        incoming = request.get("chunks", [])
        if self.wire == "f32le":
            body = b"".join(
                struct.pack("<%df" % self.dim, *self._vector(c.get("content", "")))
                for c in incoming
            )
            self._send_bytes(body)
            return
        self._send(
            {
                "chunks": [
                    {
                        "chunk_id": c["chunk_id"],
                        "vector": self._vector(c.get("content", "")),
                    }
                    for c in incoming
                ]
            }
        )

    def _vector(self, text):
        seed = hashlib.blake2b(text.encode(), digest_size=8).digest()
        raw = [(((seed[i % 8] + i) % 255) / 255.0) - 0.5 for i in range(self.dim)]
        norm = sum(x * x for x in raw) ** 0.5 or 1.0
        return [x / norm for x in raw]


def main():
    port = int(sys.argv[1])
    if len(sys.argv) > 2:
        Handler.dim = int(sys.argv[2])
    if len(sys.argv) > 3:
        Handler.wire = sys.argv[3]
    HTTPServer(("127.0.0.1", port), Handler).serve_forever()


if __name__ == "__main__":
    main()
