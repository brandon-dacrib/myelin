# 10 Push: status

## 2026-10-09 (branch `agent/push-leftovers`): the wave-2 leftovers checked off, a room stays in its email while a dropped line is unread, the rule-write lock is per user

The three leftovers `docs/next-steps.md` names for track 10 were all on `main` already, and each
was re-verified on this tree:

- **MSC4306's `postcontent` push-rule kind**: done 2026-10-05 (`ruleset.rs`, `engine.rs`,
  `routes/tests.rs`' `postcontent_is_a_kind_with_no_rules_and_none_a_client_may_add`).
  Complement `TestThreadedReceipts` and `TestThreadReceiptsInSyncMSC4102`: PASS on `main`'s
  csapi run 18 (status 14, session 11); not re-run on this branch (see Verified).
- **Push rules copied on a room upgrade**: done by `room-render` in `hs-user`'s join hook
  (`SessionHub::copy_room_push_rules`, `crates/hs-user/src/hub.rs`; Synapse's
  `copy_push_rules_from_room_to_room_for_user`), through `hs-push`'s `update_ruleset`. It had no
  test of its own: new real-binary test
  `push_rules_about_the_old_room_follow_its_members_into_the_replacement`
  (`crates/hs-cli/tests/room_upgrade.rs`): alice's room rule is there for the replacement the
  moment `/upgrade` answers; bob's disabled override rule named after the old room, with a
  `room_id` condition on it, is copied renamed, rewritten and still disabled when he joins; the
  old rules stay; his next `/sync` carries both. Nothing in `hs-room` or `hs-user` was touched.
  Complement `TestPushRuleRoomUpgrade`: 4/4 on `main` after `sync-wakes` (status 05); not re-run
  on this branch (see Verified).
- **A HELO fallback for the pusher's mailer**: done 2026-10-05 (`crates/hs-push/src/email/helo.rs`;
  `a_relay_that_refuses_ehlo_gets_the_email_over_helo`).

### Done

- **A room stays in its held email while a line the per-room limit dropped is unread**
  (`crates/hs-push/src/email/{held,mod}.rs`). A held email shows a room's last 10 lines; before,
  an older line was simply dropped, so a receipt that read every line still shown took the room
  out of the email (and the email with it, if it was the only room) even when the dropped line,
  on another scope, was never read. `HeldRoom::unshown` now records, per scope (main timeline or
  thread), the newest dropped position (`UnshownLine`; serde-defaulted, so rows an older build
  stored restore); `HeldRoom::{push_line, take_read, is_empty}` keep it. A receipt takes out
  what it covers of both, and a room leaves the email only when nothing shown or dropped is
  left. A room with no line left but something dropped is emailed with its count from the
  counts store and no lines. Logged at debug when that happens.
- **The push-rule write lock is per user** (`crates/hs-push/src/rulesets.rs`,
  `UserWriteLocks`/`UserWriteGuard`): `CachedRulesetStore::{set_ruleset, update_ruleset}` take
  the user's lock, not one for every user, so a user's slow write (a store waiting on
  PostgreSQL) holds up nobody else's. A user's lock exists only while a write of theirs holds
  or waits for it: the last guard out removes the entry. The cross-replica conditional write is
  unchanged.

### Verified

- `cargo test -p hs-push`: 101 passed (new:
  `a_receipt_reading_every_shown_line_keeps_a_room_whose_dropped_line_is_unread`,
  `one_users_slow_write_does_not_hold_up_anothers`); `cargo fmt --all`.
- **Not verified before the session was cut off** (the machine was under a merge gate, load
  28): `cargo clippy -p hs-push --all-targets -- -D warnings` was running; `cargo clippy -p
  hs-cli --all-targets` and the new real-binary test `cargo test -p hs-cli --test room_upgrade
  push_rules_about` had not run; the Complement image `complement-hs-reimplement:push-leftovers`
  was mid-rebuild and the three Complement tests were not run through `tests/complement/lock.sh`
  on this branch. Whoever picks this up: run those four, in that order.

### Left

- Thread subscriptions themselves (MSC4306: the subscription endpoints, the sync extension and
  the two `postcontent` rules). More than a day, and it needs the sync routes, which another
  track is in now.
- `synapse-small`'s `populate.py` does not make the threaded receipts (status 13).

### Interfaces changed

- `hs_push::email::held::HeldRoom` has a field `unshown: Vec<UnshownLine>` (serde default);
  `HeldRoom::{push_line, take_read, is_empty}` and `UnshownLine` are new.
- `hs_push::rulesets::{UserWriteLocks, UserWriteGuard}` are new; the store's public calls are
  unchanged.

## 2026-10-08 (branch `agent/push-receipts`): everything a receipt reads follows its thread; push rules across replicas

### Done

- **`/notifications` follows the receipt's thread** (`crates/hs-push/src/notification_log.rs`,
  `notification_log/{memory,tables}.rs`). An entry is read when the unthreaded receipt or its
  own scope's (`main`, or its thread's root) is at or past its event's room position, as the
  unread counts are. Entries carry their position and thread (`NewNotification.pos`/`thread`);
  marks are per room and scope (`ReadMark`: a position, plus a `seq` for a receipt whose event
  is not known here and one for entries logged before positions were kept), in the new
  keyspace `hs_push.notification_read_scopes`. The old whole-room `hs_push.notification_read_marks`
  rows are still read, as the unthreaded mark. `NotificationLogStore::mark_room_read` is now
  `mark_read(user, room, thread, pos)`.
- **Held notification emails follow it too** (`crates/hs-push/src/email/mod.rs`). Each held line
  carries its position and thread (`NotificationLine.pos`/`thread`, serde-defaulted so emails held
  by an older build restore); a receipt takes out only the lines it covers, a room with no line
  left leaves the email, an email with no room left is not sent. `EmailPushersHandle::room_read`
  is now `read(user, room, thread, pos)`; the throttle still resets on any receipt for the room.
  Logged at debug: "a receipt took what it read out of a waiting notification email" with
  `lines_read`.
- **Push rules across replicas** (`compiled.rs`'s "Across replicas", `rulesets.rs`,
  `crates/hs-cli/src/push_cluster.rs`):
  - Every write is announced to the other live replicas on the mesh, route
    `push.rules_changed` (`{user_id, seq}`), and a replica told drops its cached copy
    (`CachedRulesetStore::changed_elsewhere`; `RulesetChangeFeed` is the seam, installed by
    `hs_cli::push_cluster::install` in cluster mode only).
  - Cache entries carry the change-seq they were read at. What a client is shown (`GET
    /pushrules`, `/sync`'s `m.push_rules`) is checked against the store's seq on every read
    (`CachedRulesetStore::current_ruleset`, one point read), so it is never stale even if a
    message is lost; a client read also refreshes that replica's copy.
  - The evaluation path re-checks an entry older than 30 s (`REVALIDATE_AFTER`, cluster only):
    the bound on a lost message.
  - **Concurrent edits on two replicas no longer lose one** (found reviewing `sync-wakes`, see
    below): `RulesetStore::set_ruleset_if(user, ruleset, expected_seq)` writes only at the seq
    the edit started from (serializable transaction on the KV backends), and
    `update_ruleset` runs the edit again on the newer ruleset when it lost (up to 8 times;
    `edit` is `FnMut` now).
  - Metric `hs_push_rule_cache_invalidations_total{source="local"|"peer"|"stale"}`; info logs
    when a peer refuses or misses a change, and at startup ("push rules are cluster-aware").
- **A receipt's position without a room load on a non-owner** (`crates/hs-cli/src/push_delivery.rs`):
  `RegistrySource::event_position` reads the event's stored row
  (`event_id -> EventSn -> PersistedEvent.room_pos`, written with the timeline row in one
  transaction; `RoomRegistry::find_event_globally`) on a replica that does not own the room,
  where it loaded the whole room. The owner still asks its resident actor. Measured (debug
  build, Fjall, `measure_the_receipt_position_lookup`, 20 rounds each): a room of 100 messages
  9.3 ms load vs 23 µs index; 1,000: 129 ms vs 25 µs; 5,000: 470 ms vs 28 µs. In practice
  `POST .../receipt` is forwarded to the room's owner, so the non-owner case is the edge
  (ownership moving under a request), not the common path.
- **Synapse's threaded receipts are imported in their threads** (`hs-compat`, `hs-cli`'s
  migration target, `hs-user`'s `SessionHub::import_receipt`/`receipt_of`,
  `ReceiptRegistry::get`): status 13's 2026-10-08 section has the details.

