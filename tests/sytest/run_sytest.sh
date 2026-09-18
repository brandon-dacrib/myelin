#!/usr/bin/env bash
# Runs Sytest against this server's plugin (plugins/hs-reimplement/). Untested: no `hs-server`
# binary exists in this workspace yet (see plugins/hs-reimplement/lib/SyTest/Homeserver/
# HsReimplement.pm's TODOs), and Sytest's own CPAN dependencies are not installed in this
# environment. This script checks preconditions and skips cleanly (exit 0) rather than failing
# when any of them are missing, per this track's brief.
set -euo pipefail
cd "$(dirname "$0")"

SYTEST_DIR="${SYTEST_DIR:-$(cd ../../refs/sytest && pwd)}"
BINDIR="${HS_REIMPLEMENT_BINDIR:-$(cd ../../target/release 2>/dev/null && pwd || echo "")}"

if [ ! -d "$SYTEST_DIR" ]; then
  echo "run_sytest.sh: SKIP: $SYTEST_DIR not found; run tools/fetch-refs.sh (network required)." >&2
  exit 0
fi
if ! command -v perl >/dev/null 2>&1; then
  echo "run_sytest.sh: SKIP: perl is not installed." >&2
  exit 0
fi
if ! perl -MFuture -e1 >/dev/null 2>&1; then
  echo "run_sytest.sh: SKIP: Sytest's CPAN dependencies (Future, IO::Async, ...) are not" >&2
  echo "installed. See $SYTEST_DIR/README.rst / install-deps.pl for how to install them." >&2
  exit 0
fi
if [ -z "$BINDIR" ] || [ ! -x "$BINDIR/hs-server" ]; then
  echo "run_sytest.sh: SKIP: no hs-server binary found (looked in ${BINDIR:-<unset>})." >&2
  echo "This workspace has no assembled server binary yet; see tests/sytest/README.md." >&2
  exit 0
fi

export SYTEST_PLUGINS="$(pwd)/plugins"
cd "$SYTEST_DIR"
echo "run_sytest.sh: perl run-tests.pl -I HsReimplement --hs-reimplement-binary-directory $BINDIR $*" >&2
perl run-tests.pl -I HsReimplement --hs-reimplement-binary-directory "$BINDIR" "$@"
