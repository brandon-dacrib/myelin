# Where this is, and what comes next

Written 2026-09-20 by the integration lead. `PLAN.md` is the design and rarely changes; this file is the resume point and changes every session. `docs/status/dashboard.md` is the generated measurement; per-track detail lives in `docs/status/NN-*.md`.

The project is **Myelin**, and it is public: <https://github.com/brandon-dacrib/myelin>. The crates still carry the `hs-` prefix from before it had a name.

## The state of things

**A real client works.** Element Web — the actual browser client most Matrix users run — signs in against this server, shows a room list, sends and receives messages live between two independent sessions, propagates a display-name change to an already-open tab, and scrolls back through history. Screenshots in `docs/design/screenshots/`, reproduction in `web/element-testing/README.md`. That was the project's stated definition of success from day one, and it is met with one loud exception (item 1 below).

**Complement, `csapi`: 191 of 296 assertions pass** (53 of 106 top-level), up from 148/293 and 125/293 on the two runs before it.

**Complement, federation package: 52 of 212 assertions** (6 of 88 top-level). That is now a real measurement rather than a TLS wall: `federation.custom_ca_certificates` works, and the harness no longer disables certificate verification — it trusts Complement's CA the way a deployment would trust a private one.

**Spec coverage: 138 of 235 routes (58.7%)** — client-server 108/166, server-server 30/36. Generated from the manifest the binary emits, so it cannot overclaim. Registered still is not the same as working.

**It ships.** CI is green on amd64 and arm64. CD publishes a multi-arch image to `ghcr.io/brandon-dacrib/myelin`, and refuses to publish one that has not booted and answered `/health/live` and `/_matrix/client/versions` on both architectures. Binaries, the Helm chart and a GitHub release are wired to `v*` tags and have not been exercised — tagging `v0.0.1` is how you find out whether they work.

Reproduce the conformance numbers:

```
./tests/complement/build.sh complement-hs-reimplement:dev
cd refs/complement
COMPLEMENT_BASE_IMAGE=complement-hs-reimplement:dev go test -v -timeout 30m ./tests/csapi/...
COMPLEMENT_BASE_IMAGE=complement-hs-reimplement:dev go test -v -timeout 30m -skip 'TestInboundCanReturnMissingEvents' ./tests
```

A cold image build is ~4 minutes idle, up to 19 under load; each suite run is ~13-15 minutes. The `-skip` is not optional yet — see item 4.

## What to do next, in order

### 1. Element cannot create a room — one bug, one afternoon

`POST /createRoom` *replaces* the default power levels when a client sends `power_level_content_override`, instead of merging over them. The creator loses their own power-100 grant, the next bootstrap event is auth-rejected, and the request fails. Real Element sends that field on **every** room it creates, so creating a room from the UI fails unconditionally, every preset, every time.

`crates/hs-room/src/actor.rs`, `RoomActor::create_room`, at the `power_level_content_override.clone().unwrap_or_else(...)`. It is backwards from the spec's own wording for the field. Everything else in Element works; this is the one thing standing between "it works" and "it is usable", and it is small.

Two more from the same session, both cheap and both visible to a user:

- **`unsigned.prev_content` is never set anywhere** (`crates/hs-room/src/routes/render.rs::client_event_json`). Element renders a display-name change as "Alice joined the room", because without the previous content it cannot tell the two apart.
- **`GET /account/3pid` does not exist** (track 07). Element's Settings page shows a visible error.

### 2. Make it fun to install and administer

This is the product priority, and the server is now far enough along to deserve it. Two halves:

**The admin web interface.** It reads real data and degrades honestly against the 118 operations that still answer `501`, but it is a viewer. It should become the way an operator *runs* this server: configuration through the UI rather than YAML, users and rooms managed without curl, the things an operator does weekly reachable in two clicks. Every competing homeserver is administered by hand-editing config and running Python scripts — Synapse ships no admin UI at all. This is the differentiator.

Work backwards from an operator's day and let that drive which admin API operations to implement next, rather than working down the OpenAPI document in order.

**First run.** Getting from nothing to a working server should be pleasant. Today it is: generate a config, edit YAML, generate a signing key, run a binary, register a user with a shared secret you had to put in the config first. A first-run flow — a single command that produces a working server and hands you a URL and an admin login — is squarely in the spirit of this priority, and the image, chart and CD pipeline that now exist are the foundation for it.

### 3. Federation: stop crashing the suite, then finish the join

