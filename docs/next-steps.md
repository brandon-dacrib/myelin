# Where this is, and what comes next

Written 2026-09-19 by the integration lead, replacing the 2026-09-18 version. `PLAN.md` is the design and does not change often; this file is the resume point and changes every session. The generated `docs/status/dashboard.md` is the measurement; per-track detail lives in `docs/status/NN-*.md`.

## The number that matters now

**Complement, `csapi` package: 148 assertions pass, 138 fail, 7 skip** (35 of 106 top-level tests), measured 2026-09-19 against commit `576e1e1`. The first run that morning was 125/161; the day's work moved it by 23 assertions. Before any of this, every percentage in this file measured surface area. Now there is an external grade, and the triage by owning track is in `docs/status/14-test-and-conformance.md`.

**Complement's federation package ran for the first time the same day: 5 of 89 tests pass.** That number is real but it is not yet a measurement of this server's federation logic, because almost all of it never got past TLS: this server's outbound client trusts only the ~140 bundled public roots and never reads the OS trust store or any configured CA, so Complement's own test CA is rejected. Synapse has `federation_custom_ca_list` for exactly this; this server has no equivalent. The diagnosis was confirmed by disabling verification and watching 27 signature-verification failures turn into distinct, further-along bugs.

Reproduce it in one command:

```
./tests/complement/build.sh complement-hs-reimplement:dev
cd refs/complement && COMPLEMENT_BASE_IMAGE=complement-hs-reimplement:dev go test -v -timeout 30m ./tests/csapi/...
```

A cold image build is ~4 minutes; a full `csapi` run is ~15. The federation-heavy top-level `./tests/...` package has still never been run — that is the next session's first move, and it should score better than the number above, which predates the inbound-federation work.

Spec coverage is **138 of 235 routes (58.7%)**: client-server 108/166 (65.1%), server-server **30/36 (83.3%)**. Generated from the manifest the server itself emits, so it cannot overclaim. Registered is still not the same as working — each track's status file says which of its routes are stubs.

## What works today, verified by running the binary

Not "the tests pass". Each of these was checked against `hs serve` or a real external client:

- **A real Matrix client does a day's work.** `cargo test -p hs-loadgen --test real_client` drives two `matrix-rust-sdk` clients through 17 steps against the real binary: register, log in on a second device, create a room, invite, join, sync both, send both ways, read each other's messages on an incremental sync, set a display name, rename and re-topic the room, list members, paginate `/messages` backwards, log out, and get refused when reusing the revoked token.
- **A remote server can join a room here.** `send_join` verifies signature, content hash, shape and the real auth rules, then persists; the join reads back through `/event/{id}` with the remote's own signature intact and appears in the room's state (`crates/hs-cli/tests/federation_writes.rs`). `PUT /send/{txnId}` verifies and stores inbound transactions, idempotently by transaction id.
- **An operator can administer the server over HTTP.** `hs register --admin` mints a real admin through shared-secret registration; that token gets real data from `/api/v1/me`, `/server`, `/users`, can lock, unlock, deactivate, reactivate and promote users, and every mutation writes one audit entry and publishes one SSE event. A non-admin's token and an anonymous request both get 401.
- **The management interface renders that data in a browser**, signs in with a real token, and says "not implemented on this server yet" for the operations that answer 501 instead of showing an empty table.
- **Encryption works end to end.** `cargo test -p hs-loadgen --test real_client_encrypted` has one real `matrix-sdk` client encrypt a message and another decrypt it through this server. Device keys, key queries, cross-signing and atomic one-time-key claims under real concurrency (53 callers, 50 distinct keys, zero double-claims, the excess correctly served the reusable fallback key) all hold.
- **A user who left a private room cannot read it.** History visibility is enforced per event on `/messages`, `/event`, `/context`, `/state` and `/members`; a departed member sees the state as of when they left.
- **The server runs on PostgreSQL.** `storage.backend: postgres` boots, registers, creates rooms, sends messages, and survives a restart with everything intact.
- A user registers, restarts the server, and logs in again; a file uploads and downloads byte for byte; `/versions`, `/capabilities`, health, metrics and SIGTERM drain all answer; `.well-known` documents are served when configured and 404 when not.

## What to do next, in order

### 1. Re-measure, because almost everything moved

The 148/293 csapi headline and the 5/89 federation number both predate a dozen landings, several of which were the *named causes* of the failures they counted: outbound signing was spec-wrong and is fixed, private-CA trust exists, and threads, relations, room upgrade, typing, presence, room summaries, push rules in sync, profile propagation, device-list notifications and the v1 mount all landed after that run. A stale headline is worse than no headline, and nothing else on this list can be prioritised honestly until the number is current.

Run both packages against a pinned commit:

```
./tests/complement/build.sh complement-hs-reimplement:dev
cd refs/complement && COMPLEMENT_BASE_IMAGE=complement-hs-reimplement:dev go test -v -timeout 30m ./tests/csapi/...
cd refs/complement && COMPLEMENT_BASE_IMAGE=complement-hs-reimplement:dev go test -v -timeout 30m ./tests/...
```

