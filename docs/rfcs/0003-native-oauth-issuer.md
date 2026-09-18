# RFC 0003. The native OAuth 2.0 authorization server

Status: proposed (design only; no implementation in this pass). Owner: track 07. Consumers: 15
(admin API scopes and `TokenVerifier`), 16 (management interface login), 08 (device semantics for
E2EE), every client-facing track indirectly (this is what makes Element X and other
next-generation-auth clients able to log in at all).

`PLAN.md` D5: "The homeserver is its own OAuth 2.0 authorization server." This RFC is the design
for that; `docs/rfcs/0002-auth-tokens-and-requester.md` is the legacy auth design it sits beside
(both exist permanently — the native issuer does not replace `/login`, it is the *other* front
door, per MSC2964's coexistence model). Nothing in this RFC is implemented yet; `crates/hs-auth`
today has only the legacy surface. This document exists so implementation, when it starts, is not
also a design exercise, and so 15 and 16 can plan their own work against a stable scope now.

## 1. Scope of "native issuer"

The spec's next-generation auth API, per the MSC3861 family and its satellites:

| MSC | What it defines | This RFC's stance |
|---|---|---|
| MSC2964 | OAuth 2.0 API for Matrix client-server auth: the overall shape (authorization code + PKCE as the primary grant, discovery, coexistence with legacy `/login`) | Adopted as the baseline. |
| MSC2965 | OAuth 2.0 authorization server metadata discovery (`GET /_matrix/client/v1/auth_metadata`, wrapping RFC 8414) | Implemented as specified; see section 4. |
| MSC2966 | OAuth 2.0 dynamic client registration (RFC 7591) | Implemented; unauthenticated public endpoint, rate-limited. |
| MSC2967 | API scopes for the Matrix API (`urn:matrix:client:api:*`, `urn:matrix:client:device:<id>`) | Adopted verbatim; see section 6.1. |
| MSC3824 | OAuth 2.0 client hints for existing sessions ("this client already knows it's OAuth-only, skip the legacy-vs-native probe") | Adopted; a `/versions`-advertised capability. |
| MSC4254 | Native OAuth 2.0 account metadata endpoint | Adopted; folds into section 7's account management surface. |
| MSC4191 | Account management URL and deep-linking (`account_management_uri`, `account_management_actions_supported`) | Adopted; see section 7. |
| RFC 6749 | OAuth 2.0 core | Base protocol. |
| RFC 7636 | PKCE | Mandatory for the authorization code grant — never optional, even for confidential clients, matching the spec's stance that Matrix clients are effectively always public clients. |
| RFC 8628 | Device authorization grant | Implemented for QR-login and input-constrained clients (MSC4108 territory; device authorization grant is the OAuth piece of it, MSC4108's rendezvous transport is a separate, later concern owned jointly with 08). |
| RFC 8414 | Authorization server metadata | The document MSC2965's endpoint serves. |
| RFC 7591 | Dynamic client registration | MSC2966. |
| RFC 7662 | Token introspection | Implemented for this server's own resource-server-side checks and, in delegation mode, as the protocol spoken *to* an external MAS. |
| RFC 7009 | Token revocation | Implemented. |

**Out of scope for this RFC** (later design passes, or explicitly not this track's job): the
OpenID Connect layer on top (ID tokens, `/userinfo`) beyond what MSC2964 already implies is needed
for Matrix login itself; upstream OIDC/SAML/LDAP as *login methods into* this issuer (Phase 1/2,
brief section "upstream OIDC, SAML and LDAP providers" — a separate RFC when that work starts);
MAS delegation mode's full provisioning-API surface (sketched in section 9, detailed later).

## 2. Why a native issuer instead of only delegating to MAS

`PLAN.md` D5's stated reason: delegation makes Matrix Authentication Service a mandatory second
service for every deployment, which cuts directly against R9 (small ARM host, no external
dependencies) and against the operational simplicity goal generally. A native issuer means `hs
serve --single-node` is still a complete, spec-compliant OAuth 2.0 authorization server with zero
extra moving parts. Delegation remains available (section 9) for operators who already run MAS and
would rather not migrate its user base.

## 3. Crate selection

### 3.1 Authorization server framework: in-house, not `oxide-auth`

Evaluated `oxide-auth` (the only maintained general-purpose Rust OAuth 2.0 *server* crate) against
writing the grant state machines directly from the RFCs.

| | `oxide-auth` | In-house |
|---|---|---|
| Maintenance | Last substantive release activity has been slow; the crate predates async/await ergonomics in much of its API surface and its axum/hyper integration is a community adapter, not first-party | We control the release cadence entirely |
| Fit with `hs-auth`'s existing shape | Would require adapting its `Endpoint`/`Registrar`/`Authorizer`/`Issuer` trait family to `AuthStore`, effectively a second storage abstraction parallel to the one this crate already has for legacy tokens | Reuses this crate's own `TokenStore`/`UserStore` traits directly; one storage model, one token-hashing scheme, one `Requester` construction path for both legacy and OAuth-issued tokens |
| Matrix-specific requirements (device-scoped tokens via `urn:matrix:client:device:<id>` scopes, MSC2967's scope vocabulary, the account-management endpoints) | Not modeled by the crate at all; would be bolted on regardless | Modeled from the start |
| PKCE, device authorization grant, dynamic client registration | Supported, but each as a distinct extension with its own integration surface | Each grant is a few hundred lines against RFCs that are themselves short and precise; PKCE in particular is about 20 lines (S256 challenge verification) |
| Security review surface (`PLAN.md` section 15's stated risk: "an authorization server is a large security surface") | A dependency's CVE history and maintenance pace becomes part of the audit surface | Every line is ours to review, at the cost of writing every line |

**Recommendation: in-house**, built directly from RFC 6749/7636/8628/8414/7591/7662/7009 text, the
same way this crate already builds UIA and legacy tokens from the spec rather than from a
dependency. `ruma-client-api`'s existing OAuth-adjacent types (where present) are read the same way
the legacy surface reads `ruma::api::client::uiaa` — reused as plain wire types where they fit,
never as the source of the state machine's behavior. The deciding factor is not code volume (both
options are comparable) but *fit*: this crate already owns exactly the storage and token
primitives an issuer needs, and duplicating them behind a second abstraction to accommodate a
library built for a more generic OAuth server shape is a worse trade than writing the grants
directly, especially given how small and precise the individual RFCs are (PKCE and device-code
polling are the two with any real subtlety; neither benefits much from a general-purpose
abstraction layer).

### 3.2 JOSE layer: `jsonwebtoken`, not `josekit`

| | `jsonwebtoken` | `josekit` |
|---|---|---|
| Scope | Focused: JWS signing/verification, the handful of algorithms Matrix clients actually need (`RS256`, `ES256`, `EdDSA`) | Broader JOSE (JWE encryption too, which this issuer does not need — access tokens are opaque strings per section 5, not JWTs) |
| API shape | Simple, matches this crate's existing "small, direct" style | More general/ceremony for capability this issuer will not use |
| Maintenance | Active, widely used across the Rust ecosystem (including by other Matrix-adjacent projects) | Active but less widely adopted for this narrow a use case |

**Recommendation: `jsonwebtoken`**, used only where the spec actually requires a JWT: the
authorization server's signed metadata is plain JSON (RFC 8414, no JWS envelope), and this design
keeps *access tokens opaque* (section 5) rather than JWTs, so `jsonwebtoken`'s footprint is limited
to whatever ID-token-shaped artifact the OIDC layer eventually needs (out of scope here per
section 1) and to signing dynamic-client-registration software statements if a deployment ever
uses them (rare; RFC 7591 makes software statements optional and most Matrix clients register
without one).

## 4. Discovery and dynamic client registration

`GET /_matrix/client/v1/auth_metadata` (MSC2965) serves an RFC 8414 authorization server metadata
document. Fields this server populates beyond the RFC's required set:

- `authorization_endpoint`, `token_endpoint`, `registration_endpoint`, `device_authorization_endpoint`,
  `revocation_endpoint`, `introspection_endpoint` — all under this server's own base URL.
- `code_challenge_methods_supported: ["S256"]` — `plain` is never offered; PKCE without S256 is not
  meaningfully PKCE.
- `grant_types_supported: ["authorization_code", "refresh_token", "urn:ietf:params:oauth:grant-type:device_code"]`.
  Client credentials is supported for the admin API's `client` principal kind (RFC 0004 section
  8.1) but is **not** advertised here — it is not a grant ordinary Matrix clients use, and
  advertising it invites confusion about what it is for.
- `scopes_supported` — the full vocabulary from section 6.
- `account_management_uri` / `account_management_actions_supported` (MSC4191) — section 7.

`POST /_matrix/client/v1/register` (MSC2966 / RFC 7591): unauthenticated, rate-limited by source
IP. Accepts `redirect_uris`, `client_name`, `client_uri`, `logo_uri`, `tos_uri`, `policy_uri`,
`contacts`, `token_endpoint_auth_method` (`none` for public clients — the overwhelmingly common
case for Matrix clients — or `client_secret_basic` for confidential ones). Returns `client_id` and,
for confidential clients, `client_secret`. Registered clients are stored via a new
`OAuthClientStore` trait (parallel in spirit to `AuthStore`'s other traits, added to this crate
when implementation starts, not retrofitted onto the legacy `TokenStore`).

## 5. Token format

Access and refresh tokens issued by the native flow are **opaque strings**, not JWTs, reusing
`token::generate_access_token`/`generate_refresh_token`'s Synapse-compatible shape and
`TokenStore`'s hash-at-rest storage exactly as legacy tokens do (`docs/rfcs/0002-...md` section
3.4). This is a deliberate unification, not a missed opportunity to use JWTs: it means every
`Requester`-authenticating code path (`crate::middleware`) has exactly one token-lookup mechanism
regardless of which front door (legacy `/login` or the native issuer) minted the credential, and it
means token revocation is always immediate (a JWT's whole selling point — statelessness — is also
exactly the property that makes revocation-before-expiry require a separate denylist anyway, which
erases the benefit for a server that already has a fast, hashed, indexed token store). The
`AccessTokenRecord`/`RefreshTokenRecord` schema gains an `issued_by: Legacy | Native { client_id }`
tag and an OAuth token additionally records its granted `scope` string and `oauth_client_id`; no
other structural change from section 3.4's design.

Introspection (`POST /introspect`, RFC 7662) looks the token hash up in the same `TokenStore` and
reports `active`, `scope`, `client_id`, `username` (the Matrix user ID), `exp`. This is also
exactly the endpoint MAS delegation mode's resource-server side would call against an *external*
MAS if this server were configured as a delegating resource server instead (section 9) — the
request/response shape is identical either way, which is why building it once here pays for both
modes.

## 6. Scopes

### 6.1 Matrix API scopes (MSC2967)

- `urn:matrix:client:api:*` — full Matrix client-server API access, equivalent to a legacy access
  token's authority.
- `urn:matrix:client:device:<device_id>` — binds the token to a specific device the same way a
  legacy access token is bound via `AccessTokenRecord::device_id`; required alongside
  `urn:matrix:client:api:*` for any token a real client session uses (see section 8 on device
  semantics — this scope *is* how device identity is expressed under OAuth, there is no separate
  `device_id` request parameter the way legacy `/login` has one).
- `urn:matrix:client:guest` — a guest session; mutually exclusive in practice with the full `:api:`
  scope's write authority the same way `Requester::is_guest` gates writes today.

### 6.2 Admin API scopes (from RFC 0004)

RFC 0004 section 8.2 already defines the admin API's scope vocabulary (`admin:read`, `admin:write`,
`bridges:read`, `bridges:write`, `moderation:read`, `moderation:write`) and section 8.1's
`hs_admin::auth::TokenVerifier` trait as the interface this track implements. This RFC does not
invent a second scope vocabulary for admin access: a native-issuer token requesting one of these
scopes is the same token type, going through the same introspection path, that
`hs_admin::auth::TokenVerifier::verify` calls to turn a bearer token into an
`hs_admin::model::Principal`. Concretely, once implementation starts: `TokenVerifier::verify`
becomes a thin wrapper around this issuer's own introspection logic (section 5), mapping the
token's `scope` string onto `Principal::scopes` and its `kind` onto `user`/`client`/`service_account`
per RFC 0004 section 8.1's definitions. A legacy access token belonging to a user with the
server-administrator flag continues to map to `kind: legacy` with `["admin:read", "admin:write"]`,
unchanged.

Scope grant policy: `admin:*` and `bridges:*`/`moderation:*` scopes are only ever granted to a
token when the authorizing user already holds the corresponding authority (the legacy admin flag,
or a future finer-grained admin-role system out of this RFC's scope) — the authorization code
grant's consent step for these scopes is operator-only (there is no "an ordinary user consents to
grant a third-party client `admin:write`" flow; that would be a confused-deputy risk this design
explicitly avoids by never offering admin scopes in the consent screen for non-admin accounts).

## 7. Account management (MSC4191 / MSC4254)

`account_management_uri` in the discovery document points at a minimal server-rendered account
page (plain server-rendered HTML forms, no SPA framework, no build step — matching the "minimal"
answer to the brief's open question "the account management UI stack"): view active sessions
(devices + their OAuth clients and granted scopes), revoke a session, change password (for
accounts that have one), view and manage 3PIDs, and a link into `hs-admin`'s
`account_management_actions_supported` set for whichever of `org.matrix.profile`,
`org.matrix.sessions_list`, `org.matrix.session_view`, `org.matrix.session_end` MSC4191 defines
this server implements — initially all four, since each maps to an existing `AuthStore` operation
already built for the legacy surface (`DeviceStore::list_devices`, `TokenStore::delete_*`).

## 8. Device semantics under OAuth

The brief's open question: "a device is a client instance carrying a `device_id` scope." Concretely
under this design: the authorization code grant's `scope` parameter includes
`urn:matrix:client:device:<device_id>`, where `<device_id>` is either client-chosen (a client that
already has one, reconnecting) or server-generated and returned in the token response the same way
`session::create_session` generates one today for legacy `/login`. `crate::session::create_session`
is reused as-is for the native flow's token issuance step — device creation, `DeviceRecord`
upsert-or-touch, and access/refresh minting do not need OAuth-specific versions, only an
OAuth-specific *front door* that ends up calling the same function. This is the same unification
principle as section 5's token format: one device/session lifecycle, two ways to authenticate into
it.

## 9. MAS delegation mode

For operators who already run Matrix Authentication Service and would rather not migrate its user
database: a config-only mode (`auth.delegate_to_mas: <url>`) where this server acts purely as an
OAuth 2.0 *resource server* against an external authorization server. `crate::middleware`'s token
lookup gains a third branch (after appservice, before/instead of local `TokenStore` lookup): an
opaque bearer token that is not found locally is introspected against MAS's `/introspect` endpoint
(RFC 7662, the same client-side shape this server's own introspection endpoint serves, per section
5); a successful introspection is cached briefly (respecting MAS's `exp`) and produces a
`Requester` the same way a local lookup does. This mode also implements the specific
`/_synapse/mas/*`-shaped internal provisioning endpoints MAS expects when paired with a
Synapse-compatible homeserver (behavioral reference only, read from
`refs/synapse/synapse/api/auth/msc3861_delegated.py` and MAS's own documentation, not copied — MAS
is AGPL-3.0). Full provisioning-endpoint design (user/device sync from MAS's notion of accounts
into this server's `UserStore`/`DeviceStore`) is deferred to its own RFC when delegation mode
implementation starts; this section exists to record that the resource-server-side introspection
path is designed *now* to be shape-compatible with both "introspect against ourselves" (native
mode, section 5) and "introspect against an external MAS" (this mode), so building native mode
first does not foreclose delegation mode later.

## 10. Implementation ordering (when this work starts)

Not a commitment of this RFC by itself (that is Phase 1/2 planning), but the natural dependency
order given the above:

1. `OAuthClientStore` trait + in-memory implementation (parallel to `AuthStore`'s existing traits).
2. Discovery metadata endpoint (section 4) — static-ish, unblocks client testing against the rest.
3. Dynamic client registration (section 4).
4. Authorization code + PKCE grant, reusing `session::create_session` (sections 5, 8).
5. Refresh grant (reuses `TokenStore`'s existing rotation/reuse-detection from RFC 0002 section
   3.3 almost unchanged — the theft-detection logic does not care which front door minted the
   original pair).
6. Device authorization grant (RFC 8628) for QR/input-constrained clients.
7. Introspection and revocation endpoints (section 5), which is also most of MAS delegation mode's
   resource-server side (section 9) for free.
8. Admin scope wiring (`hs_admin::auth::TokenVerifier`, section 6.2).
9. Account management UI (section 7).
10. MAS delegation mode's provisioning surface (section 9), as its own follow-up RFC.

## 11. Definition of done (for whenever implementation lands, restated from the track brief)

RFC-behavior tests for 6749, 7636, 8628, 8414, 7591 and 7662; `matrix-rust-sdk` OIDC flow tests
green (Element X logs in through the native issuer); dynamic client registration, PKCE, device
authorization grant, refresh and revocation all exercised end to end; the track in scope of the
external security review the brief's risk section calls for before this surface is considered
production-ready.
