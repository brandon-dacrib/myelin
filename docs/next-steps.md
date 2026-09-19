# Where this is, and what comes next

Written 2026-09-18 by the integration lead; updated the same day, after mounting federation. `PLAN.md` is the design and does not change often; this file is the resume point and changes every session. The generated `docs/status/dashboard.md` is the measurement; per-track detail lives in `docs/status/NN-*.md`.

## What works today, verified against the running binary

Not "the tests pass". These were done by starting `hs serve` and using it:

- A user registers, and after stopping the process and starting a fresh one against the same data directory, logs in again. Duplicate registration is refused as taken; a wrong password is refused. Persistence is real.
- A room is created, a message is sent, and the event comes back by id.
- A file uploads and downloads byte for byte.
- `/_matrix/client/versions` and `/capabilities` answer, health and readiness answer, metrics export per-route counters and histograms after traffic, and SIGTERM drains and exits cleanly.
- The management interface renders and its add-a-bridge flow passes end-to-end tests against real Chromium.
- Federation reads answer over a real signed `X-Matrix` request: a remote with no member in a room is refused, a world-readable room serves full PDUs with signatures and hashes intact, `/state` answers for any event including historical ones, and `/backfill` and `/event_auth` walk the real DAG (`crates/hs-cli/tests/federation_reads.rs`).

Spec coverage is **119 of 235 routes (50.6%)**: client-server 93/166, server-server 26/36. That number is generated from the manifest the server itself emits, and probing confirmed it does not overclaim: endpoints that are absent return 404 and are absent from the manifest too. It counts *registered*, and plenty of what is registered is a `501` seam -- each track's status file says which.

Federation reads answer from real room data as of this session: `hs serve` mounts the transport server at `/_matrix/federation/v1` behind its `X-Matrix` layer, publishes `GET /_matrix/key/v2/server` self-signed with the same key that signs events (checked against the running binary: the key ID it publishes is the one in the signing-key file), and `crates/hs-cli/src/federation.rs` adapts `hs-room`, `hs-auth` and `hs-e2e` onto the seams `hs-federation` defined for them. Mounting it found two bugs that only exist once mounted -- the signature verifier was checking the prefix-stripped URI, and `/backfill?v=` could not parse a repeated parameter. Both fixed; see `docs/status/06-federation.md`.

## What is in flight

Nothing is running. Every agent from the last wave stalled (see below) and their salvageable work is committed. `docs/status/NN-*.md` is accurate per track as of the last time that track reported -- check the date at the top of each before trusting it, since a stalled agent never updated its own file.

## What happened to the eleven-agent wave

Eleven track agents were launched at once on 2026-09-18 and **every one stalled** (no progress for 600s, watchdog did not recover) on a 10-core machine that was simultaneously building Docker images and running `cargo` across eleven workspaces. Six on Sonnet had been the known-good batch size; eleven was not. Three left salvageable work, which was reviewed and committed by the integration lead (`state_at_event`, the admin token verifier, admin model types); one left an orphaned module referencing files it never wrote, which was discarded. The rest left nothing.

The lesson is the batch size, not the approach: the surviving `state_at_event` work was good, and wiring it closed the largest gap in the federation surface within minutes of salvaging it. **Launch six at a time**, and prefer tracks that do not all contend for the same cargo build lock.

## What to do next, in order

### 1. Wire the admin token verifier, then finish the admin API

`hs_auth::admin_verifier::AdminTokenVerifier` exists and has eight passing tests, and nothing uses it: `crates/hs-cli/src/serve.rs` still builds `dummy_admin_state()` with an empty `StaticVerifier`, so all 142 `/api/v1` operations still answer 401 to everyone. Wiring it needs one decision the agent that wrote it never reached -- where admin scopes are configured -- and then track 15's handlers can be implemented behind it, which is what makes the management interface real rather than mocked.

Regenerating the manifest and coverage after any route change is still:

```
cargo run -p hs-cli --bin hs routes-manifest -o docs/status/routes.json
cargo run -p hs-spec-coverage --bin hs-spec-coverage -- --spec-dir refs/matrix-spec/data/api --routes docs/status/routes.json
python3 tools/dashboard.py
```