### Review of `sync-wakes`' `hs-push` change (`a1fa71a6`)

- `CachedRulesetStore::update_ruleset` (one lock, reading the store, not the cache): correct
  within one process. The lock is a `tokio::sync::Mutex` held across store I/O for every
  user's writes; acceptable since writes are rare, but a slow store stalls all rule writes on
  that replica, noted. It did not cover replicas: two edits for one user on two replicas could
  still lose one. Fixed here with the conditional write (above), and the two-replica test shows
  it (without it, wave 0 lost two of eight rules).
- `RuleCache`'s generation: correct. The generation is read before the store read, an insert is
  refused under the write lock if any invalidation happened since, and `invalidate` bumps it
  under the same lock; a write between the read and the insert keeps the stale copy out, and a
  write before the generation read is already in the store. Any user's invalidation refuses
  every concurrent insert, which only costs a re-read. `RuleCache::insert` (unguarded) is used
  by tests only.
- `edit` was `FnOnce`; it is `FnMut` now so a lost conditional write can run it again.
  `SessionHub::copy_room_push_rules`' closure needed no change.
- The race test (`a_read_racing_a_change_does_not_cache_the_old_rules`) relies on a 5 ms vs
  20 ms sleep: under load it can pass without exercising the race, never fail spuriously.

### Verified

- `cargo test -p hs-push` (99; new: `receipts_mark_entries_read_per_thread` on both stores,
  `a_whole_room_mark_from_an_older_build_still_reads`, `a_mark_never_moves_back_and_reads_legacy_entries_by_seq`,
  `a_thread_receipt_marks_only_its_threads_entries_read`, the log check in
  `receipts_read_their_thread_up_to_their_event`, `a_receipt_takes_out_only_what_it_reads`,
  `a_change_made_on_another_replica_meanwhile_is_kept`, `a_change_on_one_replica_reaches_the_others_cache`,
  `a_lost_message_is_bounded_by_the_revalidation_interval`, `a_conditional_write_lands_only_at_the_seq_it_expects`);
  `cargo test -p hs-user` (new `get_finds_a_receipt_by_its_thread`), `-p hs-compat`, `-p hs-cli --lib`
  (`push_cluster`, `push_delivery`); clippy clean on `hs-push`, `hs-user`, `hs-compat`, `hs-cli`.
- Real server, `cargo test -p hs-cli --test thread_receipts`: `/notifications`' read flags of
  A-F checked after each receipt of Complement's `TestThreadedReceipts` (with whole-room reads,
  as on `main`, the first check fails: all six read after a `main` receipt on A).
- Real binary, `--test email_held_restart`: new
  `a_thread_receipt_leaves_the_main_timeline_in_the_waiting_email` (a thread receipt takes one
  line out, the email about the main timeline still goes); the restart test still passes.
- Real binaries, two replicas on PostgreSQL, `--test cluster_push_rules` (new; green on each of its last 4 runs,
  `HS_CLUSTER_TEST_POSTGRES_DSN`): alice's content rule changed on A, then on B, each while the
  room's owner had her rules cached; the owner's next push carries the new sound each time,
  then `GET /pushrules` and `/sync` on the other replica show it; then three waves of eight
  concurrent `PUT`s split across A and B all land. Without `push_cluster::install` round 1 fails
  (the owner pushed with the old sound); without the conditional write wave 0 loses rules.
- Also green: `--test migration` (real binary, 4 receipts), `cluster_ephemeral`,
  `push_rules_concurrent`, `push_federated_invite`, `room_upgrade`, `federation_edus`.

### Left

- Thread subscriptions (MSC4306) and their `postcontent` rules (unchanged).
- A held email trims each room to its last 10 lines; if a receipt reads every kept line, the
  room leaves the email even when an older, trimmed line on another scope is still unread.
- The rule-write lock is per process and global; fine for human-rate edits.
- `synapse-small`'s `populate.py` does not make the threaded receipts (status 13).

### Interfaces changed

- `hs_push::notification_log::NotificationLogStore::mark_read` (was `mark_room_read`);
  `NewNotification` has `pos` and `thread`.
- `hs_push::email::EmailPushersHandle::read` (was `room_read`); `Notification` has `pos` and
  `thread`; `EmailPushersWorker::read` (and `room_read`, unthreaded, kept for tests).
- `hs_push::rulesets::RulesetStore::set_ruleset_if` (new, required);
  `CachedRulesetStore::{current_ruleset, install_change_feed, changed_elsewhere}`,
  `RulesetChangeFeed`; `update_ruleset` takes `FnMut`. `hs_push::compiled::{CachedRules,
  register_metrics}`; `RuleCache::insert_if_unchanged` takes the seq; `RuleCache::{entry, confirm}`.
- `hs_user::hub::SessionHub::import_receipt` takes a `thread`; `SessionHub::receipt_of` and
  `ReceiptRegistry::get` are new.
- Mesh peer route prefix `push.` (`push.rules_changed`), `hs_cli::push_cluster`.

## 2026-10-05 (branch `agent/push-gaps`): threaded receipts and per-thread counts, `postcontent`, one HELO fallback

### Done

- **Unread counts are "after the read receipt", per thread** (MSC3771, MSC3773;
  `crates/hs-push/src/counts.rs`, `counts/{memory,tables}.rs`). Each notification is kept with
  its event's room position (`hs_push.unread`, key `(user, room, thread_key, pos)`) until a
  receipt reads it; a receipt is a position and a `ReceiptThread` (`Unthreaded`, `Main`,
  `Thread(root)`). An unthreaded receipt reads every scope up to its event, `main` the main
  timeline, a thread receipt that thread; what came after stays unread. Read positions are kept
  (`hs_push.read_marks`) so a notification the pipeline learns of after the receipt that read it
  is not counted. `CountsStore::record_notification` takes the position; `mark_read` is new.
  Counters from older builds (`hs_push.counts`) are still counted until a receipt covering their
  scope deletes them.
- **The pipeline reads receipts per thread** (`pipeline.rs`): `Job::Receipt(ReadReceipt)` carries
  the event and thread; `EventSource::event_position` (new; `hs-cli`'s `RegistrySource` asks
  `RoomActor::timeline_position`) gives its position, and an event not known here reads its
  whole scope. `ReadReceiptSink::read_receipt` is async and returns once the pipeline has
  handled the receipt (at most `RECEIPT_SETTLE`, 2 s), so the `/sync` after `POST .../receipt`
  has its counts. `SettledCounts` (installed for `/sync` in `hs-cli`'s `serve.rs`) waits up to
  `READ_SETTLE` (250 ms) for queued jobs before a read, and stops waiting for a second after a
  wait runs out. `PipelineHandle::{wait_for, settle}` are the job tickets behind both.
- **Threaded receipts in `hs-user`** (the receipt store only, plus the sync lines below):
  `ReceiptRegistry::set_in_thread`, one receipt per user, kind and thread; the content shows a
  threaded receipt's `thread_id`, and on one event the unthreaded receipt wins (MSC4102, else the
  newest). `StoredReceipt.thread_id` (serde default; a threaded row's key is
  `"{kind} {thread_id}"`, an unthreaded row keeps its old key). `POST .../receipt` takes
  `thread_id` (`main` or an event ID, else `400 M_INVALID_PARAM`); `SessionHub::set_threaded_receipt`
  (`set_receipt` is it, unthreaded). The `m.receipt` EDU carries `data.thread_id` both ways, and
  its coalescing key names the thread, so a threaded receipt never replaces an unsent
  unthreaded one. Files: `crates/hs-user/src/{receipts.rs, edu.rs, hub.rs, filter.rs,
  routes/receipts.rs, store/mod.rs, store/tables.rs}`.
