#!/usr/bin/env python3
"""Judge each phase of cluster-smoke.sh from the traffic pod's log.

Usage: analyze.py TRAFFIC_LOG PHASES_TSV

PHASES_TSV has one phase per line: name, start epoch, end epoch, rule, note (tab-separated).
A rule is one of:

  none       no request in the window may fail (decision 0017: a request that lands
             mid-handoff waits for the new owner instead of failing, so a graceful
             termination, a scale, a rolling upgrade and a rollback lose nothing)
  window:N   failures are allowed only in the first N seconds of the phase (a replica that is
             killed outright takes its shards with it until its lease lapses; the forwards
             to it are retried until their own deadline and then fail -- RFC 0001 section 4,
             decision 0017), and none after that

Prints one table row per phase (requests, failures, allowed, latency p50/p99/max, what the
failures were and when) and exits 1 if any phase breaks its rule. A failure is any request
whose status is not 200, including a transport error (status 0).
"""
import sys
from collections import Counter


def percentile(values, p):
    if not values:
        return 0.0
    values = sorted(values)
    k = (len(values) - 1) * p
    lo, hi = int(k), min(int(k) + 1, len(values) - 1)
    return values[lo] + (values[hi] - values[lo]) * (k - lo)


def main():
    log_path, phases_path = sys.argv[1], sys.argv[2]
    records = []
    with open(log_path) as f:
        for line in f:
            parts = line.split()
            if len(parts) >= 5 and parts[0] == "R":
                t, kind, status, ms = float(parts[1]), parts[2], int(parts[3]), float(parts[4])
                reason = parts[5] if len(parts) > 5 else ""
                records.append((t, kind, status, ms, reason))
    phases = []
    with open(phases_path) as f:
        for line in f:
            if not line.strip():
                continue
            name, start, end, rule, note = (line.rstrip("\n").split("\t") + [""] * 5)[:5]
            phases.append((name, float(start), float(end), rule, note))

    print(f"{len(records)} requests in the log, {len(phases)} phases")
    print()
    print(f"{'phase':<18} {'reqs':>6} {'fail':>5} {'allowed':>8} {'p50 ms':>7} {'p99 ms':>7} {'max ms':>7}  verdict")
    broken = []
    for name, start, end, rule, note in phases:
        rows = [r for r in records if start <= r[0] <= end]
        fails = [r for r in rows if r[2] != 200]
        lat = [r[3] for r in rows]
        if rule == "none":
            outside = fails
            allowed = 0
        elif rule.startswith("window:"):
            w = float(rule.split(":", 1)[1])
            outside = [r for r in fails if r[0] > start + w]
            allowed = len(fails) - len(outside)
        else:
            raise SystemExit(f"unknown rule {rule!r} for phase {name}")
        verdict = "ok" if not outside and rows else ("NO TRAFFIC" if not rows else "BROKEN")
        if verdict != "ok":
            broken.append(name)
        print(f"{name:<18} {len(rows):>6} {len(fails):>5} {allowed:>8} {percentile(lat, 0.5):>7.0f} "
              f"{percentile(lat, 0.99):>7.0f} {max(lat) if lat else 0:>7.0f}  {verdict}")
        if fails:
            kinds = Counter(f"{r[1]}:{r[2]}:{r[4] or '-'}" for r in fails)
            first, last = fails[0][0] - start, fails[-1][0] - start
            print(f"{'':<18} failures from t+{first:.1f}s to t+{last:.1f}s: "
                  + ", ".join(f"{k} x{n}" for k, n in kinds.most_common(6)))
        slow = [r for r in rows if r[2] == 200 and r[3] >= 1000]
        if slow:
            print(f"{'':<18} {len(slow)} requests took 1 s or more (served, not failed; "
                  f"the longest {max(r[3] for r in slow):.0f} ms at t+{max(slow, key=lambda r: r[3])[0] - start:.1f}s)")
        if note:
            print(f"{'':<18} {note}")
    print()
    if broken:
        print("BROKEN: " + ", ".join(broken))
        return 1
    print("every phase kept its rule")
    return 0


if __name__ == "__main__":
    sys.exit(main())
