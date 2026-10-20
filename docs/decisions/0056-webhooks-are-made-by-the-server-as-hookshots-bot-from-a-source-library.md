# 0056: Webhooks are made by the server as Hookshot's bot, from a source library

Date: 2026-10-20. Track 11 (appservices and bridges); touches tracks 15 (the admin API) and
16 (the interface). Amends 0054, whose point 5 read connections from room state and whose
consequences listed "adding or removing connections from the page (they are made in rooms by
design)" as not done.

## Context

The owner's direction for Hookshot: build it with the idea that many different webhooks come
into it. One Hookshot, one public URL, many senders (Datadog, GitHub, Linear, Notion, Gmail,
Alertmanager, scripts), each posting into its own room. Making each by hand in a room
(`!hookshot webhook <name>`, then writing a transformation function into the state event by
hand, as the demo's two GitHub rooms were made) does not scale, and the transformation
functions are the server's knowledge, not the administrator's.

Hookshot 7.5.0 has no provisioning API, but a generic webhook is nothing more than two
writes the bot makes (`GenericHookConnection.provisionConnection`): its room account data of
type `uk.half-shot.matrix-hookshot.generic.hook` mapping `{"<hook id>": "<name>"}` and the
state event of that type, state key = name, content `{name, transformationFunction}`. The
bridge picks a new state event up live (`Bridge.onRoomEvent` → `createConnectionForState`),
reads the hook id from the account data, and refuses to make one up for an event its own bot
sent. `validateState` reads the keys it knows and ignores the rest. The hook id is
`randomUUID()`.

## Decision

1. **A source library in the server**: `crates/hs-bridges/hookshot-sources/<id>/` with
   `README.md` (the sender's setup, `{{url}}` and `{{name}}` placeholders), `transform.js`
   (Hookshot's v2 contract: `data` in, global `result` out), `sample-*.json`, and the files
   the sender runs (`gmail/Code.gs`), embedded with `include_str!` into
   `hs_bridges::hookshot_sources::SOURCES`. Seven to start: `generic`, `alertmanager`,
   `datadog`, `github-comments` (the demo's, moved), `gmail`, `linear`, `notion`. Every
   transform is run on every sample with `node` in `cargo test` (skipped without `node`).
   A source is a directory and an entry, not a feature.
2. **The server makes a webhook as the bot**, masquerading through the shared instance's
   registration (the token and bot the manager already lists connections with):
   `POST /bridge-offerings/{type}/connections` `{source, name, room: {id} | {create: {name,
   topic?, invite}}}` → the room if asked (`private_chat`, the bot its creator), a v4 UUID
   hook id, the account data **first**, then the state event with the source's transform and
   `io.myelin.source: <id>`, then a notice in the room without the URL. `201` with
   `{room_id, name, source, url, setup, files}`: the URL's one showing. `bridges:write`,
   `Idempotency-Key` honoured, audited without the URL,
   `hs_bridge_webhook_connections_total{bridge_type,source,outcome}`.
3. **Any connection is removed the way the bridge removes one**: `DELETE
   /bridge-offerings/{type}/connections/{room_id}/{state_key}` empties the state event (the
   bridge purges on seeing it) and, for a webhook, drops the hook id from the account data.
   Not only webhooks: a repository's or a feed's event is emptied the same way, which is what
   `!hookshot ... remove` does.
4. **The listing says the source and the bot's rooms**: `BridgeConnection.source` from the
   marker; `BridgeConnectionList.rooms`, the rooms the bot is in, with or without
   connections, so a webhook can be added to any of them. The hook id is in neither, nor in
   the audit log: the secret lives in the bridge's account data only.
5. **The interface** adds a webhook from the offering's page (source cards, a room the bot is
   in or a new one with invites, a name), shows the URL once with a copy button and the
   sender's setup (and `Code.gs` for Gmail), and removes a listed connection after
   confirmation. The bot's commands in rooms keep working alongside.
6. **No signature verification is claimed.** Linear, Notion and GitHub sign deliveries;
   Hookshot's generic webhook checks none of it, and each source's README says so: the URL's
   last part is the only secret.

## Consequences

- An administrator adds a Datadog or Alertmanager or Linear webhook in a minute, from the
  page, and the room gets notices shaped for the sender without anyone writing JavaScript.
- The real-image story (`crates/hs-bridge-conformance/tests/real_hookshot.rs`) covers both
  directions: a webhook made in the room by the bot's command, and one made through the API
  from the `datadog` source, posted to with Datadog's sample, landing as the source's notice,
  listed with its source, and removed so that the URL answers an error.
- 0054's point 5 stands (read live from room state) and gains writes; its "not done" line
  about the page is done for webhooks. Repositories through the GitHub App and feeds are still
  connected in rooms (`!hookshot github repo`, `!hookshot feed`), listed and removable here.
- `deploy/demo/hookshot/github-comments.js` is a pointer to the source; the demo's two rooms
  were made by hand with the same text and are unchanged.
- OpenAPI 0.1.23 (additive).
