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
# Set HS_CLUSTER_TEST_POSTGRES_DSN to a PostgreSQL whose user may create databases, or the
# two-replica test in crates/hs-cli/tests/cluster_admin.rs prints SKIP and passes. For example:
#   docker run --rm -d --name hs-merge-queue-pg -e POSTGRES_PASSWORD=hspg \
#     -p 127.0.0.1:5462:5432 public.ecr.aws/docker/library/postgres:17
#   export HS_CLUSTER_TEST_POSTGRES_DSN=postgres://postgres:hspg@127.0.0.1:5462/postgres
#
# If main moves during a gate and the new commits touch only docs/, the branch is rebased again
# and pushed; if they touch code, the branch is left for the next run.
#
# Written for the bash 3.2 that macOS ships.
set -u
ROOT=$(git rev-parse --path-format=absolute --git-common-dir) || exit 1
ROOT=${ROOT%/.git}
LOCK=$ROOT/.git/myelin-merge.lock
W=$ROOT/.claude/worktrees/merge-queue
[ -f "$HOME/.cargo/env" ] && . "$HOME/.cargo/env"
export CARGO_PROFILE_DEV_DEBUG=0
DRY=0 DRY_FAIL=0
case ${1:-} in
  --dry-run) DRY=1; W=$W-dry; shift ;;
  --dry-run=fail) DRY=1 DRY_FAIL=1; W=$W-dry; shift ;;
esac
[ -n "${HS_CLUSTER_TEST_POSTGRES_DSN:-}" ] || [ $DRY = 1 ] \
  || echo "warning: HS_CLUSTER_TEST_POSTGRES_DSN unset; cluster_admin's two-replica test will skip"

git -C "$ROOT" fetch -q --prune origin || exit 1
if [ "${1:-}" = "--all" ]; then
  set -- $(git -C "$ROOT" branch -r --no-merged origin/main --format='%(refname:lstrip=3)' | grep -v '^HEAD$')
fi
[ $# -gt 0 ] || { echo "nothing to merge"; exit 0; }
[ -d "$W" ] || git -C "$ROOT" worktree add -q --detach "$W" origin/main || exit 1

take_lock() { [ $DRY = 1 ] || until mkdir "$LOCK" 2>/dev/null; do sleep 30; done; }
drop_lock() { [ $DRY = 1 ] || rmdir "$LOCK" 2>/dev/null; }

# Runs the full gate on the worktree's HEAD, logging to $1. Prints the reason and returns 3-6
# on failure. Prints the plan instead under --dry-run.
gate() {
  local log=$1
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
# deletes the branches named as arguments. Prints the plan instead under --dry-run.
push_main() {
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
  git checkout -q --detach "origin/$b" || return 1
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
    git checkout -q --detach "origin/$b" || return 1
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
