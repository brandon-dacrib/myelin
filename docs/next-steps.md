# Where this is, and what comes next

Written 2026-09-19 by the integration lead, replacing the 2026-09-18 version. `PLAN.md` is the design and does not change often; this file is the resume point and changes every session. The generated `docs/status/dashboard.md` is the measurement; per-track detail lives in `docs/status/NN-*.md`.

## The number that matters now

**Complement ran against this server for the first time on 2026-09-19: 125 assertions pass, 161 fail, 7 skip** on the `csapi` package (30 of 106 top-level tests). Before this, every percentage in this file measured surface area. Now there is an external grade, and the triage by owning track is in `docs/status/14-test-and-conformance.md`.

Reproduce it in one command:

```
./tests/complement/build.sh complement-hs-reimplement:dev
cd refs/complement && COMPLEMENT_BASE_IMAGE=complement-hs-reimplement:dev go test -v -timeout 30m ./tests/csapi/...
```

A cold image build is ~4 minutes; a full `csapi` run is ~15. The federation-heavy top-level `./tests/...` package has still never been run — that is the next session's first move, and it should score better than the number above, which predates the inbound-federation work.

Spec coverage is **125 of 235 routes (53.2%)**: client-server 95/166, server-server **30/36 (83.3%)**. Generated from the manifest the server itself emits, so it cannot overclaim. Registered is still not the same as working — each track's status file says which of its routes are stubs.

## What works today, verified by running the binary

Not "the tests pass". Each of these was checked against `hs serve` or a real external client:

- **A real Matrix client does a day's work.** `cargo test -p hs-loadgen --test real_client` drives two `matrix-rust-sdk` clients through 17 steps against the real binary: register, log in on a second device, create a room, invite, join, sync both, send both ways, read each other's messages on an incremental sync, set a display name, rename and re-topic the room, list members, paginate `/messages` backwards, log out, and get refused when reusing the revoked token.
- **A remote server can join a room here.** `send_join` verifies signature, content hash, shape and the real auth rules, then persists; the join reads back through `/event/{id}` with the remote's own signature intact and appears in the room's state (`crates/hs-cli/tests/federation_writes.rs`). `PUT /send/{txnId}` verifies and stores inbound transactions, idempotently by transaction id.
- **An operator can administer the server over HTTP.** `hs register --admin` mints a real admin through shared-secret registration; that token gets real data from `/api/v1/me`, `/server`, `/users`, can lock, unlock, deactivate, reactivate and promote users, and every mutation writes one audit entry and publishes one SSE event. A non-admin's token and an anonymous request both get 401.
- **The management interface renders that data in a browser**, signs in with a real token, and says "not implemented on this server yet" for the operations that answer 501 instead of showing an empty table.
- **An encrypting client gets everything except the room key.** Device keys, one-time keys, key queries, cross-signing and 203 concurrent atomic one-time-key claims (zero double-claims) all work against a real `matrix-sdk` with encryption on. Decryption still fails — see the top gap below.
- A user registers, restarts the server, and logs in again; a file uploads and downloads byte for byte; `/versions`, `/capabilities`, health, metrics and SIGTERM drain all answer; `.well-known` documents are served when configured and 404 when not.

## What to do next, in order

### 1. Work down the Complement failures

The list is in `docs/status/14-test-and-conformance.md`, triaged by track. In priority order:

- **History visibility is not enforced on reads** (track 04, in progress at the time of writing). A user who left a non-world-readable room can still read `/messages` and `/event/{id}`. This is a real privacy bug, not a conformance nicety, and several other failures are downstream timeouts caused by it.
- **The room directory 404s** (track 04): `PUT /directory/list/room/{roomId}` is unserved, so no room can ever be published and `/publicRooms` is always empty.
- **`/createRoom` accepts invalid parameters** (track 04) where the spec requires 400.
- **Cross-user `/keys/query` misses devices** and `/keys/claim` returns content that does not match what was uploaded (track 08).
- **Async media upload (MSC2246) and URL previews are absent** (track 09).

### 2. Make encryption actually work

