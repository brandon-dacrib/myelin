# 0057: Webhooks are self-service for every user, through a page on the server

Date: 2026-10-21. Track 11 (appservices and bridges); touches tracks 07 (the `Requester`
middleware is the user API's authentication), 15 (API conventions: a client-server extension
under an `io.myelin` unstable namespace, outside the admin OpenAPI) and 16 (a second page in
the web build). Builds on 0056, which made webhooks from the admin API as Hookshot's bot.

## Context

The owner's direction: "this server isn't just meant to be mine; the flow for adding webhooks
on a per user basis should be easy." After 0056 an administrator adds a webhook from the
offering's page in a minute, but a person on the server who wants GitHub comments or Datadog
alerts in a room of theirs has to ask one, or invite the bot and type `!hookshot webhook`
and get Hookshot's plain rendering with no transformation function.

The admin API cannot carry this: its verifier (`hs_auth::admin_verifier`) refuses a
non-administrator's token exactly like an unknown one, by design, and nothing under `/api/v1`
is for people who are not administrators. What a person has is their ordinary Matrix access
token, and what the server has is the `Requester` middleware that authenticates it.

## Decision

1. **A user API on the client listener**, `/_matrix/client/unstable/io.myelin.webhooks`
   (`hs_bridges::user_webhooks`), authenticated by the ordinary `Requester` (through a
   `CallerResolver` the binary provides), not under `/api/v1` and not in the admin OpenAPI:
   `GET /sources`, `GET /rooms` (the caller's joined rooms, each with `may_add` and the
   reason when not), `GET /webhooks` (the active webhook connections in rooms the caller is
   joined to, with `created_by`/`created_at` when known), `POST /webhooks` (`{source, name,
   room: {id} | {create: {name, topic?}}}` → `201` with the admin API's
   `BridgeConnectionCreated` shape, the URL's one showing) and `DELETE
   /webhooks/{room_id}/{name}` (`200 {}`). Errors are Matrix errors: `M_FORBIDDEN`,
   `M_NOT_FOUND` (no Hookshot offered, or none with a public URL, or no such webhook),
   `M_INVALID_PARAM`, `M_LIMIT_EXCEEDED`.
2. **The rules, read from the room's current state as the caller sees it** (`RoomFacts`,
   `verdict`, unit-tested): the caller is a joined member with the power to send the
   connection's state event type (`events[type]`, else `state_default`; room version 12's
   creators have infinite power); the room is not encrypted (Hookshot here runs without
   encryption and would post in the clear); if the bot is not in the room, the caller may
   invite (`invite` level) and the server invites it **as the caller** and the bot joins; if
   the bot is below the level the event takes, the caller must be able to change power levels
   and the server raises the bot to exactly that level, as the caller. For a new room the bot
   makes it (private), invites the caller, gives them power 100, and the server **joins the
   caller as themselves** so it is in their client at once (an invite would have left it in a
   list most clients hide). Then the manager makes the connection exactly as 0056 does, with
   `io.myelin.created_by` and `io.myelin.created_at` in the state event (Hookshot ignores keys
   it does not know), and the room's notice names the maker.
3. **The server acts as the caller with the caller's own token over loopback** for the steps
   that are theirs (reading their rooms, inviting the bot, raising its level, joining the new
   room), and as the bot with the instance's appservice token for the bot's (joining, making
   the room, the account data, the state event). Nothing is done as the user that the user did
   not ask for by calling the API.
4. **Audited, counted, logged, limited.** Every make and remove is an admin audit entry with
   the user as actor (`bridge_connection.created`/`.deleted`, actor kind `user`, never the
   URL); `hs_bridge_webhook_connections_total` gains a `via` label (`admin`/`user`); the
   server logs at INFO who made what where; creation is rate-limited per user by a small
   in-process token bucket (ten at once, one more every six minutes), answered with
   `M_LIMIT_EXCEEDED` and `retry_after_ms`.
5. **A page on the server**, `/integrations/`, from the same web build as `/admin/`
   (`web/integrations.html` → `web/src/integrations/`), signed in with the person's own
   account through the ordinary `/login`: "My webhooks", "Add a webhook" (source cards, one
   of my rooms where I may, or a new room, a name; the URL once with the sender's setup and
   `Code.gs` for Gmail), "Remove" after confirmation. Plain, phone-sized, explained inline. The
   admin offering's page links to it; the admin sign-in says "Not an administrator?" with the
   link (static text: the admin API's answer to a non-administrator's token is unchanged).
6. **Hookshot's own command stays.** `!hookshot webhook <name>` in a room with the bot still
   works and gives Hookshot's plain rendering; the page is how a person gets the library's
   formatting.

## Consequences

- A person on the server adds a Linear or GitHub or Datadog webhook to a room of their own
  in a minute, without an administrator, and the room gets the source's notices. The
  administrator sees every one in the offering's Connections table and the audit log, and
  `via="user"` in the metric.
- The real-image story (`crates/hs-bridge-conformance/tests/real_hookshot.rs`) has a third
  part: alice, no administrator, makes a webhook in a room of her own through the user API;
  the real Hookshot 7.5.0 picks the connection up live; Linear's sample lands formatted; the
  listing names her; removal stops the URL.
- `GET /rooms` reads each of the caller's rooms' state (up to 200 rooms) to say which may take
  a webhook; a page open, not a hot path.
- Signature verification (`Linear-Signature`, `X-Hub-Signature-256`) is still Hookshot's to
  do and it does not; a user's URL is as secret as an administrator's.
- Not done: rotating a URL in place; a user's view of connections made in the room by the
  bot's command (listed by the admin, not here: they carry no source and no maker); the
  guest and appservice-masquerade cases are refused or treated as the user, as `Requester`
  decides.
