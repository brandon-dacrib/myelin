# RFC 0004. The admin API (`/api/v1`)

Date: 2026-09-17. Status: draft for review by tracks 16 and 13; freezes at week 8 as "API v1 contract". Owner: track 15.

Companion artifacts: the OpenAPI 3.1 document at `crates/hs-admin/openapi/openapi.yaml` (the contract), the mock server `hs-admin-mock` (track 16 develops against it), and the `hs-admin` router whose contract tests assert agreement with the document.

## 1. Motivation

`PLAN.md` D12: the admin API is a product. It is the only thing the management interface (16) talks to, the model that the Synapse-compatible `/_synapse/admin` surface (13) is mapped onto, and the surface operators automate against. Synapse's admin API grew endpoint by endpoint from the server's internals; this one is derived from what operators do. The input is the flows in `docs/workstreams/16-management-web-interface.md` (add a bridge; find and deal with a user; understand a room; watch federation health; run a migration from Synapse) and the bridge operations in `docs/workstreams/11-appservices-and-bridges.md` and `PLAN.md` section 8.2 (add, show, list, update, pause, resume, remove, rotate tokens, replay, export a registration, health and backlog).

## 2. Summary of decisions

| # | Decision |
|---|---|
| D15.1 | Base path `/api/v1` on the client listener, same origin as `/_matrix`. Experimental endpoints under `/api/unstable/<feature>/`. |
| D15.2 | Plural kebab-case resources, `snake_case` JSON members, RFC 3339 UTC timestamps in `*_at` members, integer `*_ms` durations and `*_bytes` sizes. Matrix identifiers appear verbatim (percent-encoded in paths). Server-generated identifiers are ULIDs. |
| D15.3 | Actions that are not plain CRUD are `POST /<resource>/{id}/<verb>` (for example `POST /users/{user_id}/suspend`). The `:verb` suffix style is rejected because Matrix identifiers contain colons. |
| D15.4 | Every collection is keyset-paginated with an opaque `cursor`; the envelope is `{items, next_cursor, prev_cursor, total?}`. No offsets in the public API. The Rust admin model (which 13 maps onto) also supports offset paging so Synapse's `from`/`limit` can be served. |
| D15.5 | Filtering is by named query parameters per resource plus `q` for free text; sorting is `sort=<field>` or `sort=-<field>` from a per-resource allow-list. |
| D15.6 | Errors are RFC 9457 problem details (`application/problem+json`) with `type` URNs from a fixed catalog, plus extension members `errcode` (the Matrix error code, whenever one applies), `request_id`, `errors[]` (field-level validation), `retry_after_ms`, `required_scope`. |
| D15.7 | `Idempotency-Key` on `POST`; replays return the stored response for 24 hours; a reuse with a different payload is `422`; a concurrent in-flight duplicate is `409`. `PUT` and `DELETE` are idempotent by construction; `PATCH` supports `If-Match`. |
| D15.8 | Scopes: `admin:read`, `admin:write`, `bridges:read`, `bridges:write`, `moderation:read`, `moderation:write`. `admin:write` satisfies every requirement; each `*:write` satisfies its `*:read`. Every operation declares exactly one minimal scope in the OpenAPI document. |
| D15.9 | Long-running work returns `202 Accepted` with a `Task` and a `Location: /api/v1/tasks/{id}` header; tasks are cancellable and observable in the event stream. |
| D15.10 | Versioning is in the path. v1 is additive-only after freeze; breaking changes create v2 and v1 is kept at least two minor releases and six months with `Deprecation` and `Sunset` headers. Clients must tolerate unknown members and unknown values of open enums. |
| D15.11 | Every mutation writes an audit entry before the response is sent; the audit log is a resource (`/audit-log`) and an event stream type. |
| D15.12 | The event stream is server-sent events at `GET /api/v1/events` with a fixed, namespaced taxonomy, `Last-Event-ID` reconnect against a bounded replay buffer, and a `stream.reset` event when the client is behind the buffer. |
| D15.13 | Reports are one resource (`/reports`) with a `kind` discriminator (`event`, `user`) so the interface has one inbox. |
| D15.14 | Bridges are appservices. The `/appservices` family carries health, backlog, replay, pause, resume, token rotation and registration export; the `/bridge-types` catalog renders registrations, Compose snippets and `Bridge` resources for the add-bridge wizard. The `bridges:*` scopes cover both families. |
| D15.15 | 07's account-management pages are not behind this API. They are user-facing and use the user's own session at the OAuth account-management URL; the operator's view of the same account is `/users/{user_id}`. |
| D15.16 | The OpenAPI document is the contract and is hand-authored during Phase 0; the router is built from an operation table that also emits an OpenAPI object, and contract tests assert both agree. After the handlers are complete the generated document replaces the hand-authored one at the same path. |
| D15.17 | The Synapse-compatible `/_synapse/admin` surface (13) maps onto the Rust admin model (`hs_admin::model`), not onto the HTTP API, so 13 is not constrained by D15.4. |

