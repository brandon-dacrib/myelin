#!/bin/bash
# Merges finished agent branches into main, one at a time, each only if the full gate passes.
#
# Usage (from anywhere in the repository):
#   tools/merge-queue.sh agent/two-pod-cluster-2 agent/federation-media ...
#   tools/merge-queue.sh --all     # every origin branch not yet merged into origin/main
#
# For each branch: take the merge lock (.git/myelin-merge.lock in the main checkout, shared with
# any agent following AGENTS.md), check the branch out in one reused worktree
# (.claude/worktrees/merge-queue, so its target/ stays warm), rebase onto origin/main, run
# fmt, clippy, `cargo test --workspace --all-targets` and, if web/ changed, `npm run check` and
# `npm run test:e2e`, then push to main, delete the origin branch and release the lock. A branch
# that fails anything stays on origin untouched and the queue moves on; the reason is printed and
# the full log is in the worktree's target/merge-gate-<branch>.log.
#
# Set HS_CLUSTER_TEST_POSTGRES_DSN to a PostgreSQL whose user may create databases, or the
# two-replica test in crates/hs-cli/tests/cluster_admin.rs prints SKIP and passes. For example:
#   docker run --rm -d --name hs-merge-queue-pg -e POSTGRES_PASSWORD=hspg \
#     -p 127.0.0.1:5462:5432 public.ecr.aws/docker/library/postgres:17
#   export HS_CLUSTER_TEST_POSTGRES_DSN=postgres://postgres:hspg@127.0.0.1:5462/postgres
#
# If main moves during a gate and the new commits touch only docs/, the branch is rebased again
# and pushed; if they touch code, the branch is left for the next run.
set -u
ROOT=$(git rev-parse --path-format=absolute --git-common-dir) || exit 1
ROOT=${ROOT%/.git}
LOCK=$ROOT/.git/myelin-merge.lock
W=$ROOT/.claude/worktrees/merge-queue
[ -f "$HOME/.cargo/env" ] && . "$HOME/.cargo/env"
export CARGO_PROFILE_DEV_DEBUG=0
[ -n "${HS_CLUSTER_TEST_POSTGRES_DSN:-}" ] || echo "warning: HS_CLUSTER_TEST_POSTGRES_DSN unset; cluster_admin's two-replica test will skip"

git -C "$ROOT" fetch -q --prune origin || exit 1
if [ "${1:-}" = "--all" ]; then
  set -- $(git -C "$ROOT" branch -r --no-merged origin/main --format='%(refname:lstrip=3)' | grep -v '^HEAD$')
fi
[ $# -gt 0 ] || { echo "nothing to merge"; exit 0; }
[ -d "$W" ] || git -C "$ROOT" worktree add -q --detach "$W" origin/main || exit 1

merge_one() {
  local b=$1 log
  log="$W/target/merge-gate-${b//\//-}.log"
  mkdir -p "$W/target"
  until mkdir "$LOCK" 2>/dev/null; do sleep 30; done
  trap 'rmdir "$LOCK" 2>/dev/null' RETURN
  cd "$W" || return 1
  git fetch -q origin && git checkout -q --detach "origin/$b" || { echo "no such branch"; return 1; }
  git rebase -q origin/main || { git rebase --abort; echo "rebase conflict with main"; return 2; }
  cargo fmt --all --check > "$log" 2>&1 || { echo "fmt failed"; return 3; }
  cargo clippy --workspace --all-targets -- -D warnings >> "$log" 2>&1 || { echo "clippy failed"; return 4; }
  if ! cargo test --workspace --all-targets --no-fail-fast >> "$log" 2>&1; then
    grep -E '^test .* FAILED$' "$log" | head -10; echo "tests failed"; return 5
  fi
  if git diff --name-only origin/main HEAD | grep -q '^web/'; then
    (cd web && { [ -d node_modules ] || npm ci --silent; } && npm run check && npm run test:e2e) >> "$log" 2>&1 \
      || { echo "web checks failed"; return 6; }
  fi
  git fetch -q origin
  if [ "$(git merge-base HEAD origin/main)" != "$(git rev-parse origin/main)" ]; then
    if git diff --name-only HEAD...origin/main | grep -qv '^docs/'; then echo "main moved (code) during the gate"; return 7; fi
    git rebase -q origin/main || { git rebase --abort; echo "rebase conflict after a docs change"; return 8; }
  fi
  git push -q origin HEAD:main || { echo "push refused"; return 9; }
  git push -q origin --delete "$b"
  echo "merged as $(git rev-parse --short HEAD)"
}

for b in "$@"; do
  echo "=== $(date +%H:%M) $b"
  merge_one "$b"
done
echo "=== $(date +%H:%M) done; still unmerged:"
git -C "$ROOT" fetch -q --prune origin
git -C "$ROOT" branch -r --no-merged origin/main