The federation package is the interesting one: its previous run was almost entirely a TLS wall, and the harness still works around that with `verify_certificates: false`. Now that `federation.custom_ca_certificates` exists, try the harness *without* the workaround as well, and see whether real CA trust carries it.

### 2. Join a real public room, and point Element Web at it

This is the test of whether this is a Matrix server, and for the first time every known blocker is gone: `send_join` persists, backfill resolves missing ancestors, `.well-known` is served, signing is spec-correct, and a private CA can be trusted. Expect the attempt itself to expose things no suite covers — that has been true every time a real client was pointed at this server. `matrix-rust-sdk` is the scripted check; a browser is the honest one.

### 3. Wire the seams other tracks finished

Each of these is a crate with a working implementation on one side and nothing calling it:

- **`RoomDirectory`** — the admin API's room operations answer a real 503 because nothing implements the trait. The contract is written on the trait itself in `crates/hs-admin/src/sources.rs`; the implementation belongs in `hs-room`.
- **URL-preview config** — `hs-media` hardcodes timeout, fetch size and cache lifetime; the fields now exist in `hs-config` (`docs/status/13-*.md` names the two-line change).
- **Synapse admin shims** — five real read-only routes exist in `hs-compat` and nothing mounts them (`docs/status/13-*.md` has the merge lines).
- **Readiness on drain** — `hs serve` drains the cluster on SIGTERM but never flips the readiness flag false first, so a pod can keep taking traffic through its drain window.

### 4. Finish the cluster story

Two replicas no longer fork a room's history. Two gaps remain, both recorded in `docs/status/03-cluster.md`: `/createRoom` is not shard-gated, because the room id does not exist when the request arrives; and `RoomActor::persist` never calls `Fence::check`, which is the belt-and-braces against a stale ownership read racing a real handoff. Neither is needed for the bug that was fixed, both are needed before anyone trusts this under failover.

### 5. Bridges, end to end

Still never done, and it is one of the project's stated priorities. `crates/hs-bridge-conformance` asserts transaction shapes against a fake bridge, which proves the suite is self-consistent, not that it is right. Docker works: run `mautrix-irc` against a local IRC server pointed at this server, and run the conformance suite against Synapse as a control — if Synapse fails an assertion this suite makes, the suite is wrong.

### 6. The smaller honest gaps

`/search` needs a cross-room index the per-room actor model has no place for, and is the last of the four 404ing endpoints. `web/`'s Vitest workers time out on this machine and its unit tests have never been run — that is unverified, not passing. Sytest has never run (CPAN dependencies absent). `cargo fuzz` targets type-check but have never executed (no nightly toolchain).

## Known gaps, honestly held

| Gap | Where | Consequence |
|---|---|---|
| `/search` unimplemented | `hs-room` | needs a cross-room index the actor model has no place for |
| `/context`'s `state` reads live state, not state at the event | `hs-room` | same bug class as history visibility, one path left |
| No backfill | `hs-federation` | a join cannot be followed by history |
| `/createRoom` is not shard-gated | `hs-cli` | the first actor may be built on a non-owner |
| Fencing not called in the write path | `hs-room` | no guard against a stale ownership read |
| Postgres `tls` refused, `pool_size` ignored | `hs-kv`, `hs-cli` | encrypt in front of the database for now |
| SlateDB backend absent | `hs-kv` | deliberately not started |
| Profile changes do not rewrite existing memberships | `hs-room` | a rename shows only in rooms joined afterwards |
| Audit log is in-memory | `hs-admin` | admin history does not survive a restart |
| Deferred and quarantine scan modes use a spawned task | `hs-media` | a crash loses an in-flight verdict |
| Media upload quota unlimited | `hs-cli` | no config fields exist for it yet |
| `web/`'s unit tests and lint could not run | `web` | Vitest workers time out on a loaded machine; rerun `npm run check` when idle |
| No nightly compiler on the build machine | `cargo fuzz` | fuzz targets type-check but have never been run |
| Sytest never run | `tests/sytest` | CPAN dependencies are not installed here |

## Conventions worth keeping

- **Verify by running, not by reading.** Every claim in the second section above was checked against the binary or an external client. This session a track reported a clean typecheck that was not clean, and another reported a gap that had already been closed.
- **Point real clients at it.** Two bugs no unit test had caught fell out within minutes of a real SDK connecting — a join response missing `room_id`, and a 404 on the trailing-slash spelling of a state URL. The tests all passed because they spoke the server's own dialect.
- **Mutation-test the load-bearing guarantees.** Disable the defence and confirm a test fails. This session it caught that two existing federation signature tests would have passed with signature checking switched off.
- **Scope commits to the reporting track's paths.** `git add -A` while agents are running commits half-written files; it broke HEAD once here.
- **Registered is not working.** Say which mounted routes are stubs, and give counts: 15 of 142 admin operations are real, 127 answer 501 after real authorization.
