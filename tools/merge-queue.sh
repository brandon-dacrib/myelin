#!/bin/bash
# Merges finished agent branches into main, each only if the full gate passes.
#
# Usage (from anywhere in the repository):
#   tools/merge-queue.sh agent/two-pod-cluster-2 agent/federation-media ...   # one gate per branch
#   tools/merge-queue.sh agent/a,agent/b,agent/c agent/d                      # a,b,c share one gate; then d
#   tools/merge-queue.sh --all              # every origin branch not yet merged into origin/main, serially
#   tools/merge-queue.sh --dry-run ...      # the rebases and the plan, with no gate, no push, no lock
#   tools/merge-queue.sh --dry-run=fail ...  # the same, with every gate assumed to fail (shows the fallback)
#
# Serial, for each branch: take the merge lock (.git/myelin-merge.lock in the main checkout, shared
# with any agent following AGENTS.md), check the branch out in one reused worktree
# (.claude/worktrees/merge-queue, so its target/ stays warm), rebase onto origin/main, run fmt,
# clippy, `cargo test --workspace --all-targets` and, if web/ changed, `npm run check` and
# `npm run test:e2e`, then push to main, delete the origin branch and release the lock. A branch
# that fails anything stays on origin untouched and the queue moves on; the reason is printed and
# the full log is in the worktree's target/merge-gate-<branch>.log.
#
# Batched (branches joined with commas): the group's branches are rebased one onto the next, in
# the order given, starting from origin/main, and the result goes through one gate. A branch whose
# rebase conflicts is left out of the group and reported. If the gate passes, the whole stack is
# pushed and every branch in it deleted; if it fails, the gate cannot say which branch broke it,
# so each branch of the group is then gated on its own, serially, and merges or fails alone.
# Batch branches that touch different crates; a group is only quicker than serial when it passes.
#
# A dry run does the same rebases in a worktree of its own (.claude/worktrees/merge-queue-dry),
# prints what each group would stack and push, and runs nothing. It takes no lock, so it can
# check tomorrow's plan while tonight's queue runs.
#
#   tools/merge-queue.sh --allow-skips ...  # gate even if PostgreSQL is missing (tests SKIP)
#   tools/merge-queue.sh --no-push ...      # take the lock and gate for real, but push nothing
#                                           # and delete no branch (checks a branch or the queue)
#
# PostgreSQL. The gate needs these, or the tests that use them print SKIP and pass, and a gate
# that passes that way has not run them:
#   HS_CLUSTER_TEST_POSTGRES_DSN   a PostgreSQL whose user may create databases (hs-cli's
#                                  cluster_*.rs, the two-replica test in cluster_admin.rs)
#   HS_KV_TEST_POSTGRES_DSN        the same kind of server, no TLS (hs-kv's postgres_*.rs)
#   HS_KV_TEST_POSTGRES_TLS_DSN    a server with `ssl = on` (hs-kv's postgres_tls.rs)
#   HS_KV_TEST_POSTGRES_TLS_CERT   the PEM of that server's self-signed certificate
# The queue refuses to start, and refuses each gate, when one is unset, when a DSN's host and port
# do not accept a TCP connection, or when the certificate is not a readable file; it prints which.
# `--allow-skips` keeps the old behaviour (a warning, then the gate). hs-cli's postgres_tls.rs
# reads HS_CLUSTER_TEST_POSTGRES_TLS_DSN / _CERT; when those are unset they are set from the two
# HS_KV_TEST_POSTGRES_TLS_* (the same kind of server), and that is logged.
# If $MERGE_QUEUE_ENV (default <repository>/.claude/gate-pg/env.sh) exists, it is sourced first;
# a variable already set in the environment keeps its value. Making the servers (see
# crates/hs-kv/tests/postgres_tls.rs for the certificate):
#   docker run --rm -d --name hs-merge-queue-pg -e POSTGRES_PASSWORD=hspg \
#     -p 127.0.0.1:5462:5432 public.ecr.aws/docker/library/postgres:17
#   export HS_CLUSTER_TEST_POSTGRES_DSN=postgres://postgres:hspg@127.0.0.1:5462/postgres
#   export HS_KV_TEST_POSTGRES_DSN=$HS_CLUSTER_TEST_POSTGRES_DSN
#   (and hs-merge-queue-pg-tls on 5463 with `-c ssl=on`, for the two TLS variables)
#
# Disk. Before each gate, if the queue worktree's target/ (or $CARGO_TARGET_DIR) is over
# $MERGE_QUEUE_TARGET_MAX_GB (default 80), target/debug/incremental is deleted and, if it is
# still over, target/debug; what was removed and the sizes are logged. (The queue's target/
# reached 237 GB on 2026-10-05 and filled the disk.) A dry run reports what a gate would remove,
# measured on the real queue worktree's target/, and removes nothing.
#
# If main moves during a gate and the new commits touch only docs/, the branch is rebased again
# and pushed; if they touch code, the branch is left for the next run.
#
# A gate's `npm run check` regenerates web/src/api/schema.d.ts, so the reused worktree is dirty
# after any web gate; tracked changes are discarded before every checkout (a dirty generated
# file refused the next branch's checkout twice on 2026-10-02).
#
# Written for the bash 3.2 that macOS ships.
set -u
ROOT=$(git rev-parse --path-format=absolute --git-common-dir) || exit 1
ROOT=${ROOT%/.git}
LOCK=$ROOT/.git/myelin-merge.lock
W=$ROOT/.claude/worktrees/merge-queue
[ -f "$HOME/.cargo/env" ] && . "$HOME/.cargo/env"
export CARGO_PROFILE_DEV_DEBUG=0
DRY=0 DRY_FAIL=0 ALLOW_SKIPS=0 NO_PUSH=0
while :; do
  case ${1:-} in
    --dry-run) DRY=1 ;;
    --no-push) NO_PUSH=1 ;;
    --dry-run=fail) DRY=1 DRY_FAIL=1 ;;
    --allow-skips) ALLOW_SKIPS=1 ;;
    --*) [ "$1" = --all ] && break; echo "unknown flag $1" >&2; exit 2 ;;
    *) break ;;
  esac
  shift
