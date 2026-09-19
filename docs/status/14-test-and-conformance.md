# 14 Test and conformance (integration lead): status

## Complement: the honest number (2026-09-18, this session)

**The image builds and Complement runs against it.** This had never happened before this session
(`Dockerfile.template` referenced a crate named `hs-server` that never existed; the real binary is
`hs` from `crates/hs-cli`). Reproduce with:

```bash
./tests/complement/build.sh complement-hs-reimplement:dev   # ~4 min cold, ~1s once cargo's layer is cached
cd refs/complement && COMPLEMENT_BASE_IMAGE=complement-hs-reimplement:dev \
  go test -v -timeout 30m ./tests/csapi/...
# or the full wrapper (adds the blacklist -skip regex): ./tests/complement/run_single_node.sh
```

**Scope of this number: `tests/csapi` only** (106 top-level Go test functions, the core Matrix
client-server API suite), not the top-level `./tests/...` package (federation-heavy, two-homeserver
blueprints, ~90 more top-level tests) or any `tests/mscNNNN` package. `./tests/csapi/...` was
chosen because it is the largest single package that exercises this server without needing
federation to work first, and because a first number needed to exist before a second, harder one
did. Both runs below used the embedded (Fjall) storage backend, single node, default blacklist
(empty — nothing is skipped by this harness's own `-skip` regex; the two `SKIP`s below are
Complement's own, for the shared-secret-registration and server-notices tests this server
correctly reports as unsupported by 404/absence).

Two full runs were done, back to back, because the workspace is being edited live by five other
tracks and the first run's image was built moments before track 04 fixed a routing bug this run's
own diagnosis had just found (see below) — rebuilding and rerunning showed the fix land in real
numbers, which is a better demonstration of the harness working than either number alone.

| Run | Image built from | Leaf-level (every individual assertion) | Top-level (Go `func Test*`) |
|---|---|---|---|
| 1 | working tree at 2026-09-18 ~22:50 (before the state-key routing fix) | 283 total: **106 pass, 171 fail, 6 skip** | 106 total: **26 pass, 78 fail, 2 skip** |
| 2 | working tree at 2026-09-18 ~23:40 (after it) | 293 total: **125 pass, 161 fail, 7 skip** | 106 total: **30 pass, 74 fail, 2 skip** |

Full logs: `/tmp/complement-csapi.log` (run 1), `/tmp/complement-csapi2.log` (run 2) — not
committed (scratch files outside the repo's tracked paths); rerun the reproduction command above to
regenerate. Leaf-level counts are every individual `--- PASS/FAIL/SKIP` line at maximum
indentation (i.e., counting `TestFoo/sub/subsub` once, not also its parents `TestFoo` and
`TestFoo/sub`); top-level counts every `func Test*` Go treats as its own top-level test (each of
which may itself represent a table of many spec assertions).

**Both runs are against whatever was on disk in this live, multi-agent-edited working tree at
build time, not a specific git commit** — `tests/complement/build.sh` streams the actual working
directory (tracked and untracked changes both), not `git archive`, so uncommitted work from other
tracks' concurrent sessions is included. `git rev-parse HEAD` read at report time (after both test
runs finished) was `6d6d7beff41f3832d1efc8ebd2d8930adfec51c8`, but that is the commit *after* the
runs, not what was actually tested; do not treat it as precise provenance, only as a lower bound on
recency. A rerun against a later HEAD (this session was told mid-run that inbound federation
`/send`, `make_join`/`send_join`, `.well-known`, profiles and room v12 landed since) would very
likely score higher, especially on the federation-touching tests below.

### Top failures, grouped by owning track (from run 2's leaf-level failure messages)

1. **Track 04 (room-and-events) — room-state and lifecycle validation gaps.** `POST /createRoom`
   with invalid `room_version`/params returns `200` where the spec test expects `400`; extensible
   `m.topic` (`m.topic.m.text`, MSC3765) is not populated on room creation; `POST
   /rooms/{roomId}/forget` returns `200` instead of `400`/`403` for a non-existent or non-member
   room. (`apidoc_room_create_test.go`, `apidoc_room_forget_test.go`.)
