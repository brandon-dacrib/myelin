# Status: track 11, appservices and bridges

Last updated: 2026-10-01 (who has signed in to a bridge; the `cluster` runtime run on kind;
both below); before that 2026-09-30 (ephemeral, to-device and device-list delivery); before that
2026-09-27 (RFC 0017 run against the real binary), 2026-09-27 (the bridge manager) and
2026-09-25.

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
