#!/usr/bin/env bash
# Drives the PLAN.md section 6.3 state-representation bake-off: runs `bin/bakeoff` once per
# (candidate, backend, scenario) combination, each in its own process (so `/usr/bin/time -l`'s
# "maximum resident set size" reading is isolated per run, per
# docs/decisions/0005-state-bakeoff-methodology.md's "resident memory" measurement method), and
# writes one JSON-lines file with every result plus its RSS reading.
#
# Usage: crates/hs-state/corpus/run_bakeoff.sh [output_file] [small_rooms_count]
# Requires a release build (`cargo build -p hs-state --release --bin bakeoff`) -- this script
# builds it if missing.

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)"
BIN="$REPO_ROOT/target/release/bakeoff"
OUT="${1:-$REPO_ROOT/crates/hs-state/corpus/results/bakeoff-results.jsonl}"
SMALL_ROOMS="${2:-200}"

mkdir -p "$(dirname "$OUT")"
: > "$OUT"

if [ ! -x "$BIN" ]; then
  echo "building bakeoff --release..." >&2
  (cd "$REPO_ROOT" && cargo build -p hs-state --release --bin bakeoff)
fi

CANDIDATES=(a b c)
SCENARIOS=(large_churn support_churn policy fork_backfill "small_rooms:${SMALL_ROOMS}")

run_one() {
  local candidate="$1" backend="$2" scenario="$3" fjall_dir="${4:-}"
  local line rss
  local tmp_err
  tmp_err="$(mktemp)"
  if [ -n "$fjall_dir" ]; then
    rm -rf "$fjall_dir"
    line="$(/usr/bin/time -l "$BIN" "$candidate" "$backend" "$scenario" "$fjall_dir" 2>"$tmp_err")"
  else
    line="$(/usr/bin/time -l "$BIN" "$candidate" "$backend" "$scenario" 2>"$tmp_err")"
  fi
  rss="$(grep 'maximum resident set size' "$tmp_err" | awk '{print $1}')"
  rm -f "$tmp_err"
  # Splice rss_bytes into the JSON object (it ends in `}`).
  echo "${line%\}}, \"rss_bytes\": ${rss:-null}}" >> "$OUT"
}

echo "running memory backend..." >&2
for c in "${CANDIDATES[@]}"; do
  for s in "${SCENARIOS[@]}"; do
    echo "  $c memory $s" >&2
    run_one "$c" memory "$s"
  done
done

echo "running fjall backend..." >&2
for c in "${CANDIDATES[@]}"; do
  for s in "${SCENARIOS[@]}"; do
    echo "  $c fjall $s" >&2
    dir="$(mktemp -d)/fjall-$c-${s//:/_}"
    run_one "$c" fjall "$s" "$dir"
    rm -rf "$dir"
  done
done

echo "done: $OUT" >&2