- **`/get_missing_events` returns the wrong event first**, and it segfaults Complement's own Go binary (it dereferences a state key unconditionally where our response has an event without one). The first federation run crashed 21 tests in and silently discarded everything after, which is why the reproduction above needs `-skip`. Fixing it removes the skip and probably moves the number more than the crash suggests. Track 06's P0.
- **Restricted and knock-restricted joins fail across the board** — ten top-level tests, all `M_FORBIDDEN: invalid join_authorised_via_users_server`. Tracks 06 and 04.
- **Two-way federation needs a room bootstrap API.** Two instances of this server complete a real join handshake over TLS with a private CA, verified end to end — but only one way, because `hs-room` can create a new room or extend one it already has, and has no way to build a room from a join's verified state snapshot. Specified in `docs/rfcs/0015-outbound-join-needs-a-room-bootstrap-api.md`; the script that demonstrates the gap is `crates/hs-federation/scripts/two-server-federation.sh`.

### 4. The rest of the Complement triage

Full detail, by owning track, at the top of `docs/status/14-test-and-conformance.md`:

- **Track 02**: room-v12 additional-creator validation answers 403 where the spec wants 400; the create event's `room_id` is missing on `/state`, `/messages`, `/event` and `/context`.
- **Track 09**: federation-fetched media fails outright — thumbnails, content, filenames. Implemented, but broken for remote peers.
- **Tracks 08 and 06**: device-list and to-device delivery over federation time out at full length rather than failing fast, which reads like a delivery gap rather than a validation one.
- **No clear owner**: some error responses are not JSON — an empty-body fallback on unknown endpoints, plain-text extractor rejections. Nothing claims the base router's fallback handling; it belongs with whoever owns the shared HTTP layer.

### 5. Housekeeping worth doing deliberately

- **Rename the crates** from `hs-` to the project's own prefix. Mechanical across twenty-six crates, and best done when nothing else is in flight.
- **Tag `v0.0.1`** to exercise the untested half of CD: binaries for three targets, the Helm chart as an OCI artifact, and a GitHub release.
- **`cd.yml` documents an `edge` tag it does not produce** — pushes to `main` tag the image `main`. Fix the tag or the table; they disagree.
- **`web`'s unit tests and lint have never run on this machine.** Vitest workers time out; it has failed the same way in four separate sessions under load. The repository has since moved off iCloud, which fixed every other pathological slowdown here — try again before assuming it is unfixable.
- **Receipts and presence are in-memory**, so a restart forgets read state and presence. Postgres ignores `pool_size`, refuses `tls`, and hardcodes the `public` schema. `/createRoom` is not shard-gated. UIA on `/keys/device_signing/upload` needs a coordinated change with the loadgen scenario that bootstraps cross-signing without auth data.

## Known gaps, honestly held

| Gap | Where | Consequence |
|---|---|---|
| `/createRoom` drops the creator's power level | `hs-room` | Element cannot create a room at all |
| `unsigned.prev_content` never set | `hs-room` | clients cannot tell a rename from a join |
| `/account/3pid` unimplemented | `hs-auth` | visible error in Element's settings |
| `/get_missing_events` ordering crashes Complement | `hs-federation` | the federation suite cannot run unskipped |
| Restricted joins rejected | `hs-federation`, `hs-room` | ten conformance tests, a common room type |
| No room bootstrap from a join | `hs-room` | federation completes one way only |
| Federation media fetch broken | `hs-media` | remote avatars and attachments fail |
| `/search` unimplemented | `hs-room` | needs a cross-room index the actor model has no place for |
| Admin UI is read-mostly | `web` | the stated product priority is not met yet |
| Receipts and presence in memory | `hs-user` | a restart forgets read state |
| Postgres `tls`/`pool_size`/schema | `hs-kv`, `hs-cli` | encrypt in front of the database for now |
| `/createRoom` not shard-gated | `hs-cli` | first actor may be built on a non-owner |
| web unit tests never run here | `web` | unverified, not passing |
| Sytest never run | `tests/sytest` | CPAN dependencies absent |
| `cargo fuzz` never executed | `fuzz/` | no nightly toolchain |

## Conventions worth keeping

- **Verify by running.** Every claim above was checked against the binary, a real client, or a conformance suite. Reports that were taken on trust have been wrong repeatedly — a "clean typecheck" with two errors, a gap reported open that had been closed hours earlier, a receipts bug that was a stale binary.
- **Point real things at it.** Every serious bug this project has found came from Complement, a real SDK, or a real browser — never from its own tests. The signing bug had passed every test for weeks because the signer and the verifier shared the same wrong assumption.
- **Run the gates CI runs.** `cargo test -p <crate>` cannot see what `--workspace --all-targets` sees: feature unification, cross-crate visibility, dead code. Seven consecutive red CI runs came from exactly that gap.
- **Wait for conditions, not durations.** Five cluster tests advanced a virtual clock a fixed number of rounds and asserted a background task had kept up. They passed locally every time and failed on CI, including one that needed *real* time because the work finishes on a blocking thread.
- **Keep the repository off iCloud.** `git status` took 600 seconds there and takes 0.24 here.
- **Registered is not working.** 24 of 142 admin operations are genuinely served; the rest answer 501, or 503 when a seam exists but nothing implements it.
