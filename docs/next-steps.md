# Where this is, and what comes next

Written 2026-09-18 by the integration lead; updated the same day, after mounting federation. `PLAN.md` is the design and does not change often; this file is the resume point and changes every session. The generated `docs/status/dashboard.md` is the measurement; per-track detail lives in `docs/status/NN-*.md`.

## What works today, verified against the running binary

Not "the tests pass". These were done by starting `hs serve` and using it:

- A user registers, and after stopping the process and starting a fresh one against the same data directory, logs in again. Duplicate registration is refused as taken; a wrong password is refused. Persistence is real.
- A room is created, a message is sent, and the event comes back by id.
- A file uploads and downloads byte for byte.
- `/_matrix/client/versions` and `/capabilities` answer, health and readiness answer, metrics export per-route counters and histograms after traffic, and SIGTERM drains and exits cleanly.
- The management interface renders and its add-a-bridge flow passes end-to-end tests against real Chromium.

Spec coverage is **119 of 235 routes (50.6%)**: client-server 93/166, server-server 26/36. That number is generated from the manifest the server itself emits, and probing confirmed it does not overclaim: endpoints that are absent return 404 and are absent from the manifest too. It counts *registered*, and plenty of what is registered is a `501` seam -- each track's status file says which.

Federation reads answer from real room data as of this session: `hs serve` mounts the transport server at `/_matrix/federation/v1` behind its `X-Matrix` layer, publishes `GET /_matrix/key/v2/server` self-signed with the same key that signs events (checked against the running binary: the key ID it publishes is the one in the signing-key file), and `crates/hs-cli/src/federation.rs` adapts `hs-room`, `hs-auth` and `hs-e2e` onto the seams `hs-federation` defined for them. Mounting it found two bugs that only exist once mounted -- the signature verifier was checking the prefix-stripped URI, and `/backfill?v=` could not parse a repeated parameter. Both fixed; see `docs/status/06-federation.md`.

## The four things in flight when this was written

Check `docs/status/` for each before assuming they landed: sync (`05`), rewiring the room actor onto the state engine (`04`), end-to-end encryption (`08`), and push rules with notification counts (`10`).

## What to do next, in order

### 1. State at an event, so `/state` can answer about the past

This is now the single thing blocking federation from being useful rather than merely mounted. `hs_cli::federation::RegistryRoomSource::state_at` refuses (`404`) every event except the room's newest, because the room actor holds one flat current-state map and cannot reconstruct the state at an older event -- and answering with current state would hand a remote server state it has no way to know is wrong. `hs-state` has the machinery; track 04 owns exposing it through `RoomActorHandle`. Everything below that involves joining a real room needs this.

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

`crates/hs-bridge-conformance` asserts transaction shapes, key spellings, the null-url rule and device masquerading against a fake bridge over a real socket. The real test needs Docker: run `mautrix-irc` against a local IRC server, and run the conformance suite against Synapse as a control to prove the suite itself is right. Neither is possible on the machine this was built on.

### 5. Close the admin API's gap

Its 142 operations are mounted and answer not-implemented by design, and its token verifier rejects everyone. Track 07 needs to supply a real verifier, then the handlers need implementing behind it. The management interface is already built against the contract, so this is the work that makes those pages functional rather than mocked.

## Known gaps, honestly held

| Gap | Where | Consequence |
|---|---|---|
| Deferred and quarantine scan modes use a spawned task | `hs-media` | a crash loses an in-flight verdict |
| Room version 12 rejected outright | `hs-room` | we default to 11 while the spec and Synapse default to 12 |
| Media upload quota unlimited | `hs-cli` | no config fields exist for it yet |
| Cluster code exists but nothing runs multi-replica | `hs-cluster` | ownership, fencing and mesh are tested only in their own chaos harness |
| PostgreSQL and SlateDB backends absent | `hs-kv` | only the embedded backend can actually open |
| `/state` refuses any event but the newest | `hs-cli` federation adapter | a remote cannot resolve a gap it hit; see item 1 above |
| `GET /.well-known/matrix/server` not served | `hs-cli` | a deployment that delegates its server name cannot be found |
| 15 media test artifacts committed under `crates/hs-cli/media-store/` | `hs-media` tests | a test writes into the working directory; now gitignored, but the committed copies remain and the test still needs fixing |
| No Docker, no nightly compiler on the build machine | everywhere | Complement, Sytest, fuzzing, real scanners and real bridges have never run |

That last row is the biggest caveat on everything else. Complement is the industry's conformance suite for Matrix homeservers and it has never been run against this code. The harness is written and waiting.

## Conventions worth keeping

- **Verify by running, not by reading.** Every claim in the first section above was checked against the binary. Several reports were accurate and two were not, both in security-relevant code.
- **Mutation-test the load-bearing guarantees.** Disable the defence and confirm a test fails. This caught a chaos suite whose headline test passed with fencing switched off, and confirmed the encrypted-media, null-url and federation-auth guarantees were real.
- **Scope commits to the reporting track's paths.** `git add -A` while agents are running commits their half-written files; it broke HEAD once here.
- **Registered is not working.** Say which mounted routes are stubs.
