#!/usr/bin/env python3
"""Entry point for the differential suite in CI and locally.

`docs/workstreams/14-test-and-conformance.md`'s day-one work is "the differential recorder
capturing a baseline from Synapse 1.161 on a scripted workload." That requires a running Synapse,
which requires Docker, which is off in this environment (`docker info` fails here -- see
`docs/status/14-test-and-conformance.md`). Per this track's brief ("make the runner detect that
and skip cleanly"), this script:

1. Checks whether Docker is usable at all. If not, prints why and exits 0 (skip, not failure --
   CI should not treat "no Docker on this runner" as this suite being broken).
2. If Docker *is* usable, checks whether a candidate server binary/URL was given
   (`--candidate-url`). This workspace has no assembled `hs-server` binary yet (`hs-http` and
   `hs-auth` exist as library crates with router fragments, not a listening process), so there is
   nothing to diff against Synapse yet; this is reported plainly rather than treated as failure.
3. If both a working Docker and a `--candidate-url` are available, runs every workload in
   `workloads/` against `--baseline-dir` (recorded ahead of time per `README.md`) and against the
   candidate, exiting nonzero if anything failed to match.
"""

from __future__ import annotations

import argparse
import subprocess
import sys
from pathlib import Path

HERE = Path(__file__).parent


def docker_available() -> bool:
    try:
        result = subprocess.run(
            ["docker", "info"],
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            timeout=5,
            check=False,
        )
        return result.returncode == 0
    except (OSError, subprocess.TimeoutExpired):
        return False


def workload_names() -> list[str]:
    return sorted(p.stem for p in (HERE / "workloads").glob("*.json"))


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument(
        "--candidate-url", default=None, help="Base URL of the server under test, if one is running"
    )
    parser.add_argument(
        "--baseline-dir",
        default=str(HERE / "baselines"),
        help="Directory of <workload>.<label>.jsonl baseline recordings (see README.md)",
    )
    args = parser.parse_args()

    if not docker_available():
        print(
            "SKIP: Docker is not available (`docker info` failed). The differential suite needs a "
            "running Synapse container to record a fresh baseline from, or a pre-recorded baseline "
            "under --baseline-dir plus a --candidate-url to replay against. See "
            "tests/differential/README.md for how to record one when Docker is available. "
            "Exiting 0 (skip, not failure)."
        )
        return 0

    if not args.candidate_url:
        print(
            "Docker is available, but no --candidate-url was given and this workspace has no "
            "assembled server binary yet to point one at. Nothing to replay against; this is the "
            "expected day-one state, not a failure. Once a candidate server exists, run:\n"
            "  python3 tests/differential/run_differential.py --candidate-url http://localhost:PORT"
        )
        return 0

    baseline_dir = Path(args.baseline_dir)
    names = workload_names()
    if not names:
        print("no workloads found under tests/differential/workloads/", file=sys.stderr)
        return 1

    overall_ok = True
    for name in names:
        candidates = sorted(baseline_dir.glob(f"{name}.*.jsonl"))
        if not candidates:
            print(f"[{name}] SKIP: no baseline recording found under {baseline_dir} (see README.md)")
            continue
        baseline = candidates[0]
        print(f"[{name}] replaying against {args.candidate_url}, diffing against {baseline.name}")
        result = subprocess.run(
            [
                sys.executable,
                str(HERE / "replay.py"),
                "--workload",
                name,
                "--baseline",
                str(baseline),
                "--candidate-url",
                args.candidate_url,
            ],
            check=False,
        )
        overall_ok = overall_ok and result.returncode == 0

    return 0 if overall_ok else 1


if __name__ == "__main__":
    sys.exit(main())
