# Repository Guidelines

## Project Structure & Module Organization

Myelin is a Rust Matrix homeserver with a TypeScript management interface. Rust workspace crates live in `crates/` (for example, `hs-room`, `hs-http`, and the `hs` binary in `hs-cli`). The web app is in `web/src/`; its mock API fixtures are in `web/src/mocks/`, and browser tests are in `web/e2e/` and `web/e2e-real/`. Cross-project conformance and oracle harnesses live in `tests/`. Deployment files are in `deploy/`; architecture decisions, RFCs, workstream briefs, and current status are in `docs/`. Read `PLAN.md` and `docs/next-steps.md` before changing a major interface.

## Build, Test, and Development Commands

- `cargo build --workspace` builds the Rust workspace; `cargo run -p hs-cli --bin hs -- serve --data-dir ./data --server-name example.org` starts a local server.
- `cargo fmt --all --check` checks Rust formatting; `cargo clippy --workspace --all-targets -- -D warnings` enforces the CI lint gate.
- `cargo test --workspace --all-targets` runs Rust tests; `cargo test --workspace --doc` runs documentation tests.
- In `web/`, run `npm ci` once, `npm run dev:mock` for a mock-backed UI, and `npm run check` for lint, types, unit tests, and production build. `npm run test:e2e` runs mock-backed Playwright flows.

## Coding Style & Naming Conventions

Use Rust edition 2024 and `rustfmt` defaults (four-space indentation). Crate names use `hs-` and Rust files/modules use `snake_case`. Put shared dependency versions in workspace `Cargo.toml` and reference them with `workspace = true`. Public Rust items need doc comments; library code should return errors rather than use unexplained `unwrap` or `expect`. In `web/`, use TypeScript/React conventions, co-locate component tests as `*.test.tsx`, and run ESLint plus Prettier (`npm run lint`).

## Testing Guidelines

Add focused tests with behavior changes. Rust crate tests and integration tests under `crates/*/tests/` use the shared `hs-testkit` where appropriate. UI unit and component tests use Vitest, Testing Library, and MSW; browser flows use Playwright. No numeric coverage threshold is documented. For protocol changes, consult the relevant harness in `tests/` and update the corresponding `docs/status/` entry with verified results.

## Commit & Pull Request Guidelines

Recent commits use short, descriptive sentences about the behavior or bug, without a fixed prefix (for example, “A client's own join is in its very next /sync, every time”). Keep commits focused. Pull requests should explain the change, link relevant issues or RFCs, list checks run, and include screenshots for visible web UI changes. Call out API or cross-crate interface changes and update the relevant decision or status document.

Work does not stay in branches. When a piece of work is done, it is merged into `main` and `main` is pushed; the branch is then deleted, locally and on origin. Done means all of these are true:

- **It works.** Verified against the real `hs` binary, or on the cluster for cluster work, not only against mocks or unit seams.
- **It has docs.** The track's `docs/status/` file and `docs/next-steps.md` say what changed and what is left, and public items have doc comments.
- **It is observable.** New behavior has logs, metrics or traces where an operator would need them.
- **Its tests pass.** `cargo fmt --all --check`, `cargo clippy --workspace --all-targets -- -D warnings` and `cargo test --workspace --all-targets` for Rust changes; `npm run check` and `npm run test:e2e` for `web/` changes.

Before merging, rebase onto (or merge) the latest `origin/main` and run the checks again. When several agents work in parallel they merge one at a time. Work that is not done is reported as not done, with what is left, and is never left quietly in a branch.

## Working in Parallel

These rules come from runs of nine agents at once (2026-09-28), where finished work sat unmerged for hours:

- **Agents don't wait for the merge lock.** A subagent that waits a long time is sent back by the harness before it merges. When an agent's work is done, it commits, pushes `agent/<name>` to origin and reports. The coordinator merges serially with `tools/merge-queue.sh` (or `--all`). That script takes `.git/myelin-merge.lock`, rebases, runs the full gate, pushes to `main` and deletes the branch. Branches in disjoint crates can share one gate: `tools/merge-queue.sh agent/a,agent/b` stacks them and gates the stack once, and gates each alone if that fails; `--dry-run` shows what a plan would stack without touching the queue's worktree or the lock.
- **Merge as soon as work is done, not at the end.** Branches merged in a batch at the end conflicted with each other in shared files (`hs-admin`'s router and `lib.rs`, `hs-cli`'s `serve.rs`, the status files). A usage limit mid-run then stranded them. Each merge makes the next branch's rebase smaller.
- **Run the full workspace gate only under the lock.** Iterate with `cargo test -p <crate>`. Seven concurrent `cargo test --workspace` runs made each take 40+ minutes and made real-binary tests time out at boot. Keep a separate `target/` per worktree, and build with `CARGO_PROFILE_DEV_DEBUG=0`.
- **Set `HS_CLUSTER_TEST_POSTGRES_DSN` for the gate.** Without it, the two-replica test in `crates/hs-cli/tests/cluster_admin.rs` prints `SKIP` and passes. Use a PostgreSQL whose user can create databases (see the script's header).
- **Never `pkill -f` a pattern that other agents' processes also match** (`vitest`, `playwright`, the lock-wait loop). Kill your own PIDs.
- **Installs and long builds belong to a background agent**, not the coordinating session.
- **An agent closes everything it opened before it reports, and the coordinator removes its worktree once its branch is merged.** On 2026-10-01 a day-old `hs serve`, five Playwright Chromium processes (one at 78% CPU for 23 hours) and 28 worktrees holding 390 GB of `target/` were found from agents that had long since finished. Before reporting: kill every process you started (`hs`, test binaries, Playwright and Chromium, dev servers), stop and remove your containers and volumes, and check `ps` shows none of your PIDs. After the merge: `git worktree remove` the agent's worktree and `git worktree prune`. Stragglers are not welcome.
- **The handover lists every unmerged branch.** Before a session ends, `docs/next-steps.md` names each branch from `git branch -r --no-merged origin/main`, what it holds and how far its gate got.

## The Owner's Desktop

- **Docker works, and is how to get PostgreSQL** for tests. `docker pull` from Docker Hub fails in agent sessions because the credential helper needs the keychain; use the ECR mirror instead: `public.ecr.aws/docker/library/postgres:17`.
- **Homebrew `kubectl`, `helm` and `talosctl` cannot reach the verification cluster** (`admin@dacrib0`) from Claude Code: "no route to host", from macOS's Local Network permission for the app. Apple's `curl` and `nc` reach it. Don't retry the CLIs. Work that needs the cluster is a desktop item for the owner's terminal: the owner runs port-forwards and `helm`, and the session drives the scripts in `deploy/two-pod/` against `localhost`.
- **Cold boots are slow under load.** A debug `hs` can take 30–60 s to start, so real-binary harnesses allow 120 s.
