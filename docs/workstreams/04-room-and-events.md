# 04. Room and events

Wave 1.5, starts week 4 on the frozen KV and model interfaces with a stubbed state API. Owns the unit of consistency.

**Expert profile.** Homeserver internals (event creation, DAG maintenance, extremities, membership rules, relations, redactions, retention), actor design, performance.

**Mission.** The room actor: serialize all writes to a room, authorize against hot in-memory state, persist events and state in one transaction, maintain the timeline and indexes, and publish position updates that sync, push, appservices and federation consume. See `PLAN.md` sections 5.3, 6.2 and 6.6.

**Owns.** `hs-room`: event creation and validation pipeline, persistence with batching, forward and backward extremities, timeline and pagination tokens, membership state machine (join rules including restricted and knock), invites and third-party invites, redactions, relations and threads with bundled aggregations, retention and purge, room upgrades and tombstones, spaces hierarchy and room summaries, aliases and directory, room stats, per-room receipts, `unsigned` population, MSC4140 delayed events, MSC4354 sticky events, `state_after` (MSC4222) inputs.

**Provides.** Week 8: the room actor command protocol (create event, persist inbound events, query state and timeline, membership operations) and the publish stream: `(room_sn, room_pos, changed state keys, membership deltas, push-evaluation inputs)`.

**Consumes.** 01 (store), 02 (`hs-state`), 03 (ownership and forwarding), 07 (`Requester`), 14 (harness).

**Day-one work (design, before code at week 4).** Protocol document for the actor and the publish stream; the membership state machine as a table with tests per room version; the persistence transaction layout under 01's size limits; the hot-state cache and eviction policy.

**Phase 0 deliverables (behind the `phase0` flag).** Room creation with every preset and room version; send and state events; membership transitions; redactions; relations aggregation; timeline pagination with room-position tokens; in-memory hot state with eviction and reload; publish stream to a fake subscriber; property tests for the membership machine.

**Phase 1 and 2 deliverables.** `/messages`, `/context`, `/members`, `/joined_members`, `/state`, `/event`, `/relations`, `/threads`, `/hierarchy`, `/room_summary`, `/timestamp_to_event`, `/upgrade`, `/aliases`, directory endpoints, retention and purge, room stats; inbound federation persistence hooks with 06 (soft-fail, rejections, backfill at negative positions, partial state); delayed and sticky events; bundled aggregations; large-room fan-out-on-read threshold with 05.

**Definition of done.** Complement `csapi` room tests green; differential tests against Synapse on the scripted room workload clean; membership property tests per room version; events-per-second-per-core target from `PLAN.md` section 13 met; fuzz targets for event content and relations.

**References.** Spec client-server sections on rooms, events, relations, redactions, spaces; `refs/synapse/synapse/handlers/message.py`, `room.py`, `room_member.py`, `relations.py`, `pagination.py` and `refs/synapse/synapse/storage/controllers/persist_events.py` (behavior and the batching idea; AGPL, read only); `refs/palpo/crates/server/src/room/`; `refs/conduit/conduit/src/service/rooms/`.

**Open questions to settle first.** Exactly what the publish summary carries so 05 avoids store reads; where push actions are computed (the room owner, per the plan) and how they reach 10; positions for backfilled events; how a room above the local-member threshold signals fan-out-on-read.

**Risks.** The actor grows into a monolith; keep timeline, membership, relations and retention as modules with their own APIs and tests.
