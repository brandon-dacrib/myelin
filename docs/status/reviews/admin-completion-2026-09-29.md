# Admin integration completed, 2026-09-29

The three pending admin branches are merged into `main` and pushed. Final code: `12a19eb`.
Their local and remote branches are deleted; detached worktrees and verification logs are kept.
This closes the interrupted admin work recorded in
`docs/next-steps.md`; federation and cluster branches remain separate unfinished work.

## What shipped

- `agent/admin-followups` → `28d40dc`: reports filtered by person, live report/task updates,
  cancellable bulk media deletion, federation key operations and shared-room listing. A finished
  task can no longer be overwritten in the browser cache by its delayed starting response.
- `agent/config-history` → `eedb090`: per-setting history and revert in the API and interface,
  pagination, conflict detection, redacted secrets and an audited new revision for a revert.
  Decisions: 0015. Review found and fixed stale validation on both save and revert: another
  writer enabling MAS must prevent restoring incompatible OIDC settings, before anything is
  committed. Regression tests cover the failure.
- `agent/config-reload` → `12a19eb`: live message limits, federation domain/IP policies and log
  level; application/restart reports on saves and reverts; ten-second revision checks on other
  replicas. Failed application retries at the same revision, and readers registered after an
  early startup check receive pending changes. Decision: 0016. The rate-limit form can save one
  bucket field without supplying its other field.

The two original follow-up gate failures were outdated test expectations. The Overview now
counts media (zero on an empty server; two uploads / 19 bytes in the regression), and bulk
deletion now returns a task whose completion must be followed before checking its result.

## Verification

Each branch rebased onto the preceding `main` and passed the serial merge gate before being
pushed. The gate ran `cargo fmt --all --check`, workspace Clippy with `-D warnings`,
`cargo test --workspace --all-targets --no-fail-fast`, `npm run check`, and `npm run test:e2e`.

| Merged tip | Rust tests passed | Web unit tests passed | Mock browser flows passed |
|---|---:|---:|---:|
| `28d40dc` | 2,301 | 426 | 49 |
| `eedb090` | 2,329 | 441 | 50 |
| `12a19eb` | 2,343 | 443 | 50 |

The final Rust gate covered 92 test executables with no failed or ignored tests. PostgreSQL 17
was configured for the two-replica drain test; the final `cluster_admin` executable passed both
tests in 28.66 seconds. The real `hs` binary's history and reload suites passed too.
Separately, the real Configuration Playwright suite passed **5/5**. The browser rate-limit flow
checks both save and revert against application reports and metrics. Screenshot evidence:

- [History](../../design/screenshots/configuration-history-real.png)
- [Revert dialog](../../design/screenshots/configuration-revert-dialog-real.png)
- [Recorded revert](../../design/screenshots/configuration-reverted-real.png)
- [Rate limit applied](../../design/screenshots/config-rate-limits-applied.png)
- [Rate limit reverted](../../design/screenshots/config-rate-limits-reverted.png)

`python3 tools/admin_api_coverage.py` reports **160/160 operations with real handlers**.
This measures handler coverage, not full Matrix conformance. Full-gate logs are local in
`.claude/worktrees/merge-queue/target/merge-gate-agent-{admin-followups,config-history,config-reload}.log`;
queue transcripts are `/tmp/myelin-{admin-followups,config-history,config-reload}-queue.log`.

The broader parity dashboard was not regenerated: its spec-coverage input
`refs/matrix-spec/data/api/client-server` is absent in this checkout. It retains its earlier
dated measurement; no new protocol coverage or Complement result is claimed here.

## Still open

- `agent/two-pod-cluster-2` (`0ddf9da`) and `agent/federation-leftovers` (`8074aec`) remain on
  origin, unmerged. Their contents and verification state are listed at the top of
  `docs/next-steps.md`. No cluster deployment was performed in this wrap-up.
- Rate-limit buckets other than messages are not enforced. Message and override buckets are
  per replica. Changing listeners, storage and other startup-only settings still needs a restart.
- `RUST_LOG` takes precedence over the configured log level; applying that setting requires
  restarting without the override. Federation policy reload needs federation enabled at boot.
- Old history records without prior values cannot be reverted. Configuration history has no
  retention limit yet and keeps prior secrets in the same database as current secrets.
- Cross-section editing/validation in the interface and assisted storage-backend migration
  remain future admin work.
