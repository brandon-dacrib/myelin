#!/usr/bin/env bash
# Runs Complement against this server in single-node mode: one homeserver process per Complement
# blueprint, the default and by far most common Complement topology. Untested (needs
# build.sh, which needs a server binary that does not exist yet, and Docker).
#
# Usage: ./run_single_node.sh [-- <extra go test args>]
set -euo pipefail
cd "$(dirname "$0")"

IMAGE_TAG="${COMPLEMENT_BASE_IMAGE:-complement-hs-reimplement:dev}"
COMPLEMENT_DIR="${COMPLEMENT_DIR:-$(cd ../../refs/complement && pwd)}"

if ! command -v docker >/dev/null 2>&1 || ! docker info >/dev/null 2>&1; then
  echo "run_single_node.sh: SKIP: Docker is not available. See README.md for how to run this" >&2
  echo "once Docker is available." >&2
  exit 0
fi
if ! command -v go >/dev/null 2>&1; then
  echo "run_single_node.sh: SKIP: Go is not installed; Complement is a Go test suite." >&2
  exit 0
fi
if [ ! -d "$COMPLEMENT_DIR" ]; then
  echo "run_single_node.sh: SKIP: $COMPLEMENT_DIR not found; run tools/fetch-refs.sh (network required)." >&2
  exit 0
fi

./build.sh "$IMAGE_TAG"

SKIP_REGEX="$(./skip_regex.sh)"

cd "$COMPLEMENT_DIR"
export COMPLEMENT_BASE_IMAGE="$IMAGE_TAG"
GO_ARGS=(-v ./tests/...)
if [ -n "$SKIP_REGEX" ]; then
  GO_ARGS=(-skip "$SKIP_REGEX" "${GO_ARGS[@]}")
fi
if [ "${1:-}" = "--" ]; then
  shift
  GO_ARGS+=("$@")
fi

echo "run_single_node.sh: go test ${GO_ARGS[*]}" >&2
go test "${GO_ARGS[@]}"
