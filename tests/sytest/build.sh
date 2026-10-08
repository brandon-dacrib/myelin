#!/usr/bin/env bash
# Builds the Sytest image (tests/sytest/Dockerfile) from the repository root.
#
#   tests/sytest/build.sh [IMAGE_TAG]      # default myelin-sytest:dev (flags below)
#
# As tests/complement/build.sh does, this streams a tar of the repository to `docker build`
# instead of sending `.` as the context, leaving out `target/`, `.git/`, `refs/` and the web
# build's output. It uses a `DOCKER_CONFIG` with no credential helper, and pulls the base images
# from `mirror.gcr.io`: on the project's desktop Docker Hub pulls go through the macOS keychain
# helper, which agent sessions cannot use. Set RUST_IMAGE / SYTEST_IMAGE to override the bases
# (CI can use the Docker Hub names).
#
# It builds with BuildKit, which the Dockerfile's cache mounts need (a second build compiles
# only what changed). The replacement `DOCKER_CONFIG` hides `~/.docker/cli-plugins`, where
# OrbStack puts the buildx plugin, and without it `docker build` falls back to the classic
# builder and rejects the mounts; so the plugin directory is linked into the temporary config.
#
#   tests/sytest/build.sh [--shared-cache] [--dry-run] [IMAGE_TAG]
#
# Each image tag gets its own BuildKit cache for the build's `target/` by default, with the id
# `myelin-sytest-target-<tag>` (characters other than letters, digits, `.`, `_` and `-` become
# `-`): one cache shared by every branch's image once linked another branch's crate into an
# image (2026-10-04/05). So give each branch its own tag, `myelin-sytest:<agent name>` (a tag
# has no `/`). `--shared-cache` uses the one shared cache, `myelin-sytest-target`, as before:
# warm, and right only when one branch builds at a time. `TARGET_CACHE_ID=<id>` names any other.
# `--dry-run` prints the tag, the cache id and the build command, and builds nothing.
#
# A first build into a new cache is a cold release build (about 20 minutes here), and each cache
# holds a few GB. List them with `docker buildx du --verbose | grep -B6 'myelin-sytest-target'`
# and remove one with `docker buildx prune -f --filter id=<ID>`.
set -euo pipefail
cd "$(dirname "$0")/../.."  # repository root

SHARED_CACHE=0
DRY_RUN=0
IMAGE_TAG=myelin-sytest:dev
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
  TARGET_CACHE_ID=myelin-sytest-target
else
  TARGET_CACHE_ID="myelin-sytest-target-$(printf '%s' "$IMAGE_TAG" | tr -c 'A-Za-z0-9._-' '-')"
fi
RUST_IMAGE="${RUST_IMAGE:-mirror.gcr.io/library/rust:1.98-slim-bookworm}"
SYTEST_IMAGE="${SYTEST_IMAGE:-mirror.gcr.io/matrixdotorg/sytest:bookworm}"

if [ $DRY_RUN = 1 ]; then
  echo "build.sh: dry run: image $IMAGE_TAG, target cache id $TARGET_CACHE_ID"
  echo "build.sh: dry run: tar ... | docker build --build-arg RUST_IMAGE=$RUST_IMAGE --build-arg SYTEST_IMAGE=$SYTEST_IMAGE --build-arg TARGET_CACHE_ID=$TARGET_CACHE_ID -t $IMAGE_TAG -f tests/sytest/Dockerfile -"
  exit 0
fi
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
  [ -d "$HOME/.docker/cli-plugins" ] && ln -s "$HOME/.docker/cli-plugins" "$DOCKER_CONFIG/cli-plugins"
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
  | DOCKER_BUILDKIT=1 docker build \
      --build-arg "RUST_IMAGE=$RUST_IMAGE" \
      --build-arg "SYTEST_IMAGE=$SYTEST_IMAGE" \
      --build-arg "TARGET_CACHE_ID=$TARGET_CACHE_ID" \
      -t "$IMAGE_TAG" -f tests/sytest/Dockerfile -

echo "build.sh: built $IMAGE_TAG (target cache $TARGET_CACHE_ID)" >&2
