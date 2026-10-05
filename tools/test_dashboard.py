#!/usr/bin/env python3
"""Tests for tools/dashboard.py: `python3 -m unittest tools/test_dashboard.py` (no cargo, no
network). Each case builds its own inputs in a temporary directory and points the module's
paths at it, so the committed results are never read or written."""

from __future__ import annotations

import importlib.util
import json
import sys
import tempfile
import unittest
from pathlib import Path
from unittest import mock

HERE = Path(__file__).resolve().parent
spec = importlib.util.spec_from_file_location("dashboard", HERE / "dashboard.py")
dashboard = importlib.util.module_from_spec(spec)
sys.modules["dashboard"] = dashboard
spec.loader.exec_module(dashboard)


class Inputs(unittest.TestCase):
    def setUp(self) -> None:
        self.tmp = tempfile.TemporaryDirectory()
        root = Path(self.tmp.name)
        self.status = root / "docs" / "status"
        (self.status / "sytest").mkdir(parents=True)
        patches = {
            "ROOT": root,
            "STATUS_DIR": self.status,
            "SYTEST_DIR": self.status / "sytest",
            "ROUTES_PATH": self.status / "routes.json",
            "COVERAGE_SNAPSHOT": self.status / "spec-coverage.json",
            "DASHBOARD_PATH": self.status / "dashboard.md",
        }
        self.patchers = [mock.patch.object(dashboard, k, v) for k, v in patches.items()]
        for p in self.patchers:
            p.start()

    def tearDown(self) -> None:
        for p in self.patchers:
            p.stop()
        self.tmp.cleanup()

    def write(self, rel: str, text: str) -> Path:
        path = self.status / rel
        path.write_text(text, encoding="utf-8")
        return path

    def test_sytest_runs_newest_first_and_the_headline_is_the_newest_full_run(self) -> None:
        self.write("sytest/2026-10-01-results.txt", "PASS a\nFAIL b\nSKIP c\nPASS d\n")
        self.write("sytest/2026-10-01b-results.txt", "PASS a\nPASS b\nSKIP c\nPASS d\n")
        self.write("sytest/2026-10-02-partial-results.txt", "PASS a\n")
        self.write(
            "sytest/2026-10-01b-are-we-synapse-yet.txt",
            "Client-Server APIs: 87% (465/537 tests)\n  Registration   :  78% (21/27 tests)\n",
        )
        runs = dashboard.gather_sytest_runs()
        self.assertEqual([r.name for r in runs][0], "2026-10-02-partial")
        latest = dashboard.latest_full_sytest_run(runs)
        # Same date, neither committed: the name breaks the tie, `01b` after `01`.
        self.assertEqual(latest.name, "2026-10-01b")
        self.assertEqual((latest.counts.passed, latest.counts.failed, latest.counts.skipped), (3, 0, 1))
        self.assertEqual(latest.awsy, [("Client-Server APIs", 465, 537)], "top-level groups only")
        first = next(r for r in runs if r.name == "2026-10-01")
        self.assertEqual(first.counts.failing, ["b"])
        self.assertAlmostEqual(first.counts.rate, 200 / 3)

    def test_complement_baselines_and_their_header(self) -> None:
        self.write(
            "complement-federation-results.txt",
            "# Complement tests, top-level results.\n"
            "# Run 11, 2026-10-04, c2d74174 (run 10 on a9f62fc7 was 235 of 314, 56 of 90): "
            "241 of 314 assertions, 58 of 90 top-level (1 skipped).\n"
            "PASS TestA\nFAIL TestB\nSKIP TestC\n",
        )
        [suite] = dashboard.gather_complement_suites()
        self.assertEqual(suite.suite, "federation")
        self.assertEqual((suite.run, suite.date, suite.commit), ("11", "2026-10-04", "c2d74174"))
        self.assertEqual(suite.assertions, (241, 314), "this run's count, not the previous run's")
        self.assertEqual(suite.counts.failing, ["TestB"])

    def test_a_status_file_is_titled_and_dated_by_its_newest_section(self) -> None:
        self.write("03-cluster.md", "## 2026-10-04: a lease (branch `x`)\n\n## 2026-10-01: older\n")
        self.write(
            "11-appservices-and-bridges.md",
            "# Status: track 11, appservices and bridges\n\n## Session 2026-09-30: old\n## Session 2026-10-03: new\n",
        )
        tracks = {t.number: t for t in dashboard.gather_status_summaries()}
        self.assertEqual(tracks["03"].title, "Cluster")
        self.assertEqual(tracks["03"].last_updated, "2026-10-04")
        self.assertEqual(tracks["11"].title, "Appservices and bridges")
        self.assertEqual(tracks["11"].last_updated, "2026-10-03")
        self.assertEqual(tracks["11"].latest_section, "Session 2026-10-03: new")

    def test_without_cargo_the_committed_coverage_snapshot_is_reported(self) -> None:
        summary = {
            "overall_percent": 50.0,
            "total_spec_routes": 2,
            "total_registered": 1,
            "apis": [{"family": "client-server", "spec_total": 2, "registered": 1, "missing": 1, "extra": 0, "percent": 50.0}],
        }
        self.write(
            "spec-coverage.json",
            json.dumps({"generated_at": "2026-10-04T00:00:00Z", "routes_manifest": "docs/status/routes.json", "summary": summary}),
        )
        with mock.patch.object(dashboard.shutil, "which", return_value=None), mock.patch.object(
            sys, "argv", ["dashboard.py"]
        ):
            self.assertEqual(dashboard.main(), 0)
        text = (self.status / "dashboard.md").read_text(encoding="utf-8")
        self.assertIn("**1 / 2 spec routes registered (50.0%)**", text)
        self.assertIn("From the committed snapshot", text)
        self.assertIn("_No committed runs under `docs/status/sytest/`._", text)

    def test_coverage_from_reads_a_bare_json_out_file(self) -> None:
        bare = {"overall_percent": 100.0, "total_spec_routes": 1, "total_registered": 1, "apis": []}
        path = self.write("bare.json", json.dumps(bare))
        snapshot = dashboard.load_coverage_snapshot(path)
        self.assertEqual(snapshot["summary"], bare)


if __name__ == "__main__":
    unittest.main()
