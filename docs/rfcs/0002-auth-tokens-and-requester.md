# RFC 0002. Auth tokens, the threat model and the `Requester` middleware contract

Status: accepted (day one). Owner: track 07 (auth and identity). Consumers: every track that
registers an HTTP handler (04, 05, 06, 09, 10, 11, 15), track 03 (forwarding envelope), track 13
(config and the Synapse importer), track 14 (differential tests against Synapse).

Companion artifacts: `crates/hs-auth` (implementation), `docs/status/07-auth-and-identity.md`
(current state), `docs/rfcs/0003-native-oauth-issuer.md` (the native OAuth 2.0 authorization
server, design only).

## 1. Motivation

`docs/workstreams/README.md` freezes "Authentication middleware: request to `Requester`" as the
week-6 seam every HTTP handler in the workspace depends on. Before any handler can be written
against it, three things have to be nailed down and written up, not just coded: what a `Requester`
means (who is making this request, and under what authority — a user, a device, an appservice
wearing someone else's face, a guest, an admin), what an access/refresh/login token *is* on the
wire (so that Synapse compatibility — `PLAN.md` D8 — is not an afterthought bolted onto whatever
shape was convenient), and what happens when a credential is bad in each of the specific ways the
spec and Synapse distinguish (missing vs. unknown vs. expired vs. locked vs. suspended). This RFC
is that write-up; `crates/hs-auth` is the implementation of it.

## 2. Threat model

**In scope — what this design defends against:**

- **Storage compromise.** A backup, a misconfigured export, or a read replica leaking should not
  hand out working credentials. Tokens are hashed at rest (section 3.4); passwords are hashed with
  a memory-hard function (section 5).
- **Token replay after logout/rotation.** A refresh token that has already been exchanged is a
  theft signal (the legitimate client and an attacker both had it); reusing one revokes the whole
  session for that device (section 3.3), not just the one request.
- **Appservice over-reach.** An appservice must not be able to act as a user outside its registered
  namespaces, nor as a device that does not belong to the user it is masquerading as. Both are
  checked before a `Requester` is ever constructed (section 4.2).
- **User enumeration via login.** A wrong username and a wrong password get the identical error
  (`INVALID_USERNAME_OR_PASSWORD`, verbatim from Synapse's behavior) so a failed login never tells
  an attacker which half was wrong (`crates/hs-auth/src/routes/login.rs`).
- **Locked/suspended/deactivated accounts continuing to act.** Enforced at the point every request
  is authenticated (locked, deactivated) or at the specific write endpoints the spec restricts
  (suspended), not left to individual handlers to remember.
- **UIA bypass.** A client cannot skip a required re-authentication stage by omitting `auth` or by
  submitting a stage that was never offered; `crate::uia` only ever marks a stage complete after
  the caller has independently verified it, and a flow is only satisfied when every stage of *some*
  offered flow is complete.
- **Timing-insensitive credential comparison.** Token lookup is by hash through a `HashMap`/table
  key, not by scanning-and-comparing secrets; password verification goes through Argon2id/bcrypt,
  both of which are constant-time by construction for the comparison step.

**Explicitly out of scope for this RFC (tracked as follow-up, not silently ignored):**

- Identity-server-mediated 3PID validation (email/msisdn verification sessions) — section 8.
- Rate-limit tuning and IP-based limiting (the trait exists, `crates/hs-auth/src/ratelimit.rs`;
  wiring real client IPs through from `hs-http` is an integration step for whoever assembles the
  full server).
- Side channels from response timing between "user does not exist" and "user exists, wrong
  password" — both paths do a real Argon2id/bcrypt verification is *not* currently guaranteed
  (an unknown user short-circuits before hashing anything); this is the same trade Synapse makes
  and is not considered a meaningful practical risk (a login attempt against a known-fake user is
  not a realistic attack primitive against a homeserver), but it is named here rather than
  silently assumed away.
- The native OAuth 2.0 authorization server's own threat model (token introspection, PKCE
  downgrade, client impersonation) is in `docs/rfcs/0003-native-oauth-issuer.md`.

## 3. Token design

### 3.1 Why match Synapse's shapes

`PLAN.md` D8: Synapse compatibility lives at the edges, including an online importer for Synapse
databases. An imported Synapse `access_tokens`/`refresh_tokens` row has to keep authenticating
after import without every session in existence being invalidated on migration day. The simplest
way to guarantee that is for this server's own token *shape* to be indistinguishable from
Synapse's, so the same table schema, the same hashing-at-rest scheme, and the same handler code
path work for both a freshly minted token and an imported one.

### 3.2 The three token kinds

Transcribed (behaviorally, not by copying code — Synapse is AGPL-3.0, read as a reference only)
from `synapse/handlers/auth.py`'s `generate_access_token`, `generate_refresh_token`,
`generate_login_token` and `synapse/util/stringutils.py`'s `random_string`/`base62_encode`:

| Kind | Prefix | Shape | Embeds user identity? | Lifetime |
|---|---|---|---|---|
| Access token | `syt_` | `syt_<unpadded-b64 localpart>_<20 random ASCII letters>_<6+ char base62 crc32>` | Yes (localpart, base64-encoded, in the token string itself — not a security boundary, just debuggability) | Configurable; unbounded by default for non-refreshable sessions (`AuthConfig::nonrefreshable_access_token_ttl_ms`), 5 minutes by default for refreshable ones |
| Refresh token | `syr_` | Same shape as access, `syr_` prefix | Yes | Configurable, unbounded by default (`AuthConfig::refresh_token_ttl_ms`); single-use regardless |
| Login token (`m.login.token`) | `syl_` | `syl_<20 random ASCII letters>_<6+ char base62 crc32>` — **no localpart segment**, matching Synapse's `generate_login_token()` which takes no user argument | No — the user association lives only in the store record | 2 minutes by default, single-use |

The checksum is `base62(crc32(ascii_bytes(everything before the checksum)))`, zero-padded to at
least 6 base62 digits, base62 alphabet `0-9A-Za-z` in that order (Synapse's `_BASE62`). It is
**not a security boundary** — a forged token with a correct checksum is still just a random
64-character-ish string that has to match a stored hash to authenticate anything. Its purpose is
purely structural: tokens minted by this server are byte-shape-indistinguishable from Synapse's for
tooling that pattern-matches on them (log scrubbers, some client SDKs' "is this an access token"
heuristics, Synapse's own `_verify_refresh_token` shape sanity check reproduced here as
`token::parse_shape` for symmetry). `docs/decisions/` does not need an entry for this — it is
purely an implementation detail with no cross-track interface impact.

Implementation: `crates/hs-auth/src/token.rs`. Cross-checked against an independently computed
CRC-32/base62 oracle (Python's `zlib.crc32` plus a transcription of `base62_encode`), not against
this module's own output, in `token::tests::crc32_base62_matches_an_independently_computed_oracle`.

### 3.3 Refresh rotation and theft detection

On `POST /refresh`, the presented refresh token is looked up, checked for expiry (its own and the
session's ultimate expiry), and — if valid and unused — consumed: the old access token is deleted,
a fresh access/refresh pair is minted, and the old refresh token is marked used with a pointer to
its replacement. If a refresh token that is *already marked used* is presented again, that is
reuse: this implementation revokes every access token for that user+device outright, stricter than
Synapse's default behavior (which mostly just rejects the one request). This is a deliberate
hardening documented here rather than a silent behavior change: differential tests against Synapse
(track 14) should treat "reuse gets a harder response here" as expected, not a bug.

### 3.4 Hashing at rest

Every token is stored as `TokenHash` — SHA-256 of the token string, never the string itself
(`token::TokenHash::of`). SHA-256, not a slow password hash, is correct here: the input (20
cryptographically random ASCII letters, about 114 bits of entropy) has no realistic offline
dictionary to defend against, unlike a human-chosen password, so a fast deterministic hash is
exactly what a keyed lookup needs, and using a slow hash would only turn every authenticated
request into a deliberately expensive operation for no security benefit.

## 4. The `Requester` type

`crates/hs-auth/src/requester.rs`. Fields and why each exists:

| Field | Purpose |
|---|---|
| `user_id` | Who this request acts as. For an appservice request, the *masqueraded* user (or the appservice's own `sender` if no `user_id` param was given) — not the appservice's own identity. |
| `device_id` | The device this request is bound to, if any (ordinary sessions always have one after login; appservice requests only when MSC3202 device masquerading was used). |
| `is_guest` | Guests are forbidden from most endpoints by default; `Requester`'s own `FromRequestParts` rejects them, `AllowGuest` opts back in per-handler. |
| `is_admin` | The legacy per-user server-administrator flag (distinct from the native OAuth issuer's `admin:*` scopes in RFC 0003/0004 — a legacy admin token is documented in RFC 0004 section 8.1 as carrying `admin:write` for the compatibility admin surface). |
| `shadow_banned` | Carried here so any handler that fans out an event can apply shadow-ban semantics without a second store round trip. |
| `suspended` | *Not* enforced at the middleware layer (a suspended account can still read); `Requester::require_not_suspended()` is a one-line opt-in for the specific write handlers the spec restricts (`M_USER_SUSPENDED`). |
| `appservice: Option<AppserviceIdentity>` | Set exactly when the request authenticated with an appservice token; carries the appservice's own id, its `sender`, whether `user_id` masqueraded, and any masqueraded `device_id`. |
| `access_token_id` | The hash of the token used (for `/logout`'s "just this session", rate-limit attribution, and `mark_access_token_used` bookkeeping). |

`Requester::authenticated_entity()` returns the appservice id for appservice requests and the user
id otherwise — the thing rate limiting and audit logging should key on, so that one appservice
masquerading as a thousand users is throttled as one entity, not a thousand.

`RequesterContext` (a type alias for `Requester`, currently identical) is what track 03's mesh
forwarding envelope should name: `Requester` derives `Serialize`/`Deserialize` specifically so the
authenticated context survives a hop from the replica that terminated the HTTP request to the
replica that owns the room or user session, without re-deriving identity on the other side.

## 5. Password hashing

`crates/hs-auth/src/password.rs`.

- **Native default: Argon2id** (the `argon2` crate, RustCrypto), library defaults (Argon2id,
  version 0x13, `Params::DEFAULT`), fresh random salt per hash, stored as a self-describing PHC
  string (`$argon2id$v=19$m=...,t=...,p=...$<salt>$<hash>`). No pepper: Argon2id's own
  memory-hardness is the defense a static pepper would otherwise buy for a hash generated with a
  strong KDF, and dropping it removes one more secret operators must provision, store securely and
  rotate for no defense-in-depth benefit over what Argon2id already provides against offline
  cracking of *this* hash. (Synapse's own upstream default of bcrypt is why bcrypt needs a pepper —
  bcrypt is not memory-hard — but that reasoning does not carry over to a KDF whose whole point is
  memory hardness.)
- **bcrypt verification only, for imported hashes.** `password + pepper`, truncated to 72 bytes —
  bcrypt's own input limit — exactly reproducing Synapse's explicit truncate-and-warn behavior
  (`refs/synapse/synapse/handlers/auth.py`, behavioral reference only) so a hash imported
  byte-for-byte from a Synapse `users.password_hash` column verifies identically. The pepper comes
  from `AuthConfig::bcrypt_pepper`, mapped 1:1 from Synapse's `password_config.pepper` when track
  13's config translator runs.
- Dispatch is by stored-hash prefix (`$argon2` vs `$2[abxy]$`), so a single `verify_password` call
  handles both a freshly hashed native account and an imported Synapse one without the caller
  needing to know which.
- Tested against hashes generated by an *independent* implementation (Python's `bcrypt` 5.0.0),
  not round-tripped through this crate's own code, including a dedicated test that two passwords
  differing only after byte 72 verify against the same hash (`password::tests::*`).

## 6. User-interactive authentication

`crates/hs-auth/src/uia.rs` (the session state machine) and `crates/hs-auth/src/reauth.rs` (the
"prove you're still you" flow shared by `/account/password`, `/account/deactivate`,
`DELETE /devices/{deviceId}` and `POST /delete_devices`).

Wire types (`AuthType`, `AuthData` and its per-stage variants, `AuthFlow`, `UiaaInfo`) are reused
directly from `ruma::api::client::uiaa` as plain `serde` types rather than redefined — they already
have hand-written, non-`ruma_common::api`-macro-based `Serialize`/`Deserialize` implementations
that parse exactly the spec's shapes (confirmed by reading `ruma-client-api`'s source; MIT,
adapted by dependency, not copied). `crate::uia` owns only the session bookkeeping on top of them
(does a session exist and is it still within its timeout, which stages have completed, is some
offered flow now fully satisfied); it deliberately does **not** know how to verify any individual
stage — that is always the caller's job, because only the caller has the context (which user's
password, which registration-token store, whether this account even has a password to re-check).

**Deliberate simplifications, each with a documented reason in code:**

- **No "recent login" UIA-skip grace period.** Synapse lets a client skip UIA entirely for a short
  window after a fresh login. This implementation always requires the full re-auth round. Simpler,
  strictly more conservative; a grace period can be added later without an interface change (it
  would only affect whether `reauth::run` is called at all, not its shape).
- **Registration flows are a single AND of whatever is enabled**, not multiple alternative flows.
  If neither a registration token nor terms are required, the flow is `[m.login.dummy]`; if one or
  both are required, the flow is `[stage, stage, ...]`. Real deployments needing genuinely
  alternative flows (for example "token OR terms, not both") are Phase 1/2 work.
- **`m.login.recaptcha`, `m.login.email.identity` and `m.login.msisdn` are never offered** in the
  flows this server advertises (no verification backend exists yet for any of the three). If a
  client submits one anyway, it fails cleanly with `400 M_UNRECOGNIZED` and a message naming the
  unsupported stage, not a generic "wrong credentials" `403` — a client probing capabilities gets
  an honest, distinguishable answer instead of appearing to have gotten a stage's verification
  wrong.
- **Registration re-sends the whole body every round**, per the current spec text (not just
  `auth`/`session`), so no `username`/`password` needs to survive in UIA session data across
  rounds — it is simply re-validated every time and only committed once the flow completes.

## 7. The `Requester` middleware contract

`crates/hs-auth/src/middleware.rs`. Every handler that needs to know who is calling takes
`Requester` (rejects guests) or `AllowGuest` (does not) as an ordinary axum handler parameter;
axum's `FromRequestParts<AuthState>` runs before the handler body and produces either a populated
value or a `MatrixError` response — no handler parses headers or query parameters for
authentication itself.

Algorithm, reproducing `synapse/api/auth/base.py` and `internal.py`'s observable behavior
(behavioral reference only):

1. **Extract the token.** Exactly one of an `Authorization: Bearer <token>` header or a legacy
   `?access_token=` query parameter (the latter gate-able off via
   `AuthConfig::accept_legacy_query_param_token`, default on). Both present, or a malformed
   `Authorization` header, or neither present, is `401 M_MISSING_TOKEN`.
2. **Try the appservice registry first** (`crate::appservice::AppserviceRegistry`, today an
   in-memory stub — see section 9). A match authenticates as that appservice, honoring `user_id`
   (masquerade, checked against `AppserviceRecord::can_control`, `403 M_FORBIDDEN` otherwise) and
   `device_id` **or `org.matrix.msc3202.device_id`** (device masquerade; both parameter names are
   accepted since real-world appservices disagree on which they send — Synapse's currently
   observed behavior only reads plain `device_id`, so this is a deliberate superset for
   compatibility, not a narrowing). An unrecognized masqueraded device is
   `400 M_UNKNOWN_DEVICE`.
3. **Otherwise, look it up as an ordinary access token.** Missing or expired →
   `401 M_UNKNOWN_TOKEN`, with `soft_logout: true` on expiry (tells the client it is safe to keep
   local room state while re-authenticating — matches the spec's soft-logout semantics). A
   deactivated account's lingering token is also treated as unknown (deactivation is supposed to
   have revoked every token; this is defense in depth for a token that somehow outlived that). A
   locked account is `401 M_USER_LOCKED`. `M_USER_SUSPENDED` is **not** raised here — see section
   4's note on `suspended`.

## 8. Known gaps and follow-ups

- **3PIDs are a bare bind/lookup index** (`UserStore::bind_threepid`/`get_user_by_threepid`), not
  the full identity-server validation-session flow. `m.login.password` by email/phone identifier
  works once a 3PID is bound by some other means (an admin action, an import); there is no
  `/register/email/requestToken` verification loop yet. Phase 1/2 work per the track brief.
- **Phone-number identifier canonicalization** (`m.id.phone`'s `country` + `phone` fields) is a
  digits-only concatenation, not full E.164 canonicalization through a country-calling-code table.
  Clients that already send a bare MSISDN as `phone` with an empty `country` are unaffected.
- **Registration tokens are a flat always-valid set** (`AuthConfig::valid_registration_tokens`),
  not Synapse's per-token usage-limit/expiry table. Extending this is a store change (a new
  `RegistrationTokenStore` trait), not an API change.
- **Rate limiting is a trait with an in-memory token-bucket implementation**
  (`crates/hs-auth/src/ratelimit.rs`) but is not yet wired into any handler with real per-IP keys —
  that needs a client IP, which is `hs-http`'s listener's job to supply. The trait boundary exists
  so wiring it in later does not touch handler logic.
- **SSO redirect and token handoff, upstream OIDC/SAML/LDAP, MAS delegation** are not in this RFC
  at all; they are Phase 1/2 per the track brief and, for the native-issuer direction, designed in
  RFC 0003.

## 9. Interfaces to other tracks

- **Storage** (section 10 has the trait design). No dependency on `hs-kv`/`hs-tables` yet, per
  `docs/workstreams/README.md` rule 1; an `hs-tables`-backed `AuthStore` implementation lands
  later behind a follow-up RFC without changing any handler, since every handler holds
  `Arc<dyn AuthStore>`/`Arc<dyn TokenStore>` etc., never a concrete type.
- **Track 11 (appservices)**: `crate::appservice::AppserviceRegistry` is a deliberate stub (see
  section 4's Phase 0 deliverable: "appservice tokens and identity assertion against 11's registry
  stub"). Track 11's real registry either implements this trait directly, or this trait moves to
  `hs-appservice` behind a follow-up RFC; `crate::middleware` does not change either way, since it
  only depends on the trait, not the concrete registry.
- **Track 03 (cluster)**: `Requester`/`RequesterContext` is the serializable type their forwarding
  envelope should carry (section 4). Track 03's status file (`docs/status/03-cluster.md`) already
  names this as an interface it needs from this track; it is now frozen in the sense that
  `Requester`'s fields are stable, though new fields may be added additively.
- **Track 15 (admin API)**: RFC 0004 section 8.1 defines `hs_admin::auth::TokenVerifier`, which
  this track implements once the native OAuth issuer (RFC 0003) exists, and RFC 0004's scope table
  (`admin:read`, `admin:write`, `bridges:*`, `moderation:*`) is the scope vocabulary RFC 0003
  section 6 adopts directly rather than inventing a second one.
- **Track 13 (config)**: every field of `crate::config::AuthConfig` is a day-one stand-in with a
  named Synapse config-option analog in its doc comment, for mechanical mapping once the native
  config schema exists.

## 10. Storage traits

See `crates/hs-auth/src/store/mod.rs` for the authoritative trait definitions (`UserStore`,
`DeviceStore`, `TokenStore`, `UiaStore`, unioned as `AuthStore`) and
`crates/hs-auth/src/store/memory.rs` for the in-memory implementation every test in this crate
runs against today. Summary of the design choices:

- **Four traits, not one god-trait**, so a future backend can implement (or track 01 can review)
  them independently, and so `AuthState` can in principle hold different concrete stores per
  concern if that ever becomes useful (it does not today; `InMemoryAuthStore` implements all
  four in one struct).
- **Records are plain structs with public fields**, not opaque handles — a storage backend has no
  behavior to hide here; the interesting logic (token generation, password verification, UIA) all
  lives above the trait boundary in this crate, not inside implementations of it.
- **Errors are a three-variant `StoreError`** (`Conflict`, `NotFound`, `Backend`) — deliberately
  thin, so callers branch on *meaning*, not on which backend produced the failure; `Backend`
  failures are logged and turned into a generic `M_UNKNOWN`/500 at the `MatrixError` boundary
  (`impl From<StoreError> for MatrixError`), never surfaced to a client.
