# RFC 0005. The `routes.json` route manifest

Date: 2026-09-18. Status: draft (no RFC existed for this interface; track 14, its consumer, has not started). Owner: track 15 (`hs-http`).

## 1. Motivation

`docs/workstreams/README.md`'s seam table and `docs/workstreams/14-test-and-conformance.md` need a machine-readable inventory of every HTTP route the server serves (Matrix client-server, Matrix federation, application-service, `/_synapse/admin` compatibility, and the native `/api/v1` admin API) to drive the conformance dashboard, generate coverage reports against the Matrix spec, and let tests assert routes are wired up without booting a full server. `hs-http` builds every track's router (client listener, federation listener, admin listener) from a common construction path, so it is the natural place to emit this manifest. No RFC or format existed for it yet, so this RFC defines one; track 14 may amend it once it starts.

## 2. Format

`hs-http::router::RouteManifest` is built alongside the `axum::Router` (`hs_http::router::Builder::route` records each route as it is added; nothing is hand-maintained). `Builder::build()` returns `(Router<S>, RouteManifest)`. The manifest serializes to a file named `routes.json` (path is the caller's choice; the admin listener writes it next to the OpenAPI document, `crates/hs-admin/openapi/routes.json`, by convention, but the type itself has no opinion on where it lands).

```json
{
  "generated_at": "2026-09-18T00:00:00.000Z",
  "routes": [
    {
      "method": "GET",
      "path": "/api/v1/users",
      "surface": "admin",
      "operation_id": "users.list",
      "auth": "admin",
      "required_scope": "admin:read",
      "rate_limited": true
    },
    {
      "method": "POST",
      "path": "/_matrix/client/v3/rooms/{roomId}/join",
      "surface": "matrix-client",
      "operation_id": null,
      "auth": "matrix",
      "required_scope": null,
      "rate_limited": true
    }
  ]
}
```

Top level:

| Field | Type | Notes |
|---|---|---|
| `generated_at` | RFC 3339 string | When the manifest was built (process start, or contract-test run time). Not meaningful across builds; consumers diff `routes`, not this field. |
| `routes` | array of `Route` | One entry per `(method, path)` registered through `hs_http::router::Builder`. Unsorted at insertion; serializers sort by `path` then `method` for stable diffs. |

`Route`:

| Field | Type | Notes |
|---|---|---|
| `method` | string | Upper-case HTTP method (`GET`, `POST`, `PUT`, `PATCH`, `DELETE`, ...). |
| `path` | string | Axum path syntax (`{param}`), which is also OpenAPI 3.1's syntax. An admin-surface path here is the OpenAPI document's `paths` key with its `servers[0].url` base path (`/api/v1`) prepended, since this field records what is actually mounted on the router, not a path relative to the document's own base. |
| `surface` | string, open enum | `matrix-client`, `matrix-federation`, `matrix-appservice`, `synapse-admin-compat`, `admin`. New surfaces may be added; consumers must not reject unknown values. |
| `operation_id` | string or null | The `<resource>.<verb>` id (RFC 0004 section 6) for `admin` routes; null for Matrix routes, which have no equivalent convention. |
| `auth` | string, open enum | `none`, `matrix` (a Matrix access token / `Requester`, track 07), `admin` (an admin API bearer token, section 8 of RFC 0004), `appservice` (an `as_token`). |
| `required_scope` | string or null | For `auth: "admin"` routes, the OAuth scope from RFC 0004 section 8.2; null otherwise or when any valid token suffices (for example `GET /api/v1/me`). |
| `rate_limited` | boolean | Whether the route is subject to `hs_http::ratelimit::RateLimiter`. |

## 3. How it is produced

`hs_http::router::Builder` wraps `axum::Router`: every call to `.route(method, path, handler)` (or the per-verb helpers `.get()`, `.post()`, ...) appends one `Route` to an internal `Vec` using metadata supplied alongside the handler (`surface`, `auth`, `required_scope`, `rate_limited`, and an optional `operation_id`). `hs-admin`'s router (deliverable 5 of track 15) uses the same operation table that generates its section of the manifest, so the admin rows of `routes.json` and `openapi.yaml`'s `paths` agree by construction; `hs-http`'s contract-test helper (`hs_http::router::assert_matches_openapi`) re-derives the OpenAPI path set from a parsed document (prefixed with its `servers[0].url`, see the `path` field's definition above) and asserts the two sets of `(method, path)` for `surface: "admin"` rows are equal, catching drift if either is hand-edited without the other.

## 4. Consumers

- Track 14: coverage dashboard (which spec routes exist, which are missing), route-level test generation, CI gating.
- Track 15: the contract test in `crates/hs-admin/tests/contract.rs` (see section 3).
- Any track debugging "is this route wired up" without booting a server: `cargo run -p hs-http --example dump-routes > routes.json` is not provided (no `hs-http` binary); the manifest is produced by whichever binary builds the full router (`hs-server`, once it exists, or `hs-admin-mock` for the admin surface alone).

## 5. Non-goals

- Not a spec-coverage report (which spec-mandated routes are missing); track 14 diffs this manifest against the spec to produce that.
- Not authentication or authorization enforcement; `auth` and `required_scope` are descriptive metadata, not the enforcement mechanism (see `hs_http::error` for the Matrix error mapping and `hs_admin::auth` for the admin scope trait).
- No versioning field; `routes.json` describes the routes of the binary that produced it, at that moment. Historical comparison is the consumer's job (diff two files).
