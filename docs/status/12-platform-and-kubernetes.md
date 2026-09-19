# 12. Platform and Kubernetes

## From "runs on my machine" to "runs in a cluster" (2026-09-19)

This session's assignment: the server had never been packaged or deployed for real — no
production image had ever been built and run, and the operator had never reconciled against a
real API server. Both are now true, with transcripts. See the four subsections immediately below;
everything from "Mounting hs-room/hs-media/hs-appservice/hs-admin" down is the prior session's
record, unchanged.

### 1. Production image: built, run, and one real bug found and fixed

`deploy/Dockerfile` had never been built (its own header said so). Building it
(`docker build -f deploy/Dockerfile -t hs:track12-verify .`, OrbStack, linux/arm64 native) found
one real bug: `FROM rust:${RUST_VERSION}-slim-bookworm` read a build arg that was never declared
with `ARG RUST_VERSION` above the `FROM` line (global build args must be declared before the
first `FROM` to be visible there). `${RUST_VERSION}` silently resolved to `""` — `rust:-slim-
bookworm`, not a real tag — so the image had *never* successfully built, ever, in any environment.
Fixed with `ARG RUST_VERSION=1.98` (a default, so a plain `docker build` with no `--build-arg`
still works) — see the Dockerfile's own header comment for the full explanation.

With that one-line fix, the image builds clean: multi-stage, static-musl release build of `hs-cli`
(`cargo build --release --locked -p hs-cli --target aarch64-unknown-linux-musl`, ~2m41s compile),
copied into `gcr.io/distroless/static-debian12:nonroot`. Final image: **50.9MB**, runs as
`65532:65532` (verified with `docker top`, not just read off the Dockerfile), no shell.

