# 0011. A registration token lets one person in while open registration is off

Status: accepted, 2026-09-27. Author: track 15 (with the registration-token work).
Applies to tracks 07 (auth, whose `/register` changed), 13 (configuration) and 16 (web).

## Decision

With `auth.enable_registration` off -- the default -- `POST /register` still accepts a
registration that completes the `m.login.registration_token` stage with a token from the
registration-token store (the admin API's `registration_tokens.*`). That is what an invite link
is: an administrator makes a token, sends the link, and the person it was sent to registers on a
server that is otherwise closed.

- A closed server with no usable token answers exactly as before: `403 M_FORBIDDEN`,
  "Registration is disabled". A closed server with at least one usable token answers a
  registration that presents nothing with `401` and the single flow
  `[m.login.registration_token]` (plus `m.login.terms` when terms are on), so a client knows to
  ask for a token.
- With registration on, nothing changes unless `registration_requires_token` is set, as before.
- `GET /_matrix/client/v1/register/m.login.registration_token/validity` answers the same whether
  registration is on or off (Synapse refuses it with `403` when registration is off; here a token
  is precisely what works while it is off).
- Passing the stage takes one of the token's places for that user-interactive-auth session
  (`pending`) until the account is created (`completed`) or the session expires. `pending`
  counts against `uses_allowed`, so a one-use token cannot be presented by two people at once;
  an abandoned registration stops counting when its session would have expired, so it does not
  hold the place forever.
- Tokens listed in the configuration file (`valid_registration_tokens`, which has no limits)
  still work as before, checked first.

## Why

Synapse requires open registration *and* a token (`enable_registration` plus
`registration_requires_token`), which makes "invite a person" mean "open the server to anybody
holding any token". Operators who want invite-only registration want the server closed except to
the people they invite; one token, one person, is that.

## Consequences

- Where this is implemented: `crates/hs-auth/src/routes/register.rs` (the flow) and
  `crates/hs-auth/src/registration_tokens.rs` (the store, `hs_auth.registration_tokens`).
- The web interface's invite link, `/admin/register?token=...`, relies on it.
- A Synapse-migrated server with `enable_registration: false` and tokens in its database would
  now admit those tokens' holders. The importer does not import registration tokens yet; when it
  does, it should say so.
