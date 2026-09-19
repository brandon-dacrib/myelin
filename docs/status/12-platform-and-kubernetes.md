# 12. Platform and Kubernetes

Last updated 2026-09-18, by the "mount what already exists" assignment: `hs serve` mounted only
`hs-auth`'s routes even though `hs-room`, `hs-media`, `hs-appservice` and `hs-admin` were fully
built, tested and committed in their own crates — the structural finding that spec-coverage
reported 17/235 routes not because the work wasn't done, but because nothing served it. This
session mounts all four. See "Mounting hs-room/hs-media/hs-appservice/hs-admin" immediately below;
everything from "Integration review follow-up" down is the prior session's record, unchanged.

## `.well-known` discovery documents (2026-09-18, integration lead)

Closes the known gap "`GET /.well-known/matrix/server` not served — a deployment that delegates
its server name cannot be found" from `docs/next-steps.md`.

- **New**: `crates/hs-cli/src/well_known.rs` serves `GET /.well-known/matrix/server`
  (`{"m.server": "<host[:port]>"}`, from the new `server.well_known_server` config field) and
  `GET /.well-known/matrix/client` (`{"m.homeserver": {"base_url": ...}}`, from the existing
  `server.public_baseurl`). Both carry `Access-Control-Allow-Origin: *`; the client one needs it
  by spec (a web client on another origin is exactly who fetches it).
- **Absent, not self-referential**: each route answers `404 M_NOT_FOUND` when its config field is
  unset, rather than serving a document naming this server. A well-known that points at the name
  it was fetched from is indistinguishable from no document in the spec's resolution order, so
  serving one only adds a way to fail. Synapse defaults `serve_server_wellknown` to false for the
  same reason.
- **Config**: `hs_config::ServerConfig::well_known_server: Option<String>`, validated as a
  `host[:port]` (a URL or whitespace is rejected at config-load time, not at request time).
- **Synapse translation table** (`crates/hs-compat/src/classification.rs`): `serve_server_wellknown`
  moves from `Unsupported` to `MappedDiff` against the new field — Synapse takes a boolean and
  derives the destination itself, this takes the destination directly, so the translation cannot
  be automatic and the note says so. `extra_well_known_client_content` stays unsupported: the
  client document carries only `m.homeserver`.
- **Verified**: `cargo test -p hs-config` (71 pass), `cargo test -p hs-compat` (42 pass), and the
  module's own five tests drive the real handlers through an axum router.

The fetching side of this (`crates/hs-federation/src/discovery.rs`) already existed and is what
makes these documents load-bearing: this server resolves a remote's delegation exactly the way a
remote now resolves ours.

## Mounting hs-room, hs-media, hs-appservice and hs-admin; media-scanning startup wiring

**Before**: `crates/hs-cli/src/serve.rs` mounted `GET /_matrix/client/versions`,
`GET /_matrix/client/v3(+r0)/capabilities`, `hs-auth`'s router, and health/metrics — 38 routes in
`docs/status/routes.json`, 17/235 against the Matrix spec (`hs-spec-coverage`). `hs-media` was not
even a dependency of `hs-cli`.

**After**: `docs/status/routes.json` regenerated (`hs routes-manifest -o docs/status/routes.json`)
has 255 entries; `cargo run -p hs-spec-coverage --bin hs-spec-coverage -- --spec-dir
refs/matrix-spec/data/api --routes docs/status/routes.json` reports **52/235 (22.1%)**, all in the
`client-server` family (166 spec routes there, 52 registered, 31.3%). `server-server`,
`application-service`, `identity` and `push-gateway` are still 0% — no track has built a router for
any of those surfaces yet; there was nothing further to mount for them. `docs/status/dashboard.md`
regenerated via `python3 tools/dashboard.py` reflects the new number.

### What was mounted, and how (`crates/hs-cli/src/serve.rs::build_router`, now generic over
`B: KvBackend`)