`docs/rfcs/0013-e2ee-sync-extensions.md` specifies it precisely: `GET /sync` omits `to_device`, `device_lists`, `device_one_time_keys_count` and `device_unused_fallback_key_types` entirely, so the Megolm key never reaches the recipient. The four `hs-e2e` store methods it needs already exist and are tested. The acceptance check is `cargo test -p hs-loadgen --test real_client_encrypted`, which logs two `KNOWN BUG` lines today and must stop.

### 3. Wire the PostgreSQL backend into `hs serve`

`hs_kv::postgres_backend::PostgresBackend` exists and passes 9 of 11 conformance scenarios against a real PostgreSQL 17 (the two divergences are documented and understood — see `docs/status/01-storage-engine.md`). Nothing can open it yet: `crates/hs-cli/src/storage.rs` still answers `BackendNotImplemented` for `postgres`, and `spawn_serve` destructures `OpenedStorage::Embedded` irrefutably, so adding the variant means dispatching the generic server setup over two backend types. The exact steps are in that status file under "Wiring the integration lead must add", including the two config fields that have no effect yet (`tls`, which the backend does not support, and `pool_size`).

Until this lands, no multi-replica deployment is possible, and every cluster feature in `hs-cluster` is a cluster over nothing shareable.

### 4. Backfill, so a join can be more than a join

Inbound events that cite ancestors this server does not hold are refused with a distinct `MissingAncestors` error naming the event IDs. The backfill-then-retry loop that would resolve them is not built. Until it is, joining a real public room will get through the handshake and then stall on the first event whose history we lack.

### 5. Try a real public room, and Element Web

With `send_join` persisting and `.well-known` served, the remaining blockers to joining a real public federated room are item 4 and whatever the attempt itself exposes. Point Element Web at the server too — `matrix-rust-sdk` is the scripted check, a browser client is the honest one.

## Known gaps, honestly held

| Gap | Where | Consequence |
|---|---|---|
| History visibility not enforced on reads | `hs-room` | a user who left a private room can still read it |
| `/sync` omits every E2EE field | `hs-user` | no client can decrypt anything; see RFC 0013 |
| Room directory unserved | `hs-room` | published rooms never appear in `/publicRooms` |
| No backfill | `hs-federation` | a join cannot be followed by history |
| PostgreSQL backend not wired into `hs serve` | `hs-cli` | still single-node only in practice |
| SlateDB backend absent | `hs-kv` | deliberately not started |
| Profile changes do not rewrite existing memberships | `hs-room` | a rename shows only in rooms joined afterwards |
| Audit log is in-memory | `hs-admin` | admin history does not survive a restart |
| Deferred and quarantine scan modes use a spawned task | `hs-media` | a crash loses an in-flight verdict |
| Media upload quota unlimited | `hs-cli` | no config fields exist for it yet |
| Cluster code exists but nothing runs multi-replica | `hs-cluster` | ownership, fencing and mesh are tested only in their own chaos harness |
| `web/`'s unit tests and lint could not run | `web` | Vitest workers time out on a loaded machine; rerun `npm run check` when idle |
| No nightly compiler on the build machine | `cargo fuzz` | fuzz targets type-check but have never been run |
| Sytest never run | `tests/sytest` | CPAN dependencies are not installed here |

## Conventions worth keeping

- **Verify by running, not by reading.** Every claim in the second section above was checked against the binary or an external client. This session a track reported a clean typecheck that was not clean, and another reported a gap that had already been closed.
- **Point real clients at it.** Two bugs no unit test had caught fell out within minutes of a real SDK connecting — a join response missing `room_id`, and a 404 on the trailing-slash spelling of a state URL. The tests all passed because they spoke the server's own dialect.
- **Mutation-test the load-bearing guarantees.** Disable the defence and confirm a test fails. This session it caught that two existing federation signature tests would have passed with signature checking switched off.
- **Scope commits to the reporting track's paths.** `git add -A` while agents are running commits half-written files; it broke HEAD once here.
- **Registered is not working.** Say which mounted routes are stubs, and give counts: 15 of 142 admin operations are real, 127 answer 501 after real authorization.