## 3. Conventions

### 3.1 Transport

- Base URL `/api/v1`. All paths below are relative to it.
- Requests carry `Authorization: Bearer <token>`. Tokens come from 07's issuer (OAuth 2.0 access tokens with the scopes in section 8) or, for compatibility, a legacy access token of a user with the server-administrator flag, which is treated as `admin:write`.
- Request bodies are `application/json` (UTF-8). Unlike the Matrix routes, `/api/v1` is strict: a body with another `Content-Type` is `415 unsupported-media-type`; malformed JSON is `400 validation-failed`.
- Responses are `application/json`; errors are `application/problem+json`; the audit export is `application/x-ndjson`; the event stream is `text/event-stream`; a registration export may be `application/yaml`.
- Every response carries `X-Request-Id` (echoed from the request if present, otherwise generated). The same value is in tracing spans and in problem details.
- `GET` responses for single resources carry a weak `ETag`; `PATCH`, `PUT` and `DELETE` honour `If-Match` and answer `412 precondition-failed` on mismatch.
- CORS: `/api/v1` is same-origin with the management interface, so no CORS headers are emitted by default. An operator running the interface elsewhere configures `admin_api.cors_origins`.
- Rate limits are per token: `RateLimit` and `RateLimit-Policy` headers (IETF `draft-ietf-httpapi-ratelimit-headers`) on every response, `429 rate-limited` with `Retry-After` and `retry_after_ms` when exceeded.

### 3.2 Naming