1. **`hs-room`** (`hs_room::routes::router::<B>()`): room creation, send/state, context, messages,
   membership (join/leave/invite/kick/ban/unban/knock), redaction, aliases, relations — mounted
   under both `/_matrix/client/v3` and `/_matrix/client/r0`, the same double-mount `hs-auth`
   already used. State composition followed `hs-media`'s own documented pattern exactly
   (`crates/hs-room/src/state.rs`'s module doc credits it): `RoomState<B>` embeds `AuthState` via
   `FromRef`, `RoomRequester` bridges `hs-auth`'s `Requester` extractor onto it. Needed a real
   room-actor identity (signing key) that did not exist anywhere in `hs-cli` before — see
   `crates/hs-cli/src/identity.rs` below.
2. **`hs-media`** (`hs_media::router::authenticated_router::<B>()` under
   `/_matrix/client/v1/media`, plus `legacy_router::<B>()` under `/_matrix/media/v3` when
   `media.allow_legacy_unauthenticated_media` is set, the default): upload (sync + async
   create/put), download, thumbnails, `/config`, and the legacy unauthenticated download/thumbnail
   pair. New `crates/hs-cli/src/media.rs` builds `MediaState<B>`: object store via
   `hs_media::store::build(&config.media.storage)` (already written against
   `hs_config::MediaStorageBackend` — nothing to add), metadata via
   `MetadataStore::open(backend)`, an unlimited `InMemoryQuotaPolicy` (`hs_config::MediaConfig` has
   no quota fields — see "Interfaces needed"), and `ThumbnailPolicy` from
   `config.media.thumbnail_sizes`.
3. **`hs-appservice`** (`hs_appservice::routes::ping_router::<B>`, mounted under
   `/_matrix/client/v1`, state = `AuthState` per that module's own doc on why): new
   `crates/hs-cli/src/appservices.rs` opens `hs_appservice::registry::Registry` over the shared
   backend and loads every file in `config.appservices.registration_files` into it
   (`Registration::parse_yaml` + `registry.add`), then builds a `PingService`. Best find of the
   session: `hs-appservice` already ships
   `hs_appservice::auth_registry::RegistryAppserviceAdapter`, an `hs_auth::appservice::
   AppserviceRegistry` implementation over that same `Registry` — "replacing the stub
   `InMemoryAppserviceRegistry`" per its own doc comment. `hs-cli` only had to call it
   (`auth_state.appservices = Arc::new(RegistryAppserviceAdapter::new(registry.clone()))`); no
   bridging code needed writing. This means a loaded registration's `as_token` now actually
   authenticates through `Requester`, not just the ping route — every appservice-authenticated
   endpoint across the whole server benefits.
4. **`hs-admin`** (`hs_admin::router::build_router`, merged directly onto the top-level router
   rather than through `Builder::merge_router` since it already builds through its own `Builder`
   internally and its paths — `/api/v1/...`, `/admin/...` — are absolute, not spec-relative):
   `/api/v1`'s full operation table (142 routes, every one answering `501` by design except the
   two `openapi.yaml`/`.json` endpoints — **not reported as working**, per this assignment's
   caution) plus `/admin/`'s embedded management-interface assets. `TokenVerifier` is
   `hs_admin::auth::StaticVerifier::new()` with **no tokens registered** — every `/api/v1` request
   is `401`. This is deliberate, not a placeholder pretending to work: no real
   `hs_admin::auth::TokenVerifier` implementation exists anywhere in the workspace yet (track 07
   owns it, per `docs/status/15-admin-api-and-modules.md`'s own "Interfaces needed"), and
   `hs-cli` is not going to invent an ad hoc admin-authorization scheme for a surface whose real
   answer belongs to another track. Mounting it still establishes the real HTTP surface and lets
   `hs-admin`'s own contract test (`Builder`/OpenAPI agreement) mean something end to end.

### New files

- `crates/hs-cli/src/identity.rs`: `load_or_generate(&Config) -> Result<HomeserverIdentity,
  IdParseError>`. Reads the first `ed25519 <key_id> <base64-seed>` line found under
  `server.signing_key_path` (the same directory and text format `hs generate-signing-key` already
  wrote, `crates/hs-cli/src/signing_key.rs`) and hands it to
  `hs_model::signing::SigningKeyPair::new`. **Falls back to a freshly generated, unpersisted key**
  (logged at `warn`) if none is found — honest, not silent, but means a restart with no signing
  key file on disk signs every subsequent event under a different key than before. Run `hs
  generate-signing-key -o <signing_key_path>/hs.signing.key` before relying on room state
  surviving a restart; tracked below under "Known gaps".
- `crates/hs-cli/src/media.rs`: `build_media_state`, plus (assignment item 2) the content-scanning
  startup wiring `docs/status/09-media.md` flagged as the one missing piece:
  "`ScanAdmin::rescan`... a stable id on `AuditEntry`... **the homeserver startup wiring
  itself**... none of that glue exists yet; only the pieces it would call do." A new `hs serve
  --media-scanning-config <path>` flag (mirrors the existing `--capabilities-config` precedent
  exactly, for exactly the same reason — see "Decisions made") takes a `media.scanning` YAML file
  in `hs_media::scanning::ScanningConfig::from_yaml`'s own documented shape (the same shape
  `deploy/media-scanning/media-scanning.yaml` already contains), calls `ScanningConfig::validated`,
  builds a `ScanEngine` (`ScanMetrics::register`'d into the same `hs-telemetry` registry
  `/metrics` serves, `TracingAuditSink` for the audit trail until `ScanAdmin` has a real
  implementation to hand a richer sink to), and attaches it via `MediaRepository::with_scanning`.
  Omitting the flag is unchanged behavior (`mode: off`). Track 09's `ScanningConfig` type itself
  was not touched — it still deliberately does not live in `hs_config::MediaConfig` (that track's
  own ownership rule; see "Interfaces needed" below for what track 13 would need to do to fold it
  in for real).
- `crates/hs-cli/src/appservices.rs`: registration-file loading described above.
- `crates/hs-cli/src/appservice_manifest.rs`: hand-mirrored `routes.json` entry for
  `hs_appservice::routes::ping_router` (a bare `axum::Router<AuthState>`, not built through
  `Builder`, exactly the same situation `crate::auth_manifest` already documented and solved for
  `hs-auth`'s router — same fix, same file shape).

### Extended end-to-end test (assignment item 4)

`crates/hs-cli/tests/e2e.rs`'s main test now continues past login/whoami into: `POST
/_matrix/client/v3/createRoom` -> `PUT .../send/m.room.message/{txnId}` -> `GET
.../context/{eventId}` (asserts the same event and body round-trip) -> `POST
/_matrix/client/v1/media/upload` (raw bytes, `?filename=`) -> `GET
/_matrix/client/v1/media/download/{serverName}/{mediaId}` (asserts the downloaded bytes are
byte-for-byte the uploaded ones) — all over the same real bound socket the existing
register/login/whoami steps already used. 5 e2e tests, all passing; this is what proves the
composition is real, not merely compiling (`cargo test -p hs-cli --test e2e`).

### Verification

```
cargo check -p hs-cli                                        # clean
cargo clippy -p hs-cli --all-targets -- -D warnings           # clean
cargo test -p hs-cli --lib                                    # 59 passed
cargo test -p hs-cli --test e2e                                # 5 passed
cargo run -p hs-cli --bin hs -- routes-manifest -o docs/status/routes.json
cargo run -p hs-spec-coverage --bin hs-spec-coverage -- \
  --spec-dir refs/matrix-spec/data/api --routes docs/status/routes.json
# -> 52 / 235 spec routes registered (22.1%)
python3 tools/dashboard.py
```

### Decisions made (this session)

- **`--media-scanning-config`, not a `hs_config::MediaConfig.scanning` field.** Track 09's
  `ScanningConfig` "deliberately does **not** live in `hs_config::MediaConfig`" (that crate's own
  module doc) since track 09 does not own `hs-config`. This track doesn't either. Rather than
  reshape another track's config type (this assignment's explicit instruction: "if that is more
  than mechanical, write an RFC rather than reshaping another track's config type"), this follows
  the precedent `crate::versions`'s `--capabilities-config` already set for the identically-shaped
  problem (`unstable_features` also can't live in the main config file) — a second, optional side
  file. No RFC needed: this is mechanical (`ScanningConfig` already has its own `from_yaml`/
  `validated`), not a redesign.
- **Appservice registration files are always loaded, `appservices.enabled` is not consulted.**
  `hs_config::AppservicesConfig::enabled`'s doc says "master switch for appservice transaction
  delivery" — that's `hs-appservice`'s scheduler's business (dead-letter/backlog delivery), not
  something `hs-cli` touches or gates. `registration_files` is loaded and the auth registry wired
  either way. If a future track wants `enabled: false` to also skip loading registrations, that's
  a one-line change in `crate::serve::spawn_serve`, not a design question.
- **`hs-admin`'s `TokenVerifier` is an empty `StaticVerifier`, not a home-grown bridge to
  `hs-auth`.** Considered writing an adapter that treats any valid `hs-auth` access token as a
  full-scope admin principal, to make `/api/v1` minimally usable. Rejected: that would be inventing
  security policy (who is an admin?) that belongs to track 07/15, not something to guess at from
  `hs-cli`. An empty verifier is the honest "not wired up yet" answer — every request is `401`,
  which is correct for a server with no admin tokens configured, and does not pretend a
  not-yet-designed authorization model exists.
- **`HomeserverIdentity`'s signing key**: read from `server.signing_key_path` in the same
  Synapse-shaped text format `hs generate-signing-key` writes, falling back to an ephemeral
  generated key (not persisted) rather than failing `hs serve` outright. A homeserver that has
  never had a signing key generated for it should still boot and serve (registration, login, room
  creation all still work); it just re-signs everything under a new key on every restart until an
  operator runs `hs generate-signing-key`. Failing to boot instead would make "try the server"
  harder than it needs to be for zero benefit (nothing before this session validated
  `signing_key_path` either — `hs generate-signing-key` only ever *wrote* to it, nothing *read*
  from it).

### Reuse considered

- **`hs_appservice::auth_registry::RegistryAppserviceAdapter`** — found already built (see above);
  used as-is, wrote zero bridging code for the auth-registry seam. This was the single biggest
  time saver in the session; without it, mounting the ping route would have needed hand-converting
  `hs_appservice::namespace::NamespaceRule` (`regex`/`fancy_regex`-backed `NamespacePattern`) into
  `hs_auth::appservice::NamespaceRule` (bare `regex::Regex`) field by field — exactly the kind of
  work the adapter's own module doc says it already did, including the documented lossy edge case
  (a `fancy_regex`-only pattern has no `regex::Regex` projection).
- **`hs_media::store::build`** — already converts `hs_config::MediaStorageBackend` into an
  `Arc<dyn ObjectStore>` (local filesystem today, S3 behind a feature flag). Used directly; no
  reason to write a second conversion.
- **`RoomState`/`RoomRequester`, `MediaState`/`MediaRequester`** — both already exist in their own
  crates, both already documented as following the same pattern for the same reason (composing
  `hs-auth`'s concrete-`AuthState` `Requester` extractor onto a different router state). Nothing
  to build here beyond constructing the state values themselves.
- **Did not write a new admin `TokenVerifier`** (see "Decisions made" above) — the honest reuse
  decision was reusing `StaticVerifier` empty rather than writing a new implementation of a trait
  whose real semantics are still undecided elsewhere.
- **Did not touch `hs_config::MediaConfig` or `hs-media`'s `ScanningConfig`** — both considered and
  rejected per this assignment's explicit instruction to write an RFC instead of reshaping another
  track's config type if the fix is more than mechanical; the side-file precedent already existed
  and made this mechanical, so no RFC was needed either.

### Known gaps carried forward (said precisely, not left implicit)

- **Room-actor signing key is not durable by default** (see "Decisions made" — `identity.rs`).
- **`hs-admin`'s `/api/v1` is unauthenticatable in practice** until track 07 ships a real
  `TokenVerifier`; every request is `401`. The 142 operation routes behind it all still answer
  `501` regardless (RFC 0004 section 3.5's own Phase 0 scope, unrelated to this session).
- **Media upload quota is unconditionally unlimited** (`InMemoryQuotaPolicy::unlimited()`) —
  `hs_config::MediaConfig` has no per-user/per-server quota fields for `hs-cli` to read (only
  `max_upload_size`, which `hs-media`'s own repository already enforces independently of this
  policy trait). A real quota policy needs either a `hs_config::MediaConfig` addition (track 13)
  or an `hs-kv`-backed implementation of `hs_media::policy::UploadPolicy` (this track could write
  one without touching `hs-media`, since the trait is already public and crate-agnostic — not done
  this session, scope was mounting what exists).
- **No per-listener resource filtering, still** (carried over from before this session — every
  configured listener now serves an even larger combined router than before).
- **`server-server`, `application-service` (spec sense — the S2S-facing appservice push
  endpoints, not the client-facing ping route this session mounted), `identity` and
  `push-gateway` spec families remain at 0%** — no track has built a router fragment for any of
  them yet; there was nothing more to mount here without another track's work landing first.

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
  boot, the same way `crates/hs-cli/tests/e2e.rs` does (`hs_cli::serve::spawn_serve`). As of this
  session `hs serve` mounts `hs-auth`, `hs-room`, `hs-media` (authenticated and legacy), `hs-
  appservice`'s ping route, and `hs-admin`'s `/api/v1` + `/admin/` assets — see "Mounting
  hs-room, hs-media, hs-appservice and hs-admin" above. New reusable pieces:
  `hs_cli::identity::load_or_generate` (a `HomeserverIdentity` from native config),
  `hs_cli::appservices::load` (registration-file loading + registry construction), and
  `hs_cli::media::build_media_state` (object store + metadata store + optional content-scanning
  engine from `--media-scanning-config`).
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
  registrations rather than just validate the spec. **Update, this session**: the admin API *is*
  now wired to a router (`hs serve` mounts it), so this is unblocked on that half — reconcile code
  can now target `http://<hs>/api/v1/appservices` — but every operation still answers `501`
  (track 15's own Phase 0 scope), so there is nothing live to call yet either way.
- **From track 07**: a real `hs_admin::auth::TokenVerifier` implementation. `hs-cli` mounts
  `hs-admin`'s router with `hs_admin::auth::StaticVerifier::new()` (empty — every request `401`),
  deliberately not inventing an authorization bridge from `hs-auth` tokens (see "Decisions made,
  this session"). Once track 07 ships one (or `hs-auth` grows an "is this user a server admin"
  query `hs-cli` can wrap), swapping it into `crate::serve::dummy_admin_state` is a one-function
  change.
- **From track 13**: fold `hs_media::scanning::ScanningConfig` into `hs_config::MediaConfig` as a
  `scanning` field (track 09's own long-standing ask, `docs/status/09-media.md`). Until then, `hs
  serve --media-scanning-config <path>` (this session's addition) is the way to enable content
  scanning — a second config file, not the main one. Same ask, for the same reason, as the
  existing `capabilities`/`unstable_features` entry above.
- **From track 13, smaller**: `hs_config::MediaConfig` has no per-user/per-server upload quota
  fields, so `hs serve` always builds `hs_media::policy::InMemoryQuotaPolicy::unlimited()`. Not
  urgent (`max_upload_size` is already enforced independently), but real multi-tenant deployments
  will want it.

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
