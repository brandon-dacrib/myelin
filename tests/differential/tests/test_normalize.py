"""Unit tests for lib/normalize.py. Run with:
    python3 -m unittest discover -s tests/differential/tests -v
(from the repository root), or `python3 tests/differential/tests/test_normalize.py`.
"""

from __future__ import annotations

import sys
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).parent.parent / "lib"))
from normalize import Normalizer  # noqa: E402


class NormalizeTests(unittest.TestCase):
    def test_timestamp_keys_are_blanked(self):
        n = Normalizer()
        out = n.normalize({"origin_server_ts": 1737000000123, "content": {"body": "hi"}})
        self.assertEqual(out["origin_server_ts"], "<TS>")
        self.assertEqual(out["content"]["body"], "hi")

    def test_token_keys_are_blanked(self):
        n = Normalizer()
        out = n.normalize({"access_token": "syt_abc123", "refresh_token": "syr_xyz", "device_id": "ABCDEF"})
        self.assertEqual(out["access_token"], "<TOKEN>")
        self.assertEqual(out["refresh_token"], "<TOKEN>")
        # device_id is not in the token key list: it is a real, meaningful identifier that
        # workloads often echo back and compare deliberately.
        self.assertEqual(out["device_id"], "ABCDEF")

    def test_event_ids_get_stable_placeholders_within_one_normalizer(self):
        n = Normalizer()
        first = n.normalize({"event_id": "$abc123:example.org"})
        second = n.normalize({"replaces": "$abc123:example.org", "event_id": "$def456:example.org"})
        self.assertEqual(first["event_id"], "<EVENT_1>")
        # The same underlying event ID seen again gets the same placeholder, preserving the
        # relationship between the two responses.
        self.assertEqual(second["replaces"], "<EVENT_1>")
        self.assertEqual(second["event_id"], "<EVENT_2>")

    def test_room_ids_get_stable_placeholders(self):
        n = Normalizer()
        out = n.normalize({"room_id": "!abcdef:example.org"})
        self.assertEqual(out["room_id"], "<ROOM_1>")

    def test_two_independent_normalizers_agree_on_structurally_equivalent_input(self):
        # This is the property the whole harness depends on: two different servers minting two
        # different event IDs for "the same" event (first event created in this run) normalize to
        # the same placeholder when compared with one Normalizer instance per side.
        baseline = Normalizer().normalize(
            {"event_id": "$AAAA:synapse.example.org", "origin_server_ts": 111, "access_token": "syt_1"}
        )
        candidate = Normalizer().normalize(
            {"event_id": "$totally-different-id:ours.example.org", "origin_server_ts": 999, "access_token": "hs_2"}
        )
        self.assertEqual(baseline, candidate)

    def test_arrays_of_scalars_are_treated_as_unordered_sets(self):
        n = Normalizer()
        out = n.normalize({"flows": [{"type": "b"}, {"type": "a"}], "methods": ["reauth", "cancel"]})
        # Objects: order preserved (not a set of scalars).
        self.assertEqual(out["flows"], [{"type": "b"}, {"type": "a"}])
        # Scalars: sorted, so two servers advertising the same set in different orders match.
        self.assertEqual(out["methods"], ["cancel", "reauth"])

    def test_arrays_of_objects_are_left_in_order(self):
        n = Normalizer()
        events = [{"type": "m.room.create"}, {"type": "m.room.member"}]
        out = n.normalize({"events": events})
        self.assertEqual(out["events"], events)

    def test_non_matching_strings_pass_through_unchanged(self):
        n = Normalizer()
        out = n.normalize({"body": "hello world", "msgtype": "m.text"})
        self.assertEqual(out["body"], "hello world")
        self.assertEqual(out["msgtype"], "m.text")

    def test_none_and_scalars_pass_through(self):
        n = Normalizer()
        self.assertIsNone(n.normalize(None))
        self.assertEqual(n.normalize(42), 42)
        self.assertEqual(n.normalize(True), True)


if __name__ == "__main__":
    unittest.main()
