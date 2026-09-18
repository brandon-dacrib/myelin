# 15. Admin API and modules: status

Track brief: `docs/workstreams/15-admin-api-and-modules.md`. Owner crates: `hs-admin`, `hs-modules`, `hs-identity`, `hs-http` (shared with 07 and 14).

Last updated: 2026-09-17 (day one, session 1).

## Done

- Read PLAN.md, the workstreams README, the brief, the decisions, and the briefs of 03, 07, 11, 13, 14 and 16. No other track has written code or status yet.

## In progress

- `docs/rfcs/0004-admin-api.md`: the admin API design document (resource model from operator flows, naming, pagination, filtering, errors, idempotency, scopes, versioning, audit log, SSE stream).
- `crates/hs-admin/openapi/openapi.yaml`: OpenAPI 3.1 draft. **Track 16: this is the file to generate the client and mock against. It is not usable until this section says so.**
- `crates/hs-admin` binary `hs-admin-mock`: mock server for track 16 (fixtures, pagination, SSE, fake login issuing scoped tokens).

## Next

- `hs-http`: shared HTTP conventions (router with `routes.json` manifest, Matrix error mapping, RFC 9457 problem details for `/api/v1`, permissive JSON, CORS, body limits, listeners, rate-limiter trait).
- `hs-modules`: hook trait (eleven categories), no-op implementation, versioned JSON HTTP-callback protocol with a reference client, wasmtime feasibility spike.
- `hs-admin` skeleton: utoipa-generated router, auth and scope enforcement trait for 07, audit log trait, contract tests.
- Asset embedding for `web/` at `/admin/`.

## Blockers

None.

## Interfaces provided

- (pending) `docs/rfcs/0004-admin-api.md`, `crates/hs-admin/openapi/openapi.yaml`, `hs-admin-mock`.

## Interfaces needed

- 07: an implementation of the admin token verifier trait (`hs_admin::auth::TokenVerifier`) once the OAuth issuer exists; `Requester` middleware at week 6 for the Matrix-facing routes in `hs-http`.
- 14: the `routes.json` manifest format from `hs-spec-coverage`; if absent, `hs-http` defines it in an RFC.
- 13: the reloadable-configuration schema and the migration status model.
- 03: job leases for scheduled tasks and cluster status.
- 11: the appservice registry read model (health, backlog).

## Decisions made

- (pending; see the RFC)

## Shared dependencies added

- (none yet)
