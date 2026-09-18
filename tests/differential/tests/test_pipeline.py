"""End-to-end test of the differential harness's own machinery: the recording proxy, the
workload driver, normalization, and the diff report -- using two small local HTTP servers as
stand-ins for "Synapse" and "our server" so this runs with no Docker and no network.

This is what proves `tests/differential/` actually works, independent of Synapse being reachable
(`run_differential.py` handles the Docker-off case separately; this file is what would catch a
bug in the harness itself). Run with:

    python3 -m unittest discover -s tests/differential/tests -v
"""

from __future__ import annotations

import json
import random
import string
import sys
import tempfile
import threading
import unittest
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

sys.path.insert(0, str(Path(__file__).parent.parent / "lib"))
from diff import diff_runs, summarize  # noqa: E402
from driver import run_workload  # noqa: E402
from normalize import Normalizer  # noqa: E402
from proxy import serve as serve_proxy  # noqa: E402

TEST_WORKLOAD = {
    "name": "pipeline_test",
    "steps": [
        {
            "name": "create",
            "method": "POST",
            "path": "/thing",
            "body": {"echo": "hello from the test"},
            "extract": {"id": "$.event_id"},
        },
        {"name": "get", "method": "GET", "path": "/thing/{{id}}"},
    ],
}


def _make_handler(corrupt: bool = False):
    """Builds a `BaseHTTPRequestHandler` subclass simulating a tiny, nondeterministic
    "homeserver": every response carries a random event ID and timestamp (like a real server
    would) but echoes the request body's `echo` field back verbatim -- unless `corrupt`, which
    deliberately behaves differently, standing in for a real behavioral divergence between two
    servers under test.
    """

    class _FakeHomeserverHandler(BaseHTTPRequestHandler):
        def _handle(self) -> None:
            length = int(self.headers.get("Content-Length", 0) or 0)
            raw = self.rfile.read(length) if length else b""
            try:
                body = json.loads(raw) if raw else {}
            except json.JSONDecodeError:
                body = {}
            echo = body.get("echo")
            if corrupt and echo:
                echo = echo[::-1]
            response = {
                # Deliberately no field embedding the request path (like a real Matrix response,
                # which never echoes its own URL): normalize.py rewrites whole-value IDs, not IDs
                # buried inside a larger string, and a debug `path` echo would defeat this test's
                # point by carrying an unnormalized copy of the (nondeterministic) event ID.
                "event_id": "$" + "".join(random.choices(string.ascii_lowercase, k=12)) + ":fake.example.org",
                "origin_server_ts": random.randint(1_700_000_000_000, 1_800_000_000_000),
                "echo": echo,
            }
            data = json.dumps(response).encode("utf-8")
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(data)))
            self.end_headers()
            self.wfile.write(data)

        do_GET = _handle
        do_POST = _handle
        do_PUT = _handle

        def log_message(self, fmt: str, *args) -> None:  # noqa: A002
            pass

    return _FakeHomeserverHandler


class _RunningServer:
    def __init__(self, server: ThreadingHTTPServer):
        self.server = server
        self.thread = threading.Thread(target=server.serve_forever, daemon=True)
        self.thread.start()

    @property
    def base_url(self) -> str:
        host, port = self.server.server_address[:2]
        return f"http://127.0.0.1:{port}"

    def close(self) -> None:
        self.server.shutdown()
        self.server.server_close()


def _start_fake_homeserver(corrupt: bool = False) -> _RunningServer:
    server = ThreadingHTTPServer(("127.0.0.1", 0), _make_handler(corrupt))
    return _RunningServer(server)


