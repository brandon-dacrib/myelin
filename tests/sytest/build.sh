#!/usr/bin/env bash
# Builds the Sytest image (tests/sytest/Dockerfile) from the repository root.
#
#   tests/sytest/build.sh [IMAGE_TAG]      # default myelin-sytest:dev
#
# As tests/complement/build.sh does, this streams a tar of the repository to `docker build`
# instead of sending `.` as the context, leaving out `target/`, `.git/`, `refs/` and the web
# build's output. It uses the classic builder (`DOCKER_BUILDKIT=0`) and a `DOCKER_CONFIG` with
# no credential helper, and pulls the base images from `mirror.gcr.io`: on the project's desktop
# Docker Hub pulls go through the macOS keychain helper, which agent sessions cannot use. Set
# RUST_IMAGE / SYTEST_IMAGE to override the bases (CI can use the Docker Hub names).
set -euo pipefail
cd "$(dirname "$0")/../.."  # repository root

IMAGE_TAG="${1:-myelin-sytest:dev}"
RUST_IMAGE="${RUST_IMAGE:-mirror.gcr.io/library/rust:1.98-slim-bookworm}"
SYTEST_IMAGE="${SYTEST_IMAGE:-mirror.gcr.io/matrixdotorg/sytest:bookworm}"

if ! command -v docker >/dev/null 2>&1; then
  echo "build.sh: SKIP: docker is not installed." >&2
  exit 0
fi
if [ -z "${DOCKER_HOST:-}" ] && [ -S "$HOME/.orbstack/run/docker.sock" ]; then
  export DOCKER_HOST="unix://$HOME/.orbstack/run/docker.sock"
fi
if [ -z "${DOCKER_CONFIG:-}" ]; then
  DOCKER_CONFIG="$(mktemp -d)"
  echo '{"auths":{}}' >"$DOCKER_CONFIG/config.json"
  export DOCKER_CONFIG
fi
if ! docker info >/dev/null 2>&1; then
  echo "build.sh: SKIP: Docker is installed but not running (\`docker info\` failed)." >&2
  exit 0
fi

tar \
  --exclude='./target' \
  --exclude='./.git' \
  --exclude='./.claude' \
  --exclude='./web/node_modules' \
  --exclude='./web/dist' \
  --exclude='./refs' \
  --exclude='./media-store' \
  --exclude='./.conformance-run' \
  --exclude='./crates/*/fuzz/target' \
  --exclude='./crates/*/fuzz/artifacts' \
  --exclude='./logs' \
  -cf - . \
  | DOCKER_BUILDKIT=0 docker build \
      --build-arg "RUST_IMAGE=$RUST_IMAGE" \
      --build-arg "SYTEST_IMAGE=$SYTEST_IMAGE" \
      -t "$IMAGE_TAG" -f tests/sytest/Dockerfile -

echo "build.sh: built $IMAGE_TAG" >&2
