# 15. Admin API and modules: status

Track brief: `docs/workstreams/15-admin-api-and-modules.md`. Owner crates: `hs-admin`, `hs-modules`, `hs-identity`, `hs-http` (shared with 07 and 14).

Last updated: 2026-09-28 (the Rooms area, 23/23; Users' devices-and-identity half; and the Cluster area, 6/6; all below); before that 2026-09-27 (media, registration tokens and server notices, reports, tasks and statistics, below; before that the bridge
offering operations); before that 2026-09-26 (three public recovery operations); 2026-09-25 (additive schema change for the bridges wizard); the session log that follows is from 2026-09-19 (session 6).

> **2026-09-28, served for real: Rooms 23/23.** `tools/admin_api_coverage.py` counts **132 of
> 158** operations with a real handler, with Users' half below (Rooms was 6/23).
>
> - **Handlers** (`crates/hs-admin/src/rooms.rs`, over a new `RoomContentSource` trait,
>   `AdminState::with_room_content`, `InMemoryRoomContent` for tests): `rooms.state.list`,
>   `rooms.messages.list` (newest first, cursor), `rooms.events.get`, `rooms.events.at` (MSC3030
>   over what the server holds), `rooms.events.context`, `events.get`; `rooms.aliases.list/add/
>   remove`; `rooms.hierarchy.get`; `rooms.join` (a local user joins; invited through the most
>   powerful local member when the join rules refuse, as Synapse does); `rooms.forward_extremities.
>   list/delete` (keeps the newest by depth); `rooms.media.list` and `rooms.media.quarantine`;
>   `rooms.purge_history` and `rooms.delete`. Purge, delete and media quarantine answer `202` with
>   a task (`TaskRegistry::spawn`): progress per step, cancellable between batches, a result on
>   success. `rooms.delete` is Synapse's: optionally block, optionally a new room (creator, name,
>   message) the local members are moved into, every local member leaves, aliases removed and the
>   room unpublished, then purge; afterwards the room is not found and cannot be joined.
> - **Scopes: decision 0013** (`docs/decisions/0013-moderators-read-rooms-and-media-not-messages.md`,
>   settles RFC 0004 against the document for queue item 2e). `admin:read` satisfies every
>   `*:read`; room and media metadata is `moderation:read`; message content (`rooms.messages.list`,
>   `rooms.events.*`, `events.get`) stays `admin:read` and every successful read writes an audit
>   entry `rooms.content.read` with the path read.
> - **Observability**: every write audited (`rooms.aliases.add/remove`, `rooms.join`,
>   `rooms.forward_extremities.delete`, `rooms.media.quarantine`, `rooms.purge_history`,
>   `rooms.delete`) and published (`room.alias_added/removed`, `room.member_joined`,
>   `room.forward_extremities_pruned`, `room.purge_started`/`room.history_purged`,
>   `room.delete_started`/`room.deleted`, `room.media_quarantine_started`/`room.media_quarantined`);
>   `/metrics` has `hs_admin_room_operations_total{operation,outcome}` and
>   `hs_admin_room_operation_duration_seconds{operation}` for purges and deletions
>   (`crates/hs-cli/src/room_admin.rs`).
> - **Cluster**: every `/api/v1/rooms/{room_id}/...` path is forwarded to the room's owner by the
>   room shard gate, so reads, writes and the tasks run there. **Gap**: `GET /api/v1/events/{id}`
>   has no room in its path, so on a non-owning replica it loads the room locally to read it; the
>   room page uses the room-scoped `rooms.events.get` instead.
> - **Tests**: `crates/hs-admin/src/rooms/tests.rs` (25: scopes, audit of content reads,
>   validation, tasks, idempotency); `crates/hs-room/src/admin/content/tests.rs` (6, see status
>   04); end to end through `hs serve` in `crates/hs-cli/tests/admin_rooms.rs` (4: the reads,
>   aliases resolving for clients, join, hierarchy, media and quarantine; a purge gone from the
>   members' own `/messages`; a deletion after which nobody can join; and, through the real `hs`
>   binary across a restart, a fork reported and trimmed while the room goes on).

