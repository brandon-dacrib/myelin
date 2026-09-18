#!/usr/bin/env python3
"""Drives Synapse's own state resolution v2 implementation (`synapse.state.v2.resolve_events_with_store`)
as an oracle: given a small, self-contained room (a JSON fixture of events plus which state sets
conflict), compute Synapse's answer for "what does state resolution v2 produce here" and print it
as JSON. `hs-state` (track 02), once it exists, is meant to compute the same fixture through its
own `resolve` entry point and diff the two -- a clean diff across the fixture set is real evidence
the two algorithms agree, not just that each passes its own unit tests.

This is a *behavioral* oracle: it imports and calls Synapse's real code (`refs/synapse`, cloned by
tools/fetch-refs.sh) rather than reimplementing state resolution in Python, per this track's brief
("a Python harness that drives Synapse's own implementations"). Synapse is AGPL-3.0; nothing here
copies its algorithm, only calls it as a library the way any Synapse plugin or script would.

Status: **never executed end to end in this environment** -- `synapse` is not `pip install`-able
here (no network), so this script has only been checked for import-time and structural correctness
against `refs/synapse`'s current source, not run. Like `push_oracle.py`, this needs the actual
*installed* `matrix-synapse` package, not just the `refs/synapse` git checkout: as of this
checkout, even `synapse.api.room_versions` re-exports from `synapse.synapse_rust.room_versions`,
Synapse's compiled Rust extension, which only exists inside a built wheel -- a plain source clone
has no `.so`/`.pyd` for it. See README.md for setup, and for the known fragility (this reaches
into `synapse.state.v2`'s non-public `StateResolutionStore` protocol, an internal interface that
can change between Synapse releases without notice).

Usage:
    python3 tests/oracle/state_oracle.py tests/oracle/fixtures/state_res_ban_vs_power.json
"""

from __future__ import annotations

import asyncio
import json
import sys
from pathlib import Path


def _add_synapse_to_path() -> None:
    """Prefers an installed `synapse` package; falls back to adding the `refs/synapse` checkout
    to `sys.path` as a best effort. That fallback is not expected to actually work end to end --
    `synapse.api.room_versions` alone re-exports from the compiled `synapse.synapse_rust.*`
    extension, which a plain source checkout does not have -- but it costs nothing to try, and it
    at least gets the real `ModuleNotFoundError` (naming the missing compiled extension) instead
    of "no module named synapse" when only the checkout is present."""
    try:
        import synapse  # noqa: F401

        return
    except ModuleNotFoundError:
        pass
    checkout = Path(__file__).resolve().parent.parent.parent / "refs" / "synapse"
    if checkout.is_dir():
        sys.path.insert(0, str(checkout))


def _load_fixture(path: Path) -> dict:
    return json.loads(path.read_text())


async def _resolve(fixture: dict):
    from synapse.api.room_versions import KNOWN_ROOM_VERSIONS
    from synapse.events import make_event_from_dict
    from synapse.state import v2 as state_v2
    from synapse.util import Clock

    room_version = KNOWN_ROOM_VERSIONS[fixture["room_version"]]

    event_map = {}
    for raw_event in fixture["events"]:
        event = make_event_from_dict(raw_event, room_version)
        event_map[event.event_id] = event

    state_sets = [
        {tuple(k.split("|", 1)): v for k, v in state_set.items()}
        for state_set in fixture["state_sets"]
    ]

    store = _FixtureStateResolutionStore(event_map)

    class _FakeClock:
        async def sleep(self, _duration) -> None:
            return None

    resolved = await state_v2.resolve_events_with_store(
        clock=_FakeClock(),
        room_id=fixture.get("room_id", "!oracle:example.org"),
        room_version=room_version,
        state_sets=state_sets,
        event_map=event_map,
        state_res_store=store,
    )
    return {f"{k[0]}|{k[1]}": v for k, v in resolved.items()}


class _FixtureStateResolutionStore:
    """A `synapse.state.v2.StateResolutionStore` backed entirely by the fixture's in-memory event
    map -- no database, no lazy fetching. Adequate for small, hand-written fixtures where every
    event the resolution might need is already present; adapted in *shape* (not copied) from
    `refs/synapse/tests/state/test_v2.py`'s `TestStateResolutionStore`, which exists for exactly
    this purpose in Synapse's own test suite. The auth-chain-difference algorithm itself (each
    branch's full auth chain by DFS, then union-minus-common-intersection) is the Matrix spec's
    own definition of "auth chain difference" (room version 12 state resolution, section on
    "Auth Chain Difference"), not something specific to Synapse's implementation.
    """

    def __init__(self, event_map: dict):
        self._event_map = event_map

    async def get_events(self, event_ids, allow_rejected: bool = False):
        return {eid: self._event_map[eid] for eid in event_ids if eid in self._event_map}

    def _auth_chain(self, event_ids) -> set[str]:
        seen: set[str] = set()
        stack = list(event_ids)
        while stack:
            event_id = stack.pop()
            if event_id in seen or event_id not in self._event_map:
                continue
            seen.add(event_id)
            stack.extend(self._event_map[event_id].auth_event_ids())
        return seen

    async def get_auth_chain_difference(
        self, room_id, state_sets, conflicted_state, additional_backwards_reachable_conflicted_events
    ):
        from synapse.storage.databases.main.event_federation import StateDifference

        chains = [self._auth_chain(s) for s in state_sets]
        if not chains:
            return StateDifference(auth_difference=set(), conflicted_subgraph=set())
        common = set.intersection(*chains) if chains else set()
        union = set.union(*chains) if chains else set()
        return StateDifference(auth_difference=union - common, conflicted_subgraph=set())


def main() -> int:
    if len(sys.argv) != 2:
        print(f"usage: {sys.argv[0]} <fixture.json>", file=sys.stderr)
        return 2

    _add_synapse_to_path()
    try:
        import synapse  # noqa: F401
    except ModuleNotFoundError:
        print(
            "state_oracle.py: SKIP: the `synapse` package is not importable (checked an "
            "installed package and refs/synapse). See tests/oracle/README.md for setup. "
            "Exiting 0 (skip, not failure).",
            file=sys.stderr,
        )
        return 0

    fixture = _load_fixture(Path(sys.argv[1]))
    try:
        resolved = asyncio.run(_resolve(fixture))
    except ModuleNotFoundError as exc:
        print(
            f"state_oracle.py: SKIP: {exc}. A `synapse` module was importable but a submodule it "
            "needs (likely a compiled Rust extension) was not -- this usually means only the "
            "refs/synapse source checkout is on sys.path, not an installed package. See "
            "README.md. Exiting 0 (skip, not failure).",
            file=sys.stderr,
        )
        return 0
    except Exception as exc:  # noqa: BLE001 - this is a diagnostic tool, not a library
        print(
            f"state_oracle.py: FAILED calling Synapse's state resolution: {exc!r}\n"
            "This usually means refs/synapse has drifted from the internal API this script "
            "targets (synapse.state.v2.resolve_events_with_store / StateResolutionStore) -- see "
            "README.md.",
            file=sys.stderr,
        )
        return 1

    print(json.dumps(resolved, indent=2, sort_keys=True))
    return 0


if __name__ == "__main__":
    sys.exit(main())
