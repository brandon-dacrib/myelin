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