> **2026-09-28, served for real: Users' devices-and-identity half, 13 operations** (branch
> `users-devices-identity`; Users is 27/41). `tools/admin_api_coverage.py` now counts
> **115 of 158**.
>
> - **Handlers** (`crates/hs-admin/src/user_identity.rs`): `users.devices.get`,
>   `users.devices.update` (`display_name` required; `null` or blank clears it),
>   `users.devices.bulk_delete` (all or nothing, deduplicated, at most 1000, `Idempotency-Key`),
>   `users.threepids.list/add/remove` (email lower-cased and checked, phone reduced to its
>   digits, `/medium` and `/address` refusals, `409` when another account has it,
>   `Idempotency-Key` on add), `users.external_ids.list/add/remove` (provider without `/`,
>   subject kept exact), `users.experimental_features.get/put` (only Synapse's `msc3575`,
>   `msc3881`, `msc4222`; `PUT` merges, as Synapse's does; `GET` reports every known flag),
>   `users.account_data.list` (global account data, event type to content; the paging
>   parameters are accepted and not needed) and `users.pushers.list` (paged). Two new seams on
>   `AdminState`: `UserIdentitySource` (`with_user_identity`, implemented by
>   `hs_auth::admin_directory::AuthStoreUserDirectory`) and `UserDataSource` (`with_user_data`,
>   implemented by `hs_cli::user_data::StoredUserData` over `hs-user`'s and `hs-push`'s
>   stores). Both are separate modules so this did not touch `sources.rs`' `UserDirectory`.
> - **Observability**: every write is audited (the operation id as the action, the user as the
>   target, before and after in `changes` for a rename, a 3PID, an external id and each
>   feature flag) and published (`user.device_updated`, `user.devices_deleted`,
>   `user.threepid_added`, `user.threepid_removed`, `user.external_id_added`,
>   `user.external_id_removed`, `user.experimental_features_changed`), and logged at `info`
>   with the actor.
> - **Tests**: handler tests with a fake source (503 unwired, 403 for a read token, the rename
>   body, bulk all-or-nothing and replay, normalisation, feature merge and refusal, events);
>   the real-binary test `crates/hs-cli/tests/admin_user_identity.rs`.
>

> **2026-09-28, RFC 0020 server side (by track 13, queue item 2b).** `config.update` now puts
> back a hidden secret inside a list entry instead of storing the entry without it:
> `SecretPaths::restore_echoed_secrets` (`crates/hs-admin/src/config_schema.rs`) runs before
> `strip_echoed_secrets`. Inside an array `{"$secret": true}` takes the value stored at its own
> pointer; `{"$secret": true, "$from": "<pointer>"}` takes the one at the pointer it names (a
> secret setting of the same section that holds a value, or `400 validation-failed` on the
> placeholder's pointer). It logs how many it kept. Tests: `config_schema::tests::a_secret_*`,
> `a_from_that_names_no_stored_secret_is_refused`,
> `router::tests::a_secret_inside_a_list_survives_saving_the_list`; against the real binary,
> `web/e2e-real/configuration.spec.ts`. The `openapi.yaml` description of `ConfigSettingInfo`
> mentions it; no operation changed.

> **2026-09-28, served for real: Cluster 6/6** (branch `agent/cluster-admin`, rebuilt on main
> from the superseded `worktree-agent-ae592ed29bb65b973`, whose cluster pieces it replaces).
> `tools/admin_api_coverage.py` now counts **102 of 158** operations with a real handler.
>
> - **Handlers** (`crates/hs-admin/src/cluster.rs`): `cluster.replicas.list`, `.get`, `.drain`,
>   `.undrain` and `cluster.shards.list` over a new `ClusterSource` trait
>   (`AdminState::with_cluster`; `InMemoryCluster` for tests). Lists page with the usual
>   cursor; `kind` is validated (`/kind`). Drain and undrain honour `Idempotency-Key`, are
>   audited (`cluster.replicas.drain` / `.undrain`, with the status change) and published
>   (`cluster.replica_draining` / `cluster.replica_undrained`) only when they change something.
>   A drain starts a `cluster.replicas.drain` task (`follow_drain`: progress in shards handed
>   off, succeeds when the replica owns none, fails after 15 minutes without undoing the drain)
>   and answers the replica `draining` with `drain_task_id`; undrain cancels that task. `409`
>   when no other active replica would take the shards (always, for a single node).
> - **The real source** (`crates/hs-cli/src/cluster_admin.rs`, wired in `serve.rs`): replicas
>   from the registry rows, shard counts and owners from the shard rows, drain requests from the
>   store; single-node mode is one `single-node` replica owning the whole layout. A drain is a
>   row in the shared store that the drained replica honours itself (decision 0012,
>   `docs/decisions/0012-a-drain-is-a-request-in-the-shared-store.md`), so any replica can take
>   the request, undrain works, and a drained replica stays drained across a restart and stays
>   listed while stopped. `hs-cluster` (track 03's crate, edited with this work): `DrainRequest`,
>   `ClusterStore::{request_drain, update_drain, withdraw_drain, drain_request,
>   list_drain_requests}`, `KvOwnership` reading its drain request every heartbeat
>   (`is_admin_drained`), and a fix for a heartbeat racing deregistration (`tick_lock`).
> - **Observability**: logs on both sides (`an administrator asked a replica to drain`, `an
>   administrator asked this replica to drain`, `a replica drained: it owns no shards`, `a drain
>   was refused`, `this replica's drain was withdrawn`), `hs_cluster_admin_drains_total{event}`
>   (`requested`, `completed`, `timed_out`, `undrained`) and
>   `hs_cluster_admin_drain_duration_seconds` on `/metrics`, the audit entries and events above.
> - **Contract** (`openapi.yaml`, additive): `Replica` gained `joining`, `this_replica`,
>   `mesh_addr`, `version`, `zone`, `last_heartbeat_at`, `drain_requested_at`,
>   `drain_requested_by`, `drain_task_id` and a `required` list; `Shard` gained `epoch`, a
>   nullable `owner`, enums for `kind` and `state` and a `required` list; `kind` on
>   `GET /cluster/shards` is an enum; drain's `409` is `Conflict`; descriptions say what drain
>   does. `web/src/api/schema.d.ts` regenerated.
> - **Verified**: `cargo test -p hs-admin` (7 new handler tests in `cluster::tests`),
>   `cargo test -p hs-cluster` (an administrator's drain handing every shard to the peer and
>   back; a drained replica staying deregistered), `cargo test -p hs-cli --lib cluster_admin`
>   (3), and `cargo test -p hs-cli --test cluster_admin`, through the real `hs` binary: single
>   node (one replica owning every shard, the `409` with its reason, the metrics exported), and
>   two `hs serve` processes on one PostgreSQL (drain B through A, the task succeeding, B
>   `drained` by its own account while `/health/ready` stays 200, A owning all 137 shards, A
>   refused, the audit entry and counters, B stopped and still listed drained, B restarted and
>   still drained, undrained through B and taking shards back). The PostgreSQL test skips with a
>   message when no server is reachable (`HS_CLUSTER_TEST_POSTGRES_DSN`, default
>   `postgres://postgres:hspg@127.0.0.1:5439/postgres`; the command is in the file's docs).
> - **Left**: a drain started by `SIGTERM` still deregisters and stops (that is shutdown); the
>   operator (track 12) does not yet drain a pod through the API before evicting it;
>   `cluster.get`'s `replica_count` counts owning replicas, so a drained replica is not in it
>   (the Cluster page counts from the replica list); the two-pod run on the real cluster is a
>   desktop item (`docs/status/03-cluster.md`).

> **2026-09-27, served for real: RegistrationTokens 5/5 and ServerNotices 2/2.**
> `tools/admin_api_coverage.py` now counts **78 of 158** operations with a real handler.
>
> - **Registration tokens** (`crates/hs-admin/src/registration_tokens.rs`): the five handlers
>   (`admin:read` / `admin:write`), `RegistrationTokenSource`, `InMemoryRegistrationTokens`,
>   `AdminState::with_registration_tokens`. Create validates the token (1-64 of
>   `A-Za-z0-9._~-`, `/token`), generates one of `length` (default 16; ignored beside a given
>   token), refuses a past `expires_at` and a negative `uses_allowed` (each with its pointer),
>   `409` on a duplicate, honours `Idempotency-Key`. `PATCH` tells absent from `null` (null
>   removes a limit). Every write is audited (`registration_tokens.create/update/delete`, with
>   the before/after of each changed limit) and published (`registration_token.*`). The real
>   source is `hs_auth::registration_tokens::AdminRegistrationTokens` over the durable
>   `TablesRegistrationTokens` (`hs_auth.registration_tokens` keyspace), the same store `/register`
>   checks. Client-server side (track 07's crate, edited here with the work): with open
>   registration off, a registration token is the one way in -- `/register` offers
>   `[m.login.registration_token]` while any token is usable and stays `403` when none is;
>   passing the stage reserves one of the token's places for that UIA session (`pending`) until
>   the account is created (`completed`) or the session expires, so a one-use token cannot be
>   presented by two people at once; `GET /_matrix/client/v1/register/m.login.registration_token/validity`
>   is mounted (new `hs_auth::routes::v1_router`). This is invite-by-link user creation.
> - **Server notices** (`crates/hs-admin/src/server_notices.rs`): `server_notices.send`
>   (`moderation:write`, idempotent, recipients checked before anything is sent) and
>   `server_notices.list` (`moderation:read`, newest first), `ServerNoticeSource`,
>   `InMemoryServerNotices`, `AdminState::with_server_notices`; audited as
>   `server_notices.send`, published as `server_notice.sent`. The real source is
>   `crates/hs-cli/src/server_notices.rs`: sent as `@_server:<server name>` (created on first
>   use, no password; an account of that name this code did not create is never sent as), one
>   room per recipient (private, named "Server Notices", `users_default: -10`, recipient
>   invited, `m.server_notice` added to their `m.tag`), reused and re-invited to after a leave,
>   history and the recipient-to-room map in `hs_admin.server_notice*` keyspaces. `hs-room`
>   refuses a recipient's rejection of that invitation with `403
>   M_CANNOT_LEAVE_SERVER_NOTICE_ROOM` (`RoomRegistry::install_server_notices_user`; the room is
>   recognised by its creator). `hs-compat` shims `POST /_synapse/admin/v1/send_server_notice`
>   and `PUT .../send_server_notice/{txnId}` (a transaction id becomes the idempotency key; a
>   non-administrator's token is `403 M_FORBIDDEN`), which is what Complement calls.
> - **Contract** (`openapi.yaml`, additive): `RegistrationToken` gained `valid` and a
>   `required` list; `RegistrationTokenCreate` documents the alphabet and lost the `default`
>   on `length` (the generator made a defaulted field required); `ServerNotice` gained `id`,
>   `sender`, `type`, `content`, `room_ids` and a `required` list; both `content` objects are
>   `additionalProperties: true`. `web/src/api/schema.d.ts` regenerated. `hs-admin-mock`'s
>   fixtures follow the new shapes.
> - **Verified**: `cargo test -p hs-admin` (new `tests/tokens_and_notices.rs`, 7 tests),
>   `cargo test -p hs-auth` (store and registration tests, 229 in the lib),
>   `cargo test -p hs-room` (the leave refusal), `cargo test -p hs-compat` (the shim),
>   `cargo test -p hs-cli --test invites_and_notices` (2 end-to-end tests through the real
>   server: a one-use invite token registering one person on a closed server, then a second
>   after the limit is raised; and `TestServerNotices` step for step -- 403 for a
>   non-administrator, the invite from `@_server`, the refused rejection, join with the notice
>   in the timeline and the `m.server_notice` tag, leave, re-invite to the same room, the
>   transaction id idempotent). Clippy clean with `-D warnings` on all five crates.
>   `cargo test -p hs-cli` in full: every suite green (141 lib tests plus every integration
>   test binary).
> - **Where this stopped** (branch `agent/registration-tokens-server-notices`, not merged):
>   done and verified as above, web included (`npm run check` green, 224 Vitest tests;
>   `npm run test:e2e` 31 passed on mocks). Not run: the web pages against a real `hs serve`
>   through Playwright (`web/e2e-real/` has no spec for them yet; next step is one modelled on
>   `bridge-offerings.spec.ts`: create a token, open the invite link signed out, register,
>   send a notice), and Complement's `TestServerNotices` itself (run it on the laptop and
>   update `docs/status/complement-csapi-results.txt`). Nothing is partly written.
> - **Not done**: the server-notices localpart is fixed (`_server`), not a setting; notices to
>   "everyone" or to a room (the IA's wish) are not an operation; a durability-across-restart
>   test of the token keyspace goes through the store (reopen over the same backend), not a
>   restarted process; not yet measured under Complement itself.

> **2026-09-27, branch `agent/reports-tasks-stats`: Reports 4/4, Tasks 3/3, Statistics 4/4
> served for real** (`tools/admin_api_coverage.py`: **81 of 158**, from 71).
>
> - **Reports** (`crates/hs-admin/src/reports.rs`): wire shapes, `ReportSource`, the four
>   handlers (`moderation:read`/`moderation:write`; resolve and delete audited as
>   `reports.resolve`/`reports.delete`, published as `report.resolved`/`report.deleted`;
>   resolve is idempotent by key and `409` on a closed report). `resolution: no_action`
>   *dismisses*, anything else *resolves*. Contract changes (additive): `Report.kind` gained
>   `room`; `Report` gained `resolved_at`, `resolved_by` and `event` (the reported event as the
>   room holds it now, on `GET /reports/{id}` only). Report ids come from a monotonic ULID
>   generator (`reports::new_report_id`), because plain ULIDs minted in one millisecond do not
>   sort by time.
> - **The client-server reporting endpoints** (`crates/hs-room/src/routes/report.rs`):
>   `POST /rooms/{roomId}/report/{eventId}` (`reason?`, `score?` in -100..=0; 404 for an event
>   the reporter cannot see), `POST /rooms/{roomId}/report` and `POST /users/{userId}/report`
>   (`reason` required; 404 for an unknown room or local user; a remote user is accepted). They
>   write a durable store (`crates/hs-room/src/reports.rs`, keyspace `room_reports`) the
>   `RoomRegistry` opens (`RoomRegistry::reports()`); `hs_room::reports::RoomReports` is the
>   `ReportSource`. The Overview's `pending_reports_count` is now counted (`ServerOverview::
>   set_reports`).
> - **Tasks** (`crates/hs-admin/src/tasks.rs`): `TaskRegistry` (spawn with progress and
>   cancellation, `record_finished`, best-effort cancel across replicas, `recover_interrupted`
>   at startup marks this runner's unfinished tasks failed and prunes finished ones older than
>   30 days, `task.changed` events), `TaskStore` with an in-memory store; the durable one is
>   `crates/hs-cli/src/tasks.rs` (`hs_admin.tasks`). What reports into it today:
>   `appservices.replay` (its answer is now a task `GET /tasks/{id}` can find) and the startup
>   content-scan sweep (`media.resume_scans`, when it found anything). **For the Media agent and
>   rooms.delete/purge_history later:** `state.tasks.spawn(action, resource, actor, |ctx| async
>   { ... ctx.progress(..).await; Ok(json!(..)) })` and answer `202` with the returned task.
> - **Statistics** (`crates/hs-admin/src/statistics.rs`): `statistics.rooms` over the room
>   directory; `statistics.users_media` and `statistics.timeseries` over a `StatisticsSource`
>   (`crates/hs-cli/src/statistics.rs`). The metric vocabulary is `statistics::METRICS` and the
>   contract's `metric` enum: counters (`users.registered`, `media.uploaded`,
>   `media.uploaded_bytes`, `reports.received`) from record timestamps, gauges (the
>   `StatisticsOverview` field names) from samples the server takes every 15 minutes into
>   `hs_admin.stats_samples` (kept 400 days). Steps are epoch-aligned; at most 1000 points.
>   Media usage reads `hs_media::usage::local_uploads` (new file in `hs-media`).
>   `OverviewSource` gained `statistics_now` (default: `statistics`), which the sampler uses so
>   it neither reads nor refreshes the Overview's one-minute cache.
>
> **Verified:** `cargo test -p hs-admin` (198 lib + contract), `cargo test -p hs-room` (incl.
> `tests/reports.rs`), `cargo test -p hs-media --lib`, `cargo test -p hs-cli --lib`,
> `cargo test -p hs-cli --test reports_tasks_statistics` (the real binary, twice: reports filed
> over HTTP, resolved, audited; a replay kept as a task; statistics from real records; all of
> it still there after a restart), `cargo test -p hs-cli --test e2e the_overview`; clippy
> `-D warnings` on hs-admin, hs-room, hs-media, hs-cli; `cargo fmt --all`.
>
> **Where this stopped** (the session was asked to wrap up before the interface work):
> - Done and verified: everything above (server side, contract, tests).
> - Not started: the web interface. `web/src/api/schema.d.ts` has **not** been regenerated
>   from the changed `openapi.yaml` (`cd web && npm run generate:client`); no Reports page
>   (`/reports` is still `PlaceholderPage`), no Tasks page, no Statistics page or Overview
>   sparklines; no MSW mocks for these operations (`web/src/mocks/handlers.ts`,
>   `web/src/mocks/data/`); `npm run check` not run. `hs-admin-mock`'s report fixtures do not
>   carry the new optional fields (`resolved_at`, `resolved_by`, `event`) or `kind: room`.
> - Next, in order: regenerate the client; `web/src/api/reports.ts`, `tasks.ts`,
>   `statistics.ts` hooks; Reports page (queue with status/kind filters, open first; detail
>   with the reported event, reporter, reason, score, and resolve/dismiss with a resolution
>   select and note: structured controls only, decision 0010); Tasks page (list with status
>   filter, detail, cancel); Statistics page (largest rooms, media by user, time-series
>   charts with the existing `Sparkline`) and the Overview's Activity sparklines; MSW handlers
>   whose shapes match the Rust types above; Vitest for each page; `npm run check`; update
>   `docs/status/16-management-web-interface.md`.
> - Not run: the full `cargo test -p hs-cli` (only the lib tests and the two e2e tests named
>   above), and the web checks.

> **2026-09-27, served for real: the Media area, 9 of 9.** `media.list`, `media.get`,
> `media.delete_one`, `media.quarantine`/`unquarantine`, `media.protect`/`unprotect`,
> `media.delete_bulk` and `media.purge_remote_cache` have real handlers in
> `crates/hs-admin/src/media.rs`, over a new `media::MediaSource` trait (list, get, delete,
> set_quarantined, set_protected; `InMemoryMediaSource` for tests) wired with
> `AdminState::with_media`. The real source is `hs_media::admin_source::RepositoryMediaSource`,
> wired in `hs serve`. Filtering, search (`q` over id, server, filename, uploader, type),
> sorting (`created_at` default descending, `last_accessed_at`, `size_bytes`, `media_id`; a bad
> value is a 400 naming `/sort`), the rule that protection and quarantine exclude each other
> (409 either way), and bulk selection live in the handlers, once for every source. Each
> mutation is audited under its operation id and published as `media.quarantined`,
> `media.unquarantined`, `media.protected`, `media.unprotected` or `media.deleted` (with a
> count and bytes for the bulk ones). The two Task operations run to completion in the request
> and answer `202` with the Task already `succeeded` (result: `deleted_count`,
> `deleted_bytes`, `skipped_protected`, `skipped_quarantined`, `failed`), a `Location` of
> `/api/v1/tasks/{id}`, and a `task.succeeded` event; there is no task store yet, so that
> `Location` does not resolve until the Tasks area lands. Contract changes, additive:
> `MediaItem.last_accessed_at` (nullable), `MediaItem`'s fields marked required (the server
> always sends all of them), `media.delete_bulk`'s `before` required (a bulk deletion never
> means "everything"; a missing one is a 400 on `/before`), descriptions on both bulk bodies
> and on `media.list`. `authorization_header`, `source_unavailable`, `parse_optional_json`,
> `idempotency_key`, `replay_response` and `record_mutation` in `router.rs` are now
> `pub(crate)` so an area can live in its own module. `tools/admin_api_coverage.py`: **80 of
> 158**. Verify: `cargo test -p hs-admin --lib media::` (12 tests) and `cargo test -p hs-cli
> --test e2e an_administrator_can_find_quarantine_protect_and_delete_uploaded_media`.

> **2026-09-26, additive, served for real (RFC 0017).** Ten operations under the `Bridges` tag:
> `bridge_deployments.target` (`GET /bridge-deployment-target`), `bridge_offerings.list/get/
> put/delete` (`/bridge-offerings`, `/bridge-offerings/{type}`, `DELETE ... ?remove_instances=`
> answering `409` while instances remain) and `bridge_instances.list/get/put/delete/files`
> (`/bridge-offerings/{type}/instances[/{user_id}[/files]]`, `_` for a shared type's one
> instance). Reads need `bridges:read`, writes `bridges:write` (the RFC's section 5 says
> `admin:read`/`admin:write`; the document is what the router enforces, and the RFC should be
> corrected to it). Schemas `BridgeDeploymentTarget`, `BridgeOffering`, `BridgeOfferingRequest`,
> `BridgeOfferingAccess`, `BridgeOfferingOptions`, `BridgeInstance`, `BridgeDeployment`,
> `BridgeInstanceFiles`; `BridgeType` gained `mode` (`per_user` or `shared`) and `deployable`.
> In this crate: `bridge_offerings::BridgeOfferingSource` with `InMemoryBridgeOfferings` for
> tests and the mock, `AdminState::with_bridge_offerings`, the handlers (each write audited and
> published), and `bridge_types` renders an instance (`InstanceSpec`, `InstanceRender`, the
> `io.myelin.bridge_instance` registration tag) as well as a registration. The real source is
> `hs_bridges::manager::BridgeManager`, wired in `hs serve` (track 11's status has what it does
> and does not do); `tools/admin_api_coverage.py` counts **71 of 158** operations with a real
> handler (Bridges 26 of 26). Track 16's contract notes from its side (unpaginated `{data}`
> lists, `image` out but `image_tag` in, no reason on `deployable: false`, no structured count
> on the 409) are in `docs/status/16-management-web-interface.md` and are not addressed yet.

> **2026-09-26, additive.** Three operations under a new `Recovery` tag, all public (`security:
> []`, rate-limited, `no-store`), the siblings of `setup.*`: `recovery.links.create` (`POST
> /recovery/links`, authenticated by a signature under the server's own signing key; what `hs
> recover` calls), `recovery.inspect` and `recovery.reset` (authenticated by the one-time token
> the link carries; what the recovery page calls). Schemas `RecoveryLinkRequest`, `RecoveryLink`,
> `RecoveryInspectRequest`, `RecoveryInspection`, `RecoveryAdministrator`, `RecoveryResetRequest`;
> the reset answers a `SetupSession`. In this crate: the models, `sources::RecoverySource` with
> `RecoveryError` (`NotSigned` and `BadToken` are both `401`, so a refusal does not say which;
> `Closed` is `409`), `InMemoryRecoverySource` for tests and the mock, `AdminState::with_recovery`,
> the three handlers with audit entries `recovery.link_issued` (actor: the key, as
> `signing-key:<key id>`) and `recovery.password_reset` (actor: the account) and events
> `recovery.link_issued` and `recovery.completed`; router tests cover the 503 when nothing is
> wired, refusal without issuance, the setup-link fallback, inspection, the field pointers, the
> single use and what the audit log does and does not carry. The real source is
> `hs_auth::recovery` (track 07's status has the design); the page is track 16's.

> **2026-09-25, additive.** `openapi.yaml` grew, without removing or renaming anything:
> `BridgeType` gained `description`, `category` (`messaging|social|irc|integrations`),
> `docs_url`, `port`, `renders_config` and `sign_in` (`{steps[], notes}`; `{bot}` in a step is
> the bridge bot's Matrix ID, substituted by the client); `BridgeTypeRenderResult` gained
> `config_yaml` (nullable: the bridge's own config for mautrix types); `AppService` gained
> `bridge_type` (nullable: the catalogue entry it was created from, read from the
> registration's `io.myelin.bridge_type` key). `BridgeTypeRenderRequest` is still free-form;
> the wizard sends `homeserverAddress`, `bridgeAddress` and `adminUser` besides what it sent
> before. `crates/hs-admin/src/model.rs` and `bridge_types.rs` match; the TypeScript client was
> regenerated (`web/src/api/schema.d.ts`). The contract test is path-level and unaffected. Session 2 carried the track through the design freeze plus every Phase 0 deliverable except the WebAssembly host (deferred by design; see "Decisions made"). Session 3 was a fix from integration review: `hs-admin-mock`'s read (`GET`) handlers were never actually gated by `require_auth` (only mutations were), so every collection and item read returned 200 regardless of `Authorization`. Session 4 turned the first slice of the **real** `hs-admin` router (not the mock) from `501` seams into real handlers behind the real auth plumbing: `me.get`, `server.get`, `server.health`, `users.list`, `users.get`. Session 5 added the write path — `users.lock`, `users.unlock`, `users.deactivate`, `users.reactivate`, `users.update` — each writing a real `AuditEntry` and publishing a real `Event`, plus real `Idempotency-Key`/`If-Match` handling and the real `audit_log.list`/`audit_log.get`/`events.stream` endpoints. Session 5 was interrupted by a session rate limit partway through its next slice (`users.create`/`users.lookup`/`users.availability`); the integration lead salvaged what had landed (the three new `UserDirectory` methods with `Unavailable`-by-default bodies, `UserCreateRequest`, the lookup match types, `AdminRoom`) before this session started. **Session 6 (this session)** finishes those three handlers, adds the `RoomDirectory` seam plus `rooms.list`/`rooms.get`/`rooms.block`/`rooms.unblock`/`rooms.make_admin` (no real backing — `hs-room` is another track's crate; see "Interfaces needed" for the contract), and turns `audit_log.export` into a real NDJSON endpoint. Server notices were not reached (see "What was left undone" below). See "Done" below for exactly which of the 142 operations are genuinely served now (**24**), and "Interfaces needed" for what's still missing from other tracks.

## Done

- **`docs/rfcs/0004-admin-api.md`**: the admin API design document. Was already complete from session 1 (resource model from every flow in `docs/workstreams/16-management-web-interface.md`, naming, pagination, filtering, errors, idempotency, scopes, versioning, audit log, event stream, generated clients, asset serving). Reviewed this session and left as-is; no gaps found.
- **`docs/rfcs/0005-routes-json-manifest.md`** (new): defines the `routes.json` manifest format `hs-http` emits, since track 14 (the consumer) has not started and no format existed. Covers the schema, how `hs_http::router::Builder` produces it by construction, and how the contract test uses it.
- **`crates/hs-admin/openapi/openapi.yaml`** (OpenAPI 3.1, generated by a throwaway Python script, not checked in): **120 paths, 142 operations, 87 schemas.** Covers every resource in the assignment: users and devices, rooms and moderation, media and quarantine, federation destinations and keys, reports, registration tokens, tasks, statistics, server notices, appservices and bridges (health, backlog, replay, registration export, bridge-type catalog and rendering), cluster status, migration status, reloadable configuration, the audit log, and the SSE event stream, plus `/me`, `/server`, `/server/health`, and the document's own `/openapi.yaml`/`.json` endpoints. Cursor pagination (`Page` envelope + `Limit`/`Cursor`/`IncludeTotal` params), RFC 9457 problem details (17 reusable `components.responses`, the full closed catalog from RFC 0004 section 3.5), `Idempotency-Key` on mutating `POST`s, `If-Match` on compare-and-set `PATCH`/`PUT`s, and the OAuth2 security scheme with all six scopes (`admin:read`, `admin:write`, `bridges:read`, `bridges:write`, `moderation:read`, `moderation:write`), one minimal scope per operation. **Validated clean with `npx @redocly/cli lint`: 0 errors, 3 warnings** (two catalog entries — `Forbidden`, `NotImplemented` — and the `Event` schema are documented but not directly `$ref`'d by name from any operation; left as-is since they document real, reachable outcomes and payload shapes). Also emits `crates/hs-admin/openapi/operations.json`, the Rust-consumable operation table (method, path, operation id, scope, idempotency) that `hs-admin`'s router is built from, guaranteeing the two never drift silently.
- **`hs-admin-mock`** (`cargo run -p hs-admin --bin hs-admin-mock`, default `127.0.0.1:8090`, override with `HS_ADMIN_MOCK_ADDR`): **usable now — track 16, this is ready.** A standalone axum server (not built on the router skeleton, so it can return realistic bodies instead of `501`) covering every resource category with working cursor pagination (`limit`/`cursor`/`include_total`/`q` free-text search), realistic seeded fixtures (5 users, 4 rooms, 4 media items, 3 federation destinations, 3 reports, 2 registration tokens, 3 tasks, 2 appservices, 3 bridge types, 3 replicas, 4 shards, 3 config sections, and more), a working `GET /api/v1/events` SSE stream (replay buffer honoring `Last-Event-ID`, a `stream.hello` frame, 15-second keepalives, a `stats.snapshot` event every 10 seconds, and real events on every mutation), and a fake login (`POST /api/v1/mock/login` with an optional `{"scopes": [...], "principal_id": "..."}` body, or just use the well-known dev token `mock-admin-token`, already scoped `admin:write`+`bridges:write`+`moderation:write`, no login call needed). Mutations write real audit entries (queryable at `GET /api/v1/audit-log`) and publish real SSE events, both via the actual `hs_admin::events::EventBus` and `hs_admin::audit::InMemoryAuditSink` from the library crate, not a second parallel implementation.
  - **Session 3 fix (integration review defect): every `GET` handler is now actually gated by authentication.** Session 2's `require_auth` was correct in isolation and was called at every mutation's ~30 call sites, but no read handler (`list_users`, `get_user`, `list_rooms`, `list_appservices`, ...) took a `HeaderMap` parameter at all, so the check was simply never reached for them — every collection and item `GET` returned `200` regardless of `Authorization`, including for a missing header or an unrecognized token. Fixed by moving authentication off individual handlers and onto an `axum::middleware::from_fn_with_state` `route_layer` (`require_auth_middleware` in `main.rs`) applied to every route except the three that must stay public (`GET /api/v1/openapi.yaml`, `GET /api/v1/openapi.json`, `POST /api/v1/mock/login`, split into their own `public` sub-router). This is structurally safer than the per-handler approach it replaces: a future handler can only skip authentication by being deliberately added to the `public` router, not by omission of a parameter. Also implements the scope-enforcement follow-up (see "Decisions made"): the same middleware looks up each matched route's required scope from `hs_admin::operations::load()` (the same table the real router enforces against) and answers `403 insufficient-scope` for a token that lacks it, so track 16 can exercise that UI path too, not just the 401 one. Verified live (`curl`, both missing-header and wrong-token and insufficient-scope cases against a running `hs-admin-mock`) and with four new regression tests plus one scope test in `crates/hs-admin/src/bin/hs-admin-mock/main.rs`'s `auth_regression_tests` module (`collection_endpoint_rejects_missing_and_unknown_token`, `item_endpoint_rejects_missing_and_unknown_token`, `a_second_collection_and_item_endpoint_are_also_covered`, `public_endpoints_still_need_no_authentication`, `insufficient_scope_is_403_not_a_silent_200`), all passing (`cargo test -p hs-admin --bin hs-admin-mock`).
- **`crates/hs-http`** (shared HTTP conventions, `docs/rfcs/0005-routes-json-manifest.md`):
  - `error.rs`: `MatrixError`/`MatrixErrorCode`, the `{errcode, error, ...}` shape for `/_matrix`/`/_synapse`. Named constructors for the required cases (`unrecognized()` → 404 `M_UNRECOGNIZED`, `method_not_allowed()` → 405, `rate_limited(ms)` → 429 with `retry_after_ms` and a `Retry-After` header, `soft_logout()` → 401 `M_UNKNOWN_TOKEN` with `soft_logout: true`) plus ~10 more common errcodes and an `Other(String)` escape hatch.
  - `problem.rs`: `Problem`, the RFC 9457 shape for `/api/v1`, with a constructor per entry in RFC 0004's closed catalog and builder methods (`with_detail`, `with_retry_after_ms` which also sets the `Retry-After` header, `with_required_scope`, `with_header`, ...).
  - `body.rs`: `PermissiveJson<T>` (any/no `Content-Type`, empty body treated as `{}`, for `/_matrix`) and `StrictJson<T>` (`application/json` only or `415`, for `/api/v1`), both convertible to either error shape via `BodyParseError::{to_matrix_error, to_problem}`.
  - `router.rs`: `Builder<S>` — an `axum::Router` builder that records a `Route` (method, path, surface, operation id, auth kind, required scope, rate-limited) for every registration, `RouteManifest::{to_json_pretty, write_to_file}`, and `openapi_method_paths`/`assert_matches_openapi` for the contract check (accounts for the OpenAPI document's `servers[0].url` base path when comparing).
  - `cors.rs`: same-origin by default (empty allow-list), explicit origins or `*` otherwise, the headers RFC 0004 section 3.1 names.
  - `listener.rs`: `ListenerConfig::{Tcp, Tls, Unix}` and `serve()`. TLS is a hand-rolled `axum::serve::Listener` impl doing the accept-then-handshake loop over `rustls`/`tokio-rustls`; unix sockets use axum's built-in `UnixListener` support and remove a stale socket file before binding.
  - `ratelimit.rs`: `RateLimiter` trait (`async fn check(&self, key: &str) -> Decision`) plus `Unlimited` for tests/opt-out.
  - `time.rs`: RFC 3339 millisecond-precision UTC formatting/parsing (`2026-09-17T21:04:05.123Z`, never `+00:00`).
  - 20 unit tests, all passing; `cargo clippy -p hs-http --all-targets -- -D warnings` clean.
- **`crates/hs-admin`** (library; the router *skeleton*, not full handlers — those are Phase 1):
  - `model.rs`: the RFC 0004 section 5 common schemas (`Page<T>`, `Task`/`TaskStatus`, `Principal`, `AuditEntry`/`AuditRequest`/`AuditOutcome`/`AuditChange`, `Event`, `ResourceRef`, `Actor`) and `Scope` with the `satisfies()` partial order from D15.8 (`admin:write` satisfies everything; each `*:write` satisfies its own `*:read`).
  - `auth.rs`: the `TokenVerifier` trait 07 implements (`async fn verify(&self, bearer: &str) -> Result<Principal, AuthError>`), `AuthError` (only `Unavailable` becomes 503, everything else 401), `require_scope()` (the single enforcement point), `StaticVerifier` for tests.
  - `audit.rs`: the `AuditSink` trait (`append`, `get`, `query` against an `AuditFilter`) the storage track implements on `hs-tables`, and `InMemoryAuditSink` for tests and the mock.
  - `events.rs`: `EventBus` — a `tokio::sync::broadcast` channel plus a bounded replay ring buffer, with ids assigned from a monotonic `ulid::Generator` (plain `Ulid::new()` per event is *not* guaranteed to sort correctly against another one minted in the same millisecond, which broke the first version of the "behind the buffer" test; fixed by having the bus assign ids at publish time instead of trusting the caller's).
  - `operations.rs`: parses `openapi/operations.json` at compile time (`include_str!`) into `OperationDef`s.
  - `router.rs`: `build_router()` — for every non-`public` operation in the table, a route that extracts `Authorization`, verifies + checks scope through `auth::require_scope`, and answers `501 not-implemented` (RFC 0004 section 3.5) once authorized; `GET /openapi.yaml`/`.json` are wired directly with real handlers and excluded from the generic loop. Returns the route manifest alongside.
  - `assets.rs`: embeds `crates/hs-admin/web-dist-placeholder/` (checked-in, always present — **not** `web/dist` directly, which is gitignored and would break a clean checkout; see the comment in that placeholder's `index.html` for the one-line swap once a build pipeline runs `npm run build` before `cargo build`) via `rust-embed`, serves `/admin` and `/admin/{*path}` with `index.html` SPA fallback, `Cache-Control: immutable` for hashed assets and `no-store` for `index.html`.
  - `openapi.rs`: embeds `openapi/openapi.yaml` (`include_str!`) for the document endpoints.
  - `tests/contract.rs`: **the contract test** — builds the router, asserts its manifest's `(method, path)` set for the admin surface equals the OpenAPI document's (base-path-adjusted). Passing.
  - 33 unit tests + 2 contract tests, all passing; `cargo clippy -p hs-admin --all-targets -- -D warnings` clean.
- **`crates/hs-modules`**:
  - `hooks.rs`: the `ModuleHooks` trait, one or two representative methods per Synapse callback category (spam checker: `check_event_for_spam`, `user_may_invite`, `check_username_for_spam`; third-party rules: `check_event_allowed`; presence router: `get_interested_users`; account validity: `is_user_expired`; password auth provider: `check_password`; background-update controller: `background_update_guidance`; account data: `on_account_data_updated`; media repository: `check_media_for_spam`; ratelimit: `ratelimit_override`; federation: `should_federate_room`; add-extra-fields-to-unsigned: `extra_unsigned_fields`) — full Synapse callback parity (dozens more methods per category) is explicitly Phase 1/2 in the brief; the trait shape and protocol are the day-one deliverable, and the shape is additive (new methods later do not break this one).
  - `noop.rs`: `NoopHooks` (permissive on every hook) and `ModuleChain` (composes several `Arc<dyn ModuleHooks>` with Synapse-like veto semantics: first `Deny` wins for checks, notifications run on every module, `check_event_allowed` threads a possibly-rewritten event through the chain).
  - `callback.rs`: the versioned JSON HTTP-callback protocol (`CallbackRequest<T>`/`CallbackResponse<T>` envelopes, `protocol_version: "1"`, a `404` from the module means "doesn't implement this hook", a `409` means "unsupported protocol version"; `hook_names` constants so a module author in any language can implement the wire format without touching this crate).
  - `client.rs`: `HttpCallbackClient`, the reference implementation of `ModuleHooks` over that protocol; any error (unreachable module, timeout, bad response) falls back to the hook's permissive default rather than propagating, so one module outage cannot take the server down.
  - `tests/conformance.rs`: **module protocol conformance tests with sample modules** (definition of done, verbatim) — a spam checker and a password auth provider, each a tiny axum server implementing the wire protocol directly (not through this crate, proving the protocol is implementable without depending on it), exercised through `HttpCallbackClient` and through `ModuleChain`.
  - 6 unit tests + 3 conformance tests, all passing; `cargo clippy -p hs-modules --all-targets -- -D warnings` clean.
- **`docs/design/wasmtime-feasibility.md`**: the feasibility verdict — **feasible**, WIT interfaces map cleanly onto `ModuleHooks`, sandboxing (fuel/memory/epoch limits) gives a drop-in module the isolation an HTTP-callback module gets from the OS process boundary for free, recommend building it in Phase 1 behind a `wasm` cargo feature after a latency/pooling benchmark. **`wasmtime` was not added as a dependency and nothing was built against it**, per the instruction not to build it on this shared machine; see the document's section 0 for why (it would force every other track's `cargo check` to compile Cranelift too).

## In progress

Nothing left in this session's assigned scope. What remains is Phase 1/2 work per the brief (full per-resource handlers, generated clients, the WebAssembly host, native ports, the Synapse admin compat mapping) plus server notices (deferred this session for budget) and is not blocking any other track today.

## Next

- **Confirmed live this session**: `AdminState::with_users` is already wired by the integration lead (`GET /users` returns real `200` data against the running binary) and 07's real `UserDirectory` implementation compiles unchanged against the three new session-6 methods — but has not overridden any of them, so `users.create`/`.lookup`/`.availability` all honestly answer `503` in production today. **07: override `create_user`/`lookup_user`/`check_localpart_available`** on that same implementation.
- **`AdminState::with_rooms` needs a real `RoomDirectory` implementation** (04, or the integration lead once 04 has one) — see "Interfaces needed" for the contract. Until then `GET /rooms` and the moderation actions correctly answer `503`, verified live.
- Server notices (`server_notices.list`/`.send`): no seam exists yet; see session 6's "What was left undone" for the shape a `NoticeSender`-style trait would need.
- `AdminState::with_server_info` still needs the running binary's real identity wired in by whoever owns `hs-cli`'s `serve.rs` — every server still reports the `ServerInfo::default()` placeholder (`name: "hs"`, `build: "dev"`, empty `supported_room_versions`/`enabled_components`).
- Real `hs-admin` handlers for the remaining ~118 operations, resource by resource, once 07's `TokenVerifier` is fully wired, 03's job leases, 11's appservice registry, and 13's config/migration models exist to back them (see `REAL_HANDLERS` in `crates/hs-admin/src/router.rs` for the current, authoritative list of what's real vs. still `501`).
- `hs-admin-client` (Rust, `progenitor` or similar) and the TypeScript client generation track 16 owns from `openapi.yaml`.
- Wire `hs_http::router::Builder`'s manifest into an actual `routes.json` file once there is a binary that builds the *whole* server's router (client + federation + admin); today only `hs-admin`'s own admin-surface router exists.
- When track 16 has a real `npm run build` output, swap `crates/hs-admin/src/assets.rs`'s embed source from `web-dist-placeholder/` to the real `web/dist` (one-line change, documented in the placeholder's `index.html`).
- Phase 1 of `docs/design/wasmtime-feasibility.md` (the WASM host itself), gated on someone actually needing it and willing to pay the `wasmtime` build cost.

## Blockers

None.

## Interfaces provided

- `docs/rfcs/0004-admin-api.md` — the frozen-at-week-8 (draft until then) admin API design.
- `docs/rfcs/0005-routes-json-manifest.md` — the `routes.json` format for track 14.
- `crates/hs-admin/openapi/openapi.yaml` and `openapi/operations.json` — the OpenAPI 3.1 contract and its Rust-consumable operation table. **Track 16: generate your client and mock-check your work against `openapi.yaml`; it is validated (`redocly lint`, 0 errors) and the contract test proves the real router agrees with it.**
- `hs-admin-mock` (`cargo run -p hs-admin --bin hs-admin-mock`) — **usable now.** See "Done" above for exactly what it covers; the dev token is `mock-admin-token`.
- `hs_http::{error::MatrixError, problem::Problem, body::{PermissiveJson, StrictJson}, router::{Builder, RouteMeta, assert_matches_openapi}, cors, listener, ratelimit::RateLimiter, time}` — for 07 and 14 (and any other track standing up an HTTP listener) to build on.
- `hs_admin::{auth::TokenVerifier, audit::AuditSink, events::EventBus, router::AdminState}` — the traits and types 07, 03, and the storage track implement or feed.
- `hs_admin::sources::{UserDirectory, UserFilter, SourceError, InMemoryUserDirectory}` and `hs_admin::model::AdminUser` — the seam `GET /users`, `GET /users/{user_id}`, `POST .../lock`, `.../unlock`, `.../deactivate`, `.../reactivate`, and `PATCH /users/{user_id}` (admin flag only) now actually call; 07's real implementation is already wired in by the integration lead per the session-4 report. Contract is frozen; do not change the trait's method signatures without checking who has already implemented against them. **`create_user`/`lookup_user`/`check_localpart_available` (default `Unavailable` bodies) now have real callers** (`POST /users`, `GET /users/lookup`, `GET /users/availability`, session 6) — 07's real `UserDirectory` implementation compiles unchanged (the three methods are defaulted, not required) but answers `503` for all three until 07 overrides them; see "Interfaces needed" below.
- `hs_admin::sources::{RoomDirectory, RoomFilter, InMemoryRoomDirectory}` and `hs_admin::model::AdminRoom` (session 6, new) — the seam `GET /rooms`, `GET /rooms/{room_id}`, `POST .../block`, `.../unblock`, `.../make-admin` call. **No real implementation exists yet**; `AdminState::rooms` defaults to `None` and every handler answers a real `503 unavailable`. See `crates/hs-admin/src/sources.rs`'s doc comment on `RoomDirectory` for the exact contract (method signatures, what `make_admin` does and does not change on `AdminRoom`, and a pointer to Synapse's `make_room_admin` for reference behavior) and `AdminState::with_rooms` for how to wire a real implementation in.
- `hs_admin::idempotency::{IdempotencyStore, Replay, StoredResponse}` — the in-process `Idempotency-Key` cache; `AdminState::idempotency` is always populated (a fresh `IdempotencyStore::new()` from `AdminState::new`), no wiring needed from other tracks.
- `hs_admin::router::AdminState::{with_users, with_server_info}` — the two builder methods `hs-cli` needs to call (see "Next" above for exactly what to pass).
- `hs_modules::{ModuleHooks, NoopHooks, ModuleChain, client::HttpCallbackClient, callback}` — for any track wanting to call out to a module (07 for password auth, 11 for federation/media checks, 04/09 for spam and third-party rules).

## Interfaces needed

- 07: a real `TokenVerifier` implementation (OAuth issuer + legacy admin-flag tokens) to replace `auth::StaticVerifier` in the real server; the `Requester` middleware at week 6 for `hs-http`'s Matrix-facing routes (not yet consumed — no Matrix routes exist yet to wire it into). **New (session 6): override `UserDirectory::create_user`/`lookup_user`/`check_localpart_available` on the same implementation already wired for the other five methods** — verified live that all three currently answer `503` against the running binary (see session 6's entry below), which is correct-but-incomplete behavior, not a bug in this crate.
- 04: **implement `hs_admin::sources::RoomDirectory`** (session 6, new seam) against `hs-room`'s room-summary/state data and wire it with `AdminState::with_rooms` (integration-lead or 04's own work, same pattern as `with_users`). See `crates/hs-admin/src/sources.rs`'s doc comment on the trait for the exact contract: `get_room`/`list_rooms` read an `AdminRoom` snapshot; `set_blocked` both flips the flag and is expected to have a real blocking effect going forward; `make_admin` sends a real `m.room.power_levels` event and returns `()` (the handler re-fetches via `get_room` for its response). Until this exists, `GET /rooms`, `GET /rooms/{room_id}`, and the block/unblock/make-admin actions all correctly answer `503 unavailable`.
- 14: consumes `routes.json`'s format (`docs/rfcs/0005-routes-json-manifest.md`) once it starts; no manifest file has been written yet because no binary builds the whole server's router yet.
- 13: the reloadable-configuration schema and the migration status model, to replace `hs-admin`'s `config`/`migration` operations' `501`s with real handlers backed by real data.
- 03: job leases for the task framework (`Task`/`TaskStatus` in `hs_admin::model` are ready to receive them) and cluster status.
- 11: the appservice registry read/write model, to back the `bridges:*`-scoped operations.
- 16: usability feedback on the OpenAPI document and the mock server, as RFC 0004 amendments (RFC 0004 section 15).

## Decisions made

- **Deferred the `wasmtime` host to Phase 1**, per the instruction not to build `wasmtime` on this shared machine. Wrote the feasibility verdict (`docs/design/wasmtime-feasibility.md`) as a design document instead; no `wasmtime` dependency was added anywhere. This satisfies the brief's "day-one work: ... a `wasmtime` component-model feasibility spike" as a design spike, not a code spike.
- **`hs_admin::events::EventBus` assigns event ids itself** (a monotonic `ulid::Generator`), overwriting whatever id the caller's `Event` carried, rather than trusting `Event::new`'s plain `Ulid::new()`. RFC 0004 section 10 requires "ids are monotonic per server"; a non-monotonic generator broke a real test (two events published in the same millisecond did not sort correctly as plain strings) before this fix.
- **`crates/hs-admin/openapi/openapi.yaml` was built by a throwaway Python generator script** (not checked into the repository — it lived in the session's scratchpad), not authored by hand or with `utoipa`. 118 (now 120, after adding the document's own endpoints) hand-typed paths with full schemas would have been much more error-prone to keep consistent (pagination envelopes, error responses, scopes) than a small Python model with helper functions for the repeated patterns. The generated file is the artifact that matters and is what `redocly lint` and the contract test validate; regenerating it requires rewriting the (unsaved) generator, so **treat `openapi.yaml` and `operations.json` as hand-editable from here on** (the file header says as much) unless someone wants to reconstruct the generator first.
- **`GET /api/v1/openapi.yaml` and `.json` are in the OpenAPI document itself** (as `public: true` operations, i.e. no authentication), even though many real-world OpenAPI documents omit their own meta-endpoints. This was needed for the contract test to have zero exceptions (RFC 0004 section 3.8 already requires these two routes to exist and be unauthenticated; documenting them was the natural fix once the contract test caught the mismatch).
- **`hs-admin-mock` now enforces per-operation scopes, not just "is there a recognized bearer token".** Session 2 deliberately deferred this (see the struck-through reasoning below, kept for the record); session 3 implemented it anyway once fixing the auth-bypass defect meant restructuring auth into one middleware function regardless, and reusing `hs_admin::operations::load()` (the same generated table the real router enforces against) turned out to be a small addition, not a second copy of the scope matrix: `require_auth_middleware` extracts the request's `MatchedPath`, looks up `(method, path) -> required scope` in a table built once from that same function, and answers `403 insufficient-scope` when the caller's token lacks it. A lookup miss (a route this mock serves that isn't in the table, which does not happen today) fails open — no extra scope check — rather than denying, so a future drift between the mock's hand-registered routes and the OpenAPI document degrades gracefully instead of taking down an unrelated route. (Original session-2 reasoning, now superseded: "re-implementing the same scope matrix in the mock would be a second, divergent copy of that logic for a server whose only job is to hand back realistic fixtures" — true of a hand-copied matrix, not true of reusing the generated table directly.)
- **`hs-admin-mock`'s cursors are plain string offsets**, not RFC 0004's opaque keyset cursors (which encode a sort-key fingerprint and reject stale cursors with `400 invalid-cursor`). A mock has no real backing store to build a keyset cursor against; `next_cursor`/`prev_cursor` still round-trip correctly, which is what UI pagination needs to exercise.
- **`hs_http::listener`'s TLS support is a hand-rolled `axum::serve::Listener` implementation**, not a third-party crate (no `axum-server` or similar in the workspace): it is ~30 lines given `rustls`/`tokio-rustls` are already workspace dependencies (added by track 03), and avoids a new dependency for something this small.
- ~~**`hs-admin`'s mutation handlers in the real router skeleton are not yet audited/eventing**~~ **Superseded in session 5**: the five user mutations now real (`users.lock`, `users.unlock`, `users.deactivate`, `users.reactivate`, `users.update`) all call the shared `record_mutation` helper, which writes exactly one `AuditEntry` and publishes exactly one `Event` before answering — see the session 5 entry below.
- **`hs-identity` was left untouched** (still the empty placeholder). The brief lists it under Phase 1/2 ("the identity service design... for after 1.0"), not day-one or Phase 0 work, and this session's assignment did not name it.
- **`Idempotency-Key` is implemented as a real in-process cache** (`hs_admin::idempotency::IdempotencyStore`), not refused or stubbed: only successful responses are cached, entries expire after 24 hours, and the key is scoped per-operation-id. **`If-Match` is implemented as a real ETag** derived from `DefaultHasher` over the user's serialized fields, not a cryptographic hash — sufficient for single-process equality-checking, explicitly noted as insufficient for a future multi-replica deployment (which would need a storage-derived ETag instead). Both were genuinely built, not answered with an honest refusal, since both were straightforward given what already existed (`UserDirectory`'s fetch methods for ETag; a `HashMap` for idempotency).
- **A field a `PATCH` names that no data source can change yet (`display_name`, `avatar_url`, `user_type` on `users.update`) is rejected with `400 validation-failed` naming the field**, never silently accepted-and-ignored — including when set to its current value or explicit `null`. Same principle applied to `users.deactivate`'s `erase: true`: rejected by name (`/erase`) rather than performing a silent plain deactivation, since a caller who explicitly asked for erasure and got a quiet no-op would wrongly believe data was erased.
- **`AuditEntry.request` is left `None` on every entry the real handlers write** (session 5). RFC 0004 section 9 describes an optional `{method, path, request_id, idempotency_key, body}` block with secret redaction and 64KiB truncation; `changes`/`actor`/`target`/`outcome` already make every entry fully reconstructable without it, and building the redaction/truncation logic without a second real caller to validate the field list against felt like speculative scope. Revisit once a second mutating resource (rooms, appservices, ...) needs the same machinery.
- **Per-token-scope filtering of the SSE event stream (RFC 0004 section 10: "a token with only `bridges:read` sees `appservice.*`, ...") is not implemented.** `events.stream` itself requires `admin:read`, and under `Scope::satisfies`'s rules only `admin:read`/`admin:write` satisfy that requirement — no `bridges:*`/`moderation:*`-only token can reach the endpoint at all today, so the described filtering has no token that could currently exercise it. Deferred rather than built against a scope combination nothing can hold; revisit if a future scope model lets a token hold `admin:read` plus a narrower grant that should further restrict what it sees.

### Session 4 (2026-09-18): the first real handlers behind the real router

`docs/next-steps.md` item 1 asked for a first slice of `/api/v1` to answer for real, behind the real `hs-auth::admin_verifier::AdminTokenVerifier` track 07 was wiring in parallel (07 owns `hs-auth`; nothing there was touched here). Scope was five operations plus the data-source seam they need:

- **`crates/hs-admin/src/sources.rs`** (new, `pub mod sources;` in `lib.rs`): `SourceError` (`NotFound`/`Unavailable`/`Invalid`, each with `to_problem()` mapping onto `404 not-found` / `503 unavailable` / `400 validation-failed`), `UserFilter`, and the `UserDirectory` trait, published **verbatim** to the exact contract the task specified (track 07's agent is implementing this trait against its own user store in parallel; the signatures here must not drift). `InMemoryUserDirectory` is the in-memory fake for this crate's own tests, following `hs-federation/src/room_source.rs`'s pattern (trait here, real impl in the owning crate) — builder-style `with_user(self, user) -> Self`, `RwLock<HashMap<String, AdminUser>>` internally since the trait's mutation methods take `&self`. 15 unit tests in `sources::tests`.
- **`crates/hs-admin/src/model.rs`** gained: `AdminUser` (field-for-field match with the OpenAPI `User` schema — `user_id`, `display_name`, `avatar_url`, `admin`, `deactivated`, `erased`, `locked`, `suspended`, `shadow_banned`, `user_type`, `consent_version`, `appservice_id`, `created_at`, `last_seen_at`, `device_count`, `room_count`, `media_count` — `Default`-derived, all-`false`/`None`/`0`/`""`); `ServerInfo` (the static, operator-configured parts of the `ServerInfo` schema — `name`, `version`, `build`, `supported_room_versions`, `enabled_components`, `contract_version` — `Default` gives `name: "hs"`, `version: env!("CARGO_PKG_VERSION")`, `contract_version: "1.0"`); `ServerInfoResponse` (the full wire schema, `ServerInfo`'s fields plus `uptime_ms`, produced by `ServerInfo::with_uptime_ms`); `ServerHealth` (`status` + `checks: BTreeMap<String, String>`, built via `ServerHealth::from_checks` — `"ok"` only if every check is `"ok"`, `"down"` if any check is `"down"`, `"degraded"` otherwise, e.g. one or more checks `"unknown"`). 7 new unit tests.
- **`crates/hs-admin/src/router.rs`**: `AdminState` grew `server_info: ServerInfo` (default `ServerInfo::default()`), `started_at: Instant` (set in `AdminState::new`), and `users: Option<Arc<dyn UserDirectory>>` (default `None`) — `AdminState::new(verifier, audit, events)`'s signature and behavior are unchanged, so every existing caller (`hs-cli`, this crate's own tests) still compiles untouched. Two new builder methods: `AdminState::with_users(Arc<dyn UserDirectory>) -> Self` and `AdminState::with_server_info(ServerInfo) -> Self`, both `#[must_use]` consuming builders.
  - Five operations now have real handlers instead of the generic `501`: `me_get`, `server_get`, `server_health`, `users_list`, `users_get` (functions in `router.rs`, dispatched from a new `REAL_HANDLERS: &[&str]` constant checked in `build_router`'s loop — kept as one greppable list rather than scattering `if`s). `register_real_operation` builds each route from the *same* `(method, path, scope, rate-limited)` metadata `register_operation` would have produced, so the OpenAPI contract test (`tests/contract.rs`) cannot tell a real handler from a `501` one — it still passes unmodified.
  - `me.get`: `require_scope(..., None)` (any valid token; RFC 0004 8.2's one scope-free operation), serializes the verifier's `Principal` directly.
  - `server.get`: requires `admin:read`; returns `state.server_info.with_uptime_ms(state.started_at.elapsed())`.
  - `server.health`: requires `admin:read`; reports `audit: "ok"` and `events: "ok"` (both are non-optional constructor arguments of `AdminState::new`, so answering at all means they're reachable) and `users: "ok"` or `"unknown"` depending on whether `state.users` is wired — **never faked to `"ok"`** when absent, per the task's explicit requirement.
  - `users.list`: requires `admin:read`; `503 unavailable` (via a new `source_unavailable()` helper, never a silent `501` or a fake `200`) when `state.users` is `None`; otherwise builds a `UserFilter` from the `q`/`admin`/`deactivated`/`locked`/`suspended`/`guests` query parameters and paginates the result through `Page::paginate` (existing helper, unchanged). `q` matches `user_id` or `display_name`, case-insensitively (implemented in `InMemoryUserDirectory::list_users`; a real implementation is expected to do the same). `sort` is accepted as a declared query parameter by the OpenAPI document but **not implemented** — both the fake and any future backing store return results in `user_id` order; nothing depends on `sort` yet since no UI consumes this endpoint today.
  - `users.get`: requires `admin:read`; `503 unavailable` when unwired, `404 not-found` (RFC 9457 problem, `urn:hs:problem:not-found`) when the source has no such user, `200` with the `AdminUser` body otherwise.
  - 19 new tests in `router::tests` cover: unauthenticated → 401 (existing), insufficient scope → 403 (existing, still exercises `users.list`'s real scope check), `/me` with no scopes at all → 200 (rewritten — this used to assert `501` before `/me` was real), `/me`'s body round-trips the verifier's `Principal` (`id`, `scopes`), `/server` includes `uptime_ms` and `contract_version`, `/server/health` reports `users: "unknown"` and overall `status: "degraded"` when unwired, `/users` and `/users/{user_id}` both `503` when unwired, `/users/{user_id}` `200` with the real body and `404` problem for a missing user when wired to an `InMemoryUserDirectory`, `/users?admin=true&limit=1` paginates and filters correctly (`next_cursor: null` on the only page), `/users?q=alice` free-text filters. Also a guard test (`real_handlers_keep_the_same_paths_as_the_generic_seam_would`) that every id in `REAL_HANDLERS` actually exists in the operation table, so a typo there fails loudly instead of silently falling through to `501`.
  - Two pre-existing tests needed updating because their target operation became real: `authorized_request_to_undeclared_handler_is_501` now probes `/api/v1/rooms` instead of `/api/v1/users` (still a genuine `501` seam); `me_needs_only_authentication_not_a_specific_scope` now asserts `200` (was `501`) and uses a `moderation:read`-only token to prove no specific scope is required.
- **What was found and not fixed**: `users.list`'s `sort` parameter is declared in the OpenAPI document but not honored (see above — no consumer needs it yet, and building real keyset sorting against an in-memory fake would just be work to throw away once a real backing store exists). `users.create`, `users.update`, and every other mutating `/users/*` operation are untouched (still `501`) — the task's scope was the five read/identity operations named above, all `GET`; no audit-log or event-bus wiring was added because there is nothing yet to audit for those five (all reads). `Idempotency-Key`/`If-Match` handling is not exercised by anything built this session (still Phase 1 for the operations that need it).

**Exactly which of the 142 operations are genuinely served** (not "registered", which all 142 already were): **7** — the 2 pre-existing public document endpoints (`GET /api/v1/openapi.yaml`, `GET /api/v1/openapi.json`) plus this session's 5 (`me.get`, `server.get`, `server.health`, `users.list`, `users.get`). **135** of the 140 non-public operations still answer `501 not-implemented` after real authentication and scope enforcement — that number will keep shrinking; check `REAL_HANDLERS` in `crates/hs-admin/src/router.rs` for the current, authoritative list rather than trusting this prose as it ages.

**Verify:**
```
cargo fmt -p hs-admin
cargo clippy -p hs-admin --all-targets -- -D warnings   # clean
cargo test -p hs-admin                                  # 60 lib tests + 5 mock-bin tests + 2 contract tests, all passing
```

### Session 5 (2026-09-18): the write path, Idempotency-Key/If-Match, and the audit log/event stream as real endpoints

Scope (per this session's assignment): the user mutations whose `UserDirectory` methods already existed and were already implemented against the real store by 07 (`set_admin`, `set_locked`, `set_deactivated`), real `Idempotency-Key`/`If-Match` handling (implement or explicitly refuse — never silently ignore), and the audit log and event stream as real endpoints on the real router (not just the mock).

- **`crates/hs-admin/src/idempotency.rs`** (new, `pub mod idempotency;` in `lib.rs`): `IdempotencyStore`, an in-process `HashMap<"operation_id:key", Entry>` behind a `Mutex`. `check(scope, key, request_body) -> Replay` (`Fresh` / `Same(StoredResponse)` / `Mismatch`, matching a stored body's hash) and `record(scope, key, request_body, StoredResponse)`. Entries expire 24 hours after being recorded (`Instant`-based, lazily evicted on `check`), matching the OpenAPI `IdempotencyKey` parameter's own description ("Replays return the stored response for 24 hours"). Only successful (`200`) responses are ever recorded — a validation failure or `503` is never cached, so retrying after a real failure always retries the mutation. Scoped per operation id (`"users.lock"` vs `"users.unlock"`) so the same key value reused on a different endpoint is fresh, not a collision. 5 unit tests. `AdminState` grew `idempotency: Arc<IdempotencyStore>`, always populated by `AdminState::new` — no integration-lead wiring needed, unlike `users`.
- **`crates/hs-admin/src/router.rs`**, five new real handlers plus two supporting endpoints, added to `REAL_HANDLERS`:
  - **`users.lock` / `users.unlock`** (`POST .../lock`, `.../unlock`, `moderation:write`): body is the OpenAPI `ReasonRequest` (`reason`, `notify` — `notify` is accepted but not acted on, no notification source exists). Both funnel through a shared `toggle_user_and_record` helper (with `users.reactivate`) built around a `ToggleField` enum (`Locked`/`Deactivated`) rather than an async closure (async closures/`Fn` traits returning futures are not stable; an enum with an `async fn apply` avoided boxing a future at every one of five call sites for no real benefit). Fetches the user before and after, calls `UserDirectory::set_locked`, and only records an `AuditChange` when the value actually flipped (idempotent re-locking an already-locked user does not fabricate a bogus `false -> true` in the log).
  - **`users.deactivate`** (`admin:write`): body adds `erase: bool` to `ReasonRequest`'s shape. `erase: true` is rejected with `400 validation-failed` naming `/erase` (`urn:hs:problem:validation-failed`, `errors: [{"pointer": "/erase", ...}]`) rather than silently deactivating without erasing — no eraser is wired to `UserDirectory` yet, and a caller who explicitly asked for erasure and got a quiet no-op would believe data was gone that is not. Otherwise identical to lock/unlock via the shared helper.
  - **`users.reactivate`** (`admin:write`): un-deactivates via the same shared helper.
  - **`users.update`** (`PATCH /users/{user_id}`, `admin:write`): the OpenAPI `UserUpdate` body is parsed field-by-field as `Option<serde_json::Value>` (not `Option<T>`) specifically so presence can be told apart from absence — `{"display_name": null}` and `{}` are different requests, and only the former is a (rejected) attempt to touch a field nothing can change yet. `display_name`, `avatar_url`, and `user_type` present at all (including explicit `null`) produce `400 validation-failed` with one `ValidationError` per offending field, naming its JSON Pointer (`/display_name`, ...) — **this is the task's explicit "reject rather than silently ignore" requirement**, verified live against the running binary (see below). Only `admin` is applied, via `UserDirectory::set_admin`, and only when it actually differs from the current value.
  - **`record_mutation`** (shared by all five): builds one `AuditEntry` (via `Principal::to_actor()`, the target `ResourceRef`, the `AuditChange` list, `AuditOutcome::success(200)`) and appends it through `state.audit`, then publishes one matching `Event` (`user.locked`/`user.unlocked`/`user.deactivated`/`user.reactivated`/`user.updated`, per RFC 0004 section 10.1's taxonomy) through `state.events` — the single place this logic lives, so "audits and publishes on every mutation" cannot be forgotten per-handler. A failed audit write is propagated as `503 unavailable` (RFC 0004 section 9: "a failed write fails the request with 503"), not swallowed.
  - **`Idempotency-Key`** (declared on all four `POST`s, `idempotent_post: true` in `operations.json`): checked via `state.idempotency.check(operation_id, key, raw_body)` before the mutation runs. A replay returns the exact stored response verbatim with an added `idempotency-replayed: true` header (never present on the original response) and **does not run the mutation again** — verified both by a unit test asserting exactly one audit entry after two identical requests, and live against the running binary (below). A key reused with a different body is `422 urn:hs:problem:idempotency-key-payload-mismatch` (the catalog entry already existed in `hs_http::Problem`, unused until now).
  - **`If-Match`** (declared on `users.update`, the compare-and-set `PATCH`): `etag_for_user()` hashes the user's current serialized fields (`DefaultHasher` over `serde_json::to_vec`, wrapped in a quoted ETag string) — not cryptographic, but stable and sufficient for equality-checking a resource that only this process can mutate; a real multi-replica deployment would need a stronger, storage-derived ETag, noted here rather than solved. A mismatched `If-Match` is `412 precondition-failed`; `users_update`'s successful response also carries the new `ETag` header so a client's next compare-and-set has something to send. `normalize_etag` strips a weak-validator `W/` prefix and surrounding quotes so both forms compare equal.
  - **`audit_log.list`** (`GET /audit-log`, `admin:read`): builds an `AuditFilter` from `actor`/`action`/`target_type`/`target_id`/`outcome` (`success`/`failure`, or `400 validation-failed` for anything else)/`recorded_after`/`recorded_before`, fetches up to `AUDIT_QUERY_FETCH_LIMIT` (10,000 — a documented, honest cap, not real keyset pagination; see the comment on the constant) matching entries from `state.audit`, then slices with `Page::paginate` exactly like `users.list` does. `sort=recorded_at` reverses the (default newest-first) order; any other value is ignored (default sort), matching the `users.list` precedent of "declared parameter, only the default value fully implemented."
  - **`audit_log.get`** (`GET /audit-log/{id}`, `admin:read`): `state.audit.get(id)`, `404 not-found` if absent. Not explicitly asked for by the task but trivial given `AuditSink::get` already existed, and it's the natural pair to `audit_log.list`.
  - **`events.stream`** (`GET /events`, `admin:read`): replays `state.events`'s buffer honoring `Last-Event-ID` (header, checked first) or `?last_event_id=` (query, `hs-admin-mock`'s existing precedent), then subscribes for live events, both filtered by repeated `types=` (exact type or `prefix.*` glob), `resource_type`, `resource_id`. `types` is read from `Query<Vec<(String, String)>>` (raw key-value pairs) rather than a struct field, since `axum`'s `Query<T>` (via `serde_urlencoded`) does not reliably collect a repeated `types=a&types=b` into a `Vec<String>` struct field. **Improves on `hs-admin-mock`'s SSE handler** (which the task said to match) in one respect: when `ReplayOutcome` is `Behind` or `Unknown`, a `stream.reset` event (`{"reason": "behind", "oldest_available": ...}`) is sent before the backlog, per RFC 0004 section 10's explicit requirement — the mock ignores `_outcome` entirely and never does this. Keepalive comment every 15 seconds, same as the mock. **Not implemented**: the per-token-scope event filtering RFC 0004 section 10 describes ("a token with only `bridges:read` sees `appservice.*`, ...") — moot under the current `Scope::satisfies` rules, since `events.stream` itself requires `admin:read` and no scope other than `admin:read`/`admin:write` satisfies that requirement, so a bridges- or moderation-only token cannot reach this endpoint at all today. Documented rather than half-implemented against a scope combination nothing can currently hold.
  - 41 new unit tests in `router::tests` (mutation audit+event assertions for all five operations, `users.deactivate`'s erase rejection, `users.update`'s field-rejection and `If-Match` match/mismatch, idempotency replay and mismatch, `audit_log.get`/`.list`, and four `events.stream` tests including the hello frame, backlog replay, type filtering, and the `stream.reset` case — the last built by shrinking `EventBus`'s capacity to 1 mid-test so a second publish evicts the first). SSE tests read exactly one frame at a time with a 500ms timeout via `http_body_util::BodyExt::frame()` rather than `axum::body::to_bytes` (which would hang forever on a stream that, by design, never closes).
- **Verified end-to-end against the running binary** (`cargo build -p hs-cli --bin hs`; `hs generate-config` + `hs generate-signing-key` + `auth.registration_shared_secret` set; `hs register -u ops --admin -k <secret> -v http://127.0.0.1:8008`; `hs serve`): locked/unlocked/deactivated/reactivated a real user over HTTP with real `AdminUser` bodies in the response; confirmed locking a user **revokes that user's own tokens** (self-locking the admin used for the request produced `401 the token has been revoked` on the next call — real, correct behavior from 07's `set_locked`, not a bug, so testing continued with a second admin); `deactivate` with `{"erase":true}` returned the expected `400` naming `/erase`; `PATCH` with `{"display_name": "..."}` returned `400` naming `/display_name`; `PATCH` with a stale `If-Match` returned `412`, with the correct (fetched) ETag returned `200`; two identical `POST .../lock` requests with the same `Idempotency-Key` returned byte-identical bodies, the second carrying `idempotency-replayed: true`, and `GET /audit-log?action=users.lock` showed exactly one entry for that key (not two); a third request with the same key and a different body returned `422`; `GET /events?types=user.*` (curled with `-N`, 3-second timeout) streamed a real `stream.hello` frame followed by the real backlog of `user.locked`/`user.unlocked` events with real actor ids and reasons; `GET /audit-log/{id}` returned the matching entry, and a bogus id returned `404`.
- **What was left undone**: `audit_log.export` (NDJSON export) is still `501` — not named in this session's task and a separate streaming-response shape from `list`/`get`. `users.create`, `users.lookup`, `users.availability`, and every other `/users/*` operation beyond the five named here are still `501`. The `changes` field on lock/unlock/deactivate/reactivate audit entries only covers the one boolean that changed (matches RFC 0004 section 9's "best-effort" framing); `AuditEntry.request` (the RFC's optional `{method, path, request_id, idempotency_key, body}` block) is left `None` on every entry written this session — populating it (with the RFC's secret-redaction and 64KiB truncation rules) is real, separable work that the task's explicit requirements (audit entry + event, never silent) do not strictly need, since `changes`/`actor`/`target`/`outcome` already make "what happened and who did it" fully reconstructable.

**Exactly which of the 142 operations are genuinely served now**: **15** — `me.get`, `server.get`, `server.health`, `users.list`, `users.get`, `users.update`, `users.lock`, `users.unlock`, `users.deactivate`, `users.reactivate`, `audit_log.list`, `audit_log.get`, `events.stream`, plus the 2 pre-existing public document endpoints (`GET /api/v1/openapi.yaml`, `.json`). **127** of the 140 non-public operations still answer `501 not-implemented` after real authentication and scope enforcement. `REAL_HANDLERS` in `crates/hs-admin/src/router.rs` is the authoritative, current list.

**Verify:**
```
cargo fmt -p hs-admin -- --check                        # clean
cargo clippy -p hs-admin --all-targets -- -D warnings   # clean
cargo test -p hs-admin                                  # 83 lib tests + 5 mock-bin tests + 2 contract tests, all passing
```

### Session 6 (2026-09-19): finishing the interrupted user seam, rooms, and audit_log.export

Scope (per this session's assignment): (1) `users.create`/`users.lookup`/`users.availability` handlers on the seam methods session 5 left interrupted (default `Unavailable` bodies already salvaged by the integration lead into `crates/hs-admin/src/sources.rs`/`model.rs`); (2) a `RoomDirectory` seam plus `rooms.list`/`rooms.get`/block/unblock/make-admin, **without** implementing a real backend (`crates/hs-room` is another track's crate); (3) `audit_log.export` as NDJSON; (4) server notices if budget allowed (it did not — see "What was left undone").

- **`crates/hs-admin/src/sources.rs`**: `UserCreateRequest` gained `#[derive(Deserialize)]` with `#[serde(default)]` (field-for-field match with the OpenAPI `UserCreate` schema already existed; it just couldn't be parsed from a request body yet). New: `RoomFilter` (mirrors `UserFilter`'s shape for `rooms.list`'s query parameters) and the `RoomDirectory` trait (`get_room`, `list_rooms`, `set_blocked`, `make_admin`) plus `InMemoryRoomDirectory`, following `UserDirectory`/`InMemoryUserDirectory`'s pattern exactly (`RwLock<HashMap<String, AdminRoom>>`, builder-style `with_room`). **Not implemented against a real backend** — see the trait's doc comment for the full contract track 04 should implement against, including what `make_admin` does (a real `m.room.power_levels` event, read-for-behavior-only from Synapse's `make_room_admin`, never copied) and why it returns `()` rather than an updated `AdminRoom`. 23 new unit tests (`sources::tests`): the salvaged three methods' honest-`Unavailable` defaults, and full coverage of `InMemoryRoomDirectory` (get/list/filter/`q`-search/`set_blocked`/`make_admin`, both found and not-found cases).
- **`crates/hs-admin/src/router.rs`**:
  - `AdminState` gained `rooms: Option<Arc<dyn RoomDirectory>>` (default `None`, like `users`) and `AdminState::with_rooms(Arc<dyn RoomDirectory>) -> Self` (`#[must_use]`, consuming, chains like `with_users`).
  - **`users.availability`** (`GET /users/availability`, `admin:read`): `localpart` read as `Option<String>` (not relied on axum's built-in required-`Query` rejection, which doesn't produce this router's RFC 9457 shape) so a missing value is a real `400 validation-failed` naming `param:localpart`, matching `audit_log.list`'s precedent for parameter validation. `503 unavailable` when `state.users` is `None`; otherwise calls `UserDirectory::check_localpart_available`, returns `{"available": bool}`.
  - **`users.lookup`** (`GET /users/lookup`, `admin:read`): validates the two OpenAPI parameter pairs (`medium`+`address`, `provider`+`external_id`) — a pair with only one half present is `400`; naming neither pair is also `400` (checked before the source is ever consulted, verified live: this is a client error, not a `503` that would wrongly blame an unwired source); naming both pairs is `400`. `Ok(Some(user))` → `200`; `Ok(None))` → `404 not-found` (the OpenAPI document declares this response and it matches `users.get`'s convention of "no such resource is 404, not a null 200").
  - **`users.create`** (`POST /users`, `admin:write`, idempotent): parses the body as `UserCreateRequest` directly (no separate wire struct); `400` naming `/localpart` if neither `localpart` nor `user_id` is given (the one judgment call this handler makes — everything else, including "both given inconsistently", is left to the real implementation per the trait's doc comment, since only it knows its own homeserver domain). Full `Idempotency-Key` handling (check before, record after, same `Replay::{Same,Mismatch,Fresh}` shape every other mutation uses) and `record_mutation` (`user.created` event, audit entry with empty `changes` — a creation, not a field transition). Returns `201` with the created `AdminUser`.
  - **Rooms** (`rooms.list`, `rooms.get`, `rooms.block`, `rooms.unblock`, `rooms.make_admin`): structurally identical to the user handlers — `source_unavailable("room directory", ...)` when `state.rooms` is `None` (verified live: real `503`, not a fake `200` or a `501`), `Page::paginate` for the list, a `toggle_room_and_record` helper (the room-shaped twin of `toggle_user_and_record`) for block/unblock sharing idempotency + audit + event, and `rooms_make_admin` following `users_create`'s "no before/after diff, just an audit entry naming what happened" shape (`room.admin_granted` event, `{"user_id": ...}` data) since `RoomDirectory::make_admin` doesn't return an updated room to diff against. `make_admin`'s body defaults `user_id` to the calling principal when omitted (documented as mirroring Synapse's own default, read for behavior only).
  - **`audit_log.export`** (`GET /audit-log/export`, `admin:read`): NDJSON (`application/x-ndjson`), one `AuditEntry` JSON object per line. Built from a single `AuditSink::query` call joined into one `String` body rather than a true incrementally-streamed response — documented on the handler as the honest reason why (every reachable `AuditSink` today, including a real `hs-tables`-backed one at the query-result stage, already holds the full matching set in memory by the time this handler can see it; there is nothing to stream incrementally yet). This is deliberately a different response shape from `audit_log.list`'s `Page` envelope, matching what the OpenAPI document already declared (a plain line format for `jq`/`grep`, not a paginated resource) — not a shortcut taken to avoid building real pagination.
  - `REAL_HANDLERS` grew by 9: `users.create`, `users.lookup`, `users.availability`, `rooms.list`, `rooms.get`, `rooms.block`, `rooms.unblock`, `rooms.make_admin`, `audit_log.export`.
  - Two pre-existing tests needed updating because their target operation became real: `authorized_request_to_undeclared_handler_is_501` now probes `/api/v1/appservices` instead of `/api/v1/rooms` (`/api/v1/users` was already taken by session 4; `/api/v1/rooms` is now real too).
  - 51 new unit tests in `router::tests`: 503-when-unwired for all eight new real handlers (users.create/lookup/availability, all five room operations), the validation-error cases (`users.lookup` missing/partial/neither pair, `users.create` neither localpart nor user_id, `users.availability` missing localpart), success paths against `InMemoryRoomDirectory` (list/filter/get/404), audit+event assertions for `rooms.block`/`unblock`/`make_admin` and `users.create` (mirroring the "exactly one audit entry, exactly one event" pattern session 5 established for the user toggles), an idempotent-reblock test, and `audit_log_export`'s NDJSON shape (line count, content-type, each line parses as a real `AuditEntry`) plus its own scope-enforcement test. A small `CreatingUserDirectory` test fixture (a `UserDirectory` that overrides the three session-6 methods against a read-only `InMemoryUserDirectory`) exercises `users.create`/`users.lookup`/`users.availability`'s *success* paths, not just their honest-503 fallback against the plain fake.
- **Verified end-to-end against the running binary** (`cargo build -p hs-cli --bin hs`, clean; `hs generate-config --server-name test.local -o hs.yaml` + `hs generate-signing-key -o signing-keys`, `enable_registration: true` and `registration_shared_secret: supersecret` set by hand in the generated config; `hs register -u ops -p ... --admin -k supersecret -v http://127.0.0.1:8008`; `hs serve -c hs.yaml`): `GET /me` returned the real legacy-admin principal; `GET /users` (session 4/5's real handler) returned `200`, confirming 07's `UserDirectory` is already wired for the original five methods; `GET /users/availability?localpart=someone` and `POST /users` both returned real `503 unavailable` naming the missing capability (`"this user directory does not support ... yet"`) — **07's real implementation has not yet overridden the three session-6 methods, so this is correct, honest behavior, not a bug** (see "Interfaces needed"); `GET /users/lookup` with no query parameters returned `400 validation-failed`; `GET /rooms` and `POST /rooms/{id}/block` both returned real `503 unavailable` naming "the room directory data source" (no `RoomDirectory` implementation exists — expected, matches the assignment's "do not implement the real backing" instruction); `POST /users/{ops}/unlock` then `GET /audit-log/export` returned `200`, `content-type: application/x-ndjson`, and one real NDJSON line containing that mutation's actual `AuditEntry` (real `id`, `actor`, `action: "users.unlock"`).
- **What was left undone**: server notices (`server_notices.list`/`.send`) were not reached — budget ran out after rooms and `audit_log.export`; no `ServerNoticeSink`-shaped seam was designed or stubbed, so there is no partial contract to hand off beyond the OpenAPI schemas already in `openapi.yaml` (`ServerNoticeCreate`/`ServerNotice`/`ServerNoticePage`). Whoever picks this up next needs a seam analogous to `UserDirectory`/`RoomDirectory` (likely `NoticeSender: async fn send(&self, recipients: &[String], content: Value, event_type: &str, state_key: Option<String>) -> Result<ServerNotice, SourceError>`) backed by whatever `hs-room`/`hs-user` expose for "send this event into this room on the server's behalf," which this crate does not have visibility into. `users.create`'s "both `localpart` and `user_id` given inconsistently" case is intentionally left to the real implementation to judge (documented on the trait), not validated here. `rooms.aliases.*`, `rooms.media.*`, `rooms.messages.list`, `rooms.state.list`, `rooms.members.list`, `rooms.hierarchy.get`, `rooms.events.*`, `rooms.purge_history`, `rooms.delete`, `rooms.join`, `rooms.forward_extremities.*` — every other `/rooms/*` operation beyond the five named in this session's assignment — are still `501`.

**Exactly which of the 142 operations are genuinely served now**: **24** — `me.get`, `server.get`, `server.health`, `users.list`, `users.get`, `users.update`, `users.lock`, `users.unlock`, `users.deactivate`, `users.reactivate`, `users.create`, `users.lookup`, `users.availability`, `rooms.list`, `rooms.get`, `rooms.block`, `rooms.unblock`, `rooms.make_admin`, `audit_log.list`, `audit_log.get`, `audit_log.export`, `events.stream`, plus the 2 pre-existing public document endpoints (`GET /api/v1/openapi.yaml`, `.json`). **118** of the 140 non-public operations still answer `501 not-implemented` after real authentication and scope enforcement. `REAL_HANDLERS` in `crates/hs-admin/src/router.rs` is the authoritative, current list.

**Verify:**
```
cargo fmt -p hs-admin -- --check                        # clean
cargo clippy -p hs-admin --all-targets -- -D warnings   # clean
cargo test -p hs-admin                                  # 115 lib tests + 5 mock-bin tests + 2 contract tests, all passing
```

## Shared dependencies added

All added to `[workspace.dependencies]` in the root `Cargo.toml`, under a new "Added by track 15" comment block (and one addition, `reqwest`, was appended after that block since another track had already appended its own section by the time it was added):

- `ulid` (with `serde`) — server-generated identifiers (RFC 0004 D15.2) and the event stream's monotonic ids.
- `time` (with `formatting`, `parsing`, `serde`, `macros`) — RFC 3339 millisecond-precision timestamps.
- `tower-http` (with `cors`, `limit`, `trace`, `set-header`) — CORS and the standard axum companion middleware.
- `rust-embed` (with `mime-guess`) — embedding `web/dist` (currently the placeholder) at `/admin/`. (The `mime-guess` feature is enabled but unused in the end; `crate::assets` uses a small hand-written extension map instead. Harmless to leave enabled.)
- `rustls-pemfile` — parsing PEM certificates/keys for `hs_http::listener`'s TLS support.
- `async-stream` — the `hs-admin-mock` SSE handler's stream construction.
- `reqwest` (default-features off, `json` + `rustls-tls`) — `hs_modules::client::HttpCallbackClient`'s HTTP transport.

No new workspace entries on 2026-09-28; two existing ones were added to `hs-cli`: `prometheus-client` (the drain metrics in `cluster_admin.rs`) and, as a dev-dependency, `postgres` (the two-replica test makes and drops its own database).

## How to verify this session's work

```
# OpenAPI document (Node 26/npm already installed)
npx --yes @redocly/cli lint crates/hs-admin/openapi/openapi.yaml   # 0 errors, 3 warnings

# every crate this session touched
cargo fmt -p hs-http -p hs-admin -p hs-modules
cargo clippy -p hs-http -p hs-admin -p hs-modules --all-targets -- -D warnings   # clean
cargo test -p hs-http -p hs-admin -p hs-modules                                 # 69 tests, all passing

# the auth-bypass regression tests specifically (session 3 fix)
cargo test -p hs-admin --bin hs-admin-mock auth_regression_tests   # 5 tests, all passing

# the mock server
HS_ADMIN_MOCK_ADDR=127.0.0.1:8090 cargo run -p hs-admin --bin hs-admin-mock
# in another shell:
curl -H "Authorization: Bearer mock-admin-token" http://127.0.0.1:8090/api/v1/me
curl "http://127.0.0.1:8090/api/v1/users?limit=2"
curl -N -H "Authorization: Bearer mock-admin-token" http://127.0.0.1:8090/api/v1/events   # SSE
# auth is now enforced on reads too:
curl -o /dev/null -w "%{http_code}\n" http://127.0.0.1:8090/api/v1/users                                    # 401, no header
curl -o /dev/null -w "%{http_code}\n" -H "Authorization: Bearer bogus" http://127.0.0.1:8090/api/v1/users   # 401, unrecognized token
curl -o /dev/null -w "%{http_code}\n" -H "Authorization: Bearer mock-admin-token" http://127.0.0.1:8090/api/v1/users   # 200
```
