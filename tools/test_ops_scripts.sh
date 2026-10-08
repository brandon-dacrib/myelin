#!/bin/bash
# Tests for the harness's shell scripts, with no Docker, PostgreSQL or cargo:
#   - tests/complement/lock.sh: exit status, serialisation, a stale owner, a fresh lock with no
#     owner file yet, a signal reaching the command, and release in every case;
#   - tests/complement/build.sh and tests/sytest/build.sh --dry-run: the per-tag target cache id,
#     --shared-cache and TARGET_CACHE_ID;
#   - tools/merge-queue.sh's helpers (pg_host_port, check_pg, prune_target, size_gb), taken out
#     of the script by name, and its refusal to start when PostgreSQL is missing.
#
# Usage: tools/test_ops_scripts.sh      (about 15 s; writes about 3 GB to a scratch directory and
#                                         removes it). Prints "ok" per check and exits non-zero on
#                                         the first failure.
# Written for the bash 3.2 that macOS ships.
set -u
REPO=$(cd "$(dirname "$0")/.." && pwd)
T=$(mktemp -d "${TMPDIR:-/tmp}/test_ops_scripts.XXXXXX")
trap 'rm -rf "$T"' EXIT
n=0
ok() { n=$((n + 1)); echo "ok $n - $1"; }
fail() { echo "FAIL - $1" >&2; exit 1; }
expect() { # expect <description> <want> <got>
  [ "$2" = "$3" ] && ok "$1" || fail "$1: want [$2], got [$3]"
}

# ---- lock.sh ------------------------------------------------------------------------------
LOCK=$REPO/tests/complement/lock.sh
export COMPLEMENT_LOCK_DIR=$T/complement.lock COMPLEMENT_LOCK_POLL=1

"$LOCK" sh -c 'exit 7' 2>/dev/null
expect "lock.sh passes the command's exit status through" 7 $?
[ ! -e "$COMPLEMENT_LOCK_DIR" ] && ok "lock.sh releases the lock on exit" || fail "lock left behind"

for i in 1 2 3; do
  "$LOCK" sh -c "echo start >> $T/log; sleep 1; echo end >> $T/log" 2>/dev/null &
done
wait
expect "lock.sh runs three concurrent commands one at a time" \
  "start end start end start end" "$(tr '\n' ' ' < "$T/log" | sed 's/ $//')"

mkdir "$COMPLEMENT_LOCK_DIR"
printf 'pid=999999\ncommand=go test\n' > "$COMPLEMENT_LOCK_DIR/owner"
expect "lock.sh takes over a lock whose owner is not running" ran "$("$LOCK" echo ran 2>/dev/null)"

mkdir "$COMPLEMENT_LOCK_DIR"
"$LOCK" touch "$T/should-not-exist" 2>/dev/null &
waiter=$!
sleep 3
kill "$waiter" 2>/dev/null; wait "$waiter" 2>/dev/null
[ ! -e "$T/should-not-exist" ] && ok "lock.sh waits on a lock with no owner file yet" \
  || fail "lock.sh took a lock whose owner was between mkdir and its owner file"
rmdir "$COMPLEMENT_LOCK_DIR"

"$LOCK" sh -c 'trap "exit 143" TERM; sleep 30 & wait' 2>/dev/null &
holder=$!
sleep 1
grep -q "^pid=$holder$" "$COMPLEMENT_LOCK_DIR/owner" && ok "lock.sh writes its PID to the owner file" \
  || fail "owner file does not name $holder"
kill -TERM "$holder"; wait "$holder"
expect "lock.sh passes TERM to the command" 143 $?
[ ! -e "$COMPLEMENT_LOCK_DIR" ] && ok "lock.sh releases the lock after TERM" || fail "lock left after TERM"
unset COMPLEMENT_LOCK_DIR

