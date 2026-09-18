#!/usr/bin/env bash
# Builds the Complement image from the repository root. Untested (see Dockerfile.template's
# header): will fail until an `hs-server` binary exists. Kept as a real, runnable script -- not
# just documentation -- so the day that binary lands, getting Complement running is "fix the
# TODOs this script's error points at," not "write this script."
set -euo pipefail
cd "$(dirname "$0")/../.."  # repository root

IMAGE_TAG="${1:-complement-hs-reimplement:dev}"

if ! command -v docker >/dev/null 2>&1; then
  echo "build.sh: docker is not installed; nothing to do" >&2
  exit 1
fi
if ! docker info >/dev/null 2>&1; then
  echo "build.sh: SKIP: Docker is installed but not running (\`docker info\` failed)." >&2
  echo "See tests/complement/README.md for how to run this once Docker is available." >&2
  exit 0
fi

docker build -t "$IMAGE_TAG" -f tests/complement/Dockerfile.template .
echo "built $IMAGE_TAG" >&2
