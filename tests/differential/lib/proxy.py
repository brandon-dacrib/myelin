"""A recording HTTP proxy: forwards every request it receives to `--target` and appends one JSON
line per request/response pair to `--out`.

This is the "recording proxy that captures request and response pairs from a homeserver": point
any client at it (this harness's own `driver.py`, `curl`, a browser, `matrix-rust-sdk`, Synapse's
own test suite, ...) instead of directly at the real homeserver, and every exchange is logged
verbatim in addition to being forwarded through unmodified. See `../README.md` for the end-to-end
recording workflow against Synapse.
"""

from __future__ import annotations

import argparse
import base64
import json
import sys
import threading
import time
import urllib.error
import urllib.request
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer


def _maybe_json(raw: bytes):
    if not raw:
        return None
    try:
        return json.loads(raw)
    except json.JSONDecodeError:
        return {"__raw_base64__": base64.b64encode(raw).decode("ascii")}


class _Handler(BaseHTTPRequestHandler):
    target = ""
    out_lock = threading.Lock()
    out_file = None  # set by serve()

    def _proxy(self) -> None:
        length = int(self.headers.get("Content-Length", 0) or 0)
        body = self.rfile.read(length) if length else b""
        url = self.target.rstrip("/") + self.path
        headers = {
            k: v for k, v in self.headers.items() if k.lower() not in ("host", "content-length")
        }
        req = urllib.request.Request(url, data=body or None, method=self.command, headers=headers)
        try:
            with urllib.request.urlopen(req, timeout=30) as resp:
                status = resp.status
                resp_body = resp.read()
                resp_headers = dict(resp.getheaders())
        except urllib.error.HTTPError as exc:
            status = exc.code
            resp_body = exc.read()
            resp_headers = dict(exc.headers or {})
        except OSError as exc:
            self.send_response(502)
            self.end_headers()
            self.wfile.write(f"proxy: could not reach target: {exc}".encode())
            return

        record = {
            "ts": time.time(),
            "method": self.command,
            "path": self.path,
            "request_body": _maybe_json(body),
            "status": status,
            "response_body": _maybe_json(resp_body),
        }
        with self.out_lock:
            self.out_file.write(json.dumps(record) + "\n")
            self.out_file.flush()

        self.send_response(status)
        for k, v in resp_headers.items():
            if k.lower() in ("content-length", "transfer-encoding", "connection"):
                continue
            self.send_header(k, v)
        self.send_header("Content-Length", str(len(resp_body)))
        self.end_headers()
        self.wfile.write(resp_body)

    def do_GET(self) -> None:
        self._proxy()

    def do_POST(self) -> None:
        self._proxy()

    def do_PUT(self) -> None:
        self._proxy()

    def do_DELETE(self) -> None:
        self._proxy()

    def do_PATCH(self) -> None:
        self._proxy()

    def log_message(self, fmt: str, *args) -> None:  # noqa: A002 - stdlib signature
        pass  # quiet; the JSONL file is the record of what happened


def serve(listen_port: int, target: str, out_path: str) -> ThreadingHTTPServer:
    """Builds (but does not run) a recording proxy server. Call `.serve_forever()` on the result,
    or use it as a context manager in tests."""
    handler = type("_BoundHandler", (_Handler,), {})
    handler.target = target
    handler.out_file = open(out_path, "a", encoding="utf-8")  # noqa: SIM115 - lives with the server
    return ThreadingHTTPServer(("127.0.0.1", listen_port), handler)


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--listen-port", type=int, required=True)
    parser.add_argument(
        "--target", required=True, help="Base URL of the real homeserver, e.g. http://localhost:8008"
    )
    parser.add_argument("--out", required=True, help="JSONL file to append recorded pairs to")
    args = parser.parse_args()

    server = serve(args.listen_port, args.target, args.out)
    print(
        f"recording proxy listening on 127.0.0.1:{args.listen_port}, forwarding to {args.target}, "
        f"appending to {args.out}",
        file=sys.stderr,
    )
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        pass


if __name__ == "__main__":
    main()
