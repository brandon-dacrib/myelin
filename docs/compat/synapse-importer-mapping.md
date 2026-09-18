# Synapse importer: table mapping

Track 13 (`docs/workstreams/13-config-compat-and-migration.md`), for
`hs import synapse` (`PLAN.md` section 9.4). Source: Synapse's `main` and
`state` storage schemas at schema version 94
(`refs/synapse/synapse/storage/schema/{main,state}/full_schemas/72/full.sql.postgres`
plus the deltas after 72 — read for table and column names only, per the
brief; no Synapse code is copied, Synapse is AGPL-3.0). Target: this
project's store, as designed in `PLAN.md` section 6 (interning,
event/state storage, sync feeds) and section 6.7 (keyspace layout). Every
family here is read-only on the Synapse side: the importer never writes to
Synapse's database (`PLAN.md` section 9.4 step 5 — rollback is just
starting Synapse again).

## Copy order and transaction boundaries

The importer runs in five ordered stages. Each stage is its own set of
transactions against the target store (batched — one transaction per few
thousand rows, not one transaction for a whole table, so a 50-million-row
`events` table doesn't hold one open transaction for hours); stages are
ordered so that a row copied in stage *N* never references a short ID or
record that a later stage would create.

| Stage | Families | Why this order |
|---|---|---|
| 1. Global, static | Server signing keys, remote server keys, appservice registrations and state | No dependencies; other stages' interning (server names) wants these server rows to exist first for readable logs, though interning itself doesn't require it. |
| 2. Users and their static data | Users, password hashes, profiles, 3PIDs, filters, push rules, pushers | Establishes `user_sn` interning for every local user before device/token/room rows reference them. |
| 3. Sessions and keys | Devices, access/refresh tokens, device keys, cross-signing keys, key backups | References users from stage 2. |
| 4. Per-room data | Room directory entries, then per room: events, redactions, state (recomputed, not copied — see below), receipts, room account data | The bulk of the data and the bulk of the time; parallelized across rooms (each room is independent), sequential within a room (events must be persisted in an order the room actor's own invariants accept — topological order via `depth`/`stream_ordering`, not `event_id`). |
| 5. Cross-checks and finalization | State cross-check (below), search index rebuild, chain-cover index rebuild | Needs every room's events present first. |

Global account data (stage 2, alongside per-user rows) and room account
data (stage 4, alongside each room) are split because Synapse itself
splits them into two tables (`account_data` vs. `room_account_data`); nothing
else about the split is significant.

## Incremental watermarks

The importer is designed to run once for the bulk copy while Synapse keeps
serving traffic, then again for a short final delta after Synapse is
stopped (`PLAN.md` 9.4 step 4). Every family below that changes over time
carries a **watermark column** — the Synapse column the second pass
filters on (`WHERE <watermark column> > <last seen value>`) — recorded in
the importer's own progress table (global shard, one row per family per
room where applicable: `{family, room_id?, last_watermark, rows_copied,
completed_at}`). Families with no natural watermark (`users`, `profiles`,
`devices`, ...) are re-copied in full on the second pass — small tables
next to `events`, cheap to just redo, and Synapse doesn't stream-order
updates to them the way it does account data, receipts and events.

The master watermark for the "everything since the bulk copy started" delta
is Synapse's global `stream_ordering` counter (visible as the `events`
table's `stream_ordering` column and reused as the watermark for several
other streams below); the importer records the highest `stream_ordering` it
has seen at the moment the bulk copy finishes and the final delta re-reads
every watermarked family from there.

## Users, password hashes, profiles, 3PIDs

