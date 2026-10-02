# 0025: An admin token carries the scopes it was minted with (2026-10-02)

Status: accepted (track 15, with 16 for the page and `hs-cli` for the store and CLI). Closes
the `docs/next-steps.md` gap "No admin token narrower than full access can be minted".

## Context

RFC 0004 section 8.2 defines six scopes and every one of the 154 authenticated operations
enforces its documented one (status 15, 2026-10-01). But the only credential the `hs` binary
accepted was a Matrix access token of a user with the administrator flag, which the legacy
verifier grants `admin:read` and `admin:write`. The scopes mattered to tests and to nobody
else: an operator could not hand a bridge team a `bridges:read` token or a moderation bot a
`moderation:write` one. Section 8.1 names the principal kinds that would fix this (`user` and
`client` from an OAuth issuer, `service_account` from the CLI); the issuer is unbuilt.

## Decision

- **Admin tokens are section 8.1's `service_account` principals**, minted at
  `POST /api/v1/admin-tokens` (`admin:write`), from the Settings page or `hs admin-token`.
  A token's `scopes` is an explicit list of the six, stored and reported in catalog order.
  `Principal.scopes` is exactly that list; `Scope::satisfies` supplies the implications
  (`admin:write` everything, each `*:write` its `*:read`, `admin:read` every `:read`).
- **The default is a full administrator's** (`admin:read` and `admin:write`) when `scopes`
  is omitted, so a token minted without thinking about scopes does what the legacy credential
  does; an explicit empty list is refused, since a token that can do nothing is a mistake.
- **Only `admin:write` mints, lists, inspects or revokes tokens.** No per-request check that a
  minter holds what it grants is needed: a token can never mint one wider than its holder,
  because only a holder of everything can mint at all.
- **The token is `hsa_` and 40 letters and digits, shown once, stored as its SHA-256.** The
  prefix lets the verifier tell it from a Matrix token (`syt_`) without a lookup and lets an
  operator recognise one in a log. `ScopedTokenVerifier` decides `hsa_` bearers from the
  token store alone (`Invalid`, `Expired`, or the principal) and hands every other bearer to
  the legacy verifier unchanged, so `hs-auth` is untouched.
- **The principal's `id` is the token's id (a ULID) and `display_name` its name**, so the audit
  log and the rate limiter key on something unique and an operator still reads which token it
  was. `created_by` on a token is its minter's principal id: a user id, or another token's id.
- **Revocation deletes the row.** The list shows the live tokens; the audit log keeps the mint
  (with `/scopes`) and the revocation (with the scopes it carried), never the token.
- **Storage lives in `hs-cli`** (`TablesAdminTokens` over `hs-kv`, two keyspaces written in one
  transaction), as the audit log does; `hs-admin` keeps the trait and an in-memory source.
- **Refusals are counted**: `hs_admin_scope_refusals_total{required_scope}`.

## Consequences

- A bridge team's dashboard, a moderation bot or a deploy pipeline each get a token holding
  only what they need, and a request outside it is the RFC's `403` naming the scope.
- The web interface still signs an operator in with their own (full) account; the page says so.
  When 07's issuer exists, its `user` and `client` principals sit beside these through the
  same `TokenVerifier` seam, and `Principal.issued_by` tells them apart (`admin-tokens`,
  `legacy-admin-flag`).
- Not done: `last_used_at` (a write per verified request), object-level restrictions (RFC 0004
  section 16, after v1), and token-specific rate limits.