done
# The real queue's target/, which a dry run measures; the gate's own when not dry.
QUEUE_TARGET=${CARGO_TARGET_DIR:-$W/target}
[ $DRY = 1 ] && W=$W-dry
TARGET_MAX_GB=${MERGE_QUEUE_TARGET_MAX_GB:-80}

PG_VARS="HS_CLUSTER_TEST_POSTGRES_DSN HS_KV_TEST_POSTGRES_DSN HS_KV_TEST_POSTGRES_TLS_DSN HS_KV_TEST_POSTGRES_TLS_CERT"
PG_ALL_VARS="$PG_VARS HS_CLUSTER_TEST_POSTGRES_TLS_DSN HS_CLUSTER_TEST_POSTGRES_TLS_CERT"
ENV_FILE=${MERGE_QUEUE_ENV:-$ROOT/.claude/gate-pg/env.sh}
if [ -f "$ENV_FILE" ]; then
  # Source it, then put back whatever the caller had already set.
  for v in $PG_ALL_VARS; do eval "saved_$v=\${$v:-}"; done
  . "$ENV_FILE"
  for v in $PG_ALL_VARS; do eval "[ -z \"\$saved_$v\" ] || export $v=\"\$saved_$v\""; done
  # A file that sets without `export` still reaches cargo.
  for v in $PG_ALL_VARS; do eval "[ -z \"\${$v:-}\" ] || export $v"; done
  echo "sourced $ENV_FILE"
fi
if [ -z "${HS_CLUSTER_TEST_POSTGRES_TLS_DSN:-}" ] && [ -z "${HS_CLUSTER_TEST_POSTGRES_TLS_CERT:-}" ] \
  && [ -n "${HS_KV_TEST_POSTGRES_TLS_DSN:-}" ] && [ -n "${HS_KV_TEST_POSTGRES_TLS_CERT:-}" ]; then
  export HS_CLUSTER_TEST_POSTGRES_TLS_DSN=$HS_KV_TEST_POSTGRES_TLS_DSN
  export HS_CLUSTER_TEST_POSTGRES_TLS_CERT=$HS_KV_TEST_POSTGRES_TLS_CERT
  echo "HS_CLUSTER_TEST_POSTGRES_TLS_{DSN,CERT} set from HS_KV_TEST_POSTGRES_TLS_{DSN,CERT} (hs-cli's postgres_tls.rs)"
fi

# The host and port a PostgreSQL DSN names, as "host port": a URL
# (postgres://user:pass@host:port/db?...) or key=value pairs (host=... port=...).
pg_host_port() {
  local dsn=$1 hp host port
  case $dsn in
    postgres://*|postgresql://*)
      hp=${dsn#*://}; hp=${hp%%/*}; hp=${hp%%\?*}; hp=${hp##*@}
      case $hp in
        \[*) host=${hp#\[}; host=${host%%]*}; port=${hp##*]}; port=${port#:} ;;
        *:*) host=${hp%:*}; port=${hp##*:} ;;
        *) host=$hp; port= ;;
      esac ;;
    *)
      host=$(printf '%s\n' "$dsn" | tr ' ' '\n' | sed -n 's/^host=//p' | head -1)
      port=$(printf '%s\n' "$dsn" | tr ' ' '\n' | sed -n 's/^port=//p' | head -1) ;;
  esac
  echo "${host:-localhost} ${port:-5432}"
}