class PipelineTests(unittest.TestCase):
    def setUp(self):
        self._servers: list[_RunningServer] = []

    def tearDown(self):
        for s in self._servers:
            s.close()

    def start_fake(self, corrupt: bool = False) -> _RunningServer:
        s = _start_fake_homeserver(corrupt)
        self._servers.append(s)
        return s

    def test_driver_runs_a_workload_and_extracts_variables_between_steps(self):
        server = self.start_fake()
        results = run_workload(server.base_url, TEST_WORKLOAD)
        self.assertEqual(len(results), 2)
        self.assertEqual(results[0]["response"]["status"], 200)
        # The "get" step's path had {{id}} substituted with the "create" step's extracted event_id.
        self.assertIn(results[0]["response"]["json"]["event_id"], results[1]["request"]["path"])

    def test_two_equivalent_but_nondeterministic_servers_diff_as_a_full_match(self):
        baseline_server = self.start_fake()
        candidate_server = self.start_fake()

        baseline_raw = run_workload(baseline_server.base_url, TEST_WORKLOAD)
        candidate_raw = run_workload(candidate_server.base_url, TEST_WORKLOAD)

        norm_b, norm_c = Normalizer(), Normalizer()
        baseline_steps = [
            {"name": r["name"], "response": {"status": r["response"]["status"], "json": norm_b.normalize(r["response"]["json"])}}
            for r in baseline_raw
        ]
        candidate_steps = [
            {"name": r["name"], "response": {"status": r["response"]["status"], "json": norm_c.normalize(r["response"]["json"])}}
            for r in candidate_raw
        ]

        results = diff_runs(baseline_steps, candidate_steps)
        passed, total = summarize(results)
        self.assertEqual((passed, total), (2, 2), msg=results)

    def test_a_genuinely_divergent_server_is_caught_as_a_mismatch(self):
        baseline_server = self.start_fake(corrupt=False)
        candidate_server = self.start_fake(corrupt=True)  # reverses the echoed body: a real bug

        baseline_raw = run_workload(baseline_server.base_url, TEST_WORKLOAD)
        candidate_raw = run_workload(candidate_server.base_url, TEST_WORKLOAD)

        norm_b, norm_c = Normalizer(), Normalizer()
        baseline_steps = [
            {"name": r["name"], "response": {"status": r["response"]["status"], "json": norm_b.normalize(r["response"]["json"])}}
            for r in baseline_raw
        ]
        candidate_steps = [
            {"name": r["name"], "response": {"status": r["response"]["status"], "json": norm_c.normalize(r["response"]["json"])}}
            for r in candidate_raw
        ]

        results = diff_runs(baseline_steps, candidate_steps)
        passed, total = summarize(results)
        self.assertLess(passed, total, msg="the corrupted server's divergence must be caught")
        mismatched_names = {r["name"] for r in results if not r["match"]}
        self.assertIn("create", mismatched_names)

    def test_recording_proxy_captures_the_same_number_of_pairs_as_workload_steps(self):
        target = self.start_fake()
        with tempfile.TemporaryDirectory() as tmp:
            out_path = Path(tmp) / "baseline.jsonl"
            proxy_server = serve_proxy(0, target.base_url, str(out_path))
            proxy = _RunningServer(proxy_server)
            self._servers.append(proxy)

            results = run_workload(proxy.base_url, TEST_WORKLOAD)
            self.assertEqual(len(results), 2)

            recorded = [json.loads(line) for line in out_path.read_text().splitlines() if line.strip()]
            self.assertEqual(len(recorded), 2)
            self.assertEqual(recorded[0]["status"], 200)
            self.assertEqual(recorded[0]["response_body"]["echo"], "hello from the test")

    def test_proxy_recorded_baseline_replays_and_matches_a_fresh_run_of_the_same_target(self):
        # This is the shape record.py + replay.py use for real: record once through the proxy,
        # then compare a later, independent run's normalized responses against that recording.
        target = self.start_fake()
        with tempfile.TemporaryDirectory() as tmp:
            out_path = Path(tmp) / "baseline.jsonl"
            proxy_server = serve_proxy(0, target.base_url, str(out_path))
            proxy = _RunningServer(proxy_server)
            self._servers.append(proxy)

            run_workload(proxy.base_url, TEST_WORKLOAD)  # record.py's job
            baseline_raw = [json.loads(line) for line in out_path.read_text().splitlines() if line.strip()]

            # replay.py's job: run the same workload again directly (a second, later run of the
            # exact same target here, standing in for "our candidate server" in a real diff).
            candidate_raw = run_workload(target.base_url, TEST_WORKLOAD)

            step_names = [s["name"] for s in TEST_WORKLOAD["steps"]]
            norm_b, norm_c = Normalizer(), Normalizer()
            baseline_steps = [
                {"name": name, "response": {"status": rec["status"], "json": norm_b.normalize(rec["response_body"])}}
                for name, rec in zip(step_names, baseline_raw)
            ]
            candidate_steps = [
                {"name": r["name"], "response": {"status": r["response"]["status"], "json": norm_c.normalize(r["response"]["json"])}}
                for r in candidate_raw
            ]

            results = diff_runs(baseline_steps, candidate_steps)
            passed, total = summarize(results)
            self.assertEqual((passed, total), (2, 2), msg=results)


if __name__ == "__main__":
    unittest.main()