Ran it end to end:
```
$ docker run -d --name hs-track12-verify --read-only --tmpfs /tmp \
    -v .../homeserver.yaml:/etc/hs/config/homeserver.yaml:ro \
    -v .../signing-key-dir:/etc/hs/secrets/signing-key:ro \
    -v .../data:/var/lib/hs/data -v .../media:/var/lib/hs/media \
    -p 18108:8008 hs:track12-verify serve -c /etc/hs/config/homeserver.yaml

$ curl http://127.0.0.1:18108/health/live       -> 200 "ok"
$ curl http://127.0.0.1:18108/health/ready      -> 200 "ready"
$ curl http://127.0.0.1:18108/_matrix/client/versions
  -> 200 {"versions":["r0.0.1",...,"v1.12"],"unstable_features":{}}
$ curl http://127.0.0.1:18108/metrics | head
  -> hs_http_requests_total{method="GET",route="/health/live",status_class="2xx"} 1  (etc.)
$ docker restart hs-track12-verify; curl .../health/live -> 200 "ok"   # survives a restart
```
`docker top` during the run showed `USER 65532`, confirming the non-root claim is real, not just
asserted. Image and build cache pruned after (`docker rmi`, `docker builder prune -f`,
`docker image prune -f`); disk went from 11GiB free at session start to 15GiB free at session end
(a stray ~2GB of pre-existing dangling build cache got reclaimed along with this session's own).

**Second real bug, found by actually running the image against the chart's own config shape**:
`hs-cli`'s signing-key loader (`crates/hs-cli/src/identity.rs::first_signing_key_in_dir`) treats
`server.signing_key_path` as a *directory* — non-recursive `read_dir`, Synapse-style, one key file
per directory entry — not a file. The chart's `configmap.yaml` set `signing_key_path` to a literal
file path (`/etc/hs/secrets/signing.key`) and `statefulset.yaml` `subPath`-mounted the Secret onto
exactly that file. `read_dir` on a regular file fails, so `hs serve` silently fell back to a fresh
in-memory-only signing key (logged at `warn`, easy to miss) — reproduced live: the first run logged
`no ed25519 signing key found; generating an ephemeral one for this process only`. Every pod
restart under the old chart would have gotten a *different* signing key, silently invalidating
federation signatures and any persisted state that depends on signature continuity across
restarts — exactly the property a `StatefulSet` is supposed to give you. Fixed in the chart (mine
to own): the Secret is now mounted as a directory (no `subPath`) at
`/etc/hs/secrets/signing-key`, and `signing_key_path` points at that directory. Reran the same
container with the fixed layout: no ephemeral-key warning, `/health/live` still 200. See
`deploy/helm/hs/templates/configmap.yaml`'s comment on `signing_key_path` and
`deploy/helm/hs/templates/statefulset.yaml`'s comment on the `secrets-signing-key` volume mount
for the full explanation; `deploy/helm/hs/values.yaml`'s `secrets.signingKey` doc comment updated
to match.

Also fixed, found while reading the chart end to end for this session: `templates/NOTES.txt`'s
"check readiness" `kubectl get pods -l ...` command built its label selector from
`hs.selectorLabels` (which renders YAML `key: value` lines, correct for a labels block) by only
replacing newlines with commas — producing `app.kubernetes.io/name: hs,app.kubernetes.io/instance:
hstest`, which is not valid `-l` selector syntax (needs `=`, no space). Cosmetic (doesn't affect
the actual chart resources), but it's copy-paste output from `helm install` itself, so it's now
fixed to also replace `": "` with `"="`.

`helm lint` and `helm template` still pass after all three fixes (unchanged from the previous
session's baseline claim, now re-verified after edits).

### 2. Chart installed against a real cluster — twice, both cleaned up after

No `kind`/`minikube`/`k3d` binary exists in this environment. `kubectl config current-context`
resolves to `admin@dacrib0`, a real, long-lived, non-disposable Talos cluster (nodes at 42-415
days uptime; `cnpg-system`, `cert-manager`, `argocd`, `longhorn-system`, `metallb-system`, etc.
already installed) — the same one the previous session found and deliberately left untouched. This
session's judgment call: touching it is fine as long as every change is additively scoped to a
throwaway namespace this session creates and fully tears down, and nothing pre-existing is read,
modified, or deleted. Two such round trips happened, both cleaned up completely
(`kubectl get ns`/`get crd` confirmed empty afterward):

**a. `helm install` (real install, not `--dry-run`)**, `singleNode`/`embedded` mode (the mode
`hs-cli` can actually run today — Postgres/SlateDB storage backends still return
`StorageOpenError::BackendNotImplemented`, per the previous session's notes, unchanged), into a
scratch namespace, `image.pullPolicy=Never` (this session's locally-built image lives only on this
Mac; the cluster's Talos nodes are separate machines with no route to a registry this session has
push access to, so a real container is out of reach here — noted honestly rather than glossed
over). Real, informative results from the real API server:
- `PersistentVolumeClaim` bound automatically against the cluster's real default StorageClass
  (`longhorn`) — proves the embedded-storage PVC template is correct, not just syntactically valid.
- Pod scheduled, `SuccessfulAttachVolume` for the bound PVC, `ConfigMap`/`Secret` volumes
  constructed and referenced correctly (mount plan accepted, no error before the image pull step).
- `PodSecurityContext`/`SecurityContext` (non-root, read-only-root-fs, drop-all-caps,
  `seccompProfile: RuntimeDefault`) passed the API server's admission with no `PodSecurity` denial.
- Failed exactly where expected and nowhere else: `ErrImageNeverPull` on
  `ghcr.io/hs:track12-verify` (not present on the node, `pullPolicy: Never`) — the one piece this
  environment cannot supply (a registry the cluster can pull from), not a chart defect.
- `Service`/`Service-headless` created with the right selectors; `PodDisruptionBudget` correctly
  *absent* (skipped by the template, as documented, since `mode: singleNode` forces one replica).

  Cleanup: `helm uninstall`, `kubectl delete pvc/secret/namespace` — `kubectl get ns` confirmed
  the namespace is gone.

**b. `hs-operator`'s CRDs**, `kubectl apply --dry-run=server` for all five kinds (an upgrade from
the previous session's `--dry-run=client --validate=false`, which never contacted the API server's
own structural-schema validation) — all five `created (server dry run)`, no admission error. See
"3. The operator" below for the further step of actually applying `Homeserver`'s CRD for real (not
dry-run) and reconciling against it.

If another track needs a disposable cluster on demand rather than negotiating scoped access to a
real one each time, that is worth raising with whoever can provision one (self-hosted `kind`-in-CI
runner, or a dedicated ephemeral namespace-per-track convention on this same cluster) — out of
scope to set up this session.

### 3. The operator: reconciled against a real API server for the first time

Previously: zero cluster access, by the previous session's own account
(`crates/hs-operator/src/lib.rs`'s old "Status" section said exactly that), so every claim about
the CRDs and reconcile stubs was validated only by unit tests with no `kube::Client` involved
anywhere.

This session, with real (scoped, cleaned-up) cluster access available: applied
`deploy/crds/homeserver.yaml` for real (not dry-run), created a real `Homeserver` object in a
scratch namespace, and ran a new diagnostic binary
(`crates/hs-operator/src/bin/live_smoke.rs`, `cargo run -p hs-operator --bin live-smoke`) that
builds a real `kube::Client` from the ambient kubeconfig and wires the *actual*
`reconcile::reconcile_homeserver` function (the same one `reconcile::tests` calls directly on
hand-built values, completely unchanged) into a real `kube::runtime::Controller`. Output:
```
SMOKE: connected, watching Homeserver objects in namespace "hs-operator-smoke"
SMOKE-OK: reconciled ObjectRef { dyntype: (), name: "sample", namespace: Some("hs-operator-smoke"),
  extra: Extra { resource_version: Some("220237321"), uid: Some("540e9fc5-...") } }
  -> Action { requeue_after: Some(300s) }
```
This proves the watch-stream-to-reconcile-function plumbing is correct against a real API server
(typed deserialization of a real object off the wire, `Arc<Homeserver>` handed to the exact
production reconcile function, `Action` returned and honored) — something no test in this crate
had ever exercised before. It does **not** prove more than that: `reconcile_homeserver` is still
the stub it always was (computes a status, makes no create/patch calls) — see `reconcile`'s module
doc, unchanged, for what "stub" still means and what Phase 1/2 work remains (owned-resource
creation, status subresource patches, finalizers). `crates/hs-operator/src/lib.rs`'s "Status"
section is updated to describe exactly this line: real plumbing proven, real workload creation not
yet built.

`live-smoke` is a manual verification tool, not a production binary — nothing in `deploy/` runs it,
it is not built into `deploy/Dockerfile`'s image, and it needs `rustls`'s default `CryptoProvider`
installed by hand (`rustls::crypto::ring::default_provider().install_default()`) since nothing
else in this crate does that for it, unlike `hs-cli` which gets one transitively. Left in the crate
so the next person (or a future `kind`-based CI job) can rerun the same proof without taking this
session's word for it. Cleanup after the run: deleted the sample `Homeserver`, the scratch
namespace, and the CRD itself (`kubectl get crd | grep matrix` confirmed empty afterward) — the
cluster carries zero trace of this session's work.

`cargo fmt -p hs-operator`, `cargo clippy -p hs-operator --all-targets -- -D warnings`, and
`cargo test -p hs-operator` (20 tests, unchanged pass count from before this session, plus the new
`live-smoke` binary target compiling clean as part of `--all-targets`) all still pass.

### 4. Probes: read the real behavior, didn't touch `hs-cli` (not owned by this track this session)

Traced `/health/live` and `/health/ready` in `crates/hs-cli/src/serve.rs` (read-only — `hs-cli` is
out of scope this session) rather than assuming the brief's framing ("today it answers from a
flag") was still current. It is not, entirely: **track 03 has already wired real cluster-ownership
readiness in**, apparently concurrently with or shortly before this session:

- `/health/live` (`serve.rs:653`): unconditional 200. Correct as a liveness check — it should stay
  cheap and answer "is the process alive", not "is it useful".
- `/health/ready` (`serve.rs:662`): checks, in order, (a) a per-process `Arc<AtomicBool>` `ready`
  flag, and (b) `hs_cluster::Cluster::ready()`, which returns
  `Readiness::Ready`/`Readiness::NotReady(reason)` — explicitly documented in `hs-cluster` itself
  (`crates/hs-cluster/src/ownership.rs:64-71`) as "consumed by track 12's `/health/ready`" and, per
  that doc comment, `Ready` means "joined the mesh (or running single-node), heartbeated, and
  ownership has converged". So **shard-ownership readiness is already real**, not a gap — this
  session's brief was written before (or without seeing) that landed.
- **What's still missing, precisely**: the `ready` `AtomicBool` (b above) is constructed with
  `AtomicBool::new(true)` (`serve.rs:940`, right after storage and cluster startup already
  succeeded — a failure there returns `Err` before this line, so the flag existing at all already
  implies storage opened and cluster startup succeeded) and **is never set to `false` anywhere in
  the file** — grepped for every `ready` reference to confirm. `ServeHandle::shutdown` (`serve.rs`,
  around the `Cluster::drain` call at line ~724) *does* correctly call `cluster.drain(...)` before
  tearing down HTTP listeners on `SIGTERM`, which is the right shape for graceful handoff — but it
  never flips the `ready` flag to `false` first. If `Cluster::drain` itself doesn't make
  `cluster.ready()` report `NotReady` while draining (not verified this session — `hs-cluster`'s
  internals are track 03's, not read in depth here), then `/health/ready` may keep answering 200
  for the length of `CLUSTER_DRAIN_DEADLINE` after `SIGTERM` is received, meaning the Kubernetes
  `Service` could keep routing new requests to a pod that is actively trying to hand off its
  shards and shut down — a real, if narrow, race during rolling updates. The fix, if
  `Cluster::drain` doesn't already cover it: flip `ready.store(false, Ordering::SeqCst)` as the
  very first line of `ServeHandle::shutdown`, before calling `cluster.drain(...)`. This is a one-
  or two-line `hs-cli` change; flagging it here rather than making it, per this session's scope
  boundary.
- **Storage reachability**: not a live, ongoing check — it's checked once at startup (opening the
  backend is one of the first things `spawn_serve` does; failure there is a hard `Err`, the process
  never gets far enough to bind a listener at all). That's a reasonable fail-fast design for a
  storage handle that, once open, doesn't silently go away (Fjall) or is expected to reconnect
  transparently (a `sqlx`/similar pool, for the Postgres backend once that lands) — but it means a
  storage backend that becomes unreachable *after* startup (e.g., the Postgres backend once
  implemented, if the network partition outlasts the pool's own retry logic) has no dedicated
  `/health/ready` signal distinguishing it from "healthy" today. Worth a real check
  (`SELECT 1`-equivalent, rate-limited so it doesn't hammer the backend every 5s per the chart's
  `probes.readiness.periodSeconds`) once a reconnecting backend exists; moot for the embedded
  (Fjall) backend, which doesn't fail this way.
- **Listener-bound status**: implicit and correct as-is — a listener that failed to bind is a
  startup `Err` (same fail-fast shape as storage), and `/health/live`/`/health/ready` cannot even
  be reached by a probe on a port that never opened, so Kubernetes already sees this correctly via
  `startupProbe`/`livenessProbe` timing out against a connection refused, not a wrong 200.

No `hs-cli` files were edited to produce this section — it is exactly what's on disk today, read
and cross-referenced against `hs-cluster`'s own doc comments.

 `hs serve` mounted only
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

- **Now unblocked, not yet done**: wire `hs-operator`'s stub reconcile functions into real
  create/patch calls against owned resources (`StatefulSet`, `ConfigMap`, `Service`) — the
  `live-smoke` bin (this session) proved the `Controller`/API-server plumbing works, so this is
  now purely "write the reconcile logic", not "find out whether a controller can even run here".
  Also still open: status-subresource patches (`.status().patch(...)`) and finalizers — neither
  attempted this session, `live-smoke` only exercised the read side.
- Fold `deploy/helm/hs` into `element-hq/ess-helm`'s `matrix-stack` umbrella chart as an
  alternative to its `synapse:` block (today it is a standalone chart with an aligned-but-separate
  values schema).
- **Cosign signing and SBOM generation for the image** (`PLAN.md` section 7's requirement): still
  not started. The image itself now builds and runs (this session); signing/SBOM tooling around
  it is a separate step.
- Multi-arch: this session only built and ran `linux/arm64` natively (OrbStack on Apple Silicon).
  `docker buildx build --platform linux/amd64,linux/arm64` (the Dockerfile's own documented
  invocation) was not attempted — worth a follow-up run once there's a reason to believe the
  amd64 leg needs anything the arm64 leg didn't already exercise (unlikely, given no
  architecture-specific code path exists, but unverified is unverified).
- A real image registry the cluster's nodes can pull from: this session's chart install proved
  everything up to the image pull (PVC binding, ConfigMap/Secret mounts, security context
  admission all real and correct) and stopped at `ErrImageNeverPull` because there is no registry
  bridging this Mac's local Docker images to the Talos cluster's containerd. Push access to a
  registry both sides can reach (`ghcr.io` with real credentials, or a registry inside the
  cluster) is what the next full-pod-boot verification needs.
- Turn `.github/workflows/nightly-bench.yml` from "runs and uploads raw output" into a real
  pass/fail budget check against `PLAN.md` section 7.4's numbers, and get a dedicated (non-shared,
  non-GitHub-hosted) arm64 rig instead of `ubuntu-24.04-arm`.
- Per-listener resource filtering in `hs serve` (`listeners[].resources` is parsed and stored but
  every listener currently serves the full router regardless of its declared resource list — see
  `crates/hs-cli/src/serve.rs`'s `build_router` doc comment). Not this track's file to edit this
  session; noted for whoever owns `hs-cli` next.
- `docs/config.md` generation, Debian/RPM packages, a Nix flake, cert-manager mTLS for the mesh,
  the arm64 benchmark rig producing real numbers: still none started.

## Blockers

- None outright. Docker and a real (if not disposable) cluster both turned out to be available
  this session — see "Decisions made" for how the non-disposable-cluster question was handled.
  The one genuine environment gap found: no image registry reachable from both this Mac and the
  cluster's nodes, so a full pod successfully pulling and running this session's image was not
  achievable here (see "In progress / Next").

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
- `hs-operator`: CRD schemas (Rust types in `hs_operator::crds`, generated YAML in `deploy/crds/`),
  now also `live-smoke` (`cargo run -p hs-operator --bin live-smoke`), a manual diagnostic that
  proves the reconcile stubs run against a real `kube::runtime::Controller`/API server — see "3.
  The operator" above.
- `deploy/helm/hs`: the chart values schema (`deploy/helm/hs/values.yaml`), now with a fixed
  `signing_key_path` mount shape (a directory, not a `subPath` file — see "1. Production image").
- CI: `.github/workflows/ci.yml` is what every track's PR now runs against.

## Interfaces needed

- **From track 03 or whoever owns `hs-cli`'s shutdown path next**: `crate::serve::ServeHandle::shutdown`
  calls `Cluster::drain(...)` on `SIGTERM` but never flips the per-process `ready` `AtomicBool`
  (`serve.rs:940`) to `false` first. If `Cluster::drain` doesn't already make `cluster.ready()`
  report `NotReady` for its own duration, `/health/ready` can keep answering 200 for up to
  `CLUSTER_DRAIN_DEADLINE` after a pod receives `SIGTERM`, letting the Kubernetes `Service` keep
  routing new requests to a pod that is mid-handoff. Suggested fix (one or two lines, in
  `hs-cli`, not this track's file to edit this session):
  `ready.store(false, Ordering::SeqCst)` as the first line of `ServeHandle::shutdown`, before
  `cluster.drain(...)`. See "4. Probes" above for the full trace.
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

- **Touching the real cluster, this session (2026-09-19)**: the previous session found
  `admin@dacrib0` (a real, long-lived cluster, not disposable) and deliberately used only
  `--dry-run=client` against it, flagging the decision of whether to go further as one for
  whoever picks up cluster-dependent work next. This session made that call: every interaction
  was scoped to a namespace or CRD this session created itself, verified empty/gone afterward
  (`kubectl get ns`, `kubectl get crd | grep matrix`), and nothing pre-existing on the cluster was
  read, modified, or deleted. See "2. Chart installed against a real cluster" and "3. The
  operator" above for exactly what ran and what was torn down.
- **`deploy/Dockerfile`'s missing `ARG RUST_VERSION`**: fixed with a default
  (`ARG RUST_VERSION=1.98`) rather than requiring `--build-arg` on every invocation, so CI and a
  developer's plain `docker build` both work without extra flags; still overridable.
- **Chart's `signing_key_path` mount shape**: changed from a `subPath` file mount to a whole-
  Secret directory mount (see "1. Production image" above for why the file mount was a live bug,
  reproduced and fixed this session). `secrets.signingKey.key` in `values.yaml` is kept as
  documentation of which data key inside the Secret should hold the key, even though it is no
  longer used to build a `subPath` — removing the field entirely would be a values-schema break
  for no benefit, since the field still answers a real question (“what should I name the key when
  I create this Secret”).
- **`hs-operator` `live-smoke` bin, not a test**: a real `kube::Client`/`Controller` run against a
  live API server does not fit `cargo test`'s model (no cluster is guaranteed to exist, and this
  crate's other tests must stay fast and hermetic), so it is a separate opt-in binary rather than
  a `#[test]` behind an env-var guard. Kept in the crate (not deleted after use) so the proof is
  re-runnable, e.g. from a future `kind`-based CI job, instead of being a one-time claim in this
  file that nobody can check.
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
- `rustls = { workspace = true }` added as a direct dependency of `hs-operator` (2026-09-19; the
  workspace entry already existed for other crates, so no root `Cargo.toml` change) — only used by
  the new `live-smoke` diagnostic bin, to install a process-level `CryptoProvider` before `kube`'s
  `rustls-tls` feature makes its first TLS connection. See `src/bin/live_smoke.rs`.

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

## How to verify this session's additions (2026-09-19)

```sh
# Production image: build, run, curl, restart, teardown (needs Docker)
docker build -f deploy/Dockerfile -t hs:verify .
docker run --rm hs:verify generate-signing-key > /tmp/hs-verify/signing-key-dir/signing.key
docker run --rm hs:verify generate-config --server-name verify.example > /tmp/hs-verify/homeserver.yaml
# edit the generated config's signing_key_path/data_dir/media path to the mounts below, then:
docker run -d --name hs-verify --read-only --tmpfs /tmp \
  -v /tmp/hs-verify/homeserver.yaml:/etc/hs/config/homeserver.yaml:ro \
  -v /tmp/hs-verify/signing-key-dir:/etc/hs/secrets/signing-key:ro \
  -v /tmp/hs-verify/data:/var/lib/hs/data -v /tmp/hs-verify/media:/var/lib/hs/media \
  -p 18108:8008 hs:verify serve -c /etc/hs/config/homeserver.yaml
curl http://127.0.0.1:18108/health/live      # expect 200 "ok"
curl http://127.0.0.1:18108/health/ready     # expect 200 "ready"
curl http://127.0.0.1:18108/_matrix/client/versions
docker logs hs-verify | grep -i ephemeral    # expect NO match (persistent key loaded)
docker top hs-verify -o user                 # expect 65532 (non-root)
docker rm -f hs-verify; docker rmi hs:verify

# Chart against a real cluster (needs a kubectl context; creates/tears down a scratch namespace)
kubectl create namespace hs-verify
kubectl -n hs-verify create secret generic hs-signing-key --from-file=signing.key=/tmp/hs-verify/signing-key-dir/signing.key
helm install hstest deploy/helm/hs -n hs-verify --set serverName=verify.example \
  --set secrets.signingKey.existingSecret=hs-signing-key --set mode=singleNode \
  --set image.repository=hs --set image.tag=verify --set image.pullPolicy=Never
kubectl -n hs-verify get pod,pvc,svc   # expect PVC Bound, pod scheduled (ImagePullBackOff expected
                                        # with no reachable registry — everything else should be clean)
helm uninstall hstest -n hs-verify; kubectl delete namespace hs-verify

# CRDs against a real API server (server-side dry run, no state left behind)
for f in deploy/crds/*.yaml; do kubectl apply --dry-run=server -f "$f"; done

# Operator: real Controller against a real API server (creates/tears down a CRD + scratch namespace)
kubectl apply -f deploy/crds/homeserver.yaml
kubectl create namespace hs-operator-smoke
kubectl apply -n hs-operator-smoke -f <a sample Homeserver manifest — see src/bin/live_smoke.rs's doc comment>
cargo build -p hs-operator --bin live-smoke
HS_OPERATOR_SMOKE_NAMESPACE=hs-operator-smoke ./target/debug/live-smoke   # expect "SMOKE-OK: reconciled ..."
kubectl delete namespace hs-operator-smoke
kubectl delete -f deploy/crds/homeserver.yaml

# hs-operator unit tests + lint (unchanged pass count, now also covers the live-smoke bin target)
cargo fmt -p hs-operator
cargo clippy -p hs-operator --all-targets -- -D warnings
cargo test -p hs-operator
```
