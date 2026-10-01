#!/usr/bin/env python3
"""Turns a Sytest TAP file into a per-test result list and a summary.

    summarize.py results.tap [--results results.txt] [--summary summary.txt] [--top 10]

results.txt has one line per top-level test, in run order: ``PASS``, ``FAIL``, ``SKIP`` or
``XFAIL`` (failed, but Sytest marks it expected to fail), a space, then the test's name exactly as
Sytest prints it (the names ``are-we-synapse-yet.list`` keys on). summary.txt has the counts and
the most common failure reasons: the first line of each failure's message, with room, user and
event IDs, ports and other numbers replaced by placeholders so the same failure in different
rooms counts once.

Reads the TAP format Sytest's lib/SyTest/Output/TAP.pm writes: ``ok N name``,
``ok N name # skip reason``, ``not ok N name`` followed by ``# Started:``, ``# Ended:`` and
``# <failure>`` lines, ``not ok N (expected fail) name # TODO expected fail``; indented lines
are sub-steps of a multi-step test and are not counted.
"""

from __future__ import annotations

import argparse
import collections
import re
import sys

TEST_LINE = re.compile(r"^(not ok|ok) (\d+) (.*)$")
SUBTESTS = re.compile(r" \(\d+ subtests\)$")


def normalise_reason(line: str) -> str:
    """Collapses the parts of a failure message that differ between occurrences."""
    s = line.strip()
    s = re.sub(r"![A-Za-z0-9._=-]+:[A-Za-z0-9.-]+(:\d+)?", "!ROOM", s)
    s = re.sub(r"@[A-Za-z0-9._=/+-]+:[A-Za-z0-9.-]+(:\d+)?", "@USER", s)
    s = re.sub(r"#[A-Za-z0-9._=-]+:[A-Za-z0-9.-]+(:\d+)?", "#ALIAS", s)
    s = re.sub(r"\$[A-Za-z0-9_+/=-]{8,}(:[A-Za-z0-9.-]+(:\d+)?)?", "$EVENT", s)
    s = re.sub(r"localhost:\d+", "localhost:PORT", s)
    s = re.sub(r"\b[A-Za-z0-9_-]{20,}\b", "TOKEN", s)
    s = re.sub(r"\d+", "N", s)
    s = re.sub(r"\s+", " ", s)
    return s[:200]


def parse(tap_lines):
    """Yields (status, name, reason) per top-level test."""
    current = None  # [status, name, reason-or-None]
    for raw in tap_lines:
        line = raw.rstrip("\n")
        if line.startswith(" ") or line.startswith("\t"):
            continue
        m = TEST_LINE.match(line)
        if m:
            if current:
                yield tuple(current)
            ok, _num, rest = m.groups()
            if ok == "ok":
                if " # skip " in rest:
                    name, _, reason = rest.partition(" # skip ")
                    current = ["SKIP", name, reason.strip()]
                else:
                    name = rest.split(" # TODO ")[0]
                    name = name.removeprefix("(expected fail) ")
                    current = ["PASS", SUBTESTS.sub("", name), None]
            else:
                if rest.startswith("(expected fail) "):
                    name = rest.removeprefix("(expected fail) ").split(" # TODO ")[0]
                    current = ["XFAIL", name, None]
                else:
                    current = ["FAIL", rest, None]
            continue
        if current and current[0] in ("FAIL", "XFAIL") and line.startswith("# "):
            body = line[2:]
            if body.startswith("Started:") or body.startswith("Ended:"):
                continue
            if current[2] is None and body.strip():
                current[2] = body.strip()
    if current:
        yield tuple(current)


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("tap")
    ap.add_argument("--results", help="write the per-test list here (default stdout)")
    ap.add_argument("--summary", help="write the summary here (default stderr)")
    ap.add_argument("--top", type=int, default=10, help="failure reasons to list")
    args = ap.parse_args()

    with open(args.tap, encoding="utf-8", errors="replace") as f:
        tests = list(parse(f))

    out = open(args.results, "w", encoding="utf-8") if args.results else sys.stdout
    for status, name, _ in tests:
        out.write(f"{status} {name}\n")
    if args.results:
        out.close()

    counts = collections.Counter(status for status, _, _ in tests)
    reasons = collections.Counter(
        normalise_reason(reason or "(no message)") for status, _, reason in tests if status == "FAIL"
    )
    skip_reasons = collections.Counter(reason or "" for status, _, reason in tests if status == "SKIP")
    total = len(tests)
    lines = [
        f"tests: {total}",
        f"pass: {counts['PASS']}",
        f"fail: {counts['FAIL']}",
        f"expected fail (Sytest's own marking): {counts['XFAIL']}",
        f"skip: {counts['SKIP']}",
    ]
    if total:
        lines.append(f"pass rate (of tests run, skips excluded): "
                     f"{100 * counts['PASS'] / max(1, total - counts['SKIP']):.1f}%")
    lines.append("")
    lines.append(f"most common failure reasons (first line of the message, normalised):")
    for reason, n in reasons.most_common(args.top):
        lines.append(f"  {n:4d}  {reason}")
    lines.append("")
    lines.append("skip reasons:")
    for reason, n in skip_reasons.most_common():
        lines.append(f"  {n:4d}  {reason}")
    text = "\n".join(lines) + "\n"
    if args.summary:
        with open(args.summary, "w", encoding="utf-8") as f:
            f.write(text)
    else:
        sys.stderr.write(text)
    return 0


if __name__ == "__main__":
    sys.exit(main())
