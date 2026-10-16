# 0053: A bridge's double puppet is rate-limited as its appservice is

Date: 2026-10-16. Track 07 (`hs-auth`), with `hs-room` (the checks), `hs-appservice` (the
registry lookup), `hs-admin` and `web/` (the sessions list).

## Context

On the demo (2026-10-16) the iMessage bridge double puppets @brandon the way mautrix bridges do
with `login_shared_secret: appservice`: `POST /_matrix/client/v3/login` with
`type: m.login.application_service`, authenticated by the bridge's own `as_token`, for a user its
double-puppeting registration claims (decision 0046), and then every request on the ordinary
access token that login returns. Those requests were an ordinary user's
(`Requester.appservice == None`), so the server-wide `rate_limits.message` applied to them,
though the bridge's registration says `rate_limited: false`. A catch-up of the bridge produced
repeated `429 M_LIMIT_EXCEEDED` (`hs_room_server_rate_limited_writes_total`), each retried by the
bridge 5 s later. The owner asked that double puppeting not be rate-limited by default.

Synapse does not attribute these tokens either; its operators give the double-puppeted user a
rate-limit override (`/_synapse/admin/v1/users/<user>/override_ratelimit`), which also lifts the
limit from the person's own clients.

## Decision

- **The token says which appservice it was issued to.** `AccessTokenRecord` and
  `RefreshTokenRecord` carry `issued_by_appservice: Option<String>`, set by an
  `m.login.application_service` login (the appservice whose `as_token` logged in, also when it
  was its double-puppeting registration that let it) and by an appservice's `POST /register`
  without `inhibit_login`. `/refresh` copies it to the new pair. Every other session has `None`.
  Stored as an optional JSON field (`serde(default, skip_serializing_if = "Option::is_none")`):
  rows written before it read as `None`, rows without it are written as before, and an older
  binary ignores it. No migration.
- **The requester carries it as its own field, not as `Requester.appservice`.**
  `Requester.issued_by_appservice: Option<IssuingAppservice { appservice_id, rate_limited }>`.
  Such a request is an ordinary user session in every other respect: no masquerading, no
  namespace checks, no appservice-only endpoints, `authenticated_entity()` is the user. The
  field is `serde(default)` so a requester serialized by a replica one release behind still reads
  over the mesh.
- **Resolved per request from the current registration.** The middleware asks the registry
  (`AppserviceRegistry::rate_limited_of(id)`, one keyed read of the registration) on each request
  on such a token. A registration edited to `rate_limited: true` limits the next request; a
  removed one leaves the token an ordinary, limited session (and the registry's default answer,
  "no such registration", does the same).
- **One rule, one helper.** `Requester::rate_limit_exemption()` says whether and why a request is
  exempt; `Requester::exempt_from_rate_limits(user_has_override)` applies the order:
  1. the appservice's own request (`as_token`) under `rate_limited: false`: exempt, even over a
     per-user override (as before, and as Synapse);
  2. a session issued to an appservice under `rate_limited: false`: exempt **unless the user has
     an administrator's rate-limit override**, which then applies: the account is a real
     person's, and an override on it is a deliberate choice about it;
  3. anyone else: limited.
  `hs-room`'s `check_send_limit` (events and state), `check_redaction_limit` (an exempt
  administrator's redactions go through `check_send_limit`, not `admin_redaction`) and
  `check_join_limit` (no override applies to joins, so rule 2 is plain exemption there) use it.
  Those are every check that exempted appservices; the per-address buckets (login, registration,
  3PID validation) never did.
- **The person's other sessions are unchanged.** Their Element login has no attribution and
  stays under `rate_limits.message`. The per-user bucket is shared, but exempt requests take
  nothing from it.
- **Observable.** `hs_room_puppet_rate_limit_exempt_writes_total` counts writes let through
  because of rule 2 (one atomic increment; it is how an operator sees the exemption at work:
  during a bridge's catch-up it rises while `hs_room_server_rate_limited_writes_total` stays
  flat). The middleware logs each attributed request at DEBUG ("a session issued to an
  appservice", or "... that is no longer registered"). The admin API's sessions list
  (`users.sessions.list`, OpenAPI 0.1.20) has `Session.issued_by_appservice`, and the Users page
  marks such a session "Bridge double puppet" with what that means.

## Consequences

- **Tokens issued before this change carry no attribution** and stay limited: nothing about them
  says reliably that an appservice login made them (the device's display name is the client's
  choice, and its id is random). Not guessed. For a bridge that is already double puppeting,
  either sign its old session out (Users, the person, Sessions; or `DELETE
  /api/v1/users/{user}/devices/{device}`), after which mautrix logs in again with its shared
  secret and the new session is attributed, or give the person a rate-limit override of 0 (no
  limit, on all of their sessions, as on Synapse).
- One registry read per request on an attributed token (the appservice path already reads the
  registry for every request's token).
- mautrix bridges that double puppet with `as_token:<token>` of their `-doublepuppet`
  registration send with that token and `user_id` (masquerading, per mautrix's documentation of
  that method; not re-checked against its source here), so rule 1 already covered them, under
  the `-doublepuppet` registration's `rate_limited: false`. This decision is for bridges that
  log in: mautrix-imessage with `login_shared_secret: appservice`, and any bridge whose double
  puppeting logs in with `m.login.application_service`.
