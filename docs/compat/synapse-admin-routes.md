# Synapse admin API route map

Track 13, cross-referencing track 15's native admin API
(`crates/hs-admin/openapi/openapi.yaml`, `docs/rfcs/0004-admin-api.md`).
Every one of the 77 `/_synapse/admin/v{1,2,3}` routes in
`docs/synapse-inventory.md`, with the JSON shape it returns and the native
`/api/v1` resource and method it maps onto, so `synapse-admin`, Draupnir,
Mjolnir and existing operator scripts keep working against `hs-compat`'s
`/_synapse/admin/*` surface while the actual logic lives in `hs-admin`'s
resources (`PLAN.md` section 9.3: "the 77 `/_synapse/admin` routes are
served with the same JSON shapes on top of our admin model").

This is the answer to the brief's open question ("which Synapse-specific
admin semantics to emulate versus answer 'not applicable'"): background
updates and the manhole have no native concept (see reason codes below);
everything else that names a real operator action has a native home, even
where the shape differs.

## How to read the table

- **Synapse route** is the exact regex from `docs/synapse-inventory.md`
  (so this table can be checked against the inventory mechanically); the
  prose route below it is the human form.
- **Methods** are Synapse's, in the order a client would use them.
- **Status**: `mapped` (same shape, same semantics, just a different
  path), `mapped (diff)` (native resource covers it, but the shape,
  granularity or synchronicity differs — noted), or `unsupported` (no
  native equivalent, with a reason code).
- **Native** is the `hs-admin` `/api/v1` path and method(s).

## Reason codes for `unsupported` rows

| Code | Meaning |
|---|---|
| **R-MIGRATE** | Background schema/data migrations are bounded, transactional `hs-tables` migrations (see `docs/compat/synapse-config-table.md`'s `background_updates` row), not a throttled job queue with its own admin surface. Nothing to poll or tune. |
| **R-PHASE1** | Real functionality, not yet built by the owning track (named in the note); no native resource exists yet to map onto. |
| **R-COMPAT-PROTOCOL** | Handled entirely inside `hs-compat` by a different mechanism than the native resource API (nonce/HMAC auth rather than a bearer token), so there is no `/api/v1` equivalent to name — see the note. |

## Users (`synapse/rest/admin/users.py`, `devices.py`, `experimental_features.py`)

| Synapse route | Methods | JSON shape (brief) | Status | Native | Notes |
|---|---|---|---|---|---|
| `/account_validity/validity$` | POST | `{user_id, expiration_ts?}` → `{expiration_ts}` | unsupported | — | R-PHASE1 (hs-auth). Account-validity (expiring accounts) is not in the Phase 0 auth schema; see `docs/compat/synapse-config-table.md`'s `email` row for the related email-delivery gap. |
| `/auth_providers/(?P<provider>[^/]*)/users/(?P<external_id>[^/]*)$` | GET | `{user_id}` | mapped (diff) | `GET /users/lookup` | Native lookup is one generic endpoint (query params select the identifier kind: `external_id` + `provider`, `threepid`, etc.) rather than one route per provider. |
| `/deactivate/(?P<target_user_id>[^/]*)$` | POST | `{erase?}` → `{id_server_unbind_result}` | mapped | `POST /users/{user_id}/deactivate` | Legacy v1 path; same body/response shape as the v2 route below it in Synapse itself. |
| `/experimental_features/(?P<user_id>[^/]*)$` | GET, PUT | `{features: {msc...: bool}}` | mapped | `GET`/`PUT /users/{user_id}/experimental-features` | Synapse's per-user MSC opt-ins have no native equivalent set of flags yet (this server does not gate features per-user by MSC number, see `docs/compat/synapse-config-table.md`'s reason code R-NOFLAG); the route exists so tooling that reads/writes it does not 404, and returns an empty object. |
| `/register$` | GET, POST | `GET` → `{nonce}`; `POST {nonce, username, password, admin?, mac, user_type?}` → `{access_token, user_id, home_server, device_id}` | unsupported | — | R-COMPAT-PROTOCOL. Served by `hs-compat`'s own shared-secret registration protocol (`crates/hs-compat/src/shared_secret.rs`, `docs/compat/cli-shims.md`), authenticated by nonce+HMAC rather than a bearer token — there is no `/api/v1` resource for this because native account creation goes through `POST /users` under normal admin auth instead. This route is the one place the compat surface does *not* forward to a native resource. |
| `/reset_password/(?P<target_user_id>[^/]*)$` | POST | `{new_password, logout_devices?}` → `{}` | mapped | `POST /users/{user_id}/reset-password` | |
| `/search_users/(?P<target_user_id>[^/]*)$` | GET | `{results: [{name, password_hash, ...}]}` | mapped (diff) | `GET /users` (search query param) | Deprecated even in Synapse in favor of the v2 list-with-search below; maps onto the same native list resource with a `search_term` filter. |
| `/suspend/(?P<target_user_id>[^/]*)$` | PUT | `{suspend: bool}` → `{user_id, suspended}` | mapped (diff) | `POST /users/{user_id}/suspend`, `POST /users/{user_id}/unsuspend` | Synapse's single toggle route splits into two named actions natively, matching the shape of `shadow_ban`/`lock` below. |
| `/threepid/(?P<medium>[^/]*)/users/(?P<address>[^/]*)$` | GET | `{user_id}` | mapped (diff) | `GET /users/lookup` (query params `medium`, `address`) | Same generic lookup as `auth_providers` above. |
| `/user/(?P<user_id>[^/]*)/redact$` | POST | `{rooms?, reason?}` → `{redact_id}` | mapped (diff) | `POST /users/{user_id}/redact-events` | Returns a task id (see `/tasks/{id}` below) instead of a redaction-specific id. |
| `/user/redact_status/(?P<redact_id>[^/]*)$` | GET | `{status, failed_redactions}` | mapped (diff) | `GET /tasks/{id}` | Redaction is one instance of the native generic scheduled-task model, not a bespoke status type. |
| `/users$` | GET, POST | `GET` → `{users: [...], next_token}`; legacy `POST` (rare) creates | mapped | `GET /users` (list), `POST /users` (create) | |
| `/users/(?P<user_id>[^/]*)$` | GET, PUT | `GET` → full user record; `PUT` creates-or-modifies | mapped (diff) | `GET /users/{user_id}`; `POST /users` (create) or `PATCH /users/{user_id}` (modify) | Synapse's single idempotent `PUT` (create if absent, else modify) splits into two native operations; the compat handler decides which by checking existence first. |
| `/users/(?P<user_id>[^/]*)/_allow_cross_signing_replacement_without_uia$` | POST | `{duration_ms?}` → `{updated}` | unsupported | — | R-PHASE1 (hs-e2e, not built in Phase 0). |
| `/users/(?P<user_id>[^/]*)/accountdata$` | GET | `{account_data: {global, rooms}}` | mapped | `GET /users/{user_id}/account-data` | |
| `/users/(?P<user_id>[^/]*)/admin$` | PUT | `{admin: bool}` → `{}` | mapped (diff) | `PATCH /users/{user_id}` (`{"admin": bool}`) | Folded into the general user-patch resource instead of a dedicated sub-route. |
| `/users/(?P<user_id>[^/]*)/cumulative_joined_room_count$` | GET | `{cumulative_joined_room_count}` | mapped (diff) | `GET /users/{user_id}/statistics` | One field of the native per-user statistics resource rather than its own route. |
| `/users/(?P<user_id>[^/]*)/joined_rooms$` | GET | `{joined_rooms: [room_id, ...], total}` | mapped (diff) | `GET /users/{user_id}/memberships` (filtered to `state=join`) | Native unifies joined/invited/knocked under one memberships resource with a state filter. |
| `/users/(?P<user_id>[^/]*)/login$` | POST | `{valid_until_ms?}` → `{access_token}` | mapped | `POST /users/{user_id}/login-as` | |
| `/users/(?P<user_id>[^/]*)/memberships$` | GET | `{joined_rooms: [...]}` | mapped | `GET /users/{user_id}/memberships` | Same route family as `joined_rooms`; Synapse itself has both for historical reasons, the native side has one. |
| `/users/(?P<user_id>[^/]*)/override_ratelimit$` | GET, POST, DELETE | `{messages_per_second, burst_count}` | mapped (diff) | `GET`/`PUT`/`DELETE /users/{user_id}/rate-limit` | Renamed; `POST` (set) becomes `PUT` (native resources use `PUT` for whole-resource replace). |
| `/users/(?P<user_id>[^/]*)/pushers$` | GET | `{pushers: [...]}` | mapped | `GET /users/{user_id}/pushers` | |
| `/users/(?P<user_id>[^/]*)/sent_invite_count$` | GET | `{invite_count}` | mapped (diff) | `GET /users/{user_id}/statistics` | Another field folded into the per-user statistics resource. |
| `/users/(?P<user_id>[^/]*)/shadow_ban$` | GET, POST, DELETE | `{shadow_banned: bool}` | mapped (diff) | `POST /users/{user_id}/shadow-ban`, `POST /users/{user_id}/unshadow-ban`; status via `GET /users/{user_id}` | Same split-into-named-actions pattern as `suspend`. |

## Devices (`synapse/rest/admin/devices.py`)

| Synapse route | Methods | JSON shape (brief) | Status | Native | Notes |
|---|---|---|---|---|---|
| `/users/(?P<user_id>[^/]*)/delete_devices$` | POST | `{devices: [device_id, ...]}` → `{}` | mapped | `POST /users/{user_id}/devices/bulk-delete` | |
| `/users/(?P<user_id>[^/]*)/devices$` | GET | `{devices: [...]}` | mapped | `GET /users/{user_id}/devices` | |
| `/users/(?P<user_id>[^/]*)/devices/(?P<device_id>[^/]*)$` | GET, PUT, DELETE | `{device_id, display_name, last_seen_ip, last_seen_ts}` | mapped | `GET`/`PATCH`/`DELETE /users/{user_id}/devices/{device_id}` | `PUT` (whole-resource set) becomes `PATCH` (native only allows renaming `display_name`, matching what Synapse's `PUT` actually accepts). |

## Rooms (`synapse/rest/admin/rooms.py`, `synapse/rest/admin/__init__.py`)

| Synapse route | Methods | JSON shape (brief) | Status | Native | Notes |
|---|---|---|---|---|---|
| `/join/(?P<room_identifier>[^/]*)$` | POST | `{user_id}` → `{room_id}` | mapped | `POST /rooms/{room_id}/join` | Accepts a room ID or alias, same as Synapse. |
| `/purge_history/(?P<room_id>[^/]*)(/(?P<event_id>[^/]*))?$` | POST | `{purge_up_to_ts?, purge_up_to_event_id?, delete_local_events?}` → `{purge_id}` | mapped (diff) | `POST /rooms/{room_id}/purge-history` | Returns a task id tracked via `GET /tasks/{id}`, not a purge-specific id/status pair. |
| `/purge_history_status/(?P<purge_id>[^/]*)$` | GET | `{status}` | mapped (diff) | `GET /tasks/{id}` | Purge history is one instance of the native generic scheduled-task model. |
| `/rooms$` | GET | `{rooms: [...], total_rooms, next_batch}` | mapped | `GET /rooms` | |
| `/rooms/(?P<room_id>[^/]*)$` | GET, DELETE | `GET` → room details; `DELETE` (v1, synchronous) removes it | mapped (diff) | `GET /rooms/{room_id}`; delete via `POST /rooms/{room_id}/delete` | The v1 synchronous `DELETE` has no native equivalent — deletion is always asynchronous/task-based natively (matching Synapse's own newer v2 behavior below). |
| `/rooms/(?P<room_id>[^/]*)/block$` | GET, PUT | `{block: bool}` | mapped (diff) | `POST /rooms/{room_id}/block`, `POST /rooms/{room_id}/unblock`; status via `GET /rooms/{room_id}` | Split into named actions, same pattern as user suspend/shadow-ban. |
| `/rooms/(?P<room_id>[^/]*)/context/(?P<event_id>[^/]*)$` | GET | `{events_before, event, events_after, state}` | mapped | `GET /rooms/{room_id}/events/{event_id}/context` | |
| `/rooms/(?P<room_id>[^/]*)/delete_status$` | GET | `{results: [{delete_id, status}]}` | mapped (diff) | `GET /tasks/{id}` | Room deletion is task-based natively; this becomes a task lookup, filtered by room where the caller does not already have the task id. |
| `/rooms/(?P<room_id>[^/]*)/hierarchy$` | GET | `{rooms: [...]}` (space summary) | mapped | `GET /rooms/{room_id}/hierarchy` | |
| `/rooms/(?P<room_id>[^/]*)/members$` | GET | `{members: [user_id, ...], total}` | mapped | `GET /rooms/{room_id}/members` | |
| `/rooms/(?P<room_id>[^/]*)/messages$` | GET | `{chunk: [...], start, end}` | mapped | `GET /rooms/{room_id}/messages` | |
| `/rooms/(?P<room_id>[^/]*)/state$` | GET | `{state: [...]}` | mapped | `GET /rooms/{room_id}/state` | |
| `/rooms/(?P<room_id>[^/]*)/timestamp_to_event$` | GET | `{event_id, origin_server_ts}` | mapped (diff) | `GET /rooms/{room_id}/events/at` (`ts`, `dir` query params) | Renamed to match the native events sub-resource family. |
| `/rooms/(?P<room_identifier>[^/]*)/forward_extremities$` | GET, DELETE | `{results: [...], count}` | mapped | `GET`/`DELETE /rooms/{room_id}/forward-extremities` | Forward extremities are a Synapse storage-model artifact (`PLAN.md` section 6.2); the native store still tracks and can report/prune them even though the underlying state representation differs (section 6.3). |
| `/rooms/(?P<room_identifier>[^/]*)/make_room_admin$` | POST | `{user_id?}` → `{}` | mapped | `POST /rooms/{room_id}/make-admin` | |
| `/rooms/delete_status/(?P<delete_id>[^/]*)$` | GET | `{status, ...}` | mapped (diff) | `GET /tasks/{id}` | Same task-model mapping as `rooms/{id}/delete_status`, keyed by task id instead of room id. |
| `/room/(?P<room_id>[^/]*)/media$` | GET | `{local: [...], remote: [...]}` | mapped | `GET /rooms/{room_id}/media` | Legacy singular `/room/` alias of the same route Synapse also serves at `/rooms/{room_id}/media` under `media.py`; both forward to the same native resource. |
| `/room/(?P<room_id>[^/]*)/media/quarantine$` | POST | `{}` → `{num_quarantined}` | mapped | `POST /rooms/{room_id}/media/quarantine` | |
| `/quarantine_media/(?P<room_id>[^/]*)$` | POST | `{}` → `{num_quarantined}` | mapped | `POST /rooms/{room_id}/media/quarantine` | A second legacy alias for the same action as the route above it. |

## Media (`synapse/rest/admin/media.py`)

| Synapse route | Methods | JSON shape (brief) | Status | Native | Notes |
|---|---|---|---|---|---|
| `/media/(?P<server_name>[^/]*)/(?P<media_id>[^/]*)$` | GET, DELETE | `{...media info}` | mapped | `GET`/`DELETE /media/{server_name}/{media_id}` | |
| `/media/(?P<server_name>[^/]*)/delete$` | POST | `{before_ts?, size_gt?}` → `{deleted_media: [...]}` | mapped (diff) | `POST /media/delete` (`server_name` filter in body) | Folded into the general bulk-delete resource with a server filter instead of a per-server route. |
| `/media/delete$` | POST | `{before_ts?, size_gt?}` → `{deleted_media: [...]}` | mapped | `POST /media/delete` | |
| `/media/protect/(?P<media_id>[^/]*)$` | POST | `{}` → `{}` | mapped | `POST /media/{server_name}/{media_id}/protect` | `server_name` is implicit (local media only) in Synapse's route; native requires it explicitly. |
| `/media/quarantine/(?P<server_name>[^/]*)/(?P<media_id>[^/]*)$` | POST | `{}` → `{}` | mapped | `POST /media/{server_name}/{media_id}/quarantine` | |
| `/media/quarantine_changes$` | GET | `{changes: [...]}` | mapped (diff) | `GET /media` (`quarantined=true` filter) | Native lists current quarantine state via the general media-list resource rather than a change log. |
| `/media/unprotect/(?P<media_id>[^/]*)$` | POST | `{}` → `{}` | mapped | `POST /media/{server_name}/{media_id}/unprotect` | |
| `/media/unquarantine/(?P<server_name>[^/]*)/(?P<media_id>[^/]*)$` | POST | `{}` → `{}` | mapped | `POST /media/{server_name}/{media_id}/unquarantine` | |
| `/purge_media_cache$` | POST | `{before_ts?}` → `{deleted}` | mapped | `POST /media/purge-remote-cache` | |
| `/user/(?P<user_id>[^/]*)/media/quarantine$` | POST | `{}` → `{num_quarantined}` | mapped (diff) | Enumerate via `GET /users/{user_id}/media`, then `POST /media/{server_name}/{media_id}/quarantine` per item | No native per-user bulk-quarantine action; the compat handler performs the loop server-side so callers still see one request/response. |
| `/users/(?P<user_id>[^/]*)/media$` | GET, DELETE | `{media: [...], total}` | mapped | `GET`/`DELETE /users/{user_id}/media` | |

## Federation (`synapse/rest/admin/federation.py`)

| Synapse route | Methods | JSON shape (brief) | Status | Native | Notes |
|---|---|---|---|---|---|
| `/federation/destinations$` | GET | `{destinations: [...], total}` | mapped | `GET /federation/destinations` | |
| `/federation/destinations/(?P<destination>[^/]*)$` | GET | `{destination, retry_last_ts, failure_ts, ...}` | mapped | `GET /federation/destinations/{server_name}` | |
| `/federation/destinations/(?P<destination>[^/]*)/rooms$` | GET | `{rooms: [...], total}` | mapped | `GET /federation/destinations/{server_name}/rooms` | |
| `/federation/destinations/(?P<destination>[^/]+)/reset_connection$` | POST | `{}` → `{}` | mapped | `POST /federation/destinations/{server_name}/reset` | |

## Events (`synapse/rest/admin/events.py`)

| Synapse route | Methods | JSON shape (brief) | Status | Native | Notes |
|---|---|---|---|---|---|
| `/fetch_event/(?P<event_id>[^/]*)$` | GET | `{event, event_json}` (re-fetched live from the origin server) | unsupported | — | The closest native tool, `GET /events/{event_id}`, reads this server's own store; it does not re-fetch a live copy from a remote origin the way Synapse's federation debug tool does. No native equivalent for the live-refetch behavior; tracked as a `hs-federation` debugging feature, not an `hs-admin` one. |

## Event reports and user reports (`event_reports.py`, `user_reports.py`)

| Synapse route | Methods | JSON shape (brief) | Status | Native | Notes |
|---|---|---|---|---|---|
| `/event_reports$` | GET | `{event_reports: [...], total}` | mapped (diff) | `GET /reports` (`type=event` filter) | Native unifies event reports and user reports (MSC4194-adjacent) into one `/reports` resource with a type filter, since both are "content reported for moderation," just about different targets. |
| `/event_reports/(?P<report_id>[^/]*)$` | GET, DELETE | `{...report}` | mapped | `GET`/`DELETE /reports/{id}` | |
| `/user_reports$` | GET | `{user_reports: [...], total}` | mapped (diff) | `GET /reports` (`type=user` filter) | Same unification as `event_reports`. |
| `/user_reports/(?P<report_id>[^/]*)$` | GET, DELETE | `{...report}` | mapped | `GET`/`DELETE /reports/{id}` | |

## Registration tokens (`registration_tokens.py`)

| Synapse route | Methods | JSON shape (brief) | Status | Native | Notes |
|---|---|---|---|---|---|
| `/registration_tokens$` | GET, POST | `{registration_tokens: [...]}` / create | mapped | `GET`/`POST /registration-tokens` | |
| `/registration_tokens/(?P<token>[^/]*)$` | GET, PUT, DELETE | `{token, uses_allowed, pending, completed, expiry_time}` | mapped | `GET`/`PATCH`/`DELETE /registration-tokens/{token}` | `PUT` (whole-resource update) becomes `PATCH` (native only allows changing the mutable fields, matching what Synapse's `PUT` actually accepts). |
| `/registration_tokens/new$` | POST | `{token?, uses_allowed?, expiry_time?, length?}` → created token | mapped (diff) | `POST /registration-tokens` | A random token is generated natively when the request body omits `token`, collapsing Synapse's separate `new` route into the general create endpoint. |

## Background updates (`background_updates.py`)

| Synapse route | Methods | JSON shape (brief) | Status | Native | Notes |
|---|---|---|---|---|---|
| `/background_updates/enabled$` | GET, POST | `{enabled: bool}` | unsupported | — | R-MIGRATE. |
| `/background_updates/start_job$` | POST | `{job_name}` → `{}` | unsupported | — | R-MIGRATE. |
| `/background_updates/status$` | GET | `{enabled, current_updates}` | unsupported | — | R-MIGRATE. |

## Scheduled tasks (`scheduled_tasks.py`)

| Synapse route | Methods | JSON shape (brief) | Status | Native | Notes |
|---|---|---|---|---|---|
| `/scheduled_tasks$` | GET | `{scheduled_tasks: [...]}` | mapped | `GET /tasks` | Native's generic task resource is also what purge-history, room-delete and redact-events report through (see the Rooms and Users sections above), so this list is a superset of Synapse's own scheduled-task list. |

## Statistics (`statistics.py`)

| Synapse route | Methods | JSON shape (brief) | Status | Native | Notes |
|---|---|---|---|---|---|
| `/statistics/database/rooms$` | GET | `{rooms: [{room_id, state_events, ...}]}` | mapped (diff) | `GET /statistics/rooms` | Native per-room statistics do not expose Synapse's state-group-specific counters (`state_events`, `distinct_state_types`): state storage is a different representation entirely (`PLAN.md` section 6.3), so those columns have no equivalent. Event/member counts still map directly. |
| `/statistics/users/media$` | GET | `{users: [{user_id, media_count, media_length}], total}` | mapped | `GET /statistics/users/media` | |

## Miscellaneous (`__init__.py`, `username_available.py`)

| Synapse route | Methods | JSON shape (brief) | Status | Native | Notes |
|---|---|---|---|---|---|
| `/server_version$` | GET | `{server_version, python_version}` | mapped (diff) | `GET /server` | Native returns full server info (version, build, uptime, ...), not just the version pair; `python_version` has no equivalent (nothing to report). |
| `/username_available$` | GET | `{available: bool}` | mapped (diff) | `GET /users/availability` | Same check, renamed to match the native resource-oriented naming (`PLAN.md` D12: consistent resource naming). |

---

## Summary

| | Count |
|---|---|
| `mapped` | 41 |
| `mapped (diff)` | 29 |
| `unsupported` | 7 |
| **Total** | **77** |

Regenerate this table (and the count above) whenever `tools/synapse_inventory.py` reports a changed admin route list, or whenever track 15 changes `crates/hs-admin/openapi/openapi.yaml` in a way that affects a `Native` column entry — grep both files for the routes this document cites before every Synapse pinned-version bump.
