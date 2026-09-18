#!/usr/bin/env python3
"""Drives Synapse's own push rule evaluator as an oracle: given an event, a set of push rules, and
a little room context (power levels, member count), compute which rules Synapse's evaluator fires
and print the resulting actions as JSON. `hs-push` (track 10), once it exists, is meant to compute
the same fixture and diff the two.

Unlike `state_oracle.py`, this **requires an installed `synapse` package**, not just the
`refs/synapse` source checkout: push rule evaluation moved into Synapse's compiled Rust extension
(`synapse.synapse_rust.push.PushRuleEvaluator` -- see `refs/synapse/synapse/push/
bulk_push_rule_evaluator.py`'s import), which only exists as a built artifact inside the installed
package (a git clone has no compiled `.so`/`.pyd`). Install with `pip install matrix-synapse`
(network required) to use this.

Status: **never executed end to end in this environment** (no network to `pip install`). The call
site below mirrors `bulk_push_rule_evaluator.py`'s current positional-argument construction of
`PushRuleEvaluator`, which is a non-public, internal API that Synapse is free to change between
releases without notice -- if this script fails with a `TypeError` about the constructor
signature, that is the first thing to check (diff this script's `PushRuleEvaluator(...)` call
against the current `refs/synapse/synapse/push/bulk_push_rule_evaluator.py`).

Usage:
    python3 tests/oracle/push_oracle.py tests/oracle/fixtures/push_contains_display_name.json
"""

from __future__ import annotations

import json
import sys
from pathlib import Path


def _load_fixture(path: Path) -> dict:
    return json.loads(path.read_text())


def _evaluate(fixture: dict) -> list:
    from synapse.api.room_versions import KNOWN_ROOM_VERSIONS
    from synapse.events import make_event_from_dict
    from synapse.push.bulk_push_rule_evaluator import _flatten_dict
    from synapse.synapse_rust.push import FilteredPushRules, PushRuleEvaluator

    room_version = KNOWN_ROOM_VERSIONS[fixture.get("room_version", "12")]
    event = make_event_from_dict(fixture["event"], room_version)

    flattened = _flatten_dict(event)

    evaluator = PushRuleEvaluator(
        flattened,
        fixture.get("has_mentions", False),
        fixture.get("room_member_count", 2),
        fixture.get("sender_power_level", 0),
        fixture.get("notification_power_levels", {"room": 50}),
        fixture.get("related_events", {}),
        fixture.get("related_event_match_enabled", True),
        room_version.msc3931_push_features,
        fixture.get("msc1767_enabled", False),
        fixture.get("msc4210_enabled", False),
        fixture.get("msc4306_enabled", False),
    )

    filtered_rules = FilteredPushRules(
        fixture["rules"],
        {},  # enabled_map: empty means every rule is at its default enabled state
        msc3664_enabled=fixture.get("msc3664_enabled", False),
        msc4028_push_encrypted_events=fixture.get("msc4028_enabled", False),
        msc4210_enabled=fixture.get("msc4210_enabled", False),
        msc4306_enabled=fixture.get("msc4306_enabled", False),
    )

    results = []
    for rule, enabled in filtered_rules.rules():
        if not enabled:
            continue
        actions = evaluator.run(rule, fixture.get("user_id"), fixture.get("display_name"))
        if actions:
            results.append({"rule_id": rule["rule_id"], "actions": actions})
    return results


def main() -> int:
    if len(sys.argv) != 2:
        print(f"usage: {sys.argv[0]} <fixture.json>", file=sys.stderr)
        return 2

    try:
        import synapse  # noqa: F401
        import synapse.synapse_rust.push  # noqa: F401
    except ModuleNotFoundError as exc:
        print(
            f"push_oracle.py: SKIP: {exc}. This oracle needs an installed `matrix-synapse` "
            "package (its compiled Rust extension, not just the refs/synapse source checkout) "
            "-- see this file's module docstring and README.md. Exiting 0 (skip, not failure).",
            file=sys.stderr,
        )
        return 0

    fixture = _load_fixture(Path(sys.argv[1]))
    try:
        fired = _evaluate(fixture)
    except Exception as exc:  # noqa: BLE001 - diagnostic tool
        print(
            f"push_oracle.py: FAILED calling Synapse's push rule evaluator: {exc!r}\n"
            "This usually means the installed Synapse version's PushRuleEvaluator/FilteredPushRules "
            "constructor has changed shape -- see this file's module docstring.",
            file=sys.stderr,
        )
        return 1

    print(json.dumps(fired, indent=2, sort_keys=True))
    return 0


if __name__ == "__main__":
    sys.exit(main())