- Resource collections are plural nouns in kebab-case: `/users`, `/registration-tokens`, `/audit-log` (a log is one thing), `/bridge-types`.
- Sub-resources nest at most two levels: `/users/{user_id}/devices/{device_id}`.
- Actions are verbs: `POST /rooms/{room_id}/block`. Reversals are separate verbs (`unblock`, `unsuspend`) rather than a boolean body, so the audit log and the scopes can tell them apart.
- JSON members are `snake_case`. Booleans are adjectives (`deactivated`, `admin`, `federatable`), never `is_`-prefixed. Timestamps are RFC 3339 with millisecond precision in UTC (`2026-09-17T21:04:05.123Z`) in members ending in `_at`. Durations are integers in milliseconds in members ending in `_ms`. Sizes are integers in bytes in members ending in `_bytes`. Counts are integers in members ending in `_count`.
- Identifiers: Matrix identifiers (`@user:server`, `!room:server`, `$event`, `#alias:server`, `server.name`) are used verbatim as path parameters (clients percent-encode; `!`, `@`, `:`, `#` and `$` are all reserved characters and must be encoded). Server-generated identifiers (tasks, reports, audit entries, sessions, registration tokens' internal ids, cluster replicas) are ULIDs in canonical uppercase Crockford base32.
- Enumerations are strings. Enumerations documented as **open** may gain values in v1; clients must treat unknown values as "other". Enumerations documented as **closed** are frozen.

### 3.3 Pagination

Every collection endpoint accepts:

| Parameter | Type | Default | Notes |
|---|---|---|---|
| `limit` | integer 1..=`max` | 50 | `max` is 500 unless the resource says otherwise (members and messages allow 1000). Values above `max` are clamped, not rejected. |
| `cursor` | string | none | Opaque. Obtained from `next_cursor` or `prev_cursor` of a previous page. |
| `include_total` | boolean | false | Ask for `total`. Only honoured where a count is cheap (indexed); otherwise `total` is absent. |

and returns

```json
{
  "items": [ ... ],
  "next_cursor": "MDFKOFJ...",
  "prev_cursor": null,
  "total": 1234
}
```

- `next_cursor` is `null` on the last page; `prev_cursor` is `null` on the first page.
- Cursors are keyset cursors: they encode the sort key values and identifier of the boundary item plus a fingerprint of the sort and filters. A cursor presented with a different `sort` or different filters is `400 invalid-cursor`. Cursors do not expire and do not pin a snapshot; items inserted or removed between pages may appear or vanish, never duplicate.
- `items` is always present, possibly empty. A collection whose parent does not exist is `404`, not an empty page.

### 3.4 Filtering, searching and sorting

- Filters are named query parameters listed per operation in the OpenAPI document. Repeating a parameter means "any of" (`status=active&status=failed`). Time ranges use `<field>_after` and `<field>_before` (inclusive, exclusive). Booleans are `true`/`false`.
- `q` is free-text search whose semantics each resource documents (users: localpart and display name; rooms: name, canonical alias and room id; appservices: id and sender; destinations: server name).
- `sort=<field>` ascending, `sort=-<field>` descending, one field, with the identifier as the implicit tiebreaker. Each collection documents its allowed fields and its default. An unknown field is `400 validation-failed` with `errors[0].pointer = "param:sort"`.

### 3.5 Errors

All errors are RFC 9457 problem details:

```json
{
  "type": "urn:hs:problem:insufficient-scope",
  "title": "Insufficient scope",
  "status": 403,
  "detail": "This operation requires the admin:write scope.",
  "instance": "/api/v1/users/%40alice%3Aexample.org/suspend",
  "request_id": "01J8RQ1V7X3C9Z6E2M4N8P0S1T",
  "required_scope": "admin:write"
}
```

The catalog is closed for v1 (new types are additive but the mapping of existing ones does not change):

| `type` (`urn:hs:problem:` prefix omitted) | Status | When | Extra members |
|---|---|---|---|
| `validation-failed` | 400 | Malformed JSON, schema violation, bad parameter | `errors[]` of `{pointer, detail}`; `pointer` is a JSON Pointer into the body or `param:<name>` |
| `invalid-cursor` | 400 | Cursor unparsable or fingerprint mismatch | |
| `unauthenticated` | 401 | Missing, expired, revoked or malformed token | `WWW-Authenticate: Bearer realm="hs-admin", error="invalid_token"` |
| `insufficient-scope` | 403 | Token lacks the operation's scope | `required_scope`; `WWW-Authenticate: Bearer error="insufficient_scope", scope="..."` |
| `forbidden` | 403 | Authenticated and scoped but the action is refused (for example touching a protected user) | `errcode` when a Matrix rule applies |
| `not-found` | 404 | Unknown path or resource | `errcode: "M_NOT_FOUND"` when the resource is a Matrix object |
| `method-not-allowed` | 405 | | `Allow` header |
| `conflict` | 409 | Duplicate identifier, namespace conflict, state conflict (for example suspending a deactivated user) | `errcode` when applicable (`M_USER_IN_USE`, `M_EXCLUSIVE`) |
| `idempotency-key-in-flight` | 409 | The same key is being processed | |
| `precondition-failed` | 412 | `If-Match` mismatch | |
| `payload-too-large` | 413 | | |
| `unsupported-media-type` | 415 | | |
| `idempotency-key-payload-mismatch` | 422 | Key reused with a different request | |
| `unprocessable` | 422 | Well-formed but semantically impossible request (for example a cutover before a copy) | `errcode` when applicable |
| `rate-limited` | 429 | | `retry_after_ms`; `Retry-After`; `errcode: "M_LIMIT_EXCEEDED"` |
| `internal` | 500 | | never includes internals; `request_id` is the correlation handle |
| `not-implemented` | 501 | Endpoint declared but not yet served (Phase 0 skeleton) | |
| `unavailable` | 503 | Owning replica unreachable, store unavailable, read-only during cutover | `retry_after_ms` when known |

`errcode` is present whenever a Matrix client could observe the same failure for the same reason through the Matrix API (the admin operation ran a Matrix-level operation that failed), so tooling written against Synapse's admin API can branch on it and 13's compatibility surface can map problems back to `{errcode, error}` mechanically: `error` is the problem's `detail`.

The Matrix routes (`/_matrix/*`, `/_synapse/*`) keep the Matrix error shape and never emit problem details; `hs-http` owns both mappings.

### 3.6 Idempotency and concurrency

- `POST` operations accept `Idempotency-Key` (1 to 255 visible ASCII characters; the generated clients send a ULID by default). The server stores `(principal, key) -> (request fingerprint, status, headers, body)` for 24 hours. A repeat with the same fingerprint returns the stored response with `Idempotency-Replayed: true`. A repeat with a different fingerprint is `422 idempotency-key-payload-mismatch`. A repeat while the first is still running is `409 idempotency-key-in-flight`.
- The fingerprint is SHA-256 over method, path and the canonical JSON of the body.
- `PUT` and `DELETE` are idempotent without a key; `PATCH` bodies are JSON Merge Patch (RFC 7396) and clients that need a compare-and-set send `If-Match`.
- The audit entry of a replayed request records `replayed: true` and points to the original entry.

### 3.7 Long-running operations

Operations that can take more than a few seconds (room deletion and purge, history purge, user redaction, bulk media deletion, remote-cache purge, appservice replay, migration steps, verification) return

```
HTTP/1.1 202 Accepted
Location: /api/v1/tasks/01J8RQ2...
```

with the `Task` as the body. A `Task` has `id`, `action` (open enum: `room.delete`, `room.purge_history`, `user.redact_events`, `user.deactivate`, `media.delete`, `media.purge_remote_cache`, `appservice.replay`, `migration.copy`, `migration.verify`, `migration.cutover`, `federation.refetch_keys`, ...), `status` (closed enum: `scheduled`, `running`, `succeeded`, `failed`, `cancelled`), `resource` (`{type, id}`), `progress` (`{current, total, unit, message}` or null), `result` (JSON or null), `error` (problem or null), `created_at`, `started_at`, `finished_at`, `scheduled_for`, `created_by`. Tasks are the admin face of 03's leased background jobs; the task framework in `hs-admin` runs on those leases. Cancellation is `POST /tasks/{id}/cancel` and is best-effort for a running task. Finished tasks are retained for 30 days.

### 3.8 Versioning and deprecation

- The version is the first path segment after `/api`. `v1` freezes at week 8.
- Additive changes ship in v1 without notice: new endpoints, new optional request members, new response members, new values of open enums, new event-stream types, new problem types.
- Anything else is breaking and creates `/api/v2`: removing or renaming members, changing a type, making an optional member required, changing a default, changing a closed enum, changing a status code.
- A deprecated operation or member is marked `deprecated: true` in the OpenAPI document with `x-hs-sunset` (a date) and `x-hs-replacement`; the server sends `Deprecation: @<unix time>` and `Sunset: <http-date>` (RFC 9745 and RFC 8594) and a `Link: <...>; rel="successor-version"` header. Minimum notice: two minor releases and six months.
- Experimental operations live under `/api/unstable/<feature>/...`, are excluded from the generated clients, and may change or vanish without notice.
- `GET /api/v1/openapi.json` and `GET /api/v1/openapi.yaml` serve the document of the running binary, unauthenticated. `info.version` is the server release; `x-hs-contract` is the frozen contract version (`1.0`, then `1.1`, ...). The web interface compares `x-hs-contract` to the one it was built against and warns on a lower value.

### 3.9 Authentication and scopes

See section 8.

### 3.10 Audit log

See section 9.

### 3.11 Event stream

See section 10.

## 4. Resource model from operator tasks

Each flow names the resources it needs. The union is the API; nothing is in the API that no flow needs.

### 4.1 Add a bridge (16's marquee, 11's registry)

1. Choose a bridge type: `GET /bridge-types` (catalog: name, upstream project, image, default namespaces, config keys with descriptions, whether double puppeting is supported, which MSCs it needs). `GET /bridge-types/{type}` for one.
2. Render what the wizard produces: `POST /bridge-types/{type}/render` with the operator's choices returns `{registration, registration_yaml, compose_yaml, bridge_resource_yaml}`. Nothing is created.
3. Validate before creating: `POST /appservices?dry_run=true` returns the parsed registration and namespace conflicts against existing registrations and users.
4. Create: `POST /appservices` with a `registration` object or `registration_yaml` string. `201` with the `AppService`; it is live immediately (hot registration). Tokens are generated if absent.
5. Export the registration for the bridge's own config: `GET /appservices/{id}/registration` (`Accept: application/yaml` or JSON). Includes tokens; needs `bridges:write`.
6. Watch it come up: `GET /appservices/{id}/health` and `appservice.health_changed` events.

Ongoing bridge operations: list (`GET /appservices`, with health summaries inline), show, update (`PATCH`, everything but `id` and `sender_localpart`), pause and resume (`POST .../pause`, `.../resume`), remove (`DELETE`), rotate tokens (`POST .../rotate-tokens` returns the new `as_token` and `hs_token` once), ping (`POST .../ping`), backlog (`GET .../backlog` lists pending and dead-lettered transactions with age and last error), replay (`POST .../replay` with an optional transaction list or "everything since", returns a Task), and the bridge's own login flow is deep-linked from `AppService.links.login_url` if the bridge type declares one.

### 4.2 Find and deal with a user

1. Find: `GET /users?q=alice` (also `GET /users/lookup?medium=email&address=...` and `?provider=oidc-corp&external_id=...`, and `GET /users/availability?localpart=...`).
2. Understand: `GET /users/{user_id}` (profile, flags, 3PIDs, external ids, creation and last-seen, appservice, user type, consent, counts of devices, rooms, media), `GET .../devices`, `.../sessions` (the "whois" view: sessions with IPs, user agents and last activity), `.../memberships`, `.../media`, `.../pushers`, `.../account-data`, `.../threepids`, `.../external-ids`, `.../rate-limit`, `.../experimental-features`, `.../statistics` (sent invites, cumulative joins).
3. Act (each is its own verb, audited and scoped): `suspend`/`unsuspend`, `lock`/`unlock`, `shadow-ban`/`unshadow-ban`, `deactivate` (with `erase`) and `reactivate`, `reset-password`, `logout` (all sessions), `login-as` (mint a token for support; `admin:write` only), `redact-events` (Task), `PATCH` (display name, avatar, admin flag, user type), device create/update/delete and bulk delete, session revoke, 3PID add/remove, external-id add/remove, rate-limit override put/delete, experimental features put, `DELETE .../media` (Task).
4. Create: `POST /users` (localpart or full id, optional password, display name, admin, user type, 3PIDs, external ids).

### 4.3 Understand a room

1. Find: `GET /rooms?q=...` with filters (public, empty, blocked, encrypted, federatable, room type, version) and sorts (name, joined members, local members, state events, created).
2. Understand: `GET /rooms/{room_id}` (name, topic, avatar, canonical alias, counts, version, creator, encryption, join rules, guest access, history visibility, federatable, public, room type, blocked with reason, tombstone and replacement, forgotten), `.../members` (with membership filter), `.../state` (with type and state key filters), `.../messages` (timeline, cursor paginated in either direction), `.../events/{event_id}` and `.../events/{event_id}/context`, `.../events/at?ts=` (timestamp to event), `.../hierarchy` (spaces), `.../aliases`, `.../forward-extremities`, `.../media`. `GET /events/{event_id}` fetches any event by id when the room is unknown.
3. Act: `block`/`unblock`, `delete` (Task; with block, purge, replacement room and message), `purge-history` (Task), `make-admin`, `join` a local user, `DELETE .../forward-extremities`, `POST .../media/quarantine`, alias create and delete, and server notices to members (section 4.7).

### 4.4 Watch federation health

- `GET /federation/destinations` sorted by failure (`sort=-failing_since`) with `failing=true`; each destination carries `last_successful_at`, `failing_since`, `retry_last_at`, `retry_interval_ms`, `pending_pdu_count`, `pending_edu_count`.
- `GET /federation/destinations/{server_name}`, `.../rooms` (rooms shared with it), `POST .../reset` (clear backoff).
- `GET /federation/keys` (our signing keys and their validity, old keys), `GET /federation/keys/{server_name}` (cached remote keys), `POST /federation/keys/{server_name}/refresh`.
- `federation.destination_backoff` and `federation.destination_recovered` events for the live view; `GET /statistics/overview` carries the counts.

### 4.5 Run a migration from Synapse (13)

- `GET /migration`: status (`idle`, `copying`, `paused`, `ready_for_cutover`, `cutting_over`, `verifying`, `completed`, `failed`, `aborted`), source description, per-stream progress (users, devices, tokens, keys, account data, push rules, rooms and events, media), rates, estimated remaining time, errors.
- `POST /migration/start` (source reference: the connection string is never in the API; the operator references a configured secret), `pause`, `resume`, `cutover` (Task), `verify` (Task), `abort`.
- `GET /migration/log` for the translation and copy report entries.
- `migration.*` events with progress.

### 4.6 The rest of the operator's day

- Media: `GET /media` (origin, server, uploader, quarantined, protected, size, age), `GET /media/{server_name}/{media_id}`, `DELETE`, `quarantine`/`unquarantine`/`protect`/`unprotect`, `POST /media/delete` (bulk by age and size, Task), `POST /media/purge-remote-cache` (Task). Synapse's "quarantine changes" feed is served from the audit log (`action=media.quarantine`).
- Reports: `GET /reports` (kind, status, room, reporter, reported user), `GET /reports/{id}`, `POST /reports/{id}/resolve` (with a resolution: `no_action`, `warned`, `redacted`, `suspended`, `deactivated`, `room_blocked`, `other`, plus a note), `DELETE`.
- Registration tokens: CRUD on `/registration-tokens` with `uses_allowed`, `pending`, `completed`, `expires_at`.
- Tasks: `GET /tasks`, `GET /tasks/{id}`, `POST /tasks/{id}/cancel`.
- Statistics: `GET /statistics/overview` (the dashboard numbers), `GET /statistics/users/media`, `GET /statistics/rooms` (largest), `GET /statistics/timeseries?metric=&from=&until=&step=`.
- Server notices: `POST /server-notices` (one user or many; content, type, state key) returns the event ids; `GET /server-notices` lists sent notices.
- Cluster (03): `GET /cluster` (mode, epoch, replica and shard counts), `GET /cluster/replicas`, `GET /cluster/replicas/{id}`, `POST .../drain`, `POST .../undrain`, `GET /cluster/shards` (kind, owner, state).
- Configuration (13): `GET /config` (sections with `reloadable`, `source`, `last_reloaded_at`), `GET /config/{section}`, `PATCH /config/{section}` (only reloadable sections; validated; `If-Match`), `POST /config/validate`, `POST /config/reload` (re-read files; returns a report). Secrets are write-only and rendered as `{"$secret": true}`.
- Audit log: `GET /audit-log`, `GET /audit-log/{id}`, `GET /audit-log/export` (NDJSON).
- Server: `GET /server` (name, version, build, supported room versions, enabled components, uptime, contract version), `GET /server/health` (the probe summary), `GET /me` (the caller's principal and scopes).

### 4.7 Not in v1

- Anything a user does to their own account (07's account-management pages).
- Room message sending other than server notices.
- Raw store access, SQL, or Parquet export (D1 says purpose-built indexes; export is a CLI concern).
- Background-update control (Synapse-specific; 13 answers "not applicable").

## 5. Common schemas

`Page<T>` (section 3.3), `Problem` (section 3.5), `Task` (section 3.7), `Principal` (section 8), `AuditEntry` (section 9), `Event` (section 10), and the resource schemas in the OpenAPI document. Two shapes recur:

- `ResourceRef`: `{type, id}` where `type` is an open enum (`user`, `device`, `room`, `event`, `media`, `appservice`, `destination`, `report`, `registration_token`, `task`, `replica`, `config_section`, `migration`, `server_key`, `server_notice`) and `id` is the resource's identifier.
- `Actor`: `{kind, id, display_name, token_id, ip, user_agent}` with `kind` in `user`, `client`, `service_account`, `system`.

Money-shaped counts that are expensive (for example `joined_members` on a list of ten thousand rooms) are served from 04's room stats, never computed on read.

## 6. Naming of operations

Operation ids are `<resource>.<verb>` with dots (`users.list`, `users.get`, `users.create`, `users.update`, `users.suspend`, `appservices.rotate_tokens`, `rooms.messages.list`). They are stable identifiers: the generated clients expose them as method names, the audit log records them as `action`, and the scope table keys on them.

## 7. Request and response examples

List users who are administrators, newest first:

```
GET /api/v1/users?admin=true&sort=-created_at&limit=2
Authorization: Bearer ...

200 OK
{
  "items": [
    {"user_id": "@ops:example.org", "display_name": "Operations", "admin": true, "created_at": "2026-09-01T09:00:00.000Z", ...},
    {"user_id": "@alice:example.org", "display_name": "Alice", "admin": true, "created_at": "2026-08-12T15:20:11.412Z", ...}
  ],
  "next_cursor": "eyJ2IjoxLCJzIjoiLWNyZWF0ZWRfYXQiLCJrIjpbIjIwMjYtMDgtMTJUMTU6MjA6MTEuNDEyWiIsIkBhbGljZTpleGFtcGxlLm9yZyJdfQ",
  "prev_cursor": null
}
```

Suspend a user with an idempotency key:

```
POST /api/v1/users/%40mallory%3Aexample.org/suspend
Idempotency-Key: 01J8RQ3Q0V5B2M9N4P8R1S6T7U
Content-Type: application/json
{"reason": "Spam in #general", "notify": false}

200 OK
{"user_id": "@mallory:example.org", "suspended": true, "suspended_at": "2026-09-17T21:04:05.123Z", ...}
```

Delete a room:

```
POST /api/v1/rooms/%21abc%3Aexample.org/delete
{"block": true, "purge": true, "message": "This room violated the terms of service.", "new_room": {"name": "Content violation", "creator": "@ops:example.org"}}

202 Accepted
Location: /api/v1/tasks/01J8RQ4...
{"id": "01J8RQ4...", "action": "room.delete", "status": "scheduled", "resource": {"type": "room", "id": "!abc:example.org"}, ...}
```

## 8. Authentication and authorization

### 8.1 Principals

A request is authenticated by 07's verifier into a `Principal`:

```json
{
  "kind": "user",
  "id": "@ops:example.org",
  "display_name": "Operations",
  "scopes": ["admin:read", "admin:write"],
  "token_id": "01J8RQ...",
  "expires_at": "2026-09-18T21:04:05.000Z",
  "issued_by": "native"
}
```

`kind` is `user` (an operator logged into the interface through the OAuth issuer with PKCE), `client` (an OAuth client credentials grant for automation), `service_account` (a token minted by the CLI for scripts), or `legacy` (a Matrix access token of a user with the administrator flag; scopes are then `admin:read` and `admin:write`).

`hs-admin` defines the trait 07 implements:

```rust
#[async_trait]
pub trait TokenVerifier: Send + Sync {
    async fn verify(&self, bearer: &str) -> Result<Principal, AuthError>;
}
```

`AuthError` distinguishes `Invalid`, `Expired`, `Revoked`, `Unavailable`. Only `Unavailable` becomes `503`; the others are `401 unauthenticated`.

### 8.2 Scopes

| Scope | Grants |
|---|---|
| `admin:read` | Read every resource, including audit log, configuration (secrets redacted), cluster and migration status. Not registration exports with tokens, not `login-as`. |
| `admin:write` | Everything. Implies every other scope. |
| `bridges:read` | Read `/appservices`, `/bridge-types`, their health and backlog; read `/tasks` for `appservice.*` actions; read the `appservice.*` event types. |
| `bridges:write` | `bridges:read` plus create, update, delete, pause, resume, replay, rotate tokens, export registration, render bridge types. |
| `moderation:read` | Read `/users`, `/rooms`, `/reports`, `/media`, their sub-resources, `/tasks` for moderation actions, and the `user.*`, `room.*`, `report.*`, `media.*` event types. Not sessions' IP addresses (redacted), not account data. |
| `moderation:write` | `moderation:read` plus suspend, unsuspend, lock, unlock, shadow-ban, deactivate, redact-events, reset-password, logout, room block, delete, purge, make-admin, join, quarantine and delete media, resolve and delete reports, server notices. Not `admin` flag changes, not `login-as`, not user creation, not appservices, not configuration. |

Each operation declares its minimal scope in `security` in the OpenAPI document; the router enforces it before the handler runs. `GET /me` and the OpenAPI documents need any valid token or none respectively. The event stream requires at least one scope and filters events to what the scopes allow.

Per-resource restrictions (for example a moderator limited to some rooms) are not in v1; 07's issuer can restrict a token's scopes, not its objects.

### 8.3 Sensitive operations

`login-as`, `rotate-tokens`, registration export, `reset-password`, `admin` flag changes and `config` writes are `admin:write` (or `bridges:write` for the appservice ones), always audited with the full request, and never replayable through the idempotency cache across principals.

## 9. The audit log

Every mutation (anything but `GET`, `HEAD`, `OPTIONS`) produces exactly one `AuditEntry`, written durably before the response is sent (a failed write fails the request with `503`):

```json
{
  "id": "01J8RQ5...",
  "recorded_at": "2026-09-17T21:04:05.123Z",
  "action": "users.suspend",
  "actor": {"kind": "user", "id": "@ops:example.org", "token_id": "01J8RQ...", "ip": "203.0.113.7", "user_agent": "hs-admin-web/0.1"},
  "target": {"type": "user", "id": "@mallory:example.org"},
  "request": {"method": "POST", "path": "/api/v1/users/%40mallory%3Aexample.org/suspend", "request_id": "01J8RQ...", "idempotency_key": "01J8RQ3...", "body": {"reason": "Spam in #general", "notify": false}},
  "outcome": {"status": 200, "problem": null},
  "changes": [{"pointer": "/suspended", "from": false, "to": true}],
  "replayed": false,
  "replay_of": null
}
```

- `request.body` is stored with secrets redacted (`password`, `as_token`, `hs_token`, `access_token`, anything under a `$secret` marker) and truncated at 64 KiB.
- `changes` is best-effort: handlers that know the before and after supply it; otherwise it is empty.
- Entries are immutable and retained per `admin_api.audit_retention` (default 400 days). Export is NDJSON.
- `GET /audit-log` filters: `actor`, `action`, `target_type`, `target_id`, `outcome` (`success`, `failure`), `recorded_after`, `recorded_before`; sort `-recorded_at` (default) or `recorded_at`.
- The `hs-admin` crate exposes `AuditSink` (append and query) for the storage track to implement on `hs-tables`, and an in-memory implementation for tests and the mock.
- Actions performed by the system (scheduled tasks, retention) are recorded with `actor.kind = "system"`.

## 10. The event stream

`GET /api/v1/events` returns `text/event-stream`. Parameters: `types` (repeatable, exact type or a `prefix.*` glob), `resource_type`, `resource_id`; the `Last-Event-ID` header (or `?last_event_id=` for `EventSource` implementations that cannot set headers) resumes after that id.

Frames:

```
id: 01J8RQ6P3ZQ4K7W2X9Y1V5B8C0
event: user.suspended
data: {"id":"01J8RQ6P3ZQ4K7W2X9Y1V5B8C0","type":"user.suspended","recorded_at":"2026-09-17T21:04:05.123Z","resource":{"type":"user","id":"@mallory:example.org"},"actor":{"kind":"user","id":"@ops:example.org"},"data":{"reason":"Spam in #general"}}

: keepalive
```

- `id` is a ULID; ids are monotonic per server (per replica in a cluster, merged by the serving replica).
- A comment line `: keepalive` is sent every 15 seconds; `retry: 3000` is sent at connection start.
- The replay buffer holds the last 10,000 events or 24 hours, whichever is smaller. A `Last-Event-ID` older than the buffer yields a first event `stream.reset` (`data: {"reason": "behind", "oldest_available": "..."}`) and the client refetches the views it renders. A `Last-Event-ID` that is unknown but inside the buffer's window resumes from the next id.
- The stream is filtered by the token's scopes (section 8.2). A token with only `bridges:read` sees `appservice.*`, `task.*` for appservice tasks and `stream.*`.
- Rate: high-volume sources (`stats.snapshot`, `appservice.backlog_changed`, `migration.progress`) are coalesced to at most one event per resource per second.

### 10.1 Taxonomy (open enum; namespaces are closed for v1)

| Namespace | Types | Data |
|---|---|---|
| `stream` | `stream.reset`, `stream.hello` | `hello` carries `server`, `contract`, `replica`, `buffer_oldest_id` |
| `server` | `server.started`, `server.stopping`, `server.config_reloaded`, `server.warning` | sections reloaded; warning text and code |
| `cluster` | `cluster.replica_joined`, `cluster.replica_left`, `cluster.replica_draining`, `cluster.shard_moved`, `cluster.lease_lost` | replica id, shard, from, to, epoch |
| `user` | `user.created`, `user.updated`, `user.deactivated`, `user.reactivated`, `user.erased`, `user.suspended`, `user.unsuspended`, `user.locked`, `user.unlocked`, `user.shadow_banned`, `user.unshadow_banned`, `user.password_reset`, `user.logged_out`, `user.login`, `user.device_added`, `user.device_removed`, `user.threepid_added`, `user.threepid_removed` | the changed members; `login` carries device id, ip (redacted for `moderation:*`), auth provider |
| `room` | `room.created`, `room.updated`, `room.blocked`, `room.unblocked`, `room.deleted`, `room.purged`, `room.alias_added`, `room.alias_removed`, `room.upgraded` | the changed members; `upgraded` carries `replacement_room` |
| `appservice` | `appservice.registered`, `appservice.updated`, `appservice.removed`, `appservice.paused`, `appservice.resumed`, `appservice.tokens_rotated`, `appservice.health_changed`, `appservice.backlog_changed`, `appservice.transaction_failed`, `appservice.transaction_dead_lettered`, `appservice.replay_started`, `appservice.replay_finished` | health (`healthy`, `degraded`, `down`, `paused`, `unknown`), backlog depth and age, transaction id and error |
| `federation` | `federation.destination_backoff`, `federation.destination_recovered`, `federation.key_fetched`, `federation.key_fetch_failed`, `federation.signing_key_rotated` | server name, retry interval, error |
| `media` | `media.quarantined`, `media.unquarantined`, `media.protected`, `media.unprotected`, `media.deleted` | server name, media id, count for bulk |
| `report` | `report.created`, `report.resolved`, `report.deleted` | kind, id, resolution |
| `registration_token` | `registration_token.created`, `registration_token.updated`, `registration_token.deleted`, `registration_token.used` | token (redacted to its last four characters for `moderation:*`) |
| `task` | `task.scheduled`, `task.started`, `task.progress`, `task.succeeded`, `task.failed`, `task.cancelled` | the `Task` |
| `migration` | `migration.started`, `migration.progress`, `migration.paused`, `migration.resumed`, `migration.ready_for_cutover`, `migration.cutover_started`, `migration.completed`, `migration.failed`, `migration.aborted` | the `MigrationStatus` |
| `config` | `config.updated`, `config.reloaded` | section, actor |
| `audit` | `audit.recorded` | the `AuditEntry` (`admin:read` only) |
| `stats` | `stats.snapshot` | the `StatisticsOverview`, every 10 seconds while a client is connected |

Events are produced by the tracks that own the underlying state through the `hs_admin::events::EventBus` (an `mpsc`-backed publisher with the replay buffer), which is the only thing they depend on from this crate.

## 11. Rate limiting

Per-principal token bucket, default 50 requests per second with a burst of 200 for reads and 10 per second with a burst of 50 for writes; the event stream counts as one request. Limits are configurable in `admin_api.rate_limits`. Exceeding returns `429 rate-limited` (section 3.5). Legacy tokens and the CLI's service accounts share the same limiter.

## 12. Generated clients

- TypeScript: generated from the document into `web/src/api/` by track 16's toolchain (`openapi-typescript` plus a thin fetch wrapper that sets `Idempotency-Key`, handles problem details, and exposes an `EventSource` helper with `Last-Event-ID`).
- Rust: `hs-admin-client` (Phase 1) generated with `progenitor` or `openapi-generator`'s Rust template; the CLI's admin subcommands use it.

Both must survive the additive changes of section 3.8: unknown members and enum values are preserved, not rejected.

## 13. Serving the management interface

The built assets of `web/` are embedded (`rust-embed`) and served at `/admin/` with `index.html` fallback for client-side routes, `Cache-Control: immutable` for hashed assets, `no-store` for `index.html`, and a `Content-Security-Policy` that allows only same-origin scripts and connections. The interface obtains its token through 07's OAuth issuer (authorization code with PKCE); no cookies are involved in the API.

## 14. Affected tracks and what they implement

| Track | Provides to this API | Consumes from this crate |
|---|---|---|
| 07 | `TokenVerifier` for OAuth and legacy tokens; the `login-as` token minting; user, device, session and 3PID model operations | `Principal`, `AuthError` |
| 03 | job leases for the task framework; cluster status and drain | `EventBus`, `Task` |
| 11 | appservice registry read and write, health, backlog, replay, tokens | `EventBus` |
| 13 | reloadable configuration sections, migration control; maps `/_synapse/admin` onto `hs_admin::model` | `model::*`, offset paging |
| 04, 09, 06 | room, media and federation model operations | `EventBus` |
| 16 | consumes everything; supplies usability findings as RFC amendments | `openapi.yaml`, `hs-admin-mock` |
| 14 | the `routes.json` manifest emitted by `hs-http` covers `/api/v1` too | |

## 15. Migration of the contract

The document is a draft until week 8. Changes before then are made in place with an entry in the changelog section of the document (`info.x-hs-changelog`). After the freeze, changes follow section 3.8 and go through an RFC amendment.

## 16. Open questions deferred

- Object-level authorization (moderators scoped to rooms): after v1.
- Webhooks as an alternative to the SSE stream for automation: after v1; the audit log export is the interim.
- Whether `GET /users/{user_id}/sessions` should include federation-visible IPs for `moderation:read`: redacted in v1.
