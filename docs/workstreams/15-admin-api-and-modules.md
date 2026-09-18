# 15. Admin API and modules

Wave 1 for the API design and the module hook trait (13 and 16 depend on them); endpoints land from week 8.

**Expert profile.** API designer and backend engineer: resource modeling, OpenAPI, authorization, event streams, extension systems (WebAssembly component model).

**Mission.** A public admin API that is a product in its own right: consistent, documented, versioned, safe to automate against, and the only thing the management interface talks to. Plus an extension system that replaces Synapse's Python modules without Python. See `PLAN.md` sections 4 (D12), 5.5, 9.3, 9.5 and 11.

**Owns.** `hs-admin`: the native admin API under `/api/v1` (users and devices, rooms and moderation actions, media and quarantine, federation destinations and keys, event and user reports, registration tokens, scheduled tasks, statistics, server notices, appservices through 11's registry, cluster status through 03, migration status through 13, reloadable configuration through 13), an OpenAPI 3.1 document generated from the handlers (`utoipa` or equivalent) and published with the binary, cursor pagination, filtering and sorting conventions, RFC 9457 problem-details errors, idempotency keys on mutations, OAuth scopes (`admin:read`, `admin:write`, `bridges:read`, `bridges:write`, `moderation:*`) enforced through 07, an audit log, a server-sent-events stream of server events for live interfaces, rate limits, generated TypeScript and Rust clients, the scheduled-task framework on 03's job leases, server notices; `hs-modules`: the hook trait every track calls, the HTTP-callback module protocol for Synapse's eleven callback categories (spam checker, third-party rules, presence router, account validity, password auth provider, background-update controller, account data, media repository, ratelimit, federation, add-extra-fields-to-unsigned), the WebAssembly host on `wasmtime`, coordination of native ports (LDAP and the REST password provider with 07, shared-secret auth with 11); embedding and serving 16's built assets; `hs-identity` design for after 1.0.

**Provides.** Week 4: the API design document and an OpenAPI draft that 16 mocks against. Week 8: the module hook trait; the admin model that 13 maps the Synapse admin API onto; API v1 contract frozen.

**Consumes.** Every track's models; 07 for authorization; 03 for job leases and cluster status; 11 for appservice data; 13 for configuration and migration.

**Day-one work.** The API design document (resource model, naming, pagination, filtering, errors, idempotency, scopes, versioning and deprecation policy, event stream); the OpenAPI draft and a mock server for 16; the hook trait and the versioned JSON callback protocol; a `wasmtime` component-model feasibility spike.

**Phase 0 deliverables.** Design document reviewed with 16 and 13; OpenAPI draft and mock server; the hook trait crate frozen at week 8; API skeleton with authentication and the audit log; the SSE stream design.

**Phase 1 and 2 deliverables.** Full API v1; generated clients; scheduled tasks; server notices; reports; HTTP-callback modules complete; the WebAssembly host; native ports; the Synapse admin compat mapping with 13; the identity service design.

**Definition of done.** OpenAPI document validates and matches the handlers (contract tests); every resource has list, get and mutation tests including pagination edge cases and authorization; the audit log records every mutation; SSE stream tested under reconnect; module protocol conformance tests with sample modules (a spam checker and a password provider); `synapse-admin` works through 13's compat surface.

**References.** `refs/synapse/docs/admin_api/` and `refs/synapse/docs/modules/` (semantics; behavior only, AGPL); `refs/palpo/crates/server/src/admin/`; the Kubernetes API conventions and the Stripe and GitHub API design guides as models of consistent resource APIs; `wasmtime` component-model documentation.

**Open questions to settle first.** Whether the account-management pages (07) live behind the same API; the SSE stream's event taxonomy; module sandboxing limits.

**Risks.** Designing the API from the server's internals instead of from the operator's tasks; 16's flows are the input, not an afterthought.
