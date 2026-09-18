#!/usr/bin/env python3
"""Replays a workload directly against a candidate server and diffs its normalized responses
against a previously recorded baseline (`record.py` + `lib/proxy.py`).

    python3 replay.py --workload registration \\
        --baseline baselines/registration.synapse-1.161.jsonl \\
        --candidate-url http://localhost:8009

Alignment between the baseline's recorded requests and the candidate's live ones is positional:
both are produced by running the *same* workload's steps in the *same* order (the baseline,
indirectly, by `record.py` driving that workload through the proxy; the candidate directly, by
this script), so the Nth line of the baseline recording corresponds to the workload's Nth step by
construction. If the baseline's line count does not match the workload's step count -- most likely
because the workload file was edited after the baseline was recorded -- this script warns rather
than silently mis-aligning.
"""

from __future__ import annotations

import argparse
import json
import sys
from pathlib import Path
from typing import Any

sys.path.insert(0, str(Path(__file__).parent / "lib"))
from diff import diff_runs, summarize  # noqa: E402
from driver import run_workload  # noqa: E402
from normalize import Normalizer  # noqa: E402


def load_workload(name: str) -> dict[str, Any]:
    path = Path(__file__).parent / "workloads" / f"{name}.json"
    return json.loads(path.read_text())


def load_baseline(path: str) -> list[dict[str, Any]]:
    records = []
    with open(path, encoding="utf-8") as f:
        for line in f:
            line = line.strip()
            if line:
                records.append(json.loads(line))
    return records


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--workload", required=True)
    parser.add_argument("--baseline", required=True, help="Path to a JSONL baseline from record.py")
    parser.add_argument("--candidate-url", required=True, help="Base URL of the server under test")
    parser.add_argument("--verbose", action="store_true", help="Print full JSON for mismatches")
    args = parser.parse_args()

    workload = load_workload(args.workload)
    baseline_raw = load_baseline(args.baseline)
    step_names = [s["name"] for s in workload["steps"]]

    if len(baseline_raw) != len(step_names):
        print(
            f"warning: baseline has {len(baseline_raw)} recorded request(s) but workload "
            f"{args.workload!r} has {len(step_names)} step(s); alignment may be wrong (was this "
            "baseline recorded from a different or since-edited workload file? re-record it)",
            file=sys.stderr,
        )

    candidate_raw = run_workload(args.candidate_url, workload)

    normalizer_baseline = Normalizer()
    normalizer_candidate = Normalizer()
    baseline_steps = [
        {
            "name": name,
            "response": {
                "status": rec["status"],
                "json": normalizer_baseline.normalize(rec.get("response_body")),
            },
        }
        for name, rec in zip(step_names, baseline_raw)
    ]
    candidate_steps = [
        {
            "name": rec["name"],
            "response": {
                "status": rec["response"]["status"],
                "json": normalizer_candidate.normalize(rec["response"]["json"]),
            },
        }
        for rec in candidate_raw
    ]

    results = diff_runs(baseline_steps, candidate_steps)
    passed, total = summarize(results)
    for r in results:
        marker = "OK  " if r["match"] else "DIFF"
        print(f"[{marker}] {r['name']}")
        if not r["match"] and args.verbose:
            print(json.dumps(r, indent=2, default=str))

    print(f"\n{passed}/{total} step(s) matched", file=sys.stderr)
    return 0 if passed == total else 1


if __name__ == "__main__":
    sys.exit(main())
