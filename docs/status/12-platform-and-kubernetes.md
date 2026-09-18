# 12. Platform and Kubernetes

Last updated 2026-09-18, by the integration lead's extension of track 12 (added `crates/hs-cli`
ownership; see `.claude/agents/hs-12-platform.md` and the integration lead's assignment message).

## Integration review follow-up (same day)

The integration lead booted the built binary and found two real gaps plus one smaller one. All
three are fixed; verified by booting the binary and curling it (not just running the test suite
— see the transcript below, reproducible with the commands in "How to verify everything").

1. **Blocking: `GET /_matrix/client/versions` 404'd.** Nothing served it at all. Added
   `crates/hs-cli/src/versions.rs` (`GET /_matrix/client/versions`, config-driven
   `unstable_features`) and `crates/hs-cli/src/capabilities.rs` (`GET
   /_matrix/client/v3/capabilities` + `r0` alias), mounted in `crates/hs-cli/src/serve.rs`.
   `unstable_features` defaults to **empty**, deliberately: every flag `docs/synapse-inventory.md`
   and `PLAN.md` Appendix B list gates a feature this server does not implement yet (`hs serve`
   still only mounts `hs-auth`'s legacy routes plus these two). Advertising an unimplemented flag
   would send a bridge down a code path we cannot serve — see `versions.rs`'s module doc for the
   full reasoning. `unstable_features` is driven by an optional `hs serve
   --capabilities-config <path>` YAML file (cannot live in the main `hs-config` file: that schema
   denies unknown top-level keys, and this track does not own it — see "Interfaces needed").
   `capabilities` similarly reports honestly against what is actually mounted (`m.change_password:
   true`, everything else `false`, `m.room_versions` omitted).
2. **`GET /metrics` returned `# EOF` with no series.** `Metrics` existed and was served, but
   nothing ever called `record_http_request`. Added `crates/hs-cli/src/metrics_layer.rs`
   (`axum::middleware::from_fn_with_state` timing every request, labeled by the *matched* route
   template via `MatchedPath`) and wired it into the router. Verifying this by hand also caught a
   **second, real bug in `hs-telemetry` itself** (not something the integration review flagged
   directly — found while curling `/metrics` to confirm the fix): the `hs_http_requests_total`
   counter was registered *with* the `_total` suffix already in its name, but
   `prometheus_client`'s text encoder appends a literal `_total` to every `Counter` it renders
   unconditionally, so the wire output was `hs_http_requests_total_total`. Fixed in
   `crates/hs-telemetry/src/metrics.rs` (register as `"hs_http_requests"`, let the encoder add the
   suffix) and in `docs/decisions/0004-telemetry-conventions.md`; the existing unit test only
   checked `.contains("hs_http_requests_total")`, which the doubled name still satisfied as a
   prefix, so it passed right through the bug — tightened to check the exact metric name and
   assert the doubled form is absent.
3. **`routes.json` was never written**, so track 14's spec-coverage tool reported 0/235 routes.
   Rewrote `crates/hs-cli/src/serve.rs::build_router` to build through `hs_http::router::Builder`
   (already used by `hs-media` and `hs-admin`, per the coordinator's suggestion) instead of a bare
   `axum::Router`, added `crates/hs-cli/src/auth_manifest.rs` (a hand-mirrored, tested `Vec<Route>`
   for `hs-auth`'s pre-built router fragment, which does not itself go through `Builder`), and
   exposed `hs_cli::serve::route_manifest()` plus two ways to get it out: `hs serve
   --routes-manifest <path>` (written at startup) and the new `hs routes-manifest [-o <path>]`
   subcommand (no config, no server, no sockets — just the static route list).

**Verification transcript** (`hs serve -c config.yaml --routes-manifest routes.json`, then curled
by hand):

```
GET /_matrix/client/versions
{"versions":["r0.0.1",...,"v1.12"],"unstable_features":{}}

GET /_matrix/client/v3/capabilities
{"capabilities":{"m.change_password":{"enabled":true},"m.set_displayname":{"enabled":false},
"m.set_avatar_url":{"enabled":false},"m.3pid_changes":{"enabled":false}}}

(register + login, then:)

GET /metrics
hs_http_requests_total{method="POST",route="/_matrix/client/v3/register",status_class="2xx"} 1
hs_http_requests_total{method="POST",route="/_matrix/client/v3/login",status_class="2xx"} 1
hs_http_requests_total{method="GET",route="/_matrix/client/v3/capabilities",status_class="2xx"} 1
hs_http_requests_total{method="GET",route="/_matrix/client/versions",status_class="2xx"} 1
hs_http_request_duration_seconds_bucket{le="...",...} ...
# EOF

GET /health/live -> 200 "ok"
GET /health/ready -> 200 "ready"

routes.json: 38 routes written, e.g. {"method":"GET","path":"/_matrix/client/r0/capabilities",
"surface":"matrix-client","operation_id":"getCapabilities","auth":"none","rate_limited":false}

SIGTERM -> "shutdown signal received, draining connections", clean exit (144 = 128+SIGTERM).
```

Full command reproduction is in "How to verify everything" at the bottom of this file. 33 new/
changed tests across `hs-cli` (28 -> 47 unit, 4 -> 5 e2e) and `hs-telemetry` (tightened 1
existing test) — all passing; `cargo clippy ... -- -D warnings` clean on both crates.

## Done

- **`crates/hs-telemetry`** (full crate, not a skeleton):
  - `init` module: global `tracing` subscriber setup (`hs_telemetry::init`), JSON logs by
    default, a Synapse-like plain-text format (`LogFormat::SynapseText`), `RUST_LOG`-overridable
    level filter. OTLP trace export behind the `otlp` feature (verified: `cargo check -p
    hs-telemetry --features otlp` and `--all-features` both build clean). Sentry reporting behind
    the `sentry` feature (same verification). A `Guard` type holds both optional exporters open
    and flushes them on drop.
  - `request_id` module: `RequestIdLayer`, a generic `tower::Layer` (not axum-specific) that
    reuses a caller-supplied `X-Request-Id` or generates one, attaches it to a `tracing` span, and
    stamps it onto the response.
  - `metrics` module: `Metrics`, a shared `prometheus_client::Registry` wrapper with
    `hs_http_requests_total`/`hs_http_request_duration_seconds` pre-registered, `with_registry`
    for other subsystems to register their own families into the same registry, and
    `encode_to_string` for a `/metrics` handler. The metric- and span-naming conventions are
    normative rustdoc on this module.
  - `docs/decisions/0004-telemetry-conventions.md`: the naming conventions mirrored out of that
    rustdoc, for tracks 03 and 15 per the assignment.
  - Verified: `cargo test -p hs-telemetry --all-features` (7 tests), `cargo clippy -p
    hs-telemetry --all-targets --all-features -- -D warnings` clean, default/`otlp`/`sentry`/
    `--all-features` builds all clean.

- **`crates/hs-cli`** (full crate; new ownership per the integration lead's assignment message,
  not in the original track-12 brief):
  - `hs serve [-c CONFIG] [--capabilities-config <path>] [--routes-manifest <path>]`: loads native
    config (`hs_config::Config::load`), initializes telemetry, opens the configured storage
    backend, serves `GET /_matrix/client/versions` and `GET /_matrix/client/v3(+r0)/capabilities`,
    mounts `hs-auth`'s router under both `/_matrix/client/v3` and `/_matrix/client/r0`, serves
    `/health/live`, `/health/ready`, `/metrics` (now with real data — see "Integration review
    follow-up"), binds every configured listener, handles SIGTERM (and Ctrl+C) with graceful
    shutdown (`axum::serve(...).with_graceful_shutdown`). `hs routes-manifest [-o <path>]` writes
    the same `routes.json` `hs serve --routes-manifest` would, without booting a server.
  - `hs serve --synapse-config <path> [--allow-unsupported-synapse-config]
    [--translation-report markdown|json] [--translation-report-out <path>]`: implements
    `docs/compat/cli-shims.md`'s spec exactly, including re-applying `HS__` environment overrides
    on top of the translated config (see "Decisions made" below for the secret-file-sibling
    wrinkle this needed).
  - `hs generate-config --server-name <name> [-o <path>]`, `hs hash-password [-p <password>] [-c
    <config>]`, `hs generate-signing-key [-o <path>]`, `hs register ... SERVER_URL` (the
    shared-secret HTTP client; see "Interfaces needed" — the server-side route it talks to does
    not exist yet), `hs version`.
  - **End-to-end test** (`crates/hs-cli/tests/e2e.rs`, the deliverable called out as mattering
    most): boots a real `hs serve` server in-process on an OS-assigned port, registers a user
    through `POST /_matrix/client/v3/register`, logs in through `POST /_matrix/client/r0/login`,
    calls `/_matrix/client/v3/account/whoami` with the minted token, hits `/health/live`,
    `/health/ready` and `/metrics`, and checks a wrong-password login is rejected — all over real
    HTTP against the bound socket. 4 tests, all passing.
  - Manually smoke-tested the actual compiled binary (`cargo build -p hs-cli`): `hs version`, `hs
    generate-config`, `hs generate-signing-key`, `hs hash-password`, `hs serve -c config.yaml`
    (bound a real port, served `/health/live`/`/health/ready`/`/metrics` over `curl`, shut down
    cleanly on `SIGTERM`), and `hs serve --synapse-config homeserver.yaml` (translation report
    printed, server booted on the translated listener, served traffic, shut down on `SIGTERM`).
  - 28 unit tests + 4 e2e tests, all passing. `cargo clippy -p hs-cli --all-targets -- -D
    warnings` clean.

- **`.github/workflows/ci.yml`**: split into `fmt`, `clippy` (amd64+arm64 matrix), `test`
  (amd64+arm64 matrix, `--all-targets` and `--doc`), `audit` (`cargo-audit` via
  `rustsec/audit-check`, no extra config file needed), and a `ci-ok` gate job. `Swatinem/rust-cache`
  added to every job that builds. Concurrency group cancels superseded runs on the same ref.

- **`.github/workflows/nightly-bench.yml`**: skeleton, scheduled daily plus `workflow_dispatch`,
  runs `cargo bench --workspace` on both an amd64 and an arm64 GitHub-hosted runner and uploads
  the criterion output as an artifact. Explicitly documented as *not* the dedicated small-ARM-host
  rig `PLAN.md` section 7.4 budgets against, and *not* wired to pass/fail budgets yet — see "Next".

- **`deploy/Dockerfile`**: multi-stage (musl builder + `gcr.io/distroless/static-debian12:nonroot`
  runtime), non-root (uid/gid 65532), no shell/package manager in the final image (nothing to
  exploit even with a writable filesystem), multi-arch via `docker buildx build --platform
  linux/amd64,linux/arm64` (deliberately not cross-compiling from a single host — each platform's
  build runs natively/emulated as *that* architecture, so no cross-linker package juggling).
  **UNTESTED**: Docker is not available in this environment; reviewed line by line instead. See
  the file's own header comment for exactly what was and wasn't checked.

- **`deploy/helm/hs/`**: a standalone Helm chart (`helm lint` clean; `helm template` verified for
  both `mode: singleNode` and a `mode: cluster` configuration exercising every optional feature —
  PostgreSQL/CloudNativePG wiring, PodDisruptionBudget, HPA, Ingress, Gateway API `HTTPRoute`,
  ServiceMonitor, NetworkPolicy — all render to valid YAML). Values are modeled on
  `refs/ess-helm/charts/matrix-stack/values.yaml`'s own conventions (`image.{registry,repository,
  tag,digest,pullPolicy,pullSecrets}`, `postgres.{host,port,user,database,sslMode,password:
  {value|secret+secretKey}}`, `media.storage`-shaped PVC options, `ingress.{className,tlsEnabled,
  tlsSecret,annotations}`, `containersSecurityContext`, `storage.resourcePolicy`) so an ESS
  Community operator recognizes the shape immediately — see the chart's own header comment for
  exactly which fields line up and the follow-up needed to actually fold this into the
  `matrix-stack` umbrella chart as a `synapse:`-block replacement. `StatefulSet` used for both
  modes (stable network identity either way; `volumeClaimTemplates` only in embedded-storage
  mode). `hs.validate` template helper fails the render with a clear message rather than producing
  a workload that would just `CrashLoopBackOff` on `hs serve`'s own config validation (missing
  `serverName`, missing signing-key secret, `postgres` backend with no connection info).

- **`crates/hs-operator`**: CRD schemas for `Homeserver`, `AppService`, `Bridge`, `PushGateway`
  and `IdentityService` (all `hs.matrix.org/v1alpha1`), generated to `deploy/crds/*.yaml` via
  `cargo run -p hs-operator --bin gen-crds`, plus stub reconcile loops
  (`crates/hs-operator/src/reconcile/mod.rs` — see that module's doc comment for exactly what
  "stub" means: they compute a next status from the observed spec with no Kubernetes API calls,
  which is what the unit tests exercise; wiring them into a real `kube::runtime::Controller`
  watch loop against a live API server is Phase 1/2 work). 20 tests: schema round-trips through
  YAML for every kind, `kube`'s own structural-schema conversion succeeding for every kind
  (caught and fixed a real bug this way — see "Decisions made"), and reconcile-stub logic. `helm`-
  adjacent validation: `kubectl apply --dry-run=client --validate=false -f deploy/crds/<kind>.yaml`
  succeeds for all five (client-side only; no server round trip attempted — see "Decisions made"
  on why).

- **`deploy/observability/`**: `grafana/hs-overview.json` (valid JSON; request rate, error rate,
  p50/p95/p99 latency panels against the metrics `hs-telemetry` actually registers today, plus
  labeled TODO rows for the subsystems — room, federation, storage, cluster — that don't have
  metrics yet) and `alerts/hs-rules.yaml` (a `PrometheusRule` CRD manifest: `HsDown`,
  `HsReplicasNotReady`, `HsHighErrorRate`, `HsHighRequestLatencyP99`,
  `HsContainerRestartingFrequently`, `HsMemoryNearLimit`, plus a commented-out storage-conflict
  rule stub for once `hs-kv` metrics exist). Not evaluated against a live Prometheus/Grafana
  instance — reviewed by eye against actual metric names and valid YAML/JSON only.

## In progress / Next

- Wire `hs-operator`'s stub reconcile functions into a real `kube::runtime::Controller` that
  actually creates/patches owned resources (`StatefulSet`, `ConfigMap`, `Service`), against a
  `kind` cluster once one is available in this environment.
- Fold `deploy/helm/hs` into `element-hq/ess-helm`'s `matrix-stack` umbrella chart as an
  alternative to its `synapse:` block (today it is a standalone chart with an aligned-but-separate
  values schema).
- Actually build and push `deploy/Dockerfile` once Docker is available; add cosign signing and
  SBOM generation (`PLAN.md` section 7's requirement — not started).
- Turn `.github/workflows/nightly-bench.yml` from "runs and uploads raw output" into a real
  pass/fail budget check against `PLAN.md` section 7.4's numbers, and get a dedicated (non-shared,
  non-GitHub-hosted) arm64 rig instead of `ubuntu-24.04-arm`.
- Per-listener resource filtering in `hs serve` (`listeners[].resources` is parsed and stored but
  every listener currently serves the full router regardless of its declared resource list — see
  `crates/hs-cli/src/serve.rs`'s `build_router` doc comment).
- `docs/config.md` generation, Debian/RPM packages, a Nix flake, cert-manager mTLS for the mesh,
  the arm64 benchmark rig producing real numbers: none started (Phase 0/1 items from the original
  brief, deprioritized this session in favor of the newly-assigned `hs-cli` work, which the
  integration lead's message marked as the priority — "cannot package or probe a server that has
  no entry point").

## Blockers

- None outright, but several "Next" items need a `kind` cluster or Docker, neither available here.

## Interfaces provided

- `hs-telemetry`: `init::{Options, LogFormat, Level, init, Guard}`, `request_id::{RequestIdLayer,
  REQUEST_ID_HEADER, request_id_from_headers}`, `metrics::{Metrics, HttpLabels}`. Metric/span
  naming conventions: `docs/decisions/0004-telemetry-conventions.md`.
- `hs-cli`: the `hs` binary (`docs/compat/cli-shims.md`'s spec, mostly implemented — see
  "Interfaces needed" for the one route it depends on that doesn't exist yet) and a `hs_cli`
  library other tracks' integration tests could in principle depend on for in-process server
  boot, the same way `crates/hs-cli/tests/e2e.rs` does (`hs_cli::serve::spawn_serve`).
- `hs-operator`: CRD schemas (Rust types in `hs_operator::crds`, generated YAML in `deploy/crds/`).
- `deploy/helm/hs`: the chart values schema (`deploy/helm/hs/values.yaml`).
- CI: `.github/workflows/ci.yml` is what every track's PR now runs against.

## Interfaces needed

- **From track 13 (`hs-config`)**: no `capabilities`/`unstable_features` section exists in the
  native config schema, and it cannot be bolted onto the main file today (`hs_config::Config`
  denies unknown top-level keys). `hs-cli` works around this with its own `--capabilities-config`
  file (`crates/hs-cli/src/versions.rs`) as a stopgap. A real `hs_config::CapabilitiesConfig`
  section would let this move into the main config file and drop the separate flag.
- **From track 07 (`hs-auth`) or whoever ends up owning that rewiring**: `hs-auth::config::AuthConfig`
  is not the same type as `hs_config::AuthConfig` — it's `hs-auth`'s own pre-`hs-config` stand-in
  (per that crate's own module doc, built before `hs-config` existed, per
  `docs/workstreams/README.md` rule 1). `hs-cli` bridges the two field-by-field in
  `crates/hs-cli/src/config_bridge.rs::auth_config_from` (documented there in detail: which fields
  map, which don't, why). This works today but is a maintenance liability — every new
  `hs_config::AuthConfig` field silently has no effect on `hs-auth`'s actual behavior until
  someone updates the bridge by hand. Rewiring `hs-auth` to consume `hs_config::AuthConfig`
  directly (or exposing a `From`/`TryFrom` on `hs-auth`'s side) would remove this crate's bridge
  entirely.
- **From track 01 (`hs-kv`) and/or track 07**: `hs-auth::store::AuthStore` has exactly one
  implementation, `InMemoryAuthStore`. `hs serve` opens the configured `hs-kv` backend (proving
  the config plumbing works, see `crates/hs-cli/src/storage.rs`) but nothing downstream actually
  persists through it — every request in this milestone is served from the in-memory auth store
  regardless of `storage.backend`. An `hs-kv`-backed (or `hs-tables`-backed, once that lands)
  `AuthStore` implementation is what would close this gap; `hs-cli` cannot provide it without
  editing `hs-auth`, which is out of scope for this track.
- **From track 01 (`hs-kv`)**: only `MemoryBackend` and `FjallBackend` exist. `hs_config::StorageConfig`
  has three variants (`Embedded`/`Postgres`/`Slatedb`); `hs serve` can only actually open
  `Embedded` today — `Postgres`/`Slatedb` fail cleanly with
  `StorageOpenError::BackendNotImplemented` rather than silently falling back to something else
  (`crates/hs-cli/src/storage.rs`). This also means the chart's/CRD's PostgreSQL and SlateDB
  storage options render correct config and env wiring but cannot actually be exercised
  end-to-end yet.
- **From track 13/07 jointly (per `docs/compat/cli-shims.md`'s own framing) or whichever track
  ends up owning `hs-compat`'s HTTP layer**: `POST`/`GET /_synapse/admin/v1/register` (the
  shared-secret registration route `hs register` talks to) is not mounted anywhere. `hs-compat`
  ships the `shared_secret` library (nonce issuance, MAC compute/verify) but no axum handler and
  no router fragment exposing it. `hs register` is built and unit-tested against the documented
  protocol (`crates/hs-cli/src/register.rs`) and will work unmodified once that route exists; today
  it 404s against a live `hs serve`. The end-to-end test uses `hs-auth`'s own `POST /register`
  (`m.login.dummy` UIA, which *is* wired up) instead, to still prove the rest of the server boots
  and serves correctly.
- **From whoever owns `hs-admin`/the native admin API (track 15?)**: none of the CRD kinds'
  reconcile stubs call any admin API yet (there isn't one wired to a router either, as far as this
  track could tell) — `AppService` reconciliation in particular will need one to actually write
  registrations rather than just validate the spec.

## Decisions made

- **`hs-cli` scope**: implemented every subcommand `docs/compat/cli-shims.md` specifies
  (`serve`, `serve --synapse-config`, `generate-config`, `hash-password`,
  `generate-signing-key`, `register`, `version`). Password prompts use `rpassword` (added to
  `[workspace.dependencies]`) rather than a bare CLI argument, matching the spec's explicit
  "never as a bare CLI argument" requirement.
- **`hs serve`'s listener model**: one combined `axum::Router` serves every configured listener
  regardless of its declared `resources` list (no per-listener splitting into
  client/federation/media/metrics sockets yet). Simpler for a first working version; tracked as
  "Next".
- **`hs serve --synapse-config`'s env-override reapplication**: `hs_compat::translate::translate`
  both sets a native `..._file` field *and* (via its own internal `resolve_secrets()` call)
  resolves it into the inline field for any Synapse `*_path`-style secret option, leaving both
  set simultaneously. Re-running `Config::from_value` on that tree (needed to apply `HS__` env
  overrides on top, per the shim spec) would hit `ConfigError::SecretConflict`. Fixed generically
  (structurally, not via a hardcoded field list that would drift as other tracks add config
  fields): `crates/hs-cli/src/synapse_serve.rs::strip_resolved_secret_file_siblings` walks the
  parsed YAML tree and drops any `X_file` key whose stem `X` is already present, before the
  env-override round trip.
- **`hs-operator` CRD shape for `Homeserver.spec.storage`**: not a Rust enum with
  `#[serde(tag = "backend")]` (the natural shape, and what `hs_config::storage::StorageConfig`
  itself uses) — `kube`'s CRD schema conversion rejects it, because the Kubernetes structural-
  schema OpenAPI v3 dialect cannot express "property `backend`'s schema differs per `oneOf`
  branch" for a property shared across branches (`kube-core`'s schema merge panics: "Property
  \"backend\" ... must be identical"). Caught by the schema round-trip test, not by inspection.
  Fixed with the standard kube-rs workaround: a flat struct (`backend` discriminator field plus
  one `Option<...>` block per backend) — see `crates/hs-operator/src/crds/homeserver.rs`'s doc
  comment on `StorageSpec` for the full explanation.
- **`hs-operator`'s `schemars` version**: pinned to `0.8` (not the workspace's `schemars = "1"`)
  in `crates/hs-operator/Cargo.toml` directly, not via `{ workspace = true }`. `kube-core` 1.1.0's
  `#[derive(CustomResource)]` macro is built against `schemars = "0.8.6"` internally, and its
  generated code resolves `schemars::JsonSchema` against *its own* dependency edge — a struct
  implementing `JsonSchema` from schemars 1.x does not satisfy that, since Rust does not unify two
  semver-incompatible versions of the same crate as one type. `k8s-openapi` was pinned to `0.25`
  (not `0.26`) in `[workspace.dependencies]` for the matching reason: `kube-core` 1.1.0 depends on
  `k8s-openapi = "0.25.0"` with no floating upper bound, and Cargo cannot merge features across
  two different semver-major-equivalent (0.x) versions resolved simultaneously.
- **Live cluster caution**: this environment's `kubectl` context (`admin@dacrib0`) turned out to
  be reachable and pointed at a real, long-lived cluster (some nodes at 414 days uptime) — not a
  disposable `kind` cluster. Deliberately did not apply, create, or delete anything there; only
  used `kubectl apply --dry-run=client --validate=false` (no server round trip) as an extra sanity
  check on the generated CRD YAML's outer-object shape. Flagging this explicitly since "no cluster
  access" was this track's working assumption going in and turned out to be wrong — a real cluster
  is one `kubectl apply` away, so anyone picking up the "Next" items above should decide
  deliberately whether that cluster is an appropriate place to validate against, not assume it
  isn't there.
- **CI's advisory job**: `cargo-audit` via `rustsec/audit-check`, not `cargo-deny`. `cargo-deny`
  needs a `deny.toml`, which per this track's file-ownership boundaries
  (`.claude/agents/hs-12-platform.md`) belongs at the repo root — not a location this track's
  instructions list as editable. `cargo-audit` needs no extra config file, so it sidesteps the
  question entirely. A `cargo-deny` pass (license/bans/sources policy) is a reasonable follow-up
  once there is a repo-root file this track (or the integration lead) is comfortable placing a
  `deny.toml` at.

## Shared dependencies added (`[workspace.dependencies]` in the root `Cargo.toml`)

- `clap = { version = "4", features = ["derive", "env"] }` — `hs-cli`'s argument parsing.
- `rpassword = "7"` — `hs-cli`'s non-echoed password prompts.
- `kube = { version = "1", default-features = false, features = ["derive", "client", "runtime", "rustls-tls"] }`
  — `hs-operator`.
- `k8s-openapi = { version = "0.25", features = ["latest", "schemars"] }` — `hs-operator`; pinned
  to `0.25` (not `0.26`) for the reason under "Decisions made".
- `hex = { workspace = true }` (already present; added as a direct dependency of `hs-telemetry`
  for request-id generation — no new workspace entry needed).
- `crates/hs-operator/Cargo.toml` additionally pins `schemars = "0.8"` **directly** (not via
  `{ workspace = true }`, which stays at `"1"` for every other crate) — see "Decisions made" for
  why this one crate cannot follow the workspace's shared version.

## How to verify everything in this file

```sh
# hs-telemetry
cargo test -p hs-telemetry --all-features
cargo clippy -p hs-telemetry --all-targets --all-features -- -D warnings

# hs-cli (unit + end-to-end)
cargo test -p hs-cli
cargo clippy -p hs-cli --all-targets -- -D warnings
cargo build -p hs-cli && ./target/debug/hs version
./target/debug/hs routes-manifest | head -20  # no config, no server needed

# hs-cli manual boot + curl (what the integration review actually ran)
cat > /tmp/hs-verify-config.yaml <<'YAML'
server:
  server_name: verify.example
listeners:
  listeners:
    - port: 18124
      bind_addresses: ["127.0.0.1"]
      resources: [client, health, metrics]
storage:
  backend: embedded
  data_dir: /tmp/hs-verify-data
auth:
  enable_registration: true
YAML
./target/debug/hs serve -c /tmp/hs-verify-config.yaml --routes-manifest /tmp/routes.json &
sleep 1
curl -s http://127.0.0.1:18124/_matrix/client/versions
curl -s http://127.0.0.1:18124/_matrix/client/v3/capabilities
curl -s -X POST http://127.0.0.1:18124/_matrix/client/v3/register \
  -H 'content-type: application/json' \
  -d '{"username":"x","password":"correct horse battery staple","auth":{"type":"m.login.dummy"}}'
curl -s http://127.0.0.1:18124/metrics | grep hs_http_requests_total
kill -TERM %1

# hs-operator (CRD schema + reconcile-stub tests, and regenerating deploy/crds/)
cargo test -p hs-operator
cargo clippy -p hs-operator --all-targets -- -D warnings
cargo run -p hs-operator --bin gen-crds

# CI workflow YAML syntax
python3 -c "import yaml; yaml.safe_load(open('.github/workflows/ci.yml')); yaml.safe_load(open('.github/workflows/nightly-bench.yml'))"

# Helm chart
helm lint deploy/helm/hs --set serverName=example.org --set secrets.signingKey.existingSecret=x
helm template t deploy/helm/hs --set serverName=example.org --set secrets.signingKey.existingSecret=x

# Observability skeletons
python3 -c "import json; json.load(open('deploy/observability/grafana/hs-overview.json'))"
python3 -c "import yaml; yaml.safe_load(open('deploy/observability/alerts/hs-rules.yaml'))"
```
