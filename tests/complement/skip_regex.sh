#!/usr/bin/env bash
# Turns blacklist.txt into a single `|`-joined regex suitable for `go test -skip`. Prints nothing
# (an empty string) if the blacklist has no active entries, which is a valid `-skip` value (skips
# nothing).
set -euo pipefail
cd "$(dirname "$0")"

grep -v '^\s*#' blacklist.txt | grep -v '^\s*$' | sed -e 's/\s*#.*$//' -e 's/[[:space:]]*$//' | paste -sd '|' -
