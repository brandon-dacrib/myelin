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

## What `hs serve` serves today

`hs serve` mounts the compatibility surface on every listener with the `client` resource
(`crates/hs-cli/src/serve.rs`: `hs_auth::synapse_admin_router` for the shared-secret
registration protocol, `crate::synapse_shims` for everything else, which forwards each request
into the native `/api/v1` router in-process, behind the native tokens and scope checks, and
reshapes the answer). At startup the log says so, with the list: `the Synapse admin
compatibility surface is mounted under /_synapse/admin ...` with `routes=` and `operations=`;
`routes.json` (`--routes-manifest`) lists the same under the `synapse-admin-compat` surface,
from `hs_compat::SYNAPSE_ADMIN_ROUTES`, which `hs-compat`'s own tests hold to what its router
mounts. Real-binary test: `crates/hs-cli/tests/synapse_admin.rs` walks synapse-admin's screens
(and `invites_and_notices.rs` server notices, `e2e.rs` registration, `migration_rehearsal.rs`
the user page after a migration).

**69 routes since 2026-10-09** (8 before). The routes are the ones `synapse-admin`'s screens
call -- users, rooms, registration tokens, reports, media, federation -- plus what Draupnir and
operator scripts reach for. A caller without an administrator's token gets the native status
(`401` for no token or a token the admin API does not know, `403` for one without the scope)
with `errcode M_FORBIDDEN`; a native `404` is `M_NOT_FOUND`, a `400`/`422` is
`M_INVALID_PARAM`. Every other row of the tables further down is a mapping only: the native
resource exists, the `/_synapse/admin` path for it is not mounted, and a tool that calls it gets
`404 M_UNRECOGNIZED`.

