"""Normalizers for nondeterministic fields in recorded Matrix HTTP responses.

Two servers processing the exact same scripted workload will not produce byte-identical
responses even when they are behaviorally equivalent: event IDs, room IDs, access tokens, and
timestamps are all randomly or locally generated, and some JSON arrays are semantically unordered
sets even though JSON itself is ordered. :class:`Normalizer` rewrites a decoded JSON value in
place (well: returns a rewritten copy) so that two independently-generated but equivalent
responses normalize to the same structure, ready for a plain `==` comparison
(`tests/differential/lib/diff.py`).

Two normalizers are used per diff run (one for the baseline, one for the candidate) so that
"the Nth distinct event ID seen" gets the same placeholder on both sides even though the
underlying strings differ completely -- this preserves *relationships* between IDs (the second
event referencing the first, "which event is this the reply to") rather than just fuzzing every
ID string to a single opaque marker.
"""

from __future__ import annotations

import re
from typing import Any

# Matches a JSON object key naming a point in time, however it is spelled across the Matrix APIs:
# `origin_server_ts`, `ts`, `retry_after_ms`'s sibling `expires_in_ms` is NOT a timestamp (it's a
# duration) so this deliberately only matches `*_ts`/`timestamp*`, not every `*_ms` key.
_TIMESTAMP_KEY_RE = re.compile(r"(^|_)(ts|timestamp)(s)?$", re.IGNORECASE)

# Opaque, per-session credentials and cursors: never comparable across two independent server
# instances even when both are "correct".
_TOKEN_KEY_RE = re.compile(
    r"^(access_token|refresh_token|next_batch|prev_batch|since|txn_id|nonce|session|"
    r"registration_session_id)$",
    re.IGNORECASE,
)

_EVENT_ID_RE = re.compile(r"^\$[A-Za-z0-9_+/=.\-:]+$")
_ROOM_ID_RE = re.compile(r"^![A-Za-z0-9_+/=.\-]+:\S+$")


class Normalizer:
    """Rewrites nondeterministic fields to stable placeholders. Stateful across calls to
    :meth:`normalize` within one instance, so reuse one instance across every response from the
    same server run (see module docstring) but never share an instance between two servers being
    compared."""

    def __init__(self) -> None:
        self._event_ids: dict[str, str] = {}
        self._room_ids: dict[str, str] = {}
        self._counters = {"event": 0, "room": 0}

    def normalize(self, value: Any) -> Any:
        """Returns a normalized copy of `value` (a JSON-decoded structure, or `None`)."""
        return self._walk(value, key=None)

    def _walk(self, value: Any, key: str | None) -> Any:
        if isinstance(value, dict):
            out = {}
            for k, v in value.items():
                if _TIMESTAMP_KEY_RE.search(k):
                    out[k] = "<TS>"
                elif _TOKEN_KEY_RE.match(k) and isinstance(v, str):
                    out[k] = "<TOKEN>"
                else:
                    out[k] = self._walk(v, k)
            return out
        if isinstance(value, list):
            walked = [self._walk(v, key) for v in value]
            if walked and all(_is_scalar(v) for v in walked):
                # Heuristic: a JSON array whose every element is a bare scalar (not an object) is
                # usually an unordered set in the Matrix APIs (login flow lists, capability
                # flags, ...); arrays of *objects* (an event timeline, a room list) are almost
                # always meaningfully ordered and are left alone.
                try:
                    return sorted(walked, key=lambda v: (str(type(v)), str(v)))
                except TypeError:
                    return walked
            return walked
        if isinstance(value, str):
            return self._normalize_string(value)
        return value

    def _normalize_string(self, value: str) -> str:
        if _EVENT_ID_RE.match(value):
            return self._placeholder(self._event_ids, "event", value)
        if _ROOM_ID_RE.match(value):
            return self._placeholder(self._room_ids, "room", value)
        return value

    def _placeholder(self, table: dict[str, str], kind: str, value: str) -> str:
        if value not in table:
            self._counters[kind] += 1
            table[value] = f"<{kind.upper()}_{self._counters[kind]}>"
        return table[value]


def _is_scalar(value: Any) -> bool:
    return value is None or isinstance(value, (str, int, float, bool))
