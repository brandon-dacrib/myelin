#!/usr/bin/env python3
"""UNTESTED (see deploy/chaos/README.md). The per-shard linearizability checker.

Applies the same invariant `crates/hs-cluster/tests/chaos.rs` checks in-process: for every shard,
every committed epoch has exactly one writer, and epochs are non-decreasing in commit order. It
reads each pod's committed-write log via `kubectl logs`, on the assumption that `hs chaos-actor`
(which does not exist yet -- see README.md) prints one line per commit to stdout in the form:

    CHAOS_COMMIT {"shard": "room/3", "epoch": 2, "writer": "hs-chaos-actor-1", "seq": 14}

matching `ChaosLog`'s `LogEntry` shape in `crates/hs-cluster/tests/chaos.rs`. Track 15, when it
wires the actual subcommand, should either match this format or update this script; either way
this file is the drop-in shape of the checker, not a working implementation against a real
cluster yet.

Usage (once `hs chaos-actor` exists):
    checker.py --namespace hs-chaos --assert-no-double-writes
    checker.py --namespace hs-chaos --dump-owners
    checker.py --namespace hs-chaos --scrape-metric hs_cluster_forward_retries_total
"""

from __future__ import annotations

import argparse
import json
import re
import subprocess
import sys
from collections import defaultdict

COMMIT_PREFIX = "CHAOS_COMMIT "


def kubectl(*args: str) -> str:
    result = subprocess.run(["kubectl", *args], capture_output=True, text=True, check=True)
    return result.stdout


def pod_names(namespace: str) -> list[str]:
    out = kubectl("-n", namespace, "get", "pods", "-l", "app=hs-chaos-actor", "-o", "jsonpath={.items[*].metadata.name}")
    return out.split()


def collect_entries(namespace: str) -> list[dict]:
    entries: list[dict] = []
    for pod in pod_names(namespace):
        try:
            logs = kubectl("-n", namespace, "logs", pod, "--tail=-1")
        except subprocess.CalledProcessError as exc:
            print(f"warning: could not read logs for {pod}: {exc}", file=sys.stderr)
            continue
        for line in logs.splitlines():
            if line.startswith(COMMIT_PREFIX):
                entries.append(json.loads(line[len(COMMIT_PREFIX):]))
    return entries


def check_no_double_writes(entries: list[dict]) -> list[str]:
    """Returns a list of human-readable violations; empty means the invariant holds."""
    violations: list[str] = []
    by_shard: dict[str, list[dict]] = defaultdict(list)
    for e in entries:
        by_shard[e["shard"]].append(e)

    for shard, shard_entries in by_shard.items():
        shard_entries.sort(key=lambda e: (e["epoch"], e["seq"]))
        epoch_writers: dict[int, set[str]] = defaultdict(set)
        last_epoch = 0
        for e in shard_entries:
            if e["epoch"] < last_epoch:
                violations.append(f"shard {shard}: epoch went backwards at seq {e['seq']} ({e['epoch']} < {last_epoch})")
            last_epoch = e["epoch"]
            epoch_writers[e["epoch"]].add(e["writer"])
        for epoch, writers in epoch_writers.items():
            if len(writers) > 1:
                violations.append(f"shard {shard} epoch {epoch}: written by more than one replica: {sorted(writers)}")
    return violations


def dump_owners(namespace: str) -> str:
    # Placeholder: once `hs chaos-actor` exposes an admin endpoint (or `hs cluster status`, per
    # RFC 0001's Phase 1 deliverables), this should query it directly instead of guessing from
    # logs. For now it returns the last-seen writer per shard from the collected commit log.
    entries = collect_entries(namespace)
    owners: dict[str, str] = {}
    for e in sorted(entries, key=lambda e: (e["shard"], e["epoch"], e["seq"])):
        owners[e["shard"]] = e["writer"]
    return json.dumps(owners, sort_keys=True)


def scrape_metric(namespace: str, metric: str) -> float:
    total = 0.0
    for pod in pod_names(namespace):
        try:
            text = kubectl("-n", namespace, "exec", pod, "--", "curl", "-s", "http://localhost:9090/metrics")
        except subprocess.CalledProcessError:
            continue
        for line in text.splitlines():
            if line.startswith(metric) and not line.startswith("#"):
                m = re.search(r"}\s+([0-9.eE+-]+)$|^\S+\s+([0-9.eE+-]+)$", line)
                if m:
                    total += float(m.group(1) or m.group(2))
    return total


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--namespace", default="hs-chaos")
    parser.add_argument("--assert-no-double-writes", action="store_true")
    parser.add_argument("--assert-failed-over-from")
    parser.add_argument("--before-owners")
    parser.add_argument("--dump-owners", action="store_true")
    parser.add_argument("--scrape-metric")
    args = parser.parse_args()

    if args.dump_owners:
        print(dump_owners(args.namespace))
        return 0

    if args.scrape_metric:
        print(scrape_metric(args.namespace, args.scrape_metric))
        return 0

    entries = collect_entries(args.namespace)
    failed = False

    if args.assert_no_double_writes:
        violations = check_no_double_writes(entries)
        if violations:
            failed = True
            print("FAIL: linearizability violations found:", file=sys.stderr)
            for v in violations:
                print(f"  - {v}", file=sys.stderr)
        else:
            print(f"OK: no linearizability violations across {len(entries)} committed entries")

    if args.assert_failed_over_from and args.before_owners:
        before = json.loads(args.before_owners)
        after = json.loads(dump_owners(args.namespace))
        still_owned = [
            shard
            for shard, writer in before.items()
            if writer == args.assert_failed_over_from and after.get(shard) == args.assert_failed_over_from
        ]
        if still_owned:
            failed = True
            print(f"FAIL: shards still attributed to killed pod {args.assert_failed_over_from}: {still_owned}", file=sys.stderr)
        else:
            print(f"OK: every shard {args.assert_failed_over_from} owned has a new writer")

    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
