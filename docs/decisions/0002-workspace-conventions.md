# 0002. Workspace conventions

Date: 2026-09-17. Status: accepted.

- Rust stable (1.98 at the time of writing), edition 2024, `rust-version` 1.91. `cargo fmt` and `cargo clippy -- -D warnings` are the bar.
- One shared `target/` directory. No track sets `CARGO_TARGET_DIR`; concurrent builds wait on the build lock, which is expected.
- Shared dependency versions live in `[workspace.dependencies]`; crates reference them with `workspace = true`. A track that needs a new shared dependency adds it there and notes it in its status file.
- Each track edits only its own crates and directories (`docs/workstreams/README.md`, rules of engagement), plus `docs/status/<track>.md`, new files under `docs/rfcs/` and `docs/design/`, and append-only entries under `docs/decisions/`.
- Tracks do not run git. The integration lead commits at checkpoints.
- Library code returns errors; no `unwrap` or `expect` outside tests and binaries without a comment justifying it. No `unsafe` without a justification comment and a test.
- Public items carry doc comments. Every crate has tests. Integration tests use `hs-testkit`.
