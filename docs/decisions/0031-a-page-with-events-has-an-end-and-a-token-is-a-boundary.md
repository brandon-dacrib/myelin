# 0031: A page with events has an `end`, and a token is a boundary between events

Date: 2026-10-04. Track 04 (room and events), branch `agent/room-client-gaps`.

## Context

Sytest's `/messages` tests (`10apidoc/34room-messages.pl`) page back once from a sync's
`prev_batch`, expect an `end` on that page, page again from it and expect no `end`. Since
2026-09 `/messages` left `end` out of the last page that had events (the room's first event
reached), which is the spec's letter but not what Synapse does: Synapse leaves `end` out only
of an empty page at the end. Complement's `TestJumpToDateEndpoint` pages backwards from a
`/context` `end` and expects the context event itself in the page, which a token read as "an
exclusive position" in both directions cannot give.

## Decision

1. `GET /rooms/{roomId}/messages` gives an `end` on every page that has events, and leaves it
   out of the empty page after the last one, as Synapse does. The one exception is unchanged:
   a backward page that stopped at the oldest event held while the room's history goes on
   before it, and could not fetch more, has no `end`.
2. A `PaginationToken` is a boundary between two events. A backward token at `p` lies just
   before position `p`; a forward token at `p` just after it. Read in its own direction it is
   what it always was. Read in the other direction it is the same boundary: a forward token at
   `p` starts a backward page *at* `p`, a backward token at `p` a forward page at `p`. `/context`
   returns `start` as the backward token at its oldest event and `end` as the forward token at
   its newest (it returned empty strings before).
3. A token that is empty (`from=`) is no token, as Synapse reads it.
4. MSC2228's `org.matrix.self_destruct_after` is honoured by every read that renders an event
   for a client (`hs_room::routes::render`): once the time has passed the event is shown
   redacted. It is applied on read, not by a job, and needs no configuration. Synapse needs
   `enable_ephemeral_messages`; here the sender asked for it, and showing less is never a
   leak.
5. `GET /relations` defaults to five children per page (Synapse's default) and paginates with
   `next_batch`; it used to return every child at once.

## Consequences

- A client paging to the start of a room makes one more request than before, which comes back
  empty. Every client stops on a missing `end`, which is what that page has.
- `hs-user`'s `/sync` still leaves `prev_batch` out of a timeline that reaches the room's first
  event (`build_fresh_timeline`'s `next`). Clients then send `from=`, which now works; but
  `GET /members?at=` cannot be given such a token (Complement's `TestGetRoomMembersAtPoint`).
  The fix is `hs-user`'s: hand out `PaginationToken::new(oldest_pos, Backward)` when the page
  has events and `next` is `None`.
- The admin API's room content browser (`hs_room::admin::content`) still reads
  `RoomActor::paginate` directly and keeps its own last-page behaviour.
