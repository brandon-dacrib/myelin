#!/usr/bin/env bash
# Prints what a GitHub-hosted runner has left: memory, swap, disk and the size of target/.
#
#   runner-resources.sh            one snapshot (for a step before the build, or `if: failure()`)
#   runner-resources.sh --watch N  one line every N seconds until killed
#
# Why --watch: when a hosted runner's VM runs out of memory the runner service itself is stopped,
# the step ends with "The runner has received a shutdown signal" and exit code 143, and no later
# step (not even `if: failure()`) runs. A line printed from inside the step every few seconds is in
# the log up to the moment the VM went, so the last one says what ran out. That is how the arm64
# test kills of 2026-10-15 were found to be memory: four GNU `ld` processes linking hs-cli's test
# binaries at once (docs/status/12-platform-and-kubernetes.md).
set -u

snapshot() {
  echo "::group::runner resources $(date -u +%H:%M:%S)"
  echo "nproc: $(nproc)"
  free -m
  swapon --show 2>/dev/null || true
  df -h / "${GITHUB_WORKSPACE:-.}" 2>/dev/null | sort -u
  if [ -d target ]; then du -sh target 2>/dev/null; fi
  echo "largest processes by resident memory:"
  ps -eo rss=,comm= --sort=-rss | head -8 | awk '{printf "  %6d MiB %s\n", $1/1024, $2}'
  if command -v dmesg >/dev/null; then
    sudo -n dmesg 2>/dev/null | grep -iE 'out of memory|oom|killed process|no space' | tail -5 || true
  fi
  echo "::endgroup::"
}

line() {
  # One line, so a watch every few seconds does not bury the build's own output.
  local mem disk tgt top
  mem=$(free -m | awk '/^Mem:/ {printf "mem used %d/%d MiB avail %d", $3, $2, $7} /^Swap:/ {printf " swap %d/%d", $3, $2}')
  disk=$(df -m "${GITHUB_WORKSPACE:-.}" | awk 'NR==2 {printf "disk used %d/%d MiB avail %d", $3, $2, $4}')
  tgt=""
  if [ -d target ]; then tgt=" target $(du -sm target 2>/dev/null | cut -f1) MiB"; fi
  top=$(ps -eo rss=,comm= --sort=-rss | head -4 | awk '{printf " %s %dM", $2, $1/1024}')
  echo "[resources $(date -u +%H:%M:%S)] ${mem}; ${disk};${tgt}; rustc $(pgrep -c -x rustc || true) ld $(pgrep -c -x ld || true) lld $(pgrep -c -f 'rust-lld|ld[.]lld' || true); top:${top}"
}

if [ "${1:-}" = "--watch" ]; then
  interval="${2:-30}"
  while true; do
    line
    sleep "$interval"
  done
else
  snapshot
fi
