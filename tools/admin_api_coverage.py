#!/usr/bin/env python3
"""How much of the admin API is genuinely served, counted from the source rather than by hand.

`crates/hs-admin/openapi/operations.json` lists every operation the contract declares. Most are
registered with a generic handler that answers an honest 501; the ones with a real handler are
named in `REAL_HANDLERS` in `crates/hs-admin/src/router.rs`, plus the `public` operations, which
are always registered with their real handlers. This prints the ratio, and with `--list` the
operations on each side of it.

The figure had been quoted by hand in three documents and was different in each. "Served" here
means "has a real handler", which is necessary and not sufficient: `users.create` had one for
days while the only real user directory answered it with 503. Whether an operation *works* is
what the end-to-end tests in `crates/hs-cli/tests/e2e.rs` are for.

Usage:
    python3 tools/admin_api_coverage.py
    python3 tools/admin_api_coverage.py --list
"""

from __future__ import annotations

import json
import pathlib
import re
import sys

ROOT = pathlib.Path(__file__).resolve().parent.parent


def main() -> int:
    operations = json.loads(
        (ROOT / "crates/hs-admin/openapi/operations.json").read_text()
    )["operations"]
    router = (ROOT / "crates/hs-admin/src/router.rs").read_text()
    start = router.index("const REAL_HANDLERS: &[&str] = &[")
    block = router[start : router.index("];", start)]
    real = set(re.findall(r'"([a-z_.]+)"', block))

    declared = {op["operation_id"] for op in operations}
    public = {op["operation_id"] for op in operations if op["public"]}
    unknown = sorted(real - declared)
    if unknown:
        print(f"REAL_HANDLERS names operations the contract does not declare: {unknown}", file=sys.stderr)
        return 1

    served = (real | public) & declared
    print(f"{len(served)} of {len(operations)} admin API operations have a real handler "
          f"({100 * len(served) / len(operations):.1f}%); the rest answer 501.")
    if "--list" in sys.argv[1:]:
        by_tag: dict[str, list[str]] = {}
        for op in operations:
            mark = "x" if op["operation_id"] in served else " "
            by_tag.setdefault(op.get("tag") or "-", []).append(f"  [{mark}] {op['operation_id']}")
        for tag in sorted(by_tag):
            done = sum(1 for line in by_tag[tag] if line.startswith("  [x]"))
            print(f"\n{tag} ({done}/{len(by_tag[tag])})")
            print("\n".join(sorted(by_tag[tag])))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