### 2. Make a real client connect

The single best test of whether this is a homeserver. Once sync exists, point Element Web at it and try to log in, see a room, and receive a message. Expect failures in places no unit test covers. `matrix-rust-sdk` in `crates/hs-loadgen` is the scripted version of the same check.

### 3. Federation inbound: `/send` and the join handshakes

The read side is mounted and real. What is still a `501` seam is every write: `PUT /send/{txnId}`, `make_join`/`send_join`, `make_leave`/`send_leave`, `make_knock`/`send_knock` and `invite`. Note that the v2 spellings are currently registered *under v1* (`/_matrix/federation/v1/send_join/v2/...`), which nothing notices while they are seams and everything notices the moment they are not: mount a v2 router rather than implementing them where they sit. With inbound transactions, `send_join`, and item 1 above, try joining a real public room. That is the moment this becomes a Matrix server rather than a private one.

### 4. Bridges end to end

`crates/hs-bridge-conformance` asserts transaction shapes, key spellings, the null-url rule and device masquerading against a fake bridge over a real socket. That proves the suite is self-consistent, not that it is right. Docker is available now, so both real tests are possible and neither has been run: `mautrix-irc` against a local IRC server pointed at this server, and the conformance suite against Synapse as a control -- if Synapse fails an assertion this suite makes, the suite is wrong.

### 5. Run Complement and get the number

Docker is available and `refs/complement` is checked out, so the one externally-graded measure of whether this is a homeserver is finally possible. `tests/complement/` holds a harness that has never built an image: its `Dockerfile.template` refers to a crate named `hs-server` that has never existed, while the real binary is `hs` from `crates/hs-cli`. Fix that, run `./tests/complement/run_single_node.sh`, and write down the honest pass/fail/skip count plus a triage of the top failures by owning track. Until that number exists, every coverage percentage in this file measures surface area, not correctness.

## Known gaps, honestly held

| Gap | Where | Consequence |
|---|---|---|
| Deferred and quarantine scan modes use a spawned task | `hs-media` | a crash loses an in-flight verdict |
| Room version 12 rejected outright | `hs-room` | we default to 11 while the spec and Synapse default to 12 |
| Media upload quota unlimited | `hs-cli` | no config fields exist for it yet |
| Cluster code exists but nothing runs multi-replica | `hs-cluster` | ownership, fencing and mesh are tested only in their own chaos harness |
| PostgreSQL and SlateDB backends absent | `hs-kv` | only the embedded backend can actually open |
| `GET /.well-known/matrix/server` not served | `hs-cli` | a deployment that delegates its server name cannot be found |
| 15 media test artifacts committed under `crates/hs-cli/media-store/` | `hs-media` tests | a test writes into the working directory; now gitignored, but the committed copies remain and the test still needs fixing |
| No nightly compiler on the build machine | `cargo fuzz` | fuzz targets type-check but have never been run |

**Docker became available on 2026-09-18** and that retires the caveat that used to sit here. Complement -- the industry's conformance suite for Matrix homeservers, and the only externally-graded measure of whether this is a homeserver -- had never been run against this code; the harness in `tests/complement/` was written against a placeholder crate name and had never built an image. Track 14 is running it now. Until its number lands in `docs/status/14-test-and-conformance.md`, treat every coverage percentage in this file as a measure of surface area, not of correctness. Sytest, real malware scanners and real bridges (`mautrix-irc` against a local IRC server, with Synapse as the control) are now possible too, and are not yet done.

## Conventions worth keeping

- **Verify by running, not by reading.** Every claim in the first section above was checked against the binary. Several reports were accurate and two were not, both in security-relevant code.
- **Mutation-test the load-bearing guarantees.** Disable the defence and confirm a test fails. This caught a chaos suite whose headline test passed with fencing switched off, and confirmed the encrypted-media, null-url and federation-auth guarantees were real.
- **Scope commits to the reporting track's paths.** `git add -A` while agents are running commits their half-written files; it broke HEAD once here.
- **Registered is not working.** Say which mounted routes are stubs.