# Prints one line per PostgreSQL variable the gate needs that is unset, unreachable or (the
# certificate) unreadable; returns 1 if there was any.
check_pg() {
  local v val bad=0 host port
  for v in $PG_ALL_VARS; do
    eval "val=\${$v:-}"
    if [ -z "$val" ]; then
      echo "  $v is unset"; bad=1; continue
    fi
    case $v in
      *_CERT)
        [ -r "$val" ] && [ -f "$val" ] || { echo "  $v=$val is not a readable file"; bad=1; } ;;
      *)
        set -- $(pg_host_port "$val"); host=$1 port=$2
        nc -z -w 5 "$host" "$port" >/dev/null 2>&1 \
          || { echo "  $v: nothing accepts a connection at $host:$port"; bad=1; } ;;
    esac
  done
  return $bad
}

# Checks PostgreSQL before a gate (or at start, $1 = "start"). Returns 1 when the gate must not
# run; under --allow-skips or a dry run it only reports.
require_pg() {
  local problems
  problems=$(check_pg) && return 0
  if [ $DRY = 1 ]; then
    echo "dry run: PostgreSQL is not ready; a real run would refuse to gate (--allow-skips to gate anyway):"
    echo "$problems"; return 0
  fi
  if [ $ALLOW_SKIPS = 1 ]; then
    echo "warning (--allow-skips): PostgreSQL is not ready; the tests that need it will SKIP:"
    echo "$problems"; return 0
  fi
  echo "PostgreSQL is not ready, so the tests that need it would SKIP and pass; refusing to ${1:-gate}"
  echo "(fix the variables below, or pass --allow-skips):"
  echo "$problems"
  return 1
}

# Size of directory $1 in whole GB (rounded down); 0 if it does not exist.
size_gb() {
  [ -d "$1" ] || { echo 0; return; }
  echo $(( $(du -sk "$1" 2>/dev/null | cut -f1) / 1024 / 1024 ))
}

# Keeps the queue's target/ under $TARGET_MAX_GB: removes target/debug/incremental and then, if
# still over, target/debug, logging what went. Under --dry-run it says what it would remove.
prune_target() {
  local t=$QUEUE_TARGET total d sz dry_removed=0
  [ -d "$t" ] || return 0
  total=$(size_gb "$t")
  if [ "$total" -lt "$TARGET_MAX_GB" ]; then
    echo "target/ is $total GB (limit $TARGET_MAX_GB GB): kept"
    return 0
  fi
  for d in "$t/debug/incremental" "$t/debug"; do
    [ -d "$d" ] || continue
    sz=$(size_gb "$d")
    if [ $DRY = 1 ]; then
      # debug/ holds debug/incremental, which the step before would already have removed.
      sz=$((sz - dry_removed))
      echo "dry run: target/ is $total GB (limit $TARGET_MAX_GB GB); a gate would remove $d ($sz GB)"
      total=$((total - sz)) dry_removed=$((dry_removed + sz))
    else
      rm -rf "$d"
      echo "target/ was $total GB (limit $TARGET_MAX_GB GB): removed $d ($sz GB)"
      total=$(size_gb "$t")
    fi
    [ "$total" -lt "$TARGET_MAX_GB" ] && break
  done
  [ "$total" -lt "$TARGET_MAX_GB" ] \
    || echo "warning: target/ is still $total GB (limit $TARGET_MAX_GB GB) after pruning; nothing else is removed"
  return 0
}

require_pg "start the queue" || exit 1

git -C "$ROOT" fetch -q --prune origin || exit 1
if [ "${1:-}" = "--all" ]; then
  set -- $(git -C "$ROOT" branch -r --no-merged origin/main --format='%(refname:lstrip=3)' | grep -v '^HEAD$')