2. **Track 04/02 (room-and-events / state) — history-visibility is not enforced on read.**
   `GET /rooms/{roomId}/messages` and `GET /rooms/{roomId}/event/{eventId}` return `200` with full
   event content for a room the requesting user has left and that is not world-readable, where the
   spec test expects `403`/`404`. This also explains several downstream `MustSyncUntil: timed out`
   failures in the same test files, since the test's next step depends on the prior assertion.
   (`apidoc_room_history_visibility_test.go`.)
3. **Track 04 (room-and-events) — room directory.** `PUT
   /_matrix/client/v3/directory/list/room/{roomId}` returns `404` (route not mounted, or not
   wired to the room registry), so `GET /publicRooms` never sees the room and the test's
   `RetryUntil` polling loop times out. (`power_levels_test.go`'s use of `public_rooms_test.go`
   helpers; also directly in `TestPublicRooms`.)
4. **Track 08 (e2ee) — key management round-tripping.** `POST /keys/query` for another user's
   device keys comes back empty (`device_keys.@user-2:hs1` missing) in a cross-user query; `POST
   /keys/claim`'s returned one-time-key content/signature does not match what was uploaded;
   `POST /keys/upload` accepts a malformed device-ID-as-object shape the test expects rejected
   with `400`. (`upload_keys_test.go`, `user_query_keys_test.go`.)
5. **Track 09 (media) — async upload and URL preview unimplemented.** `POST
   /_matrix/media/v1/create` (MSC2246 async upload) returns `404`; `GET
   /_matrix/media/v3/preview_url` returns `404`. (`media_async_uploads_test.go`,
   `url_preview_test.go`.)
