# Oracle harness

`docs/workstreams/14-test-and-conformance.md`'s "open questions to settle first" names "running
Synapse's Python oracle in CI... calling functions directly"; `PLAN.md` section 12 layer L5
describes "state and auth cross-checks through a Python harness that drives Synapse's own
implementations." This directory is that harness for two of the hardest-to-get-right pieces:
state resolution (`state_oracle.py`, driving `synapse.state.v2.resolve_events_with_store`) and
push rule evaluation (`push_oracle.py`, driving `synapse.synapse_rust.push.PushRuleEvaluator`).

**Status: never executed end to end.** This environment has no network access to `pip install
matrix-synapse`, and both oracles need the real installed package -- not just the `refs/synapse`
source checkout `tools/fetch-refs.sh` clones -- because Synapse has moved several pieces this
harness touches (room versions, push rule evaluation, and likely more) into a compiled Rust
extension (`synapse.synapse_rust.*`) that only exists inside a built wheel. Both scripts detect
this (`ModuleNotFoundError`) and exit 0 with an explanation rather than failing, matching
`tests/complement/` and `tests/differential/`'s Docker-off behavior. The fixtures are hand-written
and have never been checked against a real Synapse answer -- treat them as a starting point to
validate, not verified ground truth (each fixture file says so in its own `_comment` field).

## Setup (once network is available)

```bash
python3 -m venv /tmp/oracle-venv
/tmp/oracle-venv/bin/pip install matrix-synapse==1.161.0
/tmp/oracle-venv/bin/python3 tests/oracle/state_oracle.py tests/oracle/fixtures/state_res_topic_fork.json
/tmp/oracle-venv/bin/python3 tests/oracle/push_oracle.py tests/oracle/fixtures/push_contains_display_name.json
# or, for both at once:
/tmp/oracle-venv/bin/python3 tests/oracle/run_oracle.sh   # (invoke via the venv's python3 on the shebang line, or `source`/activate first)
```

## What each script does

| Script | Drives | Needs |
|---|---|---|
| `state_oracle.py` | `synapse.state.v2.resolve_events_with_store` against a small, fully self-contained room (a fixture's `events` + conflicting `state_sets`) | An installed `matrix-synapse` (its `synapse_rust.room_versions` extension) |
| `push_oracle.py` | `synapse.synapse_rust.push.PushRuleEvaluator` against one event, one rule set, and a little room context | An installed `matrix-synapse` (push evaluation is entirely in its Rust extension now) |

Both print the oracle's answer as JSON to stdout. Once `hs-state` (track 02) and `hs-push` (track
10) exist, the intended workflow is: compute the same fixture through their own resolution/
evaluation entry points, and diff the two JSON outputs -- a clean diff is the evidence "our
algorithm agrees with Synapse's" beyond what either side's own unit tests can show alone, the same
principle `tests/differential/` applies to whole HTTP responses.

## Known fragility

Both scripts reach into non-public, internal Synapse APIs (`StateResolutionStore`'s protocol
shape, `PushRuleEvaluator`'s and `FilteredPushRules`'s constructor argument order) that Synapse is
free to change between releases without notice -- these are not stable interfaces the way its HTTP
API is. If a script fails with a `TypeError` about a constructor signature or an `ImportError` for
a symbol that used to exist, that is expected maintenance, not a bug in the fixture: diff the
script's call site against the current `refs/synapse/synapse/state/v2.py` /
`refs/synapse/synapse/push/bulk_push_rule_evaluator.py` and update it to match. `tools/fetch-refs.sh`
clones Synapse's default branch with no version pin, so `refs/synapse`'s source may already be
ahead of the `matrix-synapse==1.161.0` this README's setup step installs; when in doubt, read the
*installed* package's source (`pip show -f matrix-synapse`), not `refs/synapse`.

## Adding a fixture

State resolution fixtures (`fixtures/state_res_*.json`): `room_version` (use `"2"`, event format
V1 with explicit `event_id`s, to keep the fixture decoupled from event-ID hash computation --
that's a separate, already-covered concern), `events` (every event either state set might
reference, including full `auth_events`/`prev_events` chains back to `m.room.create` -- state
resolution v2 runs auth checks during resolution and needs the real chain), and `state_sets` (a
list of `{"type|state_key": event_id}` maps, one per conflicting branch).

Push fixtures (`fixtures/push_*.json`): `event` (the message/state event being evaluated),
`user_id`/`display_name`/`room_member_count`/`sender_power_level`/`notification_power_levels` (the
room context `PushRuleEvaluator` needs), and `rules` (a list of push rule dicts in the shape
`m.push_rules`'s `/pushrules` endpoint returns).

## References

- `refs/synapse/synapse/state/v2.py` -- `resolve_events_with_store`, the `StateResolutionStore`
  protocol.
- `refs/synapse/tests/state/test_v2.py` -- `TestStateResolutionStore`, the shape
  `state_oracle.py`'s `_FixtureStateResolutionStore` is adapted from (its auth-chain-difference
  algorithm -- each branch's auth chain by DFS, then union minus common intersection -- is the
  Matrix spec's own definition of "auth chain difference," not Synapse-specific).
- `refs/synapse/synapse/push/bulk_push_rule_evaluator.py` -- the current `PushRuleEvaluator`/
  `FilteredPushRules` call site `push_oracle.py` mirrors.

All AGPL-3.0 (Synapse): read for API shape and behavior, never copied.