- **`/sync` splits counts by thread only when asked** (`crates/hs-user/src/sync/mod.rs`, the
  joined-room entry, about lines 1270-1320; `SyncFilter::unread_thread_notifications` reads
  `room.timeline.unread_thread_notifications`): asked, `unread_notifications` is the main
  timeline and `unread_thread_notifications` each thread with something unread (left out when
  none); not asked, one count for the room and no `unread_thread_notifications` key at all.
  `sync-polling` owns this file: the change is that block only.
- **MSC4306's `postcontent` push-rule kind** (`ruleset.rs`, `engine.rs`): accepted on every
  `/pushrules` path, evaluated between `content` and `room` as Synapse does, with no rules here
  (thread subscriptions are not implemented, as Synapse with `msc4306_enabled` off), so its rules
  are `404` and a client `PUT` is `400 M_INVALID_PARAM`. Left out of `GET /pushrules/` when empty.
- **One HELO fallback** (`crates/hs-push/src/email/helo.rs`, moved from `hs-cli`'s
  `smtp_helo.rs`, which is gone): `SmtpMailer::send` retries over plain `HELO` when a relay
  without TLS and without credentials refuses `EHLO` (`500`/`502`), so notification emails reach
  such a relay too; `hs-cli`'s validation-email sender no longer has its own copy.
