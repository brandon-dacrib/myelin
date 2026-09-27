# RFC 0020: a hidden secret inside a list entry survives saving the list

Status: proposed, 2026-09-27. Author: track 16 (web). Needs: track 15 (`hs-admin`), with track 13
(`hs-config`) consulted.

## Problem

Decision 0010 replaced the Configuration page's JSON textarea with structured editors, so
`auth.oidc_providers` is now a form per provider. Each provider has an inline `client_secret`
(`OidcProviderConfig`, a `SecretString`), which `GET /config/auth` serves redacted as
`{"$secret": true}`.

`PATCH /config/{section}` is an RFC 7396 merge patch, and a merge patch replaces an array
wholesale: to change one field of one provider the interface must send the whole list. It sends
each untouched secret back as the placeholder it was shown. `SecretPaths::strip_echoed_secrets`
(`crates/hs-admin/src/config_schema.rs`) then removes every placeholder at a secret path --
including `/auth/oidc_providers/0/client_secret` -- which for a setting of its own means "leave
it alone", but inside an array element means the element is stored *without* its secret. So
editing a provider's display name silently clears every provider's inline client secret. The
same holds for any future secret inside a list.

The interface now says so in the list editor whenever an entry holds a hidden secret ("saving a
change to this list clears them"), and the mock server reproduces the behaviour rather than hide
it. That is honest, but it is not administrable: the operator cannot keep a secret they cannot
see.

## Proposal

When stripping an echoed placeholder that sits inside an array, restore the stored value instead
of dropping the key:

1. **Minimum (index match).** A placeholder at `/auth/oidc_providers/{i}/client_secret` is
   replaced by the value currently stored at that same pointer, if there is one; otherwise it is
   dropped as today. Correct as long as entries are not reordered or removed ahead of it.
2. **Preferred (named origin).** Accept a second marker form, `{"$secret": true, "$from":
   "/auth/oidc_providers/2/client_secret"}`, naming the pointer (in the currently stored
   document, at the revision the `If-Match` names) the secret came from. The server replaces it
   with the value stored there, or rejects the patch with a validation error on that pointer if
   nothing is stored there. This survives reordering and removal, which the interface's list
   editor offers (move up, move down, remove). The plain marker keeps meaning 1.

Nothing about the redaction on read changes, and a secret is still never returned.

## Affected tracks

- 15 (`hs-admin`): `strip_echoed_secrets` (and its tests) restore rather than drop inside arrays;
  the OpenAPI `ConfigSection` description documents the `$from` form.
- 16 (`web/`): once 2 ships, the list editor records each entry's original index and sends
  `$from` for an untouched secret; the warning note in `StructuredControls.tsx`
  (`ObjectListControl`) and the mock's `stripEchoedSecrets` in `web/src/mocks/data/config.ts` go.
- 13 (`hs-config`): none, unless the marker is better handled in `document.rs`'s merge.

## Migration

None: today's plain placeholder inside an array already loses the secret, so any restoration is
strictly better, and a client that never sends `$from` is unaffected.
