#!/usr/bin/env bash
# Builds and runs every cargo-fuzz target in the workspace, one after another, each for a bounded
# time, and prints one line of results per target.
#
#   tests/fuzz/run_all.sh [SECONDS_PER_TARGET] [TARGET_FILTER_REGEX]
#
# SECONDS_PER_TARGET defaults to 60 (what CI runs); the 2026-10-01 baseline in
# docs/status/14-test-and-conformance.md was 600. Needs a nightly toolchain and cargo-fuzz:
#   rustup toolchain install nightly --profile minimal && cargo install cargo-fuzz --locked
#
# The fuzz crates are their own workspaces (crates/hs-federation/fuzz, crates/hs-media/fuzz) with
# their own target/ directories. Each run starts from the checked-in seed corpus
# (crates/<crate>/fuzz/corpus/<target>, read-only here) plus a scratch corpus under $FUZZ_OUT,
# so new inputs never land in the repository. One process per target (no -jobs/-workers): the
# machine is shared. A crash leaves its input in crates/<crate>/fuzz/artifacts/<target>/ and makes
# this script exit 1 after the remaining targets have run.
#
# Environment: FUZZ_OUT (default target/fuzz-runs/<UTC timestamp>), FUZZ_TOOLCHAIN (default
# nightly), FUZZ_RSS_LIMIT_MB (default 2048), FUZZ_REQUIRE=1 (fail instead of skipping when the
# toolchain is missing).
set -uo pipefail
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
SECONDS_PER_TARGET="${1:-60}"
FILTER="${2:-.}"
TOOLCHAIN="${FUZZ_TOOLCHAIN:-nightly}"
OUT="${FUZZ_OUT:-$ROOT/target/fuzz-runs/$(date -u +%Y%m%dT%H%M%SZ)}"
RSS="${FUZZ_RSS_LIMIT_MB:-2048}"
export CARGO_PROFILE_RELEASE_DEBUG="${CARGO_PROFILE_RELEASE_DEBUG:-0}"

if ! cargo "+$TOOLCHAIN" fuzz --version >/dev/null 2>&1; then
  echo "run_all.sh: SKIP: cargo +$TOOLCHAIN fuzz is not available (see this script's header)." >&2
  # CI sets FUZZ_REQUIRE=1 so a failed toolchain install is a failure, not a quiet pass.
  [ "${FUZZ_REQUIRE:-0}" = 1 ] && exit 1
  exit 0
fi

HOST="$(rustc "+$TOOLCHAIN" -vV | sed -n 's/^host: //p')"
mkdir -p "$OUT"
SUMMARY="$OUT/summary.tsv"
printf 'crate\ttarget\tseconds\texecs\texecs_per_s\tcov\tft\tcorpus\tnew_units\tpeak_rss_mb\tresult\n' >"$SUMMARY"
FAILED=0

for fuzz_dir in "$ROOT"/crates/*/fuzz; do
  [ -f "$fuzz_dir/Cargo.toml" ] || continue
  crate="$(basename "$(dirname "$fuzz_dir")")"
  echo "run_all.sh: building $crate's fuzz targets" >&2
  mkdir -p "$OUT/$crate"
  build_log="$OUT/$crate/build.log"
  # The whole build log is kept; on failure every `error` block is printed, not the last three
  # lines (which, on 2026-10-01, said only "could not compile `cfg-if` due to 2 previous errors").
  if (cd "$fuzz_dir" && cargo "+$TOOLCHAIN" fuzz build >"$build_log" 2>&1); then
    tail -1 "$build_log" >&2
  else
    echo "run_all.sh: $crate: build failed; full log in $build_log" >&2
    sed 's/\x1b\[[0-9;]*m//g' "$build_log" | grep -E -A12 '^error' | head -80 >&2
    FAILED=1
    continue
  fi
  for target in $(cd "$fuzz_dir" && cargo "+$TOOLCHAIN" fuzz list); do
    echo "$target" | grep -Eq "$FILTER" || continue
    work="$OUT/$crate/$target"
    mkdir -p "$work/corpus"
    seeds="$fuzz_dir/corpus/$target"
    log="$work/fuzz.log"
    echo "run_all.sh: $crate/$target for ${SECONDS_PER_TARGET}s" >&2
    # The binary `cargo fuzz build` made, run directly with the arguments `cargo fuzz run` would
    # pass. `cargo fuzz run` re-invokes cargo, and hs-admin's build script reruns on every
    # invocation while web/dist is absent (it watches that missing path), which recompiled
    # hs-admin and everything above it -- about five minutes -- before each federation target.
    mkdir -p "$fuzz_dir/artifacts/$target"
    "$fuzz_dir/target/$HOST/release/$target" \
      -artifact_prefix="$fuzz_dir/artifacts/$target/" \
      -max_total_time="$SECONDS_PER_TARGET" -rss_limit_mb="$RSS" -print_final_stats=1 \
      "$work/corpus" "$seeds" >"$log" 2>&1
    status=$?
    execs="$(grep -E '^stat::number_of_executed_units:' "$log" | awk '{print $2}' | tail -1)"
    rate="$(grep -E '^stat::average_exec_per_sec:' "$log" | awk '{print $2}' | tail -1)"
    new_units="$(grep -E '^stat::new_units_added:' "$log" | awk '{print $2}' | tail -1)"
    rss="$(grep -E '^stat::peak_rss_mb:' "$log" | awk '{print $2}' | tail -1)"
    last="$(grep -E '^#[0-9]+' "$log" | tail -1)"
    cov="$(echo "$last" | sed -nE 's/.* cov: ([0-9]+).*/\1/p')"
    ft="$(echo "$last" | sed -nE 's/.* ft: ([0-9]+).*/\1/p')"
    corp="$(echo "$last" | sed -nE 's/.* corp: ([0-9]+)\/.*/\1/p')"
    if [ "$status" -eq 0 ]; then
      result=ok
    else
      result="crash(exit $status)"
      FAILED=1
      grep -E 'ERROR: libFuzzer|panicked at|SUMMARY:|Test unit written to' "$log" | head -5 >&2
    fi
    printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n' "$crate" "$target" "$SECONDS_PER_TARGET" \
      "${execs:-?}" "${rate:-?}" "${cov:-?}" "${ft:-?}" "${corp:-?}" "${new_units:-?}" "${rss:-?}" \
      "$result" | tee -a "$SUMMARY"
  done
done

echo "run_all.sh: results in $SUMMARY" >&2
exit $FAILED
