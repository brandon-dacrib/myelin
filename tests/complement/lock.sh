#!/usr/bin/env bash
# Serialises Complement runs on this desktop: two `go test` runs of one Complement package on one
# Docker daemon share container and network names and break each other (2026-10-04/05, wave 2).
#
# Usage:   tests/complement/lock.sh <command...>
# e.g.:    tests/complement/lock.sh go test -v -run TestRestrictedRooms ./tests/...
#
# The lock is a directory, `myelin-complement.lock`, under the repository's common `.git/` (so
# every worktree of the repository shares it, as `tools/merge-queue.sh`'s lock is shared), made
# with `mkdir`, which is atomic. Inside it, `owner` holds this script's PID, the start time, the
# working directory and the command. The lock is released when this script exits, however it
# exits (a trap; INT, TERM and HUP are passed on to the command first).
#
# While it waits it says once who holds the lock. A lock whose owner PID is no longer running
# (a run killed with SIGKILL, or a rebooted machine) is taken over, and that is logged; a lock
# directory with no `owner` file yet is never taken over (its owner is between `mkdir` and the
# write).
#
# Environment:
#   COMPLEMENT_LOCK_DIR   the lock directory (default <git common dir>/myelin-complement.lock)
#   COMPLEMENT_LOCK_POLL  seconds between attempts while waiting (default 20)
#
# Written for the bash 3.2 that macOS ships.
set -u

if [ $# -eq 0 ]; then
  echo "usage: $0 <command...>" >&2
  exit 2
fi

if [ -z "${COMPLEMENT_LOCK_DIR:-}" ]; then
  common=$(git -C "$(dirname "$0")" rev-parse --path-format=absolute --git-common-dir 2>/dev/null) \
    || { echo "lock.sh: not inside a git repository; set COMPLEMENT_LOCK_DIR" >&2; exit 2; }
  COMPLEMENT_LOCK_DIR=$common/myelin-complement.lock
fi
L=$COMPLEMENT_LOCK_DIR
POLL=${COMPLEMENT_LOCK_POLL:-20}

said=0
until mkdir "$L" 2>/dev/null; do
  owner_pid=$(sed -n 's/^pid=//p' "$L/owner" 2>/dev/null)
  if [ -n "$owner_pid" ] && ! kill -0 "$owner_pid" 2>/dev/null; then
    # Read again just before removing: another waiter may have taken the lock over already, and
    # its owner file names a live PID. (The window left, between this read and `rm`, is the
    # length of one `rm` of a two-entry directory.)
    [ "$(sed -n 's/^pid=//p' "$L/owner" 2>/dev/null)" = "$owner_pid" ] || continue
    echo "lock.sh: $(date '+%F %T') taking over $L from PID $owner_pid, which is no longer running:" >&2
    sed 's/^/lock.sh:   /' "$L/owner" >&2 2>/dev/null
    rm -rf "$L"
    continue
  fi
  if [ $said = 0 ]; then
    echo "lock.sh: $(date '+%F %T') waiting for $L, held by:" >&2
    sed 's/^/lock.sh:   /' "$L/owner" >&2 2>/dev/null || echo "lock.sh:   (no owner file yet)" >&2
    said=1
  fi
  sleep "$POLL"
done

child=
release() { rm -rf "$L"; }
forward() {
  [ -n "$child" ] && kill -"$1" "$child" 2>/dev/null
}
trap release EXIT
trap 'forward TERM' INT  # a background child ignores SIGINT in a non-interactive shell
trap 'forward TERM' TERM
trap 'forward HUP' HUP

{
  echo "pid=$$"
  echo "since=$(date '+%F %T %z')"
  echo "cwd=$PWD"
  echo "command=$*"
} >"$L/owner"
[ $said = 1 ] && echo "lock.sh: $(date '+%F %T') took $L" >&2

# The command runs in the background so the traps above run while it does (a foreground child
# would hold them until it exits); `wait` is repeated because a trapped signal interrupts it.
"$@" &
child=$!
status=0
while :; do
  wait "$child"
  status=$?
  kill -0 "$child" 2>/dev/null || break
done
exit "$status"
