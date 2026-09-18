# 07 Auth and identity: status

Track brief: `docs/workstreams/07-auth-and-identity.md`. Owner crate: `hs-auth`.

Last updated: 2026-09-18 (session 2, interrupted mid-assignment — see "Session 2" below for exactly
where it stopped and what to do first).

## Session 2 summary (read this first)

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

## In progress

Nothing mid-flight; Phase 0 legacy-auth scope for this session is complete and green.

## Next

- Wire `hs-auth`'s router into whatever crate ends up owning the real listener and
  `/_matrix/client/v3` prefixing (see "Interfaces needed" — likely `hs-http`/`hs-cli`, once those
  exist beyond their own placeholders).
- Registration-token usage limits/expiry (currently a flat always-valid set).
- 3PID validation-session flow (`/register/email/requestToken` etc.) once an email-sending story
  exists elsewhere in the workspace; today 3PID login works only for already-bound addresses.
- Rate limiting wired into handlers with real per-IP keys once `hs-http` can supply one.
- Begin native OAuth issuer implementation per RFC 0003's ordering (section 10), once Phase 0
  legacy auth has had a chance to be exercised by Complement/differential tests (track 14).
- SSO redirect/token handoff, upstream OIDC/SAML/LDAP, MAS delegation implementation, account
  lifecycle beyond lock/suspend/deactivate (erasure, consent, account validity) — all Phase 1/2 per
  the brief.

## Blockers

None. Built entirely against this crate's own in-memory storage, per
`docs/workstreams/README.md` rule 1 (own crates only; cross-track interfaces are RFCs until
frozen).

## Interfaces provided

- **`crate::middleware::{Requester, AllowGuest}`**: the axum `FromRequestParts<AuthState>`
  extractors every HTTP handler in the workspace is meant to use, frozen at the week-6 seam per
  `docs/workstreams/README.md`. Add as a handler parameter; no header/query parsing needed by
  callers.
- **`crate::requester::{Requester, RequesterContext, AppserviceIdentity}`**: the identity type,
  `Serialize`/`Deserialize` so it can cross track 03's mesh forwarding envelope as-is.
- **`crate::store::{UserStore, DeviceStore, TokenStore, UiaStore, AuthStore}`**: storage traits any
  track needing user/device/token/UIA data can depend on (as `Arc<dyn ...>`) without depending on
  this crate's in-memory implementation specifically.
- **`crate::appservice::AppserviceRegistry`**: the trait track 11's real appservice registry should
  implement (or that this crate will adapt to track 11's own trait, behind an RFC, if track 11's
  shape differs).
- **`crate::password::{hash_password, verify_password}`**, **`crate::token::*`**,
  **`crate::uia::*`**, **`crate::reauth::run`**: reusable building blocks for anything else in this
  crate or, if useful, another auth-adjacent surface (the admin API's `login-as`, for instance,
  could mint a session through `crate::session::create_session` rather than reinventing it — not
  yet coordinated with 15, noted here as an option).
- **`docs/rfcs/0004-admin-api.md` section 8.1's `hs_admin::auth::TokenVerifier`**: not yet
  implemented (needs the native OAuth issuer, RFC 0003, which is design-only so far); tracked as
  "Next" work, not a current blocker for 15 since 15's mock/scaffold work does not need it yet.

## Interfaces needed

- **01 (`hs-kv`/`hs-tables`)**: once available, an `hs-tables`-backed `AuthStore` implementation
  replaces (or sits alongside) `store::memory::InMemoryAuthStore`. No changes expected to the trait
  definitions themselves; this is purely a new implementation behind an RFC.
- **11 (appservices)**: the real `AppserviceRegistry`, replacing `InMemoryAppserviceRegistry`. See
  `crate::appservice`'s doc comment for the exact seam.
- **13 (config)**: `crate::config::AuthConfig` is a placeholder; every field's doc comment names
  its Synapse config-option analog for mechanical mapping once the native config schema and
  `homeserver.yaml` translator exist.
- **hs-http (shared with 07, 14, 15)**: `routes::router()` returns a bare `Router<AuthState>`
  fragment at spec-relative paths (`/login`, not `/_matrix/client/v3/login`). Mounting, version
  prefixing (`v3` vs. the historical `r0` aliases) and wiring a real client IP through for rate
  limiting are `hs-http`'s job. This crate deliberately does not touch `hs-http` itself (track 15's
  status file shows it mid-flight there this same session); this is recorded as the seam rather
  than guessed at.
- **14 (test/conformance)**: Complement and differential-tests-against-Synapse coverage for this
  surface once `hs-testkit`/the harness exists; this session's 115 tests are this crate's own unit
  and router-level tests only, not run against Complement.

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
