#!/usr/bin/env bash
# Runs Sytest against this server in Docker.
#
#   tests/sytest/build.sh myelin-sytest:dev          # once per server change (release build)
#   tests/sytest/run.sh [run-tests.pl args...]       # whole suite, or e.g. tests/11register.pl
#
# Environment:
#   SYTEST_IMAGE_TAG  image built by build.sh (default myelin-sytest:dev)
#   SYTEST_DIR        Sytest checkout to mount (default refs/sytest, cloned if missing)
#   SYTEST_LOGS       where results go (default target/sytest/<UTC timestamp>; target/ is git-ignored)
#   TIMEOUT_FACTOR    Sytest's own multiplier for every wait, including the 60 s it gives a
#                     homeserver to start (default 1)
#   SYTEST_WORK_SIZE  size of the tmpfs holding the servers' data (default 3g). A tmpfs, because
#                     a first boot creates and fsyncs every keyspace, which took over 60 s on the
#                     container's overlay filesystem on a loaded desktop and failed the start.
#
# Writes $SYTEST_LOGS/results.tap, run-tests.stderr, server-N/{hs.log,haproxy.log,myelin.yaml},
# then summarize.py's results.txt (one line per test: PASS/FAIL/SKIP/XFAIL and its name) and
# summary.txt (counts and the most common failure reasons), and are-we-synapse-yet.txt.
# Exits 0 when Docker is absent (skips cleanly, like tests/complement/), otherwise with
# run-tests.pl's status.
set -euo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$HERE/../.." && pwd)"

IMAGE="${SYTEST_IMAGE_TAG:-myelin-sytest:dev}"
SYTEST_DIR="${SYTEST_DIR:-$ROOT/refs/sytest}"
LOGS="${SYTEST_LOGS:-$ROOT/target/sytest/$(date -u +%Y%m%dT%H%M%SZ)}"

if ! command -v docker >/dev/null 2>&1; then
  echo "run.sh: SKIP: docker is not installed." >&2
  exit 0
fi
if [ -z "${DOCKER_HOST:-}" ] && [ -S "$HOME/.orbstack/run/docker.sock" ]; then
  export DOCKER_HOST="unix://$HOME/.orbstack/run/docker.sock"
fi
if ! docker info >/dev/null 2>&1; then
  echo "run.sh: SKIP: Docker is installed but not running." >&2
  exit 0
fi
if ! docker image inspect "$IMAGE" >/dev/null 2>&1; then
  echo "run.sh: image $IMAGE not found; build it with tests/sytest/build.sh $IMAGE" >&2
  exit 1
fi
if [ ! -d "$SYTEST_DIR" ]; then
  echo "run.sh: cloning Sytest into $SYTEST_DIR" >&2
  git clone --depth 1 -q https://github.com/matrix-org/sytest.git "$SYTEST_DIR"
fi

mkdir -p "$LOGS"
git -C "$SYTEST_DIR" rev-parse HEAD >"$LOGS/sytest-commit.txt" 2>/dev/null || true
echo "run.sh: image $IMAGE, Sytest $(cat "$LOGS/sytest-commit.txt" 2>/dev/null), logs in $LOGS" >&2

NAME="myelin-sytest-$$"
cleanup() { docker rm -f "$NAME" >/dev/null 2>&1 || true; }
trap cleanup EXIT

# Extra environment for the servers (they inherit the container's), e.g.
# SYTEST_EXTRA_ENV="MALLOC_ARENA_MAX=2".
EXTRA_ENV=()
for kv in ${SYTEST_EXTRA_ENV:-}; do EXTRA_ENV+=(-e "$kv"); done
# SYTEST_HS_BINARY: a Linux `hs` (built for the image's Debian release) to run instead of the
# image's own, so a server change can be tested without rebuilding the image.
if [ -n "${SYTEST_HS_BINARY:-}" ]; then
  EXTRA_ENV+=(-v "$SYTEST_HS_BINARY:/usr/local/bin/hs:ro")
fi

STATUS=0
docker run "${EXTRA_ENV[@]}" --name "$NAME" --rm \
  -v "$SYTEST_DIR:/sytest:ro" \
  -v "$HERE:/myelin:ro" \
  -v "$LOGS:/logs" \
  --tmpfs "/work:exec,size=${SYTEST_WORK_SIZE:-3g}" \
  -e "MYELIN_RUST_LOG=${MYELIN_RUST_LOG:-info}" \
  -e "TIMEOUT_FACTOR=${TIMEOUT_FACTOR:-1}" \
  -e "MYELIN_MEMORY_INTERVAL=${MYELIN_MEMORY_INTERVAL:-15}" \
  --entrypoint /bin/bash \
  "$IMAGE" /myelin/myelin_sytest.sh "$@" || STATUS=$?

if [ -s "$LOGS/results.tap" ]; then
  python3 "$HERE/summarize.py" "$LOGS/results.tap" \
    --results "$LOGS/results.txt" --summary "$LOGS/summary.txt" || true
  python3 "$HERE/are-we-synapse-yet.py" "$LOGS/results.tap" >"$LOGS/are-we-synapse-yet.txt" 2>&1 || true
  # A server or haproxy that dies mid-run fails every later test with "connection refused";
  # say so at the top of the summary instead of letting it read as a few hundred real failures.
  # (Every process is stopped at the end of a run; those exits are the last lines and expected.)
  if grep -nE "process (exited|failed)" "$LOGS/run-tests.stderr" >"$LOGS/process-exits.txt" 2>/dev/null; then
    { echo "process exits during the run (the last few are the shutdown):"; cat "$LOGS/process-exits.txt"; echo; cat "$LOGS/summary.txt"; } >"$LOGS/summary.tmp" \
      && mv "$LOGS/summary.tmp" "$LOGS/summary.txt"
  fi
  cat "$LOGS/summary.txt" >&2 || true
fi
exit $STATUS
