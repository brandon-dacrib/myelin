# Status: track 11, appservices and bridges

Last updated: 2026-10-09, later (the manager says, in its log, which bridge bot device it
cross-signed, and bridge offerings pin a release tag, decision 0037; both below); before that
2026-10-08 (one bridge's slowness never delays another's delivery: the "does this
user exist" question is asked by the bridge's own worker, and each bridge's queue is a gauge and a
column on the bridges list; below); before that 2026-10-04, late (a bridge's namespaces, protocols and room directory mean what
Sytest's `tests/60app-services/` says, and a ghost acts once it is registered; below); before
that 2026-10-04 (the demo's shared WhatsApp registration becomes a declared offering,
and the server names a bridge registered by hand beside an offering; a changed double puppeting
reaches each instance's registration; Signal, Slack, X and LinkedIn have their command prefix;
`hs-bridges` and the operator connect under the outbound policy; mautrix-signal is the second
real bridge; below); before that 2026-10-03 (the bot's device is cross-signed by the manager, so a client that
excludes insecure devices shares keys with the bridge; a withheld key is a health line; and
CI's read of the bridge's rewritten config; both below); before that
2026-10-02, evening (the bridge has a name in WhatsApp's Linked devices, and a
changed config now reaches a running bridge; below); before that, the same day, the owner's bot
was silent on the demo cluster: diagnosed from the bridge's log, and a chat the bot started is
now repaired in place; before that, the same
day, `login qr` in the personal bot's chat did nothing (found with the real bridge, fixed in the
manager) and an instance's Kubernetes objects say whose bridge they are; before that 2026-10-01
(who has signed in to a bridge; the `cluster` runtime run on kind); before that 2026-09-30
(ephemeral, to-device and device-list delivery); before that 2026-09-27 (RFC 0017 run against
the real binary), 2026-09-27 (the bridge manager) and 2026-09-25.

## Session 2026-10-09, later (branch `agent/bridge-sign-log`): the manager says which bot device it cross-signed

On 2026-10-09 the owner recovered the demo's WhatsApp bridge by resetting its crypto store, which
made a new bot device, and found no log line confirming the manager had signed it. The manager
had logged one since 2026-10-03 ("cross-signed the bot's device with its own self-signing key"),
but keyed by `bridge_type` and `owner` only, not the appservice id the other bridge lines are
found by; and the instance could name the old device as the signed one, since both devices stay
on the server, signed, and the row took whichever `/keys/query` listed last.

- **The line** (`settle_bot_identity`, `hs-bridges` manager), at `INFO`, once per signature
  uploaded and never for a look that signs nothing:
  `cross-signed the bridge bot's device with the server-held self-signing key
  bridge_type=mautrix-whatsapp owner=@brandon:example.org appservice=whatsapp-brandon
  bot=@whatsappbot_brandon:example.org device=BQBMQVR81T first_signing=false
  previous_device=I36QDLYXYI` (`first_signing=true` and no `previous_device` on the first
  signing by this identity). The ids print bare, so `grep whatsapp-brandon` and
  `grep BQBMQVR81T` both find it; the "published the bot's cross-signing keys" line prints its
  `bot` and `master_key` bare too.
- **`signed_bot_device` names the device signed in this look** ahead of one found signed
  already, so after a reset the instance page says the bridge's current device.
- No counter: `hs-bridges` has no metrics of its own, and the signing is one line a week at
  most.

