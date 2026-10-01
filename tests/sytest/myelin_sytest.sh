#!/bin/bash
# Runs inside the Sytest image (tests/sytest/Dockerfile); started by tests/sytest/run.sh, which
# mounts:
#   /sytest   a Sytest checkout (read-only)
#   /myelin   this directory (read-only): plugins/, this script, the blacklist
#   /logs     where results.tap, the run's stderr and each server's logs are written
# and passes any extra arguments through to run-tests.pl (e.g. test files to run).
#
# Modelled on Sytest's scripts/dendrite_sytest.sh (Apache-2.0): `--all` (do not stop at the
# first failure), TAP output, a work directory outside the checkout, `--exclude-deprecated`
# (this server, like Dendrite, implements the current endpoints, not r0-era ones Sytest marks
# deprecated). No PostgreSQL: this server's embedded store is what a single-node install uses.
set -uo pipefail

mkdir -p /work /logs
export SYTEST_PLUGINS=/myelin/plugins
cd /sytest

BLACKLIST_ARGS=()
if [ -s /myelin/sytest-blacklist ]; then
  BLACKLIST_ARGS=(-B /myelin/sytest-blacklist)
fi

echo "myelin_sytest.sh: hs $(/usr/local/bin/hs version 2>/dev/null || echo unknown)" >&2
echo "myelin_sytest.sh: perl run-tests.pl -I Myelin -O tap --all --exclude-deprecated $*" >&2

TEST_STATUS=0
perl run-tests.pl -I Myelin -O tap --all \
  --work-directory=/work --exclude-deprecated \
  "${BLACKLIST_ARGS[@]}" \
  "$@" >/logs/results.tap 2>/logs/run-tests.stderr &
pid=$!
trap 'kill $pid' TERM INT
wait $pid || TEST_STATUS=$?
trap - TERM INT

echo "myelin_sytest.sh: run-tests.pl exited $TEST_STATUS" >&2

# Each server's own output, its configuration and haproxy's log, for reading failures.
for dir in /work/server-*; do
  [ -d "$dir" ] || continue
  name="$(basename "$dir")"
  mkdir -p "/logs/$name"
  cp "$dir"/hs.log "$dir"/haproxy.log "$dir"/myelin.yaml "/logs/$name/" 2>/dev/null || true
done
chmod -R go+rX /logs 2>/dev/null || true

exit $TEST_STATUS
