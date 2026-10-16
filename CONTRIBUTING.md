# Contributing

Thanks for looking. Myelin is a Matrix homeserver in Rust with a TypeScript management
interface; `AGENTS.md` is the full set of working rules for the repository and this page is the
short version for a first pull request.

## Building

- Rust: `rust-toolchain.toml` pins stable with `rustfmt` and `clippy`.
  `cargo build --workspace` builds everything;
  `cargo run -p hs-cli --bin hs -- serve --data-dir ./data --server-name example.org` starts a
  server, and its log ends with the setup link that makes the first administrator.
- Web interface: in `web/`, `npm ci` once, then `npm run dev:mock` for a mock-backed interface
  or `npm run dev` against a running server. `npm run build` is what the Rust build embeds
  (`crates/hs-admin/build.rs`, when `HS_ADMIN_WEB_DIST` names the output).
- Image: `docker buildx build -f deploy/Dockerfile -t myelin:dev .`

## Before you open a pull request

Run what CI runs:

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --all-targets
cargo test --workspace --doc
cd web && npm run check && npm run test:e2e
```

Some tests need a PostgreSQL (they print `SKIP` and pass without one): point
`HS_KV_TEST_POSTGRES_DSN` and `HS_CLUSTER_TEST_POSTGRES_DSN` at a server whose user can create
databases (`docker run -d -p 5432:5432 -e POSTGRES_PASSWORD=pg public.ecr.aws/docker/library/postgres:17`).

## What a change needs

- **Tests with it.** Behaviour changes come with focused tests; a server-level behaviour is
  verified against the real `hs` binary (`crates/hs-cli/tests/`, with `hs-testkit`), not only
  against a mock. A test that starts a server reserves its port with
  `hs_testkit::ports::reserve()` and hands the bound listener over.
- **Docs with it.** Public Rust items have doc comments. A change an operator would notice is a
  `CHANGELOG.md` entry under `Unreleased`, written as what the server can do now and how that was
  verified. A change to a setting regenerates `docs/config.md`
  (`cargo run -p hs-config --bin gen_config_docs`). A change to the admin API bumps the OpenAPI
  document's version and regenerates the web client (`npm run generate:client` in `web/`).
- **Observability with it.** New behaviour an operator would need to see has a log line, a
  metric or a span.
- **Honesty about what was verified.** A pull request says what it ran. "Implemented" and
  "works" are different claims here, and the status files in `docs/status/` and the README's
  table are kept to what was measured.

## Style

- Rust edition 2024, `rustfmt` defaults, no warnings under `clippy -D warnings`. Library code
  returns errors rather than `unwrap`ping. Crate names are `hs-*`; files and modules
  `snake_case`. Shared dependency versions live in the workspace `Cargo.toml`.
- Web: TypeScript and React, ESLint and Prettier (`npm run lint`), component tests beside the
  component as `*.test.tsx`, Playwright flows in `web/e2e/` (mock) and `web/e2e-real/` (real
  server).
- Commits are short descriptive sentences about the behaviour, with no prefix:
  "A client's own join is in its very next /sync, every time". One change per commit.
- Pull requests explain the change, link an issue, RFC or decision where there is one, list the
  checks run, and include screenshots for a visible change to the interface. A change to a
  cross-crate interface or a decision gets a note in `docs/decisions/` (dated, numbered) or
  `docs/rfcs/`.

## Where things are

- `crates/`: the Rust workspace. `crates/hs-cli` is the `hs` binary; the others are one
  concern each (`hs-room`, `hs-sync`, `hs-federation`, `hs-auth`, `hs-media`, `hs-admin`, …).
- `web/`: the management interface. `deploy/`: the image, Helm chart, operator and examples.
- `tests/`: the Complement, Sytest, Synapse-interop and load harnesses.
- `docs/`: `docs/README.md` is the index; `PLAN.md` the design; `docs/next-steps.md` the
  current state and what is next; `docs/workstreams/` the sixteen areas the work is split into.

## License

Contributions are accepted under the Apache-2.0 license (`LICENSE`). Code from AGPL-licensed
projects (Synapse, Matrix Authentication Service) is not copied here; they are behavioural
references only. MIT and Apache-2.0 sources may be adapted with a comment naming the source and
its license (`docs/decisions/0001-license.md`).

## Reporting

Bugs and questions go to [GitHub issues](https://github.com/brandon-dacrib/myelin/issues) with
the version, the layout and the log lines. Security problems go through `SECURITY.md`.