# ---- build.sh --dry-run ---------------------------------------------------------------------
cache_id() { "$@" 2>&1 | sed -n 's/.*target cache id \([^ ]*\)$/\1/p'; }
expect "complement build.sh: a cache per tag" myelin-complement-target-complement-hs-reimplement-ops \
  "$(cache_id "$REPO/tests/complement/build.sh" --dry-run complement-hs-reimplement:ops)"
expect "complement build.sh: --shared-cache" myelin-complement-target \
  "$(cache_id "$REPO/tests/complement/build.sh" --shared-cache --dry-run x:y)"
expect "complement build.sh: TARGET_CACHE_ID wins" mine \
  "$(TARGET_CACHE_ID=mine cache_id "$REPO/tests/complement/build.sh" --dry-run)"
expect "sytest build.sh: a cache per tag" myelin-sytest-target-myelin-sytest-dev \
  "$(cache_id "$REPO/tests/sytest/build.sh" --dry-run)"
expect "sytest build.sh: --shared-cache" myelin-sytest-target \
  "$(cache_id "$REPO/tests/sytest/build.sh" --dry-run --shared-cache myelin-sytest:x)"
"$REPO/tests/sytest/build.sh" --no-such-flag >/dev/null 2>&1
expect "sytest build.sh: an unknown flag is an error" 2 $?

# ---- merge-queue.sh helpers -----------------------------------------------------------------
MQ=$REPO/tools/merge-queue.sh
for f in pg_host_port check_pg size_gb prune_target; do
  sed -n "/^$f() {/,/^}/p" "$MQ" >> "$T/mq-functions.sh"
done
. "$T/mq-functions.sh"
expect "pg_host_port: URL" "127.0.0.1 5462" "$(pg_host_port postgres://u:p@127.0.0.1:5462/postgres)"
expect "pg_host_port: URL without a port" "db 5432" "$(pg_host_port 'postgresql://u@db/x?sslmode=require')"
expect "pg_host_port: IPv6" "::1 5433" "$(pg_host_port 'postgres://u@[::1]:5433/x')"
expect "pg_host_port: key=value" "10.0.0.1 5555" "$(pg_host_port 'host=10.0.0.1 port=5555 user=x')"