fi
[ $# -gt 0 ] || { echo "nothing to merge"; exit 0; }
[ -d "$W" ] || git -C "$ROOT" worktree add -q --detach "$W" origin/main || exit 1

take_lock() { [ $DRY = 1 ] || until mkdir "$LOCK" 2>/dev/null; do sleep 30; done; }
drop_lock() { [ $DRY = 1 ] || rmdir "$LOCK" 2>/dev/null; }

# Runs the full gate on the worktree's HEAD, logging to $1. Prints the reason and returns 3-6
# on failure, 10 when PostgreSQL is not ready. Keeps target/ under its limit first. Prints the
# plan instead under --dry-run.
gate() {
  local log=$1
  prune_target
  require_pg || return 10
  if [ $DRY = 1 ]; then
    [ $DRY_FAIL = 1 ] && { echo "dry run: gate of $(git rev-parse --short HEAD) assumed to fail"; return 5; }
    echo "dry run: would gate $(git rev-parse --short HEAD)"; return 0
  fi
  cargo fmt --all --check > "$log" 2>&1 || { echo "fmt failed"; return 3; }
  cargo clippy --workspace --all-targets -- -D warnings >> "$log" 2>&1 || { echo "clippy failed"; return 4; }
  if ! cargo test --workspace --all-targets --no-fail-fast >> "$log" 2>&1; then
    grep -E '^test .* FAILED$' "$log" | head -10; echo "tests failed"; return 5
  fi
  if git diff --name-only origin/main HEAD | grep -q '^web/'; then
    (cd web && { [ -d node_modules ] || npm ci --silent; } && npm run check && npm run test:e2e) >> "$log" 2>&1 \
      || { echo "web checks failed"; return 6; }
  fi
  return 0
}

# Pushes the worktree's HEAD to main unless main moved under the gate (7-9 on failure), then
# deletes the branches named as arguments. Prints the plan instead under --dry-run, and only
# reports under --no-push.
push_main() {
  if [ $NO_PUSH = 1 ]; then
    [ $DRY = 1 ] && { echo "dry run: --no-push: would push nothing and delete nothing"; return 0; }
    echo "--no-push: the gate of $(git rev-parse --short HEAD) passed; nothing pushed, $* kept"
    return 0
  fi
  git fetch -q origin
  if [ "$(git merge-base HEAD origin/main)" != "$(git rev-parse origin/main)" ]; then
    if git diff --name-only HEAD...origin/main | grep -qv '^docs/'; then echo "main moved (code) during the gate"; return 7; fi
    git rebase -q origin/main || { git rebase --abort; echo "rebase conflict after a docs change"; return 8; }
  fi
  if [ $DRY = 1 ]; then echo "dry run: would push $(git rev-parse --short HEAD) to main and delete $*"; return 0; fi
  git push -q origin HEAD:main || { echo "push refused"; return 9; }
  local b
  for b in "$@"; do git push -q origin --delete "$b"; done
  echo "merged as $(git rev-parse --short HEAD)"
}

# One branch, with the lock held by the caller.
merge_one_locked() {
  local b=$1 log="$W/target/merge-gate-${1//\//-}.log"
  mkdir -p "$W/target"
  cd "$W" || return 1
  git fetch -q origin || return 1
  git rev-parse -q --verify "origin/$b^{commit}" >/dev/null || { echo "no such branch"; return 1; }
  git checkout -q -- . && git checkout -q --detach "origin/$b" || return 1
  git rebase -q origin/main || { git rebase --abort; echo "rebase conflict with main"; return 2; }
  gate "$log" || return $?
  push_main "$b"
}

merge_one() {
  local rc
  take_lock
  merge_one_locked "$1"; rc=$?
  drop_lock
  return $rc
}

# Stacks the comma-joined group $1 onto origin/main, one branch onto the next, and gates the
# stack once; the lock is held by the caller. Leaves the stacked branches in STACKED.
merge_group_locked() {
  local group=$1 log="$W/target/merge-gate-${1//[\/,]/-}.log" b base last=main
  STACKED=()
  mkdir -p "$W/target"
  cd "$W" || return 1
  git fetch -q origin || return 1
  base=origin/main
  for b in $(echo "$group" | tr , ' '); do
    if ! git rev-parse -q --verify "origin/$b^{commit}" >/dev/null; then
      echo "$b: no such branch; left out of the group"; continue
    fi
    git checkout -q -- . && git checkout -q --detach "origin/$b" || return 1
    if git rebase -q "$base" >/dev/null 2>&1; then
      STACKED[${#STACKED[@]}]=$b; base=$(git rev-parse HEAD); last=$b
      echo "$b: stacked, $(git rev-list --count origin/main..HEAD) commits over main"
    else
      git rebase --abort
      echo "$b: rebase conflict with $last; left out of the group"
    fi
  done
  [ ${#STACKED[@]} -gt 0 ] || { echo "nothing stacked"; return 2; }
  git checkout -q --detach "$base"
  gate "$log" || return $?
  push_main "${STACKED[@]}"
}

merge_group() {
  local rc b
  take_lock
  merge_group_locked "$1"; rc=$?
  drop_lock
  case $rc in
    3|4|5|6)
      echo "the group's gate failed; each branch of it is gated alone"
      for b in "${STACKED[@]}"; do
        echo "=== $(date +%H:%M) $b (from the group)"
        merge_one "$b"
      done ;;
  esac
  return $rc
}

for b in "$@"; do
  echo "=== $(date +%H:%M) $b"
  case $b in
    *,*) merge_group "$b" ;;
    *) merge_one "$b" ;;
  esac
done
echo "=== $(date +%H:%M) done; still unmerged:"
git -C "$ROOT" fetch -q --prune origin
git -C "$ROOT" branch -r --no-merged origin/main
