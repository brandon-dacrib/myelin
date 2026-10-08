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
#
#   tests/complement/build.sh [--shared-cache] [--dry-run] [IMAGE_TAG]
#                                            # default tag complement-hs-reimplement:dev
#
# Each image tag gets its own BuildKit cache for the build's `target/` by default, with the id
# `myelin-complement-target-<tag>` (characters other than letters, digits, `.`, `_` and `-` become
# `-`): one cache shared by every branch's image once linked another branch's crate into an
# image (2026-10-04/05). So give each branch its own tag,
# `complement-hs-reimplement:<agent name>` (a tag has no `/`). `--shared-cache` uses the one
# shared cache, `myelin-complement-target`, as before: warm, and right only when one branch
# builds at a time. `TARGET_CACHE_ID=<id>` names any other.
# `--dry-run` prints the tag, the cache id and the build command, and builds nothing.
#
# A first build into a new cache is a cold release build (about 20 minutes here), and each cache
# holds a few GB. List them with `docker buildx du --verbose | grep -B6 'myelin-complement-target'`
# and remove one with `docker buildx prune -f --filter id=<ID>`.
set -euo pipefail
cd "$(dirname "$0")/../.."  # repository root

SHARED_CACHE=0
DRY_RUN=0
IMAGE_TAG=complement-hs-reimplement:dev
while [ $# -gt 0 ]; do
  case $1 in
    --shared-cache) SHARED_CACHE=1 ;;
    --dry-run) DRY_RUN=1 ;;
    -*) echo "build.sh: unknown flag $1" >&2; exit 2 ;;
    *) IMAGE_TAG=$1 ;;
  esac
  shift
done
if [ -n "${TARGET_CACHE_ID:-}" ]; then
  :
elif [ $SHARED_CACHE = 1 ]; then
  TARGET_CACHE_ID=myelin-complement-target
else
  TARGET_CACHE_ID="myelin-complement-target-$(printf '%s' "$IMAGE_TAG" | tr -c 'A-Za-z0-9._-' '-')"
fi
if [ $DRY_RUN = 1 ]; then
  echo "build.sh: dry run: image $IMAGE_TAG, target cache id $TARGET_CACHE_ID"
  echo "build.sh: dry run: tar ... | docker build --build-arg TARGET_CACHE_ID=$TARGET_CACHE_ID -t $IMAGE_TAG -f tests/complement/Dockerfile.template -"
  exit 0
fi

if ! command -v docker >/dev/null 2>&1; then
  echo "build.sh: docker is not installed; nothing to do" >&2
  exit 1
fi
if [ -z "${DOCKER_HOST:-}" ] && [ -S "$HOME/.orbstack/run/docker.sock" ]; then
  export DOCKER_HOST="unix://$HOME/.orbstack/run/docker.sock"
fi
# A DOCKER_CONFIG with no credential helper (Docker Hub pulls through the desktop's keychain
# helper fail in agent sessions), with the buildx plugin linked in so `docker build` is
# BuildKit: Dockerfile.template's cache mounts need it, and the classic builder rejects them.
# The same arrangement as tests/sytest/build.sh.
if [ -z "${DOCKER_CONFIG:-}" ]; then
  DOCKER_CONFIG="$(mktemp -d)"
  echo '{"auths":{}}' >"$DOCKER_CONFIG/config.json"
  [ -d "$HOME/.docker/cli-plugins" ] && ln -s "$HOME/.docker/cli-plugins" "$DOCKER_CONFIG/cli-plugins"
  export DOCKER_CONFIG
fi
if ! docker info >/dev/null 2>&1; then
  echo "build.sh: SKIP: Docker is installed but not running (\`docker info\` failed)." >&2
  echo "See tests/complement/README.md for how to run this once Docker is available." >&2
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
  | DOCKER_BUILDKIT=1 docker build \
      --build-arg "TARGET_CACHE_ID=$TARGET_CACHE_ID" \
      -t "$IMAGE_TAG" -f tests/complement/Dockerfile.template -

echo "built $IMAGE_TAG (target cache $TARGET_CACHE_ID)" >&2