# A TCP listener for check_pg to find.
python3 -c '
import socket, sys, time
s = socket.socket(); s.bind(("127.0.0.1", 0)); s.listen(8)
print(s.getsockname()[1], flush=True); time.sleep(60)' > "$T/port" &
listener=$!
for _ in 1 2 3 4 5 6 7 8 9 10; do [ -s "$T/port" ] && break; sleep 0.3; done
open_port=$(cat "$T/port")
closed_port=$(python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1",0)); print(s.getsockname()[1])')
echo cert > "$T/server.crt"
PG_ALL_VARS="HS_CLUSTER_TEST_POSTGRES_DSN HS_KV_TEST_POSTGRES_TLS_DSN HS_KV_TEST_POSTGRES_TLS_CERT"
HS_CLUSTER_TEST_POSTGRES_DSN=postgres://u:p@127.0.0.1:$open_port/postgres
HS_KV_TEST_POSTGRES_TLS_DSN=postgres://u:p@127.0.0.1:$open_port/postgres
HS_KV_TEST_POSTGRES_TLS_CERT=$T/server.crt
check_pg >/dev/null && ok "check_pg: everything set and listening" || fail "check_pg refused a good setup"
HS_KV_TEST_POSTGRES_TLS_DSN=postgres://u:p@127.0.0.1:$closed_port/postgres
HS_KV_TEST_POSTGRES_TLS_CERT=$T/missing.crt
unset HS_CLUSTER_TEST_POSTGRES_DSN
out=$(check_pg); rc=$?
expect "check_pg: fails when anything is wrong" 1 $rc
expect "check_pg: names each problem" "3" "$(echo "$out" | wc -l | tr -d ' ')"
echo "$out" | grep -q "HS_CLUSTER_TEST_POSTGRES_DSN is unset" && ok "check_pg: an unset variable" \
  || fail "check_pg did not report the unset DSN: $out"
echo "$out" | grep -q "nothing accepts a connection at 127.0.0.1:$closed_port" \
  && ok "check_pg: an unreachable DSN" || fail "check_pg did not report the closed port: $out"
echo "$out" | grep -q "missing.crt is not a readable file" && ok "check_pg: a missing certificate" \
  || fail "check_pg did not report the certificate: $out"
kill "$listener" 2>/dev/null; wait "$listener" 2>/dev/null

# prune_target on a 3 GB target/: 1 GB in debug/incremental, 1 GB elsewhere in debug, 1 GB in
# release.
mk() { mkdir -p "$(dirname "$1")"; dd if=/dev/zero of="$1" bs=1048576 count=1100 2>/dev/null; }
QUEUE_TARGET=$T/target
mk "$T/target/debug/incremental/a"; mk "$T/target/debug/deps/b"; mk "$T/target/release/c"
DRY=1 TARGET_MAX_GB=2
out=$(prune_target)
[ -d "$T/target/debug/incremental" ] && echo "$out" | grep -q "would remove .*debug/incremental (1 GB)" \
  && echo "$out" | grep -q "would remove .*/debug (1 GB)" \
  && ok "prune_target --dry-run: says what it would remove, removes nothing" || fail "dry prune: $out"
DRY=0 TARGET_MAX_GB=3
out=$(prune_target)
[ ! -d "$T/target/debug/incremental" ] && [ -d "$T/target/debug/deps" ] \
  && echo "$out" | grep -q "target/ was 3 GB (limit 3 GB): removed .*debug/incremental (1 GB)" \
  && ok "prune_target: incremental first, and stops once under the limit" || fail "prune: $out"
mk "$T/target/debug/incremental/a"
TARGET_MAX_GB=2
out=$(prune_target)
[ ! -d "$T/target/debug" ] && [ -d "$T/target/release" ] \
  && ok "prune_target: then the whole of debug/ when still over" || fail "prune: $out"
TARGET_MAX_GB=80
expect "prune_target: under the limit keeps everything" "target/ is 1 GB (limit 80 GB): kept" \
  "$(prune_target)"

# The whole script refuses to start without PostgreSQL (and before it takes the lock or touches a
# worktree: the refusal comes first).
out=$(env -i HOME="$HOME" PATH="$PATH" MERGE_QUEUE_ENV=/nonexistent "$MQ" agent/no-such-branch 2>&1)
expect "merge-queue.sh: refuses to start without PostgreSQL" 1 $?
echo "$out" | grep -q "refusing to start the queue" && echo "$out" | grep -q "HS_KV_TEST_POSTGRES_DSN is unset" \
  && ok "merge-queue.sh: says why" || fail "merge-queue.sh output: $out"
"$MQ" --no-such-flag >/dev/null 2>&1
expect "merge-queue.sh: an unknown flag is an error" 2 $?

# The env file: sourced, exported even without `export`, and never over a value already set.
printf 'HS_KV_TEST_POSTGRES_DSN=postgres://file@127.0.0.1:1/x\nHS_CLUSTER_TEST_POSTGRES_DSN=postgres://file@127.0.0.1:1/x\n' \
  > "$T/gate.env"
sed -n '1,/^# The host and port/p' "$MQ" | sed 's/^set -u$/set -u; set -- --dry-run/' > "$T/mq-head.sh"
got=$(cd "$REPO" && env -i HOME="$HOME" PATH="$PATH" MERGE_QUEUE_ENV="$T/gate.env" \
  HS_CLUSTER_TEST_POSTGRES_DSN=postgres://caller@127.0.0.1:2/x \
  bash -c ". '$T/mq-head.sh' >/dev/null; env | grep '^HS_' | sort | tr '\n' ' '")
expect "merge-queue.sh: the env file is exported and does not override the caller" \
  "HS_CLUSTER_TEST_POSTGRES_DSN=postgres://caller@127.0.0.1:2/x HS_KV_TEST_POSTGRES_DSN=postgres://file@127.0.0.1:1/x " "$got"

echo "all $n passed"