| Screen | Routes | Native | Differences from Synapse |
|---|---|---|---|
| Probe | `GET /v1/server_version` | `GET /server` | `python_version` is `"n/a"`. |
| Users: list | `GET /v2/users` | `GET /users` | `order_by`/`dir` accepted, not honoured. `is_guest` always false. |
| Users: one | `GET /v2/users/{user_id}` | `GET /users/{id}`, `/threepids`, `/external-ids` | `creation_ts` in seconds (as Synapse's own single-user route); `consent_*` null. |
| Users: create or modify | `PUT /v2/users/{user_id}` | `POST /users` (new, `201`), else `PATCH /users/{id}`, `reset-password`, `deactivate`/`reactivate`, `lock`/`unlock`, 3PID and external-id add/remove | Each field goes through the native operation for it, so each is audited on its own; `logout_devices` defaults to true as in Synapse; `avatar_url`, `locked` and `deactivated` on a new account are applied right after creation. |
| Users: administrator flag | `GET`/`PUT /v1/users/{user_id}/admin` | `GET`/`PATCH /users/{id}` | |
| Users: devices | `GET /v2/users/{user_id}/devices`, `GET`/`PUT`/`DELETE .../devices/{device_id}`, `POST .../delete_devices` | `/users/{id}/devices`, `/devices/{device_id}`, `/devices/bulk-delete` | `last_seen_user_agent` null. |
| Users: rooms, pushers, media | `GET /v1/users/{user_id}/joined_rooms`, `/pushers`, `/media` | `/memberships?membership=join`, `/pushers`, `/media` | `quarantined_by` is `"admin"` or null (who quarantined is not kept); `next_token` is the native cursor. |
| Users: account data | `GET /v1/users/{user_id}/accountdata` | `/users/{id}/account-data` | `global` only; `rooms` is empty (no native listing of per-room account data). |
| Users: whois | `GET /v1/whois/{user_id}` | `/users/{id}/sessions` | One connection per session. |
| Users: password, act as | `POST /v1/reset_password/{user_id}`, `POST /v1/users/{user_id}/login` | `reset-password`, `login-as` | `valid_until_ms` becomes `valid_for_seconds`; the native records a reason ("Synapse admin API: login as user"). |
| Users: shadow ban, suspend | `PUT`/`DELETE /v1/users/{user_id}/shadow_ban`, `PUT /v1/suspend/{user_id}` | `shadow-ban`/`unshadow-ban`, `suspend`/`unsuspend` | Already in that state (native `409`) answers success, as Synapse does. |
| Users: username check | `GET /v1/username_available?username=` | `/users/availability?localpart=` | `400 M_USER_IN_USE` when taken, as Synapse. |
| Users: experimental features | `GET`/`PUT /v1/experimental_features/{user_id}` | `/users/{id}/experimental-features` | |
| Users: lookups | `GET /v1/auth_providers/{provider}/users/{external_id}`, `GET /v1/threepid/{medium}/users/{address}` | `/users/lookup` | |
| Users: redact | `POST /v1/user/{user_id}/redact`, `GET /v1/user/redact_status/{redact_id}` | `redact-events` (a task), `/tasks/{id}` | Only the first of `rooms` is honoured (the native takes one room or all); `failed_redactions` is always `{}`. |
| Rooms: list, one | `GET /v1/rooms`, `GET /v1/rooms/{room_id}` | `/rooms`, `/rooms/{id}` | `order_by`/`dir` accepted, not honoured; `joined_local_devices` 0. |
| Rooms: members, state | `GET /v1/rooms/{room_id}/members`, `/state` | `/rooms/{id}/members`, `/state` | |
| Rooms: delete | `DELETE /v1/rooms/{room_id}`, `DELETE /v2/rooms/{room_id}`, `GET /v2/rooms/{room_id}/delete_status`, `GET /v2/rooms/delete_status/{delete_id}` | `POST /rooms/{id}/delete` (a task), `/tasks`, `/tasks/{id}` | Both versions answer `{"delete_id"}` (the task): the deletion runs in the background here, so v1's synchronous list of kicked users cannot be answered; `shutdown_room` carries what the task's result has. |
| Rooms: block, make admin, join | `GET`/`PUT /v1/rooms/{room_id}/block`, `POST .../make_room_admin`, `POST /v1/join/{room_id}` | `block`/`unblock`, `make-admin`, `join` | `block`'s `user_id` (who blocked) is null; `join` takes a room id, not an alias. |
| Rooms: messages, context, extremities, media | `GET .../messages`, `GET .../context/{event_id}`, `GET`/`DELETE .../forward_extremities`, `GET /v1/room/{room_id}/media` | `/messages`, `/events/{id}/context`, `/forward-extremities`, `/media` | `state_group` null; `received_ts` is `origin_server_ts`; `context`'s `start`/`end` empty. |
| Registration tokens | `GET /v1/registration_tokens`, `POST .../new`, `GET`/`PUT`/`DELETE .../{token}` | `/registration-tokens` | `expiry_time` in milliseconds both ways. |
| Reports | `GET /v1/event_reports`, `GET`/`DELETE .../{report_id}` | `/reports` | `canonical_alias` and `name` null; `event_json` is the native `event`. |
| Media | `GET /v1/statistics/users/media`, `DELETE /v1/media/{server}/{id}`, `POST /v1/media/quarantine|unquarantine/{server}/{id}`, `POST /v1/media/protect|unprotect/{id}` | `/statistics/users/media`, `/media/{server}/{id}`, `/quarantine`, `/unquarantine`, `/protect`, `/unprotect` | `displayname` null in the statistics; protect/unprotect act on this server's own media. Bulk deletes and the cache purge (tasks here) are not mounted. |
| Federation | `GET /v1/federation/destinations`, `GET .../{destination}`, `GET .../{destination}/rooms`, `POST .../{destination}/reset_connection` | `/federation/destinations...` | `last_successful_stream_ordering` and `stream_ordering` null. |
| Notices, deactivation | `POST /v1/send_server_notice`, `PUT .../{txn_id}`, `POST /v1/deactivate/{user_id}` | `/server-notices`, `/users/{id}/deactivate` | `id_server_unbind_result` is `"no-support"`. |
| Registration | `GET`, `POST /v1/register` | `hs-auth`'s shared-secret protocol | Needs `auth.registration_shared_secret`; without it both answer `404 M_UNRECOGNIZED`. |

Not mounted, and why: background updates (`R-MIGRATE`), `purge_history` and its status (the native `purge-history` is a task with no Synapse-shaped status yet), `purge_media_cache` and bulk media deletes (tasks), room-wide and user-wide media quarantine (tasks), `quarantine_media/{room_id}`, `scheduled_tasks`, `statistics/database/rooms`, `search_users` (deprecated in Synapse), `account_validity`, `user_reports`, `timestamp_to_event`, `hierarchy`, `fetch_event`, `cumulative_joined_room_count`, `sent_invite_count`, `override_ratelimit`, `memberships`, `_allow_cross_signing_replacement_without_uia`, `media/{server}/{id}` `GET`.

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
| `/deactivate/(?P<target_user_id>[^/]*)$` | POST | `{erase?}` → `{id_server_unbind_result}` | mapped | `POST /users/{user_id}/deactivate` | **Implemented** (`crates/hs-compat/src/admin_proxy.rs`'s `deactivate_user`, mounted at `/_synapse/admin/v1/deactivate/{user_id}`; answers `"no-support"`, the spec's value for an identity-server unbind not attempted). Legacy v1 path; same body/response shape as the v2 route below it in Synapse itself. |
| `/experimental_features/(?P<user_id>[^/]*)$` | GET, PUT | `{features: {msc...: bool}}` | mapped | `GET`/`PUT /users/{user_id}/experimental-features` | Synapse's per-user MSC opt-ins have no native equivalent set of flags yet (this server does not gate features per-user by MSC number, see `docs/compat/synapse-config-table.md`'s reason code R-NOFLAG); the route exists so tooling that reads/writes it does not 404, and returns an empty object. |
| `/register$` | GET, POST | `GET` → `{nonce}`; `POST {nonce, username, password, admin?, mac, user_type?}` → `{access_token, user_id, home_server, device_id}` | unsupported | — | R-COMPAT-PROTOCOL. Served by `hs-compat`'s own shared-secret registration protocol (`crates/hs-compat/src/shared_secret.rs`, `docs/compat/cli-shims.md`), authenticated by nonce+HMAC rather than a bearer token — there is no `/api/v1` resource for this because native account creation goes through `POST /users` under normal admin auth instead. This route is the one place the compat surface does *not* forward to a native resource. |
| `/reset_password/(?P<target_user_id>[^/]*)$` | POST | `{new_password, logout_devices?}` → `{}` | mapped | `POST /users/{user_id}/reset-password` | |
| `/search_users/(?P<target_user_id>[^/]*)$` | GET | `{results: [{name, password_hash, ...}]}` | mapped (diff) | `GET /users` (search query param) | Deprecated even in Synapse in favor of the v2 list-with-search below; maps onto the same native list resource with a `search_term` filter. |
| `/suspend/(?P<target_user_id>[^/]*)$` | PUT | `{suspend: bool}` → `{user_id, suspended}` | mapped (diff) | `POST /users/{user_id}/suspend`, `POST /users/{user_id}/unsuspend` | Synapse's single toggle route splits into two named actions natively, matching the shape of `shadow_ban`/`lock` below. |
| `/threepid/(?P<medium>[^/]*)/users/(?P<address>[^/]*)$` | GET | `{user_id}` | mapped (diff) | `GET /users/lookup` (query params `medium`, `address`) | Same generic lookup as `auth_providers` above. |
| `/user/(?P<user_id>[^/]*)/redact$` | POST | `{rooms?, reason?}` → `{redact_id}` | mapped (diff) | `POST /users/{user_id}/redact-events` | Returns a task id (see `/tasks/{id}` below) instead of a redaction-specific id. |
| `/user/redact_status/(?P<redact_id>[^/]*)$` | GET | `{status, failed_redactions}` | mapped (diff) | `GET /tasks/{id}` | Redaction is one instance of the native generic scheduled-task model, not a bespoke status type. |
| `/users$` | GET, POST | `GET` → `{users: [...], next_token}`; legacy `POST` (rare) creates | mapped | `GET /users` (list), `POST /users` (create) | `GET` is **implemented** (`crates/hs-compat/src/admin_proxy.rs`'s `users_list`), mounted at `/_synapse/admin/v2/users` — checked against `refs/synapse/synapse/rest/admin/users.py`'s `UsersRestServletV2`/`V3` (`PATTERNS = admin_patterns("/users$", "v2"/"v3")`: this route only ever existed at `v2`/`v3`, never `v1`, so mounting at `v2` alone covers the common case). `v3`'s different `deactivated`-filter semantics and `POST` (create) are not implemented. |
| `/users/(?P<user_id>[^/]*)$` | GET, PUT | `GET` → full user record; `PUT` creates-or-modifies | mapped (diff) | `GET /users/{user_id}`; `POST /users` (create) or `PATCH /users/{user_id}` (modify) | Synapse's single idempotent `PUT` (create if absent, else modify) splits into two native operations; the compat handler decides which by checking existence first. `GET` is **implemented** (`crates/hs-compat/src/admin_proxy.rs`'s `users_get`, mounted at `/_synapse/admin/v2/users/{user_id}` — checked against `refs/synapse/synapse/rest/admin/users.py`'s `UserRestServletV2Get`, `v2`-only). `PUT` is not implemented. |
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
| `/rooms$` | GET | `{rooms: [...], total_rooms, next_batch}` | mapped | `GET /rooms` | `GET` is **implemented** (`crates/hs-compat/src/admin_proxy.rs`'s `rooms_list`, mounted at `/_synapse/admin/v1/rooms` — checked against `refs/synapse/synapse/rest/admin/rooms.py`'s `ListRoomRestServlet`, `admin_patterns("/rooms$")` with no version argument, i.e. `v1` only). `order_by`/`dir` are accepted but not honoured (native `rooms.list` has no sort parameter yet). |
| `/rooms/(?P<room_id>[^/]*)$` | GET, DELETE | `GET` → room details; `DELETE` (v1, synchronous) removes it | mapped (diff) | `GET /rooms/{room_id}`; delete via `POST /rooms/{room_id}/delete` | The v1 synchronous `DELETE` has no native equivalent — deletion is always asynchronous/task-based natively (matching Synapse's own newer v2 behavior below). `GET` is **implemented** (`crates/hs-compat/src/admin_proxy.rs`'s `rooms_get`, mounted at `/_synapse/admin/v1/rooms/{room_id}`). `DELETE` is not implemented. |
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
| `/server_version$` | GET | `{server_version, python_version}` | mapped (diff) | `GET /server` | **Implemented** (`crates/hs-compat/src/admin_proxy.rs`'s `server_version`, mounted at `/_synapse/admin/v1/server_version`; forwards to `GET /api/v1/server`, which `crates/hs-admin/src/router.rs`'s `server_get` — checked — actually serves, not a `501` stub). Native returns full server info (version, build, uptime, ...), not just the version pair; `python_version` has no equivalent, reported as a literal "n/a" string rather than omitted. |
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
