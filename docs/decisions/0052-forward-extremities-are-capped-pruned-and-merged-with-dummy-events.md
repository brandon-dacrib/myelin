# 0052: Forward extremities are capped, pruned and merged with dummy events, as Synapse does

Date: 2026-10-16. Track 04 (`hs-room`), with `hs-config`, `hs-cli`, `hs-compat` and `hs-federation`
wiring.

## Context

A room's current state is the state resolution of the states at its forward extremities. On the
demo (v0.3.25, 2026-10-16) two federated rooms, `#ping` (`!ping12Z19...`) and codestorm.net's
`!c10y-...`, carried many forward extremities that nothing merged: their extremities changed
about once a minute (gaps, soft fails, `/state_ids` fallbacks), and each change cost a resolution
of 15-22 s, cold, under the room's lock. `agent/hub-members-step` made repeated reads of one set
of extremities cheap (`ResolutionCache`); it did not make the set smaller.

This server did nothing to keep it small. Synapse does three things:

1. every event it creates cites at most ten extremities, the deepest
   (`get_prev_events_for_room`);
2. after it persists events, it drops extremities that do not change the resolved state, when
   they are old or their server has moved on (`_prune_extremities`);
3. every five minutes it sends an `org.matrix.dummy_event` into up to five rooms with more than
   `dummy_events_threshold` (10) extremities, as a joined local user
   (`_send_dummy_events_to_fill_extremities`).

This server cited *every* extremity in every event it made, and in `make_join` templates.
Synapse refuses an event with more than 20 `prev_events` ("Too many prev_events",
`_check_event_sanity`), so in a room of 21 forks every event of this server would have been
refused there.

## Decision

- **Capped `prev_events`.** Events this server builds (`RoomActor::send_event`,
  `build_membership_event`, and so dummy events) cite at most ten extremities, the deepest and
  then the latest in the timeline (`hs_room::actor::extremities::MAX_PREV_EVENTS`); `make_join`
  templates do the same (`hs_federation::join::MAX_TEMPLATE_PREV_EVENTS`). Not configurable.
- **Pruning.** After every room-actor operation that persisted events (`RoomActorHandle`'s
  `after_operation`), `RoomActor::maintain_extremities` resolves the current state and drops the
  extremities whose state differs from the resolved one, when the resolved state *is* some
  extremity's state, and only if every one dropped is: not just persisted; another server's (or
  a dummy event of ours standing only on other servers' events); and either over a day old and
  more than 100 events behind the newest just persisted, or from a server whose event was just
  persisted and more than 20 behind it. All or none, as Synapse. "Same state" is "the state
  store's diff of the two roots is empty": roots are not canonical (a resolution answering an
  existing state is a different root). The current state is identical before and after. No
  `bump_rewrites`: a read copy that keeps a pruned extremity resolves the same state, and re-reads
  the set with its next event. Thresholds are Synapse's and not configurable.
- **Dummy events.** `RoomRegistry::spawn_extremity_maintainer` runs once a minute over the
  resident rooms this replica owns: sets the gauges, warns of rooms past 50 extremities, and sends
  a dummy event into at most five rooms with more than `server.rooms.dummy_events_threshold`
  extremities, the most forked first, at most once per `server.rooms.dummy_events_interval` per
  room. The sender is the first joined local user, in user-id order, the auth rules allow; a room
  with none is left alone for an hour. A room busy with another operation is skipped, not waited
  for. Dummy events are published and federated like any event, and hidden from every client
  read (`RoomActor::event_visible_to`), as Synapse's `filter_events_for_client`.
- **Configuration** (hot, under `server.rooms`): `dummy_events_threshold`, default 10 (Synapse's
  default and key, now translated by `hs-compat`), `0` turns dummy events off;
  `dummy_events_interval`, default 5 minutes.
- **Observability**: `hs_room_forward_extremities_rooms{le}` (resident rooms with at most `le`
  extremities, Synapse's `synapse_forward_extremities` shape, replaced each pass),
  `hs_room_forward_extremities_max`, `hs_room_dummy_events_total{outcome}` (`sent`,
  `no_local_sender`, `failed`), `hs_room_extremities_pruned_total`; a `warn` naming a room past
  50 extremities (once until it recovers), an `info` for each pruning and each pass that sent
  dummy events; the slow-load `warn` carries the room's extremity count.

## Consequences

- A forked room converges within a few intervals of being noticed, without anyone writing to it,
  and the resolutions its extremity changes cost go with it.
- Clients never see dummy events; other servers and appservices receive them, as from Synapse.
- Inbound events with more than 20 `prev_events` or 10 `auth_events` are still accepted here
  (Synapse refuses them); that check is track 06's to add.
