#!/usr/bin/env python3
"""Drives one workload through an already-running recording proxy (`lib/proxy.py`) to produce a
baseline recording.

This does not start the proxy itself, nor Synapse: see `README.md` for the full sequence. Typical
use:

    python3 lib/proxy.py --listen-port 19080 --target http://localhost:8008 \\
        --out baselines/registration.synapse-1.161.jsonl &
    python3 record.py --workload registration --through http://127.0.0.1:19080
"""

from __future__ import annotations

import argparse
import json
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).parent / "lib"))
from driver import run_workload  # noqa: E402


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--workload", required=True, help="Name under workloads/, e.g. registration")
    parser.add_argument(
        "--through", required=True, help="Base URL of the running recording proxy, e.g. http://127.0.0.1:19080"
    )
    args = parser.parse_args()

    workload_path = Path(__file__).parent / "workloads" / f"{args.workload}.json"
    workload = json.loads(workload_path.read_text())

    results = run_workload(args.through, workload)
    ok = sum(1 for r in results if 200 <= r["response"]["status"] < 300)
    print(
        f"drove {len(results)} step(s) of workload {args.workload!r} through the proxy "
        f"({ok} returned 2xx); the proxy's --out file now holds the baseline recording",
        file=sys.stderr,
    )
    for r in results:
        print(f"  [{r['response']['status']}] {r['name']}", file=sys.stderr)
    return 0


if __name__ == "__main__":
    sys.exit(main())