| Synapse table | Key columns | Our model | Notes |
|---|---|---|---|
| `users` | `name`, `password_hash`, `creation_ts`, `admin`, `is_guest`, `appservice_id`, `user_type`, `deactivated`, `shadow_banned`, `consent_*` | User record, global shard, interned to `user_sn` on first sight (`PLAN.md` 6.1) | `password_hash` copies verbatim (bcrypt is unchanged; the pepper comes from the *target*'s `auth.password.pepper`, translated from Synapse's `password_config.pepper` — see `docs/compat/synapse-config-table.md`). `appservice_id` becomes a reference into the appservice registry copied in stage 1. No watermark: re-copied in full (`users` has no stream column). |
| `profiles` | `user_id`, `displayname`, `avatar_url` | Folded into the user record | One-to-one with `users`; copied in the same stage-2 pass. |
| `user_threepids` | `user_id`, `medium`, `address`, `validated_at`, `added_at` | User record's 3PID list | No watermark; small table, re-copied in full. |
| `user_external_ids` | `user_id`, `auth_provider_id`, `external_id` | User record's external-identity list | Feeds the native `GET /users/lookup` admin route (`docs/compat/synapse-admin-routes.md`). |
| `erased_users` | `user_id` | User record's erasure flag | GDPR-style erasure state; must be copied before any event redaction pass touches that user's events. |

## Devices, tokens, keys, backups

