# 07. Auth and identity

Wave 1, starts day one. Every HTTP handler depends on the middleware this track freezes at week 6.

**Expert profile.** OAuth 2.0 and OpenID Connect from the authorization-server side, web security, Matrix user-interactive auth, SAML and LDAP, password hashing and session management.

**Mission.** Legacy Matrix authentication with full fidelity, a native OAuth 2.0 issuer per the spec's next-generation auth API, upstream identity providers as login methods, and an optional delegation mode for operators who run Matrix Authentication Service. See `PLAN.md` section 4 (D5).

**Owns.** `hs-auth`: the `Requester` middleware (user, device, appservice identity assertion and device masquerading with 11, admin flag, guest), legacy `/login` types (`m.login.password`, `m.login.token`, `m.login.application_service`, SSO redirect and token handoff, JWT), user-interactive auth (dummy, password, recaptcha, terms, email, msisdn, registration token, SSO), registration including the shared-secret admin registration protocol with 13, tokens (access, refresh, login) and their storage, devices lifecycle (with 08 for keys), password policy, password hashing (argon2 native; bcrypt with pepper verification for imported hashes), 3PIDs and identity-server interactions, account lifecycle (deactivation, erasure, suspension, locking, account validity, consent), the native OAuth 2.0 authorization server and OIDC issuer (MSC2964, MSC2965, MSC2966, MSC2967, MSC3824, MSC4254, MSC4191 account management), upstream OIDC, SAML, LDAP and CAS providers, MAS delegation mode (token introspection and the provisioning endpoints MAS expects), `/capabilities` auth items, auth-related rate limits.

**Provides.** Week 4 (with 01): user and device tables. Week 6: the `Requester` middleware.

**Consumes.** 01, 13 (config), 14 (harness).

**Day-one work.** Token formats and the middleware; user and device model; hashing; legacy login, register and UIA on the in-memory backend; a threat model; an authorization-server design document with crate selection (evaluate `oxide-auth` against an in-house implementation from the RFCs; the JOSE layer from `josekit` or `jsonwebtoken`).

**Phase 0 deliverables.** Middleware and legacy auth complete on the in-memory backend with tests; appservice tokens and identity assertion against 11's registry stub; admin flag; error codes and UIA flows matching Synapse's observable behavior.

**Phase 1 and 2 deliverables.** Full legacy auth; 3PIDs with email and msisdn; upstream OIDC and SAML SSO; the native authorization server with dynamic client registration, PKCE, device authorization grant, refresh and revocation, introspection, and a minimal server-rendered account management UI; MAS delegation; account lifecycle; QR login (MSC4108) with 08.

**Definition of done.** Complement login and registration tests green; `matrix-rust-sdk` OIDC flow tests green (Element X logs in through the native issuer); RFC-behavior tests for 6749, 7636, 8628, 8414, 7591 and 7662; differential tests against Synapse for error codes and UIA flows; the track is in scope of the external security review.

**References.** Spec client-server "Client Authentication" and the OAuth 2.0 API sections; the MSCs above; `refs/synapse/synapse/handlers/auth.py`, `register.py`, `ui_auth/`, `oidc.py`, `saml.py`, `sso.py`, `refs/synapse/synapse/api/auth/msc3861_delegated.py` and `refs/synapse/synapse/rest/synapse/mas/` (what MAS expects; behavior only, AGPL); MAS documentation as a behavioral reference.

**Open questions to settle first.** Device semantics under OAuth (a device is a client instance carrying a `device_id` scope); token hashing at rest; the account management UI stack (server-rendered, minimal); whether CAS is worth keeping.

**Risks.** An authorization server is a large security surface; scope it after legacy auth ships and keep the delegation mode as the alternative.
