# 0009. Bridge offerings: the list operations are pages, heisenbridge is shared, and the catalogue says why a type cannot be deployed

Status: accepted, 2026-09-27. Author: track 11, while running RFC 0017 against the real binary.
Applies to tracks 15 (`crates/hs-admin`, the OpenAPI document) and 16 (`web/`).

## Decisions

1. **`bridge_offerings.list` and `bridge_instances.list` answer a page**, `{items, next_cursor,
   prev_cursor}` (`BridgeOfferingPage`, `BridgeInstancePage` in the document), like every other
   list operation. The document said `{data: [...]}`, the interface and its mocks followed the
   document, and the router had always answered a page: against the real server the Bridges
   page and every offering page showed nothing. The document and the interface moved to the
   router's shape rather than the router to the document's, because a third shape for two
   operations was the mistake. No cursor is ever issued: there is one offering per catalogue
   type and an offering's instances are read whole.
2. **heisenbridge is a `shared` type.** It is a bouncer one process serves a whole server with:
   the owner named on its command line is its administrator, any local user the owner allows
   drives their own networks through it, and without an owner the first local user who talks to
   it claims it. RFC 0017 section 2 already said so; the catalogue said `per_user`. Its one
   instance is created by the offering's `PUT`, is addressed as `_`, is named `heisenbridge`
   (bot `@heisenbridge`, ghosts `@irc_*`), and is rendered without `-o`.
3. **Additive fields**, so the interface stops guessing: `BridgeType.not_deployable_reason`
   (why this server cannot deploy the type, in words for an administrator; `null` when it can,
   and always `null` exactly when `deployable` is true), `BridgeOffering.image_tag` (what the
   request took, so an edit does not have to parse the image reference), and
   `BridgeInstance.last_ping_at` and `last_error` (the registry's, so the instance row can say
   what went wrong without a second request).
4. **An administrator's `PUT` of an instance bypasses `access.users`.** The access list says
   who may ask the front door; an administrator adding someone by hand has decided. The front
   door and the manager bot enforce the list; the admin API does not.
5. **Not changed**: the `409` on deleting an offering with instances carries the count in
   `detail` only (`hs_http::Problem` has no extension members); the interface's confirm dialog
   does not need the number. The bridge operations are enforced with `admin:read`/`admin:write`
   in the router while the document names `bridges:read`/`bridges:write`; an administrator's
   session holds `admin:write`, which satisfies every scope, so nothing is locked out, and the
   two are left for track 15 to reconcile in one place.

## Consequences

`web/src/api/schema.d.ts` was regenerated; `useBridgeOfferings` and `useBridgeInstances` read
`items`; the mock handlers answer pages; `notDeployableReason` prefers the server's reason;
`requestFromOffering` prefers `image_tag`. The interface's mock catalogue already had heisenbridge
shared. `docs/rfcs/0017-the-server-deploys-its-own-bridges.md` section 5 stands as written; its
section 8 records what was run.
