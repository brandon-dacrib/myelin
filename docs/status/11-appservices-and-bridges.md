# Status: track 11, appservices and bridges

Last updated: 2026-10-19 (a bridge is no longer sent its own double puppet's receipt back
when it moved nothing: the Slack read/unread flip; below); before that 2026-10-18 (the ephemeral pump leaves a room it cannot read out of the pass for
a minute instead of failing every tick; below); before that 2026-10-15 (Discord's delivery timeouts: a retry is the same transaction, a
transient failure is retried for a day, transactions carry at most 100 items and shrink when a
bridge times out, a replay gets a fresh budget, dead letters can be seen and discarded, and the
health says why; decision 0049; below); before that 2026-10-14, later (an appservice removes its users' devices without
Last updated: 2026-10-15 (a deployed bridge's volume is 5 GiB, can be raised from its page,
only grows, and a full one says so; mautrix bridges log to standard output only; below); before
that 2026-10-14, later (an appservice removes its users' devices without
user-interactive auth, MSC4190 or not, so the manager can remove the iMessage bot's left-behind
device whose key counts the bridge drops; below); before that 2026-10-14 (the ephemeral pump's
stream prunes, and the sync feed and hot-room
stream compactions, delete in bounded batches; a failed prune no longer fails the tick; below);
before that 2026-10-13, afternoon (a drain starts where the waiting entries start, the backlog
list and replays read a page at a time, and queue work runs off the async workers; the drain is
not what freezes the demo; below); before that 2026-10-13 (a bridge that is off no longer takes the server down: the scheduler and the
queue gauges read small head rows, not every queued body; below); before that 2026-10-12, latest (the iMessage bridge double puppets again, through its
double-puppeting registration; below); before that 2026-10-12, late night (double puppeting is a registration of its own with no url,
so a bridge is no longer sent every room its owner is in; decision 0046; mautrix-imessage stops
double puppeting; below); before that 2026-10-12, night (the iMessage bridge starts on the
owner's Mac: `GET /login` lists the appservice login, the config's double puppeting says
`appservice`, and `run.sh` fetches a missing `libolm`; below); before that 2026-10-12, evening
(a device signed by a replaced self-signing key can be signed
Last updated: 2026-10-12, late night (`./run.sh install` makes a Mac bridge a LaunchAgent that
starts at login; below); before that 2026-10-12, night (the iMessage bridge starts on the owner's Mac: `GET /login`
lists the appservice login, the config's double puppeting says `appservice`, and `run.sh` fetches
a missing `libolm`; below); before that 2026-10-12, evening (a device signed by a replaced self-signing key can be signed
by the new one, so the re-created WhatsApp bot's devices are signed; below); before that 2026-10-12, later (a bridge device made without keys is told it has none, so
mautrix-discord uploads its keys and can decrypt; below); before that 2026-10-12 (the manager publishes the bot's self-signing key again when the server
does not hold it, and a refused signature backs off; below); before that 2026-10-11, later (iMessage is offered: a program for the person's Mac, with its
own config layout, a run script and an address the administrator sets; below); before that
2026-10-11 (mautrix-discord is on mautrix-go's older framework and now gets that
framework's `config.yaml` layout; below); before that 2026-10-10 (the manager removes a bot device
the bridge left behind after a crypto reset, decision 0041; the OTK-count warning on the demo is
Synapse's behaviour; below); before
that 2026-10-09, later (the manager says, in its log, which bridge bot device it
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

## 2026-10-19 (branch `agent/receipt-no-regress`): the Slack read/unread flip was a receipt loop through the pump

With the Slack bridge running and the official Slack client open elsewhere, a channel
flipped between read and unread in the official client. The loop: Slack reports a read
(`channel_marked`) -> mautrix sets the owner's `m.read` as their double puppet
(`POST /read_markers`) -> the hub wrote it even when the receipt was already there, and the
write became a receipt stream entry -> the pump sent the bridge the owner's receipt (the
`fi.mau.double_puppet_source` key mautrix marks its own with is not kept by any receipt body
here or on Synapse, so the bridge cannot tell its own receipt from the owner's) -> the bridge
called `conversations.mark` at the receipt's event -> Slack reported the mark -> again. When
Slack's newest message had no Matrix event (unbridged subtype, a gap), the bridge marked the
last bridged message before it, every turn moved Slack's marker back over the newer one, and
the official client flipped. Synapse ignores a receipt that does not move forward, so the
loop never starts there. Now the hub does too: `SessionHub::set_threaded_receipt` and the
receipt EDU path drop a receipt at the same or an earlier event before anything is written
(`hs_user_receipts_ignored_total{reason}`, a `debug` line). Detail and the test in
`docs/status/05-sync.md`, the 2026-10-19 entry. The pump itself is unchanged: it sends a
bridge every receipt in its rooms, its own users' and the double puppets' included, as
Synapse's `get_new_events_as` does. **Left:** verifying on the demo after the roll that
`hs_user_receipts_ignored_total{reason="same_event"}` climbs with the Slack bridge up and the
official client open, and that the channel stays read.

## 2026-10-18 (branch `agent/stuck-room-load`): a room the pump cannot read is left out of the pass, not the whole pass

**Asked**: on the demo, from 23:20 UTC on 2026-10-17 to the roll of this change, every tick of
the ephemeral pump failed with `could not read a stream ... cannot read the members of
!BHvcNjOBRwncDQbGOk:codestorm.net: state error: unknown event EventSn#135009` (every ten
seconds at the backoff's ceiling), so no bridge was sent a receipt, a presence change, a
to-device message or a device-list change for seventeen hours. The room could not be loaded
(track 04's 2026-10-18 entry has the cause); the pass asked for its members because a user
whose presence changed is in it, and the member read's error failed the tick.

**Done** (`crates/hs-appservice/src/ephemeral.rs`, the module docs' "A room that cannot be
read"): a room whose facts (`EphemeralSource::room_facts`) cannot be read is treated for that
pass as a room this server does not have -- nothing in it reaches any appservice -- counted in
`hs_appservice_ephemeral_room_read_failures_total`, logged at `warn` with the room and the
error, and not asked for again for `ROOM_READ_RETRY_AFTER` (60 s;
`EphemeralPump::with_room_read_retry_after` for tests). A store or stream error still fails the
tick as before. Each retry of a room whose load fails is that load again (ten seconds of reads
on the demo), so the pause matters as much as the delivery.

**Verified**: `a_room_that_cannot_be_read_is_left_out_of_the_pass_and_asked_again_later`: a
receipt in a readable room is delivered while one in the unreadable room reaches nobody, the
failure is counted once, the room is not asked for again within the pause and is asked again
past it, and its receipts flow once it reads. On the demo: see `docs/next-steps.md`.

## Session 2026-10-15 (branch `agent/discord-delivery-timeouts`): Discord's delivery timeouts were a replay of the 2026-10-12 flood

**Seen on the demo** (v0.3.22, federation turned on at 19:58 UTC): `could not deliver to this
appservice; it will be tried again ... count: 20, dead_lettered: 20, ... /transactions/3524:
operation timed out` for `discord-brandon`, one every 31 s from 19:56:37 (3524, 3564, 3584, 3604,
3624, ... 3804); `hs_appservice_transactions_total{appservice="discord-brandon",outcome="failed"}`
9 and no `delivered` since boot; mautrix-discord's "Event channel is full" 307 times in hour 19
and 3,787 in 20:00-20:10, mostly `m.room.encrypted` and `m.receipt`, and `Failed to send
decryption error notice ... /rooms/!RZcNK4rQkNAMMv9jsL.../join: M_FORBIDDEN`; Discord and Slack
"Degraded" on the Registrations page with no reason.

**What happened** (Loki for both logs, Prometheus for the queue gauges; read only):

1. *A replay, not federation.* Between the 19:43 and 19:58 scrapes, on the same pod,
   `hs_appservice_queue_dead_lettered{appservice="discord-brandon"}` went 260 -> 60 and
   `hs_appservice_queue_depth` 0 -> 200, with `hs_appservice_queue_oldest_age_seconds` 260,626:
   the dead letters were put back at about 19:56 (the page's "Replay all"; the replay was audited
   but not logged). They were queued 2026-10-12 19:34, the flood decision 0046 describes (the
   registration then claimed the owner, so the bridge was sent every room he was in): encrypted
   events of rooms the bridge does not bridge (hence the `403` joins for its decryption notices),
   the owner's to-device messages ("Dropping to-device event targeted to someone else",
   `@brandon`) and his one-time-key counts. Nothing new was queued for Discord after the restart
   (depth 0 since 20:08): today's registration claims only its ghosts and bot, so federation
   coming back sent it nothing.
2. *One attempt each.* A replay kept the spent attempt count, so each batch was sent once and
   dead-lettered on its first failure (`dead_lettered: 20` on every line).
3. *Huge transactions.* Twenty queue entries to a transaction, each a room's page of up to 100
   events: the bridge's "Starting handling of transaction" lines say `pdu` 624, 1060, 1026, 913,
   781, 781, 1076, 291, 264, 84, 0, 4, 2. mautrix-go answers only when every event is in its
   channel, and spends 3 + 22 s per event it cannot decrypt: 3524 took 91 s to dispatch.
4. *Timed out but delivered.* The 30 s timeout fired every time; the bridge went on dispatching
   after the server hung up (14 "Finished dispatching" by 20:25, the warnings down to 42 in
   20:20-20:30). Everything the server counted as failed reached the bridge, and the 260 dead
   letters were back.
5. *Ids.* 20 apart because each was a new batch of twenty entries. The one retry (3624, after the
   19:58 restart cut its first attempt) reused its id, and the bridge handled it twice because the
   first was still in flight. Latent bug found reading the code: a retry re-formed its batch, so a
   failed batch of fewer than twenty that grew before its retry went under a new id with its old
   events in it.
6. *Slack* was "Degraded" for a different reason: its pod restarted around 19:03 and transaction
   147686 was refused (`Connect`) ten times in four minutes and dead-lettered (`slack-brandon` 3).
7. *Dead letters never expire, and nothing showed what they were.* The red "N failed" (discord
   260, signal 260, linkedin 280, meta 280, whatsapp 300: the 2026-10-12 flood; imessage 3,545;
   slack 3) stays until a replay, and the only action offered was to send them all again.

**Fixed** (decision 0049):

- `scheduler.rs`: a failed batch's entries record its id (`QueuedTransaction::txn_id`) and the
  retry sends exactly them under it. `TransactionSender::send` returns a `SendError` with a
  `SendErrorKind` (`TimedOut`, `Unreachable`, `Unavailable` for 5xx/408/429, `Rejected`).
  Transient failures are retried, backing off to 5 min, until failing for a day
  (`give_up_after_ms`, from `first_failed_at_ms`); a timeout waits at least 60 s
  (`timeout_backoff_ms`); `Rejected` is dead-lettered after 10 attempts as before. The entries of
  one transaction share one attempt count and are dead-lettered together. A new transaction
  carries at most `max_items` (100) items, the first entry always; the per-appservice limit halves
  on a timeout and doubles back on a delivery. `HttpTransactionSender::new` waits 60 s
  (`TRANSACTION_TIMEOUT`). `Scheduler::discard` deletes matching dead letters; `replay` resets the
  budget. Both are logged.
- `store.rs`: `discard_dead_lettered` (by heads, re-checking status in the transaction; the queue
  front counts are recounted); `requeue_dead_lettered` resets attempts, first failure and id.
  `HealthRow::last_failure_at_ms`.
- `registry.rs`: `Health::reason` (in words: how many failed in a row and the `down` threshold,
  the last error; a failed ping; paused; unknown). A ping answered after the last failure makes
  the bridge healthy again. `BacklogEntry` says when it was queued, its events, ephemeral and
  to-device counts and up to five rooms.
- `delivery.rs`: the failure line carries `txn_id`, `attempt`, `reason`, `elapsed_ms` and
  `retry_in_ms`, and says separately when a bridge is too slow ("did not answer a transaction in
  time and may still be working on it") and when entries are dead-lettered ("gave up ...; its
  entries are dead-lettered and wait for an operator to replay or discard them").
- Metrics: `hs_appservice_transaction_seconds{appservice,outcome}` (delivered, failed,
  timed_out), `hs_appservice_transaction_failures_total{appservice,reason}`,
  `hs_appservice_transaction_retries_total`, `hs_appservice_dead_lettered_total`,
  `hs_appservice_batch_item_limit`; the `appservice.transaction` span has `attempt`, `retry` and
  `reason`.
- Admin API (OpenAPI 0.1.18): `POST /api/v1/appservices/{id}/discard` (`bridges:write`,
  `{transaction_ids?, before?}`, `200 {"discarded": n}`, audited `appservice.discarded`;
  `AppserviceDirectory::discard`, default `Unavailable`; `hs-bridges`' wrapper delegates);
  `AppServiceBacklogEntry.{enqueued_at, events, ephemeral, to_device, room_ids}`;
  `AppService.health_reason`, `AppServiceHealth.reason`. `hs-admin-mock` serves discard.
- Web (track 16's pages, by this branch; `docs/status/16-management-web-interface.md`): the
  Registrations tab shows `health_reason` under a non-healthy badge and "N dead-lettered" with
  "Review on the bridge's page" instead of "N failed"; the bridge page shows the health reason, and
  its Transactions tab (`web/src/pages/bridges/BridgeTransactions.tsx`) explains dead letters,
  lists each entry's age, contents and rooms, confirms "Replay all" with the count and the oldest
  entry's age (and that the bridge receives events that old now), and offers "Discard all dead
  letters" and a per-entry Discard behind a confirmation. Mocks serve discard and the new fields;
  `web/e2e/bridge-dead-letters.spec.ts` drives it.

**Verified.** `cargo test -p hs-appservice` (139, among them
`a_retry_sends_the_same_entries_under_the_same_txn_id_whatever_arrived_since`,
`a_transient_failure_is_retried_until_it_has_failed_for_a_day`,
`a_rejected_transaction_is_dead_lettered_after_max_attempts`,
`a_timeout_waits_before_its_retry_and_shrinks_new_transactions_until_a_delivery`,
`a_transaction_carries_at_most_max_items_and_a_larger_entry_goes_alone`,
`a_replayed_entry_gets_a_fresh_retry_budget`,
`discard_deletes_the_matching_dead_letters_and_nothing_that_waits`,
`health_says_why_and_a_ping_answered_after_the_failures_ends_them`,
`a_backlog_entry_says_what_it_carries_and_where`), `-p hs-admin` (with
`discarding_deletes_dead_letters_only_and_is_audited`), `-p hs-bridges`,
`-p hs-bridge-conformance`. Real binary: `cargo test -p hs-cli --test appservice_retries`: a
bridge answering `503` while a message is sent is retried under one id with the same body until it
answers, the backlog page shows the entry's event and room, the row says `degraded` with a reason
naming the `503`, then `healthy` with none; the failure, reason, retry and latency series and the
log line are checked. Also `appservice_isolation`, `appservice_ephemeral`, `appservice_queries`,
`bridge_offerings`, `bridge_logins`. Chart: `deploy/helm/hs/ci/alerts-test.sh` (the
`HsAppserviceDeadLettered` alert watched `hs_appservice_queue_dead_lettered_total`, which was
never exported, so it could not fire; it now watches `hs_appservice_dead_lettered_total`, with a
promtool case). Web: `npm run check` (704 unit tests) and `npm run test:e2e` (75).

**Left.** No automatic expiry of dead letters (decision 0049 says why). The timeout, the day and
the item cap are constants, not settings. A bridge still handles an in-flight duplicate when a
retry arrives before it has finished the first attempt (mautrix-go dedupes only finished ids); the
60 s floor after a timeout makes that rare.
## Session 2026-10-15 (branch `agent/bridge-volume-size`): a full volume says so, and a bridge's volume can be raised

**Seen.** The demo's Slack bridge stopped on 2026-10-14 when its 1 GiB volume filled (its init
container: `cp: write error: No space left on device`); the Registrations page said only
"degraded" for over a day. Its own log files filled it: mautrix's default `logging` also writes
`./logs/bridge.log` into `/data`, up to 1.1 GB. Decision 0050; the platform side (the operator's
never-shrink, the volume sweep, the chart) is in `docs/status/12-platform-and-kubernetes.md`.

**Done.**

- **Logs to standard output only** (`hs_admin::bridge_types`, `stdout_logging`, both mautrix-go
  layouts). Checked with the real bridge: `real_mautrix_login.rs`,
  `a_bridge_rolled_with_a_render_without_its_pickle_key_keeps_its_own`, now also asserts that the
  bridge keeps the rendered writers and makes no `/data/logs`.
- **A size per instance** (`InstanceRow::volume_size`; `hs_admin::bridge_volume`): default 5Gi;
  `PUT /bridge-offerings/{type}/instances/{user_id}` with `volume_size` (a whole Mi/Gi/Ti, 1Gi
  to 1Ti, never less than the volume has, the cluster's larger size included; `400` at
  `/volume_size` otherwise, or for an instance run elsewhere). The deployment's fingerprint
  carries it, so the next step applies it and the bridge restarts.
- **How full it is**: the manager reads each deployed instance's `Bridge` once a minute
  (`watch_volumes`): `hs_bridge_volume_used_ratio{bridge,bridge_type}` and
  `hs_bridge_volume_capacity_bytes` (`crates/hs-bridges/src/metrics.rs`), a `WARN` when one
  becomes nearly full (85%) or full (95%), an `INFO` when it has room again.
  `BridgeVolume` (OpenAPI 0.1.19) is on `BridgeInstance.volume` and, through the
  offering-aware directory, on `AppService.volume`, so the Registrations page can say "Volume
  full". A deployment whose last words say "No space left on device" or "database or disk is
  full" is a full volume whatever was measured.
- **`explain_deployment`** turns those words into "the bridge's volume is full: the step that
  copies its configuration in (or the bridge) could not write to it ... give it a larger volume
  size on its page", on the instance's reason while deploying and on its page once ready.
- **UI** (track 16, on this branch): the Registrations, offering and bridge pages show the
  volume and its state; the bridge's page sets its size, explained inline.

**Verified.** `cargo test -p hs-bridges` (new: `a_bridge_stopped_by_a_full_volume_says_so_...`,
`an_instance_is_deployed_with_a_five_gib_volume_unless_one_is_set`,
`a_volume_raised_by_hand_is_kept_and_not_shrunk_from_the_admin_api`,
`a_nearly_full_volume_is_counted_and_on_the_registration`, `a_volume_change_is_logged_once_...`,
`metrics::tests`); `cargo test -p hs-admin` (`bridge_volume::tests`, `volume_tests`, the logging
assertions, the OpenAPI contract tests).

**Left.** An offering-wide default size (today the default is the server's, 5Gi, and a size is
per instance). In cluster mode only the replica running the bridge manager (the global shard's
owner) has `AppService.volume` and the gauges; another replica answers the list without it (the
instance endpoints read the cluster on any replica), and a replica that handed the shard on keeps
its last gauge values (the alerts take the `max`). Removing a bridge's old `/data/logs` is by hand
(`docs/bridges/mautrix.md`, "2026-10-15: a full volume").

## Session 2026-10-14, later (branch `agent/otk-counts-device`): the iMessage bot's left-behind device can be removed

**Seen.** The demo's mautrix-imessage bridge (legacy mautrix-go, on the owner's Mac) logged
`WRN Dropping OTK counts targeted to someone else` about 1,800 times since 2026-10-13 14:08 EDT:
1,729 for `@brandon` / `J6CEqTqXcJ` on 2026-10-13 14:08-14:24 (the owner's counts, which the
registration's non-exclusive owner namespace makes Synapse send too; see 2026-10-10), and, since
the bridge started again on 2026-10-14 14:24 EDT, two per transaction for
`@imessagebot_brandon` / `yLLbqe7oX4`.

**The facts, read-only.**

- The bridge's crypto device is `h7ijM5DMbF` (`crypto_account` in `imessage-brandon.db`; "Found
  existing device ID for bot in database" at start).
- `yLLbqe7oX4` was made by the bridge's own `/login` (`m.login.application_service`) at
  2026-10-13 14:08:42 EDT; the bridge stopped at 14:09:43 before saving its crypto account, and
  the next start logged in again and got `h7ijM5DMbF`. The bridge log has no `keys/upload` for
  `yLLbqe7oX4`: it never had keys.
- The server counts every device of the bot, keys or not, as Synapse does
  (`count_bulk_e2e_one_time_keys_for_as` is `devices LEFT JOIN e2e_one_time_keys_json`), so each
  transaction carries `yLLbqe7oX4` (0 keys, dropped by the bridge) beside `h7ijM5DMbF`. The
  bridge's real device is told its count: on 2026-10-13 14:24:21 it was told 0, uploaded 50 and
  read back 50; since then nothing to top up. So encryption to the bridge was not broken by this;
  the warning is noise from a stale device.
- Why 2026-10-13 14:08-14:24 carried only `@brandon`'s counts and none of the bot's: those were
  the backlog (server transaction ids 4181 to 38741, drained in 16 minutes), and a transaction's
  MSC3202 counts are computed when it is queued, not when it is sent. Queued before the bot had
  today's devices, they counted none of them; the first fresh one (38761) counted both users.
  Synapse computes them at send time. Not changed here (it is the scheduler's queue, which another
  branch is in); a follow-up below.
- The manager already removes a bot's left-behind device (2026-10-10, decision 0041), and tried:
  Loki, 2026-10-14T18:10:41Z (a day after the device was last seen), `could not settle the bot's
  cross-signing identity ... could not remove the bot's old device yLLbqe7oX4: delete device:
  401`. `DELETE /devices/{id}` skipped user-interactive auth only for an appservice with
  `io.element.msc4190`; the WhatsApp and Signal bridges set it, the legacy iMessage bridge does
  not, so its stale device stayed and the instance's identity step kept failing (backing off).

**Fix** (`hs-auth`, `crates/hs-auth/src/routes/devices.rs`; the coordinator's assignment, one
function): `skips_uia_to_delete_devices` is true for any appservice requester, MSC4190 or not,
for `DELETE /devices/{id}` and `POST /delete_devices`, as Synapse decides it
(`requester.app_service_id` in `DeviceRestServlet` and `DeleteDevicesRestServlet`; read, not
copied). Creating a device with `PUT /devices/{id}` stays MSC4190's. An appservice could already
log in as any of its users, so this grants nothing it could not do; a double-puppeting
registration's owner is in that set, as in Synapse.

**Key counts unchanged.** Counting a device without keys stays (it is what makes an MSC4190
bridge upload its first keys, 2026-10-12, and it is Synapse's behaviour); a deleted device's keys
are removed by `hs-e2e`'s notifier, so a removed device stops being counted.

**Tests.** `hs-auth`: `an_appservice_without_msc4190_removes_its_users_device_without_uia`,
`an_appservice_without_msc4190_bulk_removes_devices_without_uia`,
`an_appservice_without_msc4190_still_cannot_create_a_device`,
`a_user_still_needs_uia_to_bulk_remove_devices`. Real binary (`hs-cli --test
appservice_ephemeral`):
`a_legacy_bridge_bots_left_behind_device_is_removed_and_no_longer_counted`: a registration
without MSC4190, the bot logged in twice, keys for the second device only; the transaction counts
both (0 and 2); `DELETE /devices/{first}?user_id=<bot>` with the appservice token succeeds; the
bot's `/devices` lists one; the next transaction counts only the device in use.

**Operator.** None needed after a roll: the manager's next look at the instance removes
`yLLbqe7oX4` (logged at `INFO`, "removed a device the bridge bot no longer uses", and listed in
the instance's `removed_bot_devices`), the instance's identity reason clears, and the warning
stops. To remove it before a roll: admin UI, Users, `@imessagebot_brandon:myelin.dacrib.net`,
Devices, `yLLbqe7oX4`, Sign out (or `POST /api/v1/users/@imessagebot_brandon:myelin.dacrib.net/devices/bulk-delete`
with `{"device_ids": ["yLLbqe7oX4"]}`). Never `h7ijM5DMbF`.

**Next.** Compute a transaction's MSC3202 counts when it is sent, not when it is queued (Synapse's
`_compute_msc3202_otk_counts_and_fallback_keys` runs in the sender), so a bridge draining a
backlog is told its devices' counts as they are, not as they were hours before.
## 2026-10-14 (track 06's branch `agent/federation-boot-freeze`): the queue gauges read from the floor

`AppserviceStore::queue_summary` -- what `hs_appservice_queue_*` and the admin API's
`AppService.queue` read, at every scrape -- walked each queue's heads from the first entry,
the delivered entries' tombstones included: 1 to 5.7 s per scrape on the demo, whose iMessage
queue had ~31,000 delivered behind its floor (`hs_appservice queue gauges` in the
`TimedCollector` warning). It now reads from `queue_floor`, as `pending_batch` does: nothing
below the floor waits, so below it only the dead-lettered entries are counted, once, and
remembered per queue (`QueueFront`), each later summary counting only those the floor has passed
since. A replay or a purge (`requeue_dead_lettered`, `purge_queue`) has the fronts counted
again, and every remembered front is recounted from the start after ten minutes. Test:
`a_queue_summary_reads_from_the_floor_and_remembers_the_front` (counts unchanged; the second
summary reads no head below the floor).

## Session 2026-10-14 (branch `agent/ephemeral-prune`): the ephemeral pump's prune no longer spins on a refused transaction

**Why.** 0.3.10 on the demo (embedded Fjall, federation off, no scraping) was killed by its
liveness probe four times overnight (2026-10-14 04:46-04:52 and 10:46 UTC). Loki had about 3,300
of `appservice ephemeral delivery could not read a stream` every 15 minutes since 03:00 UTC (and
20:15-20:45 the evening before), each `transaction too large: 10001 mutations exceeds the
10000-mutation limit`. `EphemeralPump::tick` prunes the receipt, presence and to-device streams
after queueing, and each store prune (`hs-user`'s `prune_stream`, `hs-e2e`'s
`prune_to_device_stream`) deleted everything below the floor in one transaction. Once more than
10,000 entries sat below the floor the store refused it every time, the error failed the tick,
and the next tick (a doorbell or the 250 ms timer) read the same 10,000-key range and was refused
again: a permanent busy loop of synchronous KV work on async workers (0.4-1 core) and an ERROR
flood, a likely contributor to the liveness kills.

**What changed.**

- The stores prune in bounded batches: at most 1,000 entries a transaction, at most 10,000 a
  call, each batch starting after the last key the one before deleted
  (`hs_user::store::{STREAM_PRUNE_BATCH, STREAM_PRUNE_MAX_PER_CALL}`,
  `hs_e2e::store::{TO_DEVICE_PRUNE_BATCH, TO_DEVICE_PRUNE_MAX_PER_CALL}`). The count is still
  returned.
- `EphemeralSource::prune_receipts_below`, `prune_presence_below` and
  `DeviceSource::prune_to_device_below` return how many they deleted (`usize`, was `()`).
- A failed prune does not fail the tick: what was queued stays queued, the failure is counted
  and logged at WARN at most once a minute (with how many went unlogged), and the next tick
  prunes again. A stream whose last prune found nothing below the same bound is not asked again
  until the bound moves (entries are only appended above the head), so an idle pump no longer
  range-scans three streams four times a second. The bound is now the lowest position as just
  stored, not as read at the start of the tick, so a stream is pruned one tick sooner.
- `hs-cli` runs the three prunes on the blocking pool (`off_the_runtime`, the idiom of
  78ef1f19), and a failing tick backs off (250 ms doubling to 10 s, `ephemeral_backoff`) instead
  of retrying on every doorbell.
- New counters: `hs_appservice_ephemeral_pruned_total{stream}` and
  `hs_appservice_ephemeral_prune_failures_total{stream}` (`stream` is `receipts`, `presence` or
  `to_device`).
- The same bug class on the sync side, fixed the same way in `hs-user` (track 05's crate, at the
  coordinator's request): `compact_feed` (run after every fan-out to a feed past twice its
  retention, retried on every fan-out once refused) and `compact_hot_stream` (every 200,000
  hot-room updates at the default retention, whose first run would delete ~100,000 rows in one
  transaction) deleted up to 50,000 rows in one transaction. Both now scan and delete at most
  1,000 rows a transaction (`COMPACTION_BATCH`), within the old 50,000-row bound per call. The
  feed is walked newest first so the first transaction moves the floor (no kept entry is
  coalesced into while the rest go); the hot-room stream decides each row against the next and
  moves its floor when the scan reaches the end, as before.

**Verified.** `cargo test -p hs-user -p hs-e2e -p hs-appservice` and `cargo test -p hs-cli`
green; clippy clean on the four crates. New tests: 12,345 receipt, presence and to-device entries
(more than one transaction may delete) are pruned across calls, at most 10,000 a call, nothing
at or above the bound touched (`a_backlog_over_the_transaction_limit_is_pruned_in_batches`,
`a_to_device_backlog_over_the_transaction_limit_is_pruned_in_batches`, on the in-memory backend,
which enforces the same limit as Fjall and PostgreSQL); a 25,000-entry feed and a 12,000-row
hot-room stream are compacted across transactions with each room's newest kept
(`a_feed_compaction_over_the_transaction_limit_is_done_in_batches`,
`a_hot_stream_compaction_over_the_transaction_limit_is_done_in_batches`); a prune the store
refuses does not fail the tick, nothing is queued twice, the failures are counted, the streams
are pruned once the store accepts, and an unchanged bound is not asked again
(`a_failing_prune_does_not_fail_the_tick_and_the_streams_are_pruned_later`). Not yet verified on
the demo: roll the release and check the ERROR is gone, `hs_appservice_ephemeral_pruned_total`
climbs once (the backlog going) and then follows traffic, and the idle CPU drops.

**Other unbounded deletes found (not fixed here; none runs per tick).** One transaction each, no
batch cap; each can exceed 10,000 mutations:

- `hs-federation` `outbound_store::forget_destination`: a full PDU queue (10,000 by default) plus
  EDUs and indexes. Reached hourly by `hs-cli`'s `DestinationSweep` and by the admin forget/prune;
  a refused one is retried, and refused, every hour.
- `hs-push` `counts::tables::delete_where` (`mark_read`, `reset`, `reset_room`): every unread row
  a receipt covers; per read receipt, so a user more than 10,000 notifying events behind in a
  room can never clear it.
- `hs-e2e` key backup `delete_version`, `delete_room_sessions`, `delete_all_sessions`: per client
  request; users with more than 10,000 megolm sessions are common.
- `hs-auth` `delete_all_access_tokens_for_user`, `delete_other_access_tokens_for_user`,
  `delete_all_refresh_tokens_for_user`: two mutations a token, so more than 5,000 tokens.
- `hs-user` `forget_room_members` and `reconcile_room_members` (room-update path, large rooms),
  and `index_room_members_if_absent` (writes, rooms over ~10,000 members).
- `hs-room` `admin_ops::delete_everything`: its last transaction (aliases and a `joined_rooms`
  row per member); admin room delete of a room over ~10,000 members.
- `hs-e2e` `replace_remote_device_list` / `forget_remote_user`: bounded by a remote server's
  device count for a user.
- `hs-appservice` `store::purge_network_rooms` (on removing an appservice; one-off; this track's,
  next).
- `hs-media` `scanning::cache::VerdictCache::put`: `overflow` deletes, normally one.
- `hs-federation` `enqueue` / `enqueue_durable_edu`: writes, 2-4 per destination, past ~2,500
  destinations.

## Session 2026-10-13, afternoon (branch `agent/backlog-heads`): bounded drains, a paged backlog, and the drain is not the freeze

**Why.** 0.3.10 on the demo was killed by its liveness probe every 3-6 minutes from 17:59 UTC,
when the iMessage bridge came back to a queue of ~34,700 (3,441 dead-lettered at the front). The
suspects in this crate were the backlog list (whole bodies on every 15 s poll; the demo's logs
show no backlog requests, so not it) and the drain: `pending_batch` scanned heads from the front
of the queue on every drain, past every dead-lettered head and every delivered entry's
tombstone, and deleted a delivered batch one transaction per entry, all on an async worker.

**What changed.**

- `AppserviceStore::pending_batch` starts at the queue's floor (`hs_appservice.queue_floor`,
  `(appservice id) -> seq`: nothing below it waits) and raises it to the first waiting entry it
  finds, compare-and-set, so a replay that lowers it meanwhile wins. Whatever makes an older
  entry wait again lowers it in its own transaction (`requeue_dead_lettered`,
  `put_queue_entries` with a pending entry). Floors are forgotten when the store opens (a hint
  only; a downgrade could have made entries wait without lowering one). Heads are read 512 per
  range (`QUEUE_HEADS_PER_READ`; the PostgreSQL backend answers a range with every row at once)
  and the batch's bodies in one `multi_get`.
- A delivered batch is deleted in one transaction (`delete_queue_entries`), a failed batch's
  attempts written in one (`put_queue_entries`). `Scheduler::drain` runs its queue reads and
  writes on tokio's blocking pool (`off_the_runtime`), and `Delivery`'s worker yields between
  back-to-back drains.
- `Scheduler::replay` is `AppserviceStore::requeue_dead_lettered`: heads 512 at a time, 64
  bodies per transaction, the request matched on the head (`ReplayRequest::matches_head`).
  `purge_queue` deletes by heads, 512 per transaction.
- The backlog list reads a page: `AppserviceStore::backlog_page` / `Registry::backlog_page`
  (heads in order, delivered skipped, bodies of the page alone), `Registry::backlog_len` counts
  heads. `hs-admin`'s `AppserviceDirectory::backlog` takes a `BacklogPageRequest` and returns an
  `AdminAppserviceBacklogPage` (`hs-bridges`' wrapper forwards it); the router's cursor is
  `t<transaction id>` (keyset, opaque in the OpenAPI as before; a cursor it did not hand out is a
  `400 invalid_cursor`), `prev_cursor` is now always null, `include_total` counts heads. The
  directory reads on the blocking pool. No OpenAPI change.
- `AppserviceStore::queue_reads` counts the heads and bodies the paged reads fetched (tests and
  diagnostics). `examples/seed_queue.rs` fills a stopped server's Fjall queue (total, how many
  dead-lettered at the front) for reproducing this.

**Verified** against the real release binary on Fjall (35,000 queued, 3,500 dead-lettered at the
front, a stand-in bridge started 10 s after the server, `/health/live` every 3 s with a 5 s
timeout, `/metrics` every 15 s):

| binary | drain of 1,575 transactions | slowest `/health/live` | slowest `/metrics` |
| --- | --- | --- | --- |
| origin/main (0.3.10 + 2) | 8.2 s at ~95% of one core | 7 ms | 16 ms |
| this branch | 2.8 s at ~60% of one core | 1.8 ms | 17 ms |

With the stand-in bridge also sending three messages back per transaction (4,725 sends while
draining, as a bridge catching up does), both binaries stayed under 10 ms per probe and Fjall
reported no sealed memtables. **So the drain does not reproduce the demo's freeze**, before or
after this change; whatever stalls the demo's runtime for 30-50 s is elsewhere (things to read
from its Prometheus before the scrapes timed out: `hs_kv_fjall_sealed_memtables` and the rate of
`hs_kv_fjall_write_buffer_rotations_total` (Fjall halts every writer, in a sleep loop on whatever
thread commits, at 4 sealed memtables or 30 L0 runs, and all `hs-kv` keyspaces share one tree);
the container's working set against its 3 GiB limit and its system CPU (reclaim); and the
constant ~3.4 cores, which predates the bridge's return). Tests:
`store::tests::a_drain_walks_past_the_dead_lettered_front_of_a_long_queue_once` (each drain after
the first reads at most 512 heads and exactly its 20 bodies; a replay and an overwrite below the
floor are found; a reopened store reads from the front),
`store::tests::the_backlog_is_read_a_page_at_a_time` (3,000 entries, dead-lettered, delivered and
deleted ones mixed: pages in order, cursors continue, each page reads its own bodies),
`store::tests::requeue_and_purge_walk_the_queue_by_heads`,
`hs-admin` `router::tests::the_backlog_is_paged_by_its_own_cursor`.

**Left.** The backlog endpoint was checked through `hs-admin`'s router and the store, not
against the running binary (it needs an admin token). The queue gauges and the admin API's
`AppService.queue` still count every head of a queue on each scrape and each bridges-list read
(small rows, ~12 ms for 35,000 here). A queue for a bridge that never comes back still grows
without bound. The freeze itself is open (above).

## Session 2026-10-13 (branch `agent/queue-heads`): a bridge that is off no longer takes the server down

**What happened on the demo.** The iMessage bridge on the owner's Mac has not answered since
2026-10-12, ~19:20 UTC. Its queue grew to ~36,000 transactions (Prometheus
`hs_appservice_queue_depth{appservice="imessage-brandon"}`), each a batch of events.
`Scheduler::drain` read the whole queue, bodies included, on every retry (`queue_for`, every
few seconds), and the queue gauges read every entry on every scrape (`queue_summary`), both on
async worker threads. The scrape went from 0.02 s to 0.4-3 s as the queue grew, and hit
Prometheus's 10 s timeout from 06:23 on 2026-10-13; from 06:34 the server stopped answering
`/health/live` for 12-20 s at a time while its resident memory swung by ~1 GB, and the kubelet
restarted it 92 times in 9 hours (0.3.8). The 0.3.9 roll failed on that pod.

**What changed.** Each queue entry has a head row beside it (`hs_appservice.txn_queue_head`,
`(appservice id, seq) -> {enqueued_at_ms, next_attempt_at_ms, status}`), written in the same
transaction as the entry by every write (`enqueue_in`, `put_queue_entry`,
`delete_queue_entry`). `AppserviceStore::pending_batch` reads heads in order up to the batch's
last waiting entry and fetches only those bodies; `drain` uses it. `queue_summary` (the gauges
and the admin API's `AppService.queue`) counts heads. A store from before gets its heads once,
when it opens, each written only if its entry is still there (`pump_meta` row `queue_heads`;
logged at INFO: `gave each queued appservice transaction its head row, once`).

**Verified** against the real binary on one data directory: 0.3.9's binary queued 4,901
transactions (10,000 8 KB messages) for an appservice at a closed port. The scrape took
0.28 s and the process 30-50% CPU while retrying. The new binary built 4,901 heads on start;
the scrape then took 0.013 s, the process 4-8% CPU, `/health/live` 1.5 ms. With a fresh queue of
645 entries for a receiver that was down and then started, all 1,500 messages arrived in 33
transactions with increasing ids, none dead-lettered. Tests:
`store::tests::pending_batch_reads_the_next_waiting_entries_from_their_heads` (300 entries,
dead-lettered and deleted ones skipped) and
`store::tests::opening_a_store_builds_heads_for_a_queue_written_without_them`.

**Left.** `Registry::backlog` (the admin API's backlog list) and `Scheduler::replay` still read
whole bodies; both run only when an operator asks (both read a page or a bounded batch since the
afternoon session, above). A queue for a bridge that never comes back
grows without bound (every entry is kept until delivered or replayed); a cap or an expiry for
dead-lettered entries is the next step. A downgrade to a release before this one, then an
upgrade again, leaves entries queued in between without heads (the marker is already set):
delete the `queue_heads` row in `hs_appservice.pump_meta` to have them built again.

## Session 2026-10-12, late night (branch `agent/double-puppet-registration`): double puppeting is a registration of its own, and the bridges are not flooded

**What was wrong** (diagnosed on the demo the same day). Five of six bridges (signal, whatsapp,
meta, linkedin, discord) took no transactions and were "Degraded"; their pods were fine. Each
instance's own registration claimed its owner non-exclusively (the wizard's server-wide render
claimed `@.*`) so that it could double puppet with its own token. Under the spec's interest rules
that made every room the owner is in the bridge's business: each was sent every event of every
such room, encrypted chats its bot is not in among them, and mautrix waited 25 s on keys for each
one, serially, until its transaction handler blocked and the server's 30 s timeout fired. The
server's log said only `error sending request for url (...)`. Decision 0046 has the numbers.

**Done.**

- `hs-admin::bridge_types`: an instance's registration (`render_instance`) and the wizard's
  (`render`) claim the bridge's ghosts and bot only. `double_puppet_registration` /
  `DoublePuppetRegistration` build the separate registration: `url: null`, one non-exclusive
  users rule, tokens of its own, `io.myelin.double_puppet_for` (`DOUBLE_PUPPET_FOR_KEY`), id
  `<appservice>-doublepuppet` (`double_puppet_id`). `InstanceSpec::double_puppet`
  (`DoublePuppetTokens`) gives an instance's tokens; `InstanceRender::double_puppets` says one is
  wanted and `double_puppet_registration` is it (claiming the owner alone). The config's secret
  is that registration's token as `as_token:<token>` in `double_puppet.secrets` (bridgev2) and
  `bridge.login_shared_secret_map` (mautrix-discord); without tokens, no double puppeting is
  written at all. **mautrix-imessage no longer double puppets**: its build knows only
  `login_shared_secret: appservice` (`b26fce40`, the same day), an appservice login with its own
  token, which needs its own registration to claim the owner, the very claim that floods; it
  takes `as_token:` as an HMAC secret. Its config has no `login_shared_secret`; the manager
  still starts its owner's chat as the owner with the double-puppeting registration. The cost:
  what the owner sends from their own Apple devices is not sent as them in Matrix. It comes back
  when that bridge takes an `as_token:` secret. `user_regex` is the exact-match pattern for a user.
  `BridgeTypeRenderResult.double_puppet_registration{,_yaml}` (OpenAPI 0.1.16) for the wizard,
  with `@.*:<server>`.
- `hs-bridges::manager`: `InstanceRow::double_puppet` (`DoublePuppetRow`: id, tokens,
  `registered_fingerprint`). `settle_double_puppet`, run on every step before
  `settle_registration`: mints and registers the registration once per change of what it
  claims (a free id: `-doublepuppet`, `-doublepuppet-2`, ..., reusing one left behind for the
  same instance), removes it when double puppeting is switched off, tells an instance run
  elsewhere to fetch its files. The owner's chat is started, and a repaired chat's re-invite
  sent, with that token (`owner_token`). Removing an instance removes both registrations.
- **Migration**: an existing instance's next step registers its double puppeting, patches its
  own registration in place (same id and tokens; the existing fingerprint mechanism) to stop
  claiming the owner, and rolls its pod once through the deployment fingerprint with the new
  token and every secret it had (pickle key, provisioning secret, tokens: decision 0036). One INFO
  line per instance: `moved the bridge instance's double puppeting to a registration of its own`
  (`appservice`, `double_puppet_appservice`, `owner`, `deploy_name`).
- `hs-appservice`: `error::error_chain` (the error and every cause, each once). The transaction
  sender, ping, user/alias queries and the provisioning call use it, so the delivery log and
  `last_error` say `error sending request for url (...): operation timed out` (or `connection
  refused`, `dns error: ...`). `HttpTransactionSender::with_timeout`. A ping of a url-less
  registration answers `M_URL_NOT_SET` and records nothing; `Registry::health` of a url-less,
  unpaused registration is `unknown` whatever was recorded.
- Web: a `-doublepuppet` registration with no url is "Built in" ("Double puppeting for Signal
  Brandon", "Built in: lets the bridge post as its owner; the server sends it nothing"); any other
  url-less registration is "Sends nothing"; neither is under Attention or counted in the health
  chips. The wizard previews and creates the double-puppeting registration after the bridge's own
  (its own idempotency key; a failure says the bridge is registered and its double puppeting is
  not). Mocks follow. **2026-10-13:** the Registrations tab folds a double-puppeting registration
  into its bridge's row ("… · Double puppeting on"; `foldDoublePuppets` in `web/src/api/bridges.ts`),
  so the list has one row per bridge and "All" counts bridges. One whose bridge is not on the
  loaded page stays a row of its own. The bridge's page says "Double puppeting: On" with a link to
  `<id>-doublepuppet` (or "Off"). That registration's own page is not a bridge's page: it says it
  is the bridge's key, links the bridge, lists whom it covers, and has no health, connection test,
  tabs or actions (its health and backlog are not fetched).
- **2026-10-13, the server folds** (branch `agent/fold-doublepuppet-server`, OpenAPI 0.1.17,
  decision 0046 amended): `hs_appservice::admin_directory` fills `AppService.double_puppet_for`
  from the row's `io.myelin.double_puppet_for` and `AppService.double_puppet` from the whole
  registry (the first registration by id that names the bridge), on `list`, `get`, `create`,
  `update`, `pause` and `resume`. `GET /appservices?fold_double_puppets=true` (hs-admin's router)
  leaves out each double-puppeting registration its bridge names, before paging; an orphan stays.
  The web's client-side `foldDoublePuppets` and its `GET /appservices/<id>-doublepuppet` probe
  are gone. Checked against `hs serve`: 25 bridges, 20 double-puppeting registrations and one
  orphan give page 1 = bridge00..bridge19 with `double_puppet` set where one exists, page 2 =
  bridge20..24, the orphan and `myelin-bridges`; total 27 folded, 47 not.
- Docs: decision 0046, `docs/bridges/mautrix.md` ("2026-10-12": how it works, what the roll does,
  the post-roll checks), CHANGELOG, `docs/next-steps.md`.

**Verified.**

- `cargo test -p hs-appservice`: `pump::tests::a_bridge_is_not_sent_a_room_none_of_its_users_is_in_whoever_double_puppets`
  (the old shape hears the owner's private chat, the new one and its url-less registration do
  not), `scheduler::tests::a_bridge_that_never_answers_is_reported_as_timed_out` (a real listener
  that accepts and never answers: the drain's error and `last_error` say `timed out`),
  `error::tests::*` (a refused connection says `refused`), `ping::tests::null_url_ping_is_a_distinct_failure`.
- `cargo test -p hs-admin --lib bridge_types`: the registrations and configs of all three layouts,
  and `an_instance_without_its_double_puppeting_tokens_writes_no_double_puppeting`.
- `cargo test -p hs-bridges`: `a_changed_double_puppeting_adds_or_removes_its_registration_once_and_rolls_it`,
  `an_instance_from_before_moves_its_double_puppeting_once_and_keeps_its_secrets` (the demo's
  case: same id and tokens, same pickle key and provisioning secret in the rolled config, one
  apply, one INFO line), `removing_an_instance_removes_its_double_puppeting_too`.
- Real binary (`cargo test -p hs-cli --test bridge_offerings`, six tests):
  `a_bridge_is_sent_nothing_of_a_room_none_of_its_users_is_in_and_double_puppets_apart`: alice's
  instance, a stand-in bridge; alice and bob exchange `m.room.encrypted` events in a room of
  their own, then alice writes in her chat with the bot; the bridge is sent her line and nothing
  of the private room; the `-doublepuppet` registration has no url, health `unknown` even after a
  ping, nothing queued; its token is `alice` on `whoami?user_id=` and refused for bob; the
  bridge's own token is refused as alice and accepted as its bot. The other five tests were
  updated for the new shape (no owner claim, the registration listed, removed with the instance,
  switched off with the option).
- Real mautrix: `cargo test -p hs-bridge-conformance --test real_mautrix_login` (Docker,
  `dock.mau.dev/mautrix/{whatsapp,signal}:v0.2609.0`, the real `hs` binary): 5 passed in 93 s.
  The bridges start with the new config, the manager starts the owner's chat as them with the
  double-puppeting token and repairs a bot-started chat with it (the `repaired` case no longer
  patches a claim on alice into the bridge's registration; it asserts there is none), and
  `login qr` gets a QR code in WhatsApp and Signal. The bridge's own use of the secret (after a
  network sign-in) needs a phone and is not covered.
- Web: `npm run check` and `npm run test:e2e` (73 flows) pass; `add-bridge.spec.ts` sees the
  double-puppeting preview on Review and the "Built in" row after creating.

**Next.** Roll the demo (the coordinator; the post-roll checks are in `docs/bridges/mautrix.md`
and `docs/next-steps.md`).

**Decisions made.** Decision 0046: one double-puppeting registration per instance (claiming its
owner alone), not one shared `@.*` registration; a url-less registration is never a fault.

**Interfaces provided.** `hs_admin::bridge_types::{double_puppet_registration,
DoublePuppetRegistration, DoublePuppetTokens, double_puppet_id, user_regex,
DOUBLE_PUPPET_FOR_KEY, DOUBLE_PUPPET_ID_SUFFIX}`, `InstanceSpec::double_puppet`,
`InstanceRender::{double_puppets, double_puppet_registration}`,
`BridgeTypeRenderResult::double_puppet_registration{,_yaml}` (OpenAPI 0.1.16),
`hs_appservice::error::error_chain`, `HttpTransactionSender::with_timeout`.

**Shared dependencies added.** None.
## Session 2026-10-12, latest (branch `agent/imessage-double-puppet`): the iMessage bridge double puppets again

Decision 0046 (`agent/double-puppet-registration`) stopped a bridge's own registration from
claiming its person, and so took iMessage's double puppeting out: this build knows only
`login_shared_secret: appservice`, an appservice login with its own token. The owner wants it
back. `hs-auth`'s appservice login now allows a user the appservice's own registration does not
claim when its double-puppeting registration does
(`AppserviceRegistry::double_puppet_record_for`, which `hs-appservice`'s adapter answers from
the row whose `io.myelin.double_puppet_for` names it). It logs one INFO line per such login.
The iMessage config says `appservice` again when the instance double puppets. Masquerading and
transaction delivery are unchanged, so the bridge is still not sent the person's every room.

**Verified** with the real `mautrix-imessage` build against a local `hs` from the branch, on
port 29399 with fresh tokens, the instance's registration without the person, and a
`-doublepuppet` registration claiming them: `Successfully automatically enabled custom puppet`
and the server's INFO line. Without the double-puppeting registration it is `403 Application
service cannot masquerade`. Unit test:
`routes::login::tests::an_appservice_logs_in_as_a_user_its_double_puppeting_registration_claims`
(allowed for the claimed user, refused for another, refused with no double-puppeting
registration). `bridge_offerings.rs` passes.

**CI on arm64** failed on four of the last five commits in
`bridge_offerings.rs::a_bridge_is_sent_nothing_of_a_room_none_of_its_users_is_in_and_double_puppets_apart`
(line 914, `chat_started_by` null), which kept `d1ddf6bd` from being released. The manager
records who started the chat after it has created it and the bot has joined, and the test read
the record once, as soon as the bridge saw the invitation. The test now waits for the record
(`agent/bridge-offerings-race`).

## Session 2026-10-12, late night (branch `agent/imessage-launchagent`): a Mac bridge starts at login

The owner wants the iMessage bridge to start without Terminal once it works. The generated
`run.sh` now takes `install` and `uninstall`. `install` writes a per-user LaunchAgent
(`dev.myelin.bridge.<appservice id>`, `RunAtLoad`, `KeepAlive`, 30 s throttle, stderr to
`logs/launchd-stderr.log`) and bootstraps it in `gui/<uid>`. `uninstall` boots it out and
removes the plist. A plain run refuses beside an installed agent. It is a LaunchAgent, not a
LaunchDaemon, because Messages and Contacts are only readable in the person's session, and
Full Disk Access goes to the program, not Terminal. `docs/bridges/imessage.md` ("Starting it at
login") has the details. **Verified** with launchd on this Mac, using a stand-in program and a
test label: running, refusal, restart after a kill, and clean uninstall. **Not yet** with the
real bridge, which needs the roll that carries `agent/as-login-flow`.

## Session 2026-10-12, night (branch `agent/as-login-flow`): the iMessage bridge starts on the owner's Mac

The owner ran `run.sh` on their Mac, which is also the machine this session runs on. It hit
three problems; `docs/bridges/imessage.md` ("What was verified") has the details:

1. `run.sh` fetched the build only when the program was missing, so a missing
   `libolm.3.dylib` crashed the program at launch. It now fetches when either is missing.
2. **`GET /login` did not list `m.login.application_service`.** The doc comment said Synapse
   omits it, but Synapse's `LoginRestServlet.on_GET` appends it. mautrix-go's older framework
   checks for it before logging its bot in for encryption and stops with `homeserver does not
   support appservice login`. `hs-auth` now lists it.
3. This build's double puppeting takes `login_shared_secret: appservice`, not `as_token:`. The
   catalogue's iMessage config now says `appservice`.

**Verified** with the real `mautrix-imessage` build against a local `hs` built from the branch:
`Logged in as bridge bot`, keys uploaded, `Successfully automatically enabled custom puppet`,
`Bridge started!`. The unit tests and `bridge_offerings.rs`'s iMessage case pass.

**An incident during the check.** The first run reused the instance's port (29331) and
`hs_token`. The demo's registration sends to this Mac's address, so the test instance received
31 of the demo's real transactions (41 events, 11 to-device, 19:59:07–19:59:47 UTC) until it
was stopped. It could not decrypt them, its only server was the local one, and it had no
Contacts access, so nothing was sent anywhere. But the real bridge will not get those events.
The second run used its own port and tokens.

**Next on the Mac**, after the roll that carries fix 2: set `login_shared_secret: appservice`
in the Mac's `config.yaml` (or download the files again), then `./run.sh`. **Superseded** by
`agent/double-puppet-registration` (above): after the roll that carries it, download the files
again; they have no `login_shared_secret`, and the instance's own registration no longer claims
the owner.

## Session 2026-10-12, evening (branch `agent/stale-signatures`): the re-created WhatsApp bot's devices can be signed: the refused key was the old identity's

**What was wrong.** The 2026-10-12 section below assumed the server had lost the manager's new
self-signing key (`1MIe…`). It had not. `1MIe…` was the **previous instance's** self-signing
key. The manager signs each bot device lacking its signature, starting from the object
`/keys/query` returns. That object for `BQBMQVR81T`, the device the first instance used and
cross-signed on 2026-10-10 at 22:44, still carried the old key's signature. The server's
`/keys/signatures/upload` checked **every** entry under the signer's name
(`verify_submitted_entries_by_signer`), found no key `1MIe…` after the new identity replaced it,
and refused the whole upload. The new keys had been stored (Loki: minted 17:00:40.492,
published .497, refused .499). The manager signs devices in ID order and stops at the first
failure, so `BQBMQVR81T` < `TRV2160MAI` meant the bridge's **current** device was never signed
either. A client that excludes insecure devices withholds keys from it. The 0.3.1 fix (republish
when the self-signing key differs, back off) does not cover this: after it, the server holds the
manager's keys, nothing is republished, and every signing still fails, only less often.

**The fix, both sides.**

- **Server (`hs-e2e`):** an entry already stored on the target, unchanged
  (`signatures.<signer>.<key id>` with the same value), is not a new claim and is not checked
  again (`already_stored`). This applies to devices and to cross-signing keys, including the
  user-signing case. Synapse checks only the new signing key's signature and ignores the rest. A
  new entry is still checked: one by an unknown key, or a stored key id with a different
  signature, is still `M_INVALID_SIGNATURE`.
- **Manager (`hs-bridges`):** `BotIdentity::sign_device` sends only the device's own
  signature and the new one, as clients do, so it works on a server with the strict check too.

**Verified.**
- `hs-e2e/tests/cross_signing.rs::signatures_upload_accepts_a_device_still_carrying_a_replaced_keys_signature`
  (in-process router): sign with key A, replace the identity, re-sign the queried object with B.
  The forged and tampered entries are refused.
- `hs-cli/tests/appservice_ephemeral.rs::a_bot_device_signed_by_a_replaced_identity_is_signed_by_the_new_one`
  (real `hs`, as the appservice, with the manager's `BotIdentity`): identity one signs the device,
  identities two and three replace it, and the device is signed with the stale entry left in and
  with the manager's body. Without the server fix it fails with the demo's exact error,
  `no known key ed25519:… for signer`.
- `cross_signing::tests::a_device_is_signed_by_the_self_signing_key_and_keeps_its_own_signature`
  checks that a replaced key's entry is not sent.

**After the roll.** The manager logs `cross-signed the bridge bot's device …` for `BQBMQVR81T`
and for `TRV2160MAI`, and the `could not settle the bot's cross-signing identity` WARN stops.
`BQBMQVR81T` is the stale device; the device cleanup (decision 0041) removes it once it is
older than its threshold.

## Session 2026-10-12, later (branch `agent/discord-otk-counts`): a bridge device made without keys is told it has none

**What the owner saw.** On the demo, the new `bridge-discord-brandon` answered `login qr` in its
management room with "⚠ Your message was not bridged: the bridge hasn't received the decryption
keys", every time.

**Root cause.** mautrix-discord v0.7.7 (mautrix-go's older framework) in appservice mode makes
its bot's device with MSC4190 (`PUT /devices/7YmXUAYXKj`, no login) and does not upload keys at
start. It uploads when a transaction's MSC3202 `device_one_time_keys_count` says its device is
low. The server built those counts from `list_device_keys`, the devices that already have keys,
so a device with none was never mentioned. The bridge never uploaded, `/keys/query` showed the bot
with no device keys, and Element shared the room key with every device but the bot's. Element
did nothing wrong, and nothing was withheld. The bridge's log showed it: no `keys/upload` among
its requests, and every discord-brandon transaction had `one_time_key_counts: 0` (no device),
while the `bridgev2` bridges, which upload at start, had their bot's. Synapse counts every device
of the user, at `signed_curve25519: 0` when it has none (`count_bulk_e2e_one_time_keys_for_as`).

**The fix.** `hs-cli`'s `KeyCountSource` lists the user's devices from the accounts
(`DeliveryDeps::accounts`, `hs-auth`'s `list_devices`), adds any device with keys the accounts do
not list, and always reports `signed_curve25519`, at zero when none are left. The
`KeyCountSource` and `DeviceKeyCounts` docs in `hs-appservice` say so.

**Verified.** `appservice_ephemeral.rs::a_bridge_device_made_without_keys_is_told_it_has_none`
runs the real `hs`: an `inhibit_login` registration, an MSC4190 device, a room and a message, and
the transaction says `signed_curve25519: 0` for the device. After the stand-in uploads two keys,
the next transaction says 2. Without the fix it fails, "never happened: the bridge was told its
new device has no keys", which is the demo's symptom. The unit test
`a_device_with_no_keys_yet_is_counted_at_zero` covers the source.

**Observable.** The scheduler's `delivered a transaction to an appservice` line counts the
devices with key counts (`one_time_key_counts`). After the roll, discord-brandon's should be at
least 1, and the bridge logs `Request completed … url=…/keys/upload`.

**Left.** The demo roll, and then `login qr` in the Discord management room. A management room
whose Megolm session was shared before the bot had keys stays undecryptable for those messages.
Element starts a new session for the bot once it sees the bot's device: if it does not, the
owner can run `/discardsession` in that room, or ask for a new management room.

## Session 2026-10-12 (branch `agent/bot-identity-selfsigning`): the self-signing key is checked like the master, and a refused signature backs off

**What the demo showed** (0.3.0, read through the owner's proxy and Loki on 2026-10-11): at
17:00:04Z the owner removed and re-created their WhatsApp instance; the new row minted new
seeds, `published the bot's cross-signing keys` (the server answered 200), and from 17:00:40Z
every signing of device `BQBMQVR81T` was refused, `M_INVALID_SIGNATURE: no known key
ed25519:1MIe… for signer @whatsappbot_brandon`, once per tick, 1,200 WARN lines an hour. The
master was published (the manager would otherwise have published again and logged it), so the
server held the manager's master and not its self-signing key. (**Wrong, see 2026-10-12,
evening, above:** `1MIe…` was the old identity's key, still signed onto the device.) Why the upload left the server
without it is **not yet known** (`post_device_signing_upload` stores every key it is given; the
owner's `/keys/query` of the bot settles which side; `docs/next-steps.md`).

**What changed, regardless of which side.**

- `BotIdentity::is_published_self_signing` mirrors `is_published_master`;
  `settle_bot_identity` looks at `self_signing_keys.<bot>` as it looks at `master_keys.<bot>`
  and publishes both again when either is not this identity's. The `published the bot's
  cross-signing keys` line now carries both key ids and which of the two was already there.
- A failed settle is kept per instance (`identity_failures`: count, when to try again, the
  error) and tried again after [`step_backoff`] (3 s, 3 s, 6 s, … 5 min), not every tick;
  logged at WARN once per distinct error and at DEBUG (`the bot's cross-signing identity still
  cannot be settled`, with `failures` and `retry_in_s`) while it repeats. The row's reason
  stays what it was. A success clears it. In memory: a restart tries once more straight away.
- The fake Matrix server in the manager's tests can refuse signatures the way the demo did.

**Tests.** `manager::tests::a_self_signing_key_the_server_does_not_hold_is_published_again`
(master held, another self-signing key held: both published again, the device signed, the
row clean) and `a_refused_signature_is_retried_with_backoff` (one upload per due look, nothing
asked within the wait, the reason kept, the count doubling the wait, a success clearing it).

**Left.** Which side lost the self-signing key on 2026-10-11 (the owner's `/keys/query` of the
bot says; then an hs-e2e or hs-bridges fix with a real-binary test). On the demo, once this
rolls, the manager publishes both keys again on its next look and signs the device; the log
says `published the bot's cross-signing keys … self_signing_was_published=false`.

## Session 2026-10-11, later (branch `agent/imessage-offering`): iMessage is offered, as a program on the person's Mac

**Asked for by the owner**: offer mautrix-imessage, which runs on a Mac outside the cluster; the
interface gives a download link for the program, asks for the Mac's hostname or IP address,
and shows how to run it. `docs/bridges/imessage.md` has the whole of it; in short:

- **Catalogue** (`bridge_types.rs`): `mautrix-imessage`, port 29331, bot `imessagebot`, ghosts
  `imessage_`, `!im`; `Entry::mac_download` pins one CI build (mau.dev job 95972, commit
  `300ba6d0`, 2026-05-14; no release tags exist), surfaced as `BridgeType.runs_on: "mac"`,
  `download_url`, `download_note`; `deployable: false` with a reason, so its offering can only
  run `elsewhere`. `ConfigLayout::IMessage` (the Discord session's `legacy_config` became
  `ConfigLayout {BridgeV2, Legacy, IMessage}`) writes the older framework's layout with
  iMessage's sections and `websocket_proxy: null`, so the bridge listens and the server pushes
  to the Mac. Features are MSC2409 and MSC3202 only: its framework has no MSC4190, and the bot
  logs in with the appservice flow. `run.sh` is rendered beside the two files
  (`InstanceRender::run_sh`, `BridgeInstanceFiles.run_sh`).
- **Address** (`hs-bridges`, `hs-admin`): `PUT …/instances/{user}` takes
  `BridgeInstanceRequest { address }` (`host`, `host:port` or URL; `instance_url` in
  `bridge_offerings.rs` is the grammar); `BridgeOfferingSource::set_instance_address` keeps it
  as the row's `url`, patches the registry's copy at once (the fingerprint leaves `url` out)
  and wakes the manager; an address set before the instance is named survives naming
  (`allocate_and_register` keeps a set `url`). `BridgeInstance.url` is new. Refused for an
  instance the server deploys. The starting reason and the front-door bot say "on the Mac".
- **OpenAPI 0.1.15**: the fields above and the request body.
- **Web**: on branch `agent/imessage-web` (status 16), the "Set up on a Mac" dialog.

**Tests.** `bridge_types::tests::an_imessage_bridge_runs_on_a_mac_and_its_files_say_how`;
`bridge_offerings::address_tests`; `manager::tests::an_administrator_says_where_a_bridge_on_a_mac_listens`;
real binary: `hs-cli/tests/bridge_offerings.rs::an_imessage_offering_runs_on_a_mac_whose_address_the_administrator_sets`
(offer, instance with address, registry url, files, refused address, ready once the address
points at a stand-in that answers pings, removed with the offering).

**Left.** Not run on a real Mac: the owner's is the first (download, `./run.sh`, Full Disk
Access, a message both ways). A Mac the server cannot reach (NAT) needs a websocket on the
server's side (mautrix-wsproxy's protocol); not done. The owner cannot set the address from the
bot's chat (only an administrator, in the interface or the API).

## Session 2026-10-11 (branch `agent/discord-legacy-config`): mautrix-discord gets the older framework's config layout

**What the owner saw.** `bridge-discord-brandon` on the demo in `CrashLoopBackOff`, its `Bridge`
status carrying the container's last line: `FTL Configuration error error="appservice.database
not configured"`. The pod, Service, Secret, PVC and registration were all right; the file was
not.

**Root cause.** mautrix-discord `v0.7.7` is still on mautrix-go's older bridge framework
(`bridge/`, not `bridgev2/`; its `go.mod` wants `maunium.net/go/mautrix v0.16.3-…`). That
framework's `config.yaml` keeps the database under `appservice.database`, and the ghost
template, permissions, backfill limits, double puppeting (`login_shared_secret_map`), encryption
and provisioning under `bridge`. `bridge_types.rs` rendered every `mautrix-*` entry in
`bridgev2`'s layout (top-level `database:`, `encryption:`, `double_puppet:`, `provisioning:`),
so Discord found no database and exited before it listened. Two earlier sessions had written
"mautrix-discord was not checked to be on `bridgev2`"; it is not. Every other mautrix entry in
the catalogue is a `v0.25xx`/`v0.26xx` build, which are all `bridgev2`.

**The fix.** A catalogue entry says `legacy_config: true` (Discord alone), and
`mautrix_legacy_config` writes that framework's file from the same facts, read against
mautrix-discord v0.7.7's `example-config.yaml`:

| Setting | `bridgev2` | older framework (Discord) |
|---|---|---|
| database | `database` | `appservice.database` |
| ghost template | `appservice.username_template` | `bridge.username_template` |
| permissions | `bridge.permissions` | `bridge.permissions` |
| backfill | `backfill.enabled` | `bridge.backfill.forward_limits.{initial,missed}.{dm,channel,thread}` (0 is off; on is 50 initial, 500 missed) |
| double puppeting | `double_puppet.secrets` | `bridge.login_shared_secret_map` (`as_token:` secrets since mautrix-go 0.15) |
| encryption | `encryption` | `bridge.encryption` (`msc4190` exists there too) |
| pickle key | `encryption.pickle_key` | none: a constant in the binary, so not written and never lost by a roll |
| provisioning | `provisioning.shared_secret`, v3 `whoami` | v1, no `whoami`: `provisioning_api: none`, with why |

The wizard's shared render and an offering's instance render both take the layout from the
entry. Because the manager re-applies a deployment whose rendered files changed
(`applied_fingerprint`), a deployed Discord instance rolls by itself once a server with this
change runs: no pod work. `BridgeType.provisioning_api` for Discord is `none` with a note
("…whose provisioning API has no whoami…: ask its bot `ping`"), so the Sign in tab says the
bridge keeps that itself instead of a `refused`, and no secret is minted into its registration.

**Tests.** `bridge_types::tests::a_bridge_on_the_older_framework_gets_that_frameworks_config_layout`
parses Discord's instance file and the wizard's shared file (database under `appservice`, no
top-level `database`/`encryption`/`double_puppet`, `login_shared_secret_map` with the
`as_token:` secret, `bridge.encryption.msc4190`, backfill limits on and off, no `pickle_key`, no
provisioning secret) and that WhatsApp keeps `bridgev2`'s; the catalogue has exactly one legacy
entry. `every_type_says_what_provisioning_api_it_has…` expects `none` for Discord. The web mock
(`bridge-types.ts`) says the same.

**Left.** Not run against the real mautrix-discord image here (the Discord sign-in needs a
Discord account); the demo's `bridge-discord-brandon` is the check, on the roll after this
merges: its pod should reach Running and its bot answer `help`. The management-room rule of the
older framework (bare commands only in the room the bot was invited to) is the same as
`bridgev2`'s per its source, so the manager's chat repair applies unchanged.

## Session 2026-10-10 (branch `agent/bridge-stale-devices`): a bot device the bridge left behind is removed; the OTK-count warning is Synapse's behaviour

Seen on the demo the same day: after the 2026-10-09 crypto reset the WhatsApp bot had two
devices, `BQBMQVR81T` (the reset's) and `BSLXZIVKIV` (2026-10-02's, dead). Clients encrypted room
keys to both, the bridge dropped the dead one's ("targeted to someone else"), and a withheld key
was logged for both. And on every transaction the bridge warned `Dropping OTK counts targeted to
someone else target_user_id=@brandon:...`.

**Fix 1: the manager removes a bot's device the bridge left behind** (decision 0041).

- **The signal is real now.** `hs-auth`'s middleware writes a device's `last_seen_ts` and
  `last_seen_ip` when an appservice acts as it (`device_id` / `org.matrix.msc3202.device_id`),
  at most once a minute (`APPSERVICE_DEVICE_SEEN_EVERY_MS`). Before, an appservice device kept
  the time it was made (the demo's two times, 2026-10-02 21:08 and 2026-10-09 15:12 UTC, are the
  two creations). Test: `a_device_an_appservice_acts_as_is_recorded_as_seen_at_most_once_a_minute`.
- **The rule** (`hs_bridges::manager::bot_devices_plan`, pure): the device in use is the bot's
  device with keys (in `/keys/query`) seen last; a tie or no time means nothing is removed.
  Another device is left behind when it was last seen before that one and at least
  `STALE_BOT_DEVICE_MS` (a day) ago; a device without a time is never removed; the device in use
  never is, whatever its age.
- **The removal** (`settle_bot_devices`, called from `settle_bot_identity` after signing, so every
  minute once settled; skipped with no `/devices` call while no device has keys):
  `GET /devices?user_id=<bot>` then `DELETE /devices/{id}?user_id=<bot>` with the instance's
  token (MSC4190 skips user-interactive auth; the server deletes the device's keys and records a
  device-list change). `MatrixClient::devices` and `MatrixClient::delete_device` (404 is fine).
  A failure is the instance's reason under the identity's prefix.
- **Observable.** One `INFO` line per removal: `removed a device the bridge bot no longer uses
  (the bridge moved on to a newer one) bridge_type=... owner=... appservice=whatsapp-brandon
  bot=@whatsappbot_brandon:... device=BSLXZIVKIV last_seen=2026-10-02T21:08:21.162Z
  kept_device=BQBMQVR81T`. The row keeps the last five (`InstanceRow::removed_bot_devices`,
  `RemovedBotDevice`); the admin API serves them as `BridgeInstance.removed_bot_devices` (schema
  `RemovedBotDevice`, OpenAPI 0.1.13, `hs-admin` model `AdminRemovedBotDevice`).
  `signed_bot_device` is now the device in use when there is one.
- **Which device a withheld key was for.** `KeyWithheld` already carried `to_device_id`; the
  pages did not show it. The offering page's next steps (`InstanceNextSteps`, now also in the
  signed-in phase, through `BotDeviceNotes`) name the device, say when it is not the one the
  bridge uses and whether the server removed it, and list the removed devices; the appservice
  page's withheld line names the device.
- Tests (manager's fake server, which now answers `GET /devices` and `DELETE /devices/{id}`):
  `a_device_the_bridge_left_behind_after_a_reset_is_removed_and_the_one_in_use_kept` (the demo's
  two devices and times; the calls, the fake's devices and keys, the row, the admin view, the log
  line, and a second look that removes nothing),
  `a_bots_older_device_is_kept_until_a_day_unseen_and_the_device_in_use_never_removed`,
  `the_device_in_use_is_the_one_with_keys_seen_last_and_the_rest_wait_a_day`; the four existing
  identity tests now expect the `GET devices` call. Web: three new `InstanceNextSteps` tests.

**Fix 2: the OTK-count warning is expected.** The demo's registration claims the owner
non-exclusively (double puppeting). Synapse sends MSC3202 counts for the sender, every local
member of the transaction's rooms that `ApplicationService.is_interested_in_user` matches (any
user namespace, exclusive or not), and to-device recipients
(`synapse/appservice/scheduler.py` `_compute_msc3202_otk_counts_and_fallback_keys`,
`get_app_service_users_in_room`; read for behaviour, nothing copied). Both our pumps pick the same
users (`hs-appservice` `pump.rs` and `ephemeral.rs`, through `Interest::user`). No change to
behaviour; a comment at the selection and the test
`the_counts_cover_a_user_claimed_non_exclusively_as_synapse_does` say so. The bridge's warning
for the owner's devices would appear against Synapse too.

Verified: `cargo fmt --all --check`; `cargo clippy --workspace --all-targets -- -D warnings`;
`cargo test -p hs-bridges -p hs-appservice -p hs-auth -p hs-admin` (831 passed);
`cargo test -p hs-e2e`, `-p hs-cli --test auth_sessions --test appservice_ephemeral`,
`-p hs-bridge-conformance --test conformance`; `npm run check` in `web/` (648 tests). The real
bridges (`cargo test -p hs-bridge-conformance --test real_mautrix_login`, Docker, the real `hs`
binary, mautrix-whatsapp `v0.2609.0` and mautrix-signal): 5 of 5, and the encrypted story now
also asserts the instance has no reason and `removed_bot_devices: []` after the manager's
`GET /devices` against the real server. Not run against the real binary: an actual removal
(it needs a device a day unseen; the `DELETE` route with MSC4190 and the key removal behind it
are `hs-auth`'s and `hs-e2e`'s own tests).

**Not on the demo yet**: it needs an image roll of this branch's merge. After it, expect
`BSLXZIVKIV` removed on the manager's first look at the instance.

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
`bridgev2`; if it still uses the older framework its management-room rule may differ. (Checked 2026-10-11: it is on the older framework, and its config layout
was wrong; see the 2026-10-11 session above.)

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
`bridgev2`; if it is not, its answer is an honest `refused` (404 or 401) rather than a list. (2026-10-11: it is not; it now says `provisioning_api: none` with why, above.)
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

- 2026-10-15: `hs_appservice::scheduler::{SendError, SendErrorKind, DiscardRequest,
  TRANSACTION_TIMEOUT, Scheduler::{discard, item_limit}}`, `SchedulerConfig::{max_items,
  give_up_after_ms, timeout_backoff_ms}`, `DrainOutcome::Failed`'s `reason`, `txn_id`,
  `attempt`, `elapsed_ms`, `retry_in_ms`; `TransactionSender::send` returns `SendError` (was
  `String`); `QueuedTransaction::{txn_id, first_failed_at_ms}`,
  `AppserviceStore::discard_dead_lettered`, `HealthRow::last_failure_at_ms`, `Health::reason`,
  `BacklogEntry::{enqueued_at_ms, events, ephemeral, to_device, room_ids}`. Admin API (0.1.18):
  `appservices.discard`, `AppService.health_reason`, `AppServiceHealth.reason`, the backlog
  entry's contents; `AppserviceDirectory::discard`.
- 2026-10-10: admin API `BridgeInstance.removed_bot_devices` (schema `RemovedBotDevice`, OpenAPI
  0.1.13; `hs_admin::model::AdminRemovedBotDevice`); `hs_bridges::manager::{bot_devices_plan,
  STALE_BOT_DEVICE_MS}`; `hs_bridges::matrix::MatrixClient::{devices, delete_device}`. Server
  behaviour other tracks see: an appservice request acting as a device (`device_id`) updates that
  device's `last_seen_ts`/`last_seen_ip`, at most once a minute (`hs-auth` middleware).
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

- **2026-10-15, decision 0049: a transient delivery failure is retried for a day under the same
  transaction id.** Timeouts, unreachable bridges and 5xx/408/429 are retried until failing for a
  day; a refused transaction is dead-lettered after ten attempts; a retry is exactly the failed
  transaction; transactions carry at most 100 items and halve on a timeout; a replay gets a fresh
  budget; dead letters are kept until an operator replays or discards them.
- **2026-10-10, decision 0041: the manager removes a bot's device the bridge left behind.** The
  device in use is the bot's device with keys seen last (`last_seen_ts`, which the server now
  writes when an appservice acts as a device); another device last seen before it and a day ago
  is deleted through the instance's token (MSC4190). Never the device in use, never one without a
  time, nothing on a tie.
- **2026-10-10: MSC3202 key counts go to every user the registration claims, exclusive or not**,
  as Synapse's `is_interested_in_user` does; a double-puppeting bridge's owner is one, and a
  mautrix bridge's "Dropping OTK counts targeted to someone else" for the owner's devices is
  expected, not a bug.
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
