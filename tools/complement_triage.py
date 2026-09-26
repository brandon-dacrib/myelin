#!/usr/bin/env python3
"""Read a Complement log by named test rather than by total.

    go test -v ./tests/csapi/... > run.log
    python3 tools/complement_triage.py run.log                    # what is failing, and why
    python3 tools/complement_triage.py run.log --diff             # what moved since the baseline
    python3 tools/complement_triage.py run.log --write-baseline   # make this run the baseline
    python3 tools/complement_triage.py fed.log --suite=federation  # the same for the federation package

The baseline is `docs/status/complement-csapi-results.txt`: one line per top-level test.

Why this exists. The project's standing rule is that a change is worth something when it moves a
*named* test and that survives a re-run -- and for a long time no run kept the names, so the rule
could not be applied and progress was read off the total. The total hides things. On 2026-09-21
a run's total went up by three while one test went from PASS to FAIL, which was a real
regression (presence no longer reached existing members of a room when somebody joined); and two
tests that had been filed under "noise, they lose races under load" turned out to be failing
because `/sync` returned instantly, forever, which *was* the load.

What to look for in the triage besides the reasons:

* "Seen N /sync responses" with N in the thousands is a `/sync` that is not waiting. N around 6
  for a five-second wait is a long-poll doing its job and genuinely not getting the data.
* Several unrelated tests failing at `SendEventSynced` is one sync-delivery defect, not several
  test failures: the sender's own event never came back down their sync.
* Count clusters by test, not by log line. One polling test can print the same line 21 times.
"""

from __future__ import annotations

import collections
import pathlib
import re
import sys

ROOT = pathlib.Path(__file__).resolve().parent.parent
# One baseline per suite: `tests/csapi` by default, `tests` (the federation package) with
# `--suite=federation`. Kept apart because their test names do not overlap and their runs are
# taken separately, so a diff of one against the other's baseline would report everything as new.
BASELINES = {
    "csapi": ROOT / "docs/status/complement-csapi-results.txt",
    "federation": ROOT / "docs/status/complement-federation-results.txt",
}
SUITE_PATHS = {"csapi": "tests/csapi", "federation": "tests"}
BASELINE = BASELINES["csapi"]
ANSI = re.compile(r"\x1b\[[0-9;]*m")


def results(log: str) -> dict[str, str]:
    """`{test: PASS|FAIL|SKIP}` for every top-level test."""
    return {name: status for status, name in re.findall(r"^--- (PASS|FAIL|SKIP): (\S+)", log, re.M)}


def scrub(line: str) -> str:
    line = re.sub(r"![A-Za-z0-9_-]+:hs\d", "!ROOM", line)
    line = re.sub(r"\$[A-Za-z0-9_-]{20,}", "$EVENT", line)
    return re.sub(r"http://127\.0\.0\.1:\d+", "", line)


def triage(log: str) -> None:
    top = results(log)
    failing = [name for name, status in top.items() if status == "FAIL"]
    sub_fail = collections.Counter(m.split("/")[0] for m in re.findall(r"^\s+--- FAIL: (\S+)", log, re.M))
    sub_pass = collections.Counter(m.split("/")[0] for m in re.findall(r"^\s+--- PASS: (\S+)", log, re.M))

    reasons: dict[str, str] = {}
    for block in re.finditer(r"^=== NAME\s+(\S+)\n((?:    .*\n)+)", log, re.M):
        name = block.group(1).split("/")[0]
        if name not in failing or name in reasons:
            continue
        for line in block.group(2).splitlines():
            line = line.strip()
            if re.search(r"_test\.go:\d+:", line) and "WARNING" not in line and "Deploy times" not in line:
                reasons[name] = scrub(line)[:200]
                break

    every_pass = len(re.findall(r"^\s*--- PASS", log, re.M))
    every_fail = len(re.findall(r"^\s*--- FAIL", log, re.M))
    passed = sum(1 for s in top.values() if s == "PASS")
    print(f"{every_pass} of {every_pass + every_fail} assertions pass; "
          f"{passed} of {passed + len(failing)} top-level tests ({sum(1 for s in top.values() if s == 'SKIP')} skipped)\n")

    spins = sorted((int(n) for n in re.findall(r"Seen (\d+) /sync responses", log)), reverse=True)
    if spins and spins[0] > 100:
        print(f"!! a wait saw {spins[0]} /sync responses. If that is a MustSyncUntil, /sync is not waiting\n"
              "   and that comes first. (One test loops on purpose: TestSync's 'sync token points to a\n"
              "   redaction of an unknown event' re-syncs from a fixed token until its last event shows.)\n")

    for name in sorted(failing, key=lambda n: (-sub_fail[n], n)):
        print(f"{sub_fail[name]:3d} fail {sub_pass[name]:3d} pass  {name}")
        if name in reasons:
            print(f"          {reasons[name]}")


