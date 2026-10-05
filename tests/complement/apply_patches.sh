#!/usr/bin/env bash
# Applies tests/complement/patches/*.patch to a Complement checkout, idempotently.
#
# Usage: tests/complement/apply_patches.sh [<complement dir>]   (default: refs/complement)
#
# Each patch fixes a race in an upstream Complement test that this server's timing exposes and
# that no homeserver can rule out (the test asserts on one server what has only been
# guaranteed on another). They are harness fixes, not behaviour changes: a patch never relaxes
# an assertion, it waits for the precondition the assertion depends on. Each patch's header
# says what it waits for and why; tests/complement/README.md ("Patches to upstream tests")
# lists them.
#
# A patch already applied is left alone (`git apply --reverse --check` succeeds); a patch that
# no longer applies (upstream changed the test) is an error naming the patch, so a stale patch
# is never silently skipped. Works on a git checkout or a plain copy (`git apply` needs no
# repository). tools/fetch-refs.sh runs this after cloning; run_single_node.sh runs it before
# every run.
set -euo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
TARGET="${1:-$HERE/../../refs/complement}"
if [ ! -d "$TARGET/tests" ]; then
  echo "apply_patches.sh: $TARGET is not a Complement checkout (no tests/ directory)" >&2
  exit 1
fi
cd "$TARGET"
# `git apply` run inside some other repository (a copy under this workspace's target/, say)
# works relative to that repository's root and silently ignores paths outside the current
# directory, so a check would "succeed" having looked at nothing. Stop discovery above the
# target: a git checkout of Complement is still found, anything else is patched as plain files.
export GIT_CEILING_DIRECTORIES="$(dirname "$PWD")"
status=0
for patch in "$HERE"/patches/*.patch; do
  [ -e "$patch" ] || continue
  name="$(basename "$patch")"
  if git apply --reverse --check "$patch" >/dev/null 2>&1; then
    echo "apply_patches.sh: $name: already applied" >&2
  elif git apply --check "$patch" >/dev/null 2>&1; then
    git apply "$patch"
    echo "apply_patches.sh: $name: applied" >&2
  else
    echo "apply_patches.sh: $name: does not apply to $TARGET (upstream changed the test?);" \
      "refresh it or drop it (the race it fixed comes back if dropped)" >&2
    status=1
  fi
done
exit "$status"
