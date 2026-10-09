# 06 Federation: status

## 2026-10-09 (branch `agent/federation-95`): the suites re-measured today, the leave-then-rejoin race closed, what is left named

README.md rated Federation at ~60% on numbers from 2026-10-01/02 (Sytest's federation group
78 of 105, Complement's federation package 50 of 90) that had been overtaken by waves 1-3
(status 14 session 11: 103 of 105 and 88 of 90 on 2026-10-05) and never carried back into the
table. This session re-measured both suites on an image of today's `main`, fixed the one race
the interop run had left, and recorded the evidence for every test still failing. Crates:
`hs-federation` (the sender's delivery barrier), `hs-cli` (the forwarder's position, the hook,
the wiring, one real-binary test), `tests/federation-synapse/run.sh` (a two-replica mode).
No OpenAPI change, no decision or RFC number taken (no interface another track consumes changed:
`OutboundFederation::start` keeps its signature, `start_with_position` is added beside it).

**Measured today, before any change of this branch** (images `complement-hs-reimplement:federation-95`
and `myelin-sytest:federation-95` of `main` at `00fe0c01`, the merge gate running beside them):

| Suite | README said (2026-10-01/02) | Today | Left, by name |
|---|---|---|---|
| Sytest, federation group | 78 / 105 | **103 / 105** (98%) | "Can invite unbound 3pid over federation with users from both servers" (a race in the test), "If a device list update goes missing, the server resyncs on the next one" (a race in the test; track 08) |
| Sytest, per file | auth 16/20, send_join 8/9, invites 9/10, state 7/10, backfill 3/5, get_missing_events 2/3, federation API 9/14, device keys 4/9, query API 1/5, public rooms 0/1 | auth **19/20**, send_join **9/9**, invites **10/10**, state **10/10**, backfill **5/5**, get_missing_events **3/3**, federation API **14/14**, device keys **8/9**, query API **5/5**, public rooms **1/1**; make_join 3/3, send_leave 1/1, room versions 7/7, key API 6/6, send-to-device 2/2 unchanged | the two above (one in auth, one in device keys) |
| Sytest, whole suite | 548 / 772 | **754 / 772** (3 fail, 15 skip; 99.6% of tests run), client-server 523/534, application services 22/22, non-spec 102/107 | the two above and "The only membership state included in a gapped incremental sync is for senders in the timeline" (on Synapse's own blacklist) |
| Complement, federation package (`./tests`) | 225 / 314 assertions, 50 / 90 tests | **316 / 317 assertions, 89 / 90 top-level** (1 skipped: `TestSendJoinPartialStateResponse`, faster joins), 737 s | `TestDeviceListsUpdateOverFederationOnRoomJoin` (skipped by Synapse and Dendrite; below) |
| Complement, restricted rooms, invites and knocks | 17 / 18 | **18 / 18** | -- |

Against the committed baseline (run 13, 2026-10-05): FAIL -> PASS `TestUnbanViaInvite` (the
`sync-wakes` fix, `a1fa71a6`), no PASS -> FAIL. Per-test results: `docs/status/sytest/2026-10-09-federation-95-{results,summary,are-we-synapse-yet}.txt`
(Sytest `747315856`), `docs/status/complement-federation-results.txt` (run 14). The numbers did
not move today because nothing in this branch is aimed at a suite test: every federation test
that still fails is a race in the test or a test no server passes, and the evidence is below.

**Fixed: a leave followed at once by a rejoin let the rejoin through.** The interop run of this
morning had left a race: a Myelin user leaving an invite-only Synapse room and rejoining it in
the next request was sometimes let back in without a new invite. Found and fixed, and it is
federation-side, not the room actor's. After the leave nobody of this server is in the room, so
the rejoin goes out as `make_join` to the inviting server (Synapse's `_should_perform_remote_join`
rule, the same here); the leave is still in the outbound queue (the sender learns of local
events off the registry's broadcast stream, after `/leave` has answered), the `make_join`
overtakes it, the other server finds the user joined and hands back a join-to-join template, and
`send_join` is accepted. Reproduced on two copies of this server: one round in three
(`crates/hs-cli/tests/federation_membership.rs`,
`a_leave_then_an_immediate_rejoin_of_an_invite_only_room_is_refused_on_both_servers`, three
rounds of leave, rejoin, re-invite, join; it failed at round 3 before the fix and passed 15
rounds in a row after). The fix is a delivery barrier before any membership handshake
(`make_join`, `make_leave`, `make_knock`) for a room this server holds with its state: first the
outbound forwarder has to have handed the registry's global stream up to the request's position
to the sender (`hs_cli::federation_sender::ForwardedPosition`, the same `global_seq` the session
hub uses for `/sync`'s read-your-writes; 2 s), then the destination has to have accepted
everything queued for it (`FederationSender::wait_until_delivered`, 3 s, polled every 10 ms).
Both bounded: a destination that is down keeps its queue, the wait ends and the handshake goes
ahead (and fails against the same server); a replica that does not send for the destination
sees nothing pending. A room held as a shell (an invite, a knock) or not held at all waits for
nothing, so Sytest and Complement joins into fresh rooms cost nothing. Logged at `debug` when a
wait starts, `info` with the wait when it ends, `warn` when it gives up (the operator's trail
for "why was that join slow"). Unit test `sender::tests::the_delivery_barrier_...` covers the
three outcomes. Synapse has the same race in principle (its sender is woken by the notifier
after persistence, too); it is slower, so it rarely loses it.

**Recorded, not worked around.**

- Sytest **"Can invite unbound 3pid over federation with users from both servers"**
  (`30rooms/12thirdpartyinvite.pl`): a race in the test, confirmed again against Synapse's code.
  The joiner is a `remote_user_fixture` that has never called `GET /events`, so its first
  `await_event_for` sends no `from` and Synapse's `Notifier.get_events_for` (and `hs-user`'s
  route) starts at the current token: an `m.room.third_party_invite` that reached the second
  server before that request is never seen. It reaches it 1 ms after the first server stored it
  here; Synapse passes because its `/send` handling is slower than Sytest's next request.
- Sytest **"If a device list update goes missing, the server resyncs on the next one"**
  (`50federation/40devicelists.pl`, track 08): racy by construction (status 08, 2026-10-05): the
  user's first `sync_until_user_in_device_list` is an initial sync, which carries no
  `device_lists`, started after the first update was handled.
- Complement **`TestDeviceListsUpdateOverFederationOnRoomJoin`**: Complement skips it for
  Synapse and Dendrite (`runtime.SkipIf`), because no server sends an `m.device_list_update` on
  a join: Synapse's PR 16875, which did, was closed on 2024-02-02 ("for this to be useful, it
  requires MSC4081 to be included as well ... this PR just adds more network traffic without
  concretely fixing anything"); the remote server fetches the list over `/user/devices` when
  it first needs it, which this server does (status 08). Not implemented, by the same reasoning.
  It is not on `tests/complement/blacklist.txt` (track 14's) so the measure keeps counting it.

**The interop harness against a two-replica Myelin** (`REPLICAS=2 tests/federation-synapse/run.sh`,
new this session: a `fed-synapse-pg` PostgreSQL container, two `hs serve` replicas sharing the
server name, the signing key and the database with the mesh on 8459/8460, the TLS front
round-robinning Synapse's requests over both, five extra checks through the second replica). It
found one real cluster bug and one harness race, and ends at **51 / 51 PASS (the 46 checks plus five through the second replica), with the single-server run rerun at 46 / 46 on the same binary**:

- **Fixed: a PDU queued for a destination another replica sends for waited for a rescan, or
  forever.** The first message after a join made on replica 1 took **62 s** to reach Synapse:
  replica 2 sends for Synapse's destination, replica 1 wrote the row to the shared outbound
  store, and replica 2 found it only when something else made it start a worker for that
  destination (an idle worker rescans every 10 s; a destination with no worker yet is never
  looked at until `resume`). Now the replica that queues tells the owner over the mesh
  (`EduForwarder::wake_sender_for`, `hs_cli::edu_forward::WAKE_ROUTE` beside the forwarded
  EDUs) and the owner looks at the store at once (`FederationSender::wake_destination`: a
  worker that exists is woken, one that does not is started with the store's backlog). Same
  message, same topology: **21 ms**. Best effort: a wake that fails is logged and the rescan
  still finds the row; counted in `hs_federation_pdu_wakes_total{outcome}`. Unit test
  `sender::tests::a_wake_sends_what_another_replica_queued_with_or_without_a_worker_here`
  (no rescan interval at all, so only the wake can deliver).
- **A harness race, fixed in the harness:** "the ban did not reach Synapse". The ban *was*
  persisted by Synapse 16 ms after the harness's first poll of the banned user's initial
  `/sync`, and Synapse answers an identical initial `/sync` from its response cache for two
  minutes, so every later poll got the pre-ban answer. With one server the ban is sent before
  the first poll; with two, the request is forwarded to the room's owner first. The check now
  polls an incremental `/sync` from a token taken before the ban, like the receipt and
  to-device checks.
- **Seen, recorded, not this track's:** a `PUT /profile/displayname` on replica 1 refreshes
  the member event in every room the user is in, and for a room replica 2 owns it is fenced
  (`hs_room::routes::profile`, "profile refresh failed for one room ... fenced") -- but loading
  that room on the non-owner republished its latest event on replica 1's stream, and the
  forwarder queued the ban a second time (Synapse deduplicated it; "handling received PDU" twice
  for one event id, 33 s apart). A duplicate send is harmless; the non-owner reload is RFC
  0018's subject (track 04/03). Also `postgres:17`'s first start answers `pg_isready` on its
  socket from a temporary server; the harness asks over TCP.

**Verified.**

- Sytest whole suite on `myelin-sytest:federation-95` (`main` at `00fe0c01`): 754 / 772, federation
  group 103 / 105 (`target/sytest/before`, copied to `docs/status/sytest/2026-10-09-federation-95-*`).
- Complement `./tests` on `complement-hs-reimplement:federation-95`, patches applied, under
  `tests/complement/lock.sh`: 316 / 317 assertions, 89 / 90 top-level, 737 s
  (`docs/status/complement-federation-results.txt`, run 14).
- `cargo test -p hs-federation --lib`: 233 (two new: `sender::tests::the_delivery_barrier_...`,
  `sender::tests::a_wake_sends_what_another_replica_queued_with_or_without_a_worker_here`).
- `cargo test -p hs-cli --test federation_membership` (13, one new) `--test federation_sender`
  (5), `--lib edu_forward`; the new membership test 15 rounds in a row.
- `cargo fmt --all --check`; `cargo clippy -p hs-federation -p hs-cli --all-targets -- -D
  warnings`: clean.
- `tests/federation-synapse/run.sh` against Synapse 1.162.0 with the branch's binary: **46 / 46**
  single-server; `REPLICAS=2` **51 / 51** (after the wake fix and the harness's ban check; 49 / 51
  and 50 / 51 before, as above). Every `fed-synapse*` container and both `hs` processes removed
  on exit.

**Interfaces.** `FederationSender::wait_until_delivered` / `DeliveryWait` / `DELIVERY_POLL_INTERVAL`,
`FederationSender::wake_destination`, `EduForwarder::wake_sender_for` (a defaulted trait method,
so other implementors compile unchanged), `hs_federation::metrics::{record_pdu_wake, pdu_wakes}`
(`hs_federation_pdu_wakes_total{outcome}`); `hs_cli::federation_sender::{ForwardedPosition,
OutboundFederation::start_with_position}`, `hs_cli::remote_join::DeliveryBarrier`,
`FederationRemoteJoin::with_delivery_barrier`, `hs_cli::edu_forward::{WAKE_ROUTE, WakeRequest}`.
Nothing another track consumes changed shape.

**Left.**

- `TestDeviceListsUpdateOverFederationOnRoomJoin`, the two Sytest races and the one Synapse
  blacklists, as above: nothing to fix on the server.
- A real-binary cluster test of the wake (`crates/hs-cli/tests/cluster_edus.rs` has the two-replica
  harness and a PostgreSQL skip) would pin the 21 ms; the sender's unit test and the interop run
  are what proves it now.
- `hs serve` terminating TLS itself (removing the nginx front) is hs-cli/track 12; putting the
  interop run, single and two-replica, in a CI leg with Docker is track 12's.

## 2026-10-09 (branch `agent/federation`): the federation milestone -- Myelin federates with a real Synapse, end to end

Myelin had never been pointed at a Synapse: every federation number was Complement, Sytest or two
copies of Myelin. This session ran the interop harness (`tests/federation-synapse/run.sh`, written
2026-10-02 but never run) against a **real Synapse 1.162.0** in Docker, found and fixed the one bug
that stopped it, and left the whole story green and rerunnable. Crates: `hs-federation`,
`hs-cli` (wiring and the harness). No OpenAPI change, no decision or RFC number taken.

**What was verified against Synapse, step by step** (46 PASS, 0 FAIL;
`tests/federation-synapse/run.sh`, `results.tsv`):

- **Keys and discovery**: both servers' `GET /_matrix/key/v2/server` (Myelin plaintext, Synapse
  over TLS); Myelin fetches Synapse's keys through its own notary (`/_matrix/key/v2/query/{s}`);
  and, once the two have federated, Synapse serves Myelin's keys through *its* notary. Server names
  carry ports, so both connect directly with no `.well-known`/SRV; trust is a private CA on both
  sides (`federation.custom_ca_certificates` / Synapse's `federation_custom_ca_list`).
- **Joins both ways**: a Myelin user joins a public Synapse room by alias and a Synapse user joins
  a public Myelin room by alias; messages flow both ways; pre-join history is backfilled onto the
  joiner; and `/joined_members` agree on both sides.
- **Membership**: invites both ways accepted; a Myelin user joins, leaves and **rejoins** a public
  Synapse room (each transition confirmed in Synapse's resolved state); Synapse kicks the Myelin
  user (seen in Myelin's state); Myelin bans a Synapse user (seen in the banned user's leave sync
  on Synapse).
- **Redaction**: Synapse redacts its own message in a Myelin room; Myelin applies it.
- **EDUs**: Myelin's typing, read receipt and to-device message all reach Synapse's `/sync`
  (receipts and to-device delivered over an incremental sync, as the spec requires).
- **Media**: a file uploaded on each side downloads through the other's
  `/_matrix/client/v1/media/download` (authenticated media over federation, both directions).
- **Directory and queries**: `/publicRooms?server=` both ways; `/profile` of the remote user both
  ways; `/directory/room` of the remote alias both ways.
- **Device keys over federation** (track 08's): Synapse's `/keys/query` of a Myelin user returns
  the uploaded device key.
- **Room versions**: a room of version **10, 11 and 12** joined in **both** directions, and a
  **restricted** join into a Synapse room authorised by Synapse via a shared space.

**The bug that mattered, fixed: a server could not verify events it signed itself.** When Myelin
verifies the events in a `send_join`, `invite` or `make_join` response, the response carries, among
the room's state, the membership events Myelin's *own* user signed. Verifying those asked the
remote-key cache for Myelin's own key (`fed-synapse-myelin:8449/ed25519:a_IJcZAB`), which it tried
to **fetch over federation from Myelin itself** -- a request Myelin cannot answer, so the lookup
failed ("could not fetch keys for server `<self>`") and the event was dropped from the join state.
Against two copies of Myelin this never surfaced (each only ever verified the *other's* events; its
own join was built locally and accepted without a signature re-check). Against Synapse it broke
invites out, restricted joins, and -- because the joining user's own membership was dropped from
the stored state -- a later rejoin and every membership built on it. Fix (`hs-federation`
`keys.rs`): the `RemoteKeyCache` holds this server's own verify keys
(`RemoteKeyCache::seed_own_keys`, seeded at startup in `hs-cli` `build_mount` and `run_join_room`),
consulted before any cache lookup or fetch, trusted at any timestamp. An own signature now verifies
against the key already in memory, never a fetch. Test: `keys::tests::own_keys_verify_without_a_fetch`.

**How it was run.** `tests/federation-synapse/run.sh` builds `hs`, generates a private CA and leaf
certs, starts Synapse (server name `127.0.0.1:8448`, TLS federation on 8448, client on 8408) and an
nginx TLS front for Myelin (`fed-synapse-myelin:8449` on the `fed-synapse` network, proxying to the
host's plaintext `hs`), then drives every step above through both client APIs and writes
`results.tsv`. It exits non-zero on any FAIL and clean-skips without Docker/curl/jq/openssl, so a CI
leg without a daemon stays green. The image is pulled from the Docker Hub mirror
(`mirror.gcr.io/matrixdotorg/synapse:latest`): an agent session cannot pull from Docker Hub or
ghcr.io (the macOS keychain credential helper), and the mirror needs no credentials.

**Harness fixes this session** (so the run is correct and reruns cleanly): the image comes from the
mirror with a credential-helper-free Docker config and the OrbStack socket; Synapse's
`room_list_publication_rules` allows publishing to the directory (its default has forbidden it
since 1.126, so `/publicRooms` had nothing to list); the notary check against Synapse hits its TLS
federation listener (not the client API) and runs after the servers have federated (a fresh
notary's cache is empty); leave/rejoin/kick is tested on a **public** room (rejoining an invite-only
room after leaving correctly needs a fresh invite -- the old harness tested it on a private room and
the racy "rejoin PASS" was Myelin briefly letting a stale-state join through); receipts and
to-device are read over an incremental (since-based) `/sync`; the ban is confirmed in the banned
user's leave sync (a banned user cannot read the room's state endpoint); the key upload uses the
session's real device id; and `results.tsv` is created after its directory exists.

**Also done** (next-steps item 3 leftovers): `docs/status/routes.json` regenerated with
`hs routes-manifest -o docs/status/routes.json` (it was stale since 2026-10-05; `federationVersion`
and `federationOpenIdUserinfo` now show `auth: none`, matching the merged `fed-version` work). The
other item-3 entries were already on main: `TestCorruptedAuthChain` (wave 3, 2026-10-05), the v12
create event's `room_id` (`hs-room` `routes/render.rs::create_event_room_id`, test
`a_version_12_create_event_is_shown_with_its_room_id`), and alias queries over federation asking the
bridge (wave 3).

**Verified.**

- `tests/federation-synapse/run.sh` against Synapse 1.162.0: **46/46 PASS**, exit 0, every
  `fed-synapse*` container and the host `hs` removed on exit.
- `cargo test -p hs-federation`: 231 pass (new `keys::tests::own_keys_verify_without_a_fetch`).
- `cargo test -p hs-cli --test federation_two_servers --test federation_membership --test
  federation_writes --test federation_room_versions`: all pass against the real binary.
- `cargo fmt --all --check`; `cargo clippy -p hs-federation -p hs-cli --all-targets -- -D warnings`:
  clean.

**Left.** Device keys over federation and the device-list `stopped_server` cases are track 08's.
A same-user back-to-back leave-then-rejoin of an invite-only room can briefly let the rejoin through
locally before the leave is applied (a narrow race in this server's own membership state; the
harness no longer exercises it, since rejoining an invite-only room after leaving is forbidden
anyway). Putting the interop run in a CI leg that has Docker is track 12's; `hs serve` terminating
TLS itself (removing the nginx front) is hs-cli/track 12. The run is single-pair, not clustered.

## 2026-10-09 (branch `agent/fed-version`): `GET /_matrix/federation/v1/version` answers an unsigned request

The live demo (image `a6f02c48`) answered an unsigned `GET /_matrix/federation/v1/version`
`401 M_UNAUTHORIZED` ("signature verification failed"): the route was registered inside the
`X-Matrix` layer. The spec (`server-server/version.yaml`) gives it no `security` block, Synapse
answers it unsigned, and federation testers and other servers call it unsigned. Crates:
`hs-federation`, `hs-cli` (tests only). No OpenAPI change, no decision or RFC number taken.

- **`/version` is its own router** (`crates/hs-federation/src/transport/version.rs`), merged by
  `transport::router` *beside* the `X-Matrix` layer (`Router::layer` wraps only the routes present
  when applied), and marked `AuthKind::None` in the manifest. The `Authorization` header is not
  read, so a signed request (this server's own client signs it) still gets `200`, and so does one
  whose signature would not verify. Logged at `debug`; counted by the HTTP layer like every route.
- **Served only with federation on** (`404` with `federation.enabled: false`), following Synapse,
  which serves `/version` only from its `federation` listener resource. `/openid/userinfo` stays
  served either way.
- **Audit of every route under `/_matrix/federation` and `/_matrix/key` against the spec's
  `security` blocks**: the spec's unsigned operations are `/version`, `/openid/userinfo`,
  `PUT /3pid/onbind` and the key server (`/_matrix/key/v2/server`, `GET`/`POST /query`). Only
  `/version` was wrong; the other three were already outside the layer (`openid.rs`, `hs-cli`'s
  `onbind_router`, `key_server.rs`). `timestamp_to_event` says `accessToken` in the spec, which is
  a spec slip (it is a federation endpoint; Synapse authenticates it as one): kept signed.
- **Tests**: `transport::tests::version_answers_an_unsigned_request`,
  `version_answers_a_signed_request_and_ignores_a_bad_signature` (replacing
  `version_endpoint_works_when_properly_signed`), `the_only_unsigned_federation_route_is_version`
  (replacing `the_federation_router_has_no_unsigned_route`; v2 routes all signed);
  `every_route_is_behind_the_x_matrix_layer` still covers every signed route. Real binary:
  `crates/hs-cli/tests/federation_version.rs` (`unsigned_federation_version_is_answered`: `200`,
  `server.name == "hs"`, unsigned `/publicRooms` still `401`;
  `federation_version_is_not_served_with_federation_off`: `404`). `openid_userinfo.rs` now probes
  `/publicRooms` for "a signed route is mounted" instead of `/version`.
- **Verified**: `cargo test -p hs-federation` (230 passed), `cargo test -p hs-cli --test
  federation_version --test openid_userinfo`, per-crate clippy clean.
- **Left**: `docs/status/routes.json` still lists `federationVersion` (and
  `federationOpenIdUserinfo`) as `auth: matrix`; it is generated and stale since 2026-10-05,
  `hs routes-manifest -o docs/status/routes.json` refreshes it. The demo needs a redeploy to
  pick this up.

## 2026-10-08 (branch `agent/fed-cluster`): a room nobody here is in, the durable-EDU bound as a setting, the announcer's place per shard, `/members?at=` 404, `/openid/userinfo` with federation off

Wave 4's leftovers for tracks 06, 04 and 07. Crates: `hs-federation`, `hs-room`, `hs-config`
(federation section), `hs-cli` (wiring and tests), `web/` (the schema fixture, the mock
configuration, one unit test). No OpenAPI change, no decision or RFC number taken.

**1. A PDU pushed for a room no user of this server is joined to is ignored** (`hs-federation`
`inbound`, `hs-cli` `RegistryWriteSink::accept_pushed_event`, `hs-room` `RoomRegistry`), as
Synapse's `on_receive_pdu` ignores it ("Ignoring PDU ... as we're not in the room"). `/send`'s
first attempt at a PDU goes through the new `RoomWriteSink::accept_pushed_event` (default:
`accept_verified_event`); the sink answers the new `WriteOutcome::NotInRoom` when no local user
is joined, and `/send` answers `{}`, logs it at `info` and counts it in
`hs_federation_pdus_dropped_total{reason="not_in_room"}`. Nothing is backfilled or fetched for
it. Two exceptions, both kept: the end of a local user's invite or knock (the inviter's leave
citing the invite, a resident refusing a knock: `out_of_room_ending`, recorded out of band as
before; Synapse records a rescinded invite the same way), and a join of the room through another
server under way in this process (`RoomRegistry::remote_join_started`, a guard
`FederationRemoteJoin::join` holds; `remote_join_in_progress`), whose resident already sends
the room's events before the join is held here (Synapse queues them). The invite-out-of-band
paths `fed-wave3` relies on are unchanged: an invite still arrives over `PUT /invite`, and a
local user who is joined still gets everything over `/send`. A PDU for a room this server has
never held is still answered `{"error": "unknown room"}` and is now logged and counted
(`reason="unknown_room"`). Before, a PDU in a room every local user had left was placed in the
graph (or, citing what was not held, backfilled for).

**2. `federation.max_queued_durable_edus_per_destination`** (`hs-config`, hot; `hs-federation`
`FederationSender::set_max_queued_durable_edus_per_destination`; `hs-cli` `serve`). RFC 0023's
bound (10 000 by default, at least 1) is a setting: the sender reads it for each durable EDU it
queues, and the `federation` applier puts a change in force at once, logged ("the bound on each
server's waiting to-device and device-list updates is now in force") and counted in
`hs_config_settings_applied_total`. Synapse keeps these without a bound and has no equivalent.
`docs/config.md` and `web/src/test/fixtures/hs-config-schema.json` regenerated; the mock
configuration carries it; the Configuration page's federation section shows it with "Applies on
save" (its legend counts six such settings now).

**3. The device-list announcer keeps its place per federation shard** (`hs-cli` `edus`;
`hs-federation` `OutboundStore::set_cursors`, `FederationSender::store_positions`). Each shard's
place (`device_list_announcer/federation/{n}`, `edus::announcer_position`) is moved on only by
the replica that owns the shard, after what it read is in the sender's durable store. A shard a
replica takes on -- at start, or from a replica that died or left -- is read from its own place,
and the changes since go to its destinations as whole device lists, logged ("announcing the
device-list changes federation shards taken on were not told") and counted in
`hs_federation_device_list_catch_ups_total`. Shards owned at the previous look move together and
are told differences, as before. A shard with no place yet starts at the old whole-server place
(`device_list_announcer`, so an upgraded single node resumes where it was) or now. Before, the
one place for the whole cluster was the furthest any replica read: a replica that died holding
a shard's lease left the change made meanwhile unannounced to that shard's destinations, since
the other replica read past it while it could not send there.

**4. `GET /rooms/{roomId}/members?at=` a token no event precedes is `404 M_NOT_FOUND`**
(`hs-room` `routes::query::get_members`), as Synapse answers ("Can't find event for token"); it
answered the current members.

**5. `/_matrix/federation/v1/openid/userinfo` is served whether or not federation is enabled**
(`hs-federation` `transport::openid`, a router of its own on the new `OpenIdUserinfoSource`;
`hs-cli` `federation::AuthOpenIdUserinfo`, mounted in `build_router` beside the client routes).
It was inside the federation router (beside the `X-Matrix` layer since `auth-leftovers`), so
`federation.enabled: false` stopped serving it; an integration manager checks a token there
whether or not the user's server federates (Synapse's `openid` resource). The federation router
now has no unsigned route; `FederationQuerySource::openid_userinfo` is gone.

**Also** (coordinator): `hs-room`'s `tests/scenario.rs`
`profile_propagates_into_join_invite_and_knock_membership_content` asserted the room's *state*
still had bob's old name after `PUT /profile`, which re-stamps the member event of every joined
room in the background; it read the new name under load. It now reads the join event itself by
its ID, which no profile change edits.

**Verified.**

- Unit tests: `hs-federation` 228 (new `inbound::tests::{a_pdu_for_a_room_this_server_is_not_in_is_ignored_and_counted,
  a_pdu_for_an_unknown_room_is_counted}`, `transport::openid::tests::openid_userinfo_answers_an_unsigned_request`,
  `transport::tests::the_federation_router_has_no_unsigned_route`,
  `outbound_store::tests::cursors_are_stored_one_at_a_time_and_together`, the durable-EDU bound
  test extended to the live bound); `hs-room` (`registry::tests::a_remote_join_is_under_way_while_its_guard_is_held`,
  `routes::query::tests::members_at_a_token_before_every_event_are_not_found`); `hs-config`
  (`a_zero_durable_edu_bound_is_rejected`, the web fixture test); `hs-cli` lib
  (`edus::tests::{a_look_reads_steady_shards_from_their_place_and_taken_ones_from_theirs,
  each_shard_has_its_own_place}`). The whole of `cargo test -p` for `hs-federation`, `hs-room`,
  `hs-config` and `hs-cli` (94 result lines, none failed; `HS_CLUSTER_TEST_POSTGRES_DSN` set).
- Real `hs serve`, new: `federation_state_fallback::a_pdu_for_a_room_nobody_here_is_in_is_ignored`
  (bob's message taken while alice is in; after she leaves, his next one is `{}`, not held,
  counted, nothing fetched); `openid_userinfo` (both `federation.enabled` settings; with it off
  `/version` is still `404`); `config_hot::the_durable_edu_bound_applies_without_a_restart` (the
  real binary: `hot` in the schema, `PATCH` reloads `federation`, the log line, the
  applied counter, `0` refused); `members_at` (Complement's `TestGetRoomMembersAtPoint` step for
  step, and the `404`); `cluster_device_lists` (two replicas on PostgreSQL, one the real binary,
  `SIGKILL`ed holding B's federation shard; fails on the old announcer with "bob was never told
  alice's devices changed", passes in 16 s). Unchanged and passing: `federation_membership`,
  `federation_two_servers`, `third_party_invites_federation`, `guest_access_federation`,
  `federation_writes`, `push_federated_invite`, `auth_sessions`, `cluster_edus`,
  `federation_edus`, `federation_restart`, `e2e`.
- `cargo fmt --all --check`; `cargo clippy -p hs-federation -p hs-room -p hs-config -p hs-cli
  --all-targets -- -D warnings`: clean. `web/`: `npm run check` and `npm run test:e2e` (68)
  pass.
- **Complement** (image `complement-hs-fedcluster:dev`: `complement-hs-main:w3` with this
  branch's bookworm `hs`; patches applied, under the shared lock): the whole federation
  `./tests/` package fails only `TestDeviceListsUpdateOverFederationOnRoomJoin`, failing in the
  measured baseline too (Synapse skips it); csapi `TestGetRoomMembersAtPoint`,
  `TestGetRoomMembers`, `TestGetFilteredRoomMembers`, `TestRoomMembers` pass.
- **Sytest** (`SYTEST_HS_BINARY` of this branch): `50federation/{30room-join,31room-send,
  35room-invite,36state,40devicelists,52soft-fail}.pl` and `30rooms/{12thirdpartyinvite,
  13guestaccess}.pl`: 90 pass, 2 fail, both failing before this branch ("Can invite unbound 3pid
  over federation with users from both servers", the test's race, item 6 of 2026-10-05; "If a
  device list update goes missing, the server resyncs on the next one", inbound, failing since
  wave 2).

Found on `main`, not changed: a federation `send_join` that reaches a replica not owning the
room's shard is refused `501 M_HS_INBOUND_INGESTION_UNSUPPORTED` ("fenced: this replica no
longer owns shard ...") instead of being forwarded to the owner, so a remote join into a
clustered server fails whenever the room is not on the replica the request lands on
(`cluster_device_lists` puts its room on the surviving replica's shard to stay clear of it).

**Left.** The join-in-progress mark is per process: in a cluster, a `/send` for a room being
joined that lands on another replica than the one making the join is still ignored until the
join is held. `/members?at=` for a point the reader may not see still answers what they may see
now (Synapse: `403`). A shard's place can be moved on by a replica that lost the shard during
the look (the place is stored only for shards still owned after it, which narrows but does not
close the window).
## 2026-10-08 (branch `agent/fed-forward`): federation in a cluster reaches the room's owner (decision 0035)

Tracks 03 and 06; the cluster side is in `docs/status/03-cluster.md`'s section of the same date.
A remote server's `send_join` that reached a replica not owning the room was refused
`501 M_HS_INBOUND_INGESTION_UNSUPPORTED` ("fenced: ..."), and `/send`'s PDUs for such a room were
refused one by one. Now `hs-cli`'s shard gate forwards every federation request for one room
(the membership handshakes, `invite`, `exchange_third_party_invite`, the room reads) to the
owner, and `/send` hands each write to its room's owner over the mesh
(`hs-cli` `federation_forward`).

`hs-federation` changes, for a write the room's fence refused because the shard moved (nothing
stored; the owner should be asked):

- `WriteRejected` has `not_owner` (`WriteRejected::not_owner`); `hs-cli`'s `RegistryWriteSink`
  maps `RoomError::Fenced` to it.
- `JoinError::NotOwner` (`send_join`/`send_leave`/`send_knock`) and `InviteError::NotOwner`
  answer `503 M_HS_NOT_SHARD_OWNER`, which the gate sends on to the new owner; every other store
  failure keeps its old status. `InviteRejected` is now a struct (`message`, `not_owner`) with
  `InviteRejected::new` and `InviteRejected::not_owner`.
- `TransactionError::NotOwner`: a `/send` PDU no owner could take within the forward deadline
  answers the transaction `503` and leaves it unremembered, so the sender retries it.

Tests: `transport::join::tests::a_write_another_replica_owns_is_a_503_and_a_store_failure_is_not`,
`inbound::tests::a_pdu_no_owner_could_take_fails_the_transaction_unremembered`; end to end,
`crates/hs-cli/tests/cluster_federation.rs` (join, `/send`, leave, knock, knock withdrawn,
inbound invite, each reaching the non-owner; see status 03). `cargo test -p hs-federation`
(226 unit) and clippy pass.

Left: Complement and Sytest run single-node and are unaffected; no cluster-mode conformance run.

## 2026-10-05 (branch `agent/fed-wave3`): two regressions, durable EDUs (RFC 0023), dropped PDUs, two Sytest races, bridge aliases over federation

Wave 3's federation brief. Crates: `hs-federation`, `hs-room` (actor: `fetched_state`,
`rejected`, `load`), `hs-cli` (wiring and tests). `hs-state` and `hs-model` needed no change.
Decision **0032** (pushed PDUs answered as Synapse answers them; remote joins carry their
profile keys). RFC **0023** accepted and implemented.

**1. "Banned servers cannot /invite"** (`hs-federation` `invite`, `transport::membership`). The
ACL check was already first (the route layer, `acl::enforce_on_room_routes`); what failed was
Sytest's *control* invite, before the ban: into a version-12 room this server made, with an empty
`invite_room_state`, which wave 2's MSC4311 check refused `400 M_MISSING_PARAM`. The create event
is required only of an invite into a room this server does not hold (new `room_is_held`
argument of `receive_invite`, from `RoomDataSource::room_version`), which is what the room state
is for; Complement's `TestMSC4311RejectInvalidStrippedStateFederation` (a room only the inviter
holds) is still refused. A refusal is now logged at `info`.

**2. "outliers whose auth_events are in a different room are correctly rejected"** (`hs-room`
`actor::fetched_state`, `actor::rejected`, `RoomActor::load`). A missing prev event fetched with
its state (`accept_prev_event_with_state`) whose auth events cite an event of another room was
"missing ancestors" (the other room's event is not this room's), so the fallback failed and
`/send` answered the PDU with an error. Now `authorize_outlier` rejects it, as a received event
citing another room's event is rejected; and a rejected prev event is still held with the state
it was fetched with (the state after a rejected event is the state before it:
`record_rejected_outlier_state`, kept across a reload, and `effective_prev_sns` stops at it), so
the events after it are judged there -- Sytest's R rejected for citing Q, S accepted.

**3. Durable to-device and device-list EDUs (RFC 0023)** (`hs-federation` `sender`,
`outbound_store`; `hs-cli` `edus`). `m.direct_to_device`, `m.device_list_update` and
`m.signing_key_update` (`sender::DURABLE_EDU_TYPES`) are kept in the outbound store
(`hs_federation.outbound_edus`, coalescing index `outbound_edu_keys`, counters
`outbound_edu_lengths`) until a transaction carrying them is accepted, whichever enqueue method
names them; each transaction carries the oldest durable EDUs first. `resume` starts a worker for
every destination with durable EDUs waiting ("resuming durable EDUs left by a previous run",
`info`). Bound `SenderConfig::max_queued_durable_edus_per_destination` (10 000; past it the oldest
goes, `warn`). The device-list announcer stores its stream position in the sender's store
(`edus::ANNOUNCER_POSITION`) and resumes from it, so a change committed while the server was
stopped is announced at the next start. Not an `hs-config` key yet.

**4. A pushed PDU that cannot be placed is answered `{}`** (`hs-federation` `inbound`, decision
0032). When its missing prev events or auth events cannot be obtained, the PDU is dropped,
logged at `info`, counted in `hs_federation_pdus_dropped_total{reason}`, and answered `{}` --
as Synapse, which answers every pushed PDU before processing it. Complement's
`TestCorruptedAuthChain` needed exactly this (its received event cites E, whose chain lacks B;
Synapse drops it too). Every processed PDU is also logged at `debug` with its result.

**5. "Guest users are kicked ... over federation": the race** (`hs-cli` `remote_join`, decision
0032). The test joins the remote user a second time (`matrix_join_room_synced`) and waits for
the room in an incremental `/sync` from a position taken just before. The second join was
idempotent here (identical content: no event), so the sync only showed the room if the power
levels and guest access, sent a moment earlier on the other server, arrived after Sytest took the
position -- they usually did not, by a millisecond (Sytest logs: the remote user's sync position
`next=2` at 16,361 ms, the two events processed at 16,358 and 16,360, then ten seconds of empty
syncs). On Synapse the second join is a new event because Synapse's remote join content carries
`displayname` and `avatar_url` even when unset (`null`) and its local join does not. A join
through another server now carries them as `null` too. Pinned, deterministically (B is made to
hold the guest access before the position is taken), by `guest_access_federation`; it fails
without the change.

**6. "Can invite unbound 3pid over federation with users from both servers": a race in the test,
not fixed.** The joiner on the second server is a `remote_user_fixture` with no event-stream
token, so its first `GET /events` has no `from` and starts from now -- as Synapse's does
(`Notifier.get_events_for`). `hs-user`'s route does the same: `crates/hs-user/src/routes/events.rs`
lines 201-206 (`Some(Err(_)) | None => current_token(...)`), and nothing is wrong there. The
`m.room.third_party_invite` reaches the second server before that first request: server A stored
it at 04,453 ms, server B processed it at 04,454 (new debug log), and B's first `/events`
answered at 04,960 after its 500 ms wait, so it began at about 04,460. Synapse passes because its
staged `/send` processing takes longer than Sytest's next request. Nothing a server should do
differently; it passes when the event is slower (as on `c2d74174`).

**7. Alias queries over federation ask the bridge** (`hs-cli` `federation::ServerQuerySource`).
`GET /query/directory` for a local alias the directory does not hold asks the appservices whose
alias namespace covers it (`AppserviceRegistry::query_room_alias`, the same call the client
directory makes), as Synapse's `get_association` does; the answer lists every server with a
joined member after this one (it listed this server only). `build_mount` takes the registry.

**Verified.**

- Unit tests: `hs-federation` 224 after the rebase (new `sender::tests::{durable_edus_survive_a_restart_and_wait_for_a_destination_that_is_down,
  the_durable_edu_queue_is_bounded_dropping_the_oldest}`, the version-12 invite test extended to
  a held room); `hs-room` (new `actor::rejected::tests::a_fetched_prev_event_citing_another_rooms_event_is_rejected_and_what_follows_judged`,
  across a reload); `hs-state` unchanged and passing.
- Real `hs serve` (`hs-cli`, the whole `cargo test -p hs-cli`: 70 result lines, none failed),
  new: `guest_access_federation` (two servers; fails without item 5), `appservice_alias_federation`
  (two servers and a bridge: bob of B joins `#bridged-room:A`, which A's bridge makes when asked),
  `third_party_invites_federation` extended (the watcher on B reads the legacy `/events`
  stream, from-less first, as Sytest does, in a private room it was invited to);
  `federation_state_fallback` and `federation_writes` expect `{}` for a dropped PDU and the
  counter.
- `cargo fmt --all --check`; `cargo clippy -p hs-federation -p hs-room -p hs-cli --all-targets --
  -D warnings`: clean.
- **Complement** (image `complement-hs-fedwave3:dev`: `complement-hs-main:w2` with this branch's
  bookworm `hs`; under the shared lock, patches applied): `TestCorruptedAuthChain`,
  `TestDeviceListsUpdateOverFederation` (all three, `stopped_server` included),
  `TestToDeviceMessagesOverFederation` (all three) newly pass; `TestMSC4311RejectInvalidStrippedStateFederation`,
  `TestMSC4311FullEventsOnStrippedStateFederation`, `TestInboundFederationRejectsEventsWithRejectedAuthEvents`,
  `TestInboundCanReturnMissingEvents`, `TestOutboundFederationIgnoresMissingEventWithBadJSONForRoomVersion6`
  still pass. The whole `./tests/` package: 86 of 90 top-level pass (run 12's baseline 82):
  newly passing the three above and `TestUnbanViaInvite`; failing, as in the baseline and outside
  this brief, `TestDeviceListsUpdateOverFederationOnRoomJoin`, `TestSyncOmitsStateChangeOnFilteredEvents`,
  `TestJumpToDateEndpoint`, `TestMSC4291RoomIDAsHashOfCreateEvent_RoomIDIsOnCreateEvent`. No
  regression.
- **Sytest** (`SYTEST_HS_BINARY` of this branch): `50federation/{33room-get-missing-events,50server-acl-endpoints}.pl`
  all pass ("outliers ... correctly rejected" and "Banned servers cannot /invite" newly);
  `30rooms/13guestaccess.pl` all pass; `30rooms/12thirdpartyinvite.pl` all but item 6. The whole
  suite: **748 of 772** pass, 17 skipped as in wave 2's run, 7 fail (wave 2's measure 742): the
  two `/messages` tests of `10apidoc/34room-messages.pl` (route side), item 6, `31sync/08polling.pl`'s
  two and "The only membership state included in a gapped incremental sync ..." (`hs-user`), and
  "If a device list update goes missing, the server resyncs on the next one" (failing in wave 2
  too; E2EE).

**Left.** Item 6 is the test's race. `federation.max_queued_durable_edus_per_destination` is not
a config key (the sender's default applies). The announcer's stored position is one for a whole
cluster. `TestDeviceListsUpdateOverFederationOnRoomJoin` was not in this brief.

## 2026-10-04 (branch `agent/federation-gaps`): the /send deadlock, cross-room ancestors, third-party invites over federation, and room version 12

Wave 2's federation brief: the two Sytest regressions of `c2d74174`, Sytest's cross-room,
erasure, ban and third-party-invite federation tests, and Complement's federation and
version-12 failures. Crates: `hs-federation`, `hs-room` (actor and remote-event paths,
`third_party_invite.rs`, `remote_join.rs`), `hs-state`, `hs-cli` (wiring and tests); `hs-model`
needed no change.

**1. The regressions were a distributed deadlock** (`hs-federation`, `client`). The outbound
client allowed one request in flight per destination (`DEFAULT_PER_DESTINATION_CONCURRENCY = 1`,
on the belief that Synapse does; Synapse limits *transactions* per destination, which the
sender already does alone). When both Sytest servers were sending each other a transaction and
each `/send` handler made a request back (a device-list resync for a joiner, `GET
/user/devices`, since `5fc19dc5` more often), each request queued behind its own server's
outbound transaction, which waited for the other's handler: 30 s frozen until the request
timeout broke it (wave-1 logs: both `/send`s time out at the same millisecond, the device
fetches complete 6 ms later). "Message history can be paginated over federation" and "Remote
room alias queries can handle Unicode" ran in that window; alone they pass on `main` too.
Now 8 per destination, and a wait over a second for a slot is logged at `info` ("a federation
request waited for a free slot to its destination", with `waited_ms` and `limit`). Pinned by
`client::tests::a_request_is_not_held_up_behind_a_slow_one_to_the_same_destination`.

**2. Cross-room ancestors** (`hs-room`, `actor::rejected`, `actor::redactions`, `actor::gaps`).
An event citing an event of *another* room this server holds, in `auth_events` or
`prev_events`, is rejected (stored as such, `{}` to `/send`) instead of answered "missing
ancestors" and fetched for (`RoomActor::held_in_another_room`). A redaction naming an event of
another room is withheld from clients (held soft-failed, logged "withheld a redaction ..."), as
Synapse withholds it; the other room's event is untouched. A timeline gap does not wait for an
event of another room.

**3. A backward page backfills past a fetched prev event** (`hs-room`, `persist_with`). An event
placed after a prev event held with a fetched state (`fetched_state`) now opens a timeline gap
below it, as a rejoin does, so `/messages` stops there and asks `/backfill` for the outlier
(before, the page walked straight from it to the join and asked nothing).

**4. A local membership another server made is not sent again** (`hs-room`
`RoomActor::is_proactively_sent`, `hs-cli` `federation_sender`). The join a `send_join` made (and
a leave or knock a resident took, a restricted join through a resident) was queued for the
room's servers as if made here; the resident had it already and got it again ahead of the
next message (Complement's `TestOutboundFederationSend`, `TestFederationRedactSendsWithoutEvent`
read the first PDU). Synapse's `proactively_send = False`. A co-signed invite is still sent.

**5. Erased accounts** (`hs-cli` `RegistryRoomSource::with_erasure`). `/event`, `/backfill` and
`/get_missing_events` serve an erased local account's events redacted, as Synapse's
`filter_events_for_server`.

**6. A banned user may not read the room's state** (`hs-room` `reader_view`): `403`, as Synapse's
`check_user_in_room_or_world_readable`; a member who left still reads the state at their leave.

**7. Third-party invites over federation** (`hs-room` `third_party_invite`, `remote_join`;
`hs-cli` `identity_service::on_exchange`, `remote_join`, `serve`; `hs-federation` seam removed).
`/3pid/onbind` for a room this server is not in -- or one whose invitation a user of another
server made -- hands the invitation to the inviter's server (`PUT
/exchange_third_party_invite/{roomId}`, new `RemoteJoin::exchange_third_party_invite`); the
inbound route (behind `X-Matrix`) makes the invite (`third_party_invite::on_exchange`, the keys
re-checked with the identity server) and sends it to the invitee's server by `/invite`. An
exchange for a remote invitee now goes through `/invite` too. Counted
`hs_room_third_party_invites_total{outcome="forwarded"}`.

**8. Missing auth events are fetched by ID** (`hs-federation` `inbound`,
`state_fallback::fetch_missing_auth_events`; `hs-room` `accept_auth_outliers`). A received event
whose prev events are all held but whose auth events are not gets them by `GET /event` (and
theirs, ten rounds at most), each judged by its own auth events -- a rejected one rejects the
event citing it -- instead of `/get_missing_events` and `/backfill`. New
`RoomWriteSink::accept_auth_outliers` (default refuses).

**9. `/get_missing_events` answered means `/backfill` is not asked** (`hs-federation` `backfill`),
as Synapse asks nothing more for a pushed event; `/backfill` is still the fallback when
`/get_missing_events` fails. What is left goes to the `/state_ids` fallback or is refused.

**10. Outliers with a broken auth chain are left out** (`hs-room` `authorize_outlier`): one whose
auth events are not all held is `MissingAncestors`, not judged by the ones that are (which let
Complement's `TestCorruptedAuthChain`'s C, D, E become the state without B).

**11. Room version 12** (`hs-room`, `hs-state`, `hs-federation`). A trusted private chat's
invitees are additional creators (and not in the power levels); naming a creator in the power
levels or sending a second `m.room.create` is `400 M_BAD_JSON` before the auth rules (which
answered `403`); state resolution v2.1 includes the conflicted state subgraph (MSC4297,
`state_res::v2::conflicted_subgraph`; it passed the conflicted events alone); MSC4311: an
invite's and a knock's room state go as whole PDUs, and a version-12 invite without the create
event is `400 M_MISSING_PARAM`.

**12. Also:** a `public_chat` room has no `m.room.guest_access` (Synapse's preset;
`TestInboundCanReturnMissingEvents` reads the room's first events by position);
`/query/profile` with a user ID whose server name does not parse is `400`.

**Verified.**

- Unit tests: `hs-federation` (221; new `client::tests::a_request_is_not_held_up_behind_a_slow_one_to_the_same_destination`,
  `invite::tests::a_version_12_invite_without_the_create_event_in_its_room_state_is_refused`,
  `transport::read_routes::tests::a_user_id_with_a_non_numeric_port_is_not_valid`,
  `backfill::tests::a_partial_get_missing_events_answer_is_not_continued_by_backfill`);
  `hs-room` (lib 187 and every integration file; new
  `actor::rejected::tests::{an_event_citing_an_event_of_another_room_is_rejected_not_fetched_for, a_redaction_of_an_event_of_another_room_is_withheld}`,
  `actor::fetched_state::tests::fetched_events_whose_auth_chain_is_broken_are_left_out`,
  `actor::tests::{a_banned_member_may_not_read_the_rooms_state_but_one_who_left_may, version_12_creators_are_the_invitees_of_a_trusted_chat_and_never_named_in_power_levels}`);
  `hs-state` (new `state_res::v2::tests::an_event_between_two_conflicted_events_is_in_the_subgraph`;
  the oracle cross-check property tests, which include room version 12, still agree).
- Real `hs serve` (`hs-cli`), new: `federation_state_fallback::{back_pagination_backfills_past_a_fetched_prev_event_and_never_crosses_rooms,
  an_erased_accounts_events_are_served_to_other_servers_redacted,
  a_missing_auth_event_is_fetched_by_id_and_its_rejection_carries_over}` (the first fails
  without the gap change, checked), `federation_sender::a_local_users_join_another_server_made_is_not_sent_again`,
  and `third_party_invites_federation` (two servers and a fake identity server: B hands A the
  invitation, A invites over `/invite`, the invitee joins; with a member of B in the room too,
  who sees the invitation in `/sync`). `federation_writes`'s two backfill tests now answer
  `/get_missing_events` (or refuse it) as rule 9 needs. The whole `cargo test -p hs-cli`: 58
  result lines, none failed.
- `cargo fmt --all --check`; `cargo clippy -p hs-federation -p hs-room -p hs-state -p hs-model
  -p hs-cli --all-targets -- -D warnings`: clean. Not run: the workspace gate.
- **Sytest** (release `hs` on bookworm from this branch through `SYTEST_HS_BINARY`, the eight
  files: `30rooms/{04messages,05aliases,07ban,12thirdpartyinvite}.pl`,
  `50federation/{31room-send,32room-getevent,34room-backfill,39redactions}.pl`; 49 of 53, 41 on
  `main`'s image): newly passing "Remote banned user is kicked and may not rejoin until
  unbanned", "Can invite unbound 3pid over federation", "... with no ops into a private room",
  "Events whose auth_events are in the wrong room do not mess up the room state", "Inbound
  federation redacts events from erased users", "Backfilled events whose prev_events are in a
  different room do not allow cross-room back-pagination", "An event which redacts an event in a
  different room should be ignored"; "Message history can be paginated over federation" and
  "Remote room alias queries can handle Unicode" pass (they also pass alone on `main`: they
  failed only in the deadlock window of a full run, item 1). Still failing: the two
  "Ephemeral messages ... are correctly expired" (MSC2228: `room-client-gaps` expires them at
  render time, on its branch; nothing federation-side is needed for "from servers", whose
  message is a local user's), "Can delete canonical alias" (route side), and "Can invite unbound
  3pid over federation with users from both servers": the joiner on the second server waits for
  `m.room.third_party_invite` on the legacy `GET /events` stream and never sees it, though it
  reaches that server (its `/sync` has it, pinned in `third_party_invites_federation`) -- an
  `hs-user` question.

- **Complement** (image `complement-hs-federation-gaps:dev` of this branch, run under the shared
  lock, `refs/complement` with `apply_patches.sh` applied; `-run` the brief's tests): all pass but
  two. Newly passing: `TestInboundFederationProfile` (both), `TestFederationRedactSendsWithoutEvent`,
  `TestInboundFederationRejectsEventsWithRejectedAuthEvents` (three),
  `TestOutboundFederationIgnoresMissingEventWithBadJSONForRoomVersion6`, `TestOutboundFederationSend`,
  `TestInboundCanReturnMissingEvents` (four), `TestMSC4289PrivilegedRoomCreators` (eleven),
  `_Additional`, `_InvitedAreCreators`, `_AdditionalCreatorsAndInvited`,
  `TestMSC4291RoomIDAsHashOfCreateEvent_CannotSendCreateEvent`,
  `TestMSC4297StateResolutionV2_1_includes_conflicted_subgraph`, `TestMSC4311StrippedStateClientAPI`
  (four; the remote invite and knock had timed out at 30 s, the item 1 deadlock),
  `TestMSC4311FullEventsOnStrippedStateFederation` (two), `TestMSC4311RejectInvalidStrippedStateFederation`.
  Still failing:
  - `TestCorruptedAuthChain`: C, D and E are now left out (item 10, the test's point), but the
    event for `/state_ids` cites E among its auth events, so it cannot be held either, the
    fallback fails, and `/send` answers the received event with an error, which
    `MustSendTransaction` refuses. Synapse uses the fetched state for the pulled event without
    holding the prev event; doing that here is the next step (`hs-federation` `state_fallback`,
    `hs-room` `fetched_state`).
  - `TestMSC4291RoomIDAsHashOfCreateEvent_RoomIDIsOnCreateEvent`: the client-facing create event
    of a version-12 room needs `room_id` added when rendered (`hs-room` `routes/render.rs`,
    `room-client-gaps`'s).

**Left.** `TestCorruptedAuthChain`'s `/send` answer (above). The legacy `/events` stream not
showing a remotely received `m.room.third_party_invite` (`hs-user`). State deduplication of a
`PUT /state` with the same sender and content (Synapse's `deduplicate_state_event`) is route
side; `TestInboundCanReturnMissingEvents`'s `shared` case passes without it today because the
actor's `send_event` already reuses identical content. MSC2228 expiry is `room-client-gaps`'s
(render time); if it lands as a stored redaction instead, federation serves it redacted with no
further change here.

## 2026-10-04 (branch `agent/fed-state-ids`): the `/state_ids` fallback's federation side, soft failure, and float bodies

Closes session 18 item 6 (the half left open), the soft-failure half of `docs/next-steps.md`
item 3, the coordinator's "invalid JSON for room version 6 is `401`" item, and Complement's
`TestMSC4289PrivilegedRoomCreators_AdditionalValidation` `403`. Crates: `hs-federation`,
`hs-room`, `hs-cli` (wiring and tests). `hs-model` and `hs-state` needed no change (the
soft-failed flag was already in `EventFlags`).

**1. The `/state_ids` fallback** (`hs-federation`, new `state_fallback`). When
`backfill::resolve_missing_ancestors` gives up with events it fetched but could not place
(their own prev events unknown: the peer answered `/get_missing_events` with one hop and
`/backfill` with `404`, as Sytest's does), it now hands those events to
`state_fallback::resolve_through_state`: oldest first, each is offered to the room again, and
one still missing a prev event gets, for each such prev event (at most 5), `GET /state_ids` at
it (`GET /state` when that is not answered), `GET /event` for the prev event, then `GET /event`
for what the state, its auth chain, the prev event's auth events and the citing event's
missing auth events name that the room lacks (the whole `/state` once instead when more than
100 or a tenth of what is named is missing), one more round for those events' own auth events,
then `RoomWriteSink::accept_prev_event_with_state` (the room side, `hs-room`
`actor::fetched_state`, unchanged), then the pending event again. Then
`inbound::process_transaction` retries the received event as before. Every fetched event is
verified (`verify_pdu`), must be the event asked for and of the room (an event of another room
named in a state is dropped). At most 5 pending events cost a state fetch; the fallback has its
own `max_duration`. **The received event's own missing prev events never get the fallback**:
Sytest's "Federation rejects inbound events where the prev_events cannot be found" fails if the
state at such a prev event is asked, and Synapse refuses that event the same way (an event
pushed to us must not bring a state its sender made up). New interfaces:
`AncestorFetcher::{fetch_state_ids, fetch_state, fetch_event}` (defaults answer an error;
`FederationClient` implements them over `state_ids`, `room_state`, `event`),
`RoomWriteSink::{unknown_events, accept_prev_event_with_state}` (defaults: everything unknown,
refuse), `BackfillGiveUpReason::StateFallbackFailed { backfill, state }`. `hs-cli`'s
`RegistryWriteSink` implements both sink methods over `RoomActor::events_not_held` and
`RoomActorHandle::accept_prev_event_with_state`. Observability: `info` per prev event taken
("took a missing prev event with the state another server answered for it", with room, event,
`state_events`, `fetched`) or refused, `info` when backfill hands over to the fallback, and
`hs_federation_state_fallbacks_total{outcome=resolved|rejected|no_state|no_event|refused|timed_out}`
(registered with the transport metrics).

**2. Soft failure over federation** (`hs-room`, new `actor::soft_fail`; the flag
`EventFlags::is_soft_failed` was already in `hs-model`). `accept_remote_event` runs the third
receipt check: the auth rules against the room's current state, resolved across the forward
extremities and the state before the event (as Synapse's `_check_for_soft_fail`), skipped when
the event's prev events are the extremities and for the importer's quiet copies. An event that
fails only this is stored with the flag, placed in the timeline, fed to the state store, and
answered `RemoteEventOutcome::SoftFailed` (`/send` answers `{}`): it is **not a forward
extremity and supersedes none**, **not published** (so `/sync`, push, appservices and the
federation sender never hear of it), not in the relations or joined-rooms index, and hidden
from `event_by_id`, `event_at`, `events_around`, `events_after`, the `/messages` and `/sync`
pages and `head_update`; it stays hidden after a load and in a replica's catch-up. Federation
still serves it (`RoomActor::held_event`, which `hs_cli::federation`'s `/event`, `/state`,
`/state_ids` and auth-chain reads use now). A later accepted event citing a soft-failed (or
rejected) event supersedes the extremities beneath it (`RoomActor::superseded_extremities`,
Synapse's `_get_prevs_before_rejected`). Logged at `info` ("soft failed an event received over
federation"), counted `hs_room_soft_failed_events_total`. New: `RemoteEventOutcome::SoftFailed`
(callers in `hs-cli` updated), `RoomActor::{is_soft_failed_event, held_event}`.

**3. Found on the way, in `hs-room`'s room side (`actor::fetched_state`).** The missing prev
event is now judged by its own `auth_events` only, like the rest of the fetched events and as
Synapse judges a fetched prev event (`_auth_and_persist_outliers`); it was also checked at the
fetched state, and Sytest's state for "... asks for /state_ids and resolves the state" contains
a power-levels event whose signature does not verify (so the state had no power levels and the
honest prev event was refused). `StateBefore::Explicit` went with it. And **an outlier in room
version 12 was refused for "no create event"**: with no state before it, the auth-events
selection looked for the create event among its `auth_events`, where MSC4291 never puts it;
it now asks whether the room holds it. That broke the fallback (and any outlier) in version-12
rooms, which is what Sytest's fixtures create.

**4. `/state` and `/state_ids` at an outlier are `404`** (`hs-cli`,
`federation::state_before_with_chain`: an event with no timeline position), as Synapse
answers, though the room holds a state for a fetched prev event.

**5. A request body with a float is authenticated, then judged** (`hs-federation`,
`xmatrix`): the X-Matrix signature is checked over the body canonicalised leniently about
numbers (as Synapse), so `send_join`/`/invite`/`send_leave` with a float in a version-6 room
reach the handler and its `400 M_BAD_JSON`, instead of failing the signature check with `401`.
A body other than the one signed still fails.

**6. `createRoom` judges `creation_content.additional_creators`** (`hs-room`,
`routes::create_room`): in a room version with additional creators (12), anything but an array
of user IDs is `400 M_BAD_JSON`; it was left to the create event's auth check, a `403`.

**Verified.**

- `cargo test -p hs-federation` (218): new `state_fallback::tests::{a_fetched_event_whose_prev_event_is_missing_is_placed_through_the_state_at_it,
  the_received_events_own_missing_prev_event_never_gets_the_state_fallback,
  a_prev_event_that_cannot_be_fetched_fails_and_an_event_of_another_room_is_dropped}`,
  `xmatrix::tests::a_signed_body_with_a_float_is_authenticated_and_left_to_the_handler`;
  `backfill::tests::gives_up_cleanly_on_an_endless_chain_instead_of_looping_forever` now
  expects the fallback's give-up too.
- `cargo test -p hs-room` (lib 182, every integration file; `scenario::create_room_validates_request_shape`
  now covers Complement's four bad `additional_creators` and the good one): new
  `actor::soft_fail::tests::{soft_failed_events_are_held_hidden_and_never_extremities` (all
  three `52soft-fail.pl` graphs, extremities after each step, the next local event's
  `prev_events`, across a reload), `an_event_citing_a_soft_failed_one_is_accepted_and_supersedes_what_it_stands_on}`.
- `hs-cli`, new `tests/federation_state_fallback.rs` on the full `hs serve` (`spawn_serve`)
  and a stand-in for Sytest's server over HTTP (its own key, `/get_missing_events` one hop,
  `/backfill` and `/state` `404`, `/state_ids` and `/event`):
  `a_missing_prev_event_is_taken_with_the_state_the_sending_server_answers_for_it` and
  `..._with_its_state_in_room_version_12` (36state's C/X/Y/T graph: X and C in alice's
  `/sync`, Y and T in the room's state, the state at X has them, `/state[_ids]` at Y is `404`,
  the counter on `/metrics`; the version-12 one fails without the create-event fix), `an_event_whose_prev_event_nobody_divulges_is_refused_without_asking_the_state`,
  `an_event_the_current_state_refuses_is_soft_failed_and_kept_from_clients` (C not in alice's
  sync and `404` to her, served over federation `/event`, D in her sync, the counter). With
  the fallback and the soft-fail check disabled, the first and third fail (checked).
  The whole `cargo test -p hs-cli` (53 result lines, all `ok`), including `federation_writes` (9; the endless-chain bound now includes the fallback's
  `2 x MAX_PENDING_EVENTS` requests), `federation_two_servers`, `federation_reads`,
  `federation_room_versions`, `federation_membership`, `federation_catch_up`,
  `federation_sender`, `federation_edus`: pass.
- `cargo fmt --all --check`; `cargo clippy -p hs-federation -p hs-room -p hs-cli --all-targets
  -- -D warnings`: clean. Not run: the workspace gate.
- **Sytest**, the federation set of session 18 (`tests/50federation/*.pl`,
  `30rooms/05aliases.pl`, `30rooms/70publicroomslist.pl`; release `hs` on bookworm built from
  this branch rebased on `a802724a`, through `SYTEST_HS_BINARY`, image `myelin-sytest:dev`):
  **118 of 130** (108 on `main` `a9f62fc7`, 2026-10-04 run 3).
  `docs/status/sytest/2026-10-04-fed-state-ids-results.txt` and `-summary.txt`. Newly passing
  (11): "Outbound federation requests missing prev_events and then asks for /state_ids and
  resolves the state", "Federation handles empty auth_events in state_ids sanely", "Should not
  be able to take over the room by pretending there is no PL event", "Forward extremities
  remain so even after the next events are populated as outliers", "outliers whose auth_events
  are in a different room are correctly rejected", "/state returns M_NOT_FOUND for an outlier",
  "/state_ids returns M_NOT_FOUND for an outlier", "Inbound federation accepts a second
  soft-failed event", and the three "... invalid JSON for room version 6" (`send_join`,
  `/invite`, invite rejections). "Federation rejects inbound events where the prev_events cannot
  be found" and both other soft-failure tests still pass. **One test moved the other way:**
  "Local device key changes get to remote servers" failed in two of three runs of this branch
  and passed in the third (it receives the previous test's user's `m.device_list_update`; device
  lists are track 08's, and this branch changes nothing they send). A first run without the
  rebase measured 15/130: the branch predated `main`'s Sytest-plugin fix (`ipv4_only: false`,
  haproxy on both loopback families), so every key fetch from Sytest's server failed.

**Decisions made.**

- The fallback runs only for events backfill fetched, never for the event received over
  `/send` (see 1). Sytest's tests and Synapse agree.
- Soft-failed events take a timeline position (so a load and catch-up find them in order and
  the state store is fed in order) and every client read skips them, rather than being held as
  outliers: they are in the graph, cited by later events, and their state takes part in
  resolution.
- The soft-failure check is skipped for history (backfill, gap fills: judged at their
  position, as Synapse's `backfilled` events skip it) and for the importer.
- Request signatures are checked over lenient canonical JSON; the event's own canonical JSON
  stays strict, judged where the room version is known.
- A missing prev event fetched for the fallback is an outlier like the state fetched with it:
  judged by its own auth events only (Synapse's rule). What a made-up state can do is still
  bounded by every fetched state event passing its own auth events, and by the state
  resolution of the event that cites it (Sytest's take-over test passes).

**Left.** Of the coordinator's version-12 MSC Complement tests only the `additional_creators`
`400` was reached (not run under Complement here); `TestMSC4289*`'s others, `TestMSC4291*`,
`TestMSC4297*` and `TestMSC4311*` were not. Complement's `TestInboundCanReturnMissingEvents`
was not run. In Sytest's set, still failing and in this track: "Events whose auth_events are in
the wrong room do not mess up the room state" (an auth event of another room is a missing
ancestor; Synapse fetches the auth chain and judges the event without it),
"Backfilled events whose prev_events are in a different room do not allow cross-room
back-pagination" (a timeout in the `/messages` backfill), the cross-room redaction, erased
users' events, ephemeral messages. A soft-failed event that later becomes part of the current
state through resolution is shown in state reads but not in the timeline (the spec allows
either; Synapse does the same). Erased users' events over federation, the cross-room
redaction, ephemeral messages and federated presence are untouched.

## 2026-10-02 (branch `agent/outbound-ipv4-only`): the outbound address policy, and why `maunium.net` failed

**What was wrong.** The demo pod logged `could not fetch remote media ... origin=maunium.net ...
tcp connect error: Network unreachable (os error 101)` three times in three hours. `maunium.net`
delegates to `federation.mau.chat`, which has an A record (`95.216.50.134`) and an AAAA record
(`2a01:4f9:3a:ff34::`); the pod has no IPv6 route. The owner reproduced it on the desktop, which
has no IPv6 route either: `curl -6` fails in 7 ms, `curl -4` connects, plain `curl` connects
over IPv4 because it tries both. The server did not, and the reason was not hyper: hyper-util's
connector does Happy Eyeballs by itself (the first address's family in order, the other family
after 300 ms or at once when the first fails). The reason was `FederationClient::client_for`,
which pinned each destination's pooled client with `.resolve(tls_server_name, first address)`
-- a single `SocketAddr` chosen upstream from discovery, the AAAA record hickory listed first --
so the connector had one address and nothing to fall back to. `hs-media`'s URL previewer did the
same (`resolve_and_check` checked every address and returned `candidates[0]`). No custom
resolver and no short connect timeout were involved; it was the pinned address.

**What landed.**

- `hs-config`: a `network` section, `network.outbound.ipv4_only` (`crates/hs-config/src/network.rs`),
  on by default, classified **hot** in `reload::SETTINGS` (every outbound client's resolver
  reads it per new connection). `docs/config.md` regenerated (`network` is a fully hot
  section); the web schema fixture regenerated and `npm run check` passes, so the settings page
  renders it as a plain toggle with the doc comment as its explanation.
- `hs-http::outbound` (`crates/hs-http/src/outbound.rs`), the one place the policy lives:
  `set_ipv4_only`/`ipv4_only`/`describe`; `select` (duplicates dropped, IPv6 dropped when the
  policy says so, the resolver's order kept, a `ResolveError::OnlyIpv6` that names the setting
  when nothing is left); `Resolver`, a `reqwest::dns::Resolve` over `tokio::net::lookup_host`
  (the same `getaddrinfo` reqwest's own resolver uses) or over a caller's pinned addresses,
  which hands hyper the *whole* list; and `ObserveLayer`, a `connector_layer` that reads the
  address that connected from hyper-util's `HttpInfo` and the candidates the resolver offered
  (a task-local), logs each passed-over address and its family at `debug`, and counts
  `hs_outbound_connections_total{family}` and `hs_outbound_connect_failures_total{family}` (a
  connection that fails altogether counts every address). `hs_http::client::builder()` and
  the new `pinned_builder(host, addrs)` install both.
- Every outbound client goes through it: the federation client (`client_for` pins every
  resolved address with the connect port, and rebuilds the pooled client when the addresses
  change, not only the server), the `.well-known` fetcher and key fetches (the former through
  the shared builder, the latter through the client), remote media and URL previews (the
  previewer pins every checked address), push (`hs-push` already used the shared builder),
  appservice query/ping/scheduler (already) and provisioning (now), modules' HTTP callbacks
  (now, `hs-modules` gained the `hs-http` dependency), identity servers
  (`hs-cli/src/identity_service.rs`, now).
- `hs-cli`: `live_config::apply_network` sets the policy at boot and on a change to `network`
  and logs `outbound: IPv4 only` / `outbound: IPv4 and IPv6`; the counters are registered into
  `/metrics`; `ServeOptions::federation_resolvers` is the test seam for discovery's resolvers.
- Not covered, on purpose: the ICAP scanner (`icap-rs` owns its TCP; the host is an in-cluster
  service), the cluster mesh (`hs-cluster`'s forwarder connects to replica addresses the
  registry holds as literals), `hs-bridges`' client (another agent's crate today),
  `hs-operator` (a separate binary), the CLI subcommands that talk to the local server, and a
  URL whose host is an IP literal (hyper parses those without asking the resolver).

**What hyper gives, and what was added.** reqwest 0.12.28 over hyper-util 0.1.20: the connector
already tries every address it is given with a 300 ms family fall-back
(`set_happy_eyeballs_timeout` is hyper-util's default; reqwest does not expose it, so the
default stands). What it does not give is a per-address view: its connect error type is
private, its per-attempt log is `trace!` behind a feature reqwest does not enable, and a
`connector_layer` cannot re-call the inner connector (the request type is not `Clone`). So the
layer infers: the address that connected (public, `HttpInfo`) against the candidates in the
connector's attempt order. A slow first family that loses the 300 ms race is counted as passed
over like a dead one; the module doc says so.

**Verified.**

- `cargo test -p hs-http`: six unit tests (the policy filter and ordering, the error text, the
  attempt order, pins, the system resolver, the connect-error classification) and
  `tests/outbound.rs`, which pins `dual-stack.test` to `[2001:db8::1]:port` and a listener on
  `127.0.0.1`: under the default the IPv6 address is never tried (the listener's connection
  count and the counters say so); with IPv6 on the request succeeds over IPv4 in 0.14 s with
  one IPv6 failure counted; IPv4 only against an IPv6-only pin fails naming the setting; every
  address dead counts both families.
- `cargo test -p hs-cli --test outbound_address_policy` (2.3 s): server A named
  `dual-stack.test:{port}`, servers B and C resolve it to `2001:db8::1` then `127.0.0.1`; B
  (default) reads alice's profile from A over federation with no IPv6 failure counted; C
  (`ipv4_only: false`) reads it too, with `hs_outbound_connect_failures_total{family="ipv6"}`
  up by one and `hs_outbound_connections_total{family="ipv4"}` up by one on its `/metrics`.
- `cargo test -p hs-config` (the classification tests, the web fixture), `cargo fmt --all
  --check`, `cargo clippy --all-targets -- -D warnings` on hs-http, hs-config, hs-federation,
  hs-media, hs-modules, hs-appservice and hs-cli; `npm run check` in `web/`.

**Decisions made.**

- The policy is **IPv4 only by default**; the owner asked for it and the demo cluster is the
  reason. An operator with working IPv6 turns it off; the setting is hot.
- The flag is **process-wide** (an atomic in `hs-http`), like the network the process is on;
  every client reads it per new connection, and open connections are kept. Tests that change it
  run their cases in sequence in one test function.
- Fall-back stays hyper-util's; the fix is giving it every address. Nothing reimplements
  connect attempts.
- Synapse has no equivalent setting (Twisted tries every address); the translation table says
  so in its Federation section.

**Next.** Roll the demo and watch the `maunium.net` fetch succeed, and read
`hs_outbound_connect_failures_total` on the pod.

## 2026-10-02 (branch `agent/federation-synapse`): the first attempt at a real Synapse

**Goal.** Federate a Myelin built from this tree with a real Synapse in Docker, both directions,
and record the basic story step by step. Myelin had never talked to a Synapse: every number in the
README and here comes from Complement, Sytest or two Myelins.

**What landed: the harness, not the run.** `tests/federation-synapse/run.sh` (README beside it)
builds `hs`, generates a private CA and leaf certificates, starts `ghcr.io/element-hq/synapse`
(server name `127.0.0.1:8448`, TLS federation listener published on 8448, client API on 8408,
`federation_custom_ca_list`, `federation_ip_range_blacklist: []`, registration open, rate limits
off, `allow_public_rooms_over_federation`), an nginx container `fed-synapse-myelin` that
terminates TLS at `fed-synapse-myelin:8449` on the `fed-synapse` network and proxies to the host's
plaintext `hs` (server name `fed-synapse-myelin:8449`, `custom_ca_certificates`,
`ip_range_blocklist: []`, `x_forwarded: true`), then drives every step below through both client
APIs and writes `results.tsv`. It clean-skips without Docker, curl, jq or openssl, and removes
every `fed-synapse*` container and network on exit.

**Why there is no result column yet.** The session could not pull any image: `docker pull`
(ghcr.io, and the ECR mirror alike) fails with "keychain cannot be accessed because the current
session does not allow user interaction" -- `~/.docker/config.json` names the `osxkeychain`
credential store and the OrbStack context, and a `DOCKER_CONFIG` without a store still goes
through it. No Synapse image was on the machine (only nginx and postgres). The session was then
asked to wrap up for a reboot. So the table records the design and what each step asserts; the
first run is the next session's first action (pull the image from a terminal that can open the
keychain, then `tests/federation-synapse/run.sh`).

| Step | What the script checks | Result |
|---|---|---|
| 1 keys | both `/_matrix/key/v2/server`; each notary's `/_matrix/key/v2/query/<other>` | not run (no image) |
| 2 Myelin joins a Synapse room | join by alias, message each way, pre-join history on Myelin, `/joined_members` agree | not run |
| 3 Synapse joins a Myelin room | the same, mirrored | not run |
| 4 invites, leave/rejoin, kick, ban | invite each way accepted; Myelin user leaves and rejoins; Synapse kicks (seen on Myelin); Myelin bans (seen on Synapse) | not run |
| 4 redaction | Synapse redacts its message in the Myelin room; Myelin's `/messages` shows it emptied | not run |
| 4 typing, receipt | Myelin's `m.typing` and `m.receipt` in Synapse's `/sync` | not run |
| 4 keys, to-device | Synapse `/keys/query` of the Myelin user after a `/keys/upload`; a to-device message Myelin -> Synapse in `/sync` | not run (device/keys routes are `agent/e2ee-sytest`'s if they fail) |
| 4 media | a text upload on each side downloaded through the other's `/_matrix/client/v1/media/download` | not run |
| 4 publicRooms, profile, directory | `/publicRooms?server=` each way; `/profile` of the remote user each way; `/directory/room` of the remote alias each way | not run (`/query/*` is `agent/federation-query`'s if they fail) |
| 5 versions 10, 11, 12 | a room of each version joined in both directions | not run |
| 5 restricted | a Synapse space, a restricted v10 room allowing it, the Myelin user joins through the space with Synapse authorising | not run |

**Decisions.** Server names carry ports (direct connect, no `.well-known`/SRV): Myelin's resolver
is hickory over the system DNS and does not read `/etc/hosts`, and Docker's embedded DNS resolves
`fed-synapse-myelin` only inside the network, which is exactly where Synapse is. TLS in front of
Myelin is nginx in a container rather than stunnel on the host, so the harness has no host
dependency beyond Docker, curl, jq and openssl. Trust is a private CA on both sides
(`custom_ca_certificates` / `federation_custom_ca_list`), not `verify_certificates: false`.

**Left, and whose.** The run itself (06). `hs serve` terminating TLS on a listener (`listeners[].tls`
is accepted and only warned about, `crates/hs-cli/src/serve.rs`), which would remove the nginx
container: hs-cli, track 12/06. Putting the script in a CI leg that has Docker: track 12.

## 2026-10-01 (track 14, branch `agent/complement-remeasure`): the whole federation package re-measured

Complement's `tests` package on an image of `main` at `2a0b362`, twice: **225 / 314 assertions,
50 / 90 tests** (run 8), and 224 / 314, 49 / 90 (run 9); on 2026-09-26 it was 75 / 250 and 14 /
88. 36 tests went FAIL -> PASS and none PASS -> FAIL: invites, leaves, knocks and the `send_*`
checks (13), restricted joins (8, plus two NoCreators that won their race), `/hierarchy` (5),
media (3), typing, presence and device lists across servers (3), version 12 (2). The two extra
tests are Complement's (its 2026-09-30 checkout split one `TestMSC4311*` test into three, all
failing). The restricted-rooms, invites and knocks set of the fourteenth session is 17 / 18 in
both runs (`TestRestrictedRoomsLocalJoinNoCreatorsUsesPowerLevelsV11` lost the race both times).
Between the two identical runs only `TestKnockRestrictedRoomsLocalJoinNoCreatorsUsesPowerLevelsV11`
moved: the race of the fourteenth session's item 3. What is left, by family: `/timestamp_to_event`;
version 12's MSC4289/4291/4297/4311 tests (14); `/get_missing_events`, auth chains and outbound
`/send` (7); thumbnails and a filename-less remote download (4); device lists, key upload and
to-device over federation (4); profile queries (2); server ACLs (2); `/room_summary`, the notary
`/_matrix/key/v2/query`, Unicode remote aliases, Complement's appservice user (4). Every name,
the families and how it was run: status 14, session 6; the baseline is
`docs/status/complement-federation-results.txt` (run 8).
## Eighteenth session (2026-10-02, branch `agent/federation-query`): the query API, and what Sytest's federation files really fail

**Stopped early: the owner rebooted the machine.** Everything below builds and its crate tests
pass (`cargo test -p hs-auth --lib`, `-p hs-room --lib` (171), `-p hs-federation` (214+),
`-p hs-cli --lib`, `--test federation_reads`, `--test federation_writes`,
`--test federation_two_servers`; `cargo clippy` clean on the touched crates; `cargo fmt --check`).
**Not yet re-measured on Sytest**: the branch's bookworm `hs` build was killed at the reboot
notice, so every count here is the baseline, run this session with the federation files on
`main`'s binary (`tests/50federation/*.pl`, `tests/30rooms/05aliases.pl`,
`tests/30rooms/70publicroomslist.pl`; `docs/status/sytest/2026-10-02-federation-query-baseline-results.txt`
and `-summary.txt`): **88 of 130, 42 failing** (13 of them timeouts, 9 "Unexpected response from
/send"). A directory argument to `run-tests.pl` is ignored; the files must be listed.

**Measured 2026-10-04** (status 14 session 8: the whole suite on merged `main` `a9f62fc7`, quiet
machine, `docs/status/sytest/2026-10-04-results.txt`), the same 130 tests: **108 of 130, 22
failing** (4 timeouts, 9 "Unexpected response from /send"). The query API is 5/5 (`10query-profile.pl`
2/2, `11query-directory.pl` 2/2, "Non-numeric ports in server names are rejected"), `36state.pl`
7/18 -> 13/18, `35room-invite.pl` 12/14, `34room-backfill.pl` 4/5, `37public-rooms.pl` and
`40publicroomlist.pl` 1/1 each, `52soft-fail.pl` 2/3 (both inbound soft-fail tests pass; "accepts
a second soft-failed event" fails on the prev_event ids), `50server-acl-endpoints.pl` 11/11.
**Still failing, by family:** the nine "Unexpected response from /send" are the `/state_ids`
fallback of item 6 (`36state.pl` x5, `50no-deextrem-outliers.pl`, `33room-get-missing-events.pl`,
`34room-backfill.pl`'s cross-room back-pagination, `31room-send.pl`'s wrong-room auth_events);
`40devicelists.pl` 3 timeouts (resync after leave and rejoin, remote server down, a missed
update; with 08); invalid JSON for room version 6 is answered `401` instead of `400` in
`send_join`, `/invite` and `send_leave` (3: the request is refused before its body is judged;
the server's `info` log has no line for it, so which check answered is still to be read);
erased users' events over federation (`32room-getevent.pl`), the cross-room redaction
(`39redactions.pl`), ephemeral messages (`31room-send.pl`), "New federated private chats get
full presence information" (`44presence.pl`), "Can delete canonical alias" and "Can paginate
public room list" (hs-room).

Done, each with a Rust test that fails without it, in commit order:

1. **The query API** (`fqu` 1/4 -> expected 4/4). `GET /profile/{userId}[/field]` for a user of
   another server asks that server's `/query/profile` (`hs_auth::state::RemoteProfileSource`,
   installed by `hs serve` from `hs_cli::remote_profile` over the federation client; its `404` is
   a `404`, an unreachable server `502 M_UNKNOWN`). `GET /directory/room/{alias}` for another
   server's alias goes through `RemoteJoin::resolve_alias` (also "Remote room alias queries can
   handle Unicode"). Inbound `/query/profile` answered `{}` for every user; it answers the stored
   display name and avatar (`hs_cli::federation::ServerQuerySource::profile`). An X-Matrix
   `origin` that is not a server name (`localhost:http`) is `400 M_INVALID_PARAM`, not `401`
   ("Non-numeric ports in server names are rejected"). Real-binary test:
   `federation_two_servers::a_remote_users_profile_and_a_remote_alias_are_asked_of_their_server`.
2. **Canonical JSON before the signature**: `send_join`/`send_leave`/`send_knock` and a received
   invite check the event against the room version's canonical JSON first (a float in a v6 room
   is `400 M_BAD_JSON`; it was `403`); an invite with no signature from its sender is `403`; an
   invite answered by the invitee's server with a float is `400 M_BAD_JSON` to the inviting
   client (`OutboundJoinError::NotCanonicalJson`). Five Sytest tests.
3. **Backfill from events that are not the room's answers no events** ("Backfill checks the
   events requested belong to the room").
4. **The public room directory**: the federation `/publicRooms` lists the rooms published to the
   directory with the client-server entry shape (it listed `hs-user`'s join-rule proxy:
   `RegistryRoomSource::new` lost its `directory` argument, as did `build_mount`);
   `GET`/`POST /publicRooms?server=` asks that server over federation
   (`RemoteJoin::public_rooms`). `/state` and `/state_ids` at a rejected event are `404`.
5. **An event citing a rejected event as its prev event** is read at the state before the
   rejected one everywhere (`/state[_ids]`, the history-visibility check every client read
   applies, `replaced_state_event`): it was a `404` to federation and invisible to `/sync`, which
   is why the six rejected-event tests timed out.
6. **In progress -- the `/state_ids` fallback for a missing prev event** (`hs-room`,
   `actor::fetched_state`, done and tested; the federation side not written). The room side:
   `RoomActor::accept_prev_event_with_state(prev, state_before_ids, fetched)` holds a missing prev
   event as an outlier with the state another server answered for it (every fetched event
   authorised against its own auth events, a refused one stored rejected and left out; a durable
   `state_snapshots` row, read back on load), and an event citing it is then accepted ordinarily
   and fed to the state store with its resolved state (`PersistKind::AfterFetchedState`), keeping
   the old extremities. `authorize_remote_at` takes a `StateBefore` (prev events, explicit, none).
   **Left**: in `hs-federation`, `AncestorFetcher::{fetch_state_ids, fetch_event, fetch_state}`
   (the client has `state_ids`, `event`, `room_state`), `RoomWriteSink::{knows_event,
   accept_prev_event_with_state}`, and a `resolve_through_state` step in `inbound.rs` after
   `resolve_missing_ancestors` gives up: for each prev event of the received event the sink lacks
   (at most ~5), `/state_ids` at it, `/event` for what is missing, then the sink; then retry the
   event. Sytest's server answers `/backfill` with `404`, which is why nine tests fail with
   "Unexpected response from /send: missing ancestor ... HTTP 404"; this closes those, the two
   outlier `/state` tests, "Forward extremities remain so ...", "Outbound federation requests
   missing prev_events and then asks for /state_ids ...", "Federation handles empty auth_events
   in state_ids sanely" and "Should not be able to take over the room ..." (the room-side test for
   that one passes).

Still failing on the baseline and not touched: soft failure (3; needs a soft-failed flag in
`hs-model`, hs-room keeping such events out of every client read, sync included), device lists
(5, `agent/e2ee-sytest`), ephemeral messages (MSC2228), erased users' events redacted over
federation, the cross-room redaction, "New federated private chats get full presence
information", and the two `30rooms` tests "Can delete canonical alias" and "Can paginate public
room list" (hs-room, not federation).

Known-gaps rows: none of key server `{keyId}`, server ACLs, v1/v2 rooms, rejected PDU `{}` were
touched this session (all four were closed in the sixteenth and seventeenth sessions).

## Seventeenth session (2026-10-01): the rows Sytest's second run left

**Branch:** `agent/federation-sytest-2`, from `agent/federation-sytest` (`8a01fb5`). Closes the
ten rows the sixteenth session's "found with no row" list became in `docs/next-steps.md`'s
known-gaps table, and two things found on the way.

1. **A redacted event says what redacted it, everywhere a client reads it** (`hs-room`). A new
   `hs_room::actor::redactions` module: applying a redaction
   (`RoomActor::apply_redaction_by(target, redaction)`) writes the redaction's PDU and ID into
   the target's `unsigned` -- `redacted_because`, `redacted_by` -- in the store and in memory
   (covered by neither hashes, signatures nor the reference hash, so the event and its ID are
   unchanged), and `routes::render::client_event_json` renders them as a client event. Every
   client read goes through that renderer (`/sync`, `/messages`, `/event`, `/context`,
   `/state`, search, relations, threads, appservice delivery), so all of them show it. The first
   redaction to take effect stays the one named. An event redacted before this (no redaction
   named in it) gets the first one held, in memory, on load. A redaction of room version 11 or
   later (where `redacts` moved into `content`) is rendered with `redacts` at the top level too,
   as Synapse does. Federation never sees either field: `hs_cli::federation::full_pdu` now serves a
   redacted event in its redacted form (it served it whole), which keeps no `unsigned`.
   **And `/messages` from a `/sync` token left out the newest event**: the sync-token resolver
   answers the position of the newest event the sync delivered, and a backward page is
   exclusive of its `from`; a backward page now starts one above it
   (`routes::query::get_messages`). That is why the receiving server's `/messages` "did not
   start with the redaction" in Sytest; it was every room's newest event, not a federation bug.
2. **A member below the redact level could not redact their own message in a room of version 1
   or 2** (`hs-room`, `pipeline::build_and_authorize`). Those versions' auth rules allow a
   redaction whose event ID names the same server as the redacted event's; the redaction's ID
   was minted *after* the auth check (`event_id: None`), so the rule never matched, locally as
   much as on the remote side. The ID is now minted first and handed to the check.
3. **`send_join`'s `auth_chain` is the auth chain of the state** (`hs-cli`,
   `federation::auth_chain_from`): every event reached through an `auth_events` edge, a state
   event included when another one cites it. The walk left every state event out, so a room
   whose auth events were all current state answered an empty chain.
4. **`/event` and `/backfill` answer a `Transaction`** (`hs-federation`, `read_routes`):
   `origin` (this server), `origin_server_ts`, `pdus`. They answered `pdus` alone. The PDUs are
   served as stored (a stored PDU's own `origin` is kept; the current PDU formats have none to
   add), and `/state` and `/get_missing_events` keep the spec's own shapes, which have no
   `origin`.
5. **`make_join` refuses a user of another server than the asking one** (`403 M_FORBIDDEN`, as
   `make_leave` and `make_knock` already did; `join::check_user_is_from_origin`) **and a room no
   user of this server is in any more** (`404 M_NOT_FOUND`, "not an active room on this
   server", Synapse's answer; `JoinError::NotInRoom`). Both logged at `info`.
6. **Joins through Sytest's own server** (`hs-federation`, `hs-cli`, `hs-room`). Three causes:
   a `make_join` answer without `room_version` was refused, where the spec says it means version
   1 or 2 (Synapse reads "1"); a resident that does not answer the v2 `send_join` (Sytest's
   answers `404`) was never asked v1 -- `outbound_join::put_v2_falling_back_to_v1` now asks v1
   on a `404` or `400 M_UNRECOGNIZED` and unwraps its `[200, {...}]`, for `send_join` and
   `send_leave`, logged at `info`; and another server's client error was a `502 M_UNKNOWN` to
   the client -- it is now passed through as it came (status, `errcode`, `error`, the rest of
   the body, e.g. `M_INCOMPATIBLE_ROOM_VERSION`'s `room_version`; new
   `RoomError::RemoteRefused`), and not asked of the next server, as Synapse does, except
   `M_UNABLE_TO_AUTHORISE_JOIN`, which still moves on to the next server (and is still a `502`
   when none can).
7. **A redaction that arrives before its event** (`hs-room`, `actor::redactions`). Every
   redaction held is indexed by the event it names (`RoomActor::redactions_of`), rebuilt from the
   stored redactions on load -- as durable as the redactions, no table of its own. When an event
   is stored (`/send`, backfill, a gap fill), the redactions waiting for it take effect under the
   rule a received redaction always had (sender on the original sender's server, or
   `may_redact`), logged at `info`. A local redaction of an event not held is now sent and waits
   (it was sent and then answered `404`).
8. **Server ACLs on EDUs** (`hs-federation`, `acl::filter_edu`, MSC4163 as Synapse): an
   `m.typing` for a room whose ACL denies the origin is dropped, and so is that room's part of
   an `m.receipt`; counted under `hs_federation_acl_refusals_total{endpoint="typing"|"receipt"}`
   and logged at `info`. One verdict per room per transaction, shared with the PDUs.
9. **An auth-rejected PDU is stored as rejected** (`hs-room`, `actor::rejected`): flagged, no
   timeline position, a row in `Tables::outliers` (no new keyspace) so a load finds it, not fed
   to the state store, hidden from every read (`event_by_id`; federation's `/event` answers 404
   for it, as Synapse does). Sent again it is already known (`{}`); cited as a prev event it
   stands for its own prev events (the state after a rejected event is the state before it), so
   the citing event is placed instead of meeting "missing ancestors" and a fetch of the same
   rejected event; cited as an auth event it makes the citing event rejected too. Logged at
   `info` with the reason.
10. **The notary's held responses are kept** (`hs-federation`, new `key_store`):
    `RemoteKeyCache::with_store` writes every response it accepts under each key it lists
    (keyspace `hs_federation.held_key_responses`, one row per `(server, key id)`, overwritten by
    the next response listing that key) and starts from what is held -- each verified again,
    oldest first; one that no longer verifies, or expired over a year ago
    (`key_store::MAX_HELD_AGE_MS`), is forgotten. A restarted notary answers for a server that
    is down, and keys verify without a refetch. `hs-cli`'s `build_mount` uses it; boot logs
    "restored the key responses held for other servers" (`restored`, `forgotten`).
11. **Found on the way, no row: a received event whose content hash fails is taken redacted**
    (`hs-federation`, `inbound::verify_pdu`), as the spec says ("the event is redacted before
    processing further"); it was refused. The signature is checked first, over the redacted
    form as before, so an event nobody signed is still refused. Sytest's "Inbound federation can
    receive redacted events".

12. **Also on the way:** `send_join` answers an unsigned or badly signed join, and a server
    submitting another server's user's join, `403 M_FORBIDDEN` (it was `400 M_BAD_JSON`; the
    sender/origin check now comes before the path's event ID is compared). `PduError` became a
    struct (`message`, `unsigned`) so a caller can tell a signature failure from a malformed
    PDU.

Nothing here needs a setting.

**Interfaces changed (additive unless said).** `hs_room::actor::RoomActor::{apply_redaction_by,
redactions_of, is_rejected_event}`; `hs_room::actor::redactions::{redacted_by, REDACTED_BY,
REDACTED_BECAUSE}`; `RoomActor::event_by_id` hides rejected events as it hid purged ones;
`RoomActor::redact_txn` no longer fails for a target not held; `hs_room::RoomError::RemoteRefused`
and `hs_room::error::RemoteRefusal`; `hs_federation::key_store` (`HeldKeyStore`,
`KvHeldKeyStore`, `InMemoryHeldKeyStore`); `RemoteKeyCache::with_store`;
`hs_federation::acl::filter_edu`; `hs_federation::join::{check_user_is_from_origin,
JoinError::NotInRoom}`; `hs_federation::inbound::PduError` is now a struct (breaking for anyone
reading `.0`; nothing outside the crate did); `verify_pdu` returns a hash-failing event redacted
instead of an error.

**Verified.** Every test named here fails without its change (checked by reverting it for
the `hs-room` ones; the others assert what the old code demonstrably answered: an empty chain,
`pdus` alone, a template, a refused template, `400`, an error).

- `cargo test -p hs-federation` (210): new `read_routes::tests::event_and_backfill_answer_a_transaction_from_this_server`,
  `transport::join::tests::{make_join_for_a_user_of_another_server_is_forbidden,
  make_join_for_a_room_this_server_has_left_is_not_found}`,
  `join::tests::send_join_refuses_an_unsigned_or_badly_signed_join_as_forbidden` (and the
  wrong-server test extended to a replay under another path),
  `outbound_join::tests::a_template_without_a_room_version_and_a_resident_without_v2_send_join_still_join`
  (a stand-in for Sytest's server: no `room_version`, v2 `404`, v1 `[200, ..]`),
  `outbound_membership::tests::an_invite_goes_by_v1_to_a_server_without_v2`,
  `inbound::tests::typing_and_receipts_for_a_room_whose_acl_denies_the_origin_are_dropped`,
  `inbound::tests::verify_pdu_takes_an_event_whose_hash_fails_redacted` (was
  `verify_pdu_rejects_a_tampered_body`), `invite::tests::a_tampered_invite_does_not_verify_and_a_changed_one_is_taken_redacted`,
  `keys::tests::held_key_responses_survive_a_restart_and_long_expired_ones_are_forgotten`.
- `cargo test -p hs-room` (lib 140, all integration files pass): new
  `actor::tests::a_member_redacts_their_own_message_in_rooms_of_version_1_and_2` (failed with
  "m.room.redaction event did not pass any of the allow rules" with the ID minted late),
  `a_redaction_that_arrives_before_its_event_takes_effect_when_the_event_comes` (across two
  reloads; failed at "the waiting redaction took effect" with the application disabled),
  `a_local_redaction_is_named_in_the_event_it_redacts_and_the_first_one_stays`,
  `a_rejected_event_is_stored_as_rejected_and_later_references_to_it_are_consistent` (failed
  at `is_rejected_event` with the store disabled), `routes::query::tests::a_backward_page_from_a_sync_token_starts_with_the_newest_event_it_delivered`.
- `hs-cli`: `--lib` (183, new `remote_join::tests::another_servers_client_error_is_passed_through_to_the_client`);
  `--test federation_reads` (11; new `send_join_answers_the_auth_chain_of_the_rooms_state` --
  `make_join`, sign as the remote, `send_join` v1 and v2 in a version-1 room, the chain closed
  under `auth_events` -- and `make_join_refuses_another_servers_user_and_a_room_this_server_has_left`;
  `/event` and `/backfill` checked for `origin`/`origin_server_ts`); `--test
  federation_room_versions` (5; the redaction test is now three, versions 1, 2 and 11, two
  servers each: bob on B redacts his own message, alice's `/sync` on A gets the redaction, a
  backward `/messages` from that sync's `next_batch` starts with it, then the message with
  `unsigned.redacted_by` and a client-shaped `redacted_because`, and `/event` on both servers
  carries them -- before the session it failed at B's `403` in versions 1 and 2 and at the
  first `/messages` assertion in 11); `--test federation_keys`, `federation_writes`,
  `federation_membership` (12; its restricted-room test still gets a `502` for a join no
  server can authorise), `federation_two_servers`: pass.
- `cargo fmt --all --check`, `cargo clippy -p hs-federation -p hs-room -p hs-cli --all-targets
  -- -D warnings`: clean. Not run: the workspace gate.
- **Sytest, whole suite** (release `hs` on bookworm built from the branch's code before its
  last, clippy-only commit, through `SYTEST_HS_BINARY`, image `myelin-sytest:9cde6e9`;
  `docs/status/sytest/2026-10-01-federation-2-results.txt` and `-summary.txt`):
  **federation 50/105 → 73/105**; the whole suite **448 → 486 of 772** (239 fail, 47 skip).
  `send_join` API 0/9 → 8/9, `make_join` 0/3 → 3/3, room versions 5/7 → 7/7, Federation API
  6/14 → 9/14, Backfill 0/5 → 3/5, Invite 1/10 → 4/10, Public Room 0/1 → 1/1. 41 tests newly
  pass, among them "Can receive redactions from regular users over federation" in all twelve
  room versions, both inbound `send_join`s, all three outbound `send_join`/`make_join`
  failure tests, the two `make_join` refusals, "Inbound federation can return events",
  "Inbound/Outbound federation can backfill events", "Inbound federation can receive redacted
  events", "Inbound federation ignores redactions from invalid servers room > v3", "Outbound
  federation can send invites via v1 API", and (from the `/messages` fix) "Message history can
  be paginated" and "... over federation". The run took eight minutes (35-60 before).
  **Three that passed failed**, all client-server and none federation: "/joined_rooms returns
  only joined rooms", "Events come down the correct room" and "Previously left rooms don't
  appear in the leave section of sync". They fail again run alone, and the cause is in the
  logs: two `createRoom` calls by one user in the same millisecond got **the same room ID**
  (a version-12 room's ID is its create event's hash, and nothing makes two such create events
  differ; `RoomActor::create_placed`). The faster run made them collide. Not this session's to
  fix (track 04's create path, being changed on `agent/cluster-create-room-flake`); a
  known-gaps row is added.

**Left.**

- `may_redact` still reads the current power levels, not those at the redaction.
- Soft failure is still a hard rejection (now a stored one): Sytest's three soft-fail tests.
- A redaction whose target is in another room is stored in its own room and shown there
  (Sytest's "An event which redacts an event in a different room should be ignored"); it never
  touches the other room's event.
- `send_join` rejects invalid JSON for version 6 too late ("Inbound: send_join rejects invalid
  JSON for room version 6": the request's signature fails first); federation profile and
  directory queries (four `fqu` tests); the inbound invite tests that need the legacy
  `/events` (another row); device-list resync (five `fdk`).

## Sixteenth session (2026-10-01): what Sytest's first run found between servers

**Branch:** `agent/federation-sytest`, from `agent/test-infra-gaps` (`a649509`; the Sytest
harness and the redaction fix). Closes five rows of the known-gaps table that the first Sytest
run opened, each in its own commit (hashes are left out: the merge queue rebases them).

1. **The key server** (the branch's first commit). `hs_federation::transport::key_server` is a router fragment
   mounted at `/_matrix/key/v2`, outside the `X-Matrix` layer: `GET /server`, the deprecated
   `GET /server/{keyId}` (the same document; Sytest's federation client asks it first, so 21
   tests never started), and the notary: `POST /query` and `GET /query/{serverName}` (plus the
   old `/query/{serverName}/{keyId}`). The notary answers from `RemoteKeyCache`, which now keeps
   every self-signed response it accepts, as published, per `(server, key id)` -- expired ones
   too -- and co-signs each with this server's key (`wrap_for_notary`); for this server itself
   it answers its own fresh response. A key held valid until `minimum_valid_until_ts` (default
   now) is answered from the cache; otherwise the server is asked again first, and if that fails
   the last response held is the answer. A response that lists another key does not displace one
   held for a key it no longer lists (Synapse 5305). At most 100 servers per query (400
   `M_LIMIT_EXCEEDED` past that). `hs_federation_notary_queries_total{outcome}`.
   `hs_cli::federation::key_server_state` builds it; `serve.rs` mounts it in place of the inline
   handler.
2. **Server ACLs** (second commit). Nothing enforced `m.room.server_acl` anywhere. Now one check,
   `hs_federation::acl::check_origin` (the room's ACL from the new
   `RoomDataSource::server_acl`, overridden in `hs-cli` with one state lookup), applied two
   ways: as a route layer (`acl::enforce_on_room_routes`) over both federation routers, so every
   route whose path names a `{roomId}` -- `make_join`, `send_join` v1/v2, `make_leave`,
   `send_leave` v1/v2, `make_knock`, `send_knock`, `invite` v1/v2, `state`, `state_ids`,
   `backfill`, `event_auth`, `get_missing_events`, `hierarchy`, `timestamp_to_event` and the
   seams -- answers `403 M_FORBIDDEN` before its handler for a denied server, and a route added
   later is covered without anyone listing it; and per PDU in `/send` (once per room per
   transaction, before verification, `{"error": ...}` under the event's ID). `is_allowed` now
   matches the host without its port (the spec's "excluding any port information"; Sytest bans
   `localhost:<port>` as `localhost`), and `acl_from_content` reads a malformed ACL as Synapse
   does. Counted in `hs_federation_acl_refusals_total{endpoint}` (a fixed label set) and logged
   at `info`.
3. **A rejected PDU is `{}` in `/send`** (third and fourth commits). `WriteRejected::auth_rejected`
   (`WriteRejected::auth`), set by `hs-cli`'s sink for `RoomError::Forbidden`; `/send` answers
   `{}` for it (logged at `info`) and an error for everything else (unparsable, unplaceable,
   the store failing, missing ancestors not fetched). The event is still not stored.
4. **Rooms of version 1 and 2 over federation** (fifth commit). Three things: `make_join`
   refused to cite events by `[id, {"sha256": reference hash}]` -- it now reads each cited
   event's body through the new `RoomDataSource::event_for_reference` (overridden in `hs-cli`)
   and hashes it; the *joining* side never gave its join an `event_id`, which versions 1 and 2
   carry in the event, so the signed event did not parse (`sign_join_template` now mints
   `$<opaque>:<server>`); and `make_join` without `ver` now means `["1"]`, as the spec says.
   Also: `M_INCOMPATIBLE_ROOM_VERSION` carries `room_version`, v1 `send_join` answers
   `[200, {...}]` as the spec documents (v2 unchanged), and the "unsupported room version"
   message no longer repeats itself.
5. **Received redactions** (`hs-room`, sixth commit). Nothing applied a redaction that
   arrived over federation. `RoomActor::accept_remote_event` now applies a stored
   `m.room.redaction` to its target when the room holds the target and the redaction's sender
   is on the original sender's server (the spec's rule from version 3) or
   `RoomActor::may_redact` allows it (own event or redact power, current power levels); one
   that may not take effect is stored and logged, unapplied. Not for the importer, which applies
   its own.
6. **IDs in outbound request paths are percent-encoded** (seventh commit; found by this
   run, no row). Nothing encoded them: a room-version-3 event ID is standard base64 and
   carries a `/` about half the time, which split the path of `send_join`, `invite`,
   `send_leave`, `send_knock`, `/event`, `/state[_ids]?event_id=` and `/backfill?v=`, and the
   other server answered `404 M_UNRECOGNIZED` (Sytest's version-3 invite passed in one run and
   failed in the next). `client::encode_path_segment` (everything outside RFC 3986 `pchar`;
   `!$@:+=` stay as they are) and `client::encode_query_value` (also `+`, `=`, `&`) are applied
   to every room, event and user ID the client puts in a path or query.

**Verified.**

- `cargo test -p hs-federation` (200): new `transport::key_server::tests` (7: the key-id
  spelling, both notary spellings co-signed and still origin-signed with one fetch for three
  requests, the notary for itself, Sytest's expired-key and must-not-overwrite sequences, an
  unreachable server left out, a malformed query), `transport::tests::every_room_scoped_route_refuses_a_server_the_room_acl_denies`
  (iterates both manifests; every `{roomId}` route 403s with the ACL message and is counted
  under a named endpoint), `inbound::tests::a_pdu_from_a_server_the_room_acl_denies_is_refused`,
  `inbound::tests::a_pdu_rejected_by_auth_is_answered_with_an_empty_result`,
  `acl::tests::{the_port_is_not_part_of_the_match, a_malformed_acl_is_read_leniently,
  endpoint_labels_are_a_fixed_set}`,
  `join::tests::make_join_cites_events_by_reference_hash_in_a_version_1_room`,
  `outbound_join::tests::a_version_1_template_is_given_an_event_id_of_this_servers_making`,
  `client::tests::ids_are_encoded_for_paths_and_queries`,
  `transport::tests::an_event_id_with_a_slash_is_routed_whole_when_encoded`.
  Each fails without its change (the routes and the notary answered 404, the ACL layer let the
  handler run, the auth rejection was an error, `make_join` answered `UnsupportedRoomVersion`,
  `Event::parse` refused the template).
- `cargo test -p hs-room` (all pass): new
  `actor::tests::a_received_redaction_is_applied_only_when_its_sender_may_redact` (a power-0
  member of a third server stores a redaction that changes nothing; the sender's own applies).
- Real binaries: `cargo test -p hs-cli --test federation_keys` (A's `/server/{keyId}` is its
  `/server`; A's notary answers B's keys signed by both; an unreachable server is left out),
  `--test federation_room_versions` (a version-1 room created on A is joined from B and
  messages cross both ways -- failed with "signed make_join event does not parse: missing
  `event_id`" before the joining side's fix; bob's redaction on B empties the message on A),
  `--test federation_writes` (the auth-rejected PDU is now `{}` and `/event` does not find it),
  `federation_room_versions.rs`'s `invites_in_a_version_3_room_reach_another_server_whatever_their_event_ids`
  (six invites into a version-3 room; without the encoding all six succeed about 1 time in 60),
  `--test federation_membership`, `--test federation_two_servers`: pass.
- `cargo clippy -p hs-federation -p hs-room -p hs-cli --all-targets -- -D warnings`, `cargo fmt
  --all --check`: clean. Not run: the workspace gate.
- Sytest, whole suite, on the branch at its redaction commit (release `hs` on bookworm through
  `SYTEST_HS_BINARY`, image `myelin-sytest:9cde6e9`; `docs/status/sytest/2026-10-01-federation-results.txt`
  and `-summary.txt`): **federation 15/105 → 50/105**; whole suite 407 → 448 of 772 (276
  fail, 48 skip). Key API 2/6 → 6/6, Auth 4/20 → 16/20, room versions 0/7 → 5/7, Federation
  API 1/14 → 6/14, State APIs 3/10 → 6/10, `get_missing_events` 0/3 → 2/3, `send_leave` 0/1 →
  1/1. 44 tests newly pass, among them ten of the eleven "Banned servers cannot ..." (the
  eleventh, `/send_leave`, passed before), all four notary tests, the room-version-1 and -2
  joins, backfills and invites, and "Inbound federation can receive events". Three that
  passed in the first run failed in this one: `GET /publicRooms lists rooms` and "Newly left
  rooms appear in the leave section of gapped sync" (client-server, a listing not found and a
  send refused mid-test; not touched here), and "User can invite remote user to room with
  version 3", which is the path-encoding bug below -- it passes or fails by the luck of the
  event ID. The release build in Docker took five hours under the desktop's load, so the
  path-encoding fix (item 6) was not in the Sytest binary.

**Left.**

- A redaction that arrives before the event it redacts is never applied when the event comes;
  `may_redact` reads the current power levels, not those at the redaction.
- No event renders `unsigned.redacted_because` / `redacted_by` (Sytest's "Can receive
  redactions from regular users over federation" checks `redacted_by`, so it still fails in
  every version though the redaction now applies) -- `hs-room`/`hs-user`'s client format, no row
  yet.
- Server ACLs are not applied to EDUs (typing, receipts; Synapse does, MSC4163).
- A PDU the auth rules reject is answered `{}` but still not stored as rejected, so a later
  event citing it meets "missing ancestors"; soft failure is still a hard rejection.
- The notary's held responses are in memory only (lost on restart) and are never pruned.
- Found by the second Sytest run, no row yet: `send_join`'s `auth_chain` is empty for a room
  whose auth events are all current state (the chain leaves out the state events themselves;
  Sytest's "Inbound federation can receive v1/v2 /send_join" want it non-empty); a PDU served
  by `/event` and `/backfill` lacks `origin` and, for one test, `origin_server_ts`
  (`32room-getevent.pl`, `34room-backfill.pl`); `make_join` does not refuse a join for a user
  of another server than the requester's (v1 spelling) nor for a room everyone left; a user on
  the remote side of a version-1 or -2 room is refused (403) when redacting their own
  message; after a received redaction, the receiving server's `/messages` does not start
  with the redaction (Sytest 32room-versions, every version); outbound joins through Sytest's
  own server still answer 502 (`send_join` and `make_join` failure pass-through tests).

## Fifteenth session (2026-09-30): a destination down past its queue is caught up from the rooms

**Branch:** `agent/federation-catchup` (not merged). Closes the known gap "A destination down
for longer than its queue is not caught up from the room".

**What was actually true before.** The gap row said PDUs were not queued for a destination
"already known failing". No such path existed: `FederationSender::enqueue_pdu` wrote every PDU
for every destination, failing or not, and **the queue had no bound at all**. A destination
down for a week grew `hs_federation.outbound_queue` by a week of events, and the worker then
replayed all of it, fifty at a time, in order. What *was* lost was only what never reached the
sender (the feeder's lagged update stream, a crash between persisting and queueing).

**What changed.**

- `hs_federation::outbound_store`: four new keyspaces. `outbound_lengths` (a per-destination
  `atomic_add` counter kept in the same transactions that add and remove rows, counted once at
  open for a store written before it existed); `outbound_room_queued` and `outbound_room_sent`
  (`(destination, room_id) -> seq`: the newest PDU meant for, and accepted by, the destination
  in that room -- Synapse's `destination_rooms` plus `last_successful_stream_ordering`, per
  room); `outbound_catch_up` (`CatchUpMark`: since, reason, the sequence number it was set at).
  `OutboundStore::enqueue` now takes the room and the bound and returns `Enqueued` (queued /
  newly catching up / already catching up); `ack` records sent positions in the same
  transaction; new `catch_up_mark(s)`, `mark_catch_up`, `note_queued`, `rooms_behind`,
  `record_sent`, `finish_catch_up` (clears the mark only if no room is behind, in one SSI
  transaction, so a racing enqueue is either seen as behind or queued normally).
- `hs_federation::sender`: `SenderConfig::max_queued_pdus_per_destination`
  (`DEFAULT_MAX_QUEUED_PDUS_PER_DESTINATION` = 10,000). The PDU that finds a queue full marks
  the destination; from then on PDUs for it only move room positions. The worker sees the mark
  at the top of its loop or between attempts at its head transaction (`Delivery::Superseded`),
  drops the queue in batches of 500 (noting each row's room as behind), then loops: wait out the
  persisted backoff, compute the rooms behind (oldest queued position first, fifty), ask the new
  `CatchUpSource` trait for each room's latest event, make **one** attempt (`Mode::CatchUp`;
  recomputed before the next, so what goes out when the destination returns is what is latest
  then), record the sent positions; when nothing is behind, `finish_catch_up`. EDUs are not
  part of catch-up and go out after it. `resume` starts workers for marked destinations even
  with an empty queue. `FederationSender::{install_catch_up_source, install_catch_up_metrics,
  mark_for_catch_up, catch_up_marks}`.
- `hs_federation::metrics::CatchUpMetrics`: `hs_federation_catch_up_started_total{reason}`,
  `_completed_total`, `_rooms_total`, `hs_federation_outbound_pdus_dropped_total`; no
  destination label (unbounded). `info` logs "destination entering catch-up" (reason, queued
  when marked) and "destination caught up; leaving catch-up" (`rooms`, `events_sent`,
  `elapsed_ms`).
- `hs-config`: `federation.max_queued_pdus_per_destination` (u32, default 10,000, at least 1);
  `docs/config.md` regenerated, the web's schema fixture regenerated.
- `hs-cli`: `federation_sender::RoomCatchUp` (the `CatchUpSource` over the room registry: the
  newest local forward extremity, else the newest local event among the last 200, nothing if the
  destination has no member joined), installed by `OutboundFederation::start`; `build_mount`
  passes the bound; `serve.rs` registers the catch-up metrics next to the EDU ones.
- `hs-admin`: `AdminDestination::catch_up_since` (OpenAPI `Destination.catch_up_since`,
  nullable date-time, additive; `web/src/api/schema.d.ts` edited by hand in the generator's
  shape). `DestinationStoreSource` lists marked destinations and fills the field.

**Verified.**

- `cargo test -p hs-federation` (184 + doc): new
  `sender::tests::a_destination_down_past_its_queue_bound_gets_each_rooms_latest_event_then_its_queue`
  (five events in two rooms against a bound of three, destination refusing connections; the
  queue is dropped, `!b` moves on while it is down, and when it comes up it gets exactly
  `[!a's latest, !b's latest]` in one transaction, then the next event alone; metrics text
  checked), `sender::tests::a_destination_marked_before_a_restart_is_caught_up_by_the_next_sender`
  (KV store, mark survives, a room the destination left is skipped),
  `outbound_store::tests::a_full_queue_marks_its_destination_and_positions_say_which_rooms_are_behind`
  (both stores), `outbound_store::tests::a_queue_from_before_lengths_were_kept_is_counted_at_open`,
  and the admin-source test extended for `catch_up_since`. The first fails with the bound
  disabled (`left: 5, right: 3` at the queue-length assertion).
- `cargo test -p hs-cli --test federation_catch_up`: two real binaries over TLS; B (process and
  proxy) stopped; alice sends eight messages against `max_queued_pdus_per_destination: 3`; A's
  admin row shows `catch_up_since`, `failing_since` and nothing pending; B restarts over its data
  directory; bob's `/sync` gets the eighth, A logs `rooms=1 events_sent=1` leaving catch-up, the
  row clears, bob's `/messages` has all eight in order (B fetched the seven from A), and a
  ninth message goes the ordinary way. 22-30 s. With the bound disabled it fails after 60 s at
  the admin row (`"pending_pdu_count":8,"catch_up_since":null`).
- `cargo test -p hs-cli --test federation_sender` (new
  `catch_up_sends_the_latest_local_event_and_nothing_after_the_destination_left`),
  `--test federation_restart`, `--test federation_two_servers`, `--test federation_edus`: pass.
- `cargo test -p hs-admin`, `cargo test -p hs-config --lib federation` and the
  `web_schema_fixture` test: pass. `cargo clippy -p hs-federation -p hs-cli -p hs-admin -p
  hs-config --all-targets -- -D warnings`, `cargo fmt --all --check`: clean. Not run: the
  workspace gate and `npm run check` (the web change is one optional generated field).

**Left.**

- An event the feeder never handed to the sender (`RecvError::Lagged`, a crash between
  persisting and queueing) moves no room position, so catch-up does not know of it; it reaches a
  destination only as an ancestor of a later event. Closing it needs the feeder to say which
  rooms it lost (or a per-room "last event handed over" cursor), then
  `FederationSender::mark_for_catch_up` plus `note_queued` does the rest.
- The web interface does not show `catch_up_since` (track 16; the field is in the generated
  types).
- Catch-up in a cluster has not been run on two replicas: marks and positions are in the shared
  store and only the destination's shard owner has a worker, so it should hold, but nobody has
  watched it.
- The per-destination room position rows are never pruned (one pair per room ever shared with a
  destination, as Synapse's `destination_rooms`).

## Fourteenth session (2026-09-30): Complement remeasured, and a state-resolution tie-break

**Branch state:** `agent/federation-complement`, on top of `agent/federation-leftovers` at
874e696 (271ebb0 plus the two join-route test fixes). Not merged.

**Measured (Complement, image built from 271ebb0, targeted set `TestRestrictedRooms*`,
`TestFederationRoomsInvite`, `TestKnocking*`, `TestKnockRooms*`, `TestFederationRejectInvite`;
18 top-level tests, 98 counting subtests, `go test -count=1 -p 1` on the `tests` package):**

| Run | Image | Top-level | With subtests | Failing |
|---|---|---|---|---|
| 1 | 271ebb0 | 12/18 | 92/98 | `RemoteJoinFailOver`, `RemoteJoinFailOverInMSC3787Room`, `SpacesSummaryLocal`, `SpacesSummaryFederation`, `NoCreatorsUsesPowerLevelsV11`, `V12` |
| 2 | 271ebb0 | 13/18 | 93/98 | as run 1, without `RemoteJoinFailOverInMSC3787Room` |
| 3 | 271ebb0 + the fix below | 14/18 | 94/98 | `SpacesSummaryLocal`, `SpacesSummaryFederation`, `NoCreatorsUsesPowerLevelsV11`, `V12` |
| 4 | same | 14/18 | 94/98 | same four |

Every subtest that ran passed in every run (80/80); the 09-28 numbers (14/18, 94/98) counted
top-level tests among the "subtests", and so do these. The `RemoteJoinFailOver` pair moved
between runs (5 passes in 10 attempts before the fix, 10 in 10 after: runs 3 and 4 plus three
runs of the pair alone). The other four failed identically every time.

**Causes, read from the Complement log and the servers' logs (`RUST_LOG` through Complement's
`COMPLEMENT_SHARE_ENV_PREFIX=PASS_ PASS_RUST_LOG=...`):**

1. `TestRestrictedRoomsRemoteJoinFailOver{,InMSC3787Room}`, flapping: **this server's bug, in
   `hs-state`, fixed here.** The test has charlie (hs3) leave the restricted room 2 ms after
   alice (hs1) changed the power levels, so the leave and the power-levels event both cite
   charlie's join: a fork on every server. hs3 resolved it with charlie *joined* (the join it had
   superseded), so the next `/join` "via hs2, which is expected to fail" was answered `200` in
   1.8 ms by a local join event, without asking hs2 at all (no `remote_join` line in hs3's log;
   a new `m.room.member` queued for federation). `hs_state::state_res::v2`'s `ruma_state_res::Event`
   adapter returned `origin_server_ts` through `u32::try_from(..).unwrap_or(u32::MAX)`: every
   real millisecond timestamp (1.79e12) overflows `u32`, so every real event carried `u32::MAX`,
   and the mainline ordering's timestamp tie-break silently became an event-ID tie-break, that
   is, a coin toss per pair of events. Every unit and property test of the resolver used
   timestamps starting at zero and never saw it. The join and the leave sit at the same
   mainline position (both cite the earlier power levels), so the leave won only when its
   event ID sorted after the join's. Fixed: the adapter passes the whole timestamp
   (`UInt::new_saturating`). `RoomBuilder`'s clock now starts at a real 2026 timestamp, which
   makes the existing oracle-vs-ruma property test fail on the old code, and
   `hs_state::state_res::cross_check_tests::a_later_event_at_the_same_mainline_position_wins_at_real_timestamps`
   is the deterministic case (event IDs chosen to sort the wrong way; fails on the old code
   with `$e9` where `$e10` is expected, in versions 8, 10 and 11).
   `crates/hs-room/tests/remote_join.rs::a_leave_that_races_a_power_levels_change_in_a_restricted_room_is_a_leave`
   covers the same fork through the room actor with a remote-join snapshot and real timestamps
   (its event IDs happen to sort the right way, so it passes on the old code too; it is there
   for the actor path, not as the regression test).
   This affected every room version from 2 up, on every server: any fork whose conflicting
   events shared a mainline position resolved by event ID. Track 02's crate; the change is
   nine lines in `crates/hs-state/src/state_res/v2.rs` plus tests.

2. `TestRestrictedRoomsSpacesSummary{Local,Federation}`: **missing feature.**
   `GET /_matrix/client/v1/rooms/{roomId}/hierarchy` answers `404 M_UNRECOGNIZED`. The
   federation side (`GET /_matrix/federation/v1/hierarchy/{roomId}`, `hs_federation`'s
   `read_routes`, `RoomSource::hierarchy`) exists; the client endpoint (MSC2946: walk
   `m.space.child` from the root, summarise each room the requester may see, ask the
   `via` servers over federation for rooms this server does not hold, `suggested_only`, `limit`,
   `max_depth`, `from` pagination) does not. It is a track 04/05 client route with a
   federation fan-out; a few hundred lines, not started here.

3. `TestRestrictedRoomsLocalJoinNoCreatorsUsesPowerLevels{V11,V12}`: **a race in the test that
   this server loses under load; not a bug.** Alice (hs1) sets power levels giving bob (hs2)
   invite power; the test then, without waiting for hs2, has charlie (hs2) join the allowed
   room and the restricted room, and expects bob to authorise. hs1 queued the power-levels
   event at 44,972 and hs2 acknowledged it at 44,977; hs2 decided charlie's join at 44,974
   ("no user of this server may authorise the restricted join; joining through another
   server"), 2 ms after the client's `PUT` returned, and hs1 then refused `make_join` because
   charlie's allowed-room join (queued by hs2 at 44,974) had not reached it either. Synapse
   passes because its per-request latency is longer than its federation delivery; this server
   answers the client in under a millisecond. Both tests passed on 09-28 on an idle machine and
   failed in all four runs today with the workspace gate running alongside. Nothing on the
   server can make hs2 know about a power-levels event it has not received; the fix is a wait
   in the test.

**Verification:** `cargo fmt --all --check`, `cargo clippy -p hs-state -p hs-room --all-targets
-- -D warnings`, `cargo test -p hs-state` (72 passed) and `cargo test -p hs-room` pass. The
Complement invocation is `logs/0930/run.sh` in the worktree (untracked): `DOCKER_HOST` set to
OrbStack's socket, `DOCKER_CONFIG` pointing at a config with no credential helper,
`COMPLEMENT_SPAWN_HS_TIMEOUT_SECS=120`, `go test -v -count=1 -p 1 -timeout 45m -run '^(TestRestrictedRooms|TestFederationRoomsInvite|TestKnocking|TestKnockRooms|TestFederationRejectInvite)' ./tests/`
in `refs/complement`; the image via `tests/complement/build.sh` with `DOCKER_BUILDKIT=0`.

> **2026-09-30, later (track 04, branch `agent/hierarchy`):** item 2 is closed. The federation
> `GET /hierarchy/{roomId}` now answers the spec's object (`room` with `children_state`,
> `children` as summaries, `inaccessible_children`) instead of `{"children": [raw PDUs]}`, takes
> `suggested_only`, applies the spec's "may see" list to the asking server
> (`hs_room::hierarchy::server_access`: a user of it joined or invited, public or knockable,
> world-readable, or restricted to a room it has a user in) and answers `404` for a root it may
> not see; `RoomDataSource::hierarchy`'s signature changed accordingly (the in-memory fake too).
> `FederationClient::room_hierarchy` is the outbound half, used by `hs_cli::hierarchy` for the
> client-server walk. Details and numbers in status 04, session 11; this section's targeted
> set remeasured on that branch's image is **16/18, 96/98**, with only the two NoCreators
> races (item 3) left.

**Left:** the client `/hierarchy` endpoint (item 2, closed above); a wait in Complement's NoCreators test
or an accepted flake (item 3); the full workspace gate on this branch; the cluster run of the
thirteenth session's item 5.

## Thirteenth session (2026-09-28): what was left after the join

**Branch state (2026-09-28, wrap-up):** branch `agent/federation-leftovers`, not merged. Items 1-5
below are done, plus Complement-driven fixes (knock after knock; a client's
`join_authorised_via_users_server` dropped; only the inviter rescinds an invite over federation;
`make_join` answers 403 when it is in every allowed room; a join asks only the servers the client
named, and the allowed rooms' servers only when it named none, which replaces item 2's
"fall back after `M_UNABLE_TO_AUTHORISE_JOIN`", since Synapse does not and Complement tests that
it must not). Gate: `cargo clippy -p hs-cli -p hs-room -p hs-federation`, `cargo test -p hs-room -p
hs-user`, `federation_membership` (12/12) and `cluster_edus` (on PostgreSQL) pass; the full
workspace gate last ran green before the Complement fixes, except `hs-loadgen`'s `real_client`
(boot timeout under disk load, since fixed on main by 8cc6b92), and has not run on the tip.
Complement (`TestRestrictedRooms*`, `TestFederationRoomsInvite`, `TestKnocking*`,
`TestKnockRooms*`, `TestFederationRejectInvite`): run 1 5/18 top-level, 76/98 subtests; run 2
(before the via change) 14/18, 94/98. Left failing: `TestRestrictedRoomsSpacesSummary{Local,Federation}`
(`/hierarchy` is not implemented) and `TestRestrictedRoomsRemoteJoinFailOver*` (fixed by the via
change, not re-run). Left: merge (rebase, full gate), re-run Complement, cluster run of item 5.

Scope: `docs/next-steps.md` queue item 3's remainder, in order. Each item merged to main on its
own. Touched `hs-room`, `hs-cli` and this crate.

**1. A local user's join to a restricted room names its authoriser itself.** Done.
`hs_room::actor::RoomActor::restricted_join` (new) says what a join by a user who is neither
joined nor invited needs in a `restricted` (v8+) or `knock_restricted` (v10+) room: the allowed
rooms, the first local joined member (by user ID) with the power to invite, and every other
server with such a member. `hs_room::routes::membership::act_join` uses it when the client named
no `join_authorised_via_users_server`: if the user is joined to one of the allowed rooms held
here, the local member is named and the join is made here (what `make_join` does for a user of
another server); if no user of this server may invite, the join goes through the servers whose
users may, then the client's `via` (Synapse's `_should_perform_remote_join`), and the resident's
answer is accepted as an ordinary event of a room held for real
(`hs_cli::remote_join`, `accept_remote_event`, falling back to the resident's state only if the
join cites what this copy has not seen). A user in none of the allowed rooms is refused by the
auth rules, `403`.

- `crates/hs-cli/tests/federation_membership.rs::a_local_user_joins_a_restricted_room_without_naming_an_authoriser`:
  carol on A is refused, joins the lobby, joins the restricted room with alice named, rejoins
  (profile change), leaves both and is refused again; then in a room where only alice may invite
  and bob on B is joined, dave on B is refused, joins the lobby, and his join goes through A,
  which names alice; he speaks. Fails with the fix off (`403 cannot join restricted room without
  join_authorised_via_users_server if not invited`).
- `hs_room::actor::tests::a_restricted_join_names_the_first_local_member_who_may_invite`.

**2. A restricted join nobody asked can authorise goes to the allowed rooms' servers.** Done.
When every server the join was sent through refused it with `M_UNABLE_TO_AUTHORISE_JOIN` (or
MSC3083's `M_UNABLE_TO_GRANT_JOIN`), `hs_cli::remote_join::FederationRemoteJoin` asks the
servers of the rooms the join rules allow, as Synapse does: each allowed room's `via`, then the
servers of its joined members if this server holds it, minus those already asked
(`RoomActor::known_allowed_rooms`, new: from the room's own `m.room.join_rules`, or from the
stripped state a local user's invite or knock arrived with). A `403` from any server is still the
answer and stops the loop. `RoomActor::servers_to_join_through` now answers `Some(vec![])` for a
room held only through out-of-band membership, so such a room is always joined remotely through
the client's `via`, even when nobody can be named (a v12 room ID has no server name, and a knock
by this server's own user has no remote sender).

- `crates/hs-cli/tests/federation_membership.rs::a_restricted_join_nobody_asked_can_authorise_goes_to_the_allowed_rooms_servers`:
  three servers, v12 rooms. C is in the restricted room but not the lobby; bob on B knocks
  through C (which is how B learns the join rules), joins the lobby, then joins the room naming
  only C: C refuses with `M_UNABLE_TO_AUTHORISE_JOIN`, B falls back to A (the lobby's `via`),
  alice authorises, and carol on C sees bob's message. Fails with the fallback off (`M_UNKNOWN
  ... M_UNABLE_TO_AUTHORISE_JOIN`).

**Room version 12 over federation (found by the test above).** A v12 room could not cross
servers at all: an invite's signed event and every event over `/send` were refused (`no
m.room.create event in room state`), because from v12 (MSC4291) the create event is never
cited in `auth_events` and the state they imply must include it anyway; and a remote join's
snapshot was refused because the create event carries no `room_id` (`event $X is for room
<none>`). `RoomActor::accept_remote_event` and the remote-join snapshot check now add the room's
create event to the implied state under `room_create_event_id_as_room_id`, and accept a create
event without `room_id` whose event ID is the room ID (`RoomActor::is_own_hashed_create`).

- `crates/hs-cli/tests/federation_membership.rs::a_version_12_room_is_joined_and_used_across_servers`:
  invite, join through A, a message each way. Fails with the fix off (the invite: `403
  auth-events-implied state rejected event: no m.room.create event in room state`).

**3. The stripped state an invite or knock arrived with stays out of the timeline.** Done.
`hs_room::routes::render::client_event_json` drops `unsigned.invite_room_state` and
`unsigned.knock_room_state` (`STRIPPED_STATE_KEYS`) from every rendered event; `/sync`'s
`invite` and `knock` sections read them from the stored event instead
(`hs_user::sync::stripped_state`), so the invitee still sees the room described before joining.

- `crates/hs-cli/tests/federation_membership.rs::stripped_state_stays_out_of_the_timeline`:
  bob's `/sync` invite and knock sections carry the stripped state; once he is in, his timeline,
  `/messages` and `/event` show the invite and the knock without it. Fails with the fix off.

**4. `make_knock` in a room version without knocking answers `403 M_FORBIDDEN`** (was `400
M_INCOMPATIBLE_ROOM_VERSION`). Done. `hs_federation::join::make_membership` returns
`JoinError::NotAuthorized("room version N does not support knocking")`; the version the knocking
server supports (`ver`) is still checked first and is still a `400`. Synapse's
`on_make_knock_request` answers the same, and the spec's `make_knock` `403` is the room refusing
knocks; the `400` is for a version the knocking server lacks. The knocking server passes the
`403` on to its client as a `403` (it was a `502`, "could not complete the request").

- `crates/hs-cli/tests/federation_membership.rs::a_knock_on_a_room_version_without_knocking_is_forbidden`:
  bob on B knocks on alice's version 6 room on A: `403 M_FORBIDDEN` naming knocking, and A holds
  no membership for him. Fails with the old error (`502`, `M_INCOMPATIBLE_ROOM_VERSION` inside).
- `hs_federation::join::tests::make_knock_needs_a_knock_room_in_a_version_with_knocking` gained
  the version 6 case.

**5. EDUs in cluster mode go through the replica that sends for their destination.** Done.
A replica that takes a user's typing, receipt, presence or to-device request no longer drops the
EDU for a destination whose federation shard another replica owns; it hands it to that replica
over the mesh, which queues and sends it.

- `hs-federation`: `sender::EduForwarder` (new trait), installed with
  `FederationSender::install_edu_forwarder`; `enqueue_edu` calls it for a destination another
  replica sends for (and without one drops and counts the EDU, as before).
  `FederationSender::enqueue_edu_local` (new) skips such a destination instead: for the
  device-list announcer, which every replica runs on the same stream, and for EDUs a peer
  forwarded, which must not be forwarded again.
- `hs-cli`: `cluster::PeerRoutes`, a route-prefix multiplexer, is now the one mesh
  `PeerHandler`; `ClusterHandles::add_peer_handler(prefix, handler)` replaces
  `install_peer_handler` (sync's `SessionPeerHandler` is added for `user.`).
  `edu_forward::MeshEduForwarder` looks up `ownership.owner_of(layout.federation_shard(dest))`
  and sends batches on `federation.edu` through `Forwarder::send_to_peer` (one ordered queue
  and pump task per owner, so to-device messages keep their order; 2 s deadline per batch);
  `edu_forward::EduPeerHandler` queues each with `enqueue_edu_local`. `edu_forward::install`
  wires both in `serve.rs` before `spawn_mesh`; single-node mode installs nothing.
- Observability: `hs_federation_edus_forwarded_total{edu_type,outcome}`, `outcome` one of
  `forwarded`, `failed` (owner unreachable, refused, or no other owner during a handoff) and
  `dropped` (no forwarder) on the replica that took the request, `received` on the owner.
  Debug logs per batch, warn on a failure.
- Delivery is best effort, like any EDU: a batch that fails is counted and not retried.

Tests:
- `crates/hs-cli/tests/cluster_edus.rs::a_to_device_message_sent_through_a_replica_that_does_not_send_for_its_destination_arrives`:
  server A is two `hs serve` replicas (in process, real mesh) on one PostgreSQL, B one embedded
  server. It waits for both replicas to own shards and B's federation shard to settle, then
  alice sends one to-device message through each replica: bob on B gets both, the non-owner's
  `/metrics` says `forwarded` 1 and the owner's `received` 1. Fails with forwarding off (only
  the owner's message arrives). Needs PostgreSQL (`HS_CLUSTER_TEST_POSTGRES_DSN`, default
  `postgres://postgres:hspg@127.0.0.1:5439/postgres`), and prints SKIP without one.
- `hs_federation::sender::tests::an_edu_for_a_destination_sent_from_elsewhere_is_forwarded_unless_local_only`,
  `hs_cli::edu_forward::tests::a_forwarded_edu_round_trips_through_json`.

Not yet run on the Kubernetes cluster (a desktop item, `kubectl` unreachable from this session).

## Twelfth session (2026-09-28): to-device messages and `m.signing_key_update` over federation

Scope: `docs/next-steps.md` section 3's "to-device over federation; `m.signing_key_update`; EDUs
in cluster mode only through the owning replica". Branch `agent/federation-to-device`, off
`origin/main`, built on main's EDU design (`FederationSender::enqueue_edu`,
`FederationState::edu_sink`, `hs_cli::edus`). The superseded `worktree-agent-ab238ddfa2a8532e6`
(a different EDU design, `edu_queue.rs`) was read for reference and not merged. Touched `hs-e2e`
(the to-device hook and the inbound half), `hs-cli` (`edus.rs`, `serve.rs`) and this crate.

**Done and verified by running:**

- **To-device, both directions.** `/sendToDevice` hands each remote server's share to
  `hs_e2e::federation::ToDeviceOutbox` (implemented by `hs_cli::edus::SenderEduOutbox` over the
  sender, never coalesced) as `m.direct_to_device` with a fresh `message_id`, split per user and
  then per device past 65 000 bytes. Inbound, `hs_cli::edus::EduDispatcher` calls
  `hs_e2e::federation::receive_direct_to_device`: sender must belong to the origin, each
  `(sender, message_id)` is delivered once (durable, in `hs_e2e.to_device_txn`), `*` fans out to
  every device. Details in `docs/status/08-e2ee.md`.
- **`m.signing_key_update`.** `hs_cli::edus::DeviceListAnnouncer` remembers, per local user,
  what it last announced (device keys, master and self-signing keys) and sends only the
  difference: `m.device_list_update` per device added, changed or deleted (`deleted: true`,
  new), `m.signing_key_update` when a cross-signing key changed. A cross-signing change used to
  re-announce every device as a device-list update. The first change seen for a user after start
  announces everything, as before.
- **Metrics and logs.** New `hs_federation::metrics::EduMetrics`:
  `hs_federation_edus_sent_total{edu_type}` (counted by the sender when a destination accepts the
  transaction) and `hs_federation_edus_received_total{edu_type,outcome}` (`applied`,
  `duplicate`, `dropped`; counted by the dispatcher). `edu_type` is one of the six handled types
  or `other`. These are the first EDU metrics; they cover every EDU type, not only the new ones.
  Every EDU sent (per EDU, after acceptance) and received (with its outcome) is logged at debug.
  `hs-federation` now depends on `prometheus-client` (already a workspace dependency).
- **Also ported** from the superseded `worktree-agent-a50b12a502b9ab4e5`: a cap of 64 on the
  stripped state kept from a received invite or knock (`stripped::MAX_RECEIVED_STRIPPED_STATE`).
  The rest of that branch is on main already (audited behaviour by behaviour; its remaining
  differences are test scenarios and the `make_knock`-on-an-old-room-version status, 400 here, 403
  in Synapse).
  From the superseded `worktree-agent-ab238ddfa2a8532e6`, one thing main lacked: a device
  deleted in `hs-auth` kept its keys in `hs-e2e` and went on being served; now they are removed
  (status 08). Its remote device-list cache was not ported: main's no-cache design is deliberate
  (`hs_e2e::federation`, "No cache").
  Also from that branch, the real-binary restart test the federation-edus session listed as
  next: `crates/hs-cli/tests/e2e.rs::receipts_and_presence_are_still_there_after_a_restart_of_the_real_binary`
  (stops `hs serve`, starts it over the same data directory, and finds the read receipt and
  presence in an initial and an incremental sync). It passes on main as it is.

Tests (all fail with the fix turned off, checked by editing the code and running them):

- `crates/hs-cli/tests/federation_edus.rs::to_device_messages_cross_between_servers_in_both_directions_once`:
  two in-process servers; alice (A) sends two messages to bob's device (and retries one request),
  bob's `/sync` on B has exactly the two, in order, and nothing more a sync later; bob sends to
  `*` of alice's devices and to himself in one request, alice's `/sync` on A has hers; each
  server's `/metrics` counts the EDUs sent and received. Fails with the outbox not installed
  (`only 0 of 2 to-device messages ... reached`) and with the dispatcher dropping
  `m.direct_to_device`.
- `...::a_cross_signing_key_change_is_a_signing_key_update_on_the_other_server`: alice's device
  keys are announced first; then she uploads a master key; bob's `/sync` on B has alice in
  `device_lists.changed`, B counts an applied `m.signing_key_update`, A counts one sent, and B's
  `/keys/query` returns the new master key. Fails with the `m.signing_key_update` not queued (the
  sync never shows the change).
- `crates/hs-e2e/tests/remote_to_device.rs` (4): one EDU per server with distinct `message_id`s
  and the local share delivered here, no outbox answers 200, an inbound `message_id` delivered
  once (fails with the dedupe off), a forged sender or missing `message_id` dropped.
- Unit: `hs_e2e::federation::tests::{a_small_share_is_one_edu_with_the_message_id_as_given,
  a_share_too_large_for_one_edu_is_split_by_user_then_device_and_each_part_has_its_own_id}`,
  `hs_cli::edus::tests::{the_first_change_seen_for_a_user_announces_everything,
  a_cross_signing_change_is_a_signing_key_update_and_nothing_else,
  only_changed_and_deleted_devices_are_announced_after_the_first_change}`,
  `hs_federation::metrics::tests::edus_are_counted_by_type_and_an_unknown_type_is_other`,
  `hs_federation::stripped::tests::at_most_a_bounded_number_of_received_entries_are_kept`, and
  `sender::tests::an_edu_rides_with_waiting_pdus_and_goes_alone_when_nothing_waits` now also
  checks the sent counter.

**Not done (done since: thirteenth session, item 5): EDUs in cluster mode only through the owning replica.** It does not fit cleanly in
this change, and could not be verified here (no cluster). Today the sender *drops* an EDU for a
destination whose federation shard another replica owns (`FederationSender::enqueue_edu`). That
is right for the device-list announcer (every replica follows the shared stream, so the owner
announces it) and wrong for typing, receipts, presence and to-device messages, which only the
replica that took the request knows about. The fix, for whoever picks it up:
1. `hs-federation`: an `EduForwarder` hook on the sender, called instead of dropping, plus an
   `enqueue_edu_local` (or a flag) for the announcer, which must never forward (every replica
   would forward the same update).
2. `hs-cli`: a mesh route (`federation.edu`) over `hs_cluster::mesh::Forwarder::send_to_peer` to
   `ownership.owner_of(layout.federation_shard(destination))`, whose handler calls
   `enqueue_edu_local`. `ClusterHandles` takes one `PeerHandler` (`OnceLock`), which
   `sync_cluster::SessionPeerHandler` holds, so it needs a small route multiplexer first.
3. A two-replica test with a real mesh, which does not exist in `hs-cli/tests` yet, and a run on
   the cluster (a desktop item).

**Decisions made:**

- The inbound dedupe key is `(sender, message_id)`, stored in the existing to-device
  idempotency table under a pseudo-device, rather than Synapse's `(origin, message_id)` in a new
  table: the sender belongs to the origin (checked), so it is at least as strict, and no new
  keyspace is needed.
- Each part of a split share gets the base `message_id` with a `-n` suffix: a receiver
  delivers each `message_id` once, so parts must differ.
- EDU metrics live in `hs-federation` (not `hs-cli`) so the sender can count what a destination
  actually accepted, rather than what was queued.

## Eleventh session (2026-09-27): createRoom invites, restricted joins, local rejection

Scope: what the tenth session left (its "Not done" items 1, 2, 3 and 5). Branch
`agent/federation-membership-2`, cut from `agent/federation-membership`. Touched `hs-room`
(create-room route, the out-of-band leave, the error text), `hs-cli` (adapter error text, the
cheap `membership_of`, the tests) and this crate. `FederationState` is unchanged (no constructor
edits, so the `agent/federation-edus` merge stays mechanical).

### Where this stopped

**Done and verified by running** (Rust 1.98.1, `CARGO_PROFILE_DEV_DEBUG=0`, all green):

```
cargo fmt --all --check                                                         # clean
cargo clippy -p hs-federation -p hs-room -p hs-cli --all-targets -- -D warnings # clean
cargo test -p hs-federation      # 167/167 (was 165)
cargo test -p hs-room            # lib 79, backfill 4, out_of_room_membership 4, remote_join 8, scenario 12
cargo test -p hs-user            # lib 123, sync_scenario 6
cargo test -p hs-cli             # lib 137, bridge_offerings 3, e2e 25, federation_membership 6/6 (was 3),
                                 # federation_reads 9, federation_restart 1, federation_sender 3,
                                 # federation_two_servers 2, federation_writes 8
```

New tests in `crates/hs-cli/tests/federation_membership.rs` (two in-process servers):

- `a_create_room_invite_list_invites_a_user_of_another_server`: `createRoom` with
  `invite: [bob@B, carol@A]`, `trusted_private_chat`, `is_direct`: A holds both invites when the
  request returns, bob's `/sync` on B has the invite with `is_direct` and the room name, bob
  joins through A and speaks. Mutation-checked: with the actor inviting everyone itself (the
  old path) bob's `/sync` never shows the invite.
- `an_invite_is_rejected_locally_when_no_server_in_the_room_answers`: A invites bob, then A
  shuts down; bob's `POST /leave` answers 200, his `/sync` moves the room to `leave` with his own
  leave (reason kept) in the timeline; a second leave is refused (nothing left to reject); a knock
  through the dead A answers `502` whose text names the knock and not a join. Mutation-checked:
  without the fallback the leave is a `502`; with the old error text the knock message says
  "could not join the room through federation".
- `a_restricted_room_is_joined_through_a_resident_that_authorises_it`: alice's `restricted`
  room (v11) allowing her public lobby. Bob (B) is refused (`403`) before joining the lobby,
  then joins it and the restricted room through A; A's copy of his join names alice in
  `join_authorised_via_users_server`; he speaks. Alice leaves and rejoins **through B**, which
  authorises her with bob (so B keeps and serves the co-signed copy of bob's join, which A checks
  for its own signature). The same the other way round with bob's rooms on B. A room allowing
  only a room A is not in answers bob's join with `M_UNABLE_TO_AUTHORISE_JOIN` (a `502` to the
  client). Mutation-checked three ways: the resident never authorising (`403 cannot join
  restricted room without join_authorised_via_users_server`), the resident not co-signing
  (`no signature from A` on the joiner), the joiner dropping the co-signed copy (A's rejoin
  through B fails on bob's join).
- Unit tests in `hs_federation::join`: `make_join_names_a_local_authoriser_for_a_restricted_room`,
  `make_join_refuses_a_restricted_join_it_cannot_vouch_for` (`NotAuthorized` when this server is
  in the lobby and the user is not; `UnableToAuthorise` when it is not in the lobby).

**What was built:**

- `hs_room::routes::create_room`: with a federation hook, remote invitees are left out of the
  actor's create (`CreateRoomRequest::local_invites_only`, new, defaults to `false`) and invited
  after the room exists through `routes::membership::invite_remote` (the build / `PUT /invite`
  / accept path `POST /invite` uses, now a shared function). They still count as invitees for
  `trusted_private_chat`'s power levels. An invite that fails is logged and the rest are sent;
  `createRoom` still answers with the room ID.
- Local rejection: `RoomActor::build_out_of_band_leave` + `RoomActorHandle::reject_out_of_band`,
  over a new `pipeline::build_out_of_band_leave` (the hash-and-sign half of
  `build_and_authorize` is now `pipeline::hash_sign_and_parse`, shared). The leave is sent by the
  user, cites only their invite/knock as `prev_events` and `auth_events`, depth + 1, and is
  recorded with `accept_out_of_room_membership`; it goes nowhere (Synapse's out-of-band leave).
  `routes::membership::act` falls back to it when `remote.leave` fails for any reason and the
  user's membership is `invite` or `knock`; otherwise the original error stands.
- Neutral text: `RoomError::RemoteJoinFailed` renders "could not complete the request through
  another server: {detail}"; `hs-cli`'s adapter prefixes the detail with the handshake (`join:`,
  `leave:`, `knock:`, `invite:`); `OutboundJoinError`'s messages name no handshake.
- Restricted joins, resident side (`hs_federation::join`): `make_join` (now takes
  `own_server_name`) tries the plain template first; if that is refused and the room is
  `restricted` (v8+) or `knock_restricted` (v10+), it checks the allow list
  (`check_allow_list`: only rooms this server has a member in count; none of them is
  `UnableToAuthorise` -> `400 M_UNABLE_TO_AUTHORISE_JOIN`, user in none is `403`) and names the
  first local joined member for whom the template authorizes. `send_join` (new parameter
  `authorise_with: Option<&SigningKeyPair>`, the transport passes the key `InviteHandling`
  already carries) re-checks the allow list for a join naming one of its users, co-signs it
  (`invite::cosign`), and stores, forwards and answers (`event`, v2) with the co-signed copy.
- Restricted joins, joining side (`hs_federation::outbound_join`): the template's
  `join_authorised_via_users_server` is never overwritten by the client's content; when the
  join names an authoriser on another server, the `event` in the `send_join` answer is required,
  verified (both signatures) and must have the same event ID; that copy is what is kept.
- `hs_federation::inbound::verify_pdu` now requires the authorising server's signature on a
  restricted join (room versions 8+), as the spec's signature rules say
  (`join_authoriser_server`); `verify_pdu_to_authorise` skips it for the one server about to add
  it. This applies to `/send`, backfill and `send_join` state as well.
- `RoomDataSource::membership_of` (new, with a default that reads `state_for_join`); `hs-cli`'s
  `RegistryRoomSource` overrides it with a current-state lookup.

**Not done / next steps, in order:**

1. A local user joining a restricted room on its own server is still refused unless the client
   names an authoriser itself: `hs-room`'s local join does not pick one. Same logic as
   `make_join` (check the allow list against local rooms, name a local member who may invite);
   belongs in `hs_room::actor::membership_action`.
2. A joining server that is refused `M_UNABLE_TO_AUTHORISE_JOIN` by every resident could ask the
   allow list's rooms' servers; Synapse tries the servers it knows in the allowed rooms. Today the
   candidates are only the client's `via` and the room ID's server.
3. The invite and knock stripped state stored in `unsigned` shows up in B's timeline rendering
   of those events (tenth session's item 4).
4. Measure against Complement (`TestRestrictedRoomsRemoteJoin*`, `TestFederationRoomsInvite`,
   `TestKnocking`, `TestFederationRejectInvite`); nothing here has met another implementation.
5. Update `docs/next-steps.md` section 4 and the known-gaps table once this branch and
   `agent/federation-membership` are merged.

**Decisions made:**

- `createRoom` does not fail when a remote invite fails: the room exists and the client needs its
  ID. Logged at `warn`.
- A local rejection happens on any failure of the remote leave (including a `403`), as Synapse
  does, but only for a user whose membership is `invite` or `knock`.
- The authoriser is the first (sorted) local joined member for whom the template passes the auth
  rules, rather than a power-level computation of our own: `hs_state::auth` stays the one
  authority.
- The co-signing key for restricted joins comes from `FederationState::invites` rather than a new
  field, to keep the `FederationState` constructor untouched.

## Tenth session (2026-09-27): invites, leaves and knocks over federation

Scope: `docs/next-steps.md` section 4, "Invites, leaves and knocks over federation are seams".
Branch `agent/federation-membership`. Touched beyond this crate, because a membership change
crosses all of them: `hs-room` (the out-of-band membership entry point, the unpersisted
invite, the routes), `hs-user` (stripped state for a room held only through an invite or
knock), `hs-cli` (the adapters and the integration test). The EDU work running in parallel was
left alone.

### Where this stopped

**Done and verified by running** (all green on this branch):

```
cargo fmt --all --check                                                   # clean
cargo clippy -p hs-federation -p hs-room -p hs-user -p hs-cli --all-targets -- -D warnings   # clean
cargo test -p hs-federation      # 165/165 (was 152)
cargo test -p hs-room            # lib 79, backfill 4, remote_join 8, scenario 12,
                                 # out_of_room_membership 4/4 (new)
cargo test -p hs-user            # lib 123, sync_scenario 6
cargo test -p hs-cli             # lib 137, e2e 25, federation_membership 3/3 (new),
                                 # federation_reads 9, federation_restart 1, federation_sender 3,
                                 # federation_two_servers 2, federation_writes 8, bridge_offerings 3
```

`crates/hs-cli/tests/federation_membership.rs` (two in-process servers, as
`federation_two_servers.rs`):

- `an_invite_from_another_server_reaches_the_invitee_who_joins_through_it`: alice on A invites
  bob on B; A holds the invite when the request returns; bob's `/sync` on B has it under
  `invite` with `invite_state` carrying his own invite, the room's name and create event and
  alice's membership; bob joins with a bare `POST /join/{roomId}` (no `server_name`), which goes
  through A; messages cross both ways.
- `an_invite_is_rejected_by_the_invitee_and_rescinded_by_the_inviter_across_servers`: bob's
  `POST /leave` on an invite to a room B is not in goes through `make_leave`/`send_leave` on A,
  A shows him `leave` before the request returns, his `/sync` moves the room to `leave`; then
  alice kicks a second invite and the leave reaches B over `/send` (B holds only the invite).
- `a_knock_from_another_server_is_accepted_and_one_is_refused`: bob knocks with
  `POST /knock/{roomId}?server_name=A`; A holds the knock; bob's `/sync` has it under `knock`
  with `knock_state` from A's answer; alice invites him (accepting it), the invite replaces the
  knock on B, bob joins and speaks; a second knock is refused with a kick and bob's `/sync`
  shows `leave`.

Mutation-checked: with `invites: None` in `build_mount` all three fail (A's invite answered
`501`); with the remote leave in `hs_room::routes::membership::act` disabled the rejection fails
(`403 no m.room.create event in auth events`: B tried to author the leave itself); with
`out_of_room_ending` always false the refused knock never reaches bob's `/sync`.

**What was built:**

- `hs_federation::join`: `Handshake` (`Join`/`Leave`/`Knock`), `make_membership`,
  `send_membership`; `make_join`/`send_join` are wrappers. The submitted event must carry the
  handshake's membership and be about its own sender. Accepted leaves and knocks are forwarded
  to the room's other servers exactly as joins are.
- `hs_federation::transport::membership`: real `make_leave`, `send_leave` (v1 `[200, {}]`, v2
  `{}`), `make_knock` (requires `ver`, `M_MISSING_PARAM` otherwise), `send_knock`
  (`knock_room_state`), `invite` v1 and v2. A template is only handed to the server of the user
  it is for. The seams for all of these are gone from `transport::seams`.
- `hs_federation::invite`: `receive_invite` (verify, shape checks, co-sign over the redacted
  form, hand to `InviteSink`), `InviteHandling` on `FederationState::invites` (new field; `None`
  answers `501`). `hs_federation::stripped`: the stripped-state list and a sanitizer for what a
  remote hands over. `hs_federation::inbound::verify_server_signature` (split out of
  `verify_pdu`).
- `hs_federation::outbound_membership`: `leave_room`, `knock_room`, `send_invite` (checks the
  returned event has the same ID and the invitee server's valid signature).
  `outbound_join::make_and_sign` is the shared first half of every handshake this server starts.
- `hs-room`: `RoomActor::accept_out_of_room_membership` (+ handle, +
  `RoomRegistry::accept_out_of_room_membership`, which makes a shell room as a remote join
  does): an invite/leave/ban/knock for a local user in a room no local user is joined to, put in
  the timeline with explicit state (current state + the event), superseding every extremity --
  the remote-join persistence path reused. `build_membership_event` (built and signed, not
  persisted), `stripped_state`, `local_user_joined`. `servers_to_join_through` falls back, when
  nobody at all is joined, to the room ID's server and the servers that sent local users their
  memberships (the inviter's). `RemoteJoin` gained default-refusing `leave`, `knock` and
  `invite`. Routes: a user's own leave of a room nobody here is in goes through `remote.leave`;
  an invite of a remote user is built, sent to be co-signed, and the co-signed event is put in
  with `accept_remote_event`; `POST /knock` (both spellings) takes `server_name`/`via`, resolves
  a remote alias, and goes through `remote.knock` when the room is not held or nobody here is
  in it; knock answers `{room_id}` now. `membership::TRANSITIONS`: invite from `knock` (how a
  knock is accepted), kick from `invite` and `knock` (rescinding and refusing).
- `hs-user`: `stripped_state` adds `unsigned.invite_room_state`/`knock_room_state` from the
  recipient's own membership event when the room holds no `m.room.create`.
- `hs-cli`: `FederationRemoteJoin::{leave, knock, invite}`; `RegistryInviteSink` (records the
  invite with `unsigned.invite_room_state`, or nothing when a local user is already in the room,
  since the invite then arrives over `/send`); `RegistryWriteSink` falls back to out-of-band
  recording for a `leave`/`ban` ending a local invite or knock, only from a server
  `servers_to_join_through` names.

**Decisions made:**

- Out-of-band membership is held in a real room actor (shell + timeline event with explicit
  state), not in a side table in `hs-user`: everything downstream (hub, feeds, `/sync`, the
  join-through path, reload) already works off actors and `RoomUpdate`s.
- The invite room state is kept on the stored event's `unsigned`. It is not covered by the
  hashes or the event ID; it is rendered to clients as part of `unsigned` (Synapse did the same
  historically).
- A local user's invite is built, co-signed remotely and only then persisted, so a refusing
  invitee server means no invite (Synapse's order).

**Not done / next steps, in order** (items 1, 2, 3 and 5 were done in the eleventh session,
above):

1. `createRoom`'s `invite` list still persists invites for remote users without `PUT /invite`
   (`hs_room::actor::RoomActor::create_room` calls `membership_action` directly). Fix: in
   `routes::create_room`, route remote invitees through the same build/co-sign/accept path as
   `act`, after the room exists.
2. A leave whose `make_leave` fails everywhere (the invite was rescinded and B never heard, the
   inviting server is gone) fails the client's request. Synapse then rejects locally with an
   out-of-band leave; do the same (a leave event B signs itself, `prev_events` = the invite,
   recorded with `accept_out_of_room_membership`).
3. `RoomError::RemoteJoinFailed` is rendered as "could not join the room through federation"
   for leave, knock and invite failures too; give it a neutral message.
4. The invite and knock stripped state stored in `unsigned` shows up in B's timeline rendering
   of those events; strip it there if a client complains.
5. Restricted joins (`join_authorised_via_users_server`, room versions 8+): not started.
   `make_join` for a restricted room has to pick an authorising local user with invite power and
   `send_join` has to sign the event as the authorising server; about ten Complement tests.
6. Measure against Complement's federation package (`TestFederationRoomsInvite`,
   `TestKnocking`, `TestFederationRejectInvite`, ...) -- nothing here has met another
   implementation yet.
7. Update `docs/next-steps.md` section 4 and the known-gaps table (the seams bullet) once this
   branch is merged.

## Where this stopped (2026-09-27, branch `agent/federation-edus`): ephemeral data across servers and restarts

**Done and verified by running** (all on this branch):

- Receipts and presence are durable. `hs_user::store::UserStore` gained `put_receipt`/`list_room_receipts`
  and `put_presence`/`get_presence` (keyspaces `hs_user.receipts`, `hs_user.presence`);
  `ReceiptRegistry`/`PresenceRegistry` write through and load lazily per room/user. The typing, receipt
  and presence counters are `hs_user::stamp::Stamps` (max(previous+1, unix micros)), so a token from
  before a restart neither hides new data nor re-shows old. Tests:
  `sync::tests::receipts_and_presence_are_in_sync_after_a_new_hub_over_the_same_store` (fails with the
  registries built without the store -- checked), `receipts::tests::receipts_outlive_the_registry_that_recorded_them`,
  `presence::tests::presence_outlives_the_registry_that_recorded_it`, `stamp::tests::*`.
- EDUs both ways: `hs_user::edu` (EduOutbox seam, InboundEdu parsing with origin checks),
  `SessionHub::install_edu_outbox`/`receive_edu` (typing incl. a stop EDU when a typing lapses, m.read
  receipts only, presence on set and on a /sync-driven change). `hs_federation::sender::FederationSender::enqueue_edu`
  (in-memory per destination, 100 per transaction, coalescing keys, gate-respecting),
  `hs_federation::edu::InboundEduSink` + `FederationState::edu_sink`, `/user/keys/query` and
  `/user/keys/claim` real (`transport/keys.rs`), `/user/devices` answers even with device-name lookup off
  (names stripped). `hs_e2e::federation`: RemoteKeys hook so local /keys/query and /keys/claim ask a remote
  user's server (no cache), plus `federation_keys_query`/`federation_keys_claim`. `hs_cli::edus`:
  SenderEduOutbox, EduDispatcher (typing/receipts/presence to the hub, m.device_list_update and
  m.signing_key_update to the device-list stream), DeviceListAnnouncer (polls the e2e stream every 200 ms and
  sends m.device_list_update per device to servers sharing a room), ClientRemoteKeys; wired in `serve.rs`.
- `crates/hs-cli/tests/federation_edus.rs` (two in-process servers): typing, stop-typing, read receipts and
  presence cross A->B and B->A; a device added on B (login + key upload) is in alice's `device_lists.changed`
  on A and A's /keys/query returns it from B, and the reverse. Both pass (76 s, debug build); both fail with
  the inbound EDU sink not installed (checked).
- Commands run green: `cargo fmt --all --check`; `cargo clippy -p hs-user -p hs-federation -p hs-e2e
  --all-targets -- -D warnings`; `cargo test -p hs-user` (136+6), `-p hs-federation` (158), `-p hs-e2e`
  (26 lib + 3 new `tests/remote_keys.rs` + existing), `cargo test -p hs-cli --test federation_edus` (2/2).

**Not run / not done:**

- `cargo clippy -p hs-cli --all-targets -- -D warnings` and the rest of `cargo test -p hs-cli` (e2e,
  federation_two_servers, federation_writes, federation_reads, federation_restart) were not run after the
  hs-cli wiring; `cargo check -p hs-cli --tests` passes. Run them first.
- No real-binary restart test for receipts/presence yet (the in-process server cannot be restarted over its
  data dir); the durability proof is the hub-over-the-same-store unit test. Next: a test in the style of
  `crates/hs-cli/tests/federation_restart.rs` that sets a receipt and presence, SIGTERMs `hs serve`, restarts it
  over the same data dir, and checks an initial /sync.
- (To-device and m.signing_key_update: done 2026-09-28, see the twelfth session.) To-device over federation (m.direct_to_device) is not sent or received. Cross-signing changes go out as
  m.device_list_update, not m.signing_key_update. Device-list changes made while the server was down are not
  announced. EDUs are dropped (not stored) for destinations another cluster replica sends for, so in cluster
  mode a user's typing/receipts/presence reach only destinations their replica owns. Presence is not pushed to
  a server when it newly shares a room. Complement's TestDeviceListUpdates remote halves were not run (laptop).

## Ninth session (2026-09-27): the outbound queue survives a restart, and the sender is shard-gated

Scope, per this session's brief: `docs/next-steps.md` item 4 ("The outbound queue is in
memory"), then its second half (the sender was not shard-gated). Branch
`worktree-agent-a4a1568719279c325`, commits `238c5fe` (the durable queue), `929f453` (the
real-binary restart test) and `60be866` (the shard gate). Ownership this session:
`crates/hs-federation/**`, the sender's wiring in `crates/hs-cli` (`federation.rs`'s
`build_mount`, `federation_sender.rs`, the minimum in `serve.rs`, `Cargo.toml` dev-dependencies,
tests), this file. Nothing else was touched.

### Verified by running

```
cargo fmt --all --check                                                     # clean
cargo clippy -p hs-federation -p hs-cli --all-targets -- -D warnings        # clean
cargo test -p hs-federation                                                 # 152/152 (was 136)
cargo test -p hs-cli                                                        # 124 unit, e2e 25/25,
   # federation_reads 9/9, federation_restart 1/1 (new), federation_sender 3/3 (was 2),
   # federation_two_servers 2/2, federation_writes 8/8
```

- **`crates/hs-cli/tests/federation_restart.rs`,
  `an_event_queued_for_a_server_that_is_down_arrives_after_a_restart_of_the_sender`** (ran five
  times, 4-10s each): two real `hs serve` processes, A and B, federating over TLS. The binary
  does not terminate TLS and outbound federation is HTTPS only, so the test terminates it: a
  private CA and a leaf certificate for `127.0.0.1` from `rcgen`, and a TLS-terminating proxy in
  the test process in front of each server's plaintext listener; each server is named
  `127.0.0.1:{its proxy's port}` and trusts the CA through `federation.custom_ca_certificates`,
  as an operator with a private CA would. Bob on B joins alice's room on A through the client
  API and a message crosses. Then B's proxy is closed (B's port closes; A gets `connection
  refused`), alice sends "said while B was down", and A's admin API
  (`GET /api/v1/federation/destinations`) lists B with `failing_since` set and
  `pending_pdu_count` 1. A is stopped with SIGTERM (its log says the queued PDU is "kept for the
  next start") and started again over the same data directory; its log says "resuming an
  outbound federation queue" naming B, and the admin row shows the same `failing_since` and the
  one pending PDU -- read by a process that never queued them. B's port opens; the message
  reaches bob's `/sync` on B, exactly once; the row shows nothing pending, not failing, a new
  `last_successful_at`.
- **`crates/hs-federation/src/sender.rs`**:
  `what_was_queued_is_sent_by_the_next_sender_over_the_same_store_in_order` (a sender over a
  `KvOutboundStore` on a `MemoryBackend` fails to reach a closed port, records the failure, is
  shut down with three PDUs queued; a second sender over the same backend `resume()`s three,
  the peer comes up on that port, one transaction with the three in order arrives, the retry
  state is clean, the store is empty),
  `a_persisted_backoff_is_waited_out_after_a_restart_and_a_reset_ends_it_early` (a state left
  as "not before an hour from now" holds the resumed worker back; `reset_destination` releases
  it within the poll interval; `last_error` is kept as history),
  `a_backlog_the_sender_was_not_told_about_goes_out_before_what_is_queued_now`,
  `a_destination_another_replica_sends_for_is_stored_but_not_sent_from_here`,
  `losing_a_destination_stops_its_worker_and_leaves_its_queue_in_the_store` (mid-retry against
  a 500ing peer),
  `a_row_written_behind_the_workers_back_is_found_by_the_rescan`; the nine eighth-session
  tests unchanged in what they assert.
- **`crates/hs-federation/src/outbound_store.rs`**: the same four tests against both stores
  (order, ack through a sequence number, per-destination isolation, retry state recorded,
  reset and listed) plus `the_kv_store_keeps_its_queue_and_sequence_across_a_reopen`.
- **`crates/hs-federation/src/admin_source.rs`**,
  `the_senders_persisted_retry_state_is_merged_into_the_row_and_reset_with_it`.
- **`crates/hs-cli/tests/federation_sender.rs`**,
  `only_the_replica_that_owns_a_destinations_federation_shard_sends_to_it`: the real feeder over
  a scripted `Ownership` owning nothing -- alice's message to bob's server is queued and not
  sent, nothing pending here; acquiring the destination's federation shard sends it; releasing
  it stops the worker, and a further message is queued, not sent.

### Written, not verified by running

- Cluster mode with a real multi-replica deployment over PostgreSQL: no cluster here. What is
  proven is the gate's behaviour against scripted ownership and the rescan against an in-memory
  store; that the same rows are seen by two processes through PostgreSQL is `hs-kv`'s contract,
  not re-tested here. The `Lagged` branch of `follow_ownership` (stop, then resume) is written
  and not exercised.
- `KvOutboundStore` against `FjallBackend` is exercised only through the real binary (the
  restart test); its unit tests use `MemoryBackend`.

### What was built

1. **`crate::outbound_store`** (new). `OutboundStore`: `enqueue(destinations, pdu) -> seq`
   (one transaction, one sequence number for every destination), `peek(destination, limit)`
   (oldest first), `ack(destination, through_seq) -> removed`, `queued()` (every destination
   with a queue, at start), `queue_len`, `state`/`states`, `record_failure(destination, error,
   next_attempt_ms)`, `record_success`, `reset`, `durable()`. `KvOutboundStore<B>` over three
   keyspaces: `hs_federation.outbound_queue` `(destination, seq) -> PDU JSON`,
   `hs_federation.outbound_destinations` `(destination,) -> OutboundDestinationState` JSON
   (`failures`, `next_attempt_ms`, `last_error` (truncated to 512 bytes), `failing_since_ms`,
   `last_attempt_ms`, `last_success_ms`), and `hs_federation.outbound_meta` whose `seq` key is
   advanced with `atomic_add` inside the enqueue transaction. `InMemoryOutboundStore` for tests
   and for `FederationSender::new`, exactly as volatile as the sender was before. A row that no
   longer decodes is logged and skipped by `peek`, and removed by the next `ack` past it.
2. **`crate::sender`**: the store is the source of truth, the channels the fast path. Every
   PDU is written before any worker sees it and deleted only when its destination accepted it
   (or this server's policy refused it). A worker first drains what the store holds for its
   destination, then follows its channel; a channel copy of a row it already sent is recognised
   by its sequence number and skipped. `FederationSender::with_store`, `resume()` (workers back
   for every queued destination the gate allows; idempotent), `is_durable`,
   `destination_states`/`destination_state`, `reset_destination`, `set_gate`,
   `stop_workers_not_sent_here`. `SenderConfig` gained `reset_poll_interval` (the slice a
   waiting worker sleeps before re-reading the store for a reset; `BACKOFF_POLL_INTERVAL` = 30s
   by default) and `store_rescan_interval` (`None` by default). The persisted `failures` and
   `next_attempt_ms` are honoured when a worker starts (the doubling carries on from where it
   was, what is left of the wait is waited out). `ClientError::Backoff` (the client's own
   connection-level record) is still slept out in slices and not counted as a failure of the
   transaction. `shutdown()` says whether what is left is kept or lost, by `durable()`.
3. **`crate::admin_source`**: one row per destination from three sources -- the client's
   connection-level records, the sender's persisted retry states, the sender's live queues.
   `failing_since` is the earlier of the two records', `retry_last_at` and `last_successful_at`
   the later, `retry_interval_ms` that of whichever wait ends later. `reset` clears both records
   (and answers `Unavailable` if the sender's store refuses).
4. **`hs-cli`**: `build_mount` opens `KvOutboundStore` on the same backend as everything else
   and builds the sender over it (`store_rescan_interval` 10s when `cluster.single_node` is
   false). `OutboundFederation::start(rooms, sender, own_server_name, ownership, layout)` sets
   a `ShardGate` (ownership of `ShardLayout::federation_shard(destination)`), `resume()`s,
   follows the room stream as before and ownership events through `follow_ownership`
   (acquired federation shard: resume; released or lost: stop the workers no longer sent for
   here; lagged: both). `serve.rs` starts it after `crate::cluster::start`, still before any
   listener is bound.
5. **Tests**: above. `hs-cli` gained four dev-dependencies for the restart test (`rcgen`,
   `rustls`, `tokio-rustls`, `rustls-pki-types`), all already in `[workspace.dependencies]`;
   `Cargo.lock` updated accordingly.

### Decisions made

1. **Two records per destination, merged for the operator.** The client's
   `hs_federation.destinations` (connection-level, every outbound call, already persisted) and
   the sender's `hs_federation.outbound_destinations` (the head transaction's retrying) stay
   separate: a `/send` answered non-2xx must not put the destination's key fetches and backfill
   into backoff, and the client already records connection failures for both. The admin row
   merges them (above).
2. **`last_error` is persisted but not shown.** `hs_admin::model::AdminDestination` has no
   field for it and `hs-admin` is track 10's. See "Interfaces needed".
3. **A transaction re-sent after a restart carries a new `txnId`** (`{start_ms}-{n}`). The
   receiver may see PDUs it already applied; an event it holds is not applied again, so this is
   harmless, and the alternative (persisting in-flight transaction IDs) buys nothing.
4. **What survives is what was queued; there is still no catch-up from the room.** A local
   event the feeder never handed over (a crash between persistence and queueing, or the update
   stream lagging) is not sent. Synapse's `destination_rooms` (last stream position sent per
   destination) is the next step, not this one; said in `crate::sender`'s module docs.
5. **In a cluster a non-owner writes and does not send.** The owner finds the rows on
   `resume()` (start, shard acquisition), when it enqueues something itself for that
   destination (the worker drains the store first), or through the idle rescan every 10s. The
   pending counts an operator sees are per replica: each reports what its own workers hold.
6. **Store calls are synchronous KV transactions from the async worker**, the same shape as
   `crate::destination_store` and the appservice pump's cursor. `enqueue_pdu` holds the queue
   map's mutex across its store write so a PDU is counted pending exactly once (as backlog at
   worker start or as this enqueue).
7. **One sequence counter for the whole store**, so a PDU queued for several destinations has
   one number everywhere and a destination's key order is its send order.

### Interfaces provided

- `hs_federation::outbound_store::{OutboundStore, KvOutboundStore, InMemoryOutboundStore,
  OutboundDestinationState, QueuedPdu, OutboundStoreError}`.
- `hs_federation::sender::{FederationSender::{with_store, resume, is_durable, set_gate,
  stop_workers_not_sent_here, destination_states, destination_state, reset_destination},
  SendGate, SendsEverywhere, SenderConfig::{for_client, reset_poll_interval,
  store_rescan_interval}}`. `SenderConfig` has two new public fields: a struct literal without
  `..Default::default()` no longer compiles (one in `crates/hs-cli/tests/federation_sender.rs`
  was updated).
- `hs_cli::federation_sender::{ShardGate, follow_ownership}`; **`OutboundFederation::start`
  takes two more arguments** (`Arc<dyn Ownership>`, `ShardLayout`).

### Interfaces needed

- **Track 10 (`hs-admin`)**: `AdminDestination` could carry `last_error: Option<String>` (the
  sender persists it; the Federation page would show why a destination is failing) and the
  OpenAPI schema with it. No RFC written: it is one optional field; this note is the ask.
- **Track 03 (`hs-cluster`)**: nothing new; `Ownership::{is_mine, subscribe}` and
  `ShardLayout::federation_shard` are used as they are.

### Environment hazards found (for the integration lead)

- **The shared `target/` cross-contaminates workspace members between worktrees.** Cargo's
  artifact hash for a path crate does not include the worktree path, so when the cluster
  track's worktree added a `peers` field to `hs_cluster::mesh::MeshDeps` and built, my
  `hs-cli` (whose `hs-cluster` sources have no such field) was compiled against their `rlib` and
  failed at `crates/hs-cli/src/cluster.rs:240`. Worked around with
  `touch crates/hs-cluster/src/lib.rs` (a rebuild from my sources); that worktree will meet the
  mirror image on its next build. Any two worktrees whose copies of one crate differ will
  ping-pong like this until they are merged.
- **Disk**: `target/` reached 29G on a disk with about 38G usable and a build died with
  `ENOSPC`; space came back before I removed anything (I deleted nothing), and my builds
  afterwards ran with `CARGO_INCREMENTAL=0` to keep the footprint down.

### Shared dependencies added

None to `[workspace.dependencies]`. `crates/hs-cli` dev-dependencies: `rcgen`, `rustls`,
`tokio-rustls`, `rustls-pki-types` (workspace entries, already resolved in `Cargo.lock`).

> **Integration note, 2026-09-26, last (integration lead): what a server is served.**
> `/backfill` and `/get_missing_events` applied only the room-level gate (a member of the
> requesting server now, or `world_readable`) and served everything whole, so a server whose
> member joined a members-only room yesterday could fetch the whole of last year. Both now serve
> an event the requesting server was not in the room for in its redacted form
> (`hs_cli::federation::pdu_for_server` over `RoomActor::server_may_see`): `joined` needs one of
> that server's users joined as of the event, `invited` joined or invited, `shared` and
> `world_readable` allow anyone past the gate. Redacted rather than omitted, because a hole in a
> batch reads as missing history to the requester and the redacted form still verifies. Also
> `min_depth` on `/get_missing_events`, parsed and applied as a floor. Both tested in
> `crates/hs-cli/tests/federation_reads.rs`.

> **Integration note, 2026-09-26, later (integration lead): the gap-shaped request.** Reading
> `TestGetMissingEventsGapFilling` for why it failed found that Complement's reference federation
> server answers exactly one request when a homeserver receives an event with unknown ancestors:
> `POST /get_missing_events` with `earliest_events` naming the homeserver's forward extremities
> and `latest_events` naming the event just received. It has no `/backfill` handler, and it
> checks both lists. This crate's `backfill::resolve_missing_ancestors` only ever asked
> `/backfill`, so against that server -- and against Synapse, which serves both but is asked the
> gap-shaped one by every other implementation -- the loop could not begin. It asks
> `/get_missing_events` first now (`GapContext`, filled by `inbound::process_transaction` from
> `RoomDataSource::forward_extremities` and the triggering event), and only when that does not
> close the gap does it fall back to the `/backfill` rounds, under the same limits; three unit
> tests cover closed-by-the-first-request, unsupported-then-backfill, and partial-then-backfill.
> Found on the way: `hs_cli::federation::RegistryRoomSource::forward_extremities` answered
> "the newest timeline event", an assumption from before remote joins, forks over `/send` and
> fetched history existed; it reads the actor's real extremity set now
> (`RoomActor::forward_extremity_ids`). Not re-measured yet: the federation package run in
> progress at the time was from the commit before this.

> **Integration note, 2026-09-26 (integration lead):** the other trigger for `/backfill`. This
> crate's `backfill::resolve_missing_ancestors` fetches the missing *ancestors* of an event that
> arrived over `/send`; `hs_cli::backfill::FederationBackfill` (implementing
> `hs_room::backfill::Backfill`) now runs the same `FederationClient::backfill` call on a
> client's behalf -- one batch of a hundred from the oldest event held, against the room's
> server then the other members' servers, every PDU through `inbound::verify_pdu` -- and hands
> the batch to `RoomActor::accept_backfilled_events`, which is the history path rather than the
> ancestor-resolution one: the events are the history, not something newer's prerequisites. One
> lock per room keeps two clients paging the same room from fetching the same batch twice.
> Verified by `crates/hs-cli/tests/federation_two_servers.rs`: 120 messages before the join read
> back in three pages of fifty, down to the create event, nothing in the next incremental sync.
> Not done here: the state at a backfilled event is walked on the room side rather than asked
> for (`/state_ids` would make it exact at each batch boundary), and no auth events are fetched
> for it (`/event/{eventId}`); both are the noted next step in the actor's doc.

> **Integration note, 2026-09-25 (integration lead):** the eighth session's sender (below) and
> track 04's RFC 0015 bootstrap landed together with the piece between them: `POST /join` on a
> room this server does not hold now runs `crate::outbound_join::join_room_with_content` against
> each `via` and hands the verified snapshot to `RoomRegistry::bootstrap_from_remote_join`
> (`hs_room::remote_join::RemoteJoin`, implemented by `hs_cli::remote_join`). The joining side
> also sends `?ver=` with every supported room version on `make_join` (Synapse refuses a joiner
> that does not) and merges the user's profile into the join template. Proven end to end by
> `crates/hs-cli/tests/federation_two_servers.rs` (two in-process servers, plain HTTP, join
> through the client API, messages both ways) and by the TLS script. The sender's "B cannot hold
> the room yet" caveat in section 8 below was true when written and is closed.

> **Integration note, 2026-09-19 (integration lead):** the gap this file describes below as "the
> one gap this session could not close" — no way to persist a newly received foreign event — was
> **closed** by track 04's `hs_room::actor::RoomActor::accept_remote_event`. The fifth session
> closed the next one: the backfill-then-retry loop `MissingAncestors` was reported for but never
> consumed. The sixth session closed the TLS/CA gap Complement's federation run was blocked on,
> fixed a real PDU signature-verification bug it uncovered underneath, and found a second, more
> consequential instance of the same signature bug in `hs-room`'s own outbound pipeline (fixed by
> track 04 since, per RFC-0014). **This (seventh) session put two real, live instances of this
> server in front of each other for the first time — the first genuine "join a room on a different
> server" this workspace has ever run — and found that the pieces the previous six sessions built
> had never actually been assembled: `federation.custom_ca_certificates` had no effect on a real
> `hs serve` process (the config field existed, the client field existed, nothing read the file
> off disk), an IP-literal destination with an explicit port produced a malformed, doubled-port
> URL, and nothing in this workspace could *initiate* an outbound join at all — only answer one.**
> All three are fixed; see "Seventh session" below.

Updated: 2026-09-25 (eighth session -- the outbound sender: this server now sends its own events
to the servers of a room's remote members over `PUT /send/{txnId}`, and a resident forwards a join
it accepts to the room's other servers. In memory only, no EDUs, not shard-gated; see "Eighth
session" below). Previously updated 2026-09-19 (seventh session -- the two-instance session: two real `hs serve` processes,
different server names, federated over real HTTPS with a private CA and real X-Matrix signatures,
for the first time. Found and fixed a `hs-cli` wiring bug that silently no-op'd
`federation.custom_ca_certificates` on every real deployment, a `hs-federation` discovery bug that
broke every IP-literal-with-port destination, and closed the "nothing can initiate an outbound
join" gap with a new `crate::outbound_join` module. See "Seventh session" below). Previously
updated 2026-09-19 (sixth session -- the TLS/CA session: `hs-config`/`hs-federation` gained a real
config surface for trusting a custom CA (matching Synapse's `federation_custom_ca_list`), the
outbound client now uses it, `verify_certificates: false` is now loud, and a real send_join
signature-verification bug Complement found underneath the TLS gap is fixed. See "Sixth session"
below). Previously updated 2026-09-19 (fifth session -- the backfill session: a remote event citing
ancestors this server lacks now gets them fetched, verified and persisted, then the original event
is retried; see "Fifth session: the backfill loop" below). Before that, 2026-09-18 (fourth session
-- the write session: `/send` and the join handshake are real now, not seams; see "Fourth session:
`/send`, `make_join`/`send_join`, and the v2 mount fix" below). Before that, 2026-09-18 (third
session, the mounting session; see "Mounted into `hs serve`" below for what changed then). The
first session wrote the threat model and the plan below but stopped before any crate code existed;
the second implemented items 1-7 of that plan.
`crates/hs-federation` is no longer a placeholder: 136 passing lib tests (up from 124), plus the
`hs-cli` end-to-end suites (8 federation_reads, 8 federation_writes, 2 federation_sender -- new
this session -- and e2e, all passing), `cargo clippy -p hs-federation -p hs-cli --all-targets --
-D warnings` clean (without `--no-deps`: the `hs-http` breakage the seventh session noted is
gone), five fuzz targets that type-check. Read this file before touching `hs-federation` further.

## Eighth session (2026-09-25): the outbound sender

Scope, per this session's brief: build the outbound federation sender. Before it, this server
never sent a locally created event to any other server -- there was no `sender` module, and
`docs/next-steps.md`'s "there is no outbound queue yet" was true. After it, an event a local user
sends in a room with remote members reaches those servers via `PUT
/_matrix/federation/v1/send/{txnId}`. Ownership this session: `crates/hs-federation/**`, the
named parts of `crates/hs-cli` (a new `federation_sender.rs`, `FederationMount`/`build_mount`,
`RegistryRoomSource`, the minimum in `serve.rs`, tests), this file. `crates/hs-room` and the
client `/join` routes were another agent's and were not touched.

### 1. `crate::sender`: `FederationSender`, what is real

`FederationSender::new(client: Arc<FederationClient>, own_server_name)` (or `with_config` with an
explicit `SenderConfig { initial_backoff, max_backoff }`); `enqueue_pdu(destinations, pdu)`;
`pending_pdus()`, `pending_pdus_for(dest)`, `pending_by_destination()`; `shutdown()`. Plus the
`OutboundPduSink` trait (one method, `enqueue_pdu(Vec<String>, Value)`) that `FederationSender`
implements, so the transport server and `send_join` take an `Arc<dyn OutboundPduSink>` and are
tested with a recording sink.

- **One worker per destination**, spawned on the first `enqueue_pdu` naming it, on the current
  Tokio runtime (outside one: logged and dropped, never a panic). It drains its queue into
  transactions of at most `MAX_PDUS_PER_TRANSACTION` (50, the same constant `crate::inbound`
  enforces on receipt), body `{"origin", "origin_server_ts", "pdus", "edus": []}`, sent through
  `FederationClient::send` -- so discovery, TLS/CA trust, `X-Matrix` signing, the per-destination
  concurrency semaphore and the destination-store backoff records all apply unchanged.
- **`txnId` is `{process_start_ms}-{counter}`**: unique across restarts, monotonic within one.
  **A retry reuses the same `txnId`**, so a receiver whose response was lost replays its cached
  answer (`crate::inbound::TransactionStore`) rather than applying the PDUs twice.
- **Retried, in order, until accepted**: a non-2xx status or a connection/discovery error waits a
  doubling delay from `initial_backoff` (1s) capped at `max_backoff` (the client's own
  `max_retry_backoff`, so the two backoffs an operator sees on one destination share a ceiling).
  `ClientError::Backoff { retry_at_ms }` (the destination store's judgement, set by the client on
  connection-level failures) is slept out in slices of at most `BACKOFF_POLL_INTERVAL` (30s), so an
  administrator's reset of the destination (`federation.destinations.reset`) is honoured within
  30s instead of at the end of an hour-long wait. Only `Disabled`/`DomainDenied`/`IpDenied` --
  this server's own policy -- drop the transaction (logged at `error`); everything else retries.
- **A per-PDU `error` in a 200 response is final**: logged at `warn` with the event ID and the
  receiver's reason, not retried. Per-destination ordering is preserved throughout; destinations
  are independent (a failing one delays only its own queue).
- **Nothing is ever queued for `own_server_name`**, whatever a caller passes; duplicates in one
  call collapse to one copy.
- **In memory only, said loudly** in the module docs and here: a restart, a crash or `shutdown()`
  loses every unaccepted PDU, and there is no catch-up afterwards. `PLAN.md` section 5.2 item 6
  (per-destination queues sharded by destination hash, persisting queue state so failover resumes)
  and Synapse's `destination_rooms` catch-up are the target; a `KvBackend`-backed queue is the next
  step, not this one. `shutdown()` logs the count it lost.
- **Not shard-gated**: nothing consults `hs-cluster`. In a cluster this does not duplicate traffic
  by itself (a room's actor is resident on the replica that owns its shard, and only it publishes
  that room's updates), but a persisted, sharded sender will need to own "who sends for this
  destination" explicitly.
- **Not sent yet**: EDUs of every kind (typing, presence, receipts, device-list updates,
  to-device, signing-key updates -- no `enqueue_edu` seam, since one that discarded its argument
  would be worse than none); invites (`/invite` is its own handshake); leaves and knocks against a
  remote resident (`make_leave`/`send_leave`, `make_knock`/`send_knock`).

### 2. Resident-side forwarding of an accepted join

The spec requires the resident that accepts a `send_join` to send the new join event to every
other server in the room -- it is the only way they learn of the new member. `FederationState`
gained `sender: Option<Arc<dyn OutboundPduSink>>`; `RoomDataSource` gained `async fn
member_servers(&self, room_id) -> Vec<String>` (implemented on `InMemoryRoomSource` from
`FakeRoom::joined_servers`, and on `hs_cli::federation::RegistryRoomSource` from
`RoomActor::joined_members()`); `crate::join::send_join` takes two more parameters,
`own_server_name: &str` and `forward: Option<&dyn OutboundPduSink>`, and after a `Stored`
outcome (not `AlreadyKnown` -- a replayed join was forwarded the first time) enqueues the verified
event to every member server except `origin` and itself. Member servers are read after the store.
Every `FederationState` construction site in this crate and `hs-cli` (tests included) carries the
new field.

### 3. `hs-cli` wiring: `crate::federation_sender`

`hs_cli::federation_sender::OutboundFederation::start(rooms, sender, own_server_name)` subscribes
to `RoomRegistry::subscribe_global()` and follows it; `stop()` aborts the task and shuts the sender
down. `serve.rs` starts it right after `build_mount`, before any listener is bound (the stream does
not replay), and `ServeHandle::shutdown` stops it after appservice delivery, through a
type-erased `stop_outbound_federation` closure modelled on `stop_appservice_delivery`.
`FederationMount` gained `pub sender: Arc<FederationSender>`, built in `build_mount` over the same
client as everything else and installed as `state.sender`.

For each `RoomUpdate` whose `sender`'s server is ours, `forward_update` loads the room
(`get_or_load`), reads the event as stored and signed (`event_by_id` ->
`serde_json::from_slice(canonical_bytes())`, the federation form: `hashes`, `signatures`, no
`event_id`), and computes the destinations: the servers of `RoomActor::joined_members_after(event)`
(which exists in exactly the shape needed, so no approximation), plus, for an `m.room.member` with
`membership` `leave` or `ban`, the target's server -- a kicked or banned user's server is not
"joined after" the event, and if that was its last member it would otherwise never hear why its
user is gone (this is the "state before the event" rule Synapse applies, expressed as "after, plus
the removed target"). Our own name is dropped; invite targets are not added (the `/invite`
handshake is not built, and an invitee's server that is not in the room would only answer
"unknown room"). Events whose sender is remote are never re-sent. `RecvError::Lagged(n)` is logged
at `warn` saying exactly what it means: the skipped updates' local events will not be sent to
remote servers, because the sender has no catch-up.

The admin API's Federation page now shows real pending counts:
`DestinationStoreSource::with_sender(sender)` reports `pending_pdu_count` per destination
(and lists a destination with a queue but no backoff record yet). `pending_edu_count` stays zero,
truthfully.

### 4. Tests (all fail, or do not compile, without the change)

`crates/hs-federation/src/sender.rs` (against `hs_testkit::fake_federation::FakeFederationPeer`
on loopback, plaintext via `ClientConfig::scheme`, explicit-port destinations, with an axum layer
in front of the fake recording the `Authorization` header since the fake does not):
`three_pdus_go_out_as_one_signed_transaction`,
`sixty_pdus_split_into_transactions_of_fifty_then_ten_in_order`,
`a_failing_destination_is_retried_in_order_after_waiting` (500, 500, 200: three attempts with
the same `txnId`, elapsed at least 100ms + 200ms, no fourth attempt),
`destinations_are_served_independently`, `nothing_is_ever_sent_to_our_own_server_name`,
`a_destination_in_backoff_is_not_contacted_before_its_retry_time` (a fixed-`retry_at` destination
store), `a_per_pdu_rejection_is_final_not_retried`, `shutdown_stops_the_workers_and_drops_the_queue`,
`backoff_doubles_from_the_initial_delay_and_is_capped`.
`crates/hs-federation/src/join.rs`:
`an_accepted_join_is_forwarded_to_the_other_member_servers_but_not_the_origin`,
`a_replayed_join_is_not_forwarded_again`.
`crates/hs-cli/tests/federation_sender.rs` (a real `RoomRegistry`, the real feeder, sender and
client, a fake peer; `hs serve` itself cannot be told to federate in plaintext, so the task is
tested in isolation as the brief allowed):
`a_local_message_reaches_the_server_of_a_remote_member_and_nothing_earlier_does` (a message from
before the remote member joined, and the remote member's own join, are not in the transaction;
the message after it is, byte-for-byte the stored PDU),
`a_kick_reaches_the_kicked_users_server_and_later_events_do_not` (drives `forward_update` one
update at a time and asserts each event's destinations). Conditions with deadlines, not sleeps.

### Verification

```
cargo fmt --all
cargo clippy -p hs-federation -p hs-cli --all-targets -- -D warnings        # clean
cargo test -p hs-federation                                                 # 136/136
cargo test -p hs-cli --test federation_sender --test federation_writes --test federation_reads
                                                                            # 2/2, 8/8, 8/8
cargo test -p hs-cli --test e2e                                             # 24/24
bash crates/hs-federation/scripts/two-server-federation.sh                  # passes, see below
```

**Live, two real processes** (the seventh session's script, extended with a step 8): after bob's
server B joins alice's room on A, alice posts again; A's feeder logged `queueing a local event
for federation ... servers=1`, A's sender sent it to `127.0.0.1:8449` as
`PUT /_matrix/federation/v1/send/1790383931330-1` over stunnel-terminated TLS with the private
CA and a real `X-Matrix` signature, B's real inbound layer verified it (fetching A's key over the
same TLS) and answered 200 with a per-PDU `unknown room` error -- B cannot hold the room until
RFC-0015 -- which A logged as a final rejection and counted the transaction accepted. The wire
path from a local `/send` on A to a verified transaction on B is proven; delivery into a room on
B is not, and cannot be until track 04's bootstrap API lands. The script now launches both
servers with `RUST_LOG=info,hs_federation=debug,hs_cli=debug` (overridable) so that step can
watch A's log.

## Seventh session: two real instances, federating for real -- and three bugs only that could find

Scope, per this session's brief (`docs/next-steps.md` item 2, sharpened): put two local instances
of this server in front of each other, different server names, real HTTP, real X-Matrix signatures
-- not a public join (no public name or CA on this laptop), but the same code paths. Ownership this
session: `crates/hs-federation/**`, `crates/hs-cli/**`, `docs/status/06-federation.md` only.

### 0. The headline finding: assembling working parts for the first time finds bugs no unit test can

Every one of this session's three bugs (below) was invisible to every existing test in this
workspace -- 120 passing `hs-federation` lib tests, 24 passing `hs-cli` end-to-end tests, all still
green *with the bugs present* -- because every one of them is a seam between two things that had
never both been real at once before: a config file loaded by the actual `hs-cli` wiring (not a
`ClientConfig` built by hand in a test), a destination string shaped like a real deployment might
plausibly use, and a join initiated by an actual second process rather than a synthetic
already-signed event handed to `send_join`. This is the same lesson the fourth, fifth and sixth
sessions each drew independently (a missing `room_id` in a join response, a trailing slash, the
redaction-before-signing bug) -- restated here because it happened a third time, at a different
seam, the moment real assembly was attempted again.

### 1. `federation.custom_ca_certificates` had never worked on a real server

**Root cause.** `crates/hs-cli/src/federation.rs::client_config` -- the one function that converts
`hs-config::FederationConfig` into the `hs_federation::client::ClientConfig` a real `hs serve`
process's `FederationClient` is built from -- listed `verify_certificates`, `request_timeout` and
a few others explicitly, then filled in everything it did not mention with
`..ClientConfig::default()`. `custom_root_certificates` and `trust_os_root_store` were never in
that explicit list, so they silently took their `ClientConfig::default()` values (empty /
`false`) regardless of what `federation.custom_ca_certificates` said in the YAML. The sixth
session built the schema field, the `ClientConfig` field, the PEM-parsing logic, and proved all of
it works with an in-process TLS test that constructs `ClientConfig` directly -- and that test is
exactly why this went unnoticed: it never went through `client_config`, the one place a real
config file's file *paths* get turned into bytes. Confirmed by grep before touching anything:
`custom_root_certificates`/`custom_ca_certificates`/`trust_os_root_store` appeared nowhere in
`crates/hs-cli/src/*.rs`.

**Fixed** in `client_config` (`crates/hs-cli/src/federation.rs`): reads each configured path with
`std::fs::read`, collecting the bytes into `ClientConfig::custom_root_certificates`; a path that
fails to read is logged with `tracing::error!` and skipped, not a fatal boot error (matching
`FederationClient::new`'s own tolerance for a CA entry that reads fine but parses as malformed
PEM). `trust_os_root_store` is copied straight through. Proven by running, not just reading: this
session's two-instance script failed outbound TLS verification against the private CA until this
fix landed, then succeeded.

### 2. An IP-literal destination with an explicit port produced a malformed, doubled-port URL

**Root cause**, found while diagnosing why `federation-join-room` (below) could not even reach a
server whose TLS and registration all demonstrably worked (`curl` against the same address
succeeded). `crates/hs-federation/src/discovery.rs::resolve`'s `ParsedServerName::IpLiteral` arm
(both the direct one and the well-known-delegates-to-an-IP-literal one) set
`tls_server_name: server_name.to_string()` -- the **entire original input string**, e.g.
`"192.0.2.1:8449"` -- instead of just the IP. `crate::client::FederationClient::send_inner` then
builds the request URL as `format!("{scheme}://{tls_server_name}:{connect_port}{path}")`, which for
an IP literal with an explicit port produces `https://192.0.2.1:8449:8449/...` -- a syntactically
invalid authority that `reqwest` simply fails to connect, surfacing as a generic
`ClientError::Request` with no further detail (the actual gap that made this take real
investigation: `reqwest::Error`'s `Display` does not show the malformed-URL cause, only "error
sending request for url (...)"). Every `Hostname` arm already stored the bare host correctly;
`ipv4_literal_with_port_bypasses_discovery_entirely`, the one existing test for this exact input
shape, asserted `connect_port` and `via` but never `tls_server_name`, so the bug shipped invisibly.

**Fixed**: both `IpLiteral` arms in `discovery.rs::resolve` now set `tls_server_name: ip.to_string()`
(the bare IP), matching every other arm's convention. Two regression assertions added to the
existing tests (`ipv4_literal_with_port_bypasses_discovery_entirely`,
`well_known_delegates_to_ip_literal`) that would have caught this on day one. `cargo test -p
hs-federation --lib discovery` -- 20/20 pass.

Also discovered along the way, not a code bug but worth recording: `hs-federation`'s
`AddrResolver` (`HickoryResolver`, backed by `hickory-resolver`) is a pure userspace DNS stub that
queries the configured nameservers directly and does **not** consult `/etc/hosts`. On a network
with a search-domain-configured `/etc/resolv.conf` (this laptop's has one), resolving the hostname
`"localhost"` through it is genuinely unreliable -- it is not guaranteed to return `127.0.0.1`,
unlike `curl`/`dig`/anything going through `getaddrinfo`. This session's script uses IP literals
(`127.0.0.1:8448`/`127.0.0.1:8449`) specifically to sidestep this, not merely for convenience; see
the script's own comments. This is not a bug to fix (a pure-Rust stub resolver correctly not
reading `/etc/hosts` is ordinary, documented `hickory-resolver` behavior) but is worth any future
session knowing before spending an hour on it again.

### 3. Nothing in this workspace could *initiate* a federated join -- only answer one

**The gap.** `crate::join::{make_join, send_join}` and their mount (`crate::transport::join`) are
the *resident* side of the join handshake: this server, hosting a room, answering a remote's
`GET /make_join` and `PUT /send_join`. That side is real and was already tested end to end
(`crates/hs-cli/tests/federation_writes.rs`). Nothing anywhere in this workspace played the other
role -- a server whose own user wants to join a room hosted elsewhere, which means calling *out* to
another server's `/make_join`/`/send_join`. This was invisible until this session because nothing
had ever tried: every previous test of the join handshake, in this crate and in `hs-cli`, hands a
synthetic already-signed join event to `send_join` as if a remote had produced it.

**Closed the handshake half.** New module `crates/hs-federation/src/outbound_join.rs`,
`pub async fn join_room(client, key_cache, destination, room_id, user_id, own_server_name,
signing_key) -> Result<RemoteJoinOutcome, OutboundJoinError>`: calls `GET make_join` via the same
`FederationClient::send` every other outbound call uses (so discovery, TLS/CA trust and outbound
`X-Matrix` signing all come for free), signs the returned template the spec's real way -- hash the
full event, redact, sign the *redacted* form, copy the signature back onto the full event, per
RFC-0014, which this module follows rather than re-deriving -- calls `PUT send_join` (v2), and
verifies every event in the response's `state` and `auth_chain` through the same
`crate::inbound::verify_pdu` any other inbound PDU gets. Four tests, including a full live-HTTP
round trip against a real `axum::serve` resident bound to a real loopback socket (not
`tower::oneshot`, no mocked transport) -- see `outbound_join::tests::
join_room_completes_the_real_handshake_against_a_live_resident`. `cargo test -p hs-federation
--lib outbound_join` -- 4/4 pass.

**Did not, and could not, close persistence.** `join_room` returns a fully verified snapshot and
stops there: `hs-room` has no API to create a local room from a federation join response's state,
only to originate a brand new one (`RoomActor::create_room`) or apply one more event to a room it
already has (`RoomActor::accept_remote_event`) -- neither fits "this room's real `m.room.create`
was authored by a different server and I have never seen this room before." Filed as
`docs/rfcs/0015-outbound-join-needs-a-room-bootstrap-api.md`, addressed to track 04, with the exact
shape of the needed entry point. **Consequence**: the join is real and durably persisted on the
*resident's* side (proven live, see below) but the joining server cannot yet represent the room for
its own user to sync or post into -- a one-way proof, honestly reported as such by both the RFC and
the script's own printed summary.

**A diagnostic CLI surface, since nothing in the ordinary client API can trigger this yet.** New
`hs federation-join-room -c <config> --destination <server> --room <id> --user <user_id>`
(`crates/hs-cli/src/cli.rs`'s `Command::FederationJoinRoom`, implemented in
`crates/hs-cli/src/federation.rs::run_join_room`): loads a real `hs-config` file the same way `hs
serve` would (same server name, same signing key, same federation policy) and runs `join_room`
against it, printing what was verified. Opens no storage -- nothing it produces can be persisted
locally yet, so there is nothing for it to open. This is not what `POST /join` calls (no wiring
exists there yet, and adding it needs RFC-0015 first): it is the only way, today, to exercise the
real cross-server join handshake this crate provides.

### 4. The script: `crates/hs-federation/scripts/two-server-federation.sh`

Runnable in one command (`bash crates/hs-federation/scripts/two-server-federation.sh [workdir]`,
workdir defaults to a fresh `mktemp -d`). Requires `cargo`, `openssl`, `curl`, `jq`, and `stunnel`
(`brew install stunnel` on macOS -- `hs serve` does not terminate TLS itself yet, exactly the same
reasoning and the same tool `tests/complement/Dockerfile.template` already uses; this script's
`stunnel.conf`s are the same accept-here-forward-there shape as `tests/complement/stunnel.conf.template`).
No Docker.

What it does, in order, against two real `hs serve` processes on 127.0.0.1 with different
server names (`127.0.0.1:8448`, `127.0.0.1:8449`) and different embedded-storage data directories:
generates a private CA and one IP-SAN server certificate; writes each server's native config
(`federation.custom_ca_certificates` pointing at the shared CA, `ip_range_blocklist: []` since both
instances are on loopback); starts both `hs serve` processes and a `stunnel` in front of each
(TLS on `:8448`/`:8449`, forwarding to plaintext `:8008`/`:8018`); sanity-checks that A can fetch
B's `/_matrix/key/v2/server` over real TLS with the private CA (the §1 fix, exercised first,
because everything after it depends on outbound TLS actually working); registers `@alice` on A and
`@bob` on B through the real client-server UI-auth dance; alice creates a public room and sends a
message; **B joins A's room via `hs federation-join-room`, the real make_join/send_join handshake**;
and finally verifies, by querying A's own client API (not the script's own say-so), that bob really
is a joined member. Prints a clear summary of what was proven and what a real public join would
still exercise that this run does not (DNS-based discovery, a publicly trusted CA, another
implementation's quirks, version negotiation against a server that is not itself) -- see the
script's own final output for the exact wording, since it is the artifact this file should not
duplicate and risk drifting from.

Ran twice against two fresh workdirs this session; both runs succeeded identically.

### Verification

```
cargo fmt -p hs-federation -p hs-cli                                          # applied, no diffs after
cargo test -p hs-federation                                                   # 124/124 (was 120)
cargo test -p hs-cli --test federation_reads --test federation_writes --test e2e   # 7+8+9 = 24/24
cargo test -p hs-loadgen --test real_client                                   # 1/1 (single-server path unaffected)
bash crates/hs-federation/scripts/two-server-federation.sh                    # succeeds end to end, twice
```

`cargo clippy -p hs-federation --all-targets -- -D warnings`: **fails**, but not on this crate's
code -- `crates/hs-http/src/cors.rs` (a different track's crate, with uncommitted, in-progress
changes present in the working tree at the time of this session, confirmed via `git status`/`git
diff`) trips `clippy::double_must_use` on a function this session did not touch, and workspace
clippy lints every crate in the dependency graph, not just the one named with `-p`, unless
`--no-deps` is passed. `cargo clippy -p hs-federation --all-targets --no-deps -- -D warnings` is
clean, proving this crate's own code is not the source. Not something this track can or should fix
(`crates/hs-http/**` is out of this session's ownership); flagged here so the next session does not
waste time re-diagnosing it, and re-run without `--no-deps` once that other track's work lands or
is reverted.

## Sixth session: TLS/CA trust, and the redaction-before-signing bug

Scope, per this session's brief: close the TLS/CA gap `docs/status/14-test-and-conformance.md`
identified as blocking almost all of Complement's federation package (5/89 passing; 27 of the
remaining failures showed `tls: unknown certificate authority` in the harness's container logs),
and diagnose/fix the real `send_join` signature-verification bug track 14 found waiting underneath
it once the TLS symptom was worked around. Ownership this session: `crates/hs-federation/**`,
`crates/hs-config/**`, `docs/status/06-federation.md` only -- no `hs-cli`, `hs-room`, or any other
crate, and no Docker (track 14 owns Complement runs).

### 1. The TLS/CA gap: root cause, confirmed

`crates/hs-federation/src/client.rs::client_for` builds every outbound `reqwest::Client` with
`danger_accept_invalid_certs(!verify_certificates)` and otherwise reqwest's default TLS behaviour.
The workspace's `reqwest` dependency (`Cargo.toml`: `features = ["json", "rustls-tls"]`) resolves
`rustls-tls` to `rustls-tls-webpki-roots` only -- the ~140 baked-in public root CAs, never the OS
trust store and never any application-supplied CA. There was no config surface anywhere in
`hs-config`/`hs-federation` to add a trusted CA (confirmed by grep, matching track 14's own
finding), so a harness like Complement that runs `update-ca-certificates` to trust its generated CA
system-wide had no effect on this server's outbound federation client: only `verify_certificates:
false` (which trusts *any* certificate) could get past it, at the cost of disabling TLS
authentication entirely.

### 2. The fix: a real config surface, honoured by the client

**`hs-config::FederationConfig`** (`crates/hs-config/src/federation.rs`) gained two new fields,
both `#[serde(default)]` (empty/false), with the reasoning behind each captured in the field's own
doc comment (the deliverable's own instruction) rather than only here:

- **`custom_ca_certificates: Vec<String>`** -- paths to PEM-encoded CA certificate files, trusted
  *in addition to* the built-in public roots. Directly matches Synapse's own
  `federation_custom_ca_list`, which is exactly what `refs/synapse/docker/complement/conf/workers-shared-extra.yaml.j2`
  sets for Complement. Validated (`Validate` impl): an empty-string entry is rejected with a
  helpful message, the same style as the existing `domain_allowlist` check.
- **`trust_os_root_store: bool`** (default `false`) -- whether outbound federation TLS also trusts
  whatever CA store the operating system trusts. **Decision, with the justification inline in the
  field's own doc comment**: default `false`. Trusting the OS store is the right call for *some*
  deployments (an admin who runs `update-ca-certificates` to add a corporate or test CA reasonably
  expects every TLS client on the box, including this one, to honour it), but it is the wrong
  *unconditional default* for federation specifically: federation traffic authenticates servers
  that never agreed on a shared root of trust ahead of time, so silently broadening that trust to
  whatever the OS store happens to contain (which can be widened by anyone with root, for reasons
  having nothing to do with running a homeserver -- an unrelated package, a corporate
  TLS-inspecting proxy, a forgotten test cert) is a real, quiet security regression for exactly this
  traffic. Pairing a `false` default with the explicit, narrow `custom_ca_certificates` puts the
  choice with whoever configures federation, not whoever last ran `update-ca-certificates` for an
  unrelated reason.

**`crate::client::ClientConfig`** (`crates/hs-federation/src/client.rs`) gained the matching fields
the outbound client actually reads: `custom_root_certificates: Vec<Vec<u8>>` (raw PEM bytes, not
paths -- file I/O stays at the config-loading wiring site, so this crate's own tests can hand it
bytes straight from `rcgen` without touching a filesystem) and `trust_os_root_store: bool`.
`FederationClient::new` parses `custom_root_certificates` once via
`reqwest::Certificate::from_pem_bundle` (one entry may itself be a multi-certificate bundle) into a
new `custom_roots: Vec<reqwest::Certificate>` field, logging `tracing::error!` (not panicking, not
silently dropping) for any entry that fails to parse. `client_for` calls
`.add_root_certificate(cert.clone())` for each one -- additive, never replacing the built-in public
bundle -- and `.tls_built_in_native_certs(self.config.trust_os_root_store)` to gate the OS store.

**Enabling the OS-store toggle for real** (not just documenting an inert field) needed reqwest's
`rustls-tls-native-roots` feature, which is off at the workspace level (only `rustls-tls`, i.e.
webpki-roots, is enabled there). Added it in `crates/hs-federation/Cargo.toml` specifically (`reqwest
= { workspace = true, features = ["rustls-tls-native-roots"] }`), not the workspace root -- it is
additive to the existing `rustls-tls` feature (both root sources compile in; which one(s) actually
get consulted per-request is controlled entirely by the two `tls_built_in_*` calls above, not by
which features happen to be compiled in) and costs nothing new to fetch: `rustls-native-certs` and
its platform dependencies (`security-framework` on macOS, `schannel` on Windows) were already
resolved in the workspace's `Cargo.lock` via another crate before this session. Confirmed via
`cargo check -p hs-config -p hs-federation` that no new crate needed fetching.

### 3. `verify_certificates: false` is now loud

`FederationClient::new` logs a prominent `tracing::warn!` once, at construction time, whenever
`config.verify_certificates` is `false`, spelling out exactly what it means (outbound TLS accepts
*any* certificate from *any* peer; every event's trust then rests entirely on its Ed25519
signature; a MITM on outbound federation traffic can impersonate any remote server) and naming
`custom_ca_certificates` as the narrower alternative. `hs-config::FederationConfig::verify_certificates`'s
own doc comment carries the same warning for anyone reading the schema directly rather than the
running server's logs. The field itself is unchanged (`hs-cli`'s existing wiring already threads it
through) -- "loud" was achieved entirely inside this crate, at the one place (`FederationClient::new`)
every real mount already calls exactly once per server startup, so no `hs-cli` change was needed to
satisfy this deliverable.

### 4. The real proof: an in-process TLS test, no Docker

`crates/hs-federation/src/client.rs::tests::outbound_tls_rejects_an_unconfigured_ca_but_trusts_a_configured_one`
(plus its helper `spawn_self_signed_tls_peer`): mints a real self-signed certificate for
`"localhost"` with `rcgen` (already a dev-dependency), terminates real TLS with it via
`rustls`/`tokio-rustls` over a real loopback `TcpListener`, and serves one plain HTTP/1.1 response
per connection via `hyper::server::conn::http1` (wrapped for hyper's IO traits via
`hyper_util::rt::TokioIo`) -- no axum, since `axum::serve` only accepts a `TcpListener`-shaped
`Listener` in this axum version and standing up a custom TLS-terminating `Listener` impl was not
worth it for a test this size. Two assertions against the *exact same* peer and certificate:
`FederationClient` with a default `ClientConfig` (no custom CA) gets `ClientError::Request` (the
TLS handshake genuinely fails, exactly Complement's pre-fix symptom); the same client with
`custom_root_certificates: vec![cert.pem().into_bytes()]` gets a real `200`. New dev-dependencies
for this one test, all already `[workspace.dependencies]` entries used elsewhere in the workspace
(no new crate to fetch): `rustls`, `tokio-rustls`, `rustls-pki-types`, `hyper`, `hyper-util`,
`http-body-util`.

### 5. The bug underneath: `send_join` rejecting a genuinely signed join

Per track 14's diagnosis (`docs/status/14-test-and-conformance.md`): once the TLS symptom was
worked around, `send_join` started failing with `M_BAD_JSON: signature from
host.docker.internal:.../ed25519:... does not verify` on a join this server had no legitimate
reason to reject. **Root cause, confirmed by reading the spec directly**
(`refs/matrix-spec/content/server-server-api.md`, "Validating hashes and signatures on received
events"): signature verification must always run against the event's **redacted** form, never the
full one -- "the event is redacted following the redaction algorithm, and the resultant object is
checked for signatures... this step should succeed whether we have been sent the full event or a
redacted copy." A conformant sender signs the redacted form too (the same document's "Adding hashes
and signatures to outgoing events": hash, then redact, then sign, then copy the signature back onto
the original). `crate::inbound::verify_pdu` was calling
`hs_model::signing::verify_object(event.json(), ...)` -- the **full, unredacted** event -- instead
of the redacted one. For any event whose content carries anything redaction would strip (which for
`m.room.message` is *all* of `content`, and for `m.room.member` is anything beyond `membership`
itself, e.g. a profile), this rejects a perfectly legitimate signature.

**Fixed** in `crates/hs-federation/src/inbound.rs::verify_pdu`: computes `event.redacted_json()`
(the existing, already-tested `hs_model::Event` method) and verifies the signature against that,
not `event.json()`. The returned `Event` is unchanged (full content and all) -- only the bytes
`verify_object` checks the signature against changed. Confirmed via a new, isolated test
(`inbound::tests`'s existing `verify_pdu_accepts_a_correctly_signed_event` etc. all still pass, and
this crate's own event-signing test helpers were updated to actually sign the redacted form --
see below) plus manual reasoning against the spec text quoted above.

**A necessary companion fix to this crate's own tests**: `crate::inbound::tests::signed_event` and
`crate::backfill::tests::signed_message` both built an `m.room.message` and signed the **full**
object directly (the same shape of bug §6 below describes in `hs-room`), which is exactly what
`verify_pdu`'s old, wrong check happened to accept and its new, correct check would reject. Both
were fixed to the spec's real order: build the full object with `hashes` attached, redact it
(`hs_model::redaction::redact`), sign the *redacted* copy, then copy `signatures` back onto the
full object before returning it -- matching what a real conformant sender does and what
`verify_pdu` now actually checks. `cargo test -p hs-federation --lib` is green at 120/120 with both
the production fix and both test-helper fixes in place; `crate::join::tests::sign_member_event` did
**not** need this fix, because its events' content is exactly `{"membership": "join"}`, which
`m.room.member` redaction keeps unchanged (full and redacted forms are byte-identical for that
narrow content shape), so the bug had no observable effect there.

### 6. The same bug, found live in `hs-room`'s own outbound pipeline -- not fixed this session, RFC filed

Fixing `verify_pdu` correctly (§5) also makes it reject **this server's own previously
self-consistent, but spec-non-compliant, signatures** wherever both sides used to agree only by
both being wrong the same way. Confirmed empirically (read-only `cargo test -p hs-cli --test
federation_writes`, no `hs-cli` file edited): 4 of 8 tests newly fail. Three
(`send_rejects_a_new_event_whose_auth_events_do_not_authorize_it`,
`send_backfills_a_missing_ancestor_then_accepts_the_original_event`,
`send_gives_up_when_the_remote_serves_an_endless_backfill_chain`) are the same test-fixture bug as
§5's companion fix -- hand-built synthetic PDUs signed unredacted, mechanical fix, three lines each,
full instructions in the RFC below. The fourth,
**`send_accepts_an_event_it_already_holds_idempotently`, is not a test bug**: it resubmits an event
this server actually built and signed through the real `RoomActor`/`pipeline.rs` path, and it now
fails signature verification too -- direct proof that `crates/hs-room/src/pipeline.rs`'s
hash-and-sign step (`build_and_authorize`, around line 360-373) signs the **full, unredacted**
canonical object with no redaction step, the identical bug `verify_pdu` just had, still live in
production code this session does not own. **Consequence, if left unfixed**: any real,
spec-compliant remote homeserver, correctly redacting before checking (as `verify_pdu` now does
too), would reject this server's own outbound events whenever their content carries anything
redaction would strip -- which is every ordinary `m.room.message`. This is filed as
`docs/rfcs/0014-event-signing-must-sign-the-redacted-form.md`, addressed to track 04
(`crates/hs-room/src/pipeline.rs`) with the exact three-line shape of the fix (mirroring what this
session already did twice in its own test helpers) plus the three `hs-cli` test-fixture call sites
that need the same mechanical correction. Not fixed here: `crates/hs-room/**` is outside this
session's ownership, and `crates/hs-cli/**` likewise.

### Verification

```
cargo fmt -p hs-federation -p hs-config                                # applied, no diffs after
cargo clippy -p hs-federation -p hs-config --all-targets -- -D warnings # clean
cargo test -p hs-federation -p hs-config                               # 120 + 73 passed, 0 failed
cargo run -p hs-config --bin gen_config_docs                           # regenerated docs/config.md
```

`cargo test -p hs-cli --test federation_writes` (read-only check, no `hs-cli` file touched): 4/8
pass, 4/8 fail exactly as described in §6 above -- expected, not a regression this session
introduced silently; see the RFC for the fix.

## Fifth session: the backfill loop

Scope (`docs/next-steps.md` item 4): consume `RoomError::MissingAncestors` instead of merely
reporting it. A remote's join could already be persisted (fourth session); the very next event
that server sent citing history from before the join could not, because nothing fetched that
history. This session closes that.

### 1. The loop, in one paragraph

New module `crates/hs-federation/src/backfill.rs`. When `RoomWriteSink::accept_verified_event`
rejects an event because it names ancestors this server does not hold
(`WriteRejected::missing_ancestors`, new -- see below), `crate::inbound::process_transaction`
calls `crate::backfill::resolve_missing_ancestors(origin, room_id, room_version, missing_ids,
fetcher, key_cache, sink, limits)`. That function asks `origin` (the server that sent the
transaction -- the natural peer to ask, since it is the one that told us about an event referencing
history it presumably has) for the missing events via `GET /backfill/{roomId}?v=...&limit=...`,
verifies each returned PDU exactly the way `verify_pdu` verifies any inbound PDU (content hash,
then signature against the *sender's* server, not `origin`), and persists them through the same
`RoomWriteSink` in dependency order. If persisting one of *those* events itself reports a deeper
`MissingAncestors` (the gap is more than one event deep), that becomes the next round's fetch
target -- the loop is genuinely recursive, not a single fetch-and-hope. Once the fetched events are
all either persisted or hard-rejected (bad auth -- see below), control returns to
`process_transaction`, which retries the original event exactly once. Success or failure of that
retry is reported the normal way: a per-event `{}` or `{"error": ...}` inside the transaction
response, never a fatal transaction failure -- backfill giving up looks, from `/send`'s caller's
point of view, exactly like the event being unresolvable for any other reason.

### 2. The limits, and why

`BackfillLimits` (`crate::backfill`), four independent dimensions, all required to fail before the
attempt gives up on a genuine gap it just cannot close, and any one of which stops the attempt on
its own:

| Field | Default | What it bounds |
|---|---|---|
| `max_events_per_fetch` | 100 | The `limit` sent on each `/backfill` request, **and** the most events accepted from one response even if the peer sends more. Matches `transport::read_routes::MAX_BACKFILL_LIMIT`, this server's own server-side clamp on the same endpoint -- this server never asks for more than it would itself agree to answer, and never trusts a peer that ignores the `limit` it was given. |
| `max_rounds` | 10 | The most `/backfill` round-trips one resolution attempt makes to the same peer. This is the recursion-depth bound: each round can surface a new, deeper gap (a fetched event's own `prev_events` can themselves be missing), so a chain deeper than 10 hops is given up on, not chased further. **Confirmed load-bearing by mutation test** -- see below. |
| `max_total_events` | 500 | The total number of events fetched and signature-*verified* (an asymmetric-crypto operation) across every round, independent of how few rounds it took to reach that count. This is the real cost-of-attack bound: rounds alone would not stop a peer that returns many events per round. |
| `max_duration` | 20s | Wall-clock ceiling for the whole attempt (`tokio::time::timeout` around the entire resolution), so a slow-but-not-failing peer cannot hold the task open indefinitely. |

The request itself is also bounded independent of the response: `frontier.iter().take
(max_events_per_fetch)` caps how many event IDs go into one `?v=...&v=...` query string, so a
round that somehow discovered many independent gaps at once cannot turn into an unbounded URL.

A peer that returns *nothing new* (an empty response, or a response containing only events already
seen earlier in the same attempt) is treated as "cannot make progress" and the attempt gives up
immediately (`BackfillGiveUpReason::StillMissing`) rather than retrying the same request pointlessly
for the remaining rounds -- a real inability to help is distinguished from "still trying".

### 3. What an attacker can and cannot cost this server

**Can**: force up to 10 HTTP round-trips to itself, up to 500 signature verifications (Ed25519
verify is cheap -- microseconds -- and `RemoteKeyCache` already deduplicates concurrent lookups for
the same `(server, key_id)`, so the realistic cost is closer to "500 verifies against a handful of
cached keys" than 500 separate key fetches), and up to 20 seconds of one Tokio task's wall-clock
time, **per event that names a missing ancestor**. It can repeat this for every hostile event it
sends, so the aggregate cost across many transactions is not bounded by this module alone --
but each individual attempt is small, finite, and cannot compound into recursion, an unbounded
response, or an indefinite hang. The existing per-destination concurrency limit
(`FederationClient`, default 1 in-flight request) and `DestinationStore` backoff additionally throttle
*how fast* a single hostile server can trigger repeated attempts, though that is a pre-existing
defence this session did not add, not something `backfill.rs` itself enforces.
**Cannot**: make this server recurse forever (bounded by `max_rounds` and `max_total_events`,
enforced independently so neither alone is a single point of failure), make it accept an
unauthorized or malformed event (every fetched event still goes through the exact same
`verify_pdu` + `RoomActor::accept_remote_event`'s two-snapshot authorization check as any other
inbound event -- backfill is a *source* of candidate events, not a bypass of anything that checks
them), make it hang past 20 seconds on one attempt, or make it send an unbounded request (the `v=`
list is capped the same as the response).

### 4. The outbound `/backfill` client

`FederationClient::backfill` (`crates/hs-federation/src/client.rs`), alongside the inbound
`/backfill` server (`transport::read_routes::backfill`, existing since the second session). Builds
`GET /_matrix/federation/v1/backfill/{roomId}?limit=N&v=...`, calls the existing
`FederationClient::send` (the one signed-request path every other outbound call already goes
through -- no second X-Matrix client), and returns the raw, **unverified** `pdus` array. Verifying
is deliberately not this method's job: `crate::backfill::resolve_missing_ancestors` is the one
place that owns "fetch, then verify" as a sequence, so there is exactly one path where a fetched
event might be trusted before it is checked, and it is easy to audit.

`crate::backfill::AncestorFetcher` is the seam `FederationClient` implements this through
(`impl AncestorFetcher for FederationClient`), the same shape as `RoomDataSource`/`RoomWriteSink`:
a trait this crate owns, so `crate::backfill`'s own tests can supply fakes (`QueuedFetcher`,
`EndlessFetcher`) without a real HTTP server, and `hs-cli`'s integration tests can supply a real
`FederationClient` pointed at a real loopback listener.

### 5. Mutation test performed this session

Per this session's instructions: `BackfillLimits::default().max_rounds` was changed from `10` to
`usize::MAX` in `crates/hs-federation/src/backfill.rs`, and
`backfill::tests::gives_up_cleanly_on_an_endless_chain_instead_of_looping_forever` (which asserts
the endless-chain peer above is given up on after exactly 10 round-trips with
`BackfillGiveUpReason::TooManyRounds`) was re-run:

```
thread 'backfill::tests::gives_up_cleanly_on_an_endless_chain_instead_of_looping_forever' panicked
  at crates/hs-federation/src/backfill.rs:604:9:
Err(TooManyEvents)
test result: FAILED. 0 passed; 1 failed; ... finished in 3.18s
```

The test failed exactly as expected: with the round bound removed, the *other* independent limit
(`max_total_events`, still 500) caught the runaway instead, 500 rounds in instead of 10 -- proving
`max_rounds` is what the original test's "exactly 10 round-trips" assertion depends on, not
incidental behaviour elsewhere in the loop. It did **not** hang (3.18 seconds for 500 in-process
mock round-trips, no real network involved in this unit test), which is itself informative: even
with one limit disabled, the layered design meant this session never had to interrupt a genuinely
runaway process to observe the failure. The change was reverted immediately
(`max_rounds` back to `10`); `cargo test -p hs-federation --lib backfill` is green (6/6) with the
revert in place. This is recorded here rather than kept as a second standing test, because a test
cannot mutate the default it is itself asserting against without either duplicating the limit or
ceasing to test what its name says (see the comment left in place of the test in
`crates/hs-federation/src/backfill.rs`).

### 6. `WriteRejected` gained a structured `missing_ancestors: Vec<String>` field

(`crates/hs-federation/src/inbound.rs`, plus its two constructors `WriteRejected::other` and
`WriteRejected::missing_ancestors`.) Previously the only signal was a human-readable `error`
string; `hs_cli::federation::RegistryWriteSink` had already started interpolating the missing IDs
into that string for operator-log readability, but nothing machine-readable distinguished "missing
ancestors, maybe backfillable" from "any other rejection, not backfillable" without string-matching
the message. `crate::backfill::resolve_inner` reads `rejected.missing_ancestors` directly.
`RegistryWriteSink::accept_verified_event` (`crates/hs-cli/src/federation.rs`) now constructs
`WriteRejected::missing_ancestors(id_strings, message)` for exactly the `RoomError::MissingAncestors`
case and `WriteRejected::other(...)` everywhere else, so the string in `error` and the structured
list can never drift apart (one format call builds both).

### 7. Testing shape

- **`crates/hs-federation/src/backfill.rs`** (6 new lib tests): `resolves_a_single_hop_gap`,
  `resolves_a_multi_hop_gap_across_several_rounds` (a two-deep chain, fetched one hop per round,
  proving the loop actually recurses and that an event fetched in an earlier round is retried once
  its own blocker lands -- this is what caught a real bug during development, see below),
  `gives_up_cleanly_on_an_endless_chain_instead_of_looping_forever`,
  `gives_up_when_the_remote_returns_nothing`. Fakes: `QueuedFetcher` (hands back a scripted
  sequence of responses), `EndlessFetcher` (never converges), `DagSink` (a minimal
  `RoomWriteSink` that actually enforces "prev_events must already be known", closely mirroring
  `RoomActor::accept_remote_event`'s real shape without needing a real room actor).
- **`crates/hs-federation/src/client.rs`** (1 new test): `backfill_sends_a_signed_get_and_parses_the_pdus`,
  against `hs_testkit::FakeFederationPeer` over a real loopback socket -- asserts the exact request
  shape (`GET .../backfill/{roomId}?limit=N&v=...`) a real peer would receive, not just that the
  method compiles.
- **`crates/hs-cli/tests/federation_writes.rs`** (2 new end-to-end tests, `Harness` extended with
  `Harness::with_backfill_peer(port)`, a real `FederationClient` pointed at a real loopback
  listener via the explicit-port destination form `localhost:{port}` -- exactly the seam
  `crates/hs-federation/src/client.rs`'s own tests already use, so this is not a new test pattern):
  - `send_backfills_a_missing_ancestor_then_accepts_the_original_event`: a hand-built, correctly
    signed and hashed `m2` cites a hand-built `m1` this server was never sent. A minimal axum
    server (not `hs-testkit`, which `hs-cli` does not depend on -- see "Decisions made") answers
    `/backfill` with exactly `m1`. Asserts `m2` is accepted and `m1` is independently readable back
    through `/event/{id}` afterwards -- both fetched-via-backfill and original-event persistence
    are checked, not just "the transaction returned 200".
  - `send_gives_up_when_the_remote_serves_an_endless_backfill_chain`: the hostile peer described
    above, over real HTTP. Asserts the transaction reports a per-event error mentioning the
    give-up (not a hang, not a fake success) and that **exactly** `BackfillLimits::default()
    .max_rounds` requests reached the peer -- the bound is checked by counting real HTTP requests
    that arrived, not just by inspecting the returned error type.

**A real bug this session's own tests caught before it reached these two integration tests**: the
first version of `resolve_inner` dropped a fetched-but-blocked event the moment its first
persistence attempt failed, re-deriving only the next `frontier` from it and discarding the event
itself. That worked for a single-hop gap but silently lost multi-hop chains: fetching `e1` (which
unblocks `e2`, already fetched and discarded in the previous round) would never retry `e2`, and the
loop would report success once `e1` alone persisted even though `e2` -- the actual descendant
needed -- was still missing. `backfill::tests::resolves_a_multi_hop_gap_across_several_rounds`
failed immediately (`assertion failed: sink.known.lock().unwrap().contains(&e2_id)`) and pinpointed
exactly this. Fixed by carrying a `pending: Vec<Event>` worklist across rounds (not just within one)
and re-attempting the *entire* worklist, sorted ancestors-first by `depth`, every round -- so an
event unblocked by this round's fetch is retried in the same pass that unblocks it, not abandoned
after its first failed attempt.

**A second real thing this session's tests caught, about the auth rules rather than backfill
itself**: constructing a hand-signed test event first failed with "no m.room.create event in auth
events" -- this session had assumed (incorrectly) that room version 11 excludes `m.room.create`
from a message event's `auth_events` selection the way version 12 does
(`room_create_event_id_as_room_id`). Reading `crates/hs-model/src/room_version.rs` directly showed
V11 does **not** set that flag (only V12 does); `hs_state::auth::expected_auth_types` therefore
still requires `m.room.create` in the selection for V11. Not a bug in this session's production
code -- a wrong assumption in the test's construction, caught by the real auth rules doing their
job. Recorded here because the next person hand-constructing a V11 test event will hit the exact
same thing.

### 8. Wiring

`hs_cli::federation::build_mount` (`crates/hs-cli/src/federation.rs`, this track's file) now sets
`ancestor_fetcher: Some(client.clone() as Arc<dyn hs_federation::backfill::AncestorFetcher>)` --
the *same* `FederationClient` every other outbound call already uses, so backfill shares its
signing key, per-destination concurrency limit and backoff state with everything else this server
sends. `manifest_only_mount` sets `ancestor_fetcher: None` (routes-manifest generation needs no
outbound capability). No `serve.rs` change was needed this session -- `FederationState` already
flowed through unchanged mount points; the two new fields are just two more fields on a struct that
was already being threaded through.

### Verification

```
cargo fmt -p hs-federation -p hs-cli                              # applied, no diffs after
cargo clippy -p hs-federation --all-targets -- -D warnings        # clean
cargo clippy -p hs-cli --all-targets --no-deps -- -D warnings     # clean (see fourth session's
                                                                   # note on why --no-deps)
cargo test -p hs-federation                                       # 119 passed (lib), 0 failed
cargo test -p hs-cli --test federation_reads --test federation_writes
                                                                   # 7 + 8 passed, 0 failed
```

## Done

All against `docs/design/06-federation-threat-model.md`'s catalogue; file paths below are all
under `crates/hs-federation/src/` unless stated otherwise.

- **`Cargo.toml`** (crate and workspace root). Added to `[workspace.dependencies]`:
  `hickory-resolver` (0.26, `tokio` feature — `system-config` comes along via its own default
  features) for DNS, and `ipnet` (2.x) for CIDR containment checks (promoted from a transitive dep
  of `hickory-net` to a direct one rather than hand-rolling CIDR matching). Both noted in the root
  `Cargo.toml` with "Added by track 06" comments per convention. The crate's own `Cargo.toml` pulls
  in `hs-model`, `hs-kv`, `hs-tables`, `hs-http`, `hs-config`, `ruma`, `ed25519-dalek`, `rand_core`,
  `axum`, `tokio`, `reqwest`, `regex`, `async-trait`, and dev-deps `hs-testkit`, `tempfile`,
  `tower`, `rcgen`.
- **`discovery.rs`** (20 tests). The full server-name resolution algorithm (IP literal / explicit
  port / well-known+SRV+fallback), behind three traits (`WellKnownFetcher`, `SrvResolver`,
  `AddrResolver`) so the resolution logic is unit-tested against fakes reproducing the spec's
  worked examples, with real `reqwest`-backed (`HttpWellKnownFetcher`) and
  `hickory-resolver`-backed (`HickoryResolver`) implementations for production. Covers: an explicit
  port bypassing discovery entirely, a well-known that itself needs SRV resolution, SRV falling
  back to the deprecated `_matrix._tcp` service, malformed/absent well-known falling back to SRV on
  the *original* host, no-redirects-followed (structurally impossible in this design — the fetcher
  trait has no redirect-following code path at all), body-size cap enforced without buffering past
  it, and `CachingWellKnownFetcher` (a TTL decorator honouring `clamp_cache_control`, with separate,
  shorter negative caching for failures — the plan's "wrap `WellKnownFetcher` in a decorator"
  item). `MIN`/`MAX`/`DEFAULT`/`FAILED` cache TTL constants are committed to concrete values (60s /
  24h / 24h / 60s) since the first session left them unpicked.
- **`keys.rs`** (14 tests). `OwnSigningKeys::load_or_generate` (Synapse's `algorithm version
  base64_seed` line format, first-boot generation, multiple keys from separate files for rotation,
  malformed lines skipped not fatal). `build_server_key_response` (self-signed with every active
  key, `old_verify_keys` carried through). `wrap_for_notary` (adds the notary's own signature
  without touching the origin's). `RemoteKeyCache` with `KeyServerFetcher` as the fetch seam,
  per-origin in-flight de-duplication (`tokio::sync::Mutex` keyed by origin, tested with 8
  concurrent callers producing exactly 1 fetch), self-signature verification before trusting
  anything (tampered key, wrong claimed `server_name`, both rejected), and the old-verify-keys
  rule: `get_current` (now-valid only) vs `get_valid_at(ts)` (accepts a key that was valid *when
  signed* even if since rotated out, rejects one that had already expired) — this is the
  "expired key rejected, key valid at signing time still accepted" pair from the assignment,
  tested directly. Cross-server key substitution is structurally impossible (cache keyed by
  `(server_name, key_id)`, a response can only populate the server name it claims and self-signed
  for) and tested.
- **`xmatrix.rs`** (12 tests). Header parse (quoted/bare values, any field order, RFC 7235 escapes,
  rejects multiple `Authorization` headers, rejects wrong scheme, rejects a missing field) and
  build. The exact signed-object shape (`method`, `uri`, `origin`, `destination`, `content?`) built
  on `hs_model::canonical`/`signing` directly, not reimplemented. `verify_x_matrix`, the axum
  middleware: destination-must-equal-own-name, body-size cap enforced before signature
  verification reads it (`axum::body::to_bytes` bounded at 50 MiB), key lookup through
  `RemoteKeyCache::get_current`, full signature reconstruction and verification. Tests: tampered
  body rejected, wrong destination rejected, wrong key rejected, missing signature rejected,
  replayed valid signature against a *different* route rejected (proves `uri` binding actually
  works, not just "any valid sig passes"), multiple `Authorization` headers rejected, unknown
  origin rejected without ever caching a bogus key. **Found and fixed a real layering-order bug
  while writing these tests**: `Router::layer` wraps outside-in (last `.layer()` call runs
  first), so `Extension(ctx)` must be added *after* `from_fn(verify_x_matrix)`, not before, or the
  context is missing and every request 500s. Documented prominently in both the module doc and the
  `verify_x_matrix` doc comment so `crate::transport::router` (which has the same ordering
  requirement) doesn't regress it.
- **`acl.rs`** (11 tests). One `is_allowed(server_name, &ServerAcl) -> bool` function, used for
  both directions per the threat model's explicit requirement (no direction parameter exists, so
  there is no way to call it asymmetrically). Anchored glob-to-regex translation, tested for both
  match and adjacent-non-match (`*.example.org` vs `example.org` itself and vs
  `example.org.evil.com`; `?` matching exactly one character, not zero or two; a literal `.` in a
  pattern not acting as regex "any character"). `allow_ip_literals` checked against the *string
  form* of the server name, distinct from (and not a replacement for) `ip_range_blocklist`.
- **`room_source.rs`** (4 tests). The `RoomDataSource` trait per threat model section 5's seam
  decision (membership/visibility, event/state/state-ids/auth-chain/backfill/missing-events/
  hierarchy/timestamp-lookup, plus `get_event_by_id` for `/event/{eventId}`'s room-less path
  shape), and `InMemoryRoomSource`/`FakeRoom` for this crate's own tests. Every method takes
  `requesting_server` explicitly; `InMemoryRoomSource` enforces the visibility check *before*
  returning content in every method, and a test asserts a non-member gets `NotVisible` before ever
  seeing event content.
- **`destination_store.rs`** (7 tests). `DestinationState` (failure count, next retry time,
  exponential backoff base 1s doubling capped at `max_backoff_ms`, full jitter). `DestinationStore`
  trait with `InMemoryDestinationStore` and `KvDestinationStore<B: KvBackend>` (a real
  `hs_tables::TypedKeyspace` under `hs_federation.destinations`). The "persisted so a restart
  resumes" requirement is tested directly: two independent `KvDestinationStore` handles opened over
  the *same* `MemoryBackend` instance (simulating a process restart against the same on-disk store)
  see the same accumulated failure count.
- **`client.rs`** (10 tests). `FederationClient`: per-destination `tokio::sync::Semaphore`
  (default concurrency 1, tested that two concurrent sends to the same destination both complete
  rather than deadlocking), `DomainPolicy` (checked against the *original* server name, before any
  discovery happens — tested that a denied domain never reaches the network), `IpPolicy` (CIDR
  block/allow via `ipnet`, allowlist overrides blocklist, checked against the *resolved* address —
  tested that a blocked destination is rejected even though `AddrResolver` successfully resolved
  it), backoff-aware dispatch via `DestinationStore` (a backing-off destination is rejected before
  any network call), and a `reqwest::Client` cache pinned per destination via `.resolve()` so the
  TLS SNI / `Host` stays the delegated name while the TCP connection goes to the actually-resolved
  address (the spec's delegation contract). HTTP/1.1-only to peers, per the brief's recommendation
  — committed as a decision below. Tested end-to-end against `hs_testkit::FakeFederationPeer` bound
  to a real loopback socket (see `ClientConfig::scheme`, a documented test-only seam — see
  "Decisions made").
- **`transport/`** (a `mod.rs` + `read_routes.rs` + `seams.rs` + `queries.rs`, 9 tests in `mod.rs`
  + `read_routes.rs` + `seams.rs` combined). `router()` builds one merged `axum::Router` from
  `read_routes::add_routes` and `seams::add_routes` composed into a single `hs_http::router::Builder`
  (not two separately-built routers merged at the root — axum forbids nesting at `""`, discovered
  while wiring this up), then applies `axum::middleware::from_fn(verify_x_matrix)` and
  `Extension(x_matrix_ctx)` **exactly once**, over the whole thing, in the correct order (see the
  xmatrix.rs bug above). **The load-bearing test**
  (`transport::tests::every_route_is_behind_the_x_matrix_layer`) walks the router's own
  `RouteManifest` — not a hand-maintained list — substitutes a placeholder for every `{param}`
  path segment, and asserts every single registered route returns 401 with no `Authorization`
  header. This test does not need updating when a route is added; it fails automatically the
  moment a future route ships outside the merge. Fully implemented, against `RoomDataSource` and a
  small `FederationQuerySource` seam (profile/directory/devices/openid — not room-scoped, so not
  part of `RoomDataSource`): `/version`, `/query/{queryType}` (profile, directory),
  `/user/devices/{userId}` (gated on `allow_device_name_lookup_over_federation`), `/publicRooms`
  (gated on `allow_public_rooms_over_federation`, limit clamped to 100), `/hierarchy/{roomId}`,
  `/timestamp_to_event/{roomId}`, `/openid/userinfo`, `/event/{eventId}`, `/state/{roomId}`,
  `/state_ids/{roomId}`, `/event_auth/{roomId}/{eventId}`, `/backfill/{roomId}` (limit clamped to
  100), `/get_missing_events/{roomId}` (limit clamped to 100). Every per-room handler checks
  membership/visibility via `RoomDataSource` before returning content — tested directly (a
  non-member gets 403, a member gets 200, for the same event). `/send`, every join/leave/knock/
  invite handshake variant (v1 and v2 where the spec has both), `/exchange_third_party_invite`,
  `/3pid/onbind`, `/user/keys/claim`, `/user/keys/query` (joint-owned with track 08), and
  `/rooms/{roomId}/complexity`, `/extremities/{roomId}`, `/query/account_status` are mounted as
  seams: one shared `not_implemented` handler, behind the same layer as everything else, returning
  a typed 501. A test walks every seam route the same way the layer test does and asserts every one
  responds 501. Media endpoints and the two `_synapse/client/*` compat entries are deliberately
  **not** mounted (media is track 09's; `_synapse/client/*` is client-prefixed, not
  server-to-server).
- **`edu.rs`** (6 tests, new — not in the original plan's enumerated file list, but needed to give
  the required "EDU JSON" fuzz target something real to call). `parse_edu`: structural validation
  only (`edu_type` string required, `content` object-or-absent, size-capped at the same 64 KiB
  PDUs get, canonical-JSON validated via `hs_model::canonical::to_canonical_object` so a malformed
  number is rejected the same way it would be for a PDU). Not wired into a handler yet (`/send`
  is still a seam) — this is deliberately just the parser, matching the instruction that a parser
  touching remote input needs a fuzz target regardless of whether its caller exists yet (see
  `crates/hs-media/fuzz`'s own precedent, cited in the brief, for exactly this pattern).
- **Fuzz targets** (`crates/hs-federation/fuzz/`, five targets, one seed pair each). `pdu_parse`
  (drives `hs_model::Event::parse` directly, reusing track 02's parser, not a new one).
  `edu_parse` (drives `crate::edu::parse_edu`). `xmatrix_header_parse` (drives
  `crate::xmatrix::parse_x_matrix_header` via a fuzzed `HeaderValue`). `well_known_body_parse`
  (drives a newly-extracted `crate::discovery::parse_well_known_body`, now also used by the real
  `HttpWellKnownFetcher` so there is exactly one parsing path, not two that could drift).
  `key_server_response_parse` (drives `RemoteKeyCache::ingest_response`, made `pub` specifically so
  it is fuzzable without a live fetcher). **Confirmed these type-check on the stable toolchain
  installed here** (`cargo check` inside `crates/hs-federation/fuzz`, a separate `[workspace]` per
  the `hs-media/fuzz` template, succeeds) — actually *running* them needs `cargo-fuzz` and a
  nightly toolchain per the brief, neither available in this sandbox, so they have not been
  executed, only type-checked and given one valid + one adversarial seed file each under
  `fuzz/corpus/<target>/`.

## Mounted into `hs serve` (third session)

The transport server is no longer written-but-unserved. `hs serve` mounts it, and it answers from
this server's real data.

- **`crates/hs-cli/src/federation.rs`** (new, integration lead). `RegistryRoomSource` implements
  [`RoomDataSource`] over `hs_room::registry::RoomRegistry` and `hs-user`'s published-room
  directory; `ServerQuerySource` implements `FederationQuerySource` over `hs-auth`'s user/device
  store, `hs-e2e`'s device keys and `hs-room`'s alias keyspace; `ClientKeyFetcher` implements
  `KeyServerFetcher` over the real `FederationClient`, which is what makes inbound verification
  able to fetch a stranger's keys. `build_mount` assembles all of it, and takes the signing key
  from the *same* `HomeserverIdentity` `hs-room` signs events with, so the key this server
  advertises and the key it signs with cannot drift apart.
- **`crates/hs-cli/src/serve.rs`**. Mounts the transport router at `/_matrix/federation/v1` when
  `federation.enabled`, plus `GET /_matrix/key/v2/server` *outside* the `X-Matrix` layer (it is
  the one federation endpoint that must answer an unsigned request -- it is how a remote gets the
  keys it would need to sign one).
- **Two real bugs this surfaced**, both only visible once the router was mounted the way a
  deployment mounts it:
  1. **`xmatrix.rs`: the verifier signed over the wrong URI under a prefix mount.**
     `axum::Router::nest` rewrites `req.uri()` to the path *relative to* the nest prefix before
     inner layers run, so the layer was verifying against `/version` while every real sender signs
     `/_matrix/federation/v1/version`. Every inbound request from every real homeserver would have
     failed verification. Fixed by preferring the `OriginalUri` extension (`signed_uri`).
     `crates/hs-cli/tests/federation_reads.rs` mounts the router under the real prefix precisely
     so this stays caught.
  2. **`read_routes.rs`: `/backfill?v=$a&v=$b` was rejected with `400`.** `axum::extract::Query`
     deserializes through `serde_urlencoded`, which cannot build a sequence from repeated keys, so
     a `Vec<String>` field failed the whole extraction rather than collecting. Now parsed from raw
     pairs.
- **Two spec deviations fixed**: `/3pid/onbind` was registered `POST` where the spec says `PUT`,
  and `/query/profile` and `/query/directory` are now registered as their own paths (the spec
  names them; the generic `/query/{queryType}` still works).

### What the adapter deliberately will not answer

- **`/state` and `/state_ids` answer only for the room's newest event**, and return `404` for any
  older one. The room actor holds one flat current-state map and has no state-at-an-event query,
  so the newest event is the only one whose state it can report correctly. Answering a question
  about the past with the present would give a remote state it cannot tell is wrong. Lifting this
  needs `hs-state`'s historical snapshots.
- **`/openid/userinfo` resolves nothing**: no OpenID token is ever issued, because the
  client-server `POST /user/{userId}/openid/request_token` endpoint does not exist yet.
- **`/query/profile` returns an empty profile for a local user that exists**, because no profile
  storage exists anywhere in the workspace yet (there is no client-server `/profile` route
  either). It is `404` only for a user this server does not have.
- Auth chains are walked transitively from stored `auth_events` (bounded at 2,000 events), not
  read from `hs-state`'s chain-cover index, which the room actor does not maintain yet.

### Still unmounted or still wrong (as of the third session; see the fourth session below for
what changed)

- ~~The v2 join/leave/invite paths are registered under v1~~ -- fixed the fourth session, see below.
- ~~`GET /.well-known/matrix/server` is not served.~~ -- served as of this session's manifest (added
  by another track's work landing between sessions; not this track's change, noted here only so
  the "still wrong" list stays accurate).
- ~~`/send` and the join handshakes are still seams.~~ -- `/send`, `make_join` and `send_join` (v1
  and v2) are real as of the fourth session; `send_leave` and `invite` remain seams (out of this
  session's scope) but are now mounted at the *correct* v2 path. See below.

## Fourth session: `/send`, `make_join`/`send_join`, and the v2 mount fix

Scope for this session (`docs/next-steps.md` item 3): close the two federation writes that matter
most so this server can be joined by a remote and can receive its events, and fix the v2 routing
bug first. Every file below is new or changed under `crates/hs-federation/src/` and
`crates/hs-cli/src/federation.rs` / `crates/hs-cli/tests/`.

### 1. The v2 mount bug, fixed

`crates/hs-federation/src/transport/mod.rs` now exports two router functions:

- **`router(state, x_matrix_ctx)`** -- the v1 router: every read/query endpoint, every remaining
  seam, plus the newly-real `/send`, `make_join` and the v1 `send_join` spelling. Unchanged mount
  point (`/_matrix/federation/v1`).
- **`router_v2(state, x_matrix_ctx)`** -- a **new**, separate router for the v2-only spellings:
  the newly-real v2 `send_join`, plus the still-seam v2 `send_leave` and `invite`. Meant to be
  mounted at `/_matrix/federation/v2` -- a genuinely different mount point, not a `/v2/` path
  segment appended to the v1 router. This is what fixes the bug: v1 and v2 `send_join` share the
  *exact same route string* (`/send_join/{roomId}/{eventId}`) and only the mount prefix
  distinguishes them, so they cannot both be registered in one `Builder` (it would be registering
  the same `(method, path)` twice). `crates/hs-federation/src/transport/seams.rs` gained a
  matching `add_routes_v2` for the two seams that still need it.
- Both routers apply the same `X-Matrix` layer the same way (factored into a shared
  `apply_x_matrix_layer` helper); `transport::seams::tests::every_v2_seam_route_responds_not_implemented`
  and `transport::seams::tests::the_real_write_routes_are_not_registered_as_seams_here` guard both
  halves of this (the second one fails the moment `/send`/`make_join`/`send_join` are accidentally
  re-added as seams here, which is exactly the kind of regression this fix could otherwise invite).

**`hs serve` does not mount `router_v2` yet** -- I do not own `crates/hs-cli/src/serve.rs` this
session. See "Wiring the integration lead must add" below for the exact lines. Until that lands,
`docs/status/routes.json`/the coverage dashboard will keep showing only the v1 spellings; that is
this gap, not a regression.

### 2. `PUT /send/{txnId}`: real, with one honest, load-bearing gap

New module `crates/hs-federation/src/inbound.rs`:

- **`verify_pdu(raw, room_version, key_cache)`**: parses the PDU (`hs_model::Event::parse`),
  recomputes and checks its content hash against its declared `hashes.sha256` (an `Event::parse`
  does *not* do this -- confirmed by reading `hs-model`'s own doc comment on `parse`, which says so
  explicitly), then verifies its signature against the **sender's** server (not the transmitting
  `origin` -- a resident server relays other domains' events, so checking against `origin` would be
  wrong) using `hs_federation::keys::RemoteKeyCache::get_valid_at` (the existing "valid at the time
  it was signed" lookup, not `get_current`, which is what makes an old-but-was-valid key still
  verify).
- **`process_transaction(origin, txn_id, body, rooms, sink, key_cache, transactions)`**: the real
  `/send` algorithm -- checks `(origin, txn_id)` against the idempotency cache first (returns the
  cached response unprocessed if found), rejects the whole transaction with `400` if `pdus.len() >
  50` or `edus.len() > 100` (the spec's resource-limits table), otherwise processes every PDU **in
  order**, building the `{"pdus": {"$id": {}|{"error": "..."}}}` result map, then caches the
  response before returning it. EDUs are parsed for structural validity
  (`crate::edu::parse_edu`, already existed) and otherwise ignored -- no EDU handler exists yet.
- **`RoomWriteSink`** (the seam this session had to invent): `accept_verified_event(room_id,
  event_id, event_json) -> Result<WriteOutcome, WriteRejected>`. This is where the wall is -- see
  "The one gap this session could not close" below.
- **`TransactionStore`** (`get`/`put` by `(origin, txn_id)`), with an `InMemoryTransactionStore`.
  Deliberately in-memory only, not `KvBackend`-backed: the realistic threat this defends
  (a network-level retry of a transaction whose response was lost) does not survive a process
  restart anyway on the sending side either, and a `KvDestinationStore`-shaped persistent version is
  a mechanical follow-up, not a design question -- noted under "Next" rather than built this session
  to leave budget for the join handshake.
- Wired into the transport layer at `crates/hs-federation/src/transport/send.rs` (the axum handler:
  extracts the `X-Matrix`-verified origin, parses the body, calls `process_transaction`, maps
  `TransactionError` to `400`).

### 3. `make_join` / `send_join` (v1 and v2)

New module `crates/hs-federation/src/join.rs`, wired at
`crates/hs-federation/src/transport/join.rs`:

- **`make_join` is fully real.** It reads the room's actual current state and actual forward
  extremities (see the two new `RoomDataSource` methods below) and builds a genuine unsigned
  `m.room.member` template: real `room_version` (checked against the requester's `?ver=` list,
  `M_INCOMPATIBLE_ROOM_VERSION` if none match), real `prev_events`/`depth` from the room's actual
  extremity, and real `auth_events` selected via `hs_state::auth::expected_auth_types` against
  current state -- the actual algorithm the spec names, not a re-derivation of it. It also runs
  `hs_state::auth::check_event_auth` as a courtesy pre-check (rejects a hopeless join, e.g. no
  invite in an invite-only room, before handing out a template that `send_join` would refuse
  anyway).
- **`send_join` validates for real**: `crate::inbound::verify_pdu` (signature + content hash) on
  the submitted event, shape checks (it is `m.room.member`, `content.membership == "join"`, its
  `room_id` matches the path, and -- the check that actually matters for security -- its
  `sender`'s server matches the requester that signed the HTTP request, so one server cannot submit
  a join "on behalf of" another server's user), then `hs_state::auth::check_event_auth` against
  real current state. What it does **not** run: `hs_state::auth::check_auth_events_selection` (the
  state-*independent* half of the spec's checks) or the spec's required three-snapshot check
  (implied-by-`auth_events`, before-the-event, current-at-receipt) -- `hs-room`'s own
  `pipeline.rs` documents this exact gap as "still track 06's job" for a *locally* authored event,
  and it is out of scope here too, for the same reason: doing it properly needs the DAG-walking
  machinery this session did not have budget to build on top of the persistence gap below.
- Two new `RoomDataSource` methods (`crates/hs-federation/src/room_source.rs`), needed by both:
  `room_version(room_id) -> Option<String>` and `forward_extremities(room_id) ->
  Result<Vec<(String, i64)>, RoomSourceError>`, plus `state_for_join(room_id) ->
  Result<StateForJoin, RoomSourceError>` (current state + its auth chain, **not** gated by
  `is_visible_to` -- handing a prospective joiner's server what it needs to construct and authorize
  a join is the entire point of the handshake, not a bypass of the membership check that gates
  ordinary reads; the join itself is authorized separately). `hs-cli`'s `RegistryRoomSource`
  implements all three against `hs-room`'s existing public `RoomActor` API
  (`full_state`/`paginate`/`room_version`) -- no `hs-room` change needed for the read side.
- **Only `EventsReferenceFormat::V2IdOnly` room versions are supported** (bare event-ID references
  in `prev_events`/`auth_events` -- room versions 3 through 12). Room versions 1-2
  (`V1WithHash`, `[event_id, {"sha256": ...}]` pairs) are explicitly rejected by `make_join`/
  `send_join` with `UnsupportedRoomVersion`: computing that pair for a forward extremity needs the
  extremity's full event body, which `forward_extremities` does not carry (only `(id, depth)`).
  This server's own default and only-tested room version is 11 (`V2IdOnly`), so this is a narrow,
  named gap, not a silent one.
- **Faster joins (MSC3706/MSC4229, `omit_members`) are explicitly out of scope**, as this session's
  brief said to declare up front. Every response is the full, unabridged state and auth chain;
  `members_omitted` is always `false`.
- v2's response adds `event` (the verified event, echoed back) and `members_omitted: false` on top
  of v1's `state`/`auth_chain`/`origin`.

### The one gap this session could not close: persisting a newly-received event

Both `/send` and `send_join` bottom out at `RoomWriteSink::accept_verified_event`. Every
implementation this session can offer -- `crate::inbound::StaticWriteSink` (this crate's own
tests) and `hs_cli::federation::RegistryWriteSink` (the real one, wrapping `hs-room`'s
`RoomRegistry`) -- can only ever report success for an event **this server already holds** (checked
by event ID against the resident `RoomActor`). For a genuinely new event, both return a distinct,
documented error (`error` contains "cannot yet persist"; `send_join`'s HTTP response carries
`errcode: M_HS_INBOUND_INGESTION_UNSUPPORTED`, `501`) rather than a generic seam response or -- far
worse -- a fake success.

**Why**: `hs-room`'s `RoomActor` (`crates/hs-room/src/actor.rs`) has exactly two ways to add an
event to a room, and both build and sign a **new** event from scratch using this server's own
identity (`send_event`/`send_event_citing`, both calling `pipeline::build_and_authorize`, which
takes a `NewEvent { event_type, state_key, sender, content, redacts }` -- content and metadata
only, never an already-built `Event`). There is no entry point that takes an already-signed,
already-hashed, foreign `hs_model::Event` and persists it as-is. `hs-room`'s own `pipeline.rs` names
this precisely: its doc comment says the general inbound-event check "is still track 06's job --
see `docs/design/04-room-actor-protocol.md`'s `Command::PersistInbound`" -- a command that was
*named* in the design doc but never implemented, by either track. I do not own `crates/hs-room` and
did not edit it. I considered and rejected re-authoring the received event locally (calling
`membership_action`/`send_event` with the remote user as `sender` but signed by this server's own
key): that would silently produce a cryptographically wrong event (signed by the wrong server for
its sender's domain) that every other real homeserver in the room would reject, and would give this
event a *different* ID than the one the joining server has -- exactly the kind of half-built
handshake this project's own conventions call more dangerous than an honest error.

**What `hs-room` needs, concretely, to lift this** (for whoever picks up
`docs/design/04-room-actor-protocol.md`'s `Command::PersistInbound`, likely track 04):
a `RoomActor` entry point along the lines of

```rust
/// Persists an already-verified, already-authorized foreign event (its signature and content hash
/// checked by the caller, e.g. `hs_federation::inbound::verify_pdu`) exactly as received: no
/// re-signing, no `event_id` regeneration. Computes `depth`/forward-extremity bookkeeping from the
/// event's own `prev_events` (which may not be this actor's current extremities -- unlike
/// `send_event`, this does not get to assume convergence) and feeds `hs_state::api::StateStore` the
/// same way `RoomActor::persist` already does for a locally-built event.
pub fn accept_remote_event(&mut self, event: hs_model::Event) -> Result<(), RoomError>;
```

with the caller (this crate, or whoever owns the inbound pipeline) responsible for everything
`hs_state::auth` needs *before* calling it (the three-snapshot check this session also did not
build) and this method responsible only for the mechanical parts `RoomActor::persist` already knows
how to do (intern, write, update the state store, update forward extremities, publish a
`RoomUpdate`) minus the "build and sign a new event" half that does not apply to a foreign one.

### Mutation-tests performed this session

Per this session's instructions, both required guarantees were actually broken (not just reasoned
about) and the tests re-run to confirm they catch it, then reverted:

1. **Signature check disabled**: in `crate::inbound::verify_pdu`, short-circuited to `return
   Ok(event)` immediately after the key lookup, skipping the `signing::verify_object` call
   entirely. Result: `inbound::tests::verify_pdu_rejects_a_tampered_signature_with_hash_intact`
   failed, as did `hs-cli`'s end-to-end `federation_writes::send_rejects_a_pdu_with_a_tampered_signature`
   (a real signed PUT against the real composed router). **Notably, two *existing* tests did
   *not* catch this mutation**: `verify_pdu_rejects_a_tampered_body` (it tampers the *content*,
   which the independent content-hash check catches before signature verification is ever
   reached) and `verify_pdu_rejects_a_signature_from_the_wrong_key` (the wrong-key scenario is
   caught by the key-lookup step, upstream of `verify_object`). This is exactly the kind of gap
   this project's conventions warn about -- a test with the right *name* that is not actually
   exercising the code path it claims to -- so a new test,
   `verify_pdu_rejects_a_tampered_signature_with_hash_intact`, was added specifically to tamper
   *only* the signature bytes with the hash left intact, isolating the check. That test and the
   `hs-cli` integration test are the ones that actually prove this guarantee; the two older ones
   prove different (also real) guarantees and are kept.
2. **Idempotency disabled**: in `crate::inbound::process_transaction`, wrapped the `(origin,
   txn_id)` cache lookup in `if false { ... }`. Result:
   `inbound::tests::replaying_a_transaction_id_does_not_reprocess` failed immediately (its sink
   panics if called a second time for the same transaction, which is exactly what the disabled
   idempotency check let happen).

Both mutations were reverted immediately after confirming the failure; `cargo test -p hs-federation
--lib` is green at 114/114 with both reverted (see "Verification").

## Wiring the integration lead must add

Not done this session (`crates/hs-cli/src/serve.rs` is owned by the integration lead this
session). In `serve.rs`, the existing federation-mounting block

```rust
if let Some((state, x_matrix, own_keys, server_name)) = federation {
    let (federation_router, federation_manifest) =
        hs_federation::transport::router(state, x_matrix);
    // ...
    builder = builder
        .get("/_matrix/key/v2/server", /* ... unchanged ... */)
        .merge_router("/_matrix/federation/v1", federation_router, federation_manifest.routes);
}
```

needs to become (clone `state`/`x_matrix` -- `FederationState` derives `Clone`, `x_matrix` is
already an `Arc` -- so both routers see the same data sources and the same key cache):

```rust
if let Some((state, x_matrix, own_keys, server_name)) = federation {
    let (federation_router, federation_manifest) =
        hs_federation::transport::router(state.clone(), x_matrix.clone());
    let (federation_router_v2, federation_manifest_v2) =
        hs_federation::transport::router_v2(state, x_matrix);
    // ...
    builder = builder
        .get("/_matrix/key/v2/server", /* ... unchanged ... */)
        .merge_router("/_matrix/federation/v1", federation_router, federation_manifest.routes)
        .merge_router("/_matrix/federation/v2", federation_router_v2, federation_manifest_v2.routes);
}
```

That is the only change needed there. `crate::federation::build_mount` and
`crate::federation::manifest_only_mount` (both in `crates/hs-cli/src/federation.rs`, which I do
own) already build a `FederationState` with the new `write_sink`/`transactions` fields populated
for real, so no other `hs-cli` change is needed to pick this up -- `cargo run -p hs-cli --bin hs
routes-manifest` will then list `send_join`/`send_leave`/`invite` under
`/_matrix/federation/v2/...` for the first time.

## Verification

```
cargo fmt -p hs-federation -p hs-cli                                   # applied, no diffs after
cargo clippy -p hs-federation --all-targets -- -D warnings             # clean
cargo clippy -p hs-cli --all-targets --no-deps -- -D warnings          # clean (see note below)
cargo test -p hs-federation --lib                                     # 114 passed, 0 failed
cargo test -p hs-cli --test federation_reads                          # 7 passed, 0 failed
cargo test -p hs-cli --test federation_writes                         # 6 passed, 0 failed (new this session)
cd crates/hs-federation/fuzz && cargo check                            # clean (5 bins type-check, unchanged)
```

Note on the `hs-cli` clippy command: without `--no-deps`, `cargo clippy -p hs-cli` also lints
every path-dependency crate in the workspace as part of the same build, and `hs-push` (not this
track's crate) currently fails `-D warnings` on unrelated `result_large_err` lints. That is
pre-existing and not something this session touched; `--no-deps` scopes the check to `hs-cli`'s
own code, which is clean.

`cargo run -p hs-cli --bin hs routes-manifest` still only lists the v1 federation spellings
(`send_join`/`send_leave`/`invite` under `/_matrix/federation/v1/...`) because `router_v2` is not
mounted in `serve.rs` yet — see "Wiring the integration lead must add" above.

## In progress

Nothing mid-file. The eighth session's work (`crate::sender`, the `send_join` forwarding, the
`hs-cli` feeder and wiring, the admin pending counts) is complete and green; what it deliberately
does not do is listed at the top of "Next" below.

Earlier sessions' note, still accurate: Everything listed under "Done" (second/third session) and above (fourth session)
is a complete, tested unit, except the one named gap (`RoomWriteSink` cannot persist a new event —
see above) which is honestly reported as a gap, not left half-built. The sixth session's own work
(TLS/CA config surface, `verify_pdu`'s redaction fix) is likewise complete and fully green within
this crate; the one thing left genuinely unfinished is outside this crate's ownership -- see item 0
below and `docs/rfcs/0014-event-signing-must-sign-the-redacted-form.md`. **RFC-0014 has since been
applied by track 04** (`crates/hs-room/src/pipeline.rs` now signs the redacted form; confirmed this
(seventh) session by re-running `cargo test -p hs-cli --test federation_writes` -- 8/8 pass, not
4/8). The seventh session's own work (the `client_config`/discovery bug fixes,
`crate::outbound_join`, `hs federation-join-room`, the two-server script) is likewise complete and
fully green within this crate and `hs-cli`; the one thing left genuinely unfinished is, again,
outside this crate's ownership -- see `docs/rfcs/0015-outbound-join-needs-a-room-bootstrap-api.md`,
addressed to track 04.

## Next (for whoever resumes this track)

Superseded from earlier sessions' lists (wiring `hs-federation` into `hs serve`, the real
`RoomDataSource` adapter, `HttpKeyServerFetcher`, the key-server axum handlers) are all done as of
the third and fourth sessions and removed from this list. What remains:

New after the eighth session (the outbound sender exists; these are what it still lacks):

- **Persist the outbound queue and add catch-up.** `crate::sender` is in memory: a restart loses
  everything unaccepted and nothing is resent afterwards. The target is `PLAN.md` 5.2 item 6
  (per-destination queues sharded by destination hash, persisted queue state) with a
  `destination_rooms`-style "last position sent per destination" so an outage is caught up from
  the room's own history rather than from a queue -- which also closes the `Lagged` hole in
  `hs_cli::federation_sender` (a missed update is a lost event today) and makes the sender
  shard-gated on `hs-cluster` ownership explicitly instead of relying on room-actor residency.
- **EDUs.** Nothing outbound: typing, presence, receipts, device-list updates, to-device,
  signing-key updates. Needs its own queue (coalescing rules differ per kind) -- deliberately no
  `enqueue_edu` seam was left, see the module docs.
- **Invites over federation** (`PUT /invite` v1/v2, client role) and the client role of
  `make_leave`/`send_leave`, `make_knock`/`send_knock`: separate handshakes, not `/send`.

-1. **(New, urgent, not this track's crate)** `hs-room` needs a room-bootstrap API so a federated
   join's verified state snapshot can become a real local room, not just a verified-and-discarded
   one. Filed as `docs/rfcs/0015-outbound-join-needs-a-room-bootstrap-api.md`, addressed to track
   04, with the exact shape of the needed entry point. Until this lands, `hs
   federation-join-room`/`crate::outbound_join::join_room` (this session) proves the handshake and
   every signature real and live, and the resident server genuinely persists the join -- but the
   joining server's own user can never sync or post into the room. This is the single largest gap
   standing between this workspace and "two servers, both directions, both send messages."
0. ~~**(not this track's crate)** `crates/hs-room/src/pipeline.rs` signs outgoing events over their
   full, unredacted form instead of the redacted one the spec requires~~ -- **fixed by track 04**
   since the sixth session (confirmed this (seventh) session:
   `cargo test -p hs-cli --test federation_writes` is 8/8, not 4/8). RFC-0014 is closed.
1. ~~**`Command::PersistInbound` on `hs-room`'s `RoomActor`**~~ -- built by track 04 between
   sessions (`RoomActor::accept_remote_event`), consumed by the fourth session
   (`RegistryWriteSink`) and, as of this (fifth) session, actually reachable end to end: the
   backfill loop means an event citing history from before a join can now be resolved, not just
   accepted when its ancestors happen to already be present.
2. **The three-snapshot auth check** for both `/send`'s PDUs and `send_join`'s event
   (implied-by-`auth_events`, before-the-event, current-at-receipt state, per the server-server
   spec) -- this session's `send_join` only checks against *current* state, and `/send`'s PDUs are
   not authorization-checked at all (only signature/hash-verified) since there is nothing to gain
   from authorizing an event this server cannot yet persist either way. Both depend on (1) existing
   first: authorization checking three snapshots of a DAG this server cannot record is a check with
   nowhere to attach its result.
3. **`make_leave`/`send_leave`, `make_knock`/`send_knock`, `invite` v1/v2**: still seams (correctly
   mounted, including at the now-fixed v2 path for `send_leave`/`invite`). Same shape of work as
   `make_join`/`send_join`, and blocked on the same persistence gap for the "send" half of each
   pair; `make_leave`/`make_knock` (the read/template half) could be done independently and would
   follow `make_join`'s pattern closely.
4. **Server ACL enforcement wiring**: `acl.rs`'s `is_allowed` function exists and is tested in
   isolation, but nothing calls it yet -- it needs threading into (a) the inbound accept path
   (natural home: `crate::inbound::process_transaction`, reading the room's `m.room.server_acl` via
   a new `RoomDataSource` method) and (b) `crate::client::FederationClient::send`. Unblocked by this
   session's work (the `RoomDataSource` real adapter exists now) but not done this session --
   budget went to the write paths per this session's explicit priority order.
5. **A `KvBackend`-backed `TransactionStore`**, mirroring `destination_store.rs`'s
   `KvDestinationStore` pattern, if a transaction retry surviving a server restart ever turns out to
   matter in practice (see `crate::inbound`'s module doc for why in-memory was judged sufficient
   this session).
6. **Discovery result caching in `client.rs`** (carried over, unchanged): `client_for` caches the
   pinned `reqwest::Client` per destination but only invalidates it when a *fresh* `resolve()` call
   produces a different `ResolvedServer` -- no proactive TTL-based re-resolution independent of a
   new `send` call. Acceptable for now, noted as a gap.
7. **Backfill only ever asks `origin`** (new this session): `resolve_missing_ancestors` fetches from
   the one server that sent the transaction, never tries a different member of the room if `origin`
   is unreachable or uncooperative. Real deployments often have several servers in a room that could
   answer; this session deliberately scoped to the single-peer case (it is what closes the
   join-then-stall gap named in `docs/next-steps.md`) and left multi-peer fallback as a named gap
   rather than a half-built heuristic for picking among peers this crate has no signal to rank.
8. **`crate::acl::is_allowed` is still not threaded into the backfill path either** -- it inherits
   item 4's gap (nothing calls `is_allowed` anywhere yet), not a new one: once item 4 is done,
   `resolve_missing_ancestors`'s calls to `AncestorFetcher::fetch_backfill` go through the same
   `FederationClient::send` every other outbound call does, so wiring ACL into `FederationClient`
   once covers this path too, with no separate change needed here.

## Blockers

None for this crate's own work -- every deliverable this (seventh) session was asked for is done
and tested inside `hs-federation`/`hs-cli`. Two historical entries, resolved:

- `verify_pdu`'s correctness fix making 4 of `hs-cli`'s 8 `federation_writes` tests fail (sixth
  session) -- **resolved**: track 04 applied RFC-0014's fix since, confirmed 8/8 this session.
- `RoomActor::accept_remote_event` needing to exist on `hs-room` -- resolved between the fourth
  and fifth sessions.

**Current, real blocker for the next milestone ("both directions"), not this session's own work**:
`hs-room` has no room-bootstrap API (`docs/rfcs/0015-outbound-join-needs-a-room-bootstrap-api.md`),
so a federated join this server's own user initiates can be fully verified but never durably
represented locally. Everything up to that boundary is done, tested and proven live; that one gap
is track 04's, not track 06's, to close.

**Environmental, not this track's**: `cargo clippy -p hs-federation --all-targets -- -D warnings`
(without `--no-deps`) currently fails on an unrelated, in-progress `crates/hs-http` change (see
"Seventh session"'s verification section). `hs-federation`'s own code is clean
(`--no-deps` variant passes); this is not a regression this session introduced and not something
this track can fix (`crates/hs-http/**` is out of this session's ownership).

## Interfaces provided

- **Catch-up** (2026-09-30, fifteenth session): `crate::sender::CatchUpSource`
  (`async fn latest_pdu(&self, room_id, destination) -> Result<Option<Value>, String>`; `hs-cli`'s
  `federation_sender::RoomCatchUp` implements it), `FederationSender::{install_catch_up_source,
  install_catch_up_metrics, mark_for_catch_up, catch_up_marks}`,
  `SenderConfig::max_queued_pdus_per_destination`, `crate::metrics::CatchUpMetrics`,
  `crate::outbound_store::{CatchUpMark, Enqueued, RoomBehind}`. `OutboundStore::enqueue` and
  `ack` changed signature (room, bound; sent positions); nothing outside this crate implements
  the trait.
- **`FederationClient::{state_ids, room_state, event}`** (2026-09-30, added by track 04's
  `agent/backfill-state`): the outbound halves of `GET /state_ids/{roomId}?event_id=` (returns
  `(pdu_ids, auth_chain_ids)`), `GET /state/{roomId}?event_id=` (returns `(pdus, auth_chain)`,
  unverified) and `GET /event/{eventId}` (the first PDU, unverified). `hs_cli::backfill` is the
  caller: the state at the oldest event of a backfilled batch. Unit-tested in `client.rs`
  (`state_ids_state_and_event_read_their_answers`). The serving side changed with it, in
  `hs_cli::federation::RegistryRoomSource`: `/state` and `/state_ids` now answer the state
  *before* the event (the spec's and Synapse's meaning; it was the state after), and
  `/state_ids` takes its IDs from the events -- it answered two empty lists for every room of
  version 3 or later, whose PDUs carry no `event_id`.
- **`crate::metrics::{EduMetrics, EduOutcome}`** (2026-09-28): `EduMetrics::register(&mut
  prometheus_client::registry::Registry)` (call through `hs_telemetry::metrics::Metrics::
  with_registry`), `record_sent(edu_type)`, `record_received(edu_type, EduOutcome)`;
  `FederationSender::install_edu_metrics(EduMetrics)`. Whoever applies inbound EDUs counts them.
- **`crate::sender::{FederationSender, OutboundPduSink, SenderConfig, BACKOFF_POLL_INTERVAL}`**
  (new this eighth session): the outbound sender. `FederationSender::new(Arc<FederationClient>,
  own_server_name) -> Self` (wrap in `Arc`), `with_config(.., SenderConfig)`,
  `enqueue_pdu(impl IntoIterator<Item = String>, serde_json::Value)`, `pending_pdus() -> usize`,
  `pending_pdus_for(&str) -> usize`, `pending_by_destination() -> Vec<(String, usize)>`,
  `shutdown()`. Any track that has a PDU to distribute (a future `/invite` sender, a bridge that
  needs to fan out) hands it here. `OutboundPduSink` is the one-method trait to take when a
  recording double is wanted.
- **`crate::transport::FederationState::sender: Option<Arc<dyn OutboundPduSink>>`** and
  **`crate::room_source::RoomDataSource::member_servers(&self, room_id) -> Vec<String>`** (new
  this session): every constructor and implementor must supply them.
- **`crate::join::send_join(rooms, sink, key_cache, room_id, event_id, signed_event, origin,
  own_server_name, forward: Option<&dyn OutboundPduSink>)`**: two new trailing parameters.
- **`crate::admin_source::DestinationStoreSource::with_sender(Arc<FederationSender>)`**: makes the
  admin Federation page's `pending_pdu_count` real. **`FederationClient::max_retry_backoff()`**:
  the ceiling the sender shares.
- **`hs_cli::federation_sender::{OutboundFederation, forward_update}`** (`crates/hs-cli`, new
  this session): `OutboundFederation::start(Arc<RoomRegistry<B>>, Arc<FederationSender>,
  OwnedServerName) -> Self`, `sender()`, `stop()`; `forward_update(&RoomRegistry<B>,
  &FederationSender, &ServerName, &RoomUpdate) -> Result<Vec<String>, RoomError>` is the per-update
  step, exposed so a test (or a future catch-up) can drive it one update at a time.
  `hs_cli::federation::FederationMount::sender: Arc<FederationSender>`.
- **`crate::outbound_join::{join_room, RemoteJoinOutcome, OutboundJoinError}`** (new this seventh
  session): the client-role join handshake -- any track that needs "make this server's user join a
  room hosted elsewhere" (a future `/join` wiring, once RFC-0015 lands, or a bridge/appservice that
  needs the same) calls this directly. Returns a fully verified snapshot; persisting it is the
  caller's job once `hs-room` can (RFC-0015).
- **`hs federation-join-room` CLI subcommand** (`crates/hs-cli`, new this session): drives
  `join_room` from a real config file with no storage open. Diagnostic/administrative today; the
  natural first caller once RFC-0015 lands is `hs-room`'s own client-facing `/join` route (not this
  CLI command, which would then become redundant with it -- kept anyway as a lower-level tool for
  debugging a stuck join without a full client).
- **`crate::client::FederationClient`** (tracks 08, 09, 11 per the brief): `new(...)` takes an
  owned server name, a `SigningKeyPair`, a `ClientConfig`, and `Arc<dyn DestinationStore>` /
  `Arc<dyn WellKnownFetcher>` / `Arc<dyn SrvResolver>` / `Arc<dyn AddrResolver>`; `.send(destination,
  method, path, body) -> Result<FederationResponse, ClientError>` is the one call site. Wired into
  `hs-cli` since the third session.
- **`crate::room_source::RoomDataSource`**: the read-only room seam, now with three more methods
  (`room_version`, `forward_extremities`, `state_for_join`) added this session for the join
  handshake. `hs_cli::federation::RegistryRoomSource` is the real implementation over `hs-room`;
  `InMemoryRoomSource`/`FakeRoom` remain available for any track's own tests.
- **`crate::inbound::{verify_pdu, process_transaction, RoomWriteSink, WriteOutcome, WriteRejected,
  TransactionStore, InMemoryTransactionStore, StaticWriteSink}`**: the inbound PDU-verification and
  transaction-envelope primitives. `WriteRejected` gained a structured `missing_ancestors: Vec<String>`
  field this (fifth) session, plus `WriteRejected::other`/`WriteRejected::missing_ancestors`
  constructors -- any other implementation of `RoomWriteSink` should use these rather than
  constructing the struct literal directly, so a future field addition here does not need every
  implementation to change. `process_transaction`'s signature grew two parameters this session
  (`ancestor_fetcher: Option<&dyn crate::backfill::AncestorFetcher>`, `backfill_limits:
  &crate::backfill::BackfillLimits`) -- pass `None` and `&BackfillLimits::default()` to keep the old
  behaviour (report the gap, do not try to close it).
- **`crate::backfill::{AncestorFetcher, AncestorFetchError, BackfillLimits, BackfillGiveUpReason,
  resolve_missing_ancestors}`** (new this session): the backfill resolution loop described above.
  `FederationClient` implements `AncestorFetcher`; anything that wants to trigger a backfill
  resolution outside `process_transaction` (a future explicit "resync this room" admin action,
  say) can call `resolve_missing_ancestors` directly against any `RoomWriteSink`.
- **`crate::client::FederationClient::backfill(destination, room_id, from_event_ids, limit) ->
  Result<Vec<Value>, ClientError>`** (new this session): the outbound `/backfill` client, returning
  raw unverified PDUs -- callers must run each through `crate::inbound::verify_pdu` themselves.
- **`crate::join::{make_join, send_join, JoinTemplate, SendJoinResult, JoinError, RoomWriteSink}`**
  (new this session): the join-handshake logic, usable directly by anything that wants to build or
  validate a join without going through the axum layer (e.g. a future differential test against
  recorded Synapse traffic, per this track's definition of done).
- **`crate::transport::{FederationState, FederationQuerySource, InMemoryQuerySource, router,
  router_v2}`**: the federation router-fragment functions, both mounted in `hs-cli` as of the fourth
  session. `FederationState` gained two more fields this (fifth) session:
  `ancestor_fetcher: Option<Arc<dyn crate::backfill::AncestorFetcher>>` (`None` disables backfill)
  and `backfill_limits: crate::backfill::BackfillLimits`; any other track constructing one directly
  (none do today, per a repo-wide grep) needs to supply both -- `None` and `BackfillLimits::default()`
  reproduce the pre-this-session behaviour exactly.
- **`crate::xmatrix::{sign_request, verify_x_matrix, XMatrixContext}`**: request signing for any
  track that needs to make an authenticated federation call directly (though `FederationClient`
  should normally be preferred), and the verification middleware/context type for whoever wires
  the federation listener into `hs-cli`.
- **`crate::keys::{OwnSigningKeys, RemoteKeyCache, KeyServerFetcher, DynRemoteKeyCache}`**: key
  management for any track that needs to verify a federation signature outside the request path
  (e.g. verifying a signed `m.room.third_party_invite`).
- **`crate::acl::{ServerAcl, is_allowed}`**: the one ACL evaluation function, for whoever wires
  inbound/outbound enforcement (see "Next" item 4).
- **`crate::client::ClientConfig::{custom_root_certificates, trust_os_root_store}`** (new this
  sixth session): the fields any track constructing a `ClientConfig` directly (none do today
  outside `hs-cli`'s `client_config` conversion function) needs to populate to preserve or opt into
  custom-CA/OS-store trust; both default to "off" (`Vec::new()`/`false`) via `ClientConfig::default()`,
  reproducing pre-this-session behaviour exactly for any caller using `..ClientConfig::default()`.
- **`hs-config::FederationConfig::{custom_ca_certificates, trust_os_root_store}`** (new this sixth
  session): the schema fields; see "Sixth session" above for their doc comments and the reasoning
  behind `trust_os_root_store`'s `false` default.

## Interfaces needed

- ~~**Track 04**: the real `RoomDataSource` adapter over `RoomActorHandle`.~~ Built in the third
  session as `hs_cli::federation::RegistryRoomSource`. ~~What it still needs from track 04 is a
  **state-at-an-event** query~~ -- also lifted the third session (`RoomActor::state_at_event`);
  `/state` and `/state_ids` now answer for any event this server holds, not just the newest.
- ~~**Track 04, the real blocker**: a `RoomActor` entry point that accepts an already-verified,
  already-signed foreign event and persists it as-is.~~ Built by track 04 between the fourth and
  fifth sessions (`RoomActor::accept_remote_event`, `docs/design/04-room-actor-protocol.md`'s
  `Command::PersistInbound`). Nothing further needed from track 04 as of this session.
- **Nothing new needed from another track this (fifth) session** -- the backfill loop is entirely
  built from interfaces this crate already owned (`RoomWriteSink`, `RoomDataSource`,
  `FederationClient`) plus one already-existing `hs-room` entry point (`accept_remote_event`).
- **Track 08 (E2EE)**: `/user/keys/claim` and `/user/keys/query` remain mounted as seams pending
  track 08's contract, per the brief's joint-ownership note.
- ~~**hs-cli / whoever owns `hs serve`'s wiring**: needs to call `crate::transport::router`,
  `crate::client::FederationClient::new`, and load `OwnSigningKeys` at startup.~~ Done in the
  third session. **New this session**: `hs serve`'s wiring also needs to mount `router_v2` at
  `/_matrix/federation/v2` -- see "Wiring the integration lead must add" above; this one is not
  done yet.
- **Track 04 (`crates/hs-room/src/pipeline.rs`), urgent**: needs the redact-then-sign-then-copy-back
  fix described in `docs/rfcs/0014-event-signing-must-sign-the-redacted-form.md` -- this server's
  own outbound events are signed over their full, unredacted form, which any real spec-compliant
  remote homeserver's inbound verification (redact-then-check, matching this session's `verify_pdu`
  fix) would reject whenever the event's content is not fully retained by redaction. Not this
  track's crate to fix.
- **`hs-cli` (whoever owns `crates/hs-cli/tests/federation_writes.rs`)**: three test call sites
  (`build_signed_message` and two inline `sign_object` calls -- see the RFC for exact locations)
  need the same mechanical redact-then-sign fix already applied twice in this crate's own tests
  this session. Confirmed (read-only) these three, plus the one real-pipeline-dependent test named
  above, are the only `federation_writes` failures caused by this session's `verify_pdu` fix.
- ~~**`hs-cli`'s `crates/hs-cli/src/federation.rs::client_config`**: to actually honour the new
  `hs-config` fields end-to-end, needs two more lines...~~ **Done this (seventh) session** -- see
  "Seventh session" §1. This is the track's own crate (`crates/hs-cli/**` is in this session's
  ownership, unlike the sixth session that wrote this item), so it was fixed directly rather than
  filed as a request to another track.
- **Track 04, current**: the room-bootstrap API `docs/rfcs/0015-outbound-join-needs-a-room-bootstrap-api.md`
  asks for -- see "Blockers" above.

## Decisions made

- **Catch-up follows Synapse, with a durable queue in front of it** (2026-09-30). Synapse
  drops its in-memory queue and enters catch-up on the first failure; this server keeps its
  durable queue up to a bound (`federation.max_queued_pdus_per_destination`, 10,000) and only
  past it drops the queue and catches up from the rooms, so a short outage is still replayed
  event by event and a long one costs bounded storage. Positions are the store's own sequence
  numbers, per (destination, room). The catch-up event is the latest *local* one, preferring a
  forward extremity; rooms where the destination has no member joined are skipped. Catch-up
  transactions are attempted once each and recomputed after a failure, not retried as they
  were.
New this (eighth) session:

- **In memory first, persistence next.** The brief asked for the sender that makes "a local event
  reaches remote members" true at all; a `KvBackend`-backed, sharded, catch-up-capable queue is a
  design of its own (see "Next"). The module docs say what a restart loses, in the first
  paragraph, so nobody mistakes this for the plan's section 5.2 sender.
- **Retry everything except this server's own policy refusals.** A non-2xx status, a connection
  or discovery failure, and a destination-store backoff are all "later"; only
  `Disabled`/`DomainDenied`/`IpDenied` are "never", and those drop the transaction with an
  `error` log. A permanently 4xx-ing destination therefore retries at the cap (1h) until restart;
  accepted, since the alternative -- guessing which 4xx codes are permanent -- silently loses
  events on the guesses that are wrong.
- **A retry reuses the transaction ID.** Otherwise a receiver that processed the first attempt
  but whose response was lost would apply the same PDUs under two IDs, defeating its own
  idempotency cache.
- **The sender's own backoff has no jitter.** The destination store already jitters the
  connection-level backoff the client records; a second layer of jitter would only make the
  retry tests non-deterministic. Doubling from 1s, capped at the client's `max_retry_backoff`.
- **A destination in backoff is polled every 30s at most** (`BACKOFF_POLL_INTERVAL`), so an
  administrator's reset takes effect promptly. A poll is a store read, not a network call.
- **Per-PDU rejections are final.** The receiver looked at the event; resending gets the same
  answer. Logged at `warn`, with the receiver's reason.
- **Destinations are "joined after the event, plus the target of a leave/ban".** Equivalent to
  Synapse's "hosts in the room at the event's prev_events" for a locally originated event, using
  the `joined_members_after` `hs-room` already provides rather than asking track 04 for a
  `joined_members_before`. Invite targets are excluded until `/invite` exists.
- **Remote senders' events are never re-sent by the feeder**; the one exception the spec makes --
  the resident forwarding a `send_join` it accepted -- is done at `send_join` itself, once, on
  `Stored` only.
- **A lagged update stream is a lost event, and the log says so.** No catch-up exists to make it
  anything else; pretending otherwise (silently continuing) is the one thing the log must not do.
- **Not shard-gated**, recorded rather than half-built: room-actor residency already makes each
  local event's update reach one replica; a sharded sender belongs with the persisted queue.
- **`hs-testkit` became an `hs-cli` dev-dependency** (path, no workspace root change) so the new
  `hs-cli` test can use `FakeFederationPeer` like every `hs-federation` test does, reversing the
  fifth session's "did not add it" note.
- **`hs_cli::federation_sender::forward_update` is public** so the integration test can assert
  each event's destinations directly rather than infer them from what a fake peer eventually did
  or did not receive ("nothing was sent" is otherwise a claim about a timeout).

New this (seventh) session:

- **The two-server script uses IP-literal server names (`127.0.0.1:8448`/`127.0.0.1:8449`), not
  hostnames.** Discovered live: this crate's `AddrResolver` (`hickory-resolver`) does not consult
  `/etc/hosts`, so a hostname like `"localhost"` resolved through a real, search-domain-configured
  `/etc/resolv.conf` is not guaranteed to reach `127.0.0.1` -- confirmed on this machine's own
  network. IP literals bypass discovery entirely (`crate::discovery` step 1) and are exactly as
  spec-valid a `server_name` as a hostname, so they are the right choice for a script that must be
  reproducible on any machine's network configuration, not a workaround.
- **A malformed-CA-file entry is logged and skipped, not a fatal boot error** (`client_config`'s
  fix, §1): matches `FederationClient::new`'s own existing tolerance for a CA entry that reads but
  fails to *parse*; a path that cannot even be *read* (typo, permissions) should be equally visible
  in the log rather than crashing a server that might otherwise boot and serve local users fine.
- **`hs federation-join-room` opens no storage.** Everything it needs (server name, signing key,
  federation policy) lives in the config file alone; nothing it produces can be durably persisted
  yet regardless (RFC-0015), so there is no room store for it to open. Uses
  `InMemoryDestinationStore` rather than `KvDestinationStore` for the same reason a one-shot
  command has no backoff state worth persisting across runs.
- **`join_room` fails closed on the first unverifiable event in `state`/`auth_chain`**, rather than
  collecting partial results: a resident server that hands back even one event that fails content-
  hash or signature verification is not a resident worth trusting further for this join, matching
  `verify_pdu`'s own all-or-nothing contract for a single PDU.

Previously, sixth session:

- **`trust_os_root_store` defaults to `false`.** Full reasoning in the field's own doc comment
  (`hs-config::FederationConfig::trust_os_root_store`) and in "Sixth session" §2 above; recorded
  here per this session's brief, which asked specifically for this decision to be made and
  justified. Short version: federation authenticates servers that never agreed on a shared root of
  trust ahead of time, so silently trusting whatever the OS happens to trust (which anyone with
  root can broaden, for reasons unrelated to this server) is a quiet regression as an *unconditional
  default*; `custom_ca_certificates` is the explicit, narrow alternative, and the operator chooses
  either per deployment.
- **`ClientConfig::custom_root_certificates` takes raw PEM bytes, not file paths.** Considered
  taking `Vec<String>` (paths) directly, matching `hs-config`'s own field, and rejected it: this
  crate does not otherwise do filesystem I/O anywhere (`OwnSigningKeys::load_or_generate` is the one
  exception, and that is a different, already-established seam), and keeping `ClientConfig` free of
  I/O let this session's own TLS test hand it certificate bytes straight from `rcgen` with no
  filesystem involved at all. File reading is one `std::fs::read` per configured path at the
  `hs-cli` wiring site, which already does config-loading I/O.
- **A parse failure in `custom_root_certificates` is logged and skipped, not fatal.** Considered
  making `FederationClient::new` fallible (returning `Result`) so a malformed CA file could be a
  hard startup error, and rejected it: every other construction path in this crate today is
  infallible (`FederationClient::new` returns `Self`, not `Result<Self, _>`), and changing that
  signature would touch every call site (`hs-cli`, every test in this crate) for a case that is
  already loud (`tracing::error!` naming the exact index that failed) without also making a
  single malformed file a hard crash for a server that might otherwise start up fine on its public
  roots alone.
- **The `verify_pdu` redaction fix was kept despite the collateral `hs-cli` test failures it
  causes.** See "Sixth session" §5-6 and the RFC. Considered reverting to avoid the 4 failing
  `hs-cli` tests and rejected it: the fix is objectively spec-correct (quoted directly from
  `refs/matrix-spec/content/server-server-api.md`), it is what actually resolves the named target
  bug (`send_join` rejecting a real, correctly-signed join), and `cargo test -p hs-federation` (this
  crate's own, complete responsibility) is fully green with it in place. The failures it exposes in
  `hs-cli` are in code this session does not own, are precisely diagnosed, and are documented with
  an exact fix rather than silently left for someone else to rediscover.
- **The companion bug in `hs-room/src/pipeline.rs` was documented as an RFC rather than left as a
  one-line status-file mention.** Considered just noting "hs-room has the same bug" in this file's
  "Next" list and decided the severity (every outbound event with non-trivial content is
  mis-signed, for every remote federation partner) warranted the fuller treatment `docs/rfcs/`
  gives -- an exact reproduction, an exact patch shape with the current file's variable names
  confirmed by reading it, and an exact list of the affected `hs-cli` test call sites, so track 04
  does not have to re-derive any of it before applying the fix.

New this (fifth) session:

- **The backfill target is always `origin`** (the server that sent the transaction reporting the
  gap), never a different member of the room. Considered trying every joined server this crate
  knows about and rejected it for this session: this crate has no signal to rank peers by
  reliability, and falling back through an unranked list on every failure risks turning one
  hostile or slow peer into several round-trips' worth of wasted work for a gap that peer alone
  cannot close either. `origin` is also the server most likely to actually have the history (it is
  the one that just cited it), so it is the correct first (and, this session, only) choice. Recorded
  as "Next" item 7, not treated as a design flaw.
- **`/backfill` was chosen over `/get_missing_events` as the fetch primitive**, per the brief's
  explicit permission to pick "as the spec and the situation decide". `/backfill` takes exactly
  "the IDs I'm missing" and a limit and walks backwards from them, which is precisely this
  session's situation (an event named specific ancestor IDs this server does not hold);
  `/get_missing_events` additionally requires communicating this server's own `earliest_events`
  frontier to the peer, which needs a concept of "this room's backward frontier from this server's
  point of view" that `RoomDataSource` does not expose today and that this session judged
  unnecessary complexity for the case actually being solved. `/get_missing_events`'s outbound
  client was not built this session; noted as a possible future addition, not a gap in what this
  session was asked to close.
- **`WriteRejected` grew a structured field instead of a new error type.** Considered a
  `Result<WriteOutcome, WriteError>` where `WriteError` is an enum with a `MissingAncestors(Vec
  <String>)` variant, and rejected it: every existing caller (`crate::join::send_join`,
  `crate::inbound::process_transaction`, `hs_cli::federation::RegistryWriteSink`, this crate's own
  tests) already matches on `WriteRejected { error, .. }` as a struct; changing the error's *type*
  would touch every one of those call sites for a change that is really just "add one more field
  most callers will ignore". The two new constructors (`WriteRejected::other`,
  `WriteRejected::missing_ancestors`) keep every construction site from having to remember to set
  the new field explicitly, and are the only sites this session changed.
- **The resolution loop tracks a `pending` worklist across rounds, not just within one**, per the
  real multi-hop bug this session's own test caught (see "Fifth session" above). This is the one
  piece of this session's design that changed shape *after* being written and tested, not before --
  recorded here so the reasoning is not lost: an event fetched in round N that cannot yet be
  persisted must still be attempted again in round N+1 once whatever blocked it lands, not
  discarded the moment its first attempt fails.
- **A hard-rejected fetched event (bad auth, malformed, ...) is silently dropped, not retried and
  not reported as part of the eventual give-up reason.** Considered surfacing it (e.g. a
  `BackfillGiveUpReason::AncestorRejected(id, reason)` variant) and decided the extra type
  complexity was not worth it: a hard rejection during backfill is not actually a *backfill*
  failure -- it means a fetched event failed the same authorization check any inbound event would
  fail, which is already a defended, tested code path (`crate::inbound::verify_pdu`,
  `RoomActor::accept_remote_event`'s own authorization). The caller's eventual retry of the
  original event will report whatever ancestor is *still* actually missing (if the hard-rejected
  event was itself required), which is the information that actually matters to whoever reads the
  `/send` response.

New this (fourth) session:

- **`hs-state` was added as a direct dependency of `hs-federation`** (`Cargo.toml`, path
  dependency, not a `[workspace.dependencies]` entry -- matches how `hs-model`/`hs-kv`/etc. are
  already declared here). `crate::join` calls `hs_state::auth::{check_event_auth,
  expected_auth_types}` directly rather than re-deriving the join-rules/power-level/membership auth
  rules a second time, per this session's explicit instruction ("not a second copy of the auth
  rules"). No cycle risk: `hs-state` depends on nothing above it in the crate graph.
- **`RoomWriteSink` is a new, separate seam from `RoomDataSource`**, not a fourth read method
  bolted onto the existing (deliberately read-only, per its own doc comment) trait. `/send` and
  `send_join` share exactly one write seam rather than each inventing their own, which is what
  makes the "one honest gap" in this session's write-up a single, named thing instead of two.
- **`send_join`'s persistence-gap error is a `501` with a distinct errcode
  (`M_HS_INBOUND_INGESTION_UNSUPPORTED`)**, not a `200` with a state/auth_chain response that
  claims success. Considered and rejected: returning `200` would tell a real remote server its join
  succeeded when this server recorded nothing, which is a worse failure mode than an honest error --
  the remote would proceed to sync and participate in a room it believes it joined while this
  server's data never reflects the membership.
- **Re-authoring a received foreign event with this server's own identity, to make persistence
  "work", was considered and rejected** (see "The one gap this session could not close" above for
  the full reasoning): it would produce a cryptographically wrong event (signed by the wrong
  server for its sender's domain) with a different ID than what the joining server holds, which
  every other real homeserver in the room would reject on sight. An honest, typed error that
  proves everything *up to* persistence is real is safer than a handshake that looks complete and
  is quietly wrong.
- **`/send`'s PDUs are not authorization-checked against room state this session, only
  signature/hash-verified.** Considered doing a single-snapshot `check_event_auth` (as `send_join`
  does) and rejected it for `/send` specifically: since no PDU can be persisted regardless of the
  outcome (see the gap above), an auth check here would only ever change *which* error message an
  operator sees, at the cost of building `FlatState` for every PDU in every transaction. Revisit
  once (1) under "Next" exists and an accepted PDU has somewhere to go.
- **Only `EventsReferenceFormat::V2IdOnly` room versions are supported by the join handshake**
  (documented above under item 3) -- a deliberate, narrow scope decision given this server's
  default and only-tested room version (11) is already in that family, rather than building
  `V1WithHash` reference-pair support for forward extremities that would need a `RoomDataSource`
  signature change (fetching full event bodies, not just `(id, depth)`) to support two room
  versions this server has never been run against.

From the third session (the second session's decisions, still valid, are below under "Decisions
made (first session, unchanged)"):

- **`ClientConfig` is decoupled from `hs_config::FederationConfig`** (its own struct in
  `client.rs`, with `DomainPolicy`/`IpPolicy` built from plain `Vec<String>` CIDR/domain lists via
  `from_cidrs`/`new`, not from `hs-config`'s type directly). Keeps `client.rs` testable without a
  dependency on `hs-config`'s validation/schema machinery and keeps the conversion (a handful of
  lines) at the wiring site where `hs-config` is already in scope, rather than making this crate's
  core client logic depend on another crate's config schema shape. `hs-config` remains a
  `Cargo.toml` dependency of this crate (used nowhere yet — it was speculatively added in the
  first session's plan; still fine to keep, since `ServerConfig`/`FederationConfig` will be read at
  the `hs-cli` wiring point that lives logically alongside this crate's own types, even if not
  literally inside `client.rs`).
- **`ClientConfig::scheme` is a test-only seam, `"https"` by production default, never settable by
  any config loader.** Added specifically so `client.rs`'s own tests could exercise real discovery
  + signing + pooling + concurrency + backoff against `hs_testkit::FakeFederationPeer` over a real
  loopback socket, without also having to stand up a self-signed TLS certificate and trust chain
  (which would mostly be testing `reqwest`'s already-tested TLS stack, not this crate's logic).
  Documented inline on the field itself; not exposed through `hs-config`.
- **The `X-Matrix` middleware re-parses the `Authorization` header per read-route handler**
  (`transport::read_routes::requesting_server`) rather than threading the already-verified
  `origin` through axum request extensions from `verify_x_matrix`. This is a deliberate
  simplification for this pass, not an oversight: parsing is infallible at that point (the layer
  already proved the header is well-formed enough to have signed correctly) and cheap, and it
  avoids introducing an extension-passing contract between two independently-testable modules
  (`xmatrix` and `transport`) before there's a second consumer that would justify it. Revisit if a
  profiler ever cares, or if a second handler module needs the same value and the duplication
  starts to feel real rather than theoretical.
- **`RoomDataSource::get_event_by_id`** (room-less event lookup) was added beyond what the first
  session's plan enumerated for the trait, because `/event/{eventId}`'s actual spec path shape has
  no room ID in it — a server has to know which room an event belongs to before it can apply the
  membership check. `InMemoryRoomSource`'s implementation linearly scans every room, which is fine
  for a test fake and would not be for a real adapter (track 04's adapter should back this with an
  actual event-ID index, not a scan).
- **`edu.rs` was added, beyond the original plan's file list**, purely to give the required "EDU
  JSON" fuzz target a real parser to call rather than fuzzing raw `serde_json::Value` parsing with
  no structural validation at all. It intentionally does not interpret any `edu_type`'s `content`
  schema — that's real work for whichever session first implements EDU handling inside `/send`.
- **`RemoteKeyCache::ingest_response` and `discovery::parse_well_known_body` were made `pub`**
  (the former was previously private, the latter was extracted from being inline in
  `HttpWellKnownFetcher::fetch`) specifically so both are independently fuzzable without needing a
  live network fetcher to drive them. This also had the side benefit of eliminating a
  near-duplicate parsing path in `HttpWellKnownFetcher::fetch`, which now calls the same function
  the fuzz target does.
- **`transport::router` composes both sub-routers into one `Builder` before calling `.build()`
  once**, rather than building two separate `axum::Router`s and merging them via
  `Builder::merge_router`. Not a style preference: `Builder::merge_router` nests the incoming
  router under a prefix via `axum::Router::nest`, and axum panics ("Nesting at the root is no
  longer supported") when that prefix is `""` — which it must be here, since `read_routes` and
  `seams` both register spec-relative paths (`/version`, `/send/{txnId}`, ...) that are meant to
  live at the same level, not under a sub-prefix. Discovered by the test suite immediately (both
  `transport::tests` cases panicked at construction), not by production use. `read_routes` and
  `seams` therefore expose `pub(super) fn add_routes(builder) -> builder` (composable into a
  shared `Builder`) rather than `pub(super) fn router() -> (Router, Vec<Route>)`.

## Decisions made (first session, unchanged)

- **`RoomDataSource` is a trait owned by `hs-federation`, not a dependency on `hs-room`** — see
  threat model section 5. Confirmed correct by this session's own experience: `InMemoryRoomSource`
  made every `transport::read_routes` handler fully testable today, with the real adapter still
  entirely someone else's future work.
- **No federation route is exposed before `X-Matrix` verification wraps the whole router** — now
  enforced by an actual test (`transport::tests::every_route_is_behind_the_x_matrix_layer`), not
  just a stated intention.
- **Join/leave/knock/invite handshakes and `/send` ship as seams this pass** — done exactly as
  specified: signature verification (via the shared layer), a shared typed 501, nothing else.
- **`hickory-resolver` is the DNS resolver** — added and in use (`discovery::HickoryResolver`).
- **HTTP/1.1-only to federation peers** — committed in code (`client.rs`'s `client_for`:
  `.http1_only()` on every outbound `reqwest::Client`).

## Reuse considered (decision 0007)

Unchanged from the first session's analysis (`hickory-resolver`, `reqwest`, `hs-model`'s
canonical/signing/hashing, `ruma-federation-api` considered-but-not-adopted-for-handlers), plus:

- **`ipnet`** (new this session): adopted for CIDR parsing/containment
  (`client::IpPolicy`) rather than hand-rolling prefix-length arithmetic over `IpAddr`. Already
  present transitively via `hickory-net`; promoting it to a direct dependency costs nothing new in
  the dependency tree and avoids a bug-prone reimplementation of CIDR containment (off-by-one
  errors in prefix-length masking are a classic source of exactly the kind of SSRF-adjacent bug
  this check exists to prevent).
- **`rcgen`** (dev-dependency only, unused in the end): added anticipating a real-TLS test harness
  for `client.rs`'s integration tests, then not used — `ClientConfig::scheme`'s plaintext-test seam
  turned out to test this crate's own logic more directly without exercising `reqwest`'s TLS stack
  (someone else's already-tested code). Left in `Cargo.toml` as a dev-dependency in case a future
  session wants a real-TLS test after all; flagged here so it isn't mistaken for dead weight
  without an explanation.

## Shared dependencies added

This (eighth) session: **no root `Cargo.toml` change.** `crates/hs-cli/Cargo.toml` gained
`hs-testkit = { path = "../hs-testkit" }` under `[dev-dependencies]` (already a workspace crate,
already a dev-dependency of `hs-federation`; no cycle -- `hs-testkit` depends on no `hs-cli`).
`crates/hs-federation/Cargo.toml` is unchanged: `crate::sender` is built from `tokio`,
`serde_json` and `tracing`, all already depended on.

This (seventh) session: **none.** No `Cargo.toml` in either owned crate (`hs-federation`,
`hs-cli`) changed; `crate::outbound_join` and `hs federation-join-room` are built entirely from
types and crates both already depended on.

Sixth session: **no new `[workspace.dependencies]` root-`Cargo.toml` entries** -- every
crate this session's `hs-federation/Cargo.toml` change touches (`reqwest`'s extra feature;
`rustls`, `tokio-rustls`, `rustls-pki-types`, `hyper`, `hyper-util`, `http-body-util` as new
dev-dependencies) was already a workspace-level dependency used by some other crate, so nothing
needed fetching and no root `Cargo.toml` edit was needed or made. The one change worth flagging
explicitly, since it does grow this crate's own compiled dependency tree even though it touches no
shared workspace entry: `crates/hs-federation/Cargo.toml`'s `reqwest` line gained the
`rustls-tls-native-roots` feature (on top of the workspace's existing `rustls-tls`), which pulls in
`rustls-native-certs` and its platform-specific dependencies (`security-framework` on macOS,
`schannel` on Windows) as compiled code for this crate specifically -- see "Sixth session" §2 for
why (it is what makes `trust_os_root_store` a real, working toggle rather than a documented no-op).

None this (fifth) session -- the backfill loop is built entirely from crates already depended on
(`tokio` for `time::timeout`, `async-trait`, `ruma`, `serde_json`), and `hs-cli`'s two new
integration tests use only `axum`/`tokio` (already plain, non-dev dependencies of `hs-cli`) rather
than adding `hs-testkit` as a dev-dependency there (see "Decisions made" -- not actually a decision
this session made explicitly, but worth noting: `hs-testkit::fake_federation::FakeFederationPeer`
would have been a natural fit for the two new `hs-cli` tests' "remote server" double, and *is* used
for `crate::client`'s own new unit test in this crate, but `hs-cli/Cargo.toml` is not a file this
session owns, so the `hs-cli` tests build their own minimal axum catch-all instead).

- **`hickory-resolver`** (0.26, workspace): added the second session. Features: `tokio` (brings
  `system-config` along via its own defaults).
- **`ipnet`** (2, workspace): added the second session, for `client::IpPolicy`'s CIDR containment
  checks.
- **`hs-state`** (path dependency, this crate's own `Cargo.toml`, not a `[workspace.dependencies]`
  entry): added this (fourth) session, so `crate::join` can call the real
  `hs_state::auth::{check_event_auth, expected_auth_types}` rather than re-deriving the auth rules.
  See "Decisions made" above.

The workspace-level entries (`hickory-resolver`, `ipnet`) are noted with "Added by track 06"
attribution comments in the root `Cargo.toml`; `hs-state` needed no root `Cargo.toml` change since
internal crate-to-crate path dependencies are declared directly in each crate's own manifest (the
same way this crate already depends on `hs-model`/`hs-kv`/etc.).
