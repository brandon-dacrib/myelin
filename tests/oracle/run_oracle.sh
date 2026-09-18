#!/usr/bin/env bash
# Runs every oracle fixture through both oracle scripts. Each script skips cleanly (exit 0, a
# clear message) if Synapse is not importable, per this track's brief; this wrapper just saves
# typing the fixture paths out by hand and gives a single pass/fail/skip summary.
set -uo pipefail
cd "$(dirname "$0")"

status=0
for fixture in fixtures/state_res_*.json; do
  echo "== state_oracle.py $fixture =="
  python3 state_oracle.py "$fixture"
  rc=$?
  [ "$rc" -ne 0 ] && status=1
  echo
done

for fixture in fixtures/push_*.json; do
  echo "== push_oracle.py $fixture =="
  python3 push_oracle.py "$fixture"
  rc=$?
  [ "$rc" -ne 0 ] && status=1
  echo
done

exit "$status"
