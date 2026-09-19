#!/usr/bin/env bash
# Builds the Complement image from the repository root.
#
# The build context is NOT `docker build ... .`: this workspace's `target/` directory is tens of
# gigabytes (shared across every track's `cargo` invocations) and `.dockerignore` is a root-level
# file this track does not own (see docs/status/14-test-and-conformance.md's ownership rules).
# Instead this streams a tar of the repo to `docker build`'s stdin, excluding directories that are
# large and irrelevant to the build (`target/`, `.git/`, `web/node_modules/`, `refs/`) or would
# otherwise make the tar attempt to read a live database file mid-write (`media-store/`,
# `.conformance-run/`). `Dockerfile.template`'s `COPY . .` still sees a normal-looking workspace:
# every crate, `Cargo.toml`/`Cargo.lock`, `rust-toolchain.toml`, and `tests/complement/` itself
# (needed for `startup.sh`/`stunnel.conf.template`).
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

tar \
  --exclude='./target' \
  --exclude='./.git' \
  --exclude='./web/node_modules' \
  --exclude='./web/dist' \
  --exclude='./refs' \
  --exclude='./media-store' \
  --exclude='./.conformance-run' \
  -cf - . \
  | docker build -t "$IMAGE_TAG" -f tests/complement/Dockerfile.template -

echo "built $IMAGE_TAG" >&2
