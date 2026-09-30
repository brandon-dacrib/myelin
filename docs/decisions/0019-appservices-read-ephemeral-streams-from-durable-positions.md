# 0019: Appservices are sent ephemeral data from server-wide streams and durable positions (2026-09-30)

Status: accepted (track 11, with additions to tracks 05 and 08's stores). Closes the
`docs/next-steps.md` gap "Appservice delivery carries events only".

## Context

A registration with `receive_ephemeral` (MSC2409), `org.matrix.msc3202` (MSC3202) and
`io.element.msc4190` asks for typing, receipts and presence, to-device messages (MSC4203
included), device-list changes and one-time-key counts in its transactions. mautrix bridges in
appservice-mode encryption cannot carry an encrypted message without them: mautrix-whatsapp
connected on 2026-09-25 asking for all of it and was sent events only (`docs/bridges/mautrix.md`).

Synapse's design (`handlers/appservice.py`, read for behaviour) is a durable stream position
per appservice and stream (`read_receipt`, `presence`, `to_device`, `device_list`), read from
its global streams on every notification, with typing read from memory and no position. This
server had no global stream for receipts, presence or to-device messages: receipts and presence
are per-room and per-user rows with a stamp (05, session 9), and to-device messages are a queue
per device (08). Typing is each replica's memory, kept in step across replicas by the wake batch
(decision 0018).

The alternatives were a hook on every write (lost across a crash and, in a cluster, per replica
-- `/sendToDevice` lands wherever the client sent it), or scanning the per-room and per-device
keyspaces on every tick (all receipts on the server, per tick).

## Decision

- **Three server-wide streams, appended in the write's own transaction.** `hs_user.receipt_stream`
  (one entry per `put_receipt`, carrying the receipt and its room), `hs_user.presence_stream`
  (one per `put_presence` whose stamp changed: a `last_active` refresh is nobody's news) and
  `hs_e2e.to_device_stream` (one per `send_to_device`, naming the device and its queue position).
  Each has `*_since(pos, limit)`, `*_head()` and `prune_*(below)` on its store trait
  (`hs_user::store::UserStore`, `hs_e2e::store::ToDeviceStore`). Device lists already had one.
- **A durable position per appservice and stream** (`hs_appservice.ephemeral_pos`), written in
  the same store transaction as the transaction body it accounts for
  (`AppserviceStore::enqueue_ephemeral`), exactly as the event pump writes a room's cursor with
  what it queued. A restart resends nothing and misses nothing that is in a stream.
- **A new appservice starts at each stream's head**, not at its beginning: a bridge registered
  today did not ask for last week's receipts. (Synapse starts at the beginning and fast-forwards
  receipts on the first delivery.)
- **The streams are pruned at and below the lowest position** of the appservices that want
  them, every tick, and kept empty when none does. Synapse never prunes; here a stream is a
  hand-off, not history.
- **Typing has no stream and no position**, as in Synapse. The session hub gained an ephemeral
  observer (`SessionHub::install_ephemeral_observer`), told of every typing, receipt and
  presence change it applies, its own clients' and other replicas' alike (through
  `apply_ephemeral`). `hs-cli` installs one that rings the ephemeral pump: for typing, the
  room, whose current typing set is sent once per tick; for the rest, only a doorbell that
  spares waiting for the 250 ms poll. A typing change while nobody pumps is lost and stale
  within seconds anyway.
- **The pump runs on the global shard's owner**, like the event pump, and reads typing from its
  own hub, which holds every replica's typing because the wake batch carries it whole. The
  other replicas drop the rooms their observer noted.
- **A to-device message pushed to an appservice stays in the device's queue** for that device's
  `/sync`, as in Synapse (its `get_messages_for_user_devices` for appservices deletes nothing).
  Two reasons, both found in what bridges actually do: a non-exclusive users namespace names a
  real person's account for double puppeting (every RFC 0017 instance registration does), and
  deleting their room keys on the way to the bridge would break their own clients; and a
  mautrix bridge in sync-mode encryption ignores what is pushed to it and reads the bot's
  `/sync`. The cost is a queue that grows for a bot device that never syncs; track 08's open
  question on to-device retention is where that is answered.
- **Interest is Synapse's**: typing and receipts for a room the appservice is interested in
  (room or alias namespace, or a current member of its), private receipts only for its own
  users; presence and device-list changes for its users or for anyone in a room it is
  interested in; to-device messages addressed to its users (its bot included). Key counts in
  every transaction for an MSC3202 appservice, for its bot, its users in the rooms the
  transaction names and its to-device recipients, computed when the transaction is queued.
- **Shapes are Synapse's**: `{"type":"m.typing","room_id":...,"content":{"user_ids":[...]}}`;
  `{"type":"m.receipt","room_id":...,"content":{event_id:{"m.read":{user:{"ts":...}}}}}`;
  `{"type":"m.presence","sender":user,"content":{presence,last_active_ago,currently_active,status_msg?}}`;
  a to-device entry is the event with `to_user_id` and `to_device_id` beside `type`, `sender`
  and `content` (what `mautrix-go`'s `event.Event` parses; the crate had nested it under
  `event`, which no bridge would have read).

## Consequences

- `hs_appservice_transactions_total{appservice,outcome}` and
  `hs_appservice_delivered_items_total{appservice,kind}` exist, and every delivered transaction
  logs what it carried at `info`.
- Three keyspaces and a counter keyspace were added to stores this track does not own
  (`hs-user`, `hs-e2e`), additively: an existing database opens them empty. The observer is one
  `OnceLock` on the hub with two call sites. Recorded here rather than as an RFC left for the
  owning tracks because the gap could not be closed without them and nobody else was in the
  tree.
- `docs/status/11-appservices-and-bridges.md` (the session at the top) has the verification.
