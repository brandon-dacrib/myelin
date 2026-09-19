#!/usr/bin/env bash
# Turns blacklist.txt into a single `|`-joined regex suitable for `go test -skip`. Prints nothing
# (an empty string) if the blacklist has no active entries, which is a valid `-skip` value (skips
# nothing).
set -euo pipefail
cd "$(dirname "$0")"

# Bug fixed 2026-09-18: under `pipefail`, `grep -v` returning no matching lines (exit 1) used to
# fail this whole script even on the documented, intended "blacklist is empty" case, which in turn
# aborted `run_single_node.sh` (`SKIP_REGEX="$(./skip_regex.sh)"` with `set -e`) before it ever
# built the image. `grep ... || true` on the last filtering stage keeps that case's exit 0 without
# hiding a real failure in `sed`/`paste`.
{ grep -v '^\s*#' blacklist.txt || true; } | { grep -v '^\s*$' || true; } | sed -e 's/\s*#.*$//' -e 's/[[:space:]]*$//' | paste -sd '|' -