Verified: `cargo fmt --all --check`, `cargo clippy -p hs-bridges --all-targets -- -D warnings`,
`cargo test -p hs-bridges` (45 lib tests, among them the new
`signing_a_bot_device_is_logged_once_with_the_appservice_bot_and_device`, which captures the
manager's log through a global `tracing` subscriber routed per thread: a subscriber set on the
test's thread alone sees nothing once a parallel test has hit the callsite without one, since
`tracing` caches a callsite's interest from the first thread that hits it). `tracing-subscriber`
(already a workspace dependency) is a dev-dependency of `hs-bridges` for that. Not run against
the real bridge: the path is the one `real_mautrix_login::bot_is_cross_signed` proved on
2026-10-03; only its log line and the device it records changed.
## Session 2026-10-09 (branch `agent/bridge-image-pins`): offerings pin a release tag

Every offering's image was `:latest`; during the demo roll the same day the WhatsApp bridge
pulled a mautrix-whatsapp built 25 minutes earlier, a suspect in the outage. The catalogue
(`crates/hs-admin/src/bridge_types.rs`) now names a release tag for each image, and a test fails
if any entry says `latest` (decision 0037, `docs/decisions/0037-bridge-offerings-pin-a-release-tag.md`).

| Offering | Image | Release |
| --- | --- | --- |
| mautrix-whatsapp, -telegram, -signal, -gmessages, -meta, -twitter, -linkedin | `dock.mau.dev/mautrix/<network>:v0.2609.0` | 2026-09-16 |
| mautrix-slack | `dock.mau.dev/mautrix/slack:v0.2609.1` | 2026-09-25 |
| mautrix-gvoice | `dock.mau.dev/mautrix/gvoice:v0.2605.0` | 2026-05-16 |
| mautrix-discord | `dock.mau.dev/mautrix/discord:v0.7.7` | 2026-08-16 |
| mautrix-bluesky | `dock.mau.dev/mautrix/bluesky:v0.2510.0` | 2025-10-16 |
| heisenbridge | `hif1/heisenbridge:1.15.4` | 2025-10-04 |
| matrix-appservice-irc | `matrixdotorg/matrix-appservice-irc:release-4.0.0` (was ghcr.io `release-3.0.0`, whose package refuses an anonymous pull) | 2025-10-24 |
| matrix-hookshot | `ghcr.io/matrix-org/matrix-hookshot:7.5.0` (Docker Hub's `halfshot/matrix-hookshot` has no release tag after 7.3.2) | 2026-09-22 |

Each is the newest release of its upstream (GitHub releases) whose manifest the registry served
on 2026-10-09; the full table with each source and the `curl` that checks a manifest is in
`docs/bridges/mautrix.md` ("2026-10-09: offerings pin a release tag").

- **`latest` is not a pin.** `bridge_types::image_tag(type, requested)` resolves a blank or
  `latest` tag to the catalogue's pin and keeps any other. The manager (`hs-bridges`) and the
  admin API's in-memory source use it on `PUT /bridge-offerings/{type}` and when an offering is
  read, so the demo's WhatsApp offering, whose row says `latest` from before the pins, reads and
  deploys as `v0.2609.0`: its instance rolls once at the next roll of this server (the deploy
  fingerprint and the operator's pod-template hash both include the tag; a failing apply backs
  off per decision 0036). The wizard's render does the same with its `imageTag`.
- **The real-bridge story runs the pin.** `tests/real_mautrix_login.rs` names
  `dock.mau.dev/mautrix/whatsapp:v0.2609.0` and `signal:v0.2609.0`, and fails before anything
  boots when the offering the server made names a different image, so the catalogue and the test
  are bumped together. Run 2026-10-09 with Docker on this machine: 5 passed in 102 s, the
  bridges reporting `name=mautrix-whatsapp version=v26.09 built_at=2026-09-16T11:28:18Z` and
  `name=mautrix-signal version=v26.09 built_at=2026-09-16T11:22:20Z` (the images tag the release
  `v0.2609.0`; the bridge calls it `v26.09`).
- Fixtures and tests that asserted `:latest`: `crates/hs-cli/tests/bridge_offerings.rs`,
  `crates/hs-bridges/tests/manager.rs` (which also checks that `latest` asked for is the pin and
  a chosen release is kept), `crates/hs-operator/src/deploy.rs`, `hs-admin-mock`'s fixtures,
  `web/src/mocks/data/bridge-types.ts` (a `MAUTRIX_PINS` map mirroring the catalogue),
  `web/src/mocks/handlers.ts` and `web/src/mocks/data/bridge-offerings.ts` (no tag chosen means
  the pin), and `web/e2e-real/{bridge-offerings,add-mautrix-bridge}.spec.ts`.
- `deploy/helm/hs/values.yaml` and `deploy/demo/values-bridges.yaml` name no bridge image, so
  nothing there follows; `bridges.offerings[].image_tag` may still name a release.

Checks: `cargo fmt --all --check`; `cargo clippy -p hs-admin -p hs-cli -p hs-operator -p
hs-bridges -p hs-bridge-conformance --all-targets -- -D warnings`; `cargo test -p hs-admin`,
`cargo test -p hs-bridges`, `cargo test -p hs-operator`, `cargo test -p hs-cli --test
bridge_offerings`; `npm run check` in `web/`. All green on 2026-10-09 (hs-admin, hs-bridges 44+6, hs-operator 103,
hs-cli bridge_offerings 4; web 83 files, 631 tests, 0 lint errors).

## Session 2026-10-09 (branch `agent/crd-upgrade`): failing bridge steps back off, and a roll keeps a bridge's own pickle key

- **Back-off** (`hs-bridges` manager): a failing instance step waits 3 s, doubling to 5 min
  (`step_backoff`, `STEP_BACKOFF_MAX`), logged with `failures` and `retry_in_secs`; asking for the
  instance or changing its offering ends the wait; "the bridge instance's deployment changed:
  applied it" is logged after the apply went through. The fingerprint was already recorded right
  after the runtime accepted an apply: on the demo nothing was applied at all (the cluster refused
  the `Bridge`), so the every-3-s line was the manager retrying, not restarting the pod.
- **No pickle key is minted for an instance from before 2026-10-02** (`settle_pickle_key` is
  gone): its bridge's own key is carried by the operator's init container, which now does so even
  when the render has no `pickle_key` line (decision 0036).
- **"The supplied account key is invalid" is explained** on the instance (`explain_deployment`),
  from the bridge's last log line the operator now carries; the recovery is in
  `docs/bridges/mautrix.md` (2026-10-09), as done on the demo.
- **`Runtime::warning`**: the outdated-CRD sentence on every deployed instance.

Verified: `cargo test -p hs-bridges` (`a_refused_deployment_is_applied_once_and_then_backed_off`,
`asking_for_an_instance_again_or_changing_its_offering_ends_the_wait`,
`a_bridge_that_cannot_read_its_crypto_store_says_so_with_the_recovery`,
`what_the_runtime_says_needs_fixing_is_on_each_deployed_instance`);
`real_mautrix_login::a_bridge_rolled_with_a_render_without_its_pickle_key_keeps_its_own` with the
real `dock.mau.dev/mautrix/whatsapp:latest` and `hs`: passes, and with the old init script it
reproduces the demo's `FTL ... the supplied account key is invalid`; `bridge_offerings` and
`bridge_logins` green. Left: the backoff is per process (a restart retries at once), and the
manager does not watch a ready bridge's pod, so the explanation shows on its page but the state
stays `ready`.

## Session 2026-10-08 (branch `agent/appservice-pump`): delivery is per bridge all the way, and each bridge's queue is watched

Decision 0030 left one place where a bridge's slowness reached the others: the room pump, one
task for every room and bridge, asked a bridge about an unknown user of its namespace before
queueing an event naming them, and waited up to the query's ten-second timeout. Decision
`docs/decisions/0033-a-bridge-is-asked-about-its-users-by-its-own-delivery-worker.md` moves it.

**What changed.**

- `hs_appservice::known_users` (new): `LocalUsers` (moved from `pump`), `UserQueries`
  (`before_delivery(row, body)`) and `UNKNOWN_USER_RETRY_MS` (now kept per appservice and user).
  `Scheduler::with_user_queries` runs it in `drain`, just before a batch is sent, for that
  appservice only; the users of one batch are asked at once. `Pump::with_user_queries` is gone;
  the pump waits on nothing remote (module docs, "Nothing here waits on a bridge").
- `QueryService::user_exists_at(row, user)` (new: one appservice); `user_exists` and
  `room_alias_exists` ask their appservices at once and take the first yes (`first_yes`), so a
  hung bridge does not hold another's answer on the request path.
- Gauges `hs_appservice_queue_depth{appservice}`, `hs_appservice_queue_dead_lettered{appservice}`,
  `hs_appservice_queue_oldest_age_seconds{appservice}` (`metrics::QueueCollector`, read per
  scrape from `AppserviceStore::queue_summary`, which reads each entry's status without building
  its body), reported only for the appservices whose shard this replica owns. `hs serve`
  registers it (`AppserviceDelivery::queue_collector`).
- Admin API: `AppService.queue` (`AppServiceQueue`: `pending`, `dead_lettered`,
  `oldest_pending_age_ms`), OpenAPI **0.1.11**; `hs_admin::model::AdminAppserviceQueue`, filled
  by `RegistryAppserviceDirectory`. Web: the bridges list's "Waiting to send" column
  (`describeQueue` in `web/src/lib/bridge-state.ts`; amber past a minute, red with dead letters)
  and a sentence explaining it; mock fixtures agree with their backlogs.
- Ordering per appservice is the queue's, unchanged; shard gating unchanged (pump on the global
  shard's owner, an appservice's worker and now its questions on its shard's owner).

**Verified.** `cargo test -p hs-appservice` (116; new
`known_users::tests::only_the_appservice_being_delivered_to_is_asked_and_only_about_its_unknown_users`,
`delivery::tests::a_bridge_that_never_answers_a_user_query_holds_only_its_own_delivery`,
`metrics::tests::the_queue_gauges_say_what_waits_for_each_appservice_this_replica_delivers_to`;
the pump's user-query test moved to `known_users`), `-p hs-admin`, `-p hs-bridges`, `-p hs-cli
--lib appservice`, clippy on each. **Real binary:** `cargo test -p hs-cli --test
appservice_isolation` (new): two bridges, `stuck` never answers `GET /users/{userId}`; after
alice invites `@stuck_ghost` and speaks, `fine` has both events within a second of the send,
`stuck` is sent nothing while its question is open and the invitation after the timeout;
meanwhile `/metrics` says `hs_appservice_queue_depth{appservice="stuck"}` >= 1 with an oldest age
over a second and `fine`'s 0, and `GET /api/v1/appservices` says the same in `queue`. On `main`
the pump asked that question itself, so `fine` would have waited the ten seconds (by
construction; the test was not run against `main`'s binary). Again green: `appservice_queries`
(the Sytest story; the bridge is still asked about its invited user once, before it hears of
them), `appservice_ephemeral`, `bridge_offerings`, `bridge_logins`, and
`hs-bridge-conformance` with the real mautrix-whatsapp and mautrix-signal under Docker
(`real_mautrix_login`, 4 of 4, not skipped). Web: `npm run check` (618 tests, new
`BridgesListPage.test.tsx` and `describeQueue` cases), `npm run test:e2e` (68, `bridges-list`
checks the column).

`hs-appservice` now depends on `futures` (already a workspace dependency).

**Left.** No alert rule ships for the queue (`deploy/observability/alerts/hs-rules.yaml` is track
12's; `docs/bridges/mautrix.md` suggests one). The bridge's detail page still reads its counts
from the backlog list, not `queue`. Sytest `tests/60app-services/` was not rerun on this branch
(the question's behaviour from a bridge's side is unchanged; `appservice_queries` covers it).

## Session 2026-10-04, late (branch `agent/appservice-gaps`): what a bridge's namespaces, protocols and room directory mean outside delivery

Graded by Sytest `target/sytest/20261004-wave1/` (10 of `tests/60app-services/` failing, 2
skipped behind them) and Complement's `TestJoinFederatedRoomFromApplicationServiceBridgeUser`.
Decision `docs/decisions/0030-an-appservices-namespaces-reach-registration-aliases-and-the-room-directory-through-hs-auths-registry.md`
(the seam and the call sites, and why a ghost must be registered).

**The seam.** `hs_auth::appservice::AppserviceRegistry` gained four methods with default
bodies (`exclusive_user_owner`, `exclusive_alias_owner`, `query_room_alias`,
`network_room_ids`); `hs_appservice::auth_registry::RegistryAppserviceAdapter` answers them from
the registry and, `with_queries`, asks the bridges. The stub `InMemoryAppserviceRegistry`
answers `exclusive_user_owner` from its records' exclusive rules.

**What changed, by Sytest test.**

- "Regular users cannot register within the AS namespace": `/register` and `/register/available`
  refuse an exclusive user ID with `400 M_EXCLUSIVE` (also an appservice registering in another's;
  `hs-auth` `routes/register.rs`, `refuse_exclusive`). Log: `refused a registration in an
  appservice's exclusive namespace`.
- "Regular users cannot create room aliases within the AS namespace": `PUT /directory/room`
  refuses an exclusive alias to anyone but its appservice (`hs-room` `routes/aliases.rs`,
  `RoomError::Exclusive` → `400 M_EXCLUSIVE`). Log: `refused an alias in an appservice's
  exclusive namespace`.
- "Accesing an AS-hosted room alias asks the AS server": a local alias the directory does not
  hold is asked of each appservice whose alias namespace covers it (`GET
  /_matrix/app/v1/rooms/{alias}`, `QueryService::room_alias_exists`) and read again
  (`hs_room::routes::aliases::resolve_local_alias`, used by `GET /directory/room` and `POST
  /join/{alias}`).
- "Inviting an AS-hosted user asks the AS server": before the pump queues an event naming a local
  user with no account (sender, or a membership's target) whom a bridge covers, it asks the bridge
  (`GET /_matrix/app/v1/users/{userId}`) and waits, as Synapse's `_check_user_exists` does
  (`Pump::with_user_queries`, `LocalUsers`, `hs-cli`'s `appservice_delivery::Accounts`). A user
  nobody provides is not asked about again for a minute.
- "Events in rooms with AS-hosted room aliases are sent to AS server": alias interest was already
  in the pump; it failed only because the alias above was never made.
- "HS provides query metadata", "HS can provide query metadata on a single protocol" (and the two
  it unskips, "HS will proxy request for 3PU/3PL mapping"): new routes
  `hs_appservice::client_routes` (`GET /thirdparty/protocols`, `/thirdparty/protocol/{p}`,
  `/thirdparty/user[/{p}]`, `/thirdparty/location[/{p}]`), mounted under `v3` and `r0`. Each
  protocol is asked of every appservice declaring it and merged as Synapse merges (first answer's
  fields, every answer's instances); an instance with a `network_id` gets
  `instance_id: "{appservice}|{network}"`; answers are kept five minutes (`PROTOCOL_CACHE_MS`),
  which the single-protocol test relies on. Lookups pass the client's query (minus
  `access_token`) and keep only well-formed results.
- "AS can publish rooms in their own list", "AS and main public room lists are separate": `PUT`/
  `DELETE /directory/list/appservice/{networkId}/{roomId}` (appservice tokens only, `403`
  otherwise) keep rooms in keyspace `hs_appservice.network_rooms` (purged with the appservice);
  `/publicRooms` (`hs-room` `routes/directory.rs`) reads `third_party_instance_id` (that network)
  and `include_all_networks` (the server's and every network's); the server's own list is
  unchanged. Log: `an appservice changed its room directory`.
- "AS can deactivate a user": `/account/deactivate` skips UIA for an appservice requester
  (`hs-auth` `routes/account.rs`). Log: `an appservice deactivated one of its users`.

**Found on the way: an unregistered ghost could act.** "Ghost user must register before joining
room" passed only by Sytest's leniency (its check passed before the test did anything): the
appservice could masquerade as `@astest-02ghost-1` without registering it. With the user query
on, that unregistered sender made the pump ask a bridge that never answered, holding every
room's delivery for the ten-second timeout, and two `03passive` tests failed behind it.
`hs-auth`'s appservice authentication now refuses a masquerade as a user with no account here
(`403`, "Application service has not registered this user"), as Synapse does; the bot is exempt.
The real mautrix-whatsapp and mautrix-signal stories pass with it.

**Observable.** `hs_appservice_queries_total{appservice,kind,outcome}` (kinds `user`,
`room_alias`, `protocol`, `thirdparty_user`, `thirdparty_location`; outcomes `yes`, `no`,
`error`, `cached`), registered by `hs serve`; `WARN` when a bridge does not answer; `INFO` when it
provides a user or alias.

**Complement.** `TestJoinFederatedRoomFromApplicationServiceBridgeUser` failed with `401
M_UNKNOWN_TOKEN`: the image never listed the registrations Complement copies to
`/complement/appservice/*.yaml`. `tests/complement/startup.sh` (track 14's file; one block, 2c)
now lists them in `appservices.registration_files`.

**The catalogue's LinkedIn port** is 29341, mautrix-linkedin's `DefaultPort`
(`pkg/connector/connector.go` at commit af73c518, read 2026-10-04); it said 29325
(`hs_admin::bridge_types`, test `linkedin_listens_on_its_sources_default_port`, which also checks
no two entries share a port; the web mock's table).

**Verified.** Unit: `cargo test -p hs-appservice` (114; new `registry::tests::
exclusive_owners_and_interested_appservices`, `network_rooms_are_kept_per_appservice_and_network`,
`query::tests::protocol_metadata_is_merged_across_appservices_and_kept_a_while`,
`thirdparty_lookups_go_to_the_protocols_appservices`,
`existence_questions_go_to_the_appservices_that_cover_the_id`,
`pump::tests::the_bridge_is_asked_about_an_unknown_user_before_it_hears_of_them`,
`client_routes::tests` ×3), `-p hs-auth` (265; new
`a_person_cannot_register_in_an_appservices_exclusive_namespace`,
`an_appservice_deactivates_its_user_without_uia`, and
`appservice_can_masquerade_as_namespaced_user` now registers its ghost first), `-p hs-room --lib`
(184; new `an_appservices_aliases_are_its_own_and_it_is_asked_for_ones_nobody_has_made`,
`an_appservice_networks_rooms_are_listed_apart_from_the_servers`), `-p hs-admin`, `-p
hs-bridges`, `-p hs-cli --lib`, `-p hs-bridge-conformance` (with the real mautrix bridges, 4 of
4). **Real binary:** `cargo test -p hs-cli --test appservice_queries` (new; the whole Sytest story
in one server, metrics and log lines included), and `appservice_ephemeral`, `bridge_offerings`,
`bridge_logins` again. **Sytest** (`tests/60app-services/*.pl`, `SYTEST_HS_BINARY` of this
branch, bookworm release build): **25 of 25 pass**, the 10 graded failures and the 2 skips
behind them included (before: 13 pass, 10 fail, 2 skip; `target/sytest/base-60as/` against
`as-2/`), and "Ghost user must register before joining room" no longer passes before it does
anything. **Complement** (`refs/complement`, under the shared lock, image
`complement-hs-appservice-gaps:dev`: this branch's `hs` and `startup.sh` over
`complement-hs-main:c2d74174`): `TestJoinFederatedRoomFromApplicationServiceBridgeUser` passes;
`TestJumpToDateEndpoint`, the other test on the appservice blueprint, is unchanged (2 of 14
subtests, as on main: `/timestamp_to_event` is not a route, `hs-room`'s). Clippy `-D warnings` on hs-appservice, hs-auth, hs-room, hs-admin,
hs-cli, hs-bridge-conformance; `cargo fmt --all --check`. `web/` (the LinkedIn port in the
mock): `npm run check` (605 tests; four lint warnings in files this branch does not touch),
`npm run test:e2e` 68 of 68.

**Not done / left.** The pump is one task for all rooms, so a bridge that never answers
`/users/{userId}` holds delivery for the query's ten-second timeout once per unknown user per
minute (decision 0030, consequences). Inbound federation alias queries
(`/_matrix/federation/v1/query/directory`, `hs-federation`) do not ask the bridge yet.

## Session 2026-10-04 (branch `agent/bridge-offering-demo`): the demo's shared WhatsApp registration becomes an offering

`docs/next-steps.md` item 5; RFC 0017 section 6. Decision
`docs/decisions/0029-a-deployment-declares-bridge-offerings-and-a-hand-registered-bridge-beside-one-is-named.md`.
OpenAPI 0.1.7 → **0.1.8** (additive; client regenerated).

**1. The demo declares the offering; nothing in `deploy/` registers a shared bridge.**

- `deploy/demo/values-bridges.yaml` (new): the demo release's bridges overlay, WhatsApp on the
  cluster runtime, everyone allowed, encryption, double puppeting and backfill. Layered over
  `helm get values` (the demo's values are not in the repository; there never was a file
  carrying the shared registration: it was made through the wizard and lives in the database).
- The chart: `bridges.offerings` in `deploy/helm/hs/values.yaml` (documented there), rendered
  into `MYELIN_BRIDGES_OFFERINGS` as JSON in `templates/statefulset.yaml`, only with
  `bridges.enabled` and a non-empty list. `helm lint` clean; rendered with and without, and with
  `bridges.enabled=false` (absent).
- `hs serve` (`crates/hs-cli/src/bridges.rs`, `declared_offerings`): parses it; a list that
  does not parse, an entry without `type`, a type not in the catalogue or a type twice fails
  startup. `BridgeManager::set_declared` / `apply_declared` (`crates/hs-bridges/src/manager.rs`)
  create each once, after the manager registers its front doors on the replica owning the
  global shard, and record it in `ManagerRow.declared`; an existing offering is adopted
  unchanged (the demo's); a removed one is not recreated; one this server cannot create is
  logged at `WARN` and tried at the next start. Log lines: `the deployment declares bridge
  offerings`, `created the bridge offering the deployment declares`, `... already exists:
  keeping it as the admin API has it`.
- **The health line.** `crates/hs-bridges/src/overlap.rs`: a registration that is a bridge of
  an offered network without being the manager or one of the offering's instances (same
  `io.myelin.bridge_type`, the catalogue's bot name, or an exclusive user rule covering the
  catalogue's ghosts, matched as the registry matches) is named. `AppServiceHealth.overlaps_offering`
  (schema `OfferingOverlap`: type, name, front door, what to do) is filled by
  `crates/hs-bridges/src/directory.rs`, `OfferingAwareDirectory`, which wraps the appservice
  directory the admin API reads (`serve.rs`), so `hs-appservice` stays unaware of offerings.
  `BridgeOffering.overlapping_appservices` (schema `AppServiceOverlap`) is the same from the
  offering. The manager logs each at `WARN` when it appears and at `INFO` when it goes (looked
  at once a minute at most). **It is not paused or removed by the server** (decision 0029): the
  line offers Pause. Web: the bridge page's warning line links to the offering; the offering
  page's "Also registered by hand" links to each.
- **The owner's steps** (save the registration, roll with the overlay, Pause, people move to
  their own instance, stop the shared bridge, Remove, check): `docs/bridges/mautrix.md`,
  "2026-10-04: the demo's shared WhatsApp registration becomes an offering". A desk item.

**2. `command_prefix` for Signal, Slack and X** (and LinkedIn, found missing by the new test).
Their connectors (`pkg/connector/connector.go`, `GetName`, fetched 2026-10-04) set no
`DefaultCommandPrefix`; mautrix-go's `bridgev2/matrix/mxmain/example-config.yaml` writes
`command_prefix: '$<<or .DefaultCommandPrefix (printf "!%s" .NetworkID)>>'` and
`bridgev2/networkinterface.go` documents the fallback. So `!signal`, `!slack`, `!twitter` (the
`NetworkID` stays `twitter` when the display name is X), `!linkedin`; cited in
`hs_admin::bridge_types::command_prefix`. Test `every_mautrix_bridge_has_its_command_prefix`
(every mautrix entry has one, nothing else does). The real Signal bridge's rewritten config says
`!signal` (below). The offering page shows the prefix with what it is for.

**3. Outbound policy.** `hs_bridges::matrix::MatrixClient` and the operator's `HttpAdminApi`
are built by `hs_http::client::builder()`: IPv4 first, every address tried, counted in
`hs_outbound_connections_total` / `hs_outbound_connect_failures_total`. The operator now
registers those counters in its own `/metrics` (`OperatorMetrics::new`; test
`the_outbound_counters_are_served_too`); `hs serve` already did. `hs-http` added to both
crates' dependencies (workspace path dependency; nothing new in `[workspace.dependencies]`).

**4. A changed double puppeting re-renders the instances' claim.** `InstanceRow.registered_fingerprint`
(`manager::registration_fingerprint`: the rendered registration without `url`) is set at
registration; `settle_registration`, run on every step of a registered, deploying, starting or
ready instance, patches the registry's namespaces and feature flags when it differs, once (log
line `the bridge instance's registration changed: updated this server's copy of it`). The files
change with it, so a cluster instance rolls through the existing deployment fingerprint; an
instance run elsewhere gets the reason "its registration changed: download its files again and
restart it with them". A row from before is patched once, silently. The settings dialog says
what saving a changed double puppeting will do and to how many bridges (it also no longer
claims new settings only apply to new bridges, which stopped being true on 2026-10-02).

**5. A second real bridge: mautrix-signal.** `crates/hs-bridge-conformance/tests/real_mautrix_login.rs`
is parametrised by `Network` (type, image, port, the login text); the new story
`a_person_types_login_to_their_signal_bridge_and_gets_a_qr_code` runs
`dock.mau.dev/mautrix/signal:latest` from its rendered files: encrypted chat started as alice,
`device_name` in the admin API and in the bridge's rewritten config (`Myelin Signal bridge for
alice (test.local)`), `command_prefix` `!signal` in both, the bot cross-signed, `login`
answered with a QR code, nothing withheld. The harness now syncs once before typing: in one of
three full runs, under load, the repaired story sent `login qr` before alice's client had synced
the manager's signature of the bot's device, and the key was withheld as `m.unverified`; a
client that is running has synced.

**Verified.** `cargo test -p hs-bridges` (39 unit + 6; new: `overlap::tests` ×3,
`a_changed_double_puppeting_updates_the_instances_claim_once_and_rolls_it`,
`an_instance_run_elsewhere_is_told_to_fetch_its_files_when_its_claim_changes`,
`declared_offerings_are_created_once_and_an_existing_one_is_adopted`,
`a_bridge_registered_by_hand_is_named_on_its_health_and_on_the_offering`, the store's
`put_manager`); `cargo test -p hs-admin` (303), `-p hs-operator` (95), `-p hs-appservice`,
`-p hs-cli --lib bridges` (2, the declaration's parsing). **Real binary:** `cargo test -p hs-cli
--test bridge_offerings` 4 of 4, the new
`a_shared_registration_beside_its_offering_is_named_and_a_changed_double_puppeting_reaches_the_instance`
(a wizard-shaped `whatsapp` registration, then the offering and alice's instance: both lines
present, alice's own registration not counted; double puppeting off drops `@alice` from her
registration and `double_puppet:` from her files with the reason set; the shared registration
removed and the offering's list empty). `cargo test -p hs-bridge-conformance --test
real_mautrix_login` 4 of 4 after the sync fix (WhatsApp encrypted, plain, repaired;
Signal), 56 s; before it, 3 of 4 then 4 of 4, and the repaired story alone 2 of 2. Clippy `-D warnings` on hs-bridges, hs-admin, hs-operator, hs-appservice,
hs-bridge-conformance, hs-cli; `cargo fmt --all --check`. `web/`: `npm run check` (573 unit
tests; the four lint warnings are in files this branch does not touch), `npm run test:e2e` 64
of 64.

**Not done.** The roll on the demo (desk item; the steps are in `docs/bridges/mautrix.md`). The
server does not stop delivering to a hand-registered bridge on its own (by choice). The
catalogue's LinkedIn port (29325) differs from mautrix-linkedin's `DefaultPort` (29341); a
rendered config sets the port explicitly, so it works, but the catalogue should say 29341. No
Telegram (it needs an `api_id`, so it is not deployable from rendered files). The operator's
reconciler still talks to Kubernetes through `kube`'s own client, which the outbound policy does
not cover (it is not a reqwest client).

## Session 2026-10-03 (branch `agent/bridge-bot-verified`): the owner's Element refused to share keys with the bridge; the manager now cross-signs the bot's device

**What happened.** In the repaired chat on the demo the owner typed and the bot answered
"⚠️ Your message was not bridged: your client refused to share decryption keys with the
bridge": mautrix-go's wording (`bridgev2/matrix/cryptoerror.go`) for an `m.room_key.withheld`
from the owner's client. The code behind it is `m.unverified`: a client that excludes insecure
devices (Element Web's Labs "Exclude insecure devices when sending/receiving messages", Element
X's invisible crypto; `matrix-sdk-crypto`'s `CollectStrategy::IdentityBasedStrategy`, MSC4153)
shares a room's Megolm keys only with devices signed by their owner's self-signing key, and with
no device at all of a user without a published identity. The bot had none: a mautrix bridge
self-signs only with `encryption.self_sign: true`, which the rendered config never set.

**What mautrix offers, and why it was not used.** `self_sign: true` (`bridgev2/matrix/crypto.go`,
`doSelfSign`) generates SSSS and a recovery key, uploads master, self-signing and user-signing
keys with no UIA callback (a `401` is fatal, exit 34), signs its device and master key, and keeps
the recovery key in the bridge's own database (`kv_store.recovery_key`). When the server has the
bot's keys and that database does not, startup is fatal ("Server already has cross-signing keys,
but no key in database"): an instance recreated for the same owner (`remove` deletes the
registration and keeps the bot user; no client API deletes cross-signing keys), a reset bridge
database. Both are ordinary here.

**What was built.** The manager owns the bot's identity
(`crates/hs-bridges/src/cross_signing.rs`; decision
`docs/decisions/0027-the-manager-cross-signs-each-bridge-bots-device.md`):

1. `BotIdentity::from_seeds` builds master and self-signing `ed25519` pairs from two 32-byte
   seeds; `upload_body` is the `/keys/device_signing/upload` body (a bare master key, a
   self-signing key signed by it; no user-signing key, the bot vouches for nobody);
   `sign_device` adds the self-signing key's signature to a device-keys object, keeping the
   device's own, for `/keys/signatures/upload`; `is_published_master` and `has_signed_device`
   read `/keys/query`. Signatures are over canonical JSON with `signatures` and `unsigned`
   stripped (`hs_model::signing`), the way `hs-e2e` verifies them.
2. `InstanceRow` carries `cross_signing_master_seed`, `cross_signing_self_signing_seed` (minted
   on the first ready step, stored before anything is published, beside `pickle_key`) and
   `signed_bot_device`.
3. `BridgeManager::settle_bot_identity` runs first in every ready step: `/keys/query` as the bot
   (`MatrixClient::keys_query`, over loopback with the instance's token and `user_id`); the
   keys published when the server's master key is not this identity's (none yet, or a previous
   instance's for the same owner; the appservice replaces them without user-interactive auth,
   MSC4190, which `hs-e2e` already allowed, the same rule as Synapse's); every device of the bot
   without the self-signing signature signed. Until a device is signed it looks every tick; once
   settled every minute (`IDENTITY_RECHECK_MS`, in memory), for a device the bridge makes after
   a reset database. A failure is a reason on the row (`the bot's cross-signing: ...`) and does
   not stop the chat from being made; the next success clears it.
4. `BridgeInstance.signed_bot_device` in the admin API (OpenAPI 0.1.7), and the person's instance
   page says which device is cross-signed.

**A withheld key is now a health line.** `hs-appservice`'s scheduler reads every delivered body
for `m.room_key.withheld` to-device events (`transaction::key_withheld_in`), logs each at
`WARN` ("a client withheld a room's keys from an appservice's device": sender, code, reason,
room, device), counts `hs_appservice_key_withheld_total{appservice,code}`, and keeps the last
one in the appservice's health (`HealthRow.last_key_withheld`; the admin API's
`AppServiceHealth.last_key_withheld` and `BridgeInstance.last_key_withheld`, schema
`KeyWithheld`). The bridge page and the instance page show it, with what the code means and what
happens next (`m.unverified` with a signed device: a message sent again goes through).

**Server unchanged.** `hs-e2e`'s `/keys/device_signing/upload` already skipped UIA for an
appservice requester with keys set up, accepted a bare master key, verified the self-signing
key's signature by the master key, and recorded a device-list change on both uploads; nothing in
`hs-auth` needed touching. Status 08 is not edited; the exemption is now relied on by a real
deployment and proven below.

**Verified.** `cargo test -p hs-bridges`: `cross_signing` (seeds, the upload body's signatures,
a device signed with its own signature kept and `unsigned` outside the signature) and the
manager against its fake server (`a_ready_instances_bot_gets_a_cross_signing_identity_and_its_device_signed`,
with a later device signed on the next look and no second key upload;
`an_instance_recreated_for_the_same_owner_replaces_the_bots_old_keys`;
`a_bot_without_a_device_yet_gets_its_keys_and_is_looked_at_again_each_step`); `cargo test -p
hs-appservice` (`key_withheld_in_reads_the_withheld_events_under_either_spelling`); clippy clean
on `hs-bridges`, `hs-appservice`, `hs-admin`, `hs-bridge-conformance`; `npm run check` in `web/`
(569 unit tests, two new for the instance page). The real bridge
(`dock.mau.dev/mautrix/whatsapp:latest`, `cargo test -p hs-bridge-conformance --test
real_mautrix_login`): alice's `matrix-sdk` client built with
`with_room_key_recipient_strategy(CollectStrategy::IdentityBasedStrategy)` and her own
cross-signing bootstrapped (that strategy refuses to send at all without it, as the owner's
Element has it); 3 of 3 in 31 s. In the encrypted and repaired stories `bot_is_cross_signed`
saw, through `/keys/query` as alice, the bot's master key, its self-signing key signed by it,
and the bridge's device (`X3HXCGJKPK`, `I36QDLYXYI`) signed by the self-signing key beside its
own signature; `login qr` was decrypted by the bridge and answered with the QR notice and image,
decrypted by alice; the appservice's health had `last_key_withheld: null`. Before alice's own
cross-signing was bootstrapped the same client failed with "Encryption failed because
cross-signing is not set up on your account", which is the harness's job to know, not the
server's.

**The owner's workaround until the image rolls.** Which Element setting withholds decides it:
Labs → "Exclude insecure devices when sending/receiving messages" ignores manual device
verification and can only be turned off (Element's default) or waited out; "Only send messages
to verified users" (Settings → Security & Privacy, or per room) honours a manually verified
device: open the bot's profile → its "WhatsApp bridge" session → "Manually verify by text" and
confirm, which relaxes nothing and is the safer one; turning that setting off for the bot's chat
only (the room override, a room with just the owner and their bot) is next. Details in
`docs/bridges/mautrix.md` (2026-10-03).

**Not done.** No run against a binary without the manager change from this session (the
negative control is the owner's demo). The demo instance gets its identity on the manager's
first ready step after the image rolls; the owner's Element re-fetches the bot's keys on the
device-list change and the next message goes through. The user-signing key is not published,
so the bot cannot verify people; nothing needs it.

## Session 2026-10-03 (branch `agent/bridge-ci-config-read`): CI reads the bridge's rewritten config through the container

`main` was red from `66528ae3`: `crates/hs-bridge-conformance/tests/real_mautrix_login.rs`'s
`device_name_reached_the_bridge` read the bridge's rewritten `config.yaml` from the test's
host temp dir, and on both GitHub runners (amd64 and arm64, run 37096260816) that read failed
with `Permission denied (os error 13)`. The mautrix image runs as uid 1337 and its config
upgrader writes the file as that user with a mode nobody else can read; the test's `chmod 777`
on the directory lets the container write there but does not make the file readable by the
runner's user. On the owner's Mac it passed because Docker Desktop maps every bind-mounted
file's owner to the host user.

The fix: `Bridge::read_file` reads a path under `/data` with `docker exec <container> cat`,
the way `Bridge::log` reads the log with `docker logs`, and `device_name_reached_the_bridge`
uses it. `docker exec` runs as the image's own user, which owns the file, so the read works
under any uid model; nothing on the host reads the mount any more (the host only writes the two
files before the container starts). The unused `Scene::bridge_dir` field went with it.
Verified: `cargo test -p hs-bridge-conformance --test real_mautrix_login`, 3 of 3, 35 s, on
Docker Desktop's Linux VM (arm64). Not run on a Linux host from this session; the reasoning
above is why it holds there.

## Session 2026-10-02, evening (branch `agent/bridge-device-names`): the bridge is named in WhatsApp's Linked devices, and a changed config reaches a running bridge

**Asked.** The owner's WhatsApp bridge shows in the phone's Linked devices as "other device",
platform "Other device". Name it after the person and the server, with the word bridge in it.

**Found** (sources fetched 2026-10-02; the full account with file names is in
`docs/bridges/mautrix.md`, "2026-10-02, evening"):

- The two strings are mautrix-whatsapp's `network.os_name` (free text) and `network.browser_name`
  (a whatsmeow `PlatformType` name; `unknown` by default), handed to whatsmeow's `DeviceProps`
  and sent **only in the pairing payload** (`getRegistrationPayload`); a linked device is never
  told again. `UNKNOWN` makes the phone ignore the name and say "Other device" (whatsmeow
  discussion #469, issue #89, and the owner's phone); a browser value shows "Google Chrome
  (name)" and a browser's logo; `DESKTOP` shows the name alone with the desktop icon. So
  `browser_name: DESKTOP`, one constant (`hs_admin::bridge_types::WHATSAPP_PLATFORM`).
- Signal (`network.device_name`), Telegram (`network.device_info.device_model`) and Google
  Messages (`network.device_meta.os`) have the equivalent; Meta, Discord, Slack, X, LinkedIn,
  Bluesky and Google Voice have none (documented in `DeviceSetting`, so nobody looks twice).
- The bot's Matrix device is named "WhatsApp bridge" by mautrix-go itself
  (`bridgev2/matrix/crypto.go`), config cannot change it, the as_token double puppet has no
  device; left as it is.
- **There was no path for a changed config to reach a running instance.** The manager applied a
  deployment once, in `registered`, and the operator's init container never replaced a file in
  `/data` (the bridge's rewritten `config.yaml` holds the generated `encryption.pickle_key`).

**Done.**

- `crates/hs-admin/src/bridge_types.rs`: `device_name(type, server, owner)` is the one pattern
  (`Myelin WhatsApp bridge for brandon (myelin.dacrib.net)`; shared: `Myelin WhatsApp bridge
  (myelin.dacrib.net)`), `DeviceSetting` says which key each connector has, `network_section`
  writes it, `WHATSAPP_PLATFORM = "DESKTOP"`; `InstanceSpec::pickle_key` is rendered as
  `encryption.pickle_key`; `InstanceRender::device_name`. The wizard's hand-run bridge carries
  the shared name. Test `an_instance_is_named_on_the_networks_side_after_its_owner_and_this_server`
  covers a person's and a shared instance, every connector with a key, every one without, the
  quoting and the pickle key.
- `crates/hs-admin/src/model.rs` and `openapi/openapi.yaml`: `BridgeInstance.device_name`
  (nullable); the web client regenerated (`npm run generate:client`).
- `crates/hs-bridges`: `InstanceRow::pickle_key` (minted with the tokens; `settle_pickle_key`
  mints one for an older row) and `InstanceRow::applied_fingerprint`; `deploy_fingerprint`
  (image, port, arguments, files); `apply_deployment` records it; a deploying, starting or ready
  cluster instance whose fingerprint differs is applied again and moved to `deploying` with
  "its configuration changed: restarting the pod with it" (log line "the bridge instance's
  deployment changed: applying it, which restarts the pod"). An instance from before is applied
  once. Tests: `a_deployed_instance_is_named_on_whatsapps_side_and_its_config_is_complete`,
  `a_changed_offering_is_applied_to_a_ready_instance_once_and_rolls_it`,
  `a_deploy_fingerprint_changes_with_the_files_and_the_image_and_not_the_name`.
- `crates/hs-operator`: `deploy.rs` annotates the `Bridge` with `myelin.dev/files-hash`
  (`files_hash`); `bridge.rs` folds it into `spec_hash`, so the pod rolls; the init container's
  script now replaces each file and carries `pickle_key`, `signing_key` and `server_key` from the
  bridge's copy at the new file's indentation (POSIX sh, grep, sed, awk). Tests run the real
  script. The `Bridge` CRD's `filesSecret` description changed with it: `deploy/crds/bridge.yaml`
  and `deploy/helm/hs/crds/bridge.yaml` regenerated with `cargo run -p hs-operator --bin gen-crds`.
- `web/src/pages/bridges/offering/InstanceNextSteps.tsx`: one line under the steps, "In
  WhatsApp's own list of linked devices, this bridge is named …. A link made before that name was
  set keeps its old name until the bridge is linked again." (tests added; mock data names its
  ready instances).
- `crates/hs-bridge-conformance/tests/real_mautrix_login.rs`: the encrypted and plain stories
  now check `device_name` on the admin API and that the bridge's **rewritten** `config.yaml`
  (its upgrader ran) still says the name, `browser_name: DESKTOP` and the rendered pickle key.
- `docs/bridges/mautrix.md`: the section above, with what the owner will see.

**What the owner will see.** The demo instance's pod rolls once (new config, pickle key carried,
WhatsApp session and chat kept). The existing link stays "Other device": WhatsApp was told at
pairing and is not told again. Log the bridge out (phone: Linked devices, the device, Log out;
or `logout` to the bot) and `login qr` again; the new link shows `Myelin WhatsApp bridge for
brandon (myelin.dacrib.net)` with a desktop icon. The next-steps box says so.

**Verified.** `cargo test -p hs-admin --lib bridge_types`, `cargo test -p hs-bridges`,
`cargo test -p hs-operator --lib`, `cargo clippy -p hs-admin -p hs-bridges -p hs-operator -p
hs-cli -p hs-bridge-conformance --all-targets -- -D warnings`, `cargo fmt --all --check`; in
`web/`: `npm run lint`, `npm run typecheck`, `npx vitest run` (567 tests), `npm run build`. The
real-bridge harness, `cargo test -p hs-bridge-conformance --test real_mautrix_login`, against
`dock.mau.dev/mautrix/whatsapp:latest` and this branch's debug binary: 3 passed in 35 s, so the
bridge's own config upgrader keeps `network.os_name`, `browser_name: DESKTOP` and the rendered
`pickle_key`, and the admin API names the device. Not on the cluster (unreachable from sessions
today), so the roll itself (annotation, operator, init container) is verified by unit tests
and the script run under `/bin/sh` only.

**Decisions.** (1) The platform is `DESKTOP`, a per-type constant, not an option: it is the one
value that shows a free-text name without claiming to be a browser. (2) The name puts the
localpart before the server and the network's name in it, same pattern for every network, so a
person with three bridges tells them apart in each app. (3) The Secret is now the config: a
hand edit inside a pod's `/data/config.yaml` is replaced on the next roll, except the three
generated keys, which are carried. (4) An offering change now restarts every deployed instance
of that offering on its next step; there is no confirmation step, as there was none for the
chart's own rollouts either. (5) Not done: renaming the bot's Matrix device; the bridge's own
"WhatsApp bridge" is explicit enough and a client shows it under the person's bot.

## Session 2026-10-02 (branch `agent/bridge-responds`): the owner's bot was silent on the demo cluster; a chat the bot started is now repaired in place

**What the owner saw.** On the demo cluster (image `sha-99589af3…`, which carries
`agent/bridge-login`), as `@brandon:myelin.dacrib.net`, in the chat with
`@whatsappbot_brandon`: `login phone`, `login qr` (four times), `!wh login qr`, `hello`. Nothing
back to any of it.

**What the cluster said** (read-only: `kubectl logs`, the Bridge object, the files Secret, and
the client API through a port-forward with the instance's own token; the logs are kept in the
session's scratchpad as `bridge-before.log`, `hs-before.log`, `operator-before.log`):

- The bridge pod `bridge-d2854412-9d44fdb54-wrnpd` (3 h old, never restarted) was healthy
  across the server's restart: its pings worked, its device `BSLXZIVKIV` and 51 one-time keys
  were uploaded, and transactions 35–44 were delivered after the new server came up
  (`delivered a transaction to an appservice appservice=whatsapp-brandon txn_id=44 events=1`).
  **No restart was needed, and none was done.**
- **Every one of the eight messages reached the bridge and was decrypted**, and not one was
  taken: eight times `Decrypting received event … session_id=0KytEOpisOcHE6LOLpPEVt…` then
  `Event decrypted successfully decrypted_event_type="m.room.message (message)"`
  (`message_index` 0 through 7, the last at 00:14:48Z), and never `Received command`. Nothing
  was sent by the bot; nothing failed to send. The server's side agrees: `hs_appservice`
  delivered each one, `appservices.health` healthy.
- The chat `!Zsl5rtFMYQNrkT7I9X:myelin.dacrib.net` was **created by the bot**
  (`m.room.create` sender `@whatsappbot_brandon`, then `m.room.encryption`, then the bot's
  invitation to brandon and its sign-in notice; brandon joined; eight `m.room.encrypted` from
  brandon). Two joined members, power levels 100 each (`trusted_private_chat`), encrypted.
  Double puppeting is on (the registration's non-exclusive claim on `@brandon`), brandon is
  the bridge's `admin` in `bridge.permissions`.

**Root cause.** The known one, unchanged by the roll: a mautrix `bridgev2` bridge takes a bare
command only in the sender's management room, which it marks only in `handleBotInvite`, when
*the person invites its bot* into a chat of two (`bridgev2/matrixinvite.go`; `queue.go`:
`strings.HasPrefix(msg.Body, CommandPrefix) || evt.RoomID == sender.ManagementRoom`). The
owner's instance was made before `agent/bridge-login` and its chat is the bot's. `!wh` is not
WhatsApp's prefix (`!wa` is), so it was dropped as text too. Encryption, the appservice
connection, keys, the server restart: all fine.

**What the server does now** (`hs_bridges::manager::BridgeManager::settle_chat`, run on every
step of a ready owned instance whose chat is not yet recorded as the owner's;
`InstanceRow::dm_started_by`, `owner` or `bot`, new on the row and `None` on every row from
before today):

1. The bot is no longer in the chat (403/404 on `joined_members`): the chat is forgotten and
   the next step starts a new one, as the owner.
2. The room's creator (`GET /rooms/{id}/state`, the `m.room.create` sender; room v11 has no
   `creator` in the content) is the owner: recorded as `owner`, left alone.
3. The creator is the bot and the instance may act as the owner (double puppeting): **repaired
   in place**. The bot leaves, the owner (double-puppeted through the instance's token)
   re-invites it, the bot rejoins. The bridge is sent the invitation, accepts it (`Accepted
   invite to room as bot`), counts two members, marks the room and says "This room has been
   marked as your management room"; then the bot says why it had been silent and lists the
   steps again ("I started this chat myself, so I did not take what you typed here … I have
   left and come back on your invitation, so this chat is one now. To sign in: …"). The
   chat's history stays. If the invitation or the rejoin fails with the bot already out, the
   chat is forgotten and a new one is started as the owner; the step's reason says so.
   If the owner is not in the chat (an invitation never accepted), it waits and looks again
   each step. Log: `repaired the owner's chat with their bridge's bot`.
4. The creator is the bot and the instance cannot act as the owner (no double puppeting):
   the bot says so in the chat, once ("I started this chat myself, so I only take commands
   here with my prefix: `!wa login qr` rather than `login qr` … For bare commands, invite me
   (…) to a new direct chat"), and the row records `bot`.

A new chat records who started it at once, so nothing is looked at twice. The admin API shows
it: `BridgeInstance.chat_room` and `chat_started_by` (`owner` | `bot` | null), and
`BridgeType.command_prefix` (`!wa`, `!tg`, `!gm`, `!gv`, `!fb`, `!discord`, `!bsky`, read from
each bridge's `pkg/connector/connector.go` on 2026-10-02; Signal, Slack and X set none there,
so none is claimed). OpenAPI 0.1.5. The bridge page's next steps say "is in a direct chat
started for them: open it" for `owner`, and for `bot` a warning box: started by the bot, prefix
commands with `!wa login qr` rather than `login qr`, or start a new direct chat with the bot;
"This is you" says "open your chat with" rather than "accept the invite from".

**Verified.**

- `cargo test -p hs-bridges`: 23 unit tests (7 new, against a fake client API that records
  every call: the leave/invite/join/notice sequence as the right users; a chat the owner
  started is left alone after two reads; no double puppeting says the prefix once; a chat the
  bot is out of is forgotten and the next step creates one as the owner; a failed rejoin
  forgets the chat and the reason says "a new chat will be started"; an unaccepted invitation
  waits, then repairs without a second look at the state; a new chat is recorded `owner` and
  never looked at again; the notices' text and HTML), 6 integration tests.
- `cargo test -p hs-bridge-conformance --test real_mautrix_login`: the real binary and the
  real `dock.mau.dev/mautrix/whatsapp:latest` in Docker, **3 of 3**: the two existing stories
  (now also asserting `chat_started_by: owner` and `chat_room`), and the new
  `a_chat_the_bot_started_is_repaired_in_place_and_login_qr_gets_a_qr_code`: the offering
  with `double_puppeting: false` so the bot starts the chat (the exact shape of the owner's),
  alice accepts, `chat_started_by: bot`; then the registration is given the claim on her
  (`PATCH /appservices/{id}` `namespaces.users`) and the offering `double_puppeting: true`;
  the manager's next step repairs the chat; alice's client sees, decrypted, the bridge's
  "This room has been marked as your management room" and, plain, the bot's "come back on your
  invitation"; the bridge's log has `Accepted invite to room as bot`; the server's has
  `repaired the owner's chat`; `login qr` is `Received command`, and the QR comes back.
  All three in one run: `3 passed`, 32.7 s with the image present (the repaired story alone,
  15.3 s). The bridge's log for it is `repaired-bridge.log` under `HS_BRIDGE_LOGIN_LOG_DIR`.
- `web`: `InstanceNextSteps` 11 tests (2 new), `bridge-next-steps` 13; `tsc -b`, eslint,
  prettier on the changed files; `BridgeOfferingPage.test.tsx` now expects "This is you: open
  your chat with" (the mock's ready instances carry `chat_started_by: owner`); `npm run check`
  green (529 tests; the four eslint warnings are pre-existing, in files this branch does not
  touch).
- `cargo test -p hs-admin` (293), `cargo test -p hs-cli --test bridge_offerings` (3 of 3 on the
  real binary, the chat started as alice as before), `cargo clippy -p hs-bridges -p hs-admin -p
  hs-bridge-conformance --all-targets -- -D warnings`, `cargo fmt --all --check`. The workspace
  gate was not run (merge queue's job).

**For the owner, now.** The repair reaches the demo with the next roll of `main` after this
branch merges; until then nothing on the cluster has changed. The by-hand repair (the bot
leaves, brandon re-invites it, with the instance's own token through a port-forward) was
written and ready (`repair.sh` in the scratchpad) but **not run**: the session's classifier
declined the change to the owner's live chat, and it is not pursued. So, either:

- **Today, in the existing chat:** type `!wa login qr`. The bridge answers "⚠️ This is not
  your management room …" and then the QR (the handover's answer; `!wh` was a typo for this).
- **Or, from Element:** start a new direct chat and invite `@whatsappbot_brandon:myelin.dacrib.net`;
  the bot says the room is marked as the management room; `login qr` works there.
- **After the roll** (`helm --kube-context admin@dacrib0 upgrade myelin deploy/helm/hs -n
  myelin -f /tmp/myelin-values.yaml --set image.tag=sha-<the merged commit> --wait --timeout
  10m`): within a few seconds of the server starting, the existing chat gets a leave and a
  rejoin of the bot, the bridge's "marked as your management room" line and the bot's
  explanation; then `login qr` there. The bridge page shows the chat as the owner's. If
  instead the page shows the chat as the bot's, the instance is one without double puppeting
  and the chat says what to type.

**Decisions.** Repair in place rather than a new chat: the person's history and `m.direct`
entry stay, and nothing new appears in their room list. The bot is a member of the same room
twice over in its history, which is what the repair looks like in a client. The catalogue
claims a command prefix only where a bridge's source states one. The live chat was not
touched by hand.

**Left.** Signal, Slack and X have no `command_prefix` yet (the notice falls back to "with my
command prefix"). Changing an offering's `double_puppeting` after instances exist does not
update their registrations (the real-bridge test patches `namespaces` by hand; the manager
could re-render the claim). The repair's cluster run awaits the roll; the owner types in the
chat after it. The unit-test fake answers `joined_members` and `state` only as the bot, which
is all the manager asks.

## Session 2026-10-02 (branch `agent/bridge-login`): typing `login qr` to the personal bot did nothing

**What the owner saw.** A per-person mautrix-whatsapp bridge set up from the interface, the
invitation from `@whatsappbot_<localpart>` accepted in Element, `login qr` sent in that chat: no
reply, no QR code. The chat is end-to-end encrypted by default (`options.encryption`), so the
first suspicion was appservice-mode encryption (MSC3202/4203/4190), which no person had ever
typed through.

**Reproduced with the real bridge**, and the suspicion was wrong. The new test
`crates/hs-bridge-conformance/tests/real_mautrix_login.rs` boots the real `hs` binary
(`public_baseurl: http://host.docker.internal:<port>`, embedded store, local media), claims it,
registers alice on a `matrix-sdk` client with `e2e-encryption` on, enables the offering
(`PUT /api/v1/bridge-offerings/mautrix-whatsapp {"runtime":"elsewhere"}`), makes her instance,
writes its files (`POST .../instances/@alice:test.local/files`) into a volume, runs
`dock.mau.dev/mautrix/whatsapp:latest` (`v26.09+dev.a0325e76`) against them, patches the
registration's `url` to the container's port, waits for `ready`, and acts as alice: joins the
chat and sends `login qr`, then waits 30 s for the bot. On `main`'s manager, with the chat
encrypted:

- The server delivered everything: `delivered a transaction ... txn_id=6 events=1` (the
  `m.room.encrypted`), `txn_id=7 to_device=1` (her client's `m.room_key` for the bot's device,
  270 ms later); `appservices.health` said `healthy`, no backlog.
- The bridge **decrypted it**: `Decrypting received event event_id=$qwmu...`, `Couldn't find
  session, waiting for keys to arrive... wait_seconds=3`, `Created inbound olm session ...
  sender=@alice:test.local`, `Upserting megolm inbound group session`, `Got keys after waiting,
  trying to decrypt event again`, **`Event decrypted successfully decrypted_event_type="m.room.message
  (message)"`**. Then nothing at all: no command, no request to the room, no warning.
- The same message typed as **`!wa login qr`** (`HS_BRIDGE_LOGIN_TEXT`) was answered at once,
  encrypted, by three events the client decrypted: an `m.notice` **"⚠️ This is not your
  management room. Entering login info must be prefixed with `!wa` like other commands."**, an
  `m.notice` "Scan the QR code with the WhatsApp mobile app to log in", and an `m.image` (the QR,
  uploaded to this server's media).

**Root cause (the manager's).** A mautrix `bridgev2` bridge takes a bare command (no `!wa`)
only in the sender's *management room* (`bridgev2/queue.go`, `QueueMatrixEvent`:
`strings.HasPrefix(msg.Body, CommandPrefix) || evt.RoomID == sender.ManagementRoom`), and the
only place it ever sets a management room is `handleBotInvite` (`bridgev2/matrixinvite.go`):
*the person invites the bot* into a room, the bot accepts, and if the room then has two members
it is marked. Any other room is "not a portal" (`ErrNoPortal`, `WithSendNotice(false)`): the
message is dropped without a word, even after being decrypted. `BridgeManager::invite_owner`
created the chat **as the bot and invited the owner**, so the bridge saw its own bot create a
room and a person join it, and never an invitation to accept. Every chat the manager had ever
made was such a room; encryption had nothing to do with it (the unencrypted chat failed the
same way, in the second run of the test before the fix).

**The fix** (`crates/hs-bridges/src/manager.rs`, `invite_owner`): where the instance may act as
its owner (double puppeting on, which is the registration's non-exclusive claim on the owner and
the catalogue's default for every mautrix type), the chat is **created as the owner with the bot
invited** (`is_direct`, encrypted per the option, `m.direct` on both sides), and the manager
joins the bot at once so the sign-in steps can be posted; the bridge is sent the invitation in a
transaction, accepts it (`Accepted invite to room as bot`), counts two members and marks the
room. Without double puppeting the manager cannot act as the owner, so the chat is still the
bot's and its first line now says so: "The bridge only takes commands in a chat you start:
invite `@whatsappbot_…` to a new direct chat, then follow these steps there." The front door's
"ready" line says "I've started a chat for you with …: open it" in the first case and keeps
"I've invited you to a chat with …: accept it" in the second. A new `info` line names the room:
`started the owner's chat with their bridge's bot bridge_type= owner= room= started_by=owner|bot
encrypted=`.

**After the fix, the same test, both chats** (`cargo test -p hs-bridge-conformance --test
real_mautrix_login`, 2 of 2, 24 s with the image present): alice's `login qr` is
`Received command mx_command=login`, the bridge dials `wss://web.whatsapp.com/ws/chat`,
`Received QR codes code_count=6`, and answers "Scan the QR code with the WhatsApp mobile app to
log in" and the `m.image`; in the encrypted chat both are `m.room.encrypted` and her client
decrypts them (`Sharing group session for room ... destination_map={"@alice:test.local":{...}}`,
the bot's `m.room_key` through `/sendToDevice` and her `/sync`'s `to_device`). So **appservice-mode
encryption works end to end with a real bridge and a real encrypting client, in both
directions**, which until now had only been inferred from key uploads and a key request. The
test skips, saying why, without Docker, the image or the binary; `HS_BRIDGE_LOGIN_LOG_DIR`
keeps both logs.

**For the owner's running server, right now.** The existing chat was made by the bot and will
never be a management room; nothing in the server can change that after the fact. Either:
(a) in that chat, type **`!wa login qr`** (the prefixed form works anywhere the bot is; the bridge
will say the room is not its management room, then show the QR), or (b) from Element, **start a
new direct chat and invite `@whatsappbot_<localpart>:<server>`**: the bot accepts, says "This
room has been marked as your management room", and `login qr` works there. Or, after this
branch is deployed, `stop whatsapp confirm` and `start whatsapp` with `@bridges` (or delete and
re-create the instance on the offering page): the new chat is started as the person and works
as the steps say. Instances that already have a `dm_room` are left alone.

**How to see this when it happens again.** The server cannot tell that a bridge dropped a message
it was delivered: `appservices.health` and the backlog say delivered, and they are right. The
place to look is the bridge's own log (`docker logs <container>`, or `kubectl -n <bridges
namespace> logs deploy/<deployment.name>` from the instance's `deployment` on the Bridge page):
a message that was taken is `Received command mx_command=…`; one that was decrypted and dropped is
`Event decrypted successfully` followed by nothing; one that could not be decrypted is `Failed to
decrypt event` or `Didn't get session, giving up`, and then the server's `delivered a transaction
... to_device=` lines say whether the key ever went out.

**Verified.** `cargo test -p hs-bridge-conformance` (the 4 synthetic scenarios and the 2 real
ones), `cargo test -p hs-bridges`, `cargo test -p hs-cli --test bridge_offerings` (3; two of
them asserted the old invitation and now assert the chat started as alice with the bot's
invitation from her, `is_direct`, both joined, and the bridge sent the invitation),
`cargo clippy -p hs-bridges -p hs-bridge-conformance -p hs-cli --all-targets -- -D warnings`,
`cargo fmt --all --check`. All of these were green at the branch's tip (the first commit's message
said `bridge_offerings` was still running; it finished 3 of 3, 5.9 s). The workspace gate was
not run (merge queue's job).

**Not verified / left.** The `cluster` runtime path is the same code but was not run on kind
today. Nobody scanned the QR with a phone, so the bridge's `login` flow past its first step, and
a WhatsApp message in either direction, remain unseen. The bridge's welcome ("Hello, I'm a
WhatsApp bridge bot … This room has been marked as your management room") and the manager's
steps both land in the chat, in either order. `mautrix-discord` was not checked to be on
`bridgev2`; if it still uses the older framework its management-room rule may differ.

## Session 2026-10-02 (branch `agent/bridge-names`): the pod says what bridge and whose it is

**The complaint.** The owner set up a WhatsApp bridge for `brandon` from the interface and found
`bridge-7c1e92a0-6d4b7f9c8f-x7k2p` in `kubectl get pods`: nothing in the name said what bridge
or whose. `hs_bridges::manager::deploy_name` named every object of an instance
`bridge-<8 hex of sha256(appservice id)>`, "short, DNS-safe and stable", and nothing else.

**The rule now** (`deploy_name`, the manager's module doc, RFC 0017 section 4.1's table): the
appservice id made a DNS-1123 label behind `bridge-`, so a person's instance is
`bridge-<short type>-<owner localpart>` (`bridge-whatsapp-brandon`, pods
`bridge-whatsapp-brandon-<replicaset>-<pod>`, Secret `-files`, claim `-data`) and a shared one is
`bridge-<short type>` (`bridge-heisenbridge`). Lowercase, `[a-z0-9-]`, every other character
mapped to `-`, runs collapsed, no `-` at either end, and at most 46 characters
(`DEPLOY_NAME_MAX`: `-<10 hex pod-template hash>-<5>` on top stays within a pod name's 63). When
mapping or shortening changed the id, `-<6 hex of sha256(appservice id)>` is appended so two ids
that come out the same text stay apart (`whatsapp-a.b` and `whatsapp-a-b`; `whatsapp-ali=5fce`,
the encoded `ali_ce`, is `bridge-whatsapp-ali-5fce-<6 hex>`); an id that needed no change gets no
suffix. Seven unit tests: a plain id, the shared case, dots and uppercase, a very long owner
(exactly 46, a pod name under 63, two long ids with one prefix differ), two owners that map to the
same text, the legacy name, a row without the field.

**Labels on every object.** The `Bridge` spec gained `owner` (the Matrix ID; absent for a
shared instance; CRD regenerated, an `Owner` print column), and the operator labels the claim,
Deployment, Service and pods `myelin.dev/owner=<label-safe Matrix ID>` (`alice-example.org`)
beside the `myelin.dev/bridge-type` and `myelin.dev/appservice-id` it already set, with the exact
owner in an annotation of the same name; so `kubectl get pods -l myelin.dev/owner=brandon-example.org`
lists one person's bridges. The manager puts the same three labels on the `Bridge` itself and
the rendered `manifest_yaml` carries `owner`. The admin API's `BridgeInstance.deployment.name`
already carried the object name (the interface shows it); its OpenAPI description now states the
rule, and the document is **0.1.1** (`agent/admin-token` takes it to 0.1.2 on its own; whichever
merges second becomes 0.1.3).

**Nothing running is orphaned.** The name is decided once, when the instance is first named, and
stored on its row (`InstanceRow::deploy_name`, now `#[serde(default)]`); nothing recomputes it, so
**a running instance keeps its name until it is removed and asked for again** -- renaming would
mean a second Deployment, a second claim (the bridge's SQLite and crypto store live on it) and a
registration `url` pointing at a Service that no longer answers. Instances deployed before today
have the hashed name on their rows and keep it. A row with an appservice id and no stored name
(none should exist: the field has been written since the manager's first version, but an older
row reads back with `None` now) is named on its next `step`: `legacy_deploy_name` is asked of the
runtime first and adopted if an object is there, else the readable name; one log line says which
(`named the bridge instance's objects`, with `adopted`). Four manager tests over the in-memory
store and a fake runtime: a new instance deployed as `bridge-whatsapp-brandon` with the owner
label and the Service URL to match, a shared heisenbridge as `bridge-heisenbridge` without one, an
unnamed row with an object under the hashed name adopting it (no second deployment, never renamed
on later ticks, deleted on removal), and one with nothing running getting the readable name.

**Verified**: `cargo test -p hs-bridges` (15 + 6), `cargo test -p hs-operator` (92, including
the regenerated CRDs matching and the operator's label tests), clippy on `hs-bridges`,
`hs-operator`, `hs-admin`; `hs-cli` compiles with the new `owner`. `kind-smoke.sh`: see status
12's entry of the same date for whether it ran. **Not verified:** a real instance with a pre-2026-10-02
row on the owner's cluster; the demo cluster's one WhatsApp instance keeps its hashed name until
it is removed and requested again, by design.

## Session 2026-10-01 (branch `agent/bridge-logins`): the admin API says who has signed in to a bridge

**The gap** (`docs/next-steps.md`, "A bridge's per-user sign-in state is invisible to the admin
API"): the Sign in tab said how to sign in, never who had; the bridges keep that state
themselves. Now `GET /api/v1/appservices/{id}/logins?user_id=` (`appservices.logins`,
`bridges:read`, a read so not audited) asks the bridge, honestly per bridge type:

- **Which types answer.** The catalogue (`hs_admin::bridge_types`) gained `provisioning_api`
  and `provisioning_note` per type (`BridgeType`, OpenAPI). Every `mautrix-*` entry is
  `mautrix_v3` (a `bridgev2` bridge's `/_matrix/provision/v3`, on its appservice listener, the
  registration's `url`) and **is asked**; heisenbridge is `none` (no provisioning API, its
  state is its control room), matrix-appservice-irc is `irc_v1` (it links rooms to channels,
  reports no nicks), hookshot is `hookshot_v1` (connections, not accounts). Those three answer
  `200 {"supported": false, "reason": ...}`, never a `501`; so does a registration not made from
  the catalogue.
- **The secret.** A mautrix render (`POST /bridge-types/{type}/render`) and an offering's
  instance (`hs-bridges`, minted with its tokens, `InstanceRow::provisioning_secret`,
  `#[serde(default)]`) now carry a 64-hex `provisioning.shared_secret` in `config.yaml` and the
  same value in the registration's `io.myelin.provisioning_secret` (`PROVISIONING_SECRET_KEY`),
  which the registry keeps with the other unrecognised keys and exports with the registration
  (which already carries both tokens). A registration made before this has none and says so,
  naming the key; an administrator can copy the bridge's own secret in with a merge patch.
  **Found on the way, no row:** `PATCH /appservices/{id}` silently dropped any top-level key
  the registration format does not define (the merge went into a JSON shape that holds
  unrecognised keys under `extra`); it now merges such keys into `extra`, and a `null` removes
  one (`Registry::update`, `PATCHABLE_KEYS`).
- **The answer** (`hs_admin::bridge_logins`, pure: `plan` decides from the registration whether
  and how to ask, `answered` normalises mautrix-go's `RespWhoami`, `failed`/`refused` shape a
  failure): `{appservice_id, bridge_type, provisioning_api, supported, reason, user_id,
  signed_in, logins: [{user_id, remote_id, remote_name, state, state_reason, since}],
  checked_at, cached, error}`. `state` is the bridge's `state_event` lower-cased
  (`connected`, `bad_credentials`, ...); `since` is its `state_ts` (seconds) as RFC 3339;
  `remote_name` is the login's `name`, else the profile's name, phone, username or email.
  `user_id` may be left out for a per-user instance (its owner is asked about); a shared bridge
  without it is a `400` at `/user_id`.
- **The asking** (`hs_appservice::provisioning::BridgeLogins`, behind
  `AppserviceDirectory::logins`, a new trait method whose default answers `503`): bearer secret,
  `?user_id=`, 10 s timeout, no redirects. An unreachable, refusing or slow bridge is a `200`
  with `error: {status, reason: unreachable|timeout|refused|invalid_answer, detail}` (502, 504,
  or the bridge's own status with its `errcode`), the way a ping reports an unreachable bridge.
  Answers are kept **30 s per (bridge, user)**; failures are not kept.
- **Observability.** `hs_admin_bridge_login_queries_total{type,outcome}` (`outcome`:
  `answered`, `cached`, `unsupported`, `unreachable`, `timeout`, `refused`, `invalid_answer`;
  registered with the other appservice series in `AppserviceMetrics`, so `hs serve` needed no
  change beyond handing the directory its metrics in `appservice_delivery.rs`), and a `warn`
  line `could not ask a bridge who has signed in` with the appservice, type, user, URL, reason,
  status and detail.
- **The interface** (`web/src/components/BridgeSignInState.tsx`, on the bridge page's Sign in
  tab above the guide): "Signed in as +1 555-123-4567 since ..." per login (a badge and the
  bridge's reason when a login is not `connected`), "@bob:… is not signed in.", "Could not ask
  the bridge: ..." when it could not be asked, and "This bridge keeps who has signed in itself."
  with the reason for a type without an API. A shared bridge gets a "Matrix user" box that
  starts with the operator's own ID.

**Verified.**

- `cargo test -p hs-admin`: `bridge_logins::tests` (5: planning per type and per missing
  piece, the whoami normalisation including seconds and milliseconds, a null `logins`, a
  refusal's words), `bridge_types::tests::every_type_says_what_provisioning_api_it_has_and_only_mautrix_reports_logins`,
  the mautrix render and instance tests now assert the secret in both files, and
  `router::tests::a_bridges_logins_are_read_per_type_and_a_type_without_an_api_says_so`
  (`bridges:read` suffices; heisenbridge `200 supported: false`; `400` at `/user_id`; `404`).
  The contract test still holds; `tools/admin_api_coverage.py`: **161 of 161**.
- `cargo test -p hs-appservice`: `provisioning::tests` (4, against an axum stand-in serving
  mautrix's `whoami` shape: answered then cached with one request to the bridge, per-user
  caching, an instance's owner asked about unnamed; an unreachable port, a wrong secret (twice,
  not cached), a bridge slower than the timeout, each an answer with the error; heisenbridge and
  a custom registration not asked; the counter for each outcome);
  `admin_directory::tests::a_registrations_provisioning_secret_is_kept_and_a_patch_can_add_one`
  (failed before the merge-patch fix: the patched secret vanished); `metrics::tests`.
- `cargo test -p hs-bridges`: the manager test asserts an instance's registration and
  `config.yaml` carry the same secret and that its owner is asked about at
  `http://whatsapp-alice:29318/_matrix/provision/v3/whoami`.
- `crates/hs-cli/tests/bridge_logins.rs`,
  `the_admin_api_says_who_has_signed_in_to_a_bridge_and_says_so_when_it_cannot` (**1 of 1,
  29 s**, the real `hs` binary from a configuration file): mautrix-whatsapp rendered and
  registered over the admin API, its stand-in accepting only the secret from the rendered
  `config.yaml`; alice signed in (`remote_name`, `state: connected`, `since`), bob not, alice
  again `cached` with the bridge asked twice in all, `400` with no `user_id`; heisenbridge
  `supported: false`; a mautrix-signal with nothing listening, `error.status 502
  unreachable` and the `warn` line; the four counter series on `/metrics`.
- **The real mautrix-whatsapp** (`docs/bridges/mautrix.md`, "2026-10-01"), not signed in: its
  config upgrader kept the rendered secret, and the admin API answered `supported: true,
  signed_in: false, logins: []` from its `whoami`; asked again, `cached: true`; with the
  container stopped, `error.reason: unreachable`. No phone, so no `logins[]` from a real
  account.
- `web`: `BridgeSignInState.test.tsx` (4); mock server answers the operation for every case.

**Left.** Only mautrix `bridgev2` bridges answer. mautrix-discord was not checked to be on
`bridgev2`; if it is not, its answer is an honest `refused` (404 or 401) rather than a list.
hookshot's provisioning API may expose linked GitHub/GitLab accounts; not read. The cache is per
replica. The offering page lists instances without their sign-in state (one request per
instance; the instance's registration page has it).

## 2026-10-01 (branch `agent/platform-gaps`, track 12): RFC 0017's `cluster` runtime has run

The known gap "RFC 0017's `cluster` runtime has never run" is closed: on a kind cluster with the
chart installed (`bridges.enabled`, its default, so the operator runs and the server gets
`MYELIN_BRIDGES_NAMESPACE` and `MYELIN_BRIDGES_HOMESERVER_URL`), the server deployed a real
heisenbridge through the operator and walked it to `ready`. No change to `hs-bridges` was
needed. `deploy/operator/ci/kind-smoke.sh --heisenbridge` does the walk; the transcript is
`docs/status/transcripts/operator-kind-smoke-2026-10-01.txt` (image built from the branch's
tree), and it went the same way twice before that by hand and with the published
`sha-a01c1e0` image.

The calls, as an administrator who claimed the server through its setup link:

- `GET /api/v1/bridge-deployment-target` →
  `{"available":true,"namespace":"hs-op-smoke-19d26e","homeserver_url":"http://myelin-hs.hs-op-smoke-19d26e.svc:8008","reason":null}`.
- `PUT /api/v1/bridge-offerings/heisenbridge {"runtime":"cluster"}` → the shared offering,
  `runtime: cluster`, image `hif1/heisenbridge:latest` from the catalogue, `instances:
  {"requested":1}`.
- Then `GET .../instances/_` every quarter second. The manager's own log (to the millisecond):

  | state | at (UTC) | since the `PUT` |
  |---|---|---|
  | requested → registered | 05:38:59.138 | 0.0 s |
  | registered → deploying ("waiting for the pod") | 05:38:59.501 | 0.4 s |
  | deploying → starting ("waiting for the bridge to answer this server") | 05:39:45.843 | 46.7 s |
  | starting → ready (the bridge answered the ping) | 05:39:46.255 | 47.1 s |

  In `deploying` the instance's `deployment` carried the operator's own words as they changed:
  `Pending` "waiting for the pod", then "waiting for the pod: PodInitializing", then `Ready`
  "the bridge is accepting connections on port 9898". Final: `state: ready`, `health:
  healthy`, `deployment.name: bridge-19a1359c`, `service_url:
  http://bridge-19a1359c.hs-op-smoke-19d26e.svc:9898`, **pod
  `bridge-19a1359c-7c8c8964c7-9rhgf`**, image `hif1/heisenbridge:latest` (pulled by the kind
  node from Docker Hub), `Bridge` `bridge-19a1359c` `Ready`. heisenbridge's log ends "Init done
  with 0 networks connecting, bridge is now running!", and `@heisenbridge:smoke.invalid` is a
  user with `appservice_id: heisenbridge` (it registered its bot through the server with the
  instance's own token).
- `DELETE .../instances/_` → 204; the `Bridge`, its files Secret, Deployment, Service, claim and
  pod were gone in 34 s. `DELETE /api/v1/bridge-offerings/heisenbridge` → 204.

In the two earlier runs `deploying` lasted 81 s (hand run, 02:49Z) and 82 s (published image,
03:09Z) on a more loaded machine; the 46 s here is mostly the pod's two image pulls (the init
container and the bridge each pull `:latest`, because the runtime sets no pull policy and
Kubernetes defaults `:latest` to `Always`) and the claim's first binding.

Left: a per-user offering (mautrix-whatsapp, which needs a real account to sign in) has not
been deployed this way; nothing has run on a multi-node cluster or with a StorageClass other
than kind's `local-path`; the walk is not in CD (it pulls someone else's `:latest`), so it is a
by-hand check: `deploy/operator/ci/kind-smoke.sh <image> --kind <cluster> --heisenbridge`.

## Session 2026-09-30 (branch `agent/as-ephemeral`): bridges are sent everything but events, too

**The gap** (`docs/next-steps.md`, "Appservice delivery carries events only"): `Transaction`
had the fields for MSC2409 ephemeral events and to-device messages, MSC4203 and MSC3202, and the
pump filled `events` alone. mautrix-whatsapp in appservice-mode encryption (2026-09-25) had asked
for all of it and been sent none. Decision 0021 records the design; the short version:

- **Where it reads from.** Receipts, presence and to-device messages each got a server-wide
  stream in the store that owns them, appended in the write's own transaction
  (`hs_user.receipt_stream`, `hs_user.presence_stream`, `hs_e2e.to_device_stream`;
  `UserStore::{receipt,presence}_stream_{since,head}` and `prune_*`,
  `ToDeviceStore::to_device_stream_{since,head}`, `prune_to_device_stream`). Device lists
  already had one (`DeviceKeyStore::changed_users_since`). Typing has none: the session hub
  gained `install_ephemeral_observer` (`hs_user::hub::EphemeralObserver`), called from its
  local publish path and from `apply_ephemeral`, so on every replica it hears every change.
- **`hs_appservice::ephemeral`**: `EphemeralPump` over two traits `hs-cli` implements
  (`EphemeralSource` over the hub, its store and the room registry; `DeviceSource` and
  `KeyCountSource` over the E2EE store). `tick()` reads each stream from each appservice's
  durable position (`hs_appservice.ephemeral_pos`, `(appservice, stream)`), builds one
  transaction per appservice with everything past its position that it is interested in
  (Synapse's rules, `Interest`), and queues bodies and positions in **one store transaction**
  (`AppserviceStore::enqueue_ephemeral`), then prunes each stream at and below the lowest
  position. A new appservice starts at each stream's head. `note(Change)` is the doorbell:
  typing rooms are remembered and sent as the room's current typing set, once per tick.
- **`hs-cli`**: `appservice_delivery::Sources` (the trait impls), `Doorbell` (the hub's
  observer), `follow_ephemeral` (a tick on every doorbell and every 250 ms, on the global
  shard's owner; others discard their typing notes), `DeliveryDeps` (the start arguments,
  now with the hub, the E2EE store and the metrics). The event pump also carries MSC3202 key
  counts now (`Pump::with_key_counts`: the bot and its users among the room's members).
- **Shapes** (Synapse's, checked against `refs/mautrix-go`'s parser): `typing_event`,
  `receipt_event`/`receipt_content`, `presence_event` in `transaction.rs`; **`ToDeviceEntry`
  is now flattened** -- `to_user_id` and `to_device_id` beside `type`, `sender`, `content` --
  where before the event sat under an `event` key no bridge reads. The conformance suite
  asserts the flat shape.
- **To-device messages are not deleted when pushed** (Synapse does not either): a
  double-puppeting registration's non-exclusive namespace names a real person, whose own
  clients need the message, and a sync-mode mautrix bridge ignores what is pushed. Written
  down in decision 0019 with the cost (a never-syncing bot device's queue grows; track 08's
  retention question).
- **Observability**: `hs_appservice_transactions_total{appservice,outcome}` and
  `hs_appservice_delivered_items_total{appservice,kind}` (`hs_appservice::metrics`,
  registered by `hs serve`), and the scheduler's `info` line `delivered a transaction to an
  appservice` with `events=`, `typing=`, `receipts=`, `presence=`, `to_device=`,
  `device_list_changes=`, `one_time_key_counts=`, `fallback_key_types=`. The admin API's
  backlog entries never counted events, so an ephemeral-only transaction is reported like any
  other; the health read after one says `healthy`.

**Verified by running.**

- `crates/hs-cli/tests/appservice_ephemeral.rs`,
  `a_bridge_is_sent_ephemeral_data_once_across_a_restart_and_not_while_paused` (**1 of 1,
  19 s**, debug binary): a real `hs serve` from a configuration file importing a registration
  with `receive_ephemeral` and `org.matrix.msc3202` whose `url` is an axum stand-in; alice,
  bob, the bot and `@ghost_alice` (the last two registered through `m.login.application_service`,
  the ghost joined by masquerading). Alice types in the ghost's room and in bob's private room:
  one `m.typing` for the ghost's room, none for the private one. A receipt in each: one
  `m.receipt` with `content[event_id]["m.read"][alice].ts`. `PUT /presence` unavailable with a
  status message: `m.presence` from alice with `status_msg`, `last_active_ago`, no `user_id`
  inside. `/sendToDevice` to the ghost's device and to bob's: one to-device entry, the flat
  shape, under both spellings. Alice uploads device keys: `device_lists.changed` names her.
  The bot uploads two one-time keys: the next transaction carries
  `device_one_time_keys_count[bot][device].signed_curve25519 == 2` under all three spellings.
  `/metrics` has every `kind` and the log has the delivery line with `to_device=1`. SIGTERM
  and a restart over the same data directory: three seconds later the stand-in's tallies are
  unchanged (nothing resent), and a receipt after the restart arrives exactly once. Paused
  through the admin API: a presence change and a to-device message sit in the backlog and
  nothing arrives; resumed: both arrive, the to-device message once; health `healthy`.
  Mutant: with the pump's `tick` replaced by `Ok(vec![])` the test fails at "alice's typing
  reached the bridge" (73 s, the bound).
- `cargo test -p hs-appservice`: 99 (from 74). New: `ephemeral::tests` (6: interest and
  private-receipt scoping, typing/receipts/presence to the interested appservice and nobody
  else with positions moving and streams pruned, to-device and device lists with key counts, a
  restarted pump continuing from stored positions, a stream further behind than a page read
  whole and in order, streams kept empty with nobody listening); `store::tests::
  ephemeral_positions_are_written_with_the_bodies_they_account_for`; `transaction::tests`
  (the flat to-device entry, the shapes, `body_counts` reading either spelling once);
  `metrics::tests`; `pump::tests::an_msc3202_appservice_is_sent_key_counts_with_its_events`.
- `cargo test -p hs-user`: 148 lib + 6 (`store::tables::tests::
  receipt_and_presence_writes_append_to_the_server_wide_streams`, `hub::tests::
  the_ephemeral_observer_is_told_of_local_and_peer_changes`); `cargo test -p hs-e2e`: 30 lib +
  27 (`to_device_stream_names_every_queued_message_and_is_pruned_below_a_position`);
  `hs-bridge-conformance` 4; `hs-bridges` 9; `cargo test -p hs-cli --test bridge_offerings`
  3; `cargo fmt --all --check`; `cargo clippy --workspace --all-targets -- -D warnings`.
- **The real mautrix bridge** (`docs/bridges/mautrix.md`, "2026-09-30"): mautrix-whatsapp
  `v26.09+dev.a0325e76` in Docker, from an offering's rendered files, in appservice-mode
  encryption. 200 ms after its key upload it was sent its own device-list change with its
  one-time-key count and fallback key type and logged "Device list changes in /sync
  changes=[@whatsappbot_alice:test.local]"; alice's typing, receipt and presence arrived as
  `unstable_edu` transactions (the registration asks for the legacy spelling); and an
  `m.room_key_request` sent to its device arrived as `{"to_device":1}` and was handed to its
  Olm machine ("Starting handling to-device event ... type=m.room_key_request"). Eleven
  transactions delivered, counted the same on both sides. No phone, so no encrypted message.

**Not done / what is left.** `device_lists.left` is never filled (Synapse's TODO too). Key
counts are computed per transaction with one device listing per interesting user: a room with
hundreds of ghosts costs hundreds of keyed reads per event, unmeasured. Typing that changes
while no replica owns the global shard is lost (as designed). The to-device queue of a bot
device that never syncs is not pruned (decision 0019). A cluster run (two replicas, one bridge)
of the ephemeral pump has not been watched; the gate is the same as the event pump's, which
`appservice_delivery::tests::a_replica_pumps_and_delivers_only_for_the_shards_it_owns` covers.


**RFC 0017 runs end to end against the real binary** (`docs/next-steps.md` item 1). Everything
below marked *ran* was watched happening over the bound socket of a real `hs serve`; everything
marked *written* is code and documentation that a test does not yet reach.

Ran, in `crates/hs-cli/tests/bridge_offerings.rs` (three tests, in-process `spawn_serve`, the
admin API as the interface calls it, the client API as a Matrix client calls it, and the
appservice API the server delivers to; `cargo test -p hs-cli --test bridge_offerings`):

- `an_offering_takes_an_instance_from_requested_to_ready_and_removes_it_again`: no cluster, so
  `GET /bridge-deployment-target` says `available: false` with the reason and `PUT` with
  `runtime: cluster` is a `400` at `/runtime` saying why. Before the first offering the manager's
  registration (`myelin-bridges`) reserves `@bridges` alone and the users list holds the
  administrator and alice, nobody else; `PUT /bridge-offerings/mautrix-whatsapp` (elsewhere,
  everyone) grows the namespace to `@bridges` and `@whatsappbot` and the two bot accounts appear,
  attributed to `myelin-bridges`. `PUT .../instances/@alice:example.org` (a remote user and `_`
  are refused) is `requested` and walks to `registered` (its registration `whatsapp-alice` in
  `appservices.list`, tagged `io.myelin.bridge_instance: @alice:example.org`, namespaces
  `@whatsapp_alice_.*`, `@whatsappbot_alice`, non-exclusive `@alice`) and `starting` (reason:
  waiting for someone to run it). `POST .../files` carries the instance's tokens, the server's
  **bound** address (there is no `public_baseurl`), alice as `admin` in `permissions`, and the
  registration the registry holds. An axum stand-in answering `/_matrix/app/v1/ping` with the
  instance's `hs_token` is patched into the registration's `url` (`appservices.update`) and the
  manager's next ping takes the instance to `ready` (`ready_at`, `health: healthy`,
  `last_ping_at`); alice's `/sync` shows the invitation from `@whatsappbot_alice` with
  `is_direct`, the chat holds the sign-in steps (`login qr`), her `m.direct` names the room
  (written through double puppeting), and the stand-in was sent her join. `DELETE` the instance:
  `404`, registration gone, the instance's tokens refused by the client API, the bot account
  still an account. `DELETE` the offering: the namespace back to `@bridges`, offerings empty.
- `a_person_gets_a_bridge_by_messaging_its_front_door_and_the_manager_bot_takes_commands`: an
  offering open to alice only. Alice invites `@whatsappbot` to a direct chat; the server delivers
  the invite to `/_myelin/bridges/_matrix/app/v1/transactions/{txn}` over loopback
  (`myelin-bridges` is `healthy` afterwards), the bot joins and says an administrator runs
  WhatsApp bridges here and it has asked for one; an instance exists for her; "hello?" gets
  "still being set up (starting)" and no second instance. Bob invites it and is told once,
  politely, that it is not available to his account; two more messages get nothing. The
  stand-in is registered, the instance goes `ready`, the DM invitation arrives and the front
  door says "Your WhatsApp bridge is ready. I've invited you to a chat with
  @whatsappbot_alice...". `@bridges`, invited, explains itself; `list` says
  `- WhatsApp: \`start whatsapp\` (yours: ready)`, `status` says `- WhatsApp: ready`,
  `stop whatsapp` asks for `confirm` and changes nothing, `stop whatsapp confirm` removes the
  instance and its registration, `status` then says there are none, `start whatsapp` asks for
  one again, `start pigeons` and `stop signal` ask which one; bob's `list` offers nothing.
- `a_shared_offering_has_its_one_instance_from_the_start_and_heisenbridge_runs_from_its_files`:
  `PUT /bridge-offerings/heisenbridge` answers `mode: shared`, no front door, and one instance
  with `user_id: null`; `.../instances/_` reaches `starting` as `heisenbridge`
  (`@heisenbridge`), its files have no `config.yaml`, and its Compose command names no owner.
  **heisenbridge 1.15.4 (`pip install heisenbridge`) was run from the rendered
  `registration.yaml`** (`heisenbridge -c registration.yaml -l 127.0.0.1 -p <port> <server>`,
  the registration's `url` patched to that port): it registered `@heisenbridge` with the
  instance's token, answered the manager's ping, and the instance reached `ready` in about five
  seconds. Without `heisenbridge` on the path the test says so and a stand-in answers instead.
  `DELETE` with the instance is a `409` naming the count; with `remove_instances=true` it takes
  the registration with it.

Ran, in the interface (`web/e2e-real/bridge-offerings.spec.ts`, Playwright against the real
binary through the Vite proxy, `HS_REAL_SERVER_URL` and `HS_REAL_ADMIN_TOKEN` set and
`@alice:example.org` registered; screenshots `docs/design/screenshots/bridge-offerings-{list-empty,
wizard-runtime,offering,add-refused,instance-starting,files,list-after}-real.png`): the Bridges
page says the server cannot run bridges itself; the Offer wizard (WhatsApp, everyone) shows the
cluster card disabled and quotes the server's own reason; the offering page with the front door
line; Add for a user with `@bob:elsewhere.net` shows the server's refusal in the field; with
alice the row appears and reaches Starting with the manager's reason and Runs elsewhere; the
Files dialog shows `registration.yaml` tagged with her ID and `config.yaml` with `id:
whatsapp-alice`, her `admin` permission and the server's bound address; Remove empties the table;
Stop offering returns to the list without WhatsApp. (`e2e-real/real-server.spec.ts`'s "user
detail" case fails against this server because it hard-codes `@ops:test.local`; unrelated, left.)

Ran, unit: `cargo test -p hs-bridges` is now 9 (six in `tests/manager.rs` drive the manager
over the in-memory store and `InMemoryAppserviceDirectory`: the target and the refused cluster
runtime, the registration's namespace growing and shrinking with offerings, an instance to
`ready` and its files, a bridge that never answers staying `starting` with the error, the shared
type and the delete conflict, the loopback fallback, and two offerings for one encoded
localpart); `cargo test -p hs-admin` 175; `npm run check` green (25 files, 180 tests);
`e2e/offer-bridge.spec.ts` and `bridges-list.spec.ts` (mocks) 4 passed.

Defects found and fixed, each with a test that fails without it:

1. The offerings and instances list operations were documented as `{data}` and the interface
   read `.data`; the router had always answered `{items, next_cursor, prev_cursor}`. Against the
   real server the Bridges page and every offering page were empty. Document, client and mocks
   moved to the page shape (`docs/decisions/0009-bridge-offering-contract-corrections.md`).
2. An offering on a server with no `server.public_baseurl` rendered `address:` empty into a
   bridge's `config.yaml`. The manager now gives the bound loopback address instead.
3. The manager noticed a bridge only through its own ping, every half minute for an instance run
   elsewhere, though a mautrix bridge pings itself through the server as it starts and an
   administrator can press Ping. The registry's health is read first, and an instance run
   elsewhere is pinged every tick for its first two minutes, then every half minute.
4. The front door repeated its refusal on every message from someone the offering is not open
   to. Once per room now (`hs_bridges.refusals`).
5. `m.direct` was written with `PUT`, replacing the account data event; it is read and merged
   now, and with double puppeting the owner's own `m.direct` is updated too (RFC 0017 4.1).
6. The catalogue said heisenbridge was `per_user`; it is `shared` (decision 0009), and a shared
   heisenbridge instance is rendered without `-o @OWNER:server`.
7. `BridgeType.not_deployable_reason`, `BridgeOffering.image_tag`,
   `BridgeInstance.last_ping_at`/`last_error` added, so the interface stops inventing a reason
   and parsing the tag (track 16's contract notes of 2026-09-26, resolved on the server side; the
   `409` count and the scopes are left, see the decision).
8. The manager logs every instance transition (`bridge instance moved`, `removing bridge
   instance`, `answered this server: ready`) at `info`, and what its bots are told at `debug`;
   `crates/hs-cli/tests/bridge_offerings.rs` forwards the server's log when `RUST_LOG` is set.

Observed and **not** fixed (not this track's crates):

- `/sync` repeats an event across two consecutive incremental batches when it arrived while the
  earlier batch was being assembled: seen for the manager bots' notices (sent through the
  appservice API with `?user_id=`), confirmed by hand against the real binary (a notice's
  `event_id` came back in the batch after the one that carried it). A real client dedupes by
  event id, and so does the test's `Watch`; a `since` token that does not cover everything in
  the response is track 05's to look at.
- The overview's `users_count` is cached for a minute (`STATISTICS_TTL`), so it cannot witness
  an account made in the last minute; the tests read the users list.
- The shared `target/` builds a workspace crate from whichever worktree touched it last, so a
  crate changed in another worktree (today: `hs-cluster`'s `MeshDeps.peers`) can break the
  build here; `touch crates/<crate>/src/lib.rs` rebuilds it from this tree. Likewise
  `target/debug/hs` is whichever tree linked it last: the Playwright flow first ran against
  another tree's binary (no loopback fallback) until `cargo build -p hs-cli --bin hs` here.

Written, not run: the `cluster` runtime (`deploying`, the operator, a `Bridge` on a kind
cluster) is unchanged and still unexercised; `access.users` on an edit does not stop or remove
instances people already have (as the RFC says); an instance's registration is delivered every
event its owner sends anywhere (the non-exclusive `@alice` namespace, Synapse's rule), which is
what double puppeting needs and what a bridge expects, but was not measured for many instances.

Decisions made today: the six in `docs/decisions/0009-bridge-offering-contract-corrections.md`;
an instance's bot account is not deactivated when the instance is removed (the server keeps its
accounts; the registration's removal is what stops anyone acting as it); the
`bridge_offerings.rs` test helpers stand in for a bridge with an axum listener registered by
patching the instance's registration `url`, which is what an administrator running a bridge on
a machine the server could not have guessed does.

Shared dependencies added: none. Environment: `pip install heisenbridge` works here (1.15.4);
Playwright 1.63 wants `chromium_headless_shell-1243` while `/opt/pw-browsers` holds 1194, so
`chromium_headless_shell-1243/chrome-headless-shell-linux64/chrome-headless-shell` and
`chromium-1243/chrome-linux64/chrome` were symlinked to the 1194 binaries (never `playwright
install`).

Next: the demo's shared WhatsApp registration replaced by an offering and a phone signed in
through `@whatsappbot`; a `Bridge` on a kind cluster and a `cluster` offering through
`deploying`; the scale measurement (hundreds of instance registrations, RFC 0017 section 6).

**The bridge manager exists** (2026-09-27, before the run above) (RFC 0017 section 4.1 to 4.3,
`crates/hs-bridges`, built 2026-09-26 by the operator and offerings session, wired into
`hs serve` in `52649d2`). It is the admin API's data source for `bridge_offerings.*` and
`bridge_instances.*` and the state machine behind them:

- `store.rs`: offerings, instances and the manager's own row (its appservice tokens and its
  bots' rooms) in `hs-kv`; an instance's state is persisted at every step, so a restart or
  another replica continues rather than restarts.
- `manager.rs`: `BridgeManager` implements `hs_admin::bridge_offerings::BridgeOfferingSource`.
  `put` on an offering registers its front door (the manager's own registration is re-synced
  with every enabled offering's bot in its exclusive namespace) and, for a `shared` type,
  creates its one instance. An instance walks `requested → registered` (appservice id
  allocated, `config.yaml` and registration rendered by `hs_admin::bridge_types` with the
  instance's own tokens, registered through the appservice directory, tagged
  `io.myelin.bridge_instance`) `→ deploying` (the runtime is asked to run it) `→ starting`
  (the deployment is Ready) `→ ready` (the registry's ping succeeded), or `failed` with the
  reason; fifteen minutes for `deploying`, ten for `starting`; `removing` deletes the
  deployment, the registration and the row. At `ready` the instance's bot creates a direct
  chat with its owner, sends the catalogue's sign-in steps, and the front door says so where
  the person asked. A tick every three seconds, or when woken by a write.
- `front_door.rs`: the manager's appservice API on the client listener under
  `/_myelin/bridges/_matrix/app/v1/{transactions,ping,users}`, authenticated by its own
  `hs_token`; what `@whatsappbot` answers on an invite or a message (sets one up, says where
  theirs is, says an administrator has been told for an `elsewhere` offering, refuses politely
  once) and what `@bridges` answers (`help`, `list`, `start`, `stop ... confirm`, `status`).
- `matrix.rs`: the client API over loopback, as the bots and as an instance's bot.
- `runtime.rs`: the `Runtime` trait (`target`, `apply`, `status`, `delete`, `service_url`) and
  `manifest_yaml`, the Secret plus `Bridge` for running an instance on another cluster. The
  Kubernetes implementation is `crates/hs-cli/src/bridges.rs` over
  `hs_operator::deploy::KubeBridgeClient`, built only when the chart's
  `MYELIN_BRIDGES_NAMESPACE` and `MYELIN_BRIDGES_HOMESERVER_URL` are both set; one without the
  other fails startup. Without them `bridge_deployments.target` says `available: false` and
  offerings can only run `elsewhere`.
- In `hs serve`: the manager ticks only on the replica that owns the global shard, is
  aborted at shutdown before the drain, and its router is merged into the client listener's.
  Its registration (and the `@bridges` namespace) is made at every start; its bot accounts are
  made with the first offering, not before -- CI caught the `bridges` account showing up in the
  overview's user count on a server that offered nothing (2026-09-27).

Verified: `cargo test -p hs-bridges` is **3 tests** (the manifest is a Secret and a `Bridge`;
an instance row is inserted once and updated in place; backticks become code and HTML is
escaped), `cargo test -p hs-admin` 175 with the ten operations' handlers over the in-memory
source, and the server starts with it all wired in. **Not verified, and the whole point of
the crate:** the state machine has never taken an instance from `requested` to `ready`, the
front door has never received a transaction from the real server, no `Bridge` has been applied
to an API server, and the RFC's scale note (section 6: hundreds of registrations, event routing
not scanning every namespace linearly) has not been measured. The next thing is heisenbridge as
a `shared` offering against the real binary on a kind cluster, then WhatsApp per user on the
demo, replacing 2026-09-25's shared registration (`docs/next-steps.md` item 1).

**A mautrix bridge works, added through the interface.** mautrix-whatsapp, against the real
binary, from the registration and `config.yaml` the interface's wizard rendered, with nothing
edited by hand: it reached the server, pinged itself through it (MSC2659), created its bot
device without a login (MSC4190), queried keys with device masquerading (MSC3202), and
started in appservice-mode encryption, all within seconds. The server attributed the bot to
the bridge and reported it healthy. What it has not done yet is carry a message: signing in
needs a phone. Reproduction and the exact list: `docs/bridges/mautrix.md`. It found one
defect here: `PingService::record` kept the previous failure in `last_error` after a ping
that worked, so a bridge whose first ping raced its own listener (mautrix does this; it
retries in five seconds) was reported healthy with an error. Fixed, with a test.

The bridge catalogue (`hs_admin::bridge_types`, shared with 15 and 16) now stamps every
registration it renders with `io.myelin.bridge_type`, which this crate's registry keeps in
`extra` and `admin_directory` reads back as `AppService.bridge_type`.

**A real bridge works.** heisenbridge, against the real binary and a local IRC server: it
registers its bot, drives the server with masqueraded requests, receives every event in its
rooms as transactions, relays IRC to Matrix through ghost users and Matrix to IRC. Reproduction
and the exact list of what was and was not verified: `docs/bridges/heisenbridge.md`. Two things
had to be fixed for it to get past its first request -- `/register` had no
`m.login.application_service` branch, and `hs serve` delivered no events to any appservice
(there was a scheduler and nothing that fed it; `src/pump.rs` and `src/delivery.rs` now do) --
and two more were found on the way: a server with a registration file in its configuration
could not start a second time, and after any restart nothing said in a pre-existing room reached
`/sync`, push or a bridge. All four are fixed, each with a test that fails without it. The admin API's thirteen
`appservices.*` operations are served (`src/admin_directory.rs`), and the interface's Bridges
section was watched pausing and resuming that bridge.

## Done

`crates/hs-appservice` (library; all `cargo test -p hs-appservice` green, 74 tests; `cargo clippy
-p hs-appservice --all-targets -- -D warnings` clean; `cargo fmt -p hs-appservice` applied):

- **Registration file parser** (`src/registration.rs`, `src/namespace.rs`, `src/regexp.rs`):
  every field `PLAN.md` Appendix B lists — `id`, `url` (nullable, distinct from missing),
  `as_token`, `hs_token`, `sender_localpart`, `rate_limited`, `namespaces.{users,aliases,rooms}`
  with `regex`/`exclusive`, `protocols`, `receive_ephemeral`,
  `de.sorunome.msc2409.push_ephemeral` (tracked as **two independent booleans**, matching
  Synapse's own loader exactly — see "Decisions made"), `org.matrix.msc3202`,
  `io.element.msc4190`. Unrecognized top-level keys (e.g. `com.beeper.*`) are preserved in
  `Registration::extra` and round-tripped through `to_yaml()` export. Namespace regexes use Python
  `re.match` (prefix) semantics via a `regex`-crate-first, `fancy_regex`-fallback matcher
  (`src/regexp.rs`) for lookaround/backreferences `regex` cannot express. Tests use inline
  fixtures shaped like real `mautrix-whatsapp` and legacy `mautrix-python` registration files, plus
  a double-puppet (`url: null`) fixture.
- **Store-backed registry** (`src/store.rs`, `src/registry.rs`) over `hs-tables`/`hs-kv`: add,
  list, update (RFC 7396 JSON Merge Patch), pause/resume, remove, rotate-tokens, import (idempotent
  upsert by id, for hot-reloadable static registration files), namespace conflict detection
  (sender collision, exclusive-namespace-vs-sender, identical-exclusive-pattern-twice, plus an
  `ExternalIdentityChecker` seam for track 04's future user table — see "Interfaces needed"),
  health (`healthy`/`degraded`/`down`/`paused`/`unknown` computed from consecutive failures vs.
  `hs-config`'s `tracking_failure_threshold`), and backlog listing. `as_token`/`hs_token` are
  uniquely indexed via `hs-tables`' declarative `IndexDef`.
- **Transaction scheduler** (`src/transaction.rs`, `src/scheduler.rs`): ordered per-appservice
  delivery (strict prefix, never skips a backed-off entry), batching (merges consecutive ready
  entries into one HTTP transaction via a generic JSON deep-merge), retry with exponential+jittered
  backoff, dead-letter after a configurable attempt budget, replay (by transaction id or since a
  timestamp), and full persistence in `hs-tables` (no in-memory queue at all — a restart is
  provably lossless because there was never a volatile copy; see
  `scheduler::tests::restart_resumes_pending_delivery`). Transaction bodies carry both stable and
  legacy key spellings exactly as Appendix B lists, gated per-registration-flag the way Synapse's
  `push_bulk` gates them (`refs/synapse/synapse/appservice/api.py`, read for behavior only) —
  see "Decisions made" for the one place this goes further than Synapse's current behavior. `url:
  null` registrations are enforced as never-pushed-to at the `enqueue` boundary itself (nothing is
  ever written to their queue), not by a drain-time check.
- **`hs-bridge-conformance`** (`crates/hs-bridge-conformance`; 4 scenarios, all green): a real
  HTTP fake bridge (`src/fake_bridge.rs`, bound to a loopback TCP port with `axum::serve`, not an
  in-process shortcut) plus a harness (`src/harness.rs`) wiring this crate's real `Registry`,
  `Scheduler`, `PingService` and `hs-auth`'s real `AuthState`/`Requester` machinery together.
  Scenarios (`tests/conformance.rs`): transaction contents and every documented key spelling over a
  real HTTP PUT; a null-url registration provably receiving nothing even when a reachable bridge
  exists; ping round-tripping in both directions (outbound call, and the inbound
  `/_matrix/client/v1/appservice/{id}/ping` route triggering it); identity assertion and device
  masquerading driven through `hs-auth`'s actual `Requester` extractor. See "Known gaps" below for
  what Appendix B calls for that this suite does not yet exercise, and why.
- **Ping, both directions** (`src/ping.rs`, `src/routes.rs`): outbound `POST
  {url}/_matrix/app/v1/ping` with health bookkeeping; inbound `POST
  /_matrix/client/v1/appservice/{appserviceId}/ping` as a real axum route (mounted on
  `State<hs_auth::state::AuthState>`, `PingService` passed via `Extension` — see `src/routes.rs`'s
  module docs for why), enforcing the spec's "only the named appservice's own `as_token`" rule and
  mapping `M_URL_NOT_SET`/`M_CONNECTION_TIMEOUT`/`M_BAD_STATUS`/`M_CONNECTION_FAILED` to their
  spec'd HTTP statuses (400/504/502/502), read from
  `refs/matrix-spec/data/api/client-server/appservice_ping.yaml`.
- **User/room-alias query protocol and third-party lookups** (`src/query.rs`): outbound
  `GET /users/{userId}`, `GET /rooms/{roomAlias}`, `GET /thirdparty/protocol/{protocol}`,
  `GET /thirdparty/location/{protocol}`, `GET /thirdparty/location`,
  `GET /thirdparty/user/{protocol}`, `GET /thirdparty/user` — all six calls `ruma_appservice_api`
  documents, reusing its `thirdparty::{Location, Protocol, User}` response types.
- **`AppserviceRegistry` implementation** (`src/auth_registry.rs`): replaces `hs-auth`'s stub
  (`crates/hs-auth/src/appservice.rs`) with the real registry, including the documented
  `fancy_regex`-vs-`regex::Regex` divergence (a namespace pattern needing lookaround/backreferences
  is dropped from `hs-auth`'s fast masquerade check with a `tracing::warn!`, never silently
  mis-evaluated — see that module's docs).
- `docs/rfcs/0009-appservice-identity-capability-flags.md`: the interface `hs-auth` needs to add
  (two `bool` fields) before rate-limit exemption and MSC4190 can be enforced end to end — see
  "Interfaces needed".

## In progress / Known gaps

Since 2026-09-22 the first four items below are superseded by `docs/bridges/heisenbridge.md`'s
list: ~~the pump delivers events only~~ (closed 2026-09-30, the session at the top); the pump runs on the global shard's owner and each appservice's worker on its shard's owner, tested with a scripted ownership and not yet on a real cluster; and no mautrix-* bridge with
an external service has been tried, only heisenbridge.

Everything below is a real, specific gap, not a vague TODO — each is blocked on a concrete thing
this track does not own, listed so the next session (or another track) can pick it up precisely:

1. **MSC4190 device flow is not enforced.** `crates/hs-auth/src/routes/devices.rs`'s `put_device`
   404s on an unknown device unconditionally, and `delete_device` always requires UIA. Both need an
   `is_some_and(|a| a.msc4190_enabled)` branch, which needs the field RFC 0009 proposes. I did not
   implement this myself because it requires editing `hs-auth`'s owned routes; RFC 0009 is the
   handoff. `hs-bridge-conformance` does not yet have an MSC4190 scenario for the same reason —
   once RFC 0009 lands, add one that `PUT /devices/{id}` an unknown device as a masquerading
   appservice with `msc4190: true` and asserts creation instead of 404.
2. **Rate-limit exemption (`rate_limited: false`) is stored and round-tripped by the registry but
   not enforced.** Same root cause as (1): `AppserviceIdentity`/`AppserviceRecord` have no
   `rate_limited` field for a rate limiter to consult yet (RFC 0009). Whichever crate ends up
   owning the live rate limiter (`hs-http::ratelimit` or `hs-auth::ratelimit` — both exist today as
   separate modules; I did not referee which one is canonical, that is track 07/12's call) needs to
   check it.
3. **`ts` timestamp massaging is not exercised.** It is a room-send-time concern
   (`PUT /rooms/{room}/send/...?ts=...`, honored only for appservice-authenticated requests) that
   belongs in the room actor's send handler, which is track 04's (`hs-room`) and does not exist
   yet. The contract is simple once that handler exists: if `requester.appservice.is_some()` and a
   `ts` query parameter is present, use it as `origin_server_ts` instead of wall-clock time. No RFC
   filed since there is no code yet to interface with; flagging here so track 04 sees it when
   `hs-room` lands.
4. **MSC3983/MSC3984 key proxies are not implemented.** These need `hs-e2e` (track 08)'s key
   upload/claim storage to proxy through; `hs-e2e` is not far enough along yet for a concrete
   interface to design against. Left for a follow-up session once track 08 has landed key storage.
5. **`com.devture.shared_secret_auth` native login provider (item 6, "if budget remains") was not
   started this session** — budget went to the higher-priority items 1 through 5 in the assignment.
   It needs `hs-auth`'s login-method registry (whatever shape that takes; `hs-auth/src/routes/
   login.rs` exists but I have not surveyed its extensibility) and does not depend on anything this
   session built, so it is a clean pickup for next time.
6. **The `Bridge` custom resource, console pages, real-bridge CI, and Compose files for
   `ergo`/`mautrix-irc` and Zulip/`mautrix-zulip`** (day-one work per the brief) were not started —
   they depend on `hs-operator` (track 12) and `web/` (track 15) existing enough to integrate
   against, and on the scheduler/registry landing first, which is what this session prioritized per
   the explicit delivery order given.
7. **Running `hs-bridge-conformance` against Synapse 1.161 as a control** (the brief's day-one
   work, and this suite's own module docs) needs a running Synapse behind Docker, which is
   unavailable in this environment. The suite as written exercises this crate's own sender-side
   components (`Scheduler`, `PingService`, `Requester` wiring) against its own real HTTP fake
   bridge; it does not yet have a mode that points at an external base URL (real Synapse or a real
   `hs serve`) because no such running server exists to point it at yet either (`hs-cli` has not
   assembled a full binary). Next step once both exist: add a `SYNAPSE_BASE_URL`/`HS_BASE_URL`-gated
   variant of each scenario that drives the real client-server API to generate the traffic instead
   of calling this crate's components directly.
8. **Direct media federation (`.well-known`, key server, signed requests, multipart)**,
   **async media**, and **encrypted-bridging soak** are federation/media/e2ee dependent
   (tracks 06, 09, 08) and not started.

## Next

In priority order for a follow-up session: (1) apply RFC 0009 (or get track 07 to) and wire
MSC4190 + rate-limit exemption end to end with conformance scenarios; (2) survey `hs-auth`'s login
route extensibility and add `com.devture.shared_secret_auth`; (3) once `hs-cli` assembles a real
server binary, add the external-base-URL conformance mode and run it against Synapse in Docker as
the brief's day-one work calls for; (4) `Bridge` CRD and console pages once tracks 12/15 have
something to integrate against.

## Blockers

None blocking today's work; items above are scoped to specific other tracks landing further, not
to anything broken.

## Interfaces provided

- 2026-10-04: `hs_appservice::query::QueryService::{user_exists, room_alias_exists, protocols,
  thirdparty_lookup, set_metrics}`; `hs_appservice::client_routes::client_router` (mount under
  `/_matrix/client/v3` and `r0`); `hs_appservice::pump::{LocalUsers, Pump::with_user_queries}`;
  `Registry::{interested, exclusive_owner, set_network_room, network_room_ids}`,
  `registry::instance_id`; `RegistryAppserviceAdapter::with_queries`, which answers the four new
  `hs_auth::appservice::AppserviceRegistry` methods (decision 0030).
- `hs_appservice::provisioning::BridgeLogins` (2026-10-01): asks a mautrix bridge's
  provisioning API who has signed in, with a 30 s cache and the
  `hs_admin_bridge_login_queries_total` counter; `RegistryAppserviceDirectory::
  with_bridge_logins`. The pure half, and the registration key `io.myelin.provisioning_secret`,
  are `hs_admin::bridge_logins` and `hs_admin::bridge_types::PROVISIONING_SECRET_KEY`.
- `hs_appservice::registry::Registry<B: hs_kv::KvBackend>`: the registration/health/backlog API —
  intended consumer: track 15's admin API and console (shapes chosen to match
  `crates/hs-admin/openapi/openapi.yaml`'s `AppService`/`AppServiceHealth`/
  `AppServiceBacklogEntry`/`AppServiceReplayRequest` schemas field-for-field), and track 12's
  `AppService`/`Bridge` operator status once it exists.
- `hs_appservice::scheduler::Scheduler<B>` and `hs_appservice::ping::PingService<B>`: what a room
  actor (track 04) and the admin API (track 15) call to enqueue events and trigger pings,
  respectively. `Scheduler::enqueue` takes a generic `transaction::Transaction` of `serde_json::
  Value` events rather than a `hs-room`/`hs-model` event type, deliberately, since those crates'
  event types do not exist in a form this crate could depend on yet — track 04 will need to
  serialize to the same shape (`events: Vec<Value>` of canonical JSON) when it lands, not adopt a
  new type.
- `hs_appservice::auth_registry::RegistryAppserviceAdapter<B>`: implements `hs_auth::appservice::
  AppserviceRegistry`, wired into `hs_auth::state::AuthState::appservices` by whichever crate
  assembles the real server (not yet assembled — `hs-cli` is a placeholder today). This replaces
  track 07's `InMemoryAppserviceRegistry` stub 1:1; no other change to `hs-auth` is needed to adopt
  it.
- `hs_appservice::routes::ping_router::<B>`: an axum `Router<hs_auth::state::AuthState>` fragment
  for `/_matrix/client/v1/appservice/{appserviceId}/ping` — mount it under
  `/_matrix/client/v1/appservice` on a router whose `State` is `AuthState`.

## Interfaces needed

- **`docs/rfcs/0009-appservice-identity-capability-flags.md`** (filed this session): `hs_auth::
  appservice::AppserviceRecord` and `hs_auth::requester::AppserviceIdentity` need `rate_limited:
  bool` and `msc4190_enabled: bool` fields, populated by `RegistryAppserviceAdapter` (already ready
  on this crate's side) and consulted by the rate limiter and `hs-auth`'s device routes
  respectively. Needs track 07 (or a follow-up from this track, with track 07's sign-off since it
  touches their crate).
- **A room actor to enqueue against** (track 04, `hs-room`): does not exist yet. `Scheduler::
  enqueue`'s shape (see above) is the contract track 04 should target.
- **`hs-e2e` key storage** (track 08): needed for MSC3983/MSC3984 key proxies.
- **`hs-user`'s user table** (track 04 or wherever it lands): `registry::ExternalIdentityChecker`
  is the seam this crate already defined for it — implement the trait and pass it to `Registry::
  with_identity_checker` once a real user table exists, to extend namespace conflict detection to
  cover human accounts (today it only catches registry-vs-registry conflicts, honestly, and the
  trait's default `NoExternalUsers` says so in its doc comment).
- **A server binary to mount everything in** (track 12's `hs-cli` today is close to a placeholder):
  needed to run `hs-bridge-conformance` against a real `hs serve` or against Synapse.

## Decisions made

- **2026-10-09, decision 0037: offerings pin a release tag.** No catalogue image says `latest`;
  `image_tag` blank or `latest` resolves to the pin; bumping a pin is a release step verified by
  the real-bridge story and recorded in `docs/bridges/mautrix.md`.

- **2026-10-04: decision 0030.** The appservice seam into registration, aliases and
  `/publicRooms` is `hs-auth`'s `AppserviceRegistry` trait (default methods, no new crate edge);
  a ghost acts once registered; protocol metadata is kept five minutes.
- **The owner's chat with their bridge's bot is started as the owner, not by the bot**
  (2026-10-02). mautrix marks a room as a person's management room only when the person invites
  its bot; a chat the bot starts never takes bare commands, so the invitation the manager used to
  send produced a chat where typing did nothing. This uses double puppeting (the non-exclusive
  claim on the owner the registration already carries for the mautrix types); without it the
  chat is still the bot's and its first line tells the person to start one. Encryption stays on
  by default: it was proven end to end with the real bridge in the same session.
- **`receive_ephemeral` and `de.sorunome.msc2409.push_ephemeral` are tracked as two independent
  booleans**, not folded into one "wants ephemeral" flag, after reading Synapse's actual loader
  (`refs/synapse/synapse/config/appservice.py`: `supports_ephemeral = as_info.get
  ("receive_ephemeral", False)` and `supports_unstable_ephemeral = as_info.get
  ("de.sorunome.msc2409.push_ephemeral", False)`, read separately) and its sender
  (`refs/synapse/synapse/appservice/api.py`'s `push_bulk`, which emits the stable `ephemeral` key
  only if the former is true and the legacy `de.sorunome.msc2409.ephemeral` key only if the latter
  is true, independently). A registration that sets only the legacy key must receive only the
  legacy key, or Appendix B's "test over real mautrix registration files" bar is not actually met —
  this was not obvious from Appendix B's prose alone (which reads like one flag) and only became
  clear from the Synapse source; documented at length in `registration.rs`'s field docs and in
  `transaction.rs`'s `to_wire_json` docs.
- **This crate's transaction sender goes one step further than Synapse's current behavior**: it
  sends both `to_device` and `de.sorunome.msc2409.to_device` (Synapse today sends only the legacy
  spelling, per a comment in its own source noting MSC4203 has not completed FCP merge), and both
  the bare stable and `org.matrix.msc3202.`-prefixed spellings of `device_lists`/one-time-key
  counts/fallback key types (Synapse sends only the prefixed spellings). This follows `PLAN.md`
  Appendix B's explicit instruction to send "both stable and legacy key spellings exactly as
  Appendix B lists" over what I could verify of Synapse's current behavior from source, since
  Appendix B is itself derived from `mautrix-go`'s parser (which already accepts both) and sending
  an extra, ignored key costs nothing. Flagged here so a differential-testing session against a
  real Synapse notices this is a deliberate, not accidental, difference.
- **A `url: null` registration is never even enqueued**, not enqueued-then-suppressed-at-drain.
  Chosen because Synapse's own interest routing (`is_interested_in_user`, checked regardless of
  exclusivity) would otherwise make a double-puppet registration's typically-broad non-exclusive
  namespace "interested" in nearly every event on the server, growing an unbounded backlog for
  something that will provably never be drained. Enforced in `Scheduler::enqueue`, documented
  there.
- **Namespace conflict detection is the decidable subset, not full regex-intersection.** Checks:
  duplicate `sender_localpart`; a new registration's exclusive namespace matching an existing
  appservice's literal sender (and vice versa); and byte-identical exclusive patterns registered
  twice. Regex-vs-regex overlap in general is undecidable and, as far as I could establish reading
  `refs/synapse/synapse/appservice/__init__.py`, Synapse does not attempt it either — this is not a
  shortcut relative to the reference implementation.
- **Appservice tokens are generated as 64 lowercase-hex characters** (32 random bytes via `rand`
  + `hex`), matching the shape `mautrix`'s own registration generators and `openssl rand -hex 32`
  (the common manual-setup instruction in bridge READMEs) both produce — deliberately not
  `hs-auth`'s `syt_`/`syr_`/`syl_` user-token shapes, since appservice tokens are a different
  credential space with different real-world tooling expectations.
- **The transaction scheduler's per-appservice concurrency model is "call `Scheduler::drain`
  again"**, not a built-in background task per appservice. `PLAN.md`'s open question ("batching
  windows and per-appservice concurrency") is answered at the batching level (a config'd
  `max_batch`) but deliberately left open at the "who calls `drain` and how often" level, since that
  decision depends on `PLAN.md` section 5.2's cluster ownership model (room-owner-driven wake-ups
  once `hs-cluster`/`hs-room` exist) which is not this track's to design.

## Shared dependencies added

- `matrix-sdk-crypto = { version = "0.19", default-features = false }` to
  `[workspace.dependencies]` (2026-10-03): `CollectStrategy`, which `matrix-sdk` takes in
  `with_room_key_recipient_strategy` but does not re-export, for `hs-bridge-conformance`'s
  real-bridge test client that excludes insecure devices. Already in the lock through
  `matrix-sdk`; no new crate compiled.
- `fancy-regex = "0.19"` to `[workspace.dependencies]` (`Cargo.toml`), for namespace patterns using
  lookaround/backreferences the `regex` crate cannot express (see `src/regexp.rs`). Only this
  track's own crates currently depend on it.
- `reqwest` and `ulid` were already present in `[workspace.dependencies]` (added by track 15) and
  reused as-is for this track's outbound HTTP clients (transaction delivery, ping, queries) rather
  than hand-rolling a `hyper`/`hyper-rustls` client — noted here since it is a meaningful dependency
  choice even though the `Cargo.toml` entry itself predates this session.
- No other new `[workspace.dependencies]` entries. Per-crate dev-dependency additions only
  (`tower` in both this track's crates' `[dev-dependencies]`, for `tower::ServiceExt::oneshot` in
  axum router tests — already a workspace dependency, just not previously listed for these two
  crates).

## `/versions` `unstable_features` flags track 12 needs, per Appendix B

Every flag a mautrix bridge probes on `/versions` (`bridgev2`'s minimum accepted spec version is
v1.4; flags below are checked regardless of declared spec version, so advertise `true` the moment
the underlying feature actually works, not only once the corresponding spec version is declared):

| Flag | What it gates | Owner of the underlying feature | Advertise once |
|---|---|---|---|
| `fi.mau.msc2246.stable` | Async media uploads | track 09 (`hs-media`) | async upload endpoints (`POST /media/v1/create`, `PUT /media/v3/upload/{server}/{id}`) exist |
| `fi.mau.msc2659.stable` | Appservice ping | **this track** | `hs_appservice::routes::ping_router` is mounted on the real server (component done; mounting is not) |
| `org.matrix.msc3916.stable` | Authenticated media | track 09 | authenticated `/_matrix/client/v1/media/*` endpoints exist |
| `uk.half-shot.msc2666.query_mutual_rooms` and `.stable` | Mutual rooms | track 04/05 (whichever owns `/v1/mutual_rooms`) | that endpoint exists |
| `org.matrix.msc4194` | User redaction | likely track 03/04 (room actor) | `rooms/{room}/redact/user/{user}` exists |
| `fi.mau.msc2815` | View redacted content | track 04 | implemented |
| `uk.timedout.msc4323` and `.stable` | Account moderation | track 04/13 (admin) | `admin/{action}/{target}` exists |
| `uk.tcpip.msc4133` and `.stable` | Extended profiles | track 04 (`hs-user`) or 07 | `profile/{id}/{key}` exists |
| `org.matrix.msc4143` and `.stable` | MatrixRTC | not this track's; primarily Element Call's, but `bridgev2` probes it too | RTC transports endpoint exists |
| `com.beeper.msc4169`, `com.beeper.msc4437`/`.stable`, `com.beeper.msc4446`, `com.beeper.hungry`, `batch_sending`, `room_yeeting`, `room_create_autojoin_invites`, `arbitrary_profile_meta`, `account_data_mute`, `inbox_state`, `arbitrary_member_change` | Beeper-only extensions | — | **never** — `PLAN.md` section 8.1 point 6 and Appendix B both say these are intentionally not implemented, matching Synapse's own non-advertisement |

`GET /_matrix/client/versions` itself does not exist yet (track 12 is adding it per this track's
brief); this table is the appservice-relevant subset of its `unstable_features` map to drive from
config, per the assignment's request.
