# 0013. Moderators read rooms and media, not messages

Date: 2026-09-28. Status: accepted. Settles `docs/next-steps.md` queue item 2e's "RFC 0004 against
the document on moderator read scope".

## The disagreement

RFC 0004 section 8.2 gives `moderation:read` "read `/users`, `/rooms`, `/reports`, `/media`, their
sub-resources", and says `admin:read` reads "every resource". The OpenAPI document declared every
room and media read (`rooms.list`, `rooms.get`, `rooms.members.list`, `media.list`, ...) as
`admin:read`, and `Scope::satisfies` did not let `admin:read` satisfy `moderation:read` or
`bridges:read`. So a moderator could quarantine a piece of media but not list it, block a room
but not open it, and a read-only administrator could not read the reports queue at all.

Meanwhile the room long tail brings reads that are not like the others: a room's timeline and
its events are people's messages.

## The decision

1. **`admin:read` satisfies every `*:read`** (`bridges:read`, `moderation:read`), as RFC 0004
   says. It still satisfies no write. `Scope::satisfies` in `crates/hs-admin/src/model.rs` and
   `hasScope` in `web/src/lib/auth.ts` both say so.
2. **Room and media metadata is a moderation read.** These operations declare `moderation:read`:
   `rooms.list`, `rooms.get`, `rooms.members.list`, `rooms.state.list`, `rooms.aliases.list`,
   `rooms.hierarchy.get`, `rooms.media.list`, `media.list`, `media.get`. A moderator can see what
   they act on.
3. **Message content needs `admin:read`, and every read of it is audited.** `rooms.messages.list`,
   `rooms.events.get`, `rooms.events.at`, `rooms.events.context` and `events.get` stay `admin:read`,
   and each successful call writes an audit entry `rooms.content.read` (actor, target room or
   event, the path read) although it changes nothing. A read whose entry cannot be written is
   refused with `503`, as a mutation is. The content of a reported event reaches a moderator
   through `reports.get`, which is the case a moderator needs.
4. **Server internals stay `admin:*`.** `rooms.forward_extremities.list` is `admin:read`;
   `rooms.forward_extremities.delete` and `rooms.join` are `admin:write` (the document's existing
   choice: putting a user into a room is not a moderation action).

## Consequences

- Additive for clients holding `admin:read` or `admin:write`: nothing they could read before is
  refused now.
- The operations table (`crates/hs-admin/openapi/operations.json`) and the document were changed
  together, and the scope descriptions in the OAuth2 scheme say the above.
- **Left for the Users track:** the same rule applied to `users.*` reads (`users.list`,
  `users.get`, `users.devices.list`, `users.media.list`, ...), which are still declared
  `admin:read`. Sessions' IP addresses stay redacted for `moderation:read` (RFC 0004 8.2).
- Object-level restriction (a moderator limited to some rooms) is still after v1.