- Not done here, by the coordinator's change: `copy_room_rules` for `TestPushRuleRoomUpgrade`
  (`room-render` copies the rules in `hs-user`'s upgrade hook with `get_ruleset`/`set_ruleset`).
- Observability: `debug` when a receipt's event is unknown (whole scope read), when the
  pipeline is behind a receipt or a read; `info` when the mailer falls back to `HELO`.

### Verified

- `cargo test -p hs-push` (86; new: `receipts_read_per_thread_up_to_their_position` on both
  stores, `legacy_counters_count_until_a_receipt_reads_their_scope`,
  `receipts_read_their_thread_up_to_their_event`,
  `a_receipt_and_a_settled_read_see_what_was_queued_before_them`,
  `postcontent_is_a_kind_with_no_rules_and_none_a_client_may_add`,
  `a_relay_that_refuses_ehlo_gets_the_email_over_helo`); `cargo test -p hs-user` (new:
  threaded-receipt registry and EDU tests, `a_receipt_takes_a_thread_id`, the sync counts test
  with and without the thread filter); clippy clean on `hs-push`, `hs-user`, `hs-cli`.
- Real binary, `cargo test -p hs-cli --test thread_receipts` (5 runs, all green):
  `threaded_receipts_read_their_thread_and_counts_split_by_thread` is Complement's
  `TestThreadedReceipts` step for step, stricter (each `/sync` right at once);
  `an_unthreaded_receipt_wins_a_clash_here_and_over_federation` is
  `TestThreadReceiptsInSyncMSC4102` across two servers. Also green: `--lib`, `push_federated_invite`,
  `email_held_restart`, `email_pushers` (Mailpit), `threepid_email`, `federation_edus`,
  `appservice_ephemeral`, `e2e` receipts and push tests.
- Complement csapi, image of this branch (`complement-hs-push-gaps:dev`): `TestThreadedReceipts`
  and `TestThreadReceiptsInSyncMSC4102` PASS (both FAIL on `main`: `disableMsc4306PushRules`
  got a `400` for `postcontent`), with `TestPushSync`, `TestPushRuleCacheHealth`,
  `TestRoomReceipts`, `TestChangePasswordPushers`, `TestThreadsEndpoint` still passing.
  `TestPushRuleRoomUpgrade` still fails, all four subtests: its copy is `room-render`'s.
- Sytest with this branch's bookworm `hs` (`SYTEST_HS_BINARY`): `61push/01message-pushed.pl`,
  `03_unread_count.pl`, `08_rejected_pushers.pl`, `09_notifications_api.pl`,
  `10apidoc/37room-receipts.pl`, `30rooms/21receipts.pl`, `31sync/12receipts.pl`,
  `50federation/38receipts.pl`, `90jira/SYN-516.pl`: 20 PASS, 1 SKIP (the unstable
  `org.matrix.msc2625.mark_unread` action, skipped on `main` too).

### Left

- The `/notifications` log and the email worker still treat any receipt as reading the whole
  room (a thread receipt marks the room's log entries read and cancels its held email).
- Thread subscriptions (MSC4306) themselves, and with them its two `postcontent` rules.
- Synapse-imported threaded receipts are still taken as room receipts (`hs-compat`'s rule).
- In a cluster, a receipt on a replica that does not own the room reads the room's stored
  state to find the event's position (`RoomRegistry::read_room`), which is a full load.

## 2026-10-05 (branch `agent/email-held-flake`): the held email is stored before "holding" is logged

`email_held_restart` failed in two merge gates: the worker logged "holding a notification for an
email" before writing the row, and the test kills the server on that line, so under load the kill
landed first and nothing was restored. The worker now stores the row, then logs (Fjall's default
journal mode flushes each commit to the OS, so a committed row survives `SIGKILL`). Reproduced
with 10 runs beside a looping `cargo test -p hs-room`: 1/10 passed before, 10/10 after.

## Session 4 (2026-10-04, branch `agent/push-media-gaps`): invites pushed with their room's name, held emails survive a restart

### Done

- **An invite over federation is pushed with the room's name** (Sytest "Invites over
  federation are correctly pushed with name", `61push/01message-pushed.pl`). The invitee's
  server holds nothing of the room but the invite and the stripped state it carried
  (`unsigned.invite_room_state`), and `hs_room::routes::render::client_event_json` leaves that
  out of the rendered event, so `hs_push::pipeline`'s own fallback never saw it.
  `crates/hs-cli/src/push_delivery.rs`'s `describe` now reads the room name (then the
  canonical alias) and the inviter's display name from the stored event's stripped state
  when the room's state has none.
- **Notification emails waiting to be sent survive a restart** (`crates/hs-push/src/email/held.rs`):
  `HeldMailStore` (in-memory and `TablesHeldMailStore`, keyspace `hs_push.email_held`, key
  `(holder, user, address)`, value the held email as JSON with a wall-clock `due_ms`). The
  worker writes every change through (hold, room read, sent, retried, given up) and, when it
  starts, restores its holder's rows: due when they were, at once if that passed while the
  server was down. The holder is `hs_cli::cluster::task_runner_name` (`single-node`, or the
  replica's identity in a cluster), so a replica restores only what it held and two replicas
  never overwrite each other's rows. A row goes as soon as the SMTP server accepts its email,
  so delivery is at least once (a stop in that instant sends it again). `NotificationLine`/
  `LineText` are serde now.
  `EmailDeps` has two new fields, `held` and `holder` (wired in `hs-cli`'s `serve.rs`).
- **Observability**: `info` "restored the notification emails left waiting at the last stop"
  with the count; `warn` when a held email cannot be stored or the rows cannot be read.

### Verified

- `cargo test -p hs-push` (75; new: both stores round-trip every line kind and keep holders
  apart; `a_waiting_email_survives_a_restart`; `an_email_that_fell_due_while_stopped_goes_at_once_and_reads_still_cancel_it`).
- Real binary: `cargo test -p hs-cli --test push_federated_invite`: two in-process servers,
  a push gateway in the test, charlie on B invites alice on A to "Test Name"; the push A sends
  carries `room_name: "Test Name"` and `sender_display_name: "Charlie"` (fails on `main` with
  `room_name` absent, the Sytest failure exactly).
- Real binary: `cargo test -p hs-cli --test email_held_restart`: `hs serve` with an SMTP sink
  in the test and `delay_before_mail: 20s` holds Alice's email, is `SIGKILL`ed, and the next
  `hs` on the same data directory logs `restored ... emails=1` and sends it once; a third
  start restores and sends nothing. `--test email_pushers` (Mailpit) still passes.
- Sytest, image of this branch (`tests/sytest/run.sh tests/61push/01message-pushed.pl`):
  all nine pass, "Invites over federation are correctly pushed with name" among them.

### Left

- Password-reset and 3PID-validation email (track 07, `hs-auth`) still send nothing: there is
  no `requestToken` endpoint to use `hs_push::email::Mailer` from. Not done here (the brief
  gave it to `auth-gaps`).
- A mail held by a replica that never returns under the same identity is not sent.
- No unsubscribe link (unchanged from session 3).

## Session 3 (2026-10-04): email pushers deliver

**Starting point.** Email pushers were "stored, never delivered to" (session 2's Next;
`docs/next-steps.md` item 6). No crate sent mail; `hs-testkit`'s `FakeSmtpSink` was a recorder
with no trait behind it.

### Done

- **SMTP sender** (`crates/hs-push/src/email/smtp.rs`): `SmtpMailer` over `lettre` 0.11
  (async, tokio, rustls with the ring provider and the system's roots), one connection per
  email, `multipart/alternative` text and HTML. `starttls` (required, never opportunistic),
  `tls` (implicit) or `none`; optional credentials and `tls_name`; 30 s timeout. Its settings
  are replaced in place (`SmtpMailer::set`) when the configuration changes.
- **The `email` configuration section** (`crates/hs-config/src/email.rs`): `smtp.{host, port,
  security, username, password(_file), tls_name}`, `from`, `app_name`, `client_base_url`,
  `notifications.{enabled, delay_before_mail, throttle_start, throttle_max,
  throttle_multiplier, throttle_reset_after, subjects.*}`. Synapse's key names and subject
  placeholders are kept where the meaning matches; each doc comment names its Synapse key.
  Validated (a host needs `from`, a username needs a password and the other way round, `from`
  must be an address, `client_base_url` must be http(s), the throttle must be increasing).
  Every setting is **hot**: the mailer and the worker read them per email, wired in
  `hs-cli`'s `live.on_change("email")`. `docs/config.md` regenerated; the web's schema
  fixture regenerated.
- **The email** (`crates/hs-push/src/email/template.rs`): subject chosen as Synapse chooses
  (one message from a person in a named room, messages in a room, several rooms, an
  invitation, with or without a room name); text and HTML bodies with one section per room:
  the room's name linked to `<client_base_url>/#/room/<id>` (else `matrix.to`), the unread
  count from `CountsStore`, each message as sender's display name, time (UTC) and a snippet
  of at most 200 characters, "an encrypted message" in place of ciphertext, "and N more"
  past ten lines. Every interpolated string is HTML-escaped.
- **Batching and throttling** (`crates/hs-push/src/email/mod.rs`, `throttle.rs`): the
  pipeline hands each notification for an email pusher to a worker, which holds it per
  `(user, address)`; one email carries every room held by its due time. First email in a room
  after `delay_before_mail` (0 by default); then the room waits `throttle_start` (10 min),
  times `throttle_multiplier` (6) per email, capped at `throttle_max` (1 day), reset by a read
  receipt or by `throttle_reset_after` (12 h) without a notification. A receipt also drops the
  room from a held email; a room whose counts are zero by send time is left out, and an email
  with nothing left is not sent. Throttle state is stored (`hs_push.email_throttle`); held
  emails are in memory. A failed send is retried twice, a minute apart.
- **`POST /pushers/set` with `kind: email`** now requires the pushkey to be an email address
  bound to the account (Synapse does the same; otherwise any account could have the server
  email room traffic anywhere). `M_INVALID_PARAM` otherwise.
- **Observability**: `hs_push_email_sent_total{outcome=sent|failed|skipped}`; a boot line and
  a line per configuration change saying where mail goes (`email: notification emails go
  through this SMTP server` with host, port, security), or that none is configured; `info`
  per email sent, `warn` per failed attempt and when one is given up, `debug` per held
  notification.
- **Web**: the Configuration page gets the `email` section from the schema like every other
  section (a mail icon added); the Users page's push notifications list shows an email
  pusher as "Email to <address>" (`web/src/pages/users/UserClientDataSection.tsx`, mock and
  test updated).

### Verified

- `cargo test -p hs-push` (71: template, subject choice, escaping, throttle stores, the worker
  with paused time: first email at once, 10 min then 60 min, read receipt cancels and resets,
  several rooms in one email, encrypted rooms quote no ciphertext, retry then give up, no
  server/disabled/pusher removed sends nothing, the spawned worker; the `/pushers/set`
  address check), `cargo test -p hs-config` (the section's validation; every setting
  classified; the web fixture), `cargo test -p hs-cli --lib` (the config-to-settings mapping),
  per-crate clippy clean.
- **Real binary, real SMTP**: `cargo test -p hs-cli --test email_pushers` starts Mailpit
  (`mirror.gcr.io/axllent/mailpit`) on Docker-chosen ports, boots `hs serve` with an `email`
  section, binds Alice's address through the admin API, has her set an email pusher (refused
  before the binding), has Bob send a message in "Lunch", and reads the mail back from
  Mailpit's API: subject `[Myelin] You have a message on Myelin from Bob in the Lunch
  room...`, the snippet in text and escaped in HTML, the room link, `From` the configured
  address; then a second message is held by the throttle, and `/metrics` shows
  `hs_push_email_sent_total{outcome="sent"} 1`. Removes the container; prints `SKIP` without
  Docker (or use `HS_TEST_MAILPIT=<smtp port>,<api port>` for a running one).
- Sytest: `tests/61push/` has no email-pusher test, so there was nothing to rerun.
- Web: `npm run check` (569 tests) and `npm run test:e2e` (64) pass;
  `web/e2e-real/configuration.spec.ts`'s new case, run against a real `hs serve` from this
  branch, opens Configuration > Email (every setting a control, no JSON box), saves an SMTP
  host and sender through the page, and finds them stored and
  `hs_config_reloads_total{section="email",outcome="applied"}` counted; the server logged the
  new mail route. `email` and `network` were added to the web's `KNOWN_SECTION_ORDER`.

### Left

- ~~Synapse's `email` block is not translated by `hs-compat`~~ (done in session 4, see
  `docs/status/13-config-compat-and-migration.md`).
- Password-reset and 3PID-validation email (track 07) can use `hs_push::email::Mailer`;
  nothing does yet. Users bind an address only through an administrator.
- ~~Held emails are lost on restart~~ (stored since session 4). Synapse's ten-minute wait before the first email is `delay_before_mail: 10m`.
- No unsubscribe link (Synapse's carries a macaroon-signed one); the footer says to remove
  the email notification in the client.
- The Configuration page's mock (`npm run dev:mock`) has no `email` section, as it has no
  `network` one; the real server's page renders it from the schema.

### Decisions made

- **`lettre` with rustls/ring, no pool**: the workspace already uses ring; a connection per
  email avoids a pool to keep healthy and makes a configuration change take effect on the
  next email.
- **STARTTLS is required, not opportunistic**: `security: starttls` refuses to send in the
  clear. `none` is an explicit choice.
- **The first email goes at once by default** (Synapse waits ten minutes). Configurable.
- **Email pushers need a bound address** (above).
- **`hs-push` does not depend on `hs-config`**: `hs_push::email::Settings` and
  `smtp::SmtpSettings` restate what they need; `hs-cli/src/push_delivery.rs` maps the section.

### Shared dependencies added

- `lettre = 0.11` to `[workspace.dependencies]` (`default-features = false`, features
  `smtp-transport, builder, hostname, tokio1, tokio1-rustls, rustls-native-certs, ring`).
  `hs-push` also uses `time` (already in the workspace).

## Session 2 (2026-10-02): Sytest's push group, 19 of 53 before; the fixes are built and unit-tested, not yet rerun under Sytest

**Starting point.** `docs/status/sytest/2026-10-02-results.txt` had 19 of the 53 push-group
tests passing. A quiet-machine rerun of `tests/61push/*.pl` on the unmodified image gave the
same 19 (of 55 tests in those files; two of them Sytest groups elsewhere), so none of the
failures were load. Two causes covered nearly everything: the `/pushrules` surface rejected
what Sytest sends, and nothing in the server evaluated an event for push at all (status 10's
session 1 recorded `RoomUpdate::push_evaluation_inputs` as the blocker; the pipeline below no
longer needs it).

### Done

**Push rules (`crates/hs-push/src/ruleset.rs`, `routes/pushrules.rs`, `routes/mod.rs`)**

- The ruleset is this crate's own `Ruleset` over Ruma's rule, condition and action types. Ruma's
  `Ruleset` types a room rule's `rule_id` as `OwnedRoomId`, and Sytest's `02add_rules.pl` adds a
  room rule named `#spam:example.com` (Synapse stores rule ids verbatim). The JSON shape is
  unchanged, so stored rulesets read back as before; the engine (`engine.rs`) walks the five
  kinds in priority order and matches with `ConditionalPushRule::applies` /
  `PatternedPushRule::applies_to`, with room and sender rules a string comparison.
- `GET /pushrules/global/` and `GET /pushrules/global/{kind}/` list rules. Every other path
  shape under `/pushrules/` (missing or unknown scope, a kind without its slash, an empty rule
  id, an unknown attribute, `PUT /pushrules/`) answers `400 M_UNRECOGNIZED`, as Synapse does;
  `/pushrules` without the slash stays the router's 404. The spec's literal paths are registered
  verbatim (the spec coverage tool matches on them) with `{scope}` twins beside them.
- `PUT` validation follows the spec's error table: override/underride need `conditions`,
  content needs `pattern`, `actions` is required and limited to `notify`, `dont_notify`,
  `coalesce` and `set_tweak` objects (an unknown action such as MSC2625's `mark_unread` is a
  400, which is how Sytest learns to skip that test), ids starting with `.` or containing `/`
  or `\` are refused. A re-`PUT` keeps the rule's place and its `enabled` flag. New override
  rules slot in after `.m.rule.master`.
- `GET /pushrules/global/{kind}/{ruleId}/{attr}` returns `{attr: value}` for any field the rule
  has, 400 otherwise.

**The pipeline (`crates/hs-push/src/pipeline.rs`, `cursors.rs`; `crates/hs-cli/src/push_delivery.rs`)**

- `hs_push::pipeline` is a worker fed by the room registry's global stream (forwarded by
  `hs-cli`, as the appservice pump is) and by read receipts (`SessionHub::set_receipt` calls the
  new `install_read_receipt_sink`). For each event it asks an `EventSource` (hs-cli's
  `RegistrySource` over `RoomRegistry::read_room`) for the event's client JSON, the members with
  display names and local-ness, the power levels, the room name (`m.room.name`, else the
  canonical alias) and the sender's display name; then evaluates every joined local member
  (and the invitee of an invite) against their cached ruleset. A `notify` match increments
  `CountsStore` (thread-scoped when the event is in a thread), appends to the notification log,
  and posts to each of the user's HTTP pushers with `counts.unread` = the user's total across
  rooms. The payload is the spec's, plus `id` (deprecated, still expected by the gateway API's
  examples and Sytest), `membership`/`user_is_target` for member events, `room_name`,
  `sender_display_name`, `prio`. `format: event_id_only` sends the reduced body. A gateway's
  `rejected` list deletes those pushers. An invite from another server takes the room name and
  inviter's name from `unsigned.invite_room_state`.
- A read receipt (either kind) zeroes the room's counts (`CountsStore::reset_room`, new), marks
  the room's log entries read, and sends every HTTP pusher the zero/new badge.
- Per-room cursors (`hs_push.room_cursor`) skip an event already evaluated: the room stream
  re-announces a room's newest event whenever the room is loaded. A room first seen with a head
  older than the pipeline's start by more than 60 s is taken as such a re-announcement and not
  pushed (so an upgrade does not push every room's last message once).
- Delivery is spawned per push; a slow gateway delays only its own pushes. Lag on the room
  stream is logged with the count missed.
- Metrics: `hs_push_evaluations_total{result=notify|silent|none}` and
  `hs_push_http_pushes_total{outcome=sent|rejected|failed}`. Logs: a `debug` per matched rule
  and per push, `info` when a pusher is removed for a rejected pushkey, `warn` on a failed push.

**`GET /notifications` (`routes/notifications.rs`, `notification_log.rs`)**: pages the log
newest first with `from`/`limit`/`only=highlight`, each entry `{room_id, actions, event,
profile_tag, read, ts}`; `read` is a per-room watermark set by receipts. Entries carry the
event JSON as it was, so the endpoint needs no room lookup.

**Pushers and password changes (`pushers.rs`, `crates/hs-auth/src/state.rs`, `routes/account.rs`)**:
a pusher remembers the device that set it; `POST /account/password` with `logout_devices`
calls hs-auth's new `SessionRevocationObserver`, answered by `hs_push::pushers::RevokedSessionPushers`,
which deletes every pusher of another device. Pushers with no known device (migrated) stay.

**Tests**: hs-push 31 -> 56 (ruleset edits and ordering, every 400/404 case of
`80torture.pl` through the router with an in-memory user, `/notifications` paging, the
pipeline's evaluation/counting/receipt/delivery/rejection against `hs_testkit::FakePushGateway`,
counts and log contract tests for both backends, cursor stores, device-scoped pusher deletion).
`crates/hs-cli/tests/e2e.rs`'s `sync_keys_and_push_surfaces_answer_through_the_real_binary`
still passes.

### Where this stopped (the machine was rebooted before the rerun)

Baseline, measured this session on the unmodified `myelin-sytest:dev` image with
`tests/61push/*.pl` alone: 19 pass, 36 fail (55 tests in those files), the same 19 as the
night's whole-suite run, so no push failure was load. The failures were, by cause: 11 in
`02add_rules.pl` (room rule ids that are not room ids, and `GET /pushrules/global/{kind}/`
missing), 11 in `80torture.pl` (400 expected, 404/405/200 given), 9 in `01message-pushed.pl`,
3 in `03_unread_count.pl`, `08_rejected_pushers.pl`, `09_notifications_api.pl`, and the
password-change pusher test in `14account/` -- all because nothing evaluated events for push.

Every one of those causes is addressed above, and each has a Rust test (the `80torture.pl`
table verbatim through the router, the pipeline against `hs_testkit::FakePushGateway`,
`sync_keys_and_push_surfaces_answer_through_the_real_binary` on the real `hs`). **What is not
done:** the Sytest rerun on the modified binary. The bookworm release build of this branch's
`hs` inside Docker (needed for `SYTEST_HS_BINARY`) was still compiling when the reboot was
called and was stopped. The count after is therefore unmeasured; the expectation from the
causes above is most of the 36, with `Rejected events are not pushed` and the two
federation-invite tests the least certain (they depend on the stub room an out-of-band
invite creates answering `members()` with the invitee, which was read in the code, not run).

**Measured 2026-10-04** (status 14 session 8: the whole suite on merged `main` `a9f62fc7`, quiet
machine, `docs/status/sytest/2026-10-04-results.txt`): **`tests/61push/*.pl` 50 pass, 1 fail, 1
skip** (the `are-we-synapse-yet` "Push APIs" group: 19/53 -> 50/51), `14account/01change-password.pl`
7/7. `02add_rules.pl` 11/11, `80torture.pl` 22/22, `03_unread_count.pl` 2/2 with the MSC2625
`mark_unread` test skipped by its fixture, `09_notifications_api.pl` 1/1, `08_rejected_pushers.pl`
1/1, "Rejected events are not pushed" passes. The one failure: "Invites over federation are
correctly pushed with name" (`01message-pushed.pl` line 731: `room_name` is undef in the push for
a federated invite, whose name is only in the invite's stripped state).

To finish: `tests/sytest/build.sh myelin-sytest:push-rules` from this branch, then
`SYTEST_IMAGE_TAG=myelin-sytest:push-rules tests/sytest/run.sh tests/61push/01message-pushed.pl
tests/61push/02add_rules.pl tests/61push/03_unread_count.pl tests/61push/05_set_actions.pl
tests/61push/06_get_pusher.pl tests/61push/07_set_enabled.pl tests/61push/08_rejected_pushers.pl
tests/61push/09_notifications_api.pl tests/61push/80torture.pl tests/14account/01change-password.pl`,
and fix whatever the server logs under `server-0/hs.log` show for anything still failing.

### Next

- `.m.rule.suppress_edits`/`m.replace` and reactions already come from the default rules;
  MSC3664 (reply), MSC4028 and MSC3381 remain as session 1 left them.
- Thread receipts (MSC3771): a receipt resets the whole room, threads included, because
  `hs-user` has no thread receipts yet.
- Email pushers: delivered since session 3 (above).
- The cluster: each room's owner evaluates its events (`RegistrySource` returns nothing for a
  room this replica does not own); the pipeline's cursors and counts are in the shared store.
  Rule-cache invalidation across replicas is still the session 1 gap.

### Decisions made

- **Own `Ruleset` type instead of `ruma::push::Ruleset`** (above). `hs-cli`'s migration and
  `hs-user`'s one test were adapted; `hs_push::ruleset::RuleKind` replaces Ruma's in those
  call sites.
- **Unknown push actions are a 400**, not stored as custom actions (Ruma would accept any
  string). Synapse refuses them too; it is the only way a client can tell an action is
  unsupported.
- **`hs-push` does not depend on `hs-room`**: the room side is a trait (`EventSource`)
  implemented in `hs-cli`, like the appservice pump, so the dependency graph is unchanged.
- **The first sight of a stale room head is not pushed** (60 s grace). The alternative was one
  spurious push per room after every upgrade.
- **Badge = total `notify` count across rooms and threads** (Synapse's default, not
  group-by-room).
- **`PusherStore::set_pusher` takes the registering device** (a breaking change to this crate's
  trait; the two `hs-cli` call sites pass `None`).

### Shared dependencies added

None new to the workspace. `hs-push` now uses `bytes`, `prometheus-client` and (dev) `tower`,
all already in `[workspace.dependencies]`.

## Session 1: the `/sync` push-rules seam, and checking the default ruleset against the spec

**Starting point.** `hs-push` already existed from earlier phase-0/1 work (the rules engine on
`ruma::push`, `CachedRulesetStore`, `CountsStore`, HTTP pushers with retry/backoff, the
`/pushrules`/`/pushers` HTTP surface) — 28 tests green, `cargo clippy -p hs-push` clean. What did
not exist: any way for `/sync` (track 05, `hs-user`) to actually learn a user's push rules or
notification counts. Confirmed directly in `hs-user`'s own code before touching anything:

- `crates/hs-user/src/sync/mod.rs`'s module doc, "Not implemented in this pass": *"Unread
  notification counts: is included... shaped correctly..., with both counts hard-zero until
  track 10 (push) lands."* Its module doc's first line literally says `"unread notification
  counts (zero -- track 10 has not landed)"`.
- The same file (lines ~536-539, before this session) hardcodes
  `"unread_notifications": {"highlight_count": 0, "notification_count": 0}` and
  `"unread_thread_notifications": {}` for every joined room, unconditionally.
- `m.push_rules` never appears anywhere in that file at all — confirming this session's assigned
  bug (`docs/status/14-test-and-conformance.md`'s track-10 item, and Complement's "no pushrules
  found in sync response" on every subtest).

This session's job, per the ownership split (`crates/hs-user` is track 05's; I own `hs-push`
only): make `hs-push` expose the seam correctly, and write down *exactly* what track 05 needs to
add, rather than reach across and edit `hs-user` myself.

### Done

**1. `RulesetStore` learned a change-seq, mirroring `hs_user::store::UserStore`'s own
`account_data_seq`/`changed_seq` convention exactly** (`crates/hs-push/src/rulesets.rs`):

- `RulesetStore::set_ruleset` now returns `Result<u64, StoreError>` — the new change-seq value the
  write landed at (previously `Result<(), StoreError>`).
- `RulesetStore::changed_seq(&self, user_id) -> Result<u64, StoreError>` (new): `0` for a user who
  has never written a custom ruleset (the server-default ruleset never changes on its own),
  otherwise whatever the most recent `set_ruleset` call returned.
- `memory::InMemoryRulesetStore`: rows are now `{ruleset, changed_seq}`; `set_ruleset` increments
  under the existing write lock.
- `tables::TablesRulesetStore`: added a raw (non-`TypedKeyspace`) `hs_push.ruleset_seq` keyspace
  and uses `hs_kv`'s `atomic_add` inside the same transaction that writes the ruleset row —
  byte-for-byte the same pattern `hs_user.account_data_counter` uses in
  `crates/hs-user/src/store/tables.rs` (`put_global_account_data`/`latest_account_data_seq`), for
  the identical reason: one atomic counter per user, safe under concurrent writers via
  serializable-transaction retry, not a special lock-free primitive.

**2. `CachedRulesetStore::account_data_for_sync`** (new, `crates/hs-push/src/rulesets.rs`): the
one call `/sync` needs.

```rust
pub struct PushRulesForSync {
    pub content: serde_json::Value, // {"global": {...}}, exactly the m.push_rules content
    pub changed_seq: u64,
}

pub async fn account_data_for_sync(&self, user_id: &UserId) -> Result<PushRulesForSync, StoreError>;
```

Always returns a value — a user with no stored ruleset still has an effective one (the server
default) and a well-defined `changed_seq` (`0`). The content is built by the new free function
`rulesets::account_data_content(&Ruleset) -> serde_json::Value`, which is just `{"global":
ruleset}` — `ruma::push::Ruleset`'s own `Serialize` impl already emits the wire field names
(`content`, `override`, `room`, `sender`, `underride`) verbatim (verified: ruma-common 0.18.0's
`push.rs`, the version this crate's `Cargo.lock` actually resolves to per
`cargo tree -p hs-push -i ruma-common` — a second, newer `ruma-common` 0.20.0 is also in the lock
file for some other crate's transitive dependency, not this one), so no reshaping was needed; this
is also exactly what `GET /pushrules/`'s handler already returns
(`crate::routes::pushrules::get_pushrules_all`).

**3. Checked the default ruleset against the spec, and against Synapse.** Compared
`ruma::push::Ruleset::server_default` (what `rulesets::default_ruleset` calls) against:

- **The spec** (`refs/matrix-spec/content/client-server-api/modules/push.md`, "Predefined Rules",
  v1.18): as of MSC4210 (spec v1.17, changelog note in that same file: *"the legacy default push
  rules that looked for mentions in the body of the event were removed"*), the normative default
  list is ten override rules (`.m.rule.master`, `.m.rule.suppress_notices`,
  `.m.rule.invite_for_me`, `.m.rule.member_event`, `.m.rule.is_user_mention`,
  `.m.rule.is_room_mention`, `.m.rule.tombstone`, `.m.rule.reaction`, `.m.rule.room.server_acl`,
  `.m.rule.suppress_edits`, in that priority order), **no** default content rules (the old
  `.m.rule.contains_user_name` was retired by MSC4210, not replaced), no default room/sender
  rules, and five underride rules (`.m.rule.call`, `.m.rule.encrypted_room_one_to_one`,
  `.m.rule.room_one_to_one`, `.m.rule.message`, `.m.rule.encrypted`, in that order).
  `ruma::push::Ruleset::server_default` (`ruma-common-0.18.0/src/push/predefined.rs`) matches this
  **exactly** — same rules, same order, same actions, right down to the wire encoding of implied
  values (`Tweak::Highlight(HighlightTweakValue::Yes)` serializes as the bare
  `{"set_tweak":"highlight"}` the spec's own JSON examples use, not `{"set_tweak":"highlight",
  "value":true}` — checked in `ruma-common-0.18.0/src/push/action.rs`'s own
  `serialize_highlight_tweak` test). **No fix needed here**; found one stale doc comment instead
  (see "Decisions made"). This claim is now a regression test, not just a one-time read:
  `rulesets::tests::default_ruleset_matches_the_spec_predefined_rule_list` pins the exact rule-id
  list and order for override/content/room/sender/underride, plus `.m.rule.master`'s
  disabled-by-default / everything-else-enabled-by-default invariant.
- **Synapse** (`refs/synapse/rust/src/push/base_rules.rs`, behavior read only, not copied —
  AGPL-3.0): Synapse's `BASE_APPEND_OVERRIDE_RULES`/`BASE_APPEND_CONTENT_RULES` still ship the
  three MSC4210-retired rules (`.m.rule.contains_display_name`, `.m.rule.contains_user_name`,
  `.m.rule.roomnotif`) for backward compatibility with older clients that still string-match
  `content.body`, plus three unstable/vendor-prefixed rules this project's own brief explicitly
  names as owned but not yet implemented: MSC3664's `.im.nheko.msc3664.reply` (needs a
  "related-event" condition — the referenced event's sender — that `ruma::push::PushCondition`
  does not have any variant for at all; would need a new condition kind plus event-relation
  lookup in `crate::context`/`crate::engine`, a real feature, not a one-line add), MSC4028's
  `.org.matrix.msc4028.encrypted_event` (an override rule that is easy to *write* — plain
  `event_match(type, m.room.encrypted)` → `notify`, no new condition kind needed — but whose
  purpose (waking a client via push so it can replenish one-time-keys, MSC4028's actual concern)
  I did not want to half-implement without the OTK-count plumbing it exists for), and the
  `unstable-msc3930` poll rules for MSC3381 (gated behind a Ruma Cargo feature this crate's
  `Cargo.toml` does not currently enable). **Not implemented this session** — flagged under "Next"
  rather than guessed at, per this track's own risk note ("a server serving a subtly wrong
  ruleset makes every client notify wrongly" applies just as much to a rushed *addition* as to a
  wrong default).

**4. Notification counts: the store-side contract was already correct and tested** (nothing to
fix in `crates/hs-push/src/counts.rs` itself — `CountsStore::get_room_counts`/
`record_notification`/`reset` and both backends already had contract tests). What was missing
was the same thing as push rules: nobody outside this crate calls any of it yet, and `hs-user`
hardcodes zero. Counts do **not** need a `changed_seq`/token seam the way push rules do — they are
naturally scoped to a room already in the sync response (per `counts.rs`'s own module doc: "a
direct keyed lookup per room, never a scan"), so the seam is just "call `get_room_counts` for
every room `/sync` is about to emit" (see "Interfaces provided" below). Confirmed
`context.rs`/`state.rs`'s own doc comments: the reason counts are never populated in a real
`hs serve` process is that `hs_room::protocol::RoomUpdate::push_evaluation_inputs` is still
`Vec<()>` (track 04's placeholder) — a pre-existing, already-documented cross-track dependency,
not something this session introduced or can close from `hs-push` alone.

**5. Tests**: 28 → 31 (all in `crates/hs-push`):
- `rulesets::tests::default_ruleset_matches_the_spec_predefined_rule_list` (new)
- `rulesets::tests::account_data_for_sync_tracks_the_change_seq` (new)
- `rulesets::tests::account_data_content_has_exactly_the_client_facing_global_field` (new)

### In progress

Nothing left mid-flight; this session's scope (the `/sync` seam plus the default-ruleset check) is
complete on the `hs-push` side. The seam is unconsumed until track 05 does its half (below).

### Next

In the brief's stated priority order, and per the gap this session found but did not close:

- **MSC3664 (reply notifications)**: needs a new `PushCondition`-shaped concept (`ruma::push`
  itself has no "related event's sender" condition) plus event-relation lookup added to
  `crate::context::PushEvaluationInput`/`crate::engine`. A real feature, not a default-ruleset
  tweak.
- **MSC4028 (notify on all encrypted events)**: straightforward to add as an override rule (no new
  condition kind), but its point is to help a client with critically low one-time-keys wake up and
  replenish them — implementing the rule without also plumbing something that decides *when* to
  surface it felt like guessing at product behavior neither the spec nor this project's own docs
  pin down yet. Left for whoever picks up the OTK-count-driven push story (adjacent to track 08's
  one-time-key claim work).
- **MSC3381 (polls)**: gated behind Ruma's `unstable-msc3930` Cargo feature, not currently enabled
  in this crate's `Cargo.toml`; enabling it and re-running the golden test would need to also
  extend the default-ruleset regression test's expected list.
- Wiring `hs_room::protocol::RoomUpdate::push_evaluation_inputs` for real (track 04) so
  `record_notification` is ever actually called in a running server — everything on this crate's
  side (`crate::context`, `CountsStore`) is ready and tested; this is purely the other side of an
  already-documented cross-track dependency.
- Once track 05 implements read receipts (`SyncToken::receipts_seq`, reserved but unused per that
  crate's own docs), it should call `CountsStore::reset` when a receipt advances past a notifying
  event — the call this crate's `counts.rs` module doc already names as the reset trigger.

### Blockers

None for this session's delivered scope. The counts *pipeline* (an event actually reaching
`record_notification`) is blocked on track 04's `push_evaluation_inputs`, as already documented in
this crate's own `context.rs`/`state.rs` before this session started — not a new blocker this
session discovered.

### Interfaces provided

**For track 05 (`hs-user`), the exact wiring `/sync` needs — this is the contract, not a
suggestion:**

#### `m.push_rules` account data

Call, once per `/sync` response, per user:

```rust
let push_rules = push_state.rulesets.account_data_for_sync(user_id).await
    .map_err(/* into whatever hs-user's own error type is */)?;
```

(`push_state.rulesets` is `Arc<hs_push::rulesets::CachedRulesetStore<hs_push::rulesets::tables::TablesRulesetStore<B>>>`
— the exact same `Arc` `crates/hs-cli/src/serve.rs`'s `build_session_mounts` already constructs
for `hs_push::state::PushState`. It needs to be handed to whatever constructs `hs-user`'s
`SessionHub`/`UserState` too — today `build_session_mounts` builds `user` and `push` side by side
in the same function but does not share anything between them; that function is `hs-cli`'s, not
mine or 05's alone, so wiring the actual `Arc` through will need a small change there in whichever
order tracks 05/14 pick up this seam.)

Then, mirroring `crates/hs-user/src/sync/mod.rs`'s *existing* `global_account_data`/
`latest_account_data_seq` pattern (lines ~494-513 as of this session) field-for-field:

- **Initial sync** (`is_initial == true`): always include
  `{"type": "m.push_rules", "content": push_rules.content}` in the global `account_data.events`
  list, regardless of `push_rules.changed_seq`.
- **Incremental sync**: include it only if `push_rules.changed_seq > baseline.push_rules_seq`
  (a **new** field you'll need to add to `SyncToken`, see below).
- Either way, when composing the outgoing token: `push_rules_seq:
  push_rules.changed_seq.max(baseline.push_rules_seq)` — the same `.max(...)`-against-baseline
  shape `new_account_data_seq` already uses at line ~511.
- **Long-poll wake condition** (`crate::sync::has_new_data`, `crates/hs-user/src/sync/mod.rs`
  around line 716): add `if push_state.rulesets.store().changed_seq(user_id).await.map_err(...)? >
  baseline.push_rules_seq { return Ok(true); }` alongside the existing
  `latest_account_data_seq` check at line 727 — a rule change should wake a blocked long-poll the
  same way any other account-data change does. (`store()` is
  `CachedRulesetStore::store`, already public, returns `&TablesRulesetStore<B>` — bypasses the
  cache, which is fine here since this is a cheap counter read, not the full ruleset.)

**`SyncToken` change needed** (`crates/hs-user/src/token.rs`): add `pub push_rules_seq: u64`,
following exactly the precedent that file's own doc comment records for adding `typing_seq`
(version 1 → 2): bump `const VERSION: u8` (currently `2`) to `3`, add the field to
`SyncToken::initial()`, extend `PAYLOAD_LEN` from `1 + 7 * 8` to `1 + 8 * 8`, and extend
`encode`/`decode`'s field list. A version bump invalidates old tokens outright
(`TokenError::UnsupportedVersion`), which that file's own doc says is fine here: every deployment
of this greenfield server restarts from a freshly built binary, so no long-lived client holds a
stale-version token across the change.

#### `unread_notifications`/`unread_thread_notifications`

No token/seq plumbing needed — call, per room, right where
`crates/hs-user/src/sync/mod.rs`'s hardcoded zero block is today (~line 536):

```rust
let counts = push_state.counts.get_room_counts(user_id, room_id).await
    .map_err(/* ... */)?;
let totals = counts.totals();
```

```json
"unread_notifications": {
    "highlight_count": totals.highlight_count,
    "notification_count": totals.notification_count
},
"unread_thread_notifications": /* counts.threads, one entry per thread root event id, each
    {"highlight_count": ..., "notification_count": ...} -- empty map if counts.threads is empty,
    exactly today's `{}` placeholder for a room with no thread activity */
```

`push_state.counts` is `Arc<dyn hs_push::counts::CountsStore>` — same object
`hs_push::state::PushState` already holds; same sharing note as above applies (needs to reach
whatever builds `hs-user`'s state too). This call is cheap and side-effect-free (a direct keyed
lookup, `counts.rs`'s own module doc), safe to call unconditionally for every room in every
response — no "did this change" gate needed the way push rules needs one, since the room is
already being emitted for its own reasons (new timeline events, membership, or state) and its
counts should always be current as of the moment the response is built.

**Existing, unchanged interfaces** (present before this session, still the contract):
- `hs_push::counts::CountsStore`: `get_room_counts`, `record_notification`, `reset` — see
  `counts.rs`'s own "One source of truth" note. `record_notification` is called once per event per
  local recipient by whatever consumes track 04's publish stream (not this crate's job to call it
  from `/sync`).
- `hs_push::routes::router`: the `/pushrules`, `/pushers` HTTP surface, already mounted by
  `hs-cli`.

### Interfaces needed

- **04 (room and events)**: a real `hs_room::protocol::RoomUpdate::push_evaluation_inputs` (today
  `Vec<()>`) so this crate's own evaluation loop (`crate::pushers`, once wired to consume the
  publish stream) ever calls `CountsStore::record_notification` in a running server. Already
  documented in `crate::context`'s module doc before this session.
- **05 (sync)**: everything under "Interfaces provided" above — this is the actual ask this
  session exists to make precise.
- **05 or hs-cli (whoever owns `build_session_mounts`)**: thread the same `Arc<CachedRulesetStore<...>>`
  and `Arc<dyn CountsStore>` `crates/hs-cli/src/serve.rs` already builds for `hs_push::state::PushState`
  into whatever constructs `hs-user`'s `SessionHub`/`UserState`, so both crates share one set of
  stores over one backend rather than opening the ruleset/counts keyspaces twice.

### Decisions made

- **`RulesetStore::set_ruleset`'s return type changed from `Result<(), StoreError>` to
  `Result<u64, StoreError>`** (the new change-seq). A breaking change to this crate's own trait,
  fully within `hs-push`'s ownership; the two call sites in `crate::routes::pushrules` needed no
  code change (the returned value is already discarded as a bare statement).
- **Push-rules change tracking is a per-user monotonic counter (`atomic_add` over one raw KV
  key), not a content hash or a timestamp** — mirrors `hs_user::store::tables`'s
  `account_data_counter` exactly (same primitive, same reasoning: cheap, monotonic, safe under
  concurrent writers via the existing transaction-retry machinery, no clock dependency).
- **A user who has never customized their ruleset has `changed_seq == 0` forever, not a
  seeded/materialized row.** Rejected the alternative of writing the default ruleset into storage
  the first time it's read (which would give every never-customized user an artificial "change" at
  first read/first sync): `0` cleanly means "never changed" for the overwhelming common case (most
  users never touch `/pushrules`), and correctly compares as "unchanged" against any baseline a
  client's token could carry once `push_rules_seq` starts at `0` for `SyncToken::initial()` too.
- **Counts need no seq/token seam**, unlike push rules — documented above under "Done" item 3.
  This was worth stating explicitly since the task framing ("fold them into the same interface if
  the same seam gap blocks them") suggested they might need one; they don't, because they're
  scoped per-room-already-in-the-response rather than a standalone top-level account-data event.
- **MSC3664/MSC4028/MSC3381 were not implemented this session**, despite being named in this
  track's brief. See "Next" for why each was left rather than guessed at: MSC3664 needs a new
  condition kind Ruma does not provide, MSC4028's rule is trivial to write but its purpose depends
  on OTK-count plumbing this session did not build, and MSC3381 needs a Cargo feature flip plus
  updating the new golden test. Recorded here rather than silently dropped, per this track's own
  brief naming them as owned.
- Fixed a stale doc comment on `rulesets::default_ruleset` that named `.m.rule.contains_user_name`
  as an example of a user-ID-dependent default rule — that rule was retired by MSC4210 and is not
  part of `ruma::push::Ruleset::server_default`'s actual output; replaced with a citation-backed
  comment (see "Done" item 3) instead of a wrong example.

### Shared dependencies added

None. This session used only what `hs-push` already depended on (`ruma`, `hs-kv`, `hs-tables`,
`serde_json`) — no `Cargo.toml` or root `[workspace.dependencies]` change.
