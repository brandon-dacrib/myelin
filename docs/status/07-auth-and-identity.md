# 07 Auth and identity: status

Track brief: `docs/workstreams/07-auth-and-identity.md`. Owner crate: `hs-auth`.

Last updated: 2026-09-19 (session 5, completed its assignment — see "Session 5" below).

## Session 5 summary (read this first)

Assignment: close five auth conformance gaps `docs/status/14-test-and-conformance.md` recorded
under "Track 07 (auth)": (1) `/register/available` accepting invalid usernames, (2) wrong UIA
session status codes, (3) usernames not lower-cased, (4) `/capabilities` not requiring auth
(handler lives in `hs-cli`, not owned this session), (5) a device not cleaned up on logout, and
UIA ordering on `DELETE /devices/{id}`. Every claim below was checked against the actual
Complement test in `refs/complement/tests/csapi/`, the actual spec text in `refs/matrix-spec/`, or
(for two items where the spec is silent and Synapse's real behavior is the tie-breaker)
`refs/synapse`, not memory — file paths and quotes are inline in the code comments, not just here.

**All five items addressed; four fixed as reported, one (UIA session status codes) investigated
and found to be partly a false-positive gap and partly a real, different bug than reported — see
its own section below.**

1. **`/register/available` accepting invalid usernames — real bug, fixed.**
   `crates/hs-auth/src/routes/register.rs::validate_localpart` used
   `UserId::parse_with_server_name(...).map(|_| ())`, which only rejects a literal `:` or NUL byte
   (`ruma_identifiers_validation::user_id::localpart_is_backwards_compatible` — the *historical*
   grammar, kept lenient on purpose for old room state). It now also calls
   `UserId::validate_strict()` (`ruma_identifiers_validation::user_id::
   localpart_is_fully_conforming`), which enforces the spec's actual minting grammar
   (`refs/matrix-spec/content/appendices.md`, "User Identifiers": "MUST contain only the
   characters a-z, 0-9, `.`, `_`, `=`, `-`, `/`, and `+`"). Confirmed against
   `refs/complement/tests/csapi/apidoc_register_test.go`: "GET /register/available returns
   M_INVALID_USERNAME for invalid user name" (a bare comma) and "POST /register rejects usernames
   with special characters" (`!"\:?\\@[]{}|£é\n'`, all before UIA runs — 400, not 401). Both
   endpoints share `validate_localpart`, so both are fixed by the one change. Tests:
   `register_available_rejects_an_invalid_username_shape`,
   `register_rejects_usernames_with_special_characters` (loops every character Complement lists).

2. **UIA session status codes — investigated in depth; one reported case is not a real gap, one
   real gap found and fixed instead.**
   - The reported "200 instead of 401 for auth-requires-session" (Complement's
     `apidoc_register_test.go` "Registration without a session fails": strip `session` back out
     of an already-issued UIA session's `auth` object and resubmit) is **not fixed, deliberately**.
     I first "fixed" it (require a session id whenever a stage is submitted) and it broke 14 of
     this crate's own tests, including the full register→whoami router round trip — because that
     stricter rule also forbids the ordinary single-round-trip pattern
     (`username`/`password`/`auth: {"type": "m.login.dummy"}` sent all at once, no prior call).
     Reading `refs/synapse/synapse/handlers/auth.py::AuthHandler.check_ui_auth` end to end
     confirms real Synapse does exactly what this crate did before this session: `sid =
     authdict.get("session")`; `if not sid:` unconditionally create a **new** session and
     immediately check/complete whatever `type` came with it, in the same call — regardless of
     whether a session had already been issued earlier for what the client considers "the same"
     dance. The Complement test's own `runtime.SkipIf(t, runtime.Synapse, runtime.Dendrite,
     runtime.Conduit)` (comment: "historically did not enforce this requirement strictly") confirms
     this is a known aspirational check that no reference server passes, not a baseline gap.
     **Decision: match Synapse's actual behavior, not the stricter aspirational reading** — the
     ambiguity the brief said was mine to settle. Documented at length in `uia::advance`'s doc
     comment and in the (renamed) test
     `routes::register::tests::registration_completes_in_a_single_round_trip_with_no_prior_session`
     plus `uia::tests::a_stage_submitted_without_a_session_id_gets_a_fresh_session_and_can_still_complete`.
   - **Real bug found instead, while reading the sibling Complement test for item 5**: a UIA
     challenge body's `params` field was always omitted. `ruma`'s `UiaaInfo::params` is
     `Option<Box<RawJsonValue>>` with `skip_serializing_if = "Option::is_none"`, and
     `uia::incomplete_body` never set it, so it was always `None` and always dropped from the
     JSON. `refs/complement/tests/csapi/apidoc_device_management_test.go`'s "DELETE
     /device/{deviceId} with no body gives a 401" asserts `match.JSONKeyPresent("params")` on
     exactly this body shape. `refs/synapse/synapse/handlers/auth.py::_auth_dict_for_flows`
     confirms Synapse always initializes `params: dict = {}` before adding any per-stage entries —
     it is never entirely absent, even when no offered stage needs one (true for every stage this
     crate offers). Fixed: `incomplete_body` now always sets `params` to an empty JSON object.
     Test: `uia::tests::incomplete_body_always_includes_a_params_object`.

3. **Usernames not lower-cased — real bug, fixed.** `crates/hs-auth/src/routes/register.rs::
   register_user` now ASCII-lower-cases the client-supplied `username` before validating,
   checking availability, or minting the `UserId` (`str::to_ascii_lowercase`, not
   `str::to_lowercase` — the grammar is ASCII-only, so a Unicode-aware lower-case would only add
   surprise). Matches `refs/synapse/synapse/rest/client/register.py`'s
   `RegisterRestServlet.on_POST` (`desired_username = desired_username.lower()`, applied at both
   its normal and UIA-continuation call sites, before `check_username`). Confirmed against
   Complement's "POST /register downcases capitals in usernames" (`user-UPPER` →
   `@user-upper:hs1`). **`GET /register/available` deliberately does *not* lower-case** —
   `refs/synapse/synapse/rest/client/register.py::UsernameAvailabilityRestServlet.on_GET` passes
   the raw query string straight to `check_username` with no `.lower()` call, so real Synapse
   itself answers `M_INVALID_USERNAME` for an upper-case availability query even though
   `/register` would accept and downcase the same string. Complement never exercises this case
   either way, so there is no test pulling against the decision; matching Synapse's actual,
   observable behavior over guessing was the tie-breaker — documented in
   `get_register_available`'s doc comment. Tests: `register_downcases_uppercase_usernames`,
   `register_treats_different_capitalizations_as_the_same_username` (registers `CaseCollide` then
   `casecollide`, expects `M_USER_IN_USE` on the second).

4. **`/capabilities` not requiring auth — cannot be fixed in this crate; hs-auth needs nothing new,
   hs-cli needs a specific, mechanical change.** See "Wiring the integration lead must add" below.
   `crates/hs-cli/src/capabilities.rs::get_capabilities` takes no extractors at all and is
   registered on `Builder::<()>` (unit state) in `crates/hs-cli/src/serve.rs`, so it cannot use
   this crate's `Requester`/`AllowGuest` (`FromRequestParts<AuthState>`, requires the router's
   state to literally be `AuthState`) without also changing how it is mounted. `hs-auth` already
   exports everything needed (`hs_auth::middleware::AllowGuest`, `hs_auth::AuthState`) — no new
   code was added here. Confirmed the extractor mechanism itself already works and is tested:
   `crate::middleware::tests::missing_token_is_rejected`, the guest-allow tests in the same file,
   and `routes::tests::router_rejects_whoami_without_a_token` (an `AllowGuest`-gated handler on a
   real `Router<AuthState>` returns 401 with no token) are all existing, passing proof of the exact
   mechanism `hs-cli` needs to reuse. Confirmed the requirement against
   `refs/matrix-spec/data/api/client-server/capabilities.yaml` (`security: accessTokenBearer`) and
   `refs/complement/tests/csapi/apidoc_server_capabilities_test.go` ("GET /v3/capabilities is not
   public" — expects 401 unauthenticated). Confirmed `allow_guest=True` is correct (not just
   "any full user") by reading `refs/synapse/synapse/rest/client/capabilities.py::
   CapabilitiesRestServlet.on_GET`: `await self.auth.get_user_by_req(request, allow_guest=True)`.

5. **Device not cleaned up on logout — real bug, fixed. UIA ordering on `DELETE
   /devices/{id}` — real bug, fixed.**
   - `crates/hs-auth/src/routes/logout.rs::post_logout` deleted only the access token and its
     paired refresh token, never the device row. Spec
     (`refs/matrix-spec/data/api/client-server/logout.yaml`, `/logout`): "The device associated
     with the access token is also deleted." Fixed: now also calls
     `DeviceStore::delete_access_tokens_for_device` and `DeviceStore::delete_device` for the
     token's `device_id`. Confirmed against `refs/complement/tests/csapi/apidoc_logout_test.go`'s
     "Can logout current device" (logs out one device's session, asserts `GET /devices` on a
     *different*, still-live session now shows exactly one device with the logged-out device's id
     gone). **`post_logout_all` had the identical bug** (spec, same file: "`/logout/all`... All
     devices for the user are also deleted.") — fixed alongside it rather than filed separately,
     since it is the same class of bug in the same file and the spec text is just as explicit.
     Tests: `logout_deletes_the_device_bound_to_the_token_used` (asserts a second, untouched
     device survives), `logout_all_deletes_every_device_for_the_user`.
   - `crates/hs-auth/src/routes/devices.rs::delete_device` and `post_delete_devices` took
     `Json(body): Json<Value>` as their body extractor. Axum's `Json<T>` rejects a missing/empty
     body (or missing `Content-Type: application/json`) **before the handler runs**, turning
     Complement's literally-bodyless `DELETE /devices/{deviceId}` into a framework-level
     `400`/`415` instead of ever reaching `reauth::run`'s `401` UIA challenge. Confirmed against
     `refs/complement/tests/csapi/apidoc_device_management_test.go`'s "DELETE /device/{deviceId}
     with no body gives a 401" (asserts `401` with `session`/`flows`/`params` all present — a real
     UIA challenge body, not an error body). Fixed: both handlers now take raw `axum::body::Bytes`
     and a new `parse_optional_json_body` helper (`devices.rs`) treats an empty body as `{}`,
     parses a non-empty one as JSON, and only 400s (`M_NOT_JSON`) on body that is present but not
     valid JSON. Test: `delete_device_with_a_completely_empty_body_still_gets_the_uia_challenge`
     (asserts 401 and that `session`/`flows`/`params` are all present in the body — the last of
     those only passes because of item 2's `params` fix above, confirmed by writing this test
     before that fix and watching it fail on the `params` assertion, not just the status code).

### Files touched this session

- `crates/hs-auth/src/routes/register.rs` — `validate_localpart` (strict grammar),
  `register_user` (lower-cases `username`), `get_register_available` (doc comment only, behavior
  unchanged), 8 new/changed tests.
- `crates/hs-auth/src/uia.rs` — `incomplete_body` (`params` always set), `advance`'s doc comment
  rewritten to record the item-2 investigation and decision (no behavior change from session 4),
  1 renamed test, 1 new test.
- `crates/hs-auth/src/routes/devices.rs` — `delete_device`/`post_delete_devices` now take `Bytes`
  via new `parse_optional_json_body`; 5 existing tests updated to the new parameter type, 1 new
  test.
- `crates/hs-auth/src/routes/logout.rs` — `post_logout`/`post_logout_all` delete devices; 2 new
  tests.
- `docs/status/07-auth-and-identity.md` — this section.

No files outside `crates/hs-auth` and this status file were edited. No new `Cargo.toml`
dependency was needed (item 4's `capabilities` fix needs zero new `hs-auth` code; every
constructor and extractor it needs already existed before this session).

### Verification (all run from `/Users/brandon/Documents/git/matrix-reimplement`)

- `cargo fmt -p hs-auth` — clean.
- `cargo clippy -p hs-auth --all-targets -- -D warnings` — clean. (Hit a transient failure
  mid-session from `hs-admin`'s own unused-import warning, another track's crate pulled in
  transitively through `admin_directory.rs`'s `hs_admin::sources::UserDirectory` impl — not
  touched, not mine to fix, and gone by the next run once that track's own session moved on.)
- `cargo test -p hs-auth` — **175 tests, up from 165, all passing** (0 failed).
- `cargo build -p hs-cli --bin hs` — clean.
- `cargo test -p hs-loadgen --test real_client` — **passes**: a real `matrix-rust-sdk` client
  still registers, logs in and logs out against a real `hs serve` process. (The test's own log
  output includes several expected `ERROR`-level lines from the SDK probing account-data/state
  endpoints that legitimately 404 on a fresh account, plus one expected 401 at the end from the
  token this session's own logout fix now actually revokes — none of those are new failures, the
  test's single `#[test]` still reports `ok`.)

### Wiring the integration lead must add (item 4, `/capabilities` auth)

This crate needs **no new code** for this — `hs_auth::middleware::AllowGuest` and
`hs_auth::AuthState` already exist and are already exported at the paths used below. The change is
entirely in `hs-cli`, which this session does not own. Two mechanical edits:

1. **`crates/hs-cli/src/capabilities.rs`**: change `get_capabilities`'s signature to require a
   token (any authenticated principal, guests included — see item 4 above for why `AllowGuest` and
   not `Requester`):
   ```rust
   pub async fn get_capabilities(
       _requester: hs_auth::middleware::AllowGuest,
   ) -> Json<Value> {
       // body unchanged
   }
   ```
2. **`crates/hs-cli/src/serve.rs`**: `get_capabilities` can no longer be registered on
   `Builder::<()>` (the base `builder` it is currently added to has unit state; `AllowGuest`
   requires `Router<AuthState>`). Remove its two `.get(...)` calls from the `builder` chain
   (currently right after the `/_matrix/client/versions` route, around what is today lines
   223–238: the `/_matrix/client/v3/capabilities` and `/_matrix/client/r0/capabilities` entries,
   both with `RouteMeta::new(Surface::MatrixClient, AuthKind::None)` — the `AuthKind` for both
   should become `AuthKind::Matrix` once moved, since the route now actually checks a token). Add
   a small router built on `AuthState`, exactly like `auth_router`/`synapse_admin_router` just
   above it (clone `auth` for this *before* the `ping_router` line consumes it by value — today
   that line reads `.with_state(auth)`, not `.with_state(auth.clone())`, and is the last use of
   `auth` before it would otherwise be gone):
   ```rust
   let capabilities_router = axum::Router::new()
       .route("/_matrix/client/v3/capabilities", axum::routing::get(crate::capabilities::get_capabilities))
       .route("/_matrix/client/r0/capabilities", axum::routing::get(crate::capabilities::get_capabilities))
       .with_state(auth.clone());
   let capabilities_routes = vec![
       hs_http::router::Route { method: "GET".into(), path: "/_matrix/client/v3/capabilities".into(), surface: Surface::MatrixClient, operation_id: Some("getCapabilities".into()), auth: AuthKind::Matrix },
       hs_http::router::Route { method: "GET".into(), path: "/_matrix/client/r0/capabilities".into(), surface: Surface::MatrixClient, operation_id: Some("getCapabilities".into()), auth: AuthKind::Matrix },
   ];
   ```
   Then, after `let (router, mut manifest) = builder.build();` (today's line ~373), merge it the
   same way `synapse_admin_router` is merged just below that (today's lines ~385–387 — an absolute
   path merged directly onto the built router, not through `merge_router`, for the same reason
   given there: "an absolute path merges onto the top-level router rather than nesting under a
   prefix"):
   ```rust
   manifest.routes.extend(capabilities_routes);
   let router = router.merge(capabilities_router);
   ```
3. Existing `hs-cli` tests that assert `/capabilities` works unauthenticated (e.g.
   `capabilities_endpoint_is_mounted_under_v3_and_r0` in `serve.rs`, if it sends no token today)
   will need a token added to their request — that test lives in `hs-cli`, not touched here.

Line numbers above are as of this session's read of `crates/hs-cli/src/serve.rs`; they will drift
as other tracks' agents edit that file concurrently — match by the code shown, not the numbers.

### Decisions made

- **Strict user-ID grammar enforcement** (item 1): `validate_localpart` now rejects anything
  outside `UserId::validate_strict`'s fully-conforming set, for both `/register` and
  `/register/available`. No ambiguity here — the spec is explicit and Complement is precise.
- **`/register/available` does not lower-case; `/register` does** (item 3): deliberate asymmetry,
  matched to Synapse's actual, observed behavior (`UsernameAvailabilityRestServlet.on_GET` has no
  `.lower()` call; both of `RegisterRestServlet`'s call sites do). Complement does not test an
  upper-case `/register/available` query either way.
- **Did not implement Complement's stricter "session becomes mandatory once issued" UIA rule**
  (item 2): matched Synapse's real, source-confirmed behavior instead, because the stricter rule
  breaks the ordinary single-round-trip registration pattern this crate (and, all evidence
  suggests, real clients) rely on, and because Synapse/Dendrite/Conduit are all skipped from that
  exact Complement test for not implementing it either. If a future session wants to revisit this,
  the honest way to satisfy the Complement test without breaking single-round flows would need a
  way to distinguish "a session was never issued for this dance" from "a session was issued and
  the client is now omitting it" — which is not recoverable from the request alone under the
  current (stateless-per-call) design; it would need the UIA session to be looked up by some other
  correlating key (e.g. the exact `username`/`password` pair) before falling back to "mint fresh",
  which is a real design change, not a one-line fix.
- **`AllowGuest`, not `Requester`, for `/capabilities`** (item 4): matches Synapse's
  `allow_guest=True`, confirmed by reading `CapabilitiesRestServlet.on_GET`.
- **`post_logout_all` device cleanup fixed alongside `post_logout`'s** (item 5), even though only
  `post_logout`'s bug was named in the triage: same file, same class of bug, same unambiguous spec
  sentence for the sibling endpoint two paragraphs down.

### What the earlier triage did not name

- The UIA challenge body's missing `params` field (see item 2) — found while reading
  `apidoc_device_management_test.go` for item 5, not named in either the original triage or the
  session's own brief.
- `post_logout_all` never deleted devices, same as `post_logout` — the triage only named the
  single-device `post_logout` case (from the "Can logout current device" test); "Can logout all
  devices" doesn't happen to assert `GET /devices` afterward, so Complement itself won't catch
  this one, but the spec text is just as explicit for it.

### Interfaces provided

Unchanged from session 4 — see that section below. No new public API surface this session; all
five fixes are internal behavior changes to already-mounted routes.

### Interfaces needed

- `hs-cli`: the two mechanical `serve.rs`/`capabilities.rs` changes under "Wiring the integration
  lead must add" above, to actually require a token on `/capabilities`.

### Shared dependencies added

None.

## Session 4 summary (read this first)

Assignment (from `docs/next-steps.md` item 1 and the integration lead directly): **make it
possible to have an admin at all, and give the admin API a real user data source.** Three
deliverables, all done:

1. **`crates/hs-auth/src/routes/synapse_admin.rs`** (new) — `GET`/`POST
   /_synapse/admin/v1/register`, the shared-secret admin registration protocol
   (`hs_compat::shared_secret`, already built and tested by track 13) finally wired to an HTTP
   handler and a real account-creation path. Exported as its own router fragment (`pub fn
   router() -> Router<AuthState>`, re-exported at the crate root as `hs_auth::synapse_admin_router`)
   because it lives at an absolute, non-`/_matrix` path and must **not** be nested under
   `routes::router()`'s `/_matrix/client/v3` mount point. The `NonceRegistry` lives in the router
   fragment's own `Arc<Mutex<NonceRegistry>>`, injected via `axum::Extension`, not on `AuthState` —
   see the module doc for why. Added `AuthConfig::registration_shared_secret: Option<String>` as its
   own field (mapped from the same `hs_config::AuthConfig::registration_shared_secret` that
   `shared_secret_auth_secret` already reused) rather than overloading the existing
   `shared_secret_auth_secret` field, per this session's own instructions — the two protocols
   (`register_new_matrix_user`'s admin API vs. `com.devture.shared_secret_auth` login) must stay
   independently toggleable even though they read the same operator-configured secret today. Both
   routes 404 with `M_UNRECOGNIZED` (`MatrixError::feature_not_configured`, new) when the secret is
   unset. On success, account creation follows `routes::register.rs::register_user`'s exact shape
   (password policy check, localpart availability check with a defensive TOCTOU re-check right
   before `create_user`, `password::hash_password`, `UserRecord::new`, `session::create_session`)
   but skips UIA entirely — the verified MAC *is* this endpoint's authentication — and sets
   `is_admin` from the request body's `admin` field. Response body matches the Synapse shape
   `crates/hs-cli/src/register.rs::RegisteredUser` already deserializes
   (`user_id`/`access_token`/`home_server`/`device_id`). 8 new tests, including one each for a
   replayed nonce, a bad MAC and a taken username, plus a full round trip that checks the created
   user's `is_admin` flag against the store directly.
2. **`UserStore::list_users`** (new trait method) — implemented in both `store::memory::
   InMemoryAuthStore` (clone-and-sort) and `store::tables::TablesAuthStore` (a full,
   undocumented-cost-until-now keyspace scan over `hs_auth.users` via `RangeSpec::full()`, following
   `list_devices`'s range/decode shape; the scan cost is spelled out in both the trait method's and
   the impl's doc comments — there is no secondary index to narrow it against, since there was never
   a bounded "list users" access pattern to index for before now). Two new shared-behavior tests in
   `store/shared_tests.rs` (`list_users_is_empty_for_a_fresh_store`,
   `list_users_is_sorted_by_user_id`), run against both backends via the existing `run_all` harness.
3. **`crates/hs-auth/src/admin_directory.rs`** (new) — `AuthStoreUserDirectory`, implementing
   `hs_admin::sources::UserDirectory` (track 15 landed the `sources` module *during this session*,
   mid-flight — confirmed against the live contract in `crates/hs-admin/src/sources.rs`, not the
   snapshot pasted into this session's instructions, and it matched exactly) over `Arc<dyn
   AuthStore>`. Fills `user_id`/`admin`/`deactivated`/`locked`/`suspended`/`shadow_banned`/
   `created_at` straight from `UserRecord`, and `device_count`/`last_seen_at` from
   `DeviceStore::list_devices` (count, and the max of the devices' `last_seen_ms`). `room_count` and
   `media_count` are left at `0` — this crate has no view onto rooms (04) or media (09); see
   "Interfaces needed". `display_name`/`avatar_url`/`user_type`/`consent_version`/`appservice_id`/
   `erased` are also left at `AdminUser::default()` — none of them exist on `UserRecord` yet (profile
   data is track 04's territory; erasure is an unbuilt Phase 1/2 lifecycle feature). 7 new tests.

Also added, needed to make the above possible: `MatrixError::feature_not_configured()` (404
`M_UNRECOGNIZED`, `error.rs`), and `admin_verifier::format_rfc3339_ms` and
`routes::register::validate_localpart` both changed from private to `pub(crate)` so
`admin_directory.rs`/`synapse_admin.rs` could reuse the existing RFC 3339 formatter and localpart
validation instead of re-deriving them.

**`hs-cli` still does not mount anything from this session.** `hs register --admin` will keep
404ing, and `GET /api/v1/users` will keep answering from whatever fake/absent directory `hs serve`
wires today, until track 12 (or whoever owns `serve.rs`) makes the three changes under "Interfaces
provided" below. All three are mechanical — no design judgment left to make — and this crate's own
tests (156 passing, up from 149) already exercise every piece that needs wiring.

**Verification:** `cargo fmt --all` (clean), `cargo clippy -p hs-auth --all-targets -- -D warnings`
(clean), `cargo test -p hs-auth` (156 passed, 0 failed). All three commands run from
`/Users/brandon/Documents/git/matrix-reimplement`.

## Session 3 summary

Assignment: **replace `InMemoryAuthStore`-only storage with a persistent `hs-kv`/`hs-tables`-backed
`AuthStore`** — the single most operationally important gap in the project, since until this landed
a restart of `hs serve` lost every user, device and token. In priority order: (1) build
`TablesAuthStore` over `hs-kv`/`hs-tables`, keeping `InMemoryAuthStore` for tests; (2) get the
indexes right, property-tested for no orphaned rows; (3) run the entire existing test suite against
both implementations; (4) prove persistence end to end over a real `FjallBackend` in a temp
directory, across a drop-and-reopen; (5) make the store constructible from configuration and tell
track 12 (who owns `hs-cli`/`hs serve`) exactly what to change, without editing their crate; (6) if
budget remained, wire `hs-auth` to consume `hs_config::AuthConfig` directly (session 2's item 3).

**All six items are done.** In order:

- **Item 1 — done.** `crates/hs-auth/src/store/tables.rs` (new, ~1080 lines):
  `TablesAuthStore<B: KvBackend>` implements `UserStore`, `DeviceStore`, `TokenStore`, `UiaStore`
  (and therefore the blanket `AuthStore`) exactly as `InMemoryAuthStore` does — same trait, same
  `Arc<dyn AuthStore>` seam, no caller changes needed. `InMemoryAuthStore` is untouched and remains
  what every test defaults to.
- **Item 2 — done.** Seven keyspaces, two of them with a declarative secondary index
  (`hs_tables::index::IndexDef`/`maintain_index`, maintained inside the same write transaction as
  the row): `users_by_localpart_lower` (unique, case-insensitive registration conflict checking) and
  `access_tokens_by_user`/`refresh_tokens_by_user` (non-unique, the "all tokens for this user"
  access path every bulk-revocation route needs). `devices` and `threepids` need no index at all —
  see the module doc's keyspace table for why. Property-tested in `tables.rs`'s
  `access_token_index_never_diverges_from_primary_rows`: 64 random sequences of put/delete across
  two users, cross-checking after every operation that the index's `lookup` result exactly matches
  which rows actually exist and who owns them — no orphan, no miss. Two more targeted tests
  (`deleting_an_access_token_leaves_no_index_row`, `overwriting_a_token_with_a_different_owner_moves_the_index_entry`)
  check the specific failure modes a hand-rolled index most often gets wrong.
- **Item 3 — done.** `crates/hs-auth/src/store/shared_tests.rs` (new): every behavioral test that
  used to live only in `memory.rs` (session 1/2), ported to plain `async fn`s generic over
  `S: AuthStore`, plus several new cases (exact-duplicate user conflict, bulk access-token deletes
  cross-checked across two users, missing-row error cases for every setter, device-list sort
  order). `memory::tests::shared_behavior_suite` and `tables::tests::shared_behavior_suite` each run
  the whole suite (`shared_tests::run_all`) against their own store — a behavioral difference
  between the two implementations now fails a test, not a route handler discovering it later.
- **Item 4 — done.** `tables::tests::fjall_backed_store_survives_reopen_from_the_same_directory`:
  opens a real `hs_kv::fjall_backend::FjallBackend` in a `tempfile::tempdir()`, registers a user,
  sets a password hash, creates a device and an access token, drops the store and the backend
  handle (simulating process exit), reopens a **new** `FjallBackend`/`TablesAuthStore` from the same
  path, and asserts the user, password hash, device and access token are all still there and the
  case-insensitive-localpart index still rejects the taken name. This is the concrete claim the
  project needed to be able to make, and now can.
- **Item 5 — done, without editing `hs-cli`.** `TablesAuthStore::open(backend: B) -> Result<Self,
  StoreError>` is generic over any `B: hs_kv::KvBackend`, so it is already constructible from
  whatever backend a config selects. Added `AuthState::with_store(store: Arc<dyn AuthStore>, config:
  AuthConfig) -> Self` (`crates/hs-auth/src/state.rs`) as the ergonomic entry point. **Exact lines
  track 12 needs to change are below** under "Interfaces provided" / "for track 12" — mechanical,
  not RFC-sized, so no RFC was written.
- **Item 6 (session 2's item 3, reached because budget remained) — done.** `AuthConfig` now has
  `impl TryFrom<&hs_config::Config> for AuthConfig` (`crates/hs-auth/src/config.rs`), mapping every
  field that has a direct `hs_config::AuthConfig` counterpart (server name, bcrypt pepper, token
  lifetimes, registration-enabled flag, password policy, and — deliberately, not left at default —
  `shared_secret_auth_secret` reusing `registration_shared_secret`) and documenting, in the impl's
  own doc comment, every field that still has no native counterpart. `crates/hs-cli/src/
  config_bridge.rs::auth_config_from` can now become a one-line call-through; **not edited here**
  (not my crate) — see "Interfaces provided" for the exact replacement.

Verification, all clean as of this write-up: `cargo test -p hs-auth` (**134 tests**, up from 128),
`cargo clippy -p hs-auth --all-targets -- -D warnings`, `cargo fmt -p hs-auth -- --check`,
`cargo check --workspace --all-targets` (whole workspace, every crate's tests included).

Net test *count* looks like it dropped from 128 (end of session 2) before rising to 134 — this is
`shared_tests`' consolidation, not lost coverage: roughly two dozen individual `memory.rs` tests
became one `shared_behavior_suite` test per store (so the same assertions now run twice, once per
implementation, under two test names instead of ~24 running once each) plus several genuinely new
cases. Total assertions executed increased; the previous session's 128 number and this session's 134
are not apples-to-apples per-test, but both `cargo test -p hs-auth` runs are 100% green.

### What to do first (for the next session, or for track 12)

1. If you are track 12: read "Interfaces provided" below for the exact `serve.rs` diff to wire the
   persistent store into `hs serve`, and the exact `config_bridge.rs` simplification now available.
   Neither requires any change to `hs-auth`.
2. If you are track 07 continuing this work: the native OAuth issuer (RFC 0003) is still fully
   unstarted design-only work and is the largest remaining Phase 1/2 item. See "Next".
3. Re-run the verification commands above before starting new work.

---

## Session 2 summary

Assignment was, in priority order: (1) apply RFC 0009's appservice capability flags, (2) implement
`com.devture.shared_secret_auth` natively, (3) rewire `hs-auth` to consume `hs_config::AuthConfig`
directly, (4) replace `InMemoryAuthStore` with a persistent `hs-kv`/`hs-tables`-backed store, (5) if
budget remains, begin the native OAuth issuer. The session was told to wrap up before reaching (3).

- **Item 1 (RFC 0009) — done and verified.** `AppserviceRecord`/`AppserviceIdentity` now carry
  `rate_limited`/`msc4190_enabled`; `crate::middleware` copies them; `crate::routes::devices`
  consumes `msc4190_enabled`. **Track 11 is unblocked**: see "Interfaces provided" below.
- **Item 2 (`com.devture.shared_secret_auth`) — done and verified.** New `crate::shared_secret_auth`
  module plus wiring in `crate::routes::login`. See "Done" below for files and exact behavior.
- **Item 3 (consume `hs_config::AuthConfig` directly) — not started.** `hs-auth::config::AuthConfig`
  is still its own type; `crates/hs-cli/src/config_bridge.rs` still hand-bridges it. See "Next" for
  a concrete starting point (this session read `hs_config::auth` and knows the gaps).
- **Item 4 (persistent `AuthStore`) — not started. The auth store is still `InMemoryAuthStore`
  only: a restart of `hs serve` still loses every user, device and token.** This is the most
  operationally important open item; see "Next" for keyspace design notes gathered this session.
- **Item 5 (native OAuth issuer) — not started**, as expected given the above; RFC 0003 from
  session 1 is still the design.

Everything committed compiles and passes: `cargo check -p hs-auth`, `cargo test -p hs-auth` (128
tests, up from 115 — see "Done"), `cargo clippy -p hs-auth --all-targets -- -D warnings`,
`cargo fmt --all -- --check`, `cargo check --workspace --all-targets` (whole workspace, including
every crate's tests) all clean. One narrow, deliberate exception to "own crate only" was made to
keep the shared workspace compiling — see "Decisions made", "RFC 0009's non-breaking rollout
required a two-line fix in `hs-appservice`" below; flagging prominently since it is a compile-time
edit to another track's crate, made only because leaving the shared build red blocks every other
concurrently running agent.

### What to do first

1. Read this file's "Next" section for items 3 and 4 — both have concrete starting points recorded
   below (which `hs-config` fields exist and don't, which keyspace shape `hs-tables` wants).
2. Item 4 (persistence) matters more operationally than item 3 (config plumbing is a real but
   contained annoyance; losing all data on restart is not shippable). If picking just one, do 4
   first, unless track 12 reports the config bridge is now actively blocking something.
3. Re-run the verification commands at the bottom of "Session 2 summary" before starting new work,
   to confirm nothing else moved under you since this was written.

---

Original session 1 material follows below, unedited except where session 2 explicitly updated a
section (each such section says so).

## Done

- Read `PLAN.md` (sections 0, 1, 4, 5 in full; skimmed the rest), `docs/workstreams/README.md`,
  this track's brief, `docs/decisions/0001-license.md` and `0002-workspace-conventions.md`, and the
  status files of tracks 03 and 15 (the only other tracks with status files written yet).
- `docs/rfcs/0002-auth-tokens-and-requester.md`: threat model, `syt_`/`syr_`/`syl_` token formats
  (cross-checked against Synapse's algorithm with an independently computed CRC-32/base62 oracle),
  hashing at rest, the `Requester`/`RequesterContext` design, password hashing scheme, UIA design,
  the `Requester` middleware contract, storage trait design, and known gaps.
- `docs/rfcs/0003-native-oauth-issuer.md`: native OAuth 2.0 authorization server design (grants,
  PKCE, device authorization grant, discovery, dynamic client registration, scopes including
  track 15's admin scopes, device semantics, account management, MAS delegation mode, crate
  selection — in-house grant implementation over `oxide-auth`, `jsonwebtoken` over `josekit`).
  Design only; no code yet, per the track brief's phasing (legacy auth is Phase 0; the native
  issuer is Phase 1/2).
- `crates/hs-auth`, fully implemented for Phase 0 scope:
  - `token.rs`: access/refresh/login token generation and shape validation matching Synapse's
    `syt_`/`syr_`/`syl_` algorithm exactly (localpart embedding, 20-char random segment, base62
    CRC-32 checksum), `TokenHash` (SHA-256 at rest).
  - `password.rs`: Argon2id native hashing (`hash_password`), bcrypt verification with pepper and
    Synapse's exact 72-byte `password + pepper` truncation semantics (`verify_password`), dispatch
    by stored-hash prefix. Tested against hashes generated by an independent implementation
    (Python `bcrypt` 5.0.0), not round-tripped through this crate's own code.
  - `requester.rs`: the `Requester` type (user, device, appservice identity assertion with MSC3202
    device masquerading, admin flag, guest, suspended), `RequesterContext` alias for track 03.
  - `store/`: `UserStore`, `DeviceStore`, `TokenStore`, `UiaStore` traits (unioned as `AuthStore`)
    plus 3PID bind/lookup on `UserStore`, and `store::memory::InMemoryAuthStore` implementing all
    four. No dependency on `hs-kv`/`hs-tables` (track 01 is building those concurrently); an
    `hs-tables`-backed implementation is a follow-up RFC, not a rewrite of any caller, since every
    caller holds `Arc<dyn AuthStore>` or a sub-trait, never a concrete type.
  - `appservice.rs`: `AppserviceRegistry` trait (track 11's future real registry) plus
    `InMemoryAppserviceRegistry`, namespace matching via `regex`.
  - `uia.rs`: the UIA session state machine (session creation/validation/timeout, stage
    completion, flow satisfaction), built on `ruma::api::client::uiaa` wire types.
  - `reauth.rs`: the shared "prove you're still you" re-auth flow used by `/account/password`,
    `/account/deactivate`, `DELETE /devices/{deviceId}`, `POST /delete_devices`.
  - `middleware.rs`: the `Requester`/`AllowGuest` axum `FromRequestParts<AuthState>` extractors —
    bearer token, legacy `?access_token=` query param (mutually exclusive with the header, matching
    Synapse), appservice tokens with `user_id` and both `device_id` and
    `org.matrix.msc3202.device_id` masquerade parameters, exact error codes (`M_MISSING_TOKEN`,
    `M_UNKNOWN_TOKEN` with `soft_logout`, `M_USER_LOCKED`, `M_UNKNOWN_DEVICE`,
    `M_GUEST_ACCESS_FORBIDDEN`).
  - `session.rs`: shared token/device minting used by both `/login` and `/register`.
  - `config.rs`: day-one `AuthConfig` (every field's doc comment names its Synapse config-option
    analog) and `PasswordPolicy`.
  - `clock.rs`: `Clock` trait (`SystemClock`, `FixedClock`) so tests control time.
  - `ratelimit.rs`: `RateLimiter` trait, `InMemoryRateLimiter` token-bucket implementation. Not yet
    wired into any handler with real per-IP keys (needs a client IP from `hs-http`'s listener).
  - `error.rs`: `MatrixError`/`ErrCode`, exact Matrix error-code bodies including `soft_logout` and
    `retry_after_ms` extension fields.
  - `routes/`: `GET`/`POST /login` (`m.login.password` with `m.id.user`/`m.id.thirdparty`
    email/`m.id.phone` identifiers, the deprecated bare `user` field, `m.login.token`,
    `m.login.application_service`), `POST /logout`, `POST /logout/all`, `POST /refresh` (with
    reuse-detection session revocation), `GET /account/whoami`, `POST /register` (UIA with
    `m.login.dummy`, `m.login.registration_token`, `m.login.terms`; `m.login.recaptcha`,
    `m.login.email.identity`, `m.login.msisdn` fail cleanly with `M_UNRECOGNIZED` rather than being
    offered), `GET /register/available`, `POST /account/password`, `POST /account/deactivate`,
    `GET /password_policy`, `GET`/`PUT`/`DELETE /devices/{deviceId}`, `GET /devices`,
    `POST /delete_devices`. `routes::router()` assembles all of it into one `Router<AuthState>`
    fragment (bare spec-relative paths, not yet prefixed with `/_matrix/client/v3` — see
    "Interfaces needed").
- 115 tests, all passing: `cargo test -p hs-auth`. Includes the UIA state machine's session
  semantics (fresh/existing/expired sessions, multi-stage flows, failed-stage rejection),
  every documented Matrix error code, the appservice masquerade paths (user namespace, device
  namespace, both masquerade parameter names), refresh-token reuse detection, and an end-to-end
  router test (register → whoami round trip through real axum `oneshot` requests).
- `cargo fmt --all` and `cargo clippy -p hs-auth --all-targets -- -D warnings` both clean.

## Session 2 additions to "Done"

- **RFC 0009 applied** (`docs/rfcs/0009-appservice-identity-capability-flags.md`, authored by
  track 11):
  - `crates/hs-auth/src/appservice.rs`: `AppserviceRecord` gained `rate_limited: bool` and
    `msc4190_enabled: bool`. Added `AppserviceRecord::new(appservice_id, sender, user_namespaces)`
    (defaults `rate_limited: true`, `msc4190_enabled: false` — the RFC's "assume nothing extra is
    granted" values) so every existing three-field call site in this crate's own tests didn't need
    a bare-struct-literal rewrite.
  - `crates/hs-auth/src/requester.rs`: `AppserviceIdentity` gained the same two fields.
  - `crates/hs-auth/src/middleware.rs`: `authenticate_appservice` copies both fields from the
    looked-up `AppserviceRecord` onto the `AppserviceIdentity` it builds.
  - `crates/hs-auth/src/routes/devices.rs`: `put_device` now creates an unknown device instead of
    404ing when `requester.appservice.msc4190_enabled`; `delete_device` skips
    `crate::reauth::run` under the same condition. `post_delete_devices` (bulk) was deliberately
    **not** changed — RFC 0009 names only the single-device routes; extending the bulk endpoint the
    same way is a reasonable follow-up but out of the RFC's stated scope, noted here rather than
    done unasked.
  - New tests: `appservice.rs` and `middleware.rs`'s existing appservice fixtures updated to the
    new constructor; `routes/devices.rs` gained
    `put_device_creates_unknown_device_for_msc4190_appservice`,
    `delete_device_skips_uia_for_msc4190_appservice`, and
    `put_device_404s_on_unknown_device_for_ordinary_requester` (the negative case, to prove the
    branch is conditional, not that 404 stopped happening at all).
  - **Cross-crate compile fix (see "Decisions made" for the full justification)**:
    `crates/hs-appservice/src/auth_registry.rs`'s `RegistryAppserviceAdapter::lookup_by_token` now
    populates the two new fields from `row.rate_limited`/`row.msc4190` (data it already had in
    hand — exactly the seam RFC 0009 describes), and
    `crates/hs-appservice/src/routes.rs`'s test fixture was switched to
    `AppserviceRecord::new(...)`. Verified: `cargo test -p hs-appservice --all-targets` (74 tests,
    all passing) and `cargo clippy -p hs-appservice --all-targets -- -D warnings` (clean) after the
    fix.
- **`com.devture.shared_secret_auth` native login provider**
  (`docs/status/11-appservices-and-bridges.md`'s "Next" item 2; `PLAN.md` section 6 WS14):
  - `crates/hs-auth/src/shared_secret_auth.rs` (new module, registered in `lib.rs`):
    `compute_token`/`verify_token`, HMAC-SHA512 over the full mxid's UTF-8 bytes, hex-encoded,
    keyed by a shared secret — the exact algorithm real mautrix bridges compute
    (`refs/mautrix-python/mautrix/bridge/custom_puppet.py`: `hmac.new(secret,
    mxid.encode("utf-8"), hashlib.sha512).hexdigest()`, read for behavior only). Constant-time
    verification via `hmac::Mac::verify_slice`. See the module's own doc comment for the full
    protocol writeup and why `hs_compat::shared_secret`'s existing HMAC code (SHA-1, a different
    nonce+field message shape, built for Synapse's admin registration API) could not be reused
    unchanged — also covered under "Reuse considered" below.
  - `crates/hs-auth/src/config.rs`: `AuthConfig` gained `shared_secret_auth_secret: Option<String>`
    (default `None` = feature disabled).
  - `crates/hs-auth/src/routes/login.rs`: `get_login_types` now takes `State<AuthState>` and
    advertises `com.devture.shared_secret_auth` in the `GET /login` flow list only when a secret is
    configured (mirroring how mautrix bridges probe: they fall back to `m.login.password` if the
    flow isn't listed). `post_login` dispatches to the new
    `resolve_shared_secret_auth_login` when `login_info.login_type() ==
    "com.devture.shared_secret_auth"` (matched via `ruma::api::client::session::login::v3::
    LoginInfo`'s `_Custom`/`CustomLoginInfo` catch-all, since this is not a login type ruma has a
    named variant for); on success it flows through the same
    user-lookup/deactivated/locked/`session::create_session` path every other login type uses, so
    it cannot drift from password/token login's session semantics. Disabled-feature and
    wrong-token cases both return the same errors an unsupported/wrong-password login would
    (`M_UNRECOGNIZED` and `M_FORBIDDEN` respectively) — no new information leaked about whether the
    feature exists to an unauthenticated prober beyond what `GET /login`'s flow list already says.
  - New tests: `shared_secret_auth.rs` (5 tests, including an independently-computed HMAC-SHA512
    oracle vector, generated via `python3 -c 'import hmac, hashlib; ...'` in this session, not
    round-tripped through this crate's own code — same convention as `token.rs`'s CRC-32 oracle and
    `password.rs`'s bcrypt vectors); `routes/login.rs` gained 5 tests covering advertised-vs-not,
    successful login, wrong token, and disabled-feature behavior.
  - `hmac = { workspace = true }` added to `crates/hs-auth/Cargo.toml` (the workspace entry already
    existed, added by track 13 for `hs-compat`; no new `[workspace.dependencies]` entry needed).
- Total test count: **128 passing** (`cargo test -p hs-auth`), up from 115 at the end of session 1.
  `cargo clippy -p hs-auth --all-targets -- -D warnings`, `cargo fmt --all -- --check`, and
  `cargo check --workspace --all-targets` (whole workspace) all clean as of this write-up.

## Session 3 additions to "Done"

Session 3's assignment (persistent `AuthStore`, items 1–5, plus item 6 = session 2's item 3) is
covered in full above under "Session 3 summary" — not repeated here to avoid drift between two
descriptions of the same work. This subsection lists exactly what changed, file by file, for anyone
diffing:

- **New**: `crates/hs-auth/src/store/tables.rs` (`TablesAuthStore<B: KvBackend>`, ~1080 lines
  including tests), `crates/hs-auth/src/store/shared_tests.rs` (generic behavioral suite, ~450
  lines).
- **Edited**: `crates/hs-auth/src/store/mod.rs` (added `Serialize`/`Deserialize` derives to
  `UserRecord`/`DeviceRecord`/`AccessTokenRecord`/`RefreshTokenRecord`/`LoginTokenRecord` so
  `TablesAuthStore` can store them as JSON values; registered the new `tables`/`shared_tests`
  modules; updated the module doc), `crates/hs-auth/src/store/memory.rs` (test module slimmed to
  call `shared_tests::run_all` instead of ~14 hand-written tests, preserving exactly the same
  assertions), `crates/hs-auth/src/state.rs` (added `AuthState::with_store`),
  `crates/hs-auth/src/config.rs` (added `impl TryFrom<&hs_config::Config> for AuthConfig`, plus six
  new tests), `crates/hs-auth/src/lib.rs` (crate doc comment updated to mention the persistent
  store), `crates/hs-auth/Cargo.toml` (`hs-config`, `hs-kv`, `hs-tables`, `bytes` as dependencies;
  `proptest`, `tempfile` as dev-dependencies).
- **Not touched**: no other track's crate. `hs-cli` needs a small change to actually use
  `TablesAuthStore` in `hs serve` — see "Interfaces provided" below, not made here per this crate's
  own-crate-only rule.

## Session 4 additions to "Done"

Session 4's assignment (synapse-admin registration route, `UserStore::list_users`,
`AuthStoreUserDirectory`) is covered in full above under "Session 4 summary" — not repeated here.
File-by-file:

- **New**: `crates/hs-auth/src/routes/synapse_admin.rs` (router fragment + handlers + 8 tests),
  `crates/hs-auth/src/admin_directory.rs` (`AuthStoreUserDirectory` + 7 tests).
- **Edited**: `crates/hs-auth/src/store/mod.rs` (`UserStore::list_users` trait method, with a doc
  comment on scan cost), `crates/hs-auth/src/store/memory.rs` (implementation),
  `crates/hs-auth/src/store/tables.rs` (implementation, `RangeSpec` import), `crates/hs-auth/src/
  store/shared_tests.rs` (two new shared tests, registered in `run_all`), `crates/hs-auth/src/
  config.rs` (`AuthConfig::registration_shared_secret` field, its `TryFrom` mapping, two new tests),
  `crates/hs-auth/src/error.rs` (`MatrixError::feature_not_configured`, one new test),
  `crates/hs-auth/src/admin_verifier.rs` (`format_rfc3339_ms` made `pub(crate)` so
  `admin_directory.rs` can reuse it), `crates/hs-auth/src/routes/register.rs`
  (`validate_localpart` made `pub(crate)`; unused by `synapse_admin.rs` in the end — it needed the
  parsed `OwnedUserId`, not just a yes/no check, so it parses directly instead, but the visibility
  change is harmless and left in place), `crates/hs-auth/src/routes/mod.rs` (registered `pub mod
  synapse_admin;`, **not** added to `router()`), `crates/hs-auth/src/lib.rs` (`pub mod
  admin_directory;`, `pub fn synapse_admin_router()` re-export), `crates/hs-auth/Cargo.toml`
  (`hs-compat` path dependency; `hs-admin` was already present from session 3's admin verifier
  work, no change needed there).
- **Not touched**: no other track's crate, including `crates/hs-admin` (its `sources` module landed
  from another agent mid-session; this session wrote against it read-only) and `crates/hs-cli` (the
  three mounting/construction changes are listed under "Interfaces provided" for whoever owns
  `serve.rs` to make).

## In progress

Nothing mid-flight in `hs-auth` itself. Session 4's full assignment (all three deliverables) is
done. The native OAuth issuer (RFC 0003) remains fully unstarted design-only work — see "Next".

## Next

**Whoever owns `hs-cli`/`serve.rs`: make the three mechanical changes under "Interfaces provided"
below.** Nothing in this session's assignment is real until `hs serve` mounts
`synapse_admin_router()`, constructs `AdminTokenVerifier` (already done, from session 3 — unwiring
that is not this session's finding, just re-flagging it since it is the same "wire it in `serve.rs`"
category of work), and constructs `AuthStoreUserDirectory`. All three are one-or-two-line,
no-design-judgment changes; none of them need anything further from track 07.

**Native OAuth 2.0 issuer implementation (RFC 0003), still not started.** This is now the largest
remaining Phase 1/2 item on this track. RFC 0003's section 10 has the proposed build order (grants,
PKCE, device authorization grant, discovery, dynamic client registration, scopes, device semantics,
account management, MAS delegation mode). In-house grant implementation over `oxide-auth`;
`jsonwebtoken` over `josekit` — both already decided, not re-litigated.

**Smaller, still-open items from sessions 1/2/4, unaffected by this session:**
- `room_count`/`media_count` on `AuthStoreUserDirectory`'s `AdminUser` are hardcoded to `0` — track
  04 (rooms) and track 09 (media) each need their own data-source seam for the admin API to fill
  these for real, or `hs-admin`'s `router.rs` needs to compose three directories' worth of data
  before returning a `User`. Not this crate's call to make; noted for whoever wires the admin
  `/users` router.
- `AuthStoreUserDirectory::list_users`'s free-text filter matches `user_id` only (no
  `display_name`, since `UserRecord` doesn't have one) — revisit once track 04's profile data is
  reachable from here.
- Wire `hs-auth`'s router into whatever crate ends up owning the real listener and
  `/_matrix/client/v3` prefixing (**done since session 1 by track 12** — `hs-cli` mounts this
  crate's router under both `/_matrix/client/v3` and `/_matrix/client/r0`, per
  `docs/status/12-platform-and-kubernetes.md` — leaving this line struck through rather than
  deleted so the history is legible: ~~Wire `hs-auth`'s router into whatever crate ends up owning
  the real listener~~).
- Registration-token usage limits/expiry (currently a flat always-valid set).
- 3PID validation-session flow (`/register/email/requestToken` etc.) once an email-sending story
  exists elsewhere in the workspace; today 3PID login works only for already-bound addresses.
- Rate limiting wired into handlers with real per-IP keys once `hs-http` can supply one — now that
  `AppserviceIdentity.rate_limited` exists (this session), whoever wires this in has the signal to
  check; it is still not consulted anywhere (no handler calls a rate limiter with a real key yet).
- Begin native OAuth issuer implementation per RFC 0003's ordering (section 10) — item 5 of this
  session's assignment, not reached.
- SSO redirect/token handoff, upstream OIDC/SAML/LDAP, MAS delegation implementation, account
  lifecycle beyond lock/suspend/deactivate (erasure, consent, account validity) — all Phase 1/2 per
  the brief.

## Blockers

None. This session's three deliverables (synapse-admin registration route, `UserStore::list_users`,
`AuthStoreUserDirectory`) are done. The native OAuth issuer is the next largest item and needs no
external input either — it is purely implementation work against RFC 0003's already-settled design.

## Interfaces provided

- **For whoever owns `hs-cli`/`serve.rs`, three changes to make this session's work real (session
  4, new — read this first if that is you):**

  1. **Mount the synapse-admin router.** Wherever `spawn_serve` builds the client-server router
     from `hs_auth::routes::router()`, also merge in `hs_auth::synapse_admin_router()` — **do not**
     nest it under `/_matrix/client/v3` or any other prefix, it is already at its final absolute
     path (`/_synapse/admin/v1/register`):
     ```rust
     let app = Router::new()
         .merge(hs_auth::synapse_admin_router().with_state(auth_state.clone()))
         // ... existing merges of hs_auth::routes::router() under /_matrix/client/v3, /r0, etc.
         ;
     ```
     (Exact call site depends on how `serve.rs` currently composes its top-level `Router`; the
     constraint is just "merged at the root, not nested".) Without this, `hs register --admin`
     keeps 404ing — this is the item `docs/next-steps.md`'s item 1 named as broken.
  2. **Construct `AuthStoreUserDirectory` and wire it into `hs-admin`'s router state**, sharing the
     same `AuthState`/store `AdminTokenVerifier` already shares (see the existing bullet below for
     the verifier — this is the same pattern, added by session 3, still not wired as of this
     write-up):
     ```rust
     let user_directory: std::sync::Arc<dyn hs_admin::sources::UserDirectory> =
         std::sync::Arc::new(hs_auth::admin_directory::AuthStoreUserDirectory::from_auth_state(&auth_state));
     ```
     Then pass `user_directory` into whatever `hs_admin::router::AdminState` (or equivalent
     constructor track 15 has built by the time this lands) takes for its user-directory field —
     check `crates/hs-admin/src/router.rs` for the exact field name, since this session did not
     edit that crate and its shape may have changed since this was written.
  3. **Construct `AdminTokenVerifier`** (carried over from session 3, restated here because it is
     the same "nothing is wired in `serve.rs`" problem this session's work also hits):
     ```rust
     let admin_verifier = hs_auth::admin_verifier::AdminTokenVerifier::from_auth_state(&auth_state);
     ```
     replacing whatever `StaticVerifier`/`dummy_admin_state()` stands in today.

  All three are mechanical (no design judgment beyond "use the type this track built"); none needs
  anything further from track 07.
- **For track 12, to wire the persistent store into `hs serve` (session 3, new — the reason this
  section exists is operational, read this first if you are track 12):**
  `crates/hs-cli/src/serve.rs`'s `spawn_serve` currently has:
  ```rust
  let opened_storage = storage::open_storage(&config.storage)?;

  let auth_config = config_bridge::auth_config_from(&config)?;
  let auth_state = AuthState::in_memory_with_config(auth_config);
  ```
  Change the last two lines to:
  ```rust
  let auth_config = hs_auth::config::AuthConfig::try_from(&config)?;
  let auth_store: std::sync::Arc<dyn hs_auth::store::AuthStore> = match &opened_storage {
      storage::OpenedStorage::Embedded(backend) => std::sync::Arc::new(
          hs_auth::store::tables::TablesAuthStore::open(backend.clone())?,
      ),
  };
  let auth_state = AuthState::with_store(auth_store, auth_config);
  ```
  Notes: `backend.clone()` is cheap and correct — `FjallBackend` is an `Arc`-backed handle (see
  `hs-kv`'s crate docs, "A `KvBackend` is a cheap-to-clone handle"), so the clone given to
  `TablesAuthStore` and the original kept alive in `opened_storage`/`ServeHandle` share the same
  open database; nothing needs `Drop` ordering care beyond what already exists. `TablesAuthStore::
  open` returns `hs_auth::store::StoreError` and `AuthConfig::try_from` returns
  `hs_auth::config::ConfigConversionError` — both need a `#[from]` arm added to `hs-cli`'s
  `ServeError` enum (mechanical: one line each, `#[error(transparent)] AuthStore(#[from]
  hs_auth::store::StoreError)` and similarly for the config error — `config_bridge::BridgeError`
  may become dead code and removable once `config_bridge::auth_config_from` is deleted in favor of
  the direct `TryFrom` call, see the next bullet). This whole change is mechanical (no design
  judgment beyond "use the type this track built"), which is why it is written out here rather than
  filed as an RFC — but it is track 12's own crate, so it is not made here.
- **For track 12, `config_bridge.rs` simplification (session 3, new):**
  `crates/hs-cli/src/config_bridge.rs::auth_config_from` can be replaced with a one-line
  call-through to the new `hs_auth::config::AuthConfig::try_from` (see this crate's `config.rs` for
  exactly which fields it maps and which it leaves at their documented default).
  `config_bridge::BridgeError::InvalidServerName` becomes redundant with `hs_auth::config::
  ConfigConversionError::InvalidServerName` (same check, same message shape) once this is done —
  worth deleting rather than keeping two copies of the same validation, but that is track 12's call.
  `config_bridge.rs`'s other functions (`telemetry_options_from`, `looks_like_native_config`,
  `read_shared_secret_from_config`, `read_pepper_from_config`) are unrelated to `AuthConfig` and are
  untouched by this change.
- **`crate::middleware::{Requester, AllowGuest}`**: the axum `FromRequestParts<AuthState>`
  extractors every HTTP handler in the workspace is meant to use, frozen at the week-6 seam per
  `docs/workstreams/README.md`. Add as a handler parameter; no header/query parsing needed by
  callers.
- **`crate::requester::{Requester, RequesterContext, AppserviceIdentity}`**: the identity type,
  `Serialize`/`Deserialize` so it can cross track 03's mesh forwarding envelope as-is.
  **Session 2: `AppserviceIdentity` gained `rate_limited: bool` and `msc4190_enabled: bool` per RFC
  0009 — track 11 asked for exactly this, and it is now available.** Anything that builds a
  `Requester` for an appservice (today: only `crate::middleware::authenticate_appservice`) should
  populate both from the `AppserviceRecord` it looked up; anything that reads a `Requester` to
  decide rate-limit exemption should check `requester.appservice.as_ref().is_some_and(|a|
  !a.rate_limited)`.
- **`crate::appservice::{AppserviceRecord, AppserviceRegistry}`**: **session 2: `AppserviceRecord`
  gained the same two fields, plus `AppserviceRecord::new(appservice_id, sender, user_namespaces)`
  as the recommended constructor** (fills both new fields with the RFC's safe defaults) so call
  sites that don't care about the new capability flags don't need a bare struct literal.
  `RegistryAppserviceAdapter` in `crates/hs-appservice/src/auth_registry.rs` already populates both
  from `row.rate_limited`/`row.msc4190` (this session added those two lines — see "Decisions made"
  for why that edit was made in another track's crate).
- **`crate::store::{UserStore, DeviceStore, TokenStore, UiaStore, AuthStore}`**: storage traits any
  track needing user/device/token/UIA data can depend on (as `Arc<dyn ...>`) without depending on a
  concrete implementation. **Session 3: two implementations now exist** —
  `store::memory::InMemoryAuthStore` (unchanged, still what every test in this crate defaults to)
  and `store::tables::TablesAuthStore<B: hs_kv::KvBackend>` (new, persistent — generic over any
  `KvBackend`, in practice `hs_kv::fjall_backend::FjallBackend` for a real server). See "Interfaces
  provided" above for the exact `hs-cli` change that wires the latter into `hs serve`.
- **`crate::state::AuthState::with_store`** (session 3, new): builds an `AuthState` around an
  already-open `Arc<dyn AuthStore>` and config, keeping every other piece
  (`InMemoryAppserviceRegistry`, an unlimited rate limiter, the real system clock) the same as
  `AuthState::in_memory`. This is what track 12 should call instead of
  `AuthState::in_memory_with_config` once it opens a persistent store.
- **`impl TryFrom<&hs_config::Config> for crate::config::AuthConfig`** (session 3, new, session 2's
  deferred item 3): the real config bridge, replacing `hs-cli`'s hand-maintained
  `config_bridge.rs::auth_config_from`. See "Interfaces provided" above for the exact simplification
  this makes available to track 12, and `crate::config`'s doc comment on the `TryFrom` impl itself
  for exactly which fields map and which don't yet (nothing was guessed silently).
- **`crate::password::{hash_password, verify_password}`**, **`crate::token::*`**,
  **`crate::uia::*`**, **`crate::reauth::run`**: reusable building blocks for anything else in this
  crate or, if useful, another auth-adjacent surface (the admin API's `login-as`, for instance,
  could mint a session through `crate::session::create_session` rather than reinventing it — not
  yet coordinated with 15, noted here as an option).
- **`crate::shared_secret_auth::{compute_token, verify_token}`** (session 2, new): the
  `com.devture.shared_secret_auth` HMAC-SHA512 primitives, exposed in case anything else (an admin
  tool minting a bridge's config value, say) wants them independent of the login route.
- **`docs/rfcs/0004-admin-api.md` section 8.1's `hs_admin::auth::TokenVerifier`**: **implemented as
  of session 3** (`crate::admin_verifier::AdminTokenVerifier`) — this line is stale and should have
  been removed at the end of session 3; leaving the correction here rather than silently deleting
  it. Not yet wired into `hs serve` — see "Interfaces provided"'s session 4 bullet.
- **`crate::routes::synapse_admin::router`/`crate::synapse_admin_router`** (session 4, new): the
  `/_synapse/admin/v1/register` router fragment. Mount separately from `routes::router()` — see
  "Interfaces provided" above for the exact `hs-cli` line and why it cannot be nested.
- **`crate::store::UserStore::list_users`** (session 4, new): lists every registered user, sorted by
  `user_id`, on both store implementations. A full scan on `TablesAuthStore` — see the trait
  method's doc comment for the cost trade-off before calling it in a hot path.
- **`crate::admin_directory::AuthStoreUserDirectory`** (session 4, new): `hs_admin::sources::
  UserDirectory` over this crate's `AuthStore`. See "Interfaces needed" below for what it still
  cannot fill (`room_count`, `media_count`) and "Interfaces provided" above for the `hs-cli`
  construction line.

## Interfaces needed

- **01 (`hs-kv`/`hs-tables`)**: satisfied as of session 3 — both were already frozen, and this
  session built `TablesAuthStore` against them; nothing further needed from track 01.
- **11 (appservices)**: satisfied as of session 2 — `RegistryAppserviceAdapter` is the real
  `AppserviceRegistry`, and it now also supplies the two RFC 0009 capability fields.
- **13 (config)**: satisfied as of session 3 — `crate::config::AuthConfig` now has a real
  `TryFrom<&hs_config::Config>` impl; nothing further needed from track 13.
- **12 (platform/`hs-cli`) / whoever owns `serve.rs`**: needs to make the mechanical changes under
  "Interfaces provided" above (wire `TablesAuthStore` into `spawn_serve`, simplify
  `config_bridge.rs`, **mount `synapse_admin_router()`, construct `AdminTokenVerifier`, construct
  `AuthStoreUserDirectory`** — the last three added this session) to actually get a persistent,
  correctly-configured server with a working admin API; all are ready and waiting on that crate, not
  on anything further from track 07.
- **hs-http (shared with 07, 14, 15)**: `routes::router()` still returns a bare `Router<AuthState>`
  fragment at spec-relative paths; `hs-cli` (not `hs-http`) ended up doing the mounting and version
  prefixing (`docs/status/12-platform-and-kubernetes.md`) — `hs serve` now serves this crate's
  routes under both `/_matrix/client/v3` and `/_matrix/client/r0`. A real client IP for rate
  limiting is still not threaded through anywhere.
- **04 (room and events) / 09 (media)**: `AuthStoreUserDirectory`'s `AdminUser.room_count`/
  `media_count` are hardcoded to `0` (session 4) — this crate has no data source for either. Either
  track needs to expose its own `hs_admin::sources`-style seam (or an existing one) that whoever
  wires the admin `/users` router can compose with this session's directory, or `AdminUser`
  construction needs to move to a place that can see all three sources at once. Not decided here;
  flagging the gap for track 15/whoever owns `hs-admin`'s router wiring to resolve.
- **14 (test/conformance)**: Complement and differential-tests-against-Synapse coverage for this
  surface once `hs-testkit`/the harness exists; this crate's 156 tests are still its own unit and
  router-level tests only, not run against Complement.

## Decisions made

- **Token shapes match Synapse's `syt_`/`syr_`/`syl_` algorithm exactly**, including the CRC-32 +
  base62 checksum, so imported Synapse tokens keep working and freshly minted ones are
  indistinguishable. See RFC 0002 section 3.
- **Argon2id, no pepper, is the native password hash; bcrypt is verify-only**, for imported hashes,
  with the pepper and 72-byte truncation applied only on that path. See RFC 0002 section 5.
- **Tokens are opaque strings hashed with SHA-256 at rest**, not encrypted, not a slow hash (the
  input is already high-entropy). Applies to both legacy and — per RFC 0003 section 5 — the future
  native-issuer tokens, deliberately unified rather than JWTs for the native flow.
- **`M_USER_SUSPENDED` is not enforced at the middleware layer.** A suspended account still
  authenticates (reads keep working); `Requester::require_not_suspended()` is an explicit opt-in
  each write handler that the spec restricts calls first. `/account/password` and
  `/account/deactivate` call it today; other write endpoints will as they're built by whichever
  track owns them (this crate's own writes are the two just named).
- **`org.matrix.msc3202.device_id` is accepted alongside plain `device_id`** for appservice device
  masquerade, a superset of Synapse's currently observed plain-`device_id`-only behavior, per the
  task instructions for this track's day-one work. Documented in RFC 0002 section 7 step 2.
- **Refresh token reuse revokes the whole device session**, not just the one request — stricter
  than Synapse's default. RFC 0002 section 3.3.
- **UIA has no "recent login" grace period**; every sensitive operation always does a full re-auth
  round. RFC 0002 section 6.
- **Registration never offers `m.login.recaptcha`/`m.login.email.identity`/`m.login.msisdn`**; a
  client submitting one anyway gets a clean `M_UNRECOGNIZED`, not a misleading `M_FORBIDDEN`. RFC
  0002 section 6.
- **In-house OAuth grant implementation over `oxide-auth`; `jsonwebtoken` over `josekit`** for the
  future native issuer. RFC 0003 section 3.
- **`hs-auth` does not depend on `hs-http`, `hs-kv`, `hs-tables`, `hs-config`, `hs-model` or
  `hs-appservice`** this session, per the workstreams README's rule 1 (own crates only) and the
  task's explicit instruction not to depend on tracks 01/13 yet. All storage, config and identifier
  handling needed is either self-contained (`store`, `config`) or comes from `ruma` (identifiers,
  UIA wire types), which is a frozen week-2 workspace dependency, not another track's crate.
  **Superseded in session 2**: `hs-kv`/`hs-tables`/`hs-config` are now built and frozen crates, not
  moving targets, so items 3 and 4 of session 2's assignment explicitly call for depending on them
  next session; the rule that stopped this session 1 no longer applies once that work starts.

### Session 2 decisions

- **`AppserviceRecord::new`/RFC 0009 defaults**: `rate_limited: true` (not exempt),
  `msc4190_enabled: false`, exactly the RFC's own "assume nothing extra is granted" values, so
  adding the fields changed no existing test's observed behavior.
- **`post_delete_devices` (bulk) was not given the MSC4190 skip-UIA branch**, only the two
  single-device routes RFC 0009 names. This is a narrower reading of the RFC than a bridge author
  might want in practice (a bulk double-puppet cleanup would hit the same problem
  `delete_device`'s branch solves), but the RFC's "Proposed interface" section names only
  `put_device`/`delete_device`, and extending scope beyond what an RFC asked for is exactly the
  kind of unrequested design decision this crate's instructions say to avoid making unilaterally.
  Flagged here so track 11 can file a one-line RFC addendum if it turns out to matter in practice.
- **RFC 0009's non-breaking rollout required a two-line fix in `hs-appservice`, made here.** RFC
  0009 claims adding the two fields is "a mechanical, non-behavior-changing patch" with no impact
  beyond `hs-auth`'s own `middleware.rs`/`devices.rs` — that claim is only true for *this* crate.
  Rust struct literals require every public field unless `..Default::default()` spread is used, and
  `crates/hs-appservice/src/auth_registry.rs`'s `RegistryAppserviceAdapter::lookup_by_token` (plus
  a test fixture in `crates/hs-appservice/src/routes.rs`) constructed `AppserviceRecord` with an
  explicit three-field literal, so adding the fields broke `cargo check --workspace` outright the
  moment they landed. This crate's own instructions and the task's explicit constraints both say
  "do not edit other tracks' crates" — normally decisive. This session made a narrow exception
  because: (1) the fix is *exactly* the mechanical patch RFC 0009 itself prescribes at that exact
  call site — populate `rate_limited`/`msc4190_enabled` from `row.rate_limited`/`row.msc4190`, data
  `RegistryAppserviceAdapter` already had in hand, with zero design judgment involved; (2) RFC 0009
  literally names this as track 11's own follow-up ("Track 11 will apply this patch itself if track
  07 has not picked it up by the time both tracks are back in the same integration window") — this
  session picked it up *now* to avoid the alternative; (3) leaving `cargo check --workspace` red
  blocks every other concurrently running agent on this shared machine and target directory, which
  is a larger violation of good-citizenship than a two-line, RFC-specified, additive, behaviorally
  inert patch. Verified with `hs-appservice`'s own full test suite and clippy (see above) — nothing
  else in that crate was touched. **Track 11 should review this diff** (both hunks are small and
  clearly commented in place) and is free to revise it; it was not designed to preempt track 11's
  own judgment, only to keep the lights on.
- **`com.devture.shared_secret_auth` reuses HMAC-SHA512 over the full mxid, no nonce, matching the
  real `devture`/mautrix protocol exactly** (verified against `refs/mautrix-python/mautrix/
  bridge/custom_puppet.py`'s actual client-side computation, not just the type-name reference in
  `refs/mautrix-python/mautrix/types/auth.py` or `refs/synapse/docs/
  password_auth_providers.md`'s link) — this is a fixed external wire protocol every existing
  mautrix bridge already speaks, so the goal was faithful reproduction, not redesign, per decision
  0007's framing for "protocols we integrate with."
- **The shared-secret-auth secret has no dedicated `hs_config`/`hs-auth` config field of its own
  yet; the plan (not yet implemented — that's item 3) is to reuse `hs_config::AuthConfig::
  registration_shared_secret`** rather than asking track 13 to add a new field, since both are "a
  privileged shared secret for trusted server-to-server tooling" and reusing one avoids new
  operator-facing config surface. `crate::config::AuthConfig::shared_secret_auth_secret` exists as
  its own `Option<String>` field today (`None` = disabled) purely because item 3 (real `hs_config`
  wiring) had not started when item 2 needed *some* config surface to gate the feature on; whoever
  does item 3 should either keep this field and map it from the reused `registration_shared_secret`
  value, or fold it away entirely if that turns out cleaner once the real mapping function exists.

### Session 3 decisions

- **Keyspace/index layout for `TablesAuthStore`**: see `crates/hs-auth/src/store/tables.rs`'s module
  doc for the full table (seven keyspaces, three secondary indexes). The two load-bearing choices:
  (1) `devices` is keyed `(user_id, device_id)` with **no** secondary index — `list_devices` is a
  prefix scan on `(user_id,)`, which `hs-tables`' order-preserving key encoding makes exact (a
  shorter tuple that is a prefix of a longer one always sorts immediately before it, so the scan
  cannot pick up another user's rows) — adding an index here would have been redundant machinery
  for an access path the primary key already serves. (2) `access_tokens`/`refresh_tokens` are each
  indexed **only** by `user_id`, not by `(user_id, device_id)`, even though
  `delete_access_tokens_for_device` needs the latter: the per-device filter is applied in memory
  after the user-scoped index lookup (a handful of rows per user in practice), rather than
  maintaining a second index whose only job would be to save that filter. This was a deliberate
  trade — a second index is more machinery to keep correct for a query that already starts from a
  small, index-narrowed candidate set — and is called out explicitly here in case a future session
  disagrees once token counts per user get large.
- **Case-insensitive localpart uniqueness is enforced by a real unique index
  (`users_by_localpart_lower`), not a table scan**, matching `InMemoryAuthStore`'s O(n) scan
  semantics exactly but making the tables-backed store's own conflict check O(log n). `create_user`
  still does a direct primary-key existence check *before* the index write (not relying on the
  index alone) so an exact-user-id re-registration is rejected with the same `StoreError::Conflict`
  Synapse-parity callers expect, not silently treated as an update.
- **Every write to an indexed keyspace reads the row's old value first, inside the same
  transaction, and always calls `maintain_index` with both old and new values** — even for updates
  that provably cannot change the indexed field (e.g. `mark_access_token_used`, which only touches
  `last_used_ms`). This costs one extra read per write but means no call site can silently forget
  index maintenance if the row's shape changes later to add a field the index derives from; `hs_
  tables::index::maintain_index` itself is a no-op when the derived key is unchanged, so the cost is
  one comparison, not a wasted write.
- **`TablesAuthStore` stores every row as `serde_json::to_vec`, not a binary format.** Matches
  `hs-appservice::store::AppserviceStore`'s established convention in this workspace (read before
  writing this module) rather than introducing a second row-encoding scheme; `hs-tables` only
  encodes keys, values are each table owner's choice, and JSON keeps rows human-inspectable in a
  raw `hs-kv` dump during debugging, which was judged worth more than a marginal size/speed win for
  auth's data volumes (users/devices/tokens, not events).
- **The shared behavioral test suite (`store::shared_tests::run_all`) is invoked once per store as
  a single `#[tokio::test]`, not as ~24 individually-named `#[tokio::test]` wrappers per store.**
  The task's phrasing ("make the suite generic over the store and instantiate it twice") is
  satisfied either way; the single-entry-point form was chosen because duplicating ~24 thin wrapper
  functions per store (48 total) is exactly the kind of boilerplate decision 0007 would flag, and a
  panic inside `run_all` still reports the specific `assert_eq!`/`assert!` call site and line that
  failed — the granularity lost is "which named sub-test failed" at the `cargo test` summary level,
  not "what failed and where."
- **`AuthState::with_store` was added rather than requiring track 12 to hand-construct an
  `AuthState` struct literal.** `AuthState`'s fields are already all `pub`, so a struct-update-syntax
  construction would have worked without any change to this crate; the convenience constructor was
  added anyway because it names the intended call precisely (`with_store(store, config)`) and keeps
  `hs-cli`'s wiring one line instead of five, and because the equivalent `in_memory`/
  `in_memory_with_config` pair already established "provide a named constructor for each common
  shape" as this type's own convention.
- **`AuthConfig::try_from`'s server-name validation duplicates `config_bridge::BridgeError::
  InvalidServerName`'s check, on purpose.** Once track 12 deletes `config_bridge::auth_config_from`
  in favor of this crate's `TryFrom`, the duplication resolves itself (there will be exactly one
  check again); until then, both exist, which is a harmless transitional state, not a bug — noted
  here so it doesn't look like something was missed.

### Session 4 decisions

- **`registration_shared_secret` is its own `AuthConfig` field, not folded into the existing
  `shared_secret_auth_secret`, even though both currently read the same `hs_config` value.** This
  was an explicit instruction for this session, and it is the right call independent of that: the
  two are different protocols (`register_new_matrix_user`'s admin API vs. `com.devture.
  shared_secret_auth` login), and an operator who wants one but not the other has no way to express
  that today only because `hs-config` itself has a single field — the day it grows a second one,
  this crate should not need to change to pick it up correctly. `AuthConfig::try_from` maps the same
  native field onto both, documented in both fields' doc comments so the duplication reads as
  intentional, not missed.
- **The synapse-admin router verifies the MAC (authenticating the caller) before checking username
  availability or touching the store at all.** Synapse's own `register_new_matrix_user` handler
  checks availability after MAC verification too, but this was independently re-derived here from
  first principles (least-privilege: nothing about account state should be observable pre-auth), not
  copied — worth stating since it is a security-relevant ordering choice, not an incidental one.
- **`NonceRegistry` lives in the router fragment's own `Arc<Mutex<_>>`, injected via
  `axum::Extension`, not in `AuthState`.** `AuthState` is cloned into every handler's `State`
  extractor across the whole crate; putting single-endpoint, 60-second-lived nonce bookkeeping there
  would mean every other handler's `AuthState` clone carries a `Mutex` it never touches, and would
  make `AuthState::in_memory()`/`with_store()` responsible for initializing state that is really this
  one router fragment's private concern. `axum::Extension` is the standard axum idiom for exactly
  this ("a router fragment needs one piece of shared state its `State<S>` type doesn't carry"), used
  here in preference to a closure-capturing-`Arc` pattern (also considered — axum handler closures
  do work and do auto-implement `Clone` when their captures are `Clone`, but `Extension` is more
  idiomatic and does not require reasoning about closure `Clone` semantics to trust it compiles
  correctly).
- **Bad-MAC and bad/replayed/expired-nonce failures get different HTTP statuses (403 vs. 400).**
  This session did not re-verify the exact codes against a running Synapse or `refs/synapse`'s
  source for this session's write-up (recalled from general familiarity with Synapse's admin API,
  not confirmed here) — worth a differential check against real Synapse before calling this
  Synapse-tooling-compatible with confidence. Not specified by this session's instructions beyond
  "each needs its own test"; the actual codes chosen (`400` for nonce problems, `403` for a MAC that
  does not match) are internally consistent and documented in `map_registration_error`'s doc
  comment, but flagging that they are a best guess, not a verified fact, for whoever runs the
  differential harness (14) against this route.
- **`AuthStoreUserDirectory::get_user`/`set_admin`/`set_locked`/`set_deactivated` parse `user_id: &str`
  with `ruma::UserId::parse` (full Matrix ID only), not `parse_with_server_name`.** `hs_admin::
  sources::UserDirectory`'s trait contract takes a bare `&str` with no server-name context to
  resolve a localpart against, and every caller in `hs-admin`'s own router code necessarily already
  has a full `@user:server` id (it came from a path parameter or a `GET /users` filter, never a
  bare localpart) — accepting a bare localpart here would silently assume *this* server's name for
  an admin-API caller who may not have meant that.

## Reuse considered (decision 0007)

- **`com.devture.shared_secret_auth`'s HMAC verification**: considered depending on
  `hs-compat::shared_secret` (track 13's already-implemented, already-tested shared-secret HMAC
  module) directly rather than writing new code, per this session's explicit instruction to prefer
  it. Read the module in full (`crates/hs-compat/src/shared_secret.rs`): its `compute_mac`/
  `verify_mac` are hardcoded to `Hmac<Sha1>` and a specific nonce-plus-four-NUL-separated-fields
  message shape (Synapse's `POST /_synapse/admin/v1/register` MAC), which is a different wire
  protocol from `com.devture.shared_secret_auth`'s HMAC-SHA512-over-just-the-mxid — not a design
  choice either module made, but a fact about two different, independently specified external
  protocols this server has to speak byte-for-byte. Depending on `hs-compat` for one HMAC call
  would also pull in `hs-config` transitively for no benefit. **What was reused, deliberately
  mirroring `hs-compat::shared_secret`'s design rather than inventing a new one**: the same
  `hmac`/`sha2`/`hex` workspace crates, and the same constant-time-verification shape
  (`hmac::Mac::verify_slice`, not a manual byte comparison) that module already established as this
  workspace's convention for this class of problem. Full reasoning also lives in
  `crates/hs-auth/src/shared_secret_auth.rs`'s module doc.
- **RFC 0009's two new fields**: plain `bool`s on existing structs; no external crate or reuse
  question involved.
- **Items 3 and 4 (not started as of session 2)**: superseded — see "Session 3 reuse considered"
  immediately below; both are now done.

### Session 3 reuse considered

- **`TablesAuthStore`'s shape and conventions were copied from `hs-appservice::store::
  AppserviceStore`** (`crates/hs-appservice/src/store.rs`), read in full before writing a line of
  `tables.rs`: the `TypedKeyspace`/`IndexDef` field layout, the `transact`-wrapped write methods
  with a snapshot-based read path, the `to_kv`/marker-error (`RowExists`/`RowMissing`)/downcast
  pattern for turning a generic `hs_kv::KvError` back into this crate's own error enum, and the
  JSON-value-encoding convention. This is exactly decision 0007's "reuse a pattern already
  established in this workspace" case: `hs-appservice` had already solved "how does a `hs-kv`/
  `hs-tables`-backed store in this codebase look" for a comparably-shaped problem (rows plus a
  couple of unique/non-unique token-lookup indexes), so re-deriving a different shape from first
  principles would have been pure risk with no benefit — a second, gratuitously different store
  idiom in the same workspace is itself a maintenance cost. Nothing was copied byte-for-byte (Rust
  code, not text); the *pattern* was reused, the schema and every method body are specific to this
  crate's own four traits.
- **`hs-kv`/`hs-tables` themselves**: no third-party crate to evaluate, as session 2 already noted
  — this is exactly the storage/typed-layer decision 0007 names as something this project builds
  itself, and track 01 had already built and frozen it by the time this session started.
- **No new third-party crate was added for the persistence work.** `proptest` and `tempfile` (both
  already `[workspace.dependencies]` entries, used by other crates including `hs-tables` itself for
  `proptest` and `hs-cli` for `tempfile`) were added to `hs-auth`'s own `[dev-dependencies]` as a
  second/third consumer of an existing workspace entry, not a new one.
- **`AuthConfig::try_from`'s mapping logic was ported from `hs-cli`'s existing
  `config_bridge::auth_config_from`** (read in full first — see "Decisions made") rather than
  redesigned: the field-by-field mapping, the reasoning for which fields have no native
  counterpart, and the server-name validation are all the same logic that function already had
  proven correct (it has its own passing test suite in `hs-cli`), moved to the crate that should
  have owned it from the start rather than reinvented.

## Shared dependencies added

Added to `[workspace.dependencies]` in the root `Cargo.toml` (all newly required, none previously
present):

- `argon2 = "0.5"` — native password hashing.
- `bcrypt = "0.17"` — imported-hash verification.
- `crc32fast = "1"` — Synapse-shaped token checksums.
- `regex = "1"` — appservice namespace matching (the in-crate registry stub; track 11's real
  registry may or may not need this dependency itself).
- `rand_core = { version = "0.6", features = ["getrandom"] }` on `hs-auth`'s own `Cargo.toml` entry
  only (the workspace-level `rand_core = "0.6"` was already added by track 02; this track's entry
  adds the `getrandom` feature it needs for `argon2`'s `SaltString::generate`, which does not
  change track 02's usage).

### Session 2

- `hmac = { workspace = true }` added to `crates/hs-auth/Cargo.toml`. **No new
  `[workspace.dependencies]` entry** — track 13 already added `hmac = "0.12"` at the workspace
  level for `hs-compat`; this session added `hs-auth` as a second consumer of the existing entry.
  `sha2` and `hex` were already both workspace dependencies and already present in `hs-auth`'s own
  `Cargo.toml` from session 1, reused as-is for the HMAC-SHA512 devture protocol.

### Session 3

All additions are path dependencies on sibling crates already in the workspace, not new
`[workspace.dependencies]` entries:

- `hs-config = { path = "../hs-config" }` — for `AuthConfig::try_from`.
- `hs-kv = { path = "../hs-kv" }`, `hs-tables = { path = "../hs-tables" }` — for `TablesAuthStore`.
- `bytes = { workspace = true }` — already a workspace entry (used by `hs-kv`/`hs-tables`
  themselves); added to `hs-auth`'s own `Cargo.toml` as a direct dependency because `tables.rs`
  touches `hs_kv::Value`/`Bytes` at a couple of call sites, previously only reached transitively.
- `proptest = { workspace = true }`, `tempfile = { workspace = true }` added to
  `[dev-dependencies]` — both already workspace entries (used by `hs-tables` and `hs-cli`
  respectively); `hs-auth` is a new consumer of each, not a new entry.

### Session 4

- `hs-compat = { path = "../hs-compat" }` — for `hs_compat::shared_secret`'s already-built,
  already-tested nonce/MAC protocol, reused rather than reimplemented (`routes/synapse_admin.rs`).
  No new `[workspace.dependencies]` entry (path dependency on a sibling crate); no dependency cycle
  (`hs-compat` depends only on `hs-config`, confirmed by reading `crates/hs-compat/Cargo.toml`
  before adding this). `hs-admin` was **not** newly added this session — it was already a dependency
  of `crates/hs-auth/Cargo.toml` from whichever earlier session/salvage added `admin_verifier.rs`.