| Synapse table | Key columns | Our model | Notes |
|---|---|---|---|
| `devices` | `user_id`, `device_id`, `display_name`, `last_seen`, `ip`, `user_agent`, `hidden` | User-shard device record | `hidden` devices (used internally by Synapse for cross-signing) are copied but flagged, not exposed over the client API, matching Synapse's own behavior. |
| `access_tokens` | `id`, `user_id`, `device_id`, `token`, `valid_until_ms`, `puppets_user_id` | Session record, user shard | **Tokens survive the cutover unchanged** (`PLAN.md` 9.4: "sessions survive the cutover") — the token string itself is copied verbatim so existing client sessions keep working without a re-login. `puppets_user_id` (appservice masquerading) maps to the native identity-assertion record. Watermark: `id` (monotonic), for the final delta (new logins between bulk copy and cutover). |
| `refresh_tokens` | `id`, `user_id`, `device_id`, `token`, `next_token_id`, `expiry_ts` | Session record's refresh-token chain | `next_token_id` self-reference requires refresh tokens to be copied in `id` order within a user so the chain resolves; watermark: `id`. |
| `e2e_device_keys_json` | `user_id`, `device_id`, `ts_added_ms`, `key_json` | Device key record | Copied verbatim (already Synapse's own canonical JSON of the signed key). |
| `e2e_one_time_keys_json`, `e2e_fallback_keys_json` | `user_id`, `device_id`, `key_id`, `key_json` | One-time / fallback key pool | Copied verbatim; watermark not needed (one-time keys are consumed, not updated — copy whatever remains at cutover). |
| `e2e_cross_signing_keys` | `user_id`, `keytype`, `keydata`, `stream_id` | Cross-signing key record | One row per key type (`master`, `self_signing`, `user_signing`); watermark: `stream_id`. |
| `e2e_cross_signing_signatures` | `user_id`, `key_id`, `target_user_id`, `target_device_id`, `signature` | Cross-signing signature record | Copied after both the signing and target keys exist. |
| `e2e_room_keys` | `user_id`, `room_id`, `session_id`, `version`, `first_message_index`, `forwarded_count`, `is_verified`, `session_data` | Key-backup session record, user shard | Copied verbatim (`session_data` is already client-encrypted; this server cannot and does not need to read it). |
| `e2e_room_keys_versions` | `user_id`, `version`, `algorithm`, `auth_data`, `deleted`, `etag` | Key-backup version record | Copied before `e2e_room_keys` (versions are the parent). |
| `dehydrated_devices` | `user_id`, `device_id`, `device_data` | Dehydrated device record | MSC3814, on the required MSC list (`PLAN.md` 10.4). |

## Account data, push rules, pushers, filters

| Synapse table | Key columns | Our model | Notes |
|---|---|---|---|
| `account_data` | `user_id`, `account_data_type`, `stream_id`, `content` | Global account data, user shard | Watermark: `stream_id`. |
| `room_account_data` | `user_id`, `room_id`, `account_data_type`, `stream_id`, `content` | Per-room account data, user shard | Watermark: `stream_id`; copied per room in stage 4 alongside that room's other per-room data. |
| `room_tags`, `room_tags_revisions` | `user_id`, `room_id`, `tag`, `content` / `stream_id` | Folded into room account data (`m.tag` is account data in the current spec; Synapse's separate tag tables predate that) | Watermark: `room_tags_revisions.stream_id`. |
| `push_rules` | `id`, `user_name`, `rule_id`, `priority_class`, `priority`, `conditions`, `actions` | Compiled push-rule record, user shard | `conditions`/`actions` are already spec-shaped JSON; copied verbatim, then compiled by `hs-push` the same way a freshly-set rule would be. |
| `push_rules_enable` | `id`, `user_name`, `rule_id`, `enabled` | Push-rule enabled flag | Joined onto the corresponding `push_rules` row during copy (our model does not keep them as two records). |
| `pushers` | `id`, `user_name`, `kind`, `app_id`, `app_display_name`, `device_display_name`, `pushkey`, `ts`, `lang`, `data`, `last_stream_ordering`, `last_success`, `failing_since` | Pusher record, user shard | `last_stream_ordering` re-based against this server's own sync feed positions rather than copied literally — see "Sync tokens" below. |
| `deleted_pushers` | `stream_id`, `app_id`, `pushkey`, `user_id` | Not imported | Tombstones for already-deleted pushers carry no useful state once the pusher itself is gone. |
| `user_filters` | `user_id`, `filter_id`, `filter_json` | Filter record, user shard | `filter_id` is re-issued by the target (filter IDs are locally scoped per spec, so clients re-fetch/re-create as needed — Synapse's own numbering is not preserved). |

## Room directory, appservice state

| Synapse table | Key columns | Our model | Notes |
|---|---|---|---|
| `rooms` | `room_id`, `is_public`, `creator`, `room_version` | Room record, global shard (interned to `room_sn`) | `room_version` selects the event-auth and redaction rules used while replaying that room's events (`PLAN.md` D4: all room versions 1–12 supported). |
| `room_aliases` | `room_alias`, `room_id`, `creator` | Room directory alias record | |
| `room_alias_servers` | `room_alias`, `server` | Alias-server hint list | Rarely populated even in Synapse; copied if present. |
| `application_services_state` | `as_id`, `state`, `read_receipt_stream_id`, `presence_stream_id`, `to_device_stream_id`, `device_list_stream_id` | Appservice registry entry's delivery cursor | The four stream-id columns become the appservice sender's initial per-stream cursors, so delivery resumes from where Synapse left off rather than replaying from the start. |
| `application_services_txns` | `as_id`, `txn_id`, `event_ids` | Appservice transaction history | Only the highest `txn_id` per appservice matters (next transaction ID to use); full history is not imported. |

## Signing keys

| Synapse table | Key columns | Our model | Notes |
|---|---|---|---|
| `server_signature_keys` | `server_name`, `key_id`, `from_server`, `ts_added_ms`, `verify_key`, `ts_valid_until_ms` | Remote-server key cache, global shard | This server's *own* signing key does not come from this table — see `signing_key_path` in `docs/compat/synapse-config-table.md` — but the cache of *other* servers' keys this Synapse has already fetched is valuable to carry over so federation doesn't have to re-fetch everything on day one. |
| `server_keys_json` | `server_name`, `key_id`, `from_server`, `ts_added_ms`, `ts_valid_until_ms`, `key_json` | Same cache, raw JSON form | Newer Synapse versions store the full signed key response here instead of just the verify key; import whichever of the two tables the source schema version populates (94 has both; `key_json` is authoritative when present). |

## Events, state, receipts

This is the largest family by row count and the one with real algorithmic
weight, because state is not copied — it is **recomputed**.

| Synapse table | Key columns | Our model | Notes |
|---|---|---|---|
| `events` | `event_id`, `room_id`, `type`, `state_key`, `sender`, `content` (in `event_json`, see below), `depth`, `origin_server_ts`, `stream_ordering`, `outlier`, `rejection_reason` | Event header + body (`PLAN.md` 6.2: `event/{event_sn}`), interned (`room_sn`, `type_id`, `sender_sn`, `state_key_id`) | Copied in `depth`-then-`stream_ordering` order per room (topological, matching how the room actor expects to persist history) rather than `event_id` order. `outlier` and `rejection_reason` copy straight across as header flags. |
| `event_json` | `event_id`, `json`, `format_version` | The event's canonical JSON bytes | This is the actual event body; `events` (above) is Synapse's denormalized index over it. Both are read together per event during copy. |
| `event_edges` | `event_id`, `prev_event_id` | `prev_events` edges, interned | |
| `event_auth` | `event_id`, `auth_id` | `auth_events` edges, interned | Feeds the chain-cover index build in stage 5 (`PLAN.md` 6.4), rebuilt fresh rather than copying Synapse's own auth-chain tables (`event_auth_chains`, `event_auth_chain_links`), which are an implementation detail of Synapse's own (different) auth-chain algorithm. |
| `event_forward_extremities`, `event_backward_extremities` | `event_id`, `room_id` | Forward/backward extremity sets | Recomputed from the copied DAG rather than trusted verbatim — cheap to verify (a forward extremity is just an event with no children among copied events) and this is exactly the kind of small derived state that should never silently diverge from the source of truth. |
| `redactions` | `event_id`, `redacts`, `have_censored` | Redaction record | Applied during event copy, not as a separate pass: an event copied after its redacting event already has the redaction applied, matching how a live server processes them. |
| `rejections` | `event_id`, `reason` | Rejected-event marker | Rejected events are still copied (they're part of the DAG other events may reference) but marked rejected, never fed into state resolution. |
| `partial_state_events`, `partial_state_rooms` | `event_id` / `room_id` | Partial-state (faster-join) markers | If a room is still partial-state in Synapse at import time, it stays partial-state in the target and un-partials the same way a live faster join does (backfilling missing state in the background) — not eagerly resolved during import. |
| `state_groups`, `state_groups_state`, `state_group_edges` (separate `state` schema) | `id`/`room_id`/`event_id`; `state_group`/`type`/`state_key`/`event_id`; `state_group`/`prev_state_group` | **Not copied.** Source input to the cross-check only (below). | Synapse's state-group delta-chain representation (`PLAN.md` 6.3 candidate A) is exactly the design this project moved away from; the target computes its own state representation (candidate C, or whichever the Phase 0 bake-off selects) from the event DAG via the same state resolution algorithm a live server would use when receiving these events, room version by room version. |
| `event_to_state_groups` | `event_id`, `state_group` | **Not copied as data — used as the cross-check oracle.** | See "State cross-check" below. |
| `current_state_events` | `event_id`, `room_id`, `type`, `state_key`, `membership` | Room's current state map | Recomputed as the natural result of replaying all of a room's events through state resolution, then compared against this table (see below), not copied directly. |
| `room_memberships` | `event_id`, `user_id`, `sender`, `room_id`, `membership`, `forgotten`, `display_name`, `avatar_url` | Membership index, room shard | Denormalized from state events during replay rather than copied; `forgotten` (the per-user "I left and don't want this room in my list" flag) is the one column here with no state-event equivalent, copied separately onto the room-membership record. |
| `receipts_linearized` | `stream_id`, `room_id`, `receipt_type`, `user_id`, `event_id`, `data`, `thread_id` | Receipt record, room and user shards | Watermark: `stream_id`. `receipts_graph` (the unlinearized DAG form) is not imported — it exists in Synapse only to compute `receipts_linearized`, which is what every consumer actually reads. |
| `local_current_membership` | `room_id`, `user_id`, `event_id`, `membership` | Denormalized index of each local user's current membership | Rebuilt from the recomputed current state rather than copied, same reasoning as `current_state_events`. |

### State cross-check

`PLAN.md` 9.4 step 2 is explicit that this is mandatory: *"State is
recomputed by our engine from the events and cross-checked against
Synapse's `event_to_state_groups` mapping for every event; a mismatch is a
bug report, not a silent difference."* Procedure, run per room as the last
part of stage 4 (before that room is marked imported):

1. Replay the room's copied events in DAG order through this server's own
   event-authorization and state-resolution code (the same code path a
   live server uses for inbound federation events), producing this
   server's state map at every event.
2. For every event `e`, compute the *set* of `(type, state_key) -> event_id`
   pairs implied by the state our engine attached to `e`.
3. Independently, resolve Synapse's `event_to_state_groups` mapping for `e`
   to its state group, walk `state_group_edges` to the full delta chain,
   and materialize the same `(type, state_key) -> event_id` set from
   `state_groups_state`.
4. Compare the two sets. Equal: `e` passes. Not equal: record a
   **state mismatch** — `{room_id, event_id, our_state, synapse_state,
   diff}` — into the migration report (`docs/status/13-config-compat-and-migration.md`
   links the current importer-mapping status; the rehearsal tooling owned
   jointly with track 14 surfaces these) and continue (a mismatch in one
   room must not abort the whole import; operators triage the report).
5. A room with zero mismatches across every event is marked
   state-verified. The cutover procedure (`PLAN.md` 9.4 step 4) refuses to
   complete while any room has unresolved mismatches, unless explicitly
   overridden by an operator who has reviewed the report.

This is deliberately expensive (it is, in effect, running two independent
state-resolution implementations over the same history and diffing them)
and deliberately run once per room during import rather than continuously:
it is the same cross-implementation property test idea as WS2's oracle
comparison (`PLAN.md` section 11), applied to real production history
instead of synthetic DAGs, which is a far stronger signal that the new
state engine is correct on the actual rooms this deployment cares about.

### Sync tokens

Synapse's `stream_ordering` (global) and the various per-stream watermarks
above do not translate to this server's per-room, no-global-sequence sync
positions (`PLAN.md` 6.6). Rather than attempt a token translation, the
importer records the import cutover's Synapse `stream_ordering` per
client, and `hs`'s sync handler accepts a still-valid Synapse-style sync
token exactly once post-cutover, treating it as "give me everything since
the import point" — computed from each room's first imported event after
the recorded cutover — rather than resolving it token-for-token
(`PLAN.md` 9.4, final paragraph). This is why `pushers.last_stream_ordering`
above is re-based rather than copied literally: it is translated through
the same cutover-point mapping, not carried across as a raw integer that
means something different in the two systems.

## Media

| Synapse table | Key columns | Our model | Notes |
|---|---|---|---|
| `local_media_repository` | `media_id`, `media_type`, `media_length`, `created_ts`, `upload_name`, `user_id`, `quarantined_by`, `safe_from_quarantine` | Media metadata record, global shard (`PLAN.md` 6.7) | Bytes: either bulk-copied from Synapse's `local_content` directory into the target's object-store backend, or the existing directory is mounted through the local `object_store` backend with a layout adapter and copied lazily on first access (`PLAN.md` 9.4 step 3; the lazy-vs-eager choice is an operator flag, not a hardcoded policy — large single-node deployments on the same host default to lazy/mount, clustered deployments moving to real object storage default to eager bulk copy). |
| `local_media_repository_thumbnails` | `media_id`, `thumbnail_width`, `thumbnail_height`, `thumbnail_type`, `thumbnail_method`, `thumbnail_length` | Thumbnail metadata | Bytes copied or mounted alongside the parent media; if a requested thumbnail size doesn't match `media.thumbnail_sizes` on the target (`docs/compat/synapse-config-table.md`), it is left importable-on-demand rather than regenerated eagerly for every historical upload. |
| `remote_media_cache`, `remote_media_cache_thumbnails` | `media_origin`, `media_id`, ... | Remote-media cache entries | Copied on the same lazy/eager policy as local media; this is a cache, so a cold miss after cutover is correct-but-slower behavior, not a correctness bug — mismatches here are never part of the differential test harness's pass/fail criteria. |
| `local_media_repository_url_cache` | `url`, `media_id`, `download_ts`, `expires_ts` | URL-preview cache entry | Low value; not imported (a cold URL-preview cache repopulates itself on first request). |

## What the importer never touches

Background-update bookkeeping (`background_updates`), Synapse's own cache
invalidation streams (`cache_invalidation_stream_by_instance`,
`current_state_delta_stream`), presence (`presence`, `presence_stream` —
presence is intentionally not carried across; it is transient, ephemeral
state that a freshly started server should rebuild from scratch, not
history), monthly-active-user tracking (`monthly_active_users` — see
`docs/compat/synapse-config-table.md`'s R-MAU), search indexes
(`event_search` — rebuilt by `hs-search`'s own indexer in stage 5, not
copied, since the target's `tantivy` index format is unrelated to
Postgres full-text search), and anything under the "coordinating workers"
family in `docs/compat/synapse-config-table.md` (R-WORKER: no
target-side equivalent to import into).