6. **Track 05 (sync, which per this project's ownership map also owns presence) — presence
   endpoint absent or incomplete.** `GET`/`PUT /_matrix/client/v3/presence/{userId}/status` return
   `404` or a non-JSON body rather than a presence document. (`apidoc_presence_test.go`.)
7. **Track 06 (federation) — cross-server join signature verification, seen from inside a csapi
   test.** One csapi test (`TestSync`'s federation-join helper) hit `make_join` on a *second*
   homeserver via `X-Matrix` and got `401 M_UNAUTHORIZED: signature verification failed`. Per the
   coordinator's update, inbound `/send`, `make_join`/`send_join` landed on HEAD after this run's
   image was built; this failure may already be fixed and should be the first thing a rerun
   checks.
8. **Track 07 (auth) — minor device/session validation gaps.** `DELETE /device/{deviceId}` with a
   malformed body returns `400` where UIA should be attempted first (`401`); `GET
   /_matrix/client/v3/devices` after logging out other sessions returns one more device than
   expected, suggesting a device isn't being cleaned up on that logout path.
   (`apidoc_device_management_test.go`, `apidoc_logout_test.go`.)

### Harness bugs found and fixed this session (all within `tests/complement/`, so fixed directly)

- **`tests/complement/Dockerfile.template` referenced a nonexistent `hs-server` crate.** Fixed to
  build the real `[[bin]] name = "hs"` from `crates/hs-cli` (`cargo build --release --jobs 4 -p
  hs-cli`; `--jobs 4` deliberately, not the full core count, per this track's brief about sharing
  a 10-core machine with five other agents' `cargo` invocations).
- **`hs serve` does not terminate TLS** (`crates/hs-cli/src/serve.rs` logs a warning and binds
  plaintext even with a listener's `tls:` block set — confirmed by reading the code). Complement
  requires HTTPS on 8448. Fixed by running `stunnel4` (chosen over `nginx`/`nginx-light` for
  footprint and because it needs no HTTP-proxy configuration, only "TLS in, plaintext out") inside
  the image, terminating TLS on 8448 and forwarding to `hs` on 8008 — safe because `hs serve`'s one
  `axum::Router` already answers every resource (client *and* federation) on whichever port it's
  bound to (see `crate::serve::build_router`'s doc comment), so `hs` only needs one plaintext
  listener. `tests/complement/stunnel.conf.template` + `startup.sh` do the cert-signing (unchanged
  from the original scaffold's already-correct `openssl` recipe) and stunnel config generation.
- **The build context was `.` (repository root)**, and `target/` alone is 27 GB on this shared
  workspace; a root `.dockerignore` is not a file this track owns. Fixed by having `build.sh`
  stream a `tar` (excluding `target`, `.git`, `web/node_modules`, `refs`, `media-store`,
  `.conformance-run`) to `docker build`'s stdin instead of using `.` as the context directly.
  Aside: macOS's `bsdtar` auto-skips any directory containing a `CACHEDIR.TAG` (which every Cargo
  `target/` directory has, including nested ones like `crates/hs-federation/fuzz/target`), so the
  explicit `target` exclude turned out to be redundant but is kept for portability to GNU tar.
- **`tests/complement/skip_regex.sh` failed under `set -o pipefail` whenever the blacklist had zero
  active (non-comment, non-blank) lines** — exactly the documented, intended common case — because
  `grep -v` with no matching lines exits 1, and `pipefail` propagates that through the pipeline
  even though the final `paste` succeeds. This aborted `run_single_node.sh` (`SKIP_REGEX="$(...)"`
  under `set -e`) before it ever built the image, so the checked-in wrapper script had never
  actually reached `go test` even after the image built. Fixed with `{ grep ... || true; }` on
  both filtering stages.
- **`VOLUME /data`** in the Dockerfile made Complement print "volumes can lead to unpredictable
  behaviour due to test pollution" on every run (Complement's own contract-linting warning).
  Removed — storage is embedded in the container's own writable layer, so no volume was ever
  needed for the "manage its own storage" requirement.

### Build time on this machine (a finding worth recording per the coordinator's request)

Cold (`cargo build --release --jobs 4 -p hs-cli` inside the build stage, no warm target cache in
the image): **~4 minutes** (`4m 00s` reported by `cargo` itself, ~251s total Docker stage time,
including dependency compilation from scratch — the base `rust:1.98-slim` image has no crates.io
cache). A rebuild that only touches the final runtime stage (no source change): a few hundred
milliseconds, fully cached. A rebuild after a source change elsewhere in the workspace: not
measured directly, but expect somewhere between these two depending on how much of the dependency
graph the change invalidates — Docker's layer cache does not help across *source* changes the way
`cargo`'s own incremental compilation would, since `COPY . .` invalidates every layer after it
whenever any file changes. Each `go test` run of `./tests/csapi/...` (106 top-level tests, one or
two containers each, ~5s container-deploy overhead per test) took **~15 minutes** end to end
(947s and 909s respectively).

### What a next session should do first

1. Rerun against current HEAD (the federation landing mentioned above) — the `make_join` signature
   failure and possibly others in the list above may already be gone.
2. Run the full top-level `./tests/...` package (federation-heavy, two-homeserver blueprints), not
   just `./tests/csapi/...` — this session did not have time for it after two `csapi` runs.
3. Once the top-5 list above is address by each owning track, populate `blacklist.txt` with the
   *remaining* known-and-understood gaps (each with a one-line reason), the way Palpo's own
   blacklist is grown, rather than leaving it empty forever.

---

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

- **Complement is now real** (see the top of this file): rerun `./tests/complement/build.sh &&
  cd refs/complement && go test ./tests/csapi/...` against current HEAD, then run the full
  top-level `./tests/...` package (federation-heavy) which this session didn't have time for.
- Fill in the still-`TODO` `tests/sytest/plugins/hs-reimplement/lib/SyTest/Homeserver/
  HsReimplement.pm` the same way `tests/complement/`'s scaffold was filled in this session (real
  `hs` binary, real config) — Sytest's own CPAN dependencies still aren't installed in this
  environment, so it can be wired up but not run end to end yet.
- Once `hs-http`'s `Builder` (or `hs-admin-mock`) is wired into a binary that writes a real
  `routes.json`: point `hs-spec-coverage`/`tools/dashboard.py` at it and watch the coverage number
  move off 0%. (Unrelated to Complement — `hs serve` already writes `routes.json` via
  `--routes-manifest`; this bullet is about whichever track's status file still says 0%.)
- When network is available: record a real Synapse 1.161 baseline (`tests/differential/README.md`)
  and validate the two `tests/oracle/` fixtures against an installed `matrix-synapse`.
- `hs-loadgen` once the rest of Phase 0 is further along and a server exists to load-test.
- Re-run `tools/dashboard.py` after any status file changes; it is meant to be regenerated often,
  not hand-edited.

## Blockers

None for this session's own scope. Complement is unblocked as of this session (Docker + a real
binary both exist now). Still blocked on inputs outside this track's control: network access
(`pip install matrix-synapse`, Sytest's CPAN deps) for the oracle harness and the Sytest plugin's
own end-to-end run.

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
