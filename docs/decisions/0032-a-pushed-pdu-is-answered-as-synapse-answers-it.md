# 0032: A pushed PDU is answered as Synapse answers it, and a remote join carries its profile keys

Date: 2026-10-05. Track 06 (federation), branch `agent/fed-wave3`.

## Context

`PUT /_matrix/federation/v1/send/{txnId}` answers each PDU with `{}` or `{"error": ...}`.
Until now this server answered an error for a PDU it verified but could not place because what it
stands on could not be obtained: prev events the sender would not supply
(`/get_missing_events`, `/backfill` and the `/state_ids` fallback all gave up), or auth events
that could not be fetched or judged. Synapse never does: it checks a pushed PDU's signatures,
stages it, answers `{}`, and processes it after answering; one that then fails is dropped and the
sender is never told. Sytest's `send_event` and Complement's `MustSendTransaction` fail on any
per-PDU error, so tests where Synapse drops the event (Complement's `TestCorruptedAuthChain`, whose
received event cites an auth chain the sender withholds a link of) failed here only for the
answer.

Separately, a user who joins a room through another server and later joins it again from here
(once the room is resident) got no second event: the second join's content was identical to the
first, and an identical membership is idempotent. On Synapse the second join *is* a new event,
because Synapse's remote join puts `displayname` and `avatar_url` in the content even when they are
unset (`null`), and its local join only the ones that are set. Sytest's "Guest users are kicked
from guest_access rooms on revocation of guest_access over federation" joins a remote user twice
and waits for the second join to show the room in an incremental `/sync`; here it waited forever
whenever the room's newest events reached the user's server before Sytest took its sync position.

## Decision

1. A PDU received over `/send` that verified but cannot be placed because its missing prev
   events or auth events could not be obtained is **dropped and answered `{}`**, logged at `info`
   ("dropped a PDU received over federation: what it stands on could not be obtained", with the
   reason) and counted in `hs_federation_pdus_dropped_total{reason="missing_ancestors" |
   "missing_auth_events"}` (`hs_federation::inbound`). A later event citing it fetches it again.
   An event that authorization rejected is answered `{}` as before; a PDU that does not parse,
   verify, or that the room's server ACL refuses is still answered with an error.
2. A join made through another server (`hs-cli` `remote_join`) carries `displayname` and
   `avatar_url` as `null` when the user has none, as Synapse's does, so a later local join with
   an unset avatar is a new event.

## Consequences

The sending server no longer learns, per PDU, that this server could not place an event; it did
nothing with that answer anyway (a per-PDU error is final and only logged, by Synapse and by
`hs_federation::sender`). Operators see the drops in the log and the counter. Clients of a remote
user's membership may see `"avatar_url": null`, which Synapse has always sent.
