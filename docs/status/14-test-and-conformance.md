# 14 Test and conformance (integration lead): status

Track brief: `docs/workstreams/14-test-and-conformance.md`. Owner crates: `hs-testkit`,
`hs-spec-coverage`, `hs-loadgen` (not started, see "Decisions made"), `tests/` (Complement, Sytest,
differential, oracle harnesses).

Last updated: 2026-09-18 (day one, session 1). A previous attempt was interrupted before writing
anything; this session started from the empty placeholders it left behind.

## Done

- **`hs-testkit`** (`crates/hs-testkit/`): fake clock ([`clock::FakeClock`], compatible with
  `tokio::time`'s paused-time mode — reads `tokio::time::Instant`, so it advances exactly when
  `tokio::time::advance` does); the scenario DSL ([`scenario::Scenario`], generic over any
  `axum::Router<()>` via `tower::ServiceExt::oneshot`, tracking named users' `user_id`,
  `access_token`, `device_id`, `refresh_token` across calls, with `register`/`login`/`whoami`/
  `refresh`/`logout`/`sync`/`send` verbs); Matrix error construction/assertion helpers
  (`matrix_error::MatrixErrorExpectation`); an append-only `RecordLog` built on
  `hs_kv::memory::MemoryBackend` (per this track's brief: "use it in the testkit rather than
  writing your own fake store") that every fake sink below is built on; a fake appservice receiver
  (`PUT /transactions/{txnId}`), a fake federation peer (records any request, answers queued or
  default responses), a fake push gateway (`POST /_matrix/push/v1/notify`, configurable rejected
  pushkeys), and a fake SMTP sink (in-memory recorder, no protocol-level SMTP server — see that
  module's doc comment for why). 19 unit tests plus an integration test
  (`tests/hs_auth_round_trip.rs`) that drives `hs-auth`'s real router (`hs_auth::routes::router()`)
  through register → whoami → register (second user) → login → whoami → refresh → whoami →
  logout → whoami(fails, `M_MISSING_TOKEN`) → whoami with an injected bogus token
  (`M_UNKNOWN_TOKEN`), plus two more tests for wrong-password and double-registration error
  shapes. `cargo fmt` clean, `cargo clippy -p hs-testkit --all-targets -- -D warnings` clean, all
  19 tests pass.
- **`hs-spec-coverage`** (`crates/hs-spec-coverage/`): a library plus binary. `spec` parses the
  top-level `*.yaml` files under each of the five `refs/matrix-spec/data/api/<family>/` directories
  (client-server, server-server, application-service, identity, push-gateway), reading each file's
  own `servers[0].variables.basePath.default` and prepending it to every `paths` entry, so a
  parsed route's `path` is already what a router would register (`/_matrix/client/v3/login`, not
  `/login`). `manifest` is this crate's own copy of the `routes.json` schema from
  `docs/rfcs/0005-routes-json-manifest.md` (RFC 0005, already drafted and largely implemented by
  track 15's `hs-http::router::Builder`/`RouteManifest` before this session started — see
  "Decisions made"); its test suite includes the RFC's own example JSON parsed verbatim. `coverage`
  diffs the two by exact `(method, path)` match per family/surface, producing registered/missing/
  extra counts. `report` renders Markdown. The `hs-spec-coverage` binary (`--spec-dir --routes
  --out --json-out --missing-limit --fail-on-missing`) ties it together. 21 unit tests plus
  `tests/real_spec.rs` (6 tests) against the actual `refs/matrix-spec` checkout already present in
  this workspace: parses 235 routes across the five APIs, confirms the well-known legacy auth
  paths and a federation `send` route are present with the right base path, and — the most
  concrete demonstration — diffs the spec against a hand-built manifest of `hs-auth`'s 16 real
  registered routes, showing 15 registered (all real spec routes) and one genuine "extra"
  (`GET /password_policy`, which `hs-auth` serves but does not appear in this spec checkout — a
  real, tool-caught mismatch, not a bug; see "Decisions made"). `cargo fmt`/`clippy -D warnings`
  clean, all 42 tests pass. Ran the binary directly against the real spec tree: 0/235 routes
  registered (0.0%) with no manifest supplied, which is the honest answer on a day nothing mounts
  the Matrix surfaces yet.
- **`tests/differential/`**: the recording proxy (`lib/proxy.py`, a real forwarding HTTP proxy
  logging every request/response pair to JSONL — point any client at it, not just this harness's
  own driver), normalizers for nondeterministic fields (`lib/normalize.py`: timestamps, event IDs,
  room IDs, tokens, and unordered scalar arrays, with two independent `Normalizer` instances per
  diff so relationships between IDs within one run are preserved through matching placeholders),
  the workload driver (`lib/driver.py`, `{{var}}` substitution and JSONPath-lite extraction between
  steps), the diff report (`lib/diff.py`), `record.py` (drives a workload through the proxy to
  produce a baseline) and `replay.py` (runs a workload directly against a candidate and diffs
  against a recorded baseline, positionally aligned by construction — see that file's docstring),
  and the five first workloads (`workloads/{registration,room_creation,messaging,membership,
  sync}.json`). `run_differential.py` detects Docker is off here (`docker info` fails) and exits 0
  with an explanation; confirmed by running it. The harness's own correctness is proven
  independently of Docker/Synapse by `tests/test_pipeline.py` (5 tests, using two small local
  `http.server` instances standing in for "two homeservers," one deliberately corrupted to confirm
  real divergences are caught) plus `tests/test_normalize.py` (9 tests) — 14 tests total, all pass
  (`python3 -m unittest discover -s tests/differential/tests`). `README.md` documents the exact
  Synapse 1.161 baseline-recording procedure for when Docker is available.
- **`tests/complement/`**: `Dockerfile.template` (multi-stage: `cargo build --release -p
  hs-server` placeholder, then a runtime image with `openssl`/`curl`, `EXPOSE 8008 8448`, a
  `HEALTHCHECK` against `/_matrix/client/versions`), `startup.sh` (signs a federation TLS cert
  against Complement's mounted `/complement/ca/{ca.crt,ca.key}` using the exact `openssl` recipe
  `refs/complement/README.md`'s "Complement PKI" section documents, trusts that CA itself, then
  execs the placeholder server invocation), `build.sh`/`run_single_node.sh` (Docker/Go/checkout
  precondition checks, clean skip if any is missing — confirmed: skips here), `run_cluster.sh` (a
  documented seam for track 03/12's cluster topology, not a working implementation — confirmed:
  skips here), `blacklist.txt` + `skip_regex.sh` (a runtime `go test -skip` regex instead of
  Complement's per-implementation build-tag convention, which needs upstream registration this
  server doesn't have). **Untested**: no `hs-server` binary exists to build an image from.
- **`tests/sytest/`**: a real Sytest plugin using Sytest's actual `Module::Pluggable` discovery
  mechanism (`SYTEST_PLUGINS=.../plugins`, each plugin a `<name>/lib/SyTest::HomeserverFactory::*`
  + `SyTest::Homeserver::*` pair) —
  `plugins/hs-reimplement/lib/SyTest/{HomeserverFactory,Homeserver}/HsReimplement.pm`, adapted in
  shape (with attribution) from `refs/sytest/lib/SyTest/Homeserver/Dendrite.pm` (Apache-2.0), the
  closest existing analog (a single monolith binary, not a Python framework). `run_sytest.sh`
  checks for the Sytest checkout, Perl, Sytest's CPAN dependencies, and an `hs-server` binary,
  skipping cleanly if any is missing — confirmed: skips here (Sytest's CPAN deps are not
  installed). Brace/paren-balance checked (`perl -c` itself needs the CPAN deps this environment
  lacks, so full compilation was not verified — see "Decisions made").
- **`tests/oracle/`**: `state_oracle.py` (drives `synapse.state.v2.resolve_events_with_store`,
  with an in-memory `StateResolutionStore` implementation whose auth-chain-difference algorithm is
  the Matrix spec's own definition, adapted in shape from Synapse's own test helper) and
  `push_oracle.py` (drives `synapse.synapse_rust.push.PushRuleEvaluator`), two illustrative
  fixtures, and `run_oracle.sh`. **Never executed end to end**: both need an installed
  `matrix-synapse` package (its compiled Rust extension — `synapse.api.room_versions` and push
  evaluation both moved out of pure Python in this Synapse checkout), which needs network this
  environment does not have. Both scripts detect `ModuleNotFoundError` and exit 0 with setup
  instructions — confirmed: both skip cleanly. `py_compile` clean; fixtures are valid JSON.
- **`tools/dashboard.py`**: the parity dashboard generator. Parses every `docs/status/*.md` file
  (title, `Last updated:`/`Updated:` line, Done/In progress/Blockers bullet counts — handles both
  phrasings other tracks use), runs `hs-spec-coverage` as a subprocess against the real spec tree
  (using any `routes.json` found in the repo, or an empty manifest if none exists — none does
  yet), and detects each `PLAN.md` section 12 layer's status (`live` / `scaffold, untested` /
  `not started`) from what exists on disk rather than a hand-maintained table. Ran it for real:
  `docs/status/dashboard.md` now exists (58 lines as of this writing), showing 0/235 spec routes
  registered, L1–L2 live, L3–L5 scaffolded-and-honestly-labeled, L6–L12 not started, and every
  reporting track's Done/In-progress/Blockers counts.
- `hs-loadgen`: explicitly skipped per this session's instructions ("skip hs-loadgen unless
  everything above is done" — everything above *is* done, but `hs-loadgen` needs
  `matrix-rust-sdk`, called out as "a heavy dependency on this shared machine," and there is
  nothing running yet for a load generator to point at).

## In progress

Nothing left mid-change; every file above is in a working, tested (or honestly-marked-untested)
state.

## Next

- Once any track assembles a real `hs-server` (or equivalent) binary: fill in the `TODO`s in
  `tests/complement/Dockerfile.template`/`startup.sh` and `tests/sytest/plugins/hs-reimplement/
  lib/SyTest/Homeserver/HsReimplement.pm`, then actually run both suites.
- Once `hs-http`'s `Builder` (or `hs-admin-mock`) is wired into a binary that writes a real
  `routes.json`: point `hs-spec-coverage`/`tools/dashboard.py` at it and watch the coverage number
  move off 0%.
- When network is available: record a real Synapse 1.161 baseline (`tests/differential/README.md`)
  and validate the two `tests/oracle/` fixtures against an installed `matrix-synapse`.
- `hs-loadgen` once the rest of Phase 0 is further along and a server exists to load-test.
- Re-run `tools/dashboard.py` after any status file changes; it is meant to be regenerated often,
  not hand-edited.

## Blockers

None for this session's own scope. Everything above that is "untested"/"never executed" is
blocked on inputs outside this track's control: a server binary (every other track), network
access (`pip install matrix-synapse`, Sytest's CPAN deps), and Docker being turned on.

## Interfaces provided

- `hs-testkit`: `Scenario`, `FakeClock`, `RecordLog`, `FakeAppservice`, `FakeFederationPeer`,
  `FakePushGateway`, `FakeSmtpSink`, `matrix_error::{assert_matrix_error, MatrixErrorExpectation}`.
  Every crate in the workspace should write its integration tests against this
  (`docs/decisions/0002-workspace-conventions.md`).
- `hs-spec-coverage`: the `hs-spec-coverage` binary and its library (`ApiFamily`, `SpecRoute`,
  `RouteManifest`, `CoverageReport`, `render_markdown`) — any track can run
  `cargo run -p hs-spec-coverage -- --routes <their routes.json>` to see their own coverage.
- `docs/rfcs/0005-routes-json-manifest.md`'s `routes.json` format is the contract
  `hs-spec-coverage::manifest::RouteManifest` consumes; see "Decisions made" for the two surface
  values this crate adds to it.
- `tools/dashboard.py` / `docs/status/dashboard.md`: the project's one status report
  (`docs/workstreams/README.md` rule 5). Run `python3 tools/dashboard.py` after any status file
  changes.
- `tests/complement/`, `tests/sytest/`, `tests/differential/`, `tests/oracle/`: ready to receive a
  server binary; each has a `README.md` describing exactly what to fill in.

## Interfaces needed

- A server binary (whichever track/integration step assembles `hs-http`'s listener +
  `hs-auth`'s router + the rest into one process) to point Complement, Sytest, and the
  differential replayer at.
- `hs-http`'s or `hs-admin`'s `routes.json` output, once something writes one to disk (RFC 0005's
  format is already implemented in `hs-http::router`; nothing calls `write_to_file` yet from a
  binary this session could find).
- Track 03/12's cluster deployment manifests, for `tests/complement/run_cluster.sh` to actually
  drive (currently a documented seam only).
- Network access, for `tools/fetch-refs.sh`-cloned-but-unbuildable pieces: Sytest's CPAN
  dependencies and an installed `matrix-synapse` for `tests/oracle/`.

## Decisions made

- **`docs/rfcs/0005-routes-json-manifest.md` surface extension**: RFC 0005 (drafted by track 15
  before this track started, which the RFC itself anticipated: "track 14 may amend it once it
  starts") lists `matrix-client`, `matrix-federation`, `matrix-appservice`, `synapse-admin-compat`,
  `admin` as its `surface` enum, explicitly open for new values. `hs-spec-coverage` adds
  `matrix-identity` and `matrix-push-gateway` (`ApiFamily::manifest_surface`) for the two Matrix
  APIs this track's brief also requires coverage for that RFC 0005 did not enumerate (identity,
  push-gateway). No format change, no other track needs to do anything differently; recorded here
  as the RFC invited.
- **`hs-spec-coverage` defines its own `routes.json` schema** (`manifest.rs`) rather than
  depending on `hs-http` for `RouteManifest`. Keeps this crate consumable by any future
  `routes.json` producer, not just `hs-http::router::Builder`, and avoids a cross-track compile
  dependency for what is fundamentally a data-interchange format. Verified compatible with RFC
  0005's own example JSON (a unit test parses it verbatim) and with `hs-http::router`'s actual
  field names/enum casing (read, not depended on).
- **`tools/fetch-refs.sh`'s Synapse clone is unpinned** (`git clone --depth 1`, default branch, no
  `SYNAPSE_VERSION` tag): `tests/differential/README.md` and the project generally say "Synapse
  1.161," but the checkout in `refs/synapse` may already be ahead of that tag. `tests/oracle/`'s
  README calls this out explicitly (read the *installed* package's source when validating, not
  `refs/synapse`). Not fixed here (outside this track's file-ownership scope to change
  `tools/fetch-refs.sh` without checking who else depends on its current behavior) but worth
  another track or a later session pinning if reproducibility across sessions matters more than
  always testing against Synapse's latest development.
- **Synapse's push rule evaluation and room-version table are no longer pure Python**: as of the
  `refs/synapse` checkout in this workspace, `synapse.api.room_versions` re-exports from a
  compiled `synapse.synapse_rust.*` extension, and push rule evaluation
  (`synapse.synapse_rust.push.PushRuleEvaluator`) is entirely in that extension. This means
  `tests/oracle/`'s scripts need an *installed* `matrix-synapse` wheel, not just the `refs/synapse`
  source checkout other tracks read for behavior — recorded so a future session doesn't waste time
  trying to make the source-checkout-only path work.
- **`hs-loadgen` skipped this session**, per explicit instruction and because `matrix-rust-sdk` is
  a heavy dependency with nothing yet to load-test.
- **Fake SMTP sink is not a protocol-level SMTP server** (`hs-testkit::fake_smtp`): an in-memory
  recorder matching the minimal shape any future mailer trait needs, rather than carrying a real
  SMTP implementation as a dependency on a shared, resource-constrained machine for a feature no
  crate sends mail through yet.

## Shared dependencies added

None. `hs-testkit` and `hs-spec-coverage` use only workspace dependencies already present
(`axum`, `http`, `http-body-util`, `tower`, `tokio`, `serde`, `serde_json`, `serde_yaml_ng`,
`bytes`, `thiserror`, `tracing`) plus path dependencies on `hs-kv` (regular) and `hs-auth`
(dev-only, for the integration test). No new `[workspace.dependencies]` entries were needed.

## How to verify

```bash
# hs-testkit: fake clock, scenario DSL, fake doubles, and the hs-auth round-trip proof
cargo fmt -p hs-testkit -p hs-spec-coverage
cargo clippy -p hs-testkit --all-targets -- -D warnings
cargo test -p hs-testkit --all-targets   # 19 unit + 3 integration tests

# hs-spec-coverage: OpenAPI parsing, routes.json schema, coverage diff, Markdown report
cargo clippy -p hs-spec-coverage --all-targets -- -D warnings
cargo test -p hs-spec-coverage --all-targets   # 36 unit + 6 real-spec integration tests
cargo run -p hs-spec-coverage -- --spec-dir refs/matrix-spec/data/api   # 0/235 registered, honest

# Differential harness (no Docker/Synapse needed for these):
python3 -m unittest discover -s tests/differential/tests -v   # 14 tests
python3 tests/differential/run_differential.py                # clean skip, exit 0

# Complement / Sytest / oracle scaffolds (all clean-skip without their respective dependencies):
./tests/complement/build.sh; ./tests/complement/run_cluster.sh
./tests/sytest/run_sytest.sh
./tests/oracle/run_oracle.sh

# The parity dashboard:
python3 tools/dashboard.py && cat docs/status/dashboard.md
```