def diff(log: str) -> int:
    if not BASELINE.exists():
        print(f"no baseline at {BASELINE.relative_to(ROOT)}; run with --write-baseline first", file=sys.stderr)
        return 1
    before = dict(
        reversed(line.split(" ", 1)) for line in BASELINE.read_text().splitlines() if line and not line.startswith("#")
    )
    now = results(log)
    moved = [(name, before.get(name, "(new)"), status) for name, status in sorted(now.items()) if before.get(name) != status]
    gone = sorted(set(before) - set(now))
    if not moved and not gone:
        print("no named test moved")
        return 0
    for name, was, status in moved:
        flag = "  <-- regression" if was == "PASS" and status == "FAIL" else ""
        print(f"{was:>6} -> {status:<5} {name}{flag}")
    for name in gone:
        print(f"{before[name]:>6} -> (not run) {name}")
    return 0


def write_baseline(log: str, source: str, suite: str) -> None:
    top = results(log)
    every_pass = len(re.findall(r"^\s*--- PASS", log, re.M))
    every_fail = len(re.findall(r"^\s*--- FAIL", log, re.M))
    passed = sum(1 for s in top.values() if s == "PASS")
    failed = sum(1 for s in top.values() if s == "FAIL")
    header = [
        f"# Complement {SUITE_PATHS[suite]}, top-level results. One line per test so that two runs can be compared",
        "# by name: a change is worth something when it moves a *named* test and that survives a re-run.",
        "# Written and read by tools/complement_triage.py (--write-baseline, --diff).",
        f"# {source}: {every_pass} of {every_pass + every_fail} assertions, "
        f"{passed} of {passed + failed} top-level ({sum(1 for s in top.values() if s == 'SKIP')} skipped).",
    ]
    lines = [f"{status} {name}" for name, status in sorted(top.items())]
    BASELINE.write_text("\n".join(header + lines) + "\n")
    print(f"wrote {BASELINE.relative_to(ROOT)} ({len(lines)} tests)")


def main() -> int:
    global BASELINE
    args = [a for a in sys.argv[1:] if not a.startswith("--")]
    flags = {a for a in sys.argv[1:] if a.startswith("--")}
    if len(args) != 1:
        print(__doc__, file=sys.stderr)
        return 2
    suite = next((a.split("=", 1)[1] for a in flags if a.startswith("--suite=")), "csapi")
    if suite not in BASELINES:
        print(f"unknown suite {suite!r}; one of {', '.join(BASELINES)}", file=sys.stderr)
        return 2
    BASELINE = BASELINES[suite]
    log = ANSI.sub("", pathlib.Path(args[0]).read_text(errors="replace"))
    if "--write-baseline" in flags:
        note = next((a.split("=", 1)[1] for a in flags if a.startswith("--note=")), "Run")
        write_baseline(log, note, suite)
        return 0
    if "--diff" in flags:
        return diff(log)
    triage(log)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
