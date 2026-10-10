# 0042: 2026-10-10: a federation destination this server shares no room with is state, not a relationship, and may be forgotten

Status: accepted (track 06; touches 15's admin API, 16's Federation pages, 13's `federation` configuration section).

## The problem

Every server this one ever sent to is remembered: its outbound queue, its backoff and retry
state, its catch-up mark and its cached signing keys (`hs-federation`'s outbound store, the
client's destination records, the key cache). Nothing removed a record. After a user joined and
left one large room (`#matrix:matrix.org` has users from thousands of servers), the Federation
page listed thousands of servers, most of them "failing" for ever: gone servers, servers that
refused us, servers that will never be written to again because no room has users from both.
The operator could not tell those from a server that matters, the sender kept retrying them on
their backoff ceiling, and Synapse offers no answer either (it keeps `destinations` rows for
ever and has no setting).

## What was chosen

1. **The rooms decide.** A destination this server shares a room with (a room where both have a
   joined member) is a relationship: it will be written to the moment anyone speaks, and its
   queue is the users' own. A destination sharing none is only state: nothing is sent to it until
   a room brings the two together again, and when one does the sender learns it again from
   nothing (first attempt, keys fetched, backoff from zero). So a destination sharing no room may
   be forgotten without loss, and one sharing a room may not, unless the operator insists.
   `Destination.shared_rooms_count` carries the number on every admin row (`null` when the server
   cannot read its rooms), and the list filters on `shares_room`.
2. **Forgetting drops everything held for it.** `DELETE /federation/destinations/{server_name}`
   (`federation.destinations.forget`, `admin:write`) drops the queue (unsent events are lost, the
   answer counts them), the backoff and retry state, the catch-up mark and the cached keys. It is
   `409 conflict` while a room is shared, naming how many, unless `force=true`; `404` for a
   server never tried. Audited; the event stream carries `federation.destination_forgotten`.
3. **A prune applies one rule to every destination.** `POST /federation/destinations/prune`
   (`dry_run` for the same report without forgetting) forgets every destination sharing no room
   with nothing queued (`unused`) and, with `failing_for`, every one failing at least that long
   whose queued events are only for rooms this server has since left (`failing`: leaving a large
   room leaves one row per server that was in it, most of them never answering, each with the
   leave queued). Kept, with the reason: `shares_rooms`, `queued_for_current_rooms`,
   `queued_not_failing`, `failing_recently`, `active_recently`. The report counts both sides by
   reason and names the first 50 of each.
4. **A sweep does it on its own, conservatively.** `hs serve` runs the prune every hour (first
   five minutes after start, once the sender has resumed its queues) with
   `federation.forget_unused_destinations_after` (default `1w`, hot, `0` off) as both the idle
   time and the failing time: a destination sharing no room goes once it has had nothing queued
   and nothing happen for a week, or has failed for a week with a queue only for rooms this server
   left. The administrator's prune uses zero idle time (it is asked for now). The sweep logs one
   line when it forgets something and keeps no report; the admin API's dry run is how an operator
   sees what would go now.
5. **The interface shows and explains it.** The Federation page has a "No shared room" filter,
   a Shared rooms column, Forget on each row and on the destination's page (with the warning and
   the force when a room is shared, and the server's own refusal when the list could not say),
   and a prune panel that previews before forgetting, states the sweep's setting in words with a
   link to change it, and shows both sides of the report with the reasons in plain words.

## What was not chosen

- **Forgetting a failing destination on time alone** (Synapse-style "failing for N days, drop
  it"): a server down for a month that still shares a room comes back and needs its queue; the
  rooms, not the clock, say whether a destination matters. The clock is used only together with
  "no shared room" (`unused`) or "queue only for rooms we left" (`failing`).
- **Dropping the record silently when the last shared room is left:** the leave itself has to
  reach the destination first, and a rejoin a minute later would start from nothing. The week
  of grace and the operator's hand cover both.
- **Keeping the sweep's last report in the store:** one more table for a line the log already
  has; the dry run answers "what would go now" more usefully than "what went an hour ago".

## Consequences

- Metric `hs_federation_destinations_forgotten_total{reason, by}` (`reason`: `administrator`,
  `unused`, `failing`; `by`: `administrator`, `sweep`). Logs at `INFO`: one line per destination
  forgotten (destination, reason, by, what was dropped), one per administrator's forget or
  prune, one per sweep that forgot something (with the first eight names).
- Shared-room counts are read from the room registry (`hs-cli`'s `RegistryRoomSharing`), one
  read per room, cached in the admin source for a short while; a cluster replica reads another
  replica's rooms from the store.
- OpenAPI 0.1.14.
