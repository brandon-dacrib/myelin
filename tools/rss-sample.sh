#!/usr/bin/env bash
# Samples one process's resident set size over time, for finding a memory leak.
#
#   tools/rss-sample.sh <pid> [interval_seconds=30] [samples=40] [outfile]
#
# Writes one line per sample, "<unix_time> <elapsed_s> <rss_kib> <vsz_kib>", to stdout (and to
# <outfile> when given), using `ps` so it works on Linux and macOS alike. Ends early when the
# process exits. The last line is a summary: first and last RSS and the slope in KiB per hour.
#
# Used for docs/status/06-federation.md's 2026-10-10 entry (the idle creep and the
# unresolvable-destination soak of `hs serve`); pair it with `cargo test -p hs-federation
# --test sender_soak -- --ignored` for the in-process sender soak.
set -euo pipefail

pid="${1:?pid}"
interval="${2:-30}"
samples="${3:-40}"
outfile="${4:-}"

emit() {
  if [ -n "$outfile" ]; then
    echo "$*" | tee -a "$outfile"
  else
    echo "$*"
  fi
}

start=$(date +%s)
first_rss=""
last_rss=""
last_elapsed=0
for ((i = 0; i < samples; i++)); do
  if ! line=$(ps -o rss=,vsz= -p "$pid" 2>/dev/null) || [ -z "$line" ]; then
    emit "# process $pid exited"
    break
  fi
  now=$(date +%s)
  elapsed=$((now - start))
  rss=$(echo "$line" | awk '{print $1}')
  vsz=$(echo "$line" | awk '{print $2}')
  emit "$now $elapsed $rss $vsz"
  [ -z "$first_rss" ] && first_rss="$rss"
  last_rss="$rss"
  last_elapsed="$elapsed"
  if ((i + 1 < samples)); then
    sleep "$interval"
  fi
done
if [ -n "$first_rss" ] && [ "$last_elapsed" -gt 0 ]; then
  slope=$(awk -v a="$first_rss" -v b="$last_rss" -v t="$last_elapsed" 'BEGIN { printf "%.0f", (b - a) * 3600 / t }')
  emit "# rss ${first_rss} -> ${last_rss} KiB over ${last_elapsed}s: ${slope} KiB/h"
fi
