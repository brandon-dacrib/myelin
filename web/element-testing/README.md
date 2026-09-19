# Pointing Element Web at `hs serve`

Reproducible setup for track 16's "point a real Matrix client at this server" exercise. Everything
here is a test harness, not shipped product code.

## 2026-09-19 (later session): it works, directly, no proxy needed

The CORS gap that forced the previous session into a same-origin reverse-proxy workaround is now
fixed (`hs_http::cors::matrix_layer()`, applied in `crates/hs-cli/src/serve.rs`) — confirmed live:

```
curl -i -X OPTIONS http://127.0.0.1:8098/_matrix/client/v3/login \
  -H "Origin: http://localhost:8080" -H "Access-Control-Request-Method: POST" \
  -H "Access-Control-Request-Headers: content-type,authorization"
# -> 200, access-control-allow-origin: *, -allow-methods, -allow-headers all present
```

**Element Web now runs pointed straight at `hs serve` on its own port, no reverse proxy, no
same-origin trick.** `scripts/element-proxy.mjs` is kept (still useful if a future test needs a
single origin for some other reason, e.g. cookie-based auth), but is not part of the normal path
any more. Use `element-config-direct.json` (points `base_url` straight at the homeserver) instead
of `element-config.json` (the old proxy-origin config, kept for reference).

Also fixed since the previous session: `GET /capabilities` no longer lies about
`m.set_displayname`/`m.set_avatar_url`.

### One-time setup

```sh
cd web/element-testing

# 1. Build the server binary (from the repo root)
cargo build -p hs-cli --bin hs

# 2. Generate + patch config (already done for this session; regenerate with:)
../../target/debug/hs generate-config --server-name test.local -o config.yaml
# then hand-patch: port 8098, add `admin` to listener resources, enable_registration: true,
# registration_shared_secret: elementtestsecret, public_baseurl: http://127.0.0.1:8098,
# rate_limits.enabled: false (so ordinary interactive use doesn't trip the default rate limit
# while testing -- a test convenience, not a production recommendation).
```

### Every run

```sh
# 1. The homeserver
../../target/debug/hs serve -c config.yaml
# listens on 127.0.0.1:8098

# 2. Register test users (once per fresh ./data)
../../target/debug/hs register http://127.0.0.1:8098 -u ops -p opspassword123 -k elementtestsecret --admin -v
../../target/debug/hs register http://127.0.0.1:8098 -u alice -p alicepassword123 -k elementtestsecret -v
../../target/debug/hs register http://127.0.0.1:8098 -u bob -p bobpassword123 -k elementtestsecret -v

# 3. Element Web, pulled (not built)
docker run -d --name element-web-test -p 8080:80 \
  -v "$PWD/element-config-direct.json:/app/config.json:ro" \
  vectorim/element-web:latest

# (optional) a second instance on a different port/origin, so a second browser session
# (a genuinely separate localStorage, i.e. a second logged-in user) can be driven at the same
# time without Element's own "already open in another tab" lock kicking in:
docker run -d --name element-web-test-bob -p 8081:80 \
  -v "$PWD/element-config-direct.json:/app/config.json:ro" \
  vectorim/element-web:latest
```

Open **http://localhost:8080/** (and, for a second session, **http://localhost:8081/**) directly
— this is real Element Web talking straight to `hs serve` on `127.0.0.1:8098`, cross-origin, with
no workaround. Log in as `alice` / `alicepassword123` or `bob` / `bobpassword123` (or register a
new account — registration is enabled in this config).

### What was actually driven and verified, this session (Playwright, real Chromium, two separate
origins/sessions)

1. Landing page (`#/welcome`) loads correctly, no console errors, no CORS failures.
2. Sign in as `alice` via the real username/password form — real `POST .../login`, real session.
3. Room list, "Welcome" empty state, "New room" dialog.
4. **A real bug found here** — see "Bugs found" below: creating a room through Element's own "New
   room" dialog **fails with a 403** because Element's `createRoom` body includes
   `power_level_content_override`. Rooms created without it (via direct API calls, or without
   Element's default call-member power override) work fine and appear correctly in Element's own
   room list, so the rest of the scenario was driven against those.
5. Two fully independent browser sessions (alice on `:8080`, bob on `:8081`, separate origins so
   each gets its own `localStorage`/session) in the same room: invite, join, membership events,
   read receipts ("Seen by N person"), and **bidirectional real-time message delivery** — a
   message typed in one tab appears in the other via live `/sync` long-polling, no reload.
6. Display name change (Settings > Account): `PUT .../profile/{userId}/displayname` → `200`,
   updates immediately in the sending client's own UI, and propagates live to the other session's
   timeline and member list.
7. Scrollback: with 45+ messages in the room, scrolling the timeline to the top renders the room's
   very first event (`created and configured the room`), i.e. the full history renders correctly
   client-side; scrolling to the bottom jumps back to the latest message.

Screenshots: `docs/design/screenshots/element-0{1..11}-*.png`.

### `.well-known` client discovery, verified separately

`GET http://127.0.0.1:8098/.well-known/matrix/client` returns the correct document
(`{"m.homeserver":{"base_url":"http://127.0.0.1:8098"}}`, confirmed by direct curl) once
`server.public_baseurl` is set. This was not exercised through Element's own discovery flow (that
needs a real domain name and TLS to resolve `test.local` from the browser, which this local setup
does not have) — `default_server_config` is used instead, a normal thing for a self-hosted Element
deployment to do.

## Bugs found this session, diagnosed to a route and an owning track

**1. `POST /_matrix/client/v3/createRoom` rejects the room's own creator once a client supplies
`power_level_content_override`, because the server *replaces* the default power-levels content
with the override instead of merging it on top.** Real Element sends
`power_level_content_override` on every room it creates (by default, to set a power level for
`org.matrix.msc3401.call.member`), so **this blocks room creation from Element's UI entirely, for
every preset, every time** — the single most severe finding of this session, and worse than any
previous one because it isn't a missing feature, it breaks the primary flow.
- Minimal reproduction:
  ```
  curl -X POST http://127.0.0.1:8098/_matrix/client/v3/createRoom \
    -H "Authorization: Bearer $TOKEN" -H "Content-Type: application/json" \
    -d '{"preset":"private_chat","power_level_content_override":{"events":{"m.room.history_visibility":100}},"name":"x"}'
  # -> 403 {"errcode":"M_FORBIDDEN","error":"sender does not have enough power to send event of type m.room.join_rules"}
  ```
  `{"preset":"private_chat","name":"x"}` (no override) on the same server: `200`. Adding
  `"users":{"@creator:...":100}` explicitly to the override also works — confirming the creator's
  implicit power-100 grant is what's being dropped.
- Root cause, read not guessed: `crates/hs-room/src/actor.rs`, `RoomActor::create_room`, lines
  ~1320-1344:
  ```rust
  let power_levels_content =
      request.power_level_content_override.clone().unwrap_or_else(|| { /* builds users: {creator: 100} */ });
  ```
  This **replaces** the generated default power-levels content wholesale whenever the client
  supplies any override at all, rather than merging the override on top of the default as the
  Matrix spec requires (`createRoom`: "This object is applied on top of the generated power level
  event content prior to it being sent to the room"). Any override that omits `users` silently
  drops the creator's power-100 grant, so the very next bootstrap events this function sends
  itself (`m.room.join_rules`, `m.room.history_visibility`, `m.room.guest_access`, all sent by
  `creator` right after) are auth-rejected because the creator now has power 0 against
  `state_default` (50).
- Owning track: `hs-room` (track 04) — `crates/hs-room/src/actor.rs::create_room`.

**2. `GET /_matrix/client/v3/account/3pid` is unimplemented — plain `404`, no route at all, not a
homeserver `M_NOT_FOUND` body.** Element's Settings > Account page calls this on every load to
show linked email addresses/phone numbers; it renders "Unable to load email addresses" /
"...phone numbers" as a visible error banner in Settings today. Confirmed no handler exists
anywhere in the workspace (`grep -rn "account/3pid" crates/*/src` finds no route, only unrelated
config/translation-table hits).
  ```
  curl -i http://127.0.0.1:8098/_matrix/client/v3/account/3pid -H "Authorization: Bearer $TOKEN"
  # -> 404, content-length: 0
  ```
  Owning track: `hs-auth` (track 07) — brief explicitly lists "3PIDs, account lifecycle."

**3. State events never carry `unsigned.prev_content`, on `/sync` or `/messages`, anywhere.** Per
the Matrix spec, a state event's `unsigned.prev_content` should hold the previous content for that
`(type, state_key)`, so clients can tell "Alice changed her display name" apart from "Alice joined
the room" for two `m.room.member` events that otherwise look alike (same `membership: "join"`).
This server never populates it (confirmed: `grep -rln "prev_content" crates/*/src/` returns
nothing in the entire workspace), so Element's timeline literally renders a display-name change as
**"Alice Wonderland joined the room"** — visibly wrong, reproduced live (see
`element-testing`/screenshots, and the raw event below).
  ```
  curl ".../rooms/{roomId}/messages?dir=b&limit=10" -H "Authorization: Bearer $TOKEN"
  # the display-name-change m.room.member event's "unsigned" is "{}" -- no prev_content at all
  ```
  Root cause: `crates/hs-room/src/routes/render.rs::client_event_json` — the single shared
  function every route (`/messages`, `/sync`, `/context`, ...) uses to turn a stored `Event` into
  client JSON — builds `unsigned` from scratch (`obj.entry("unsigned").or_insert_with(|| json!({}))`)
  and never looks up or attaches the prior state content. This needs access to state just before
  the event, which `render.rs`'s pure function doesn't have today — a bigger fix than 1-2 lines,
  but well isolated.
  Owning track: `hs-room` (track 04) — `crates/hs-room/src/routes/render.rs`, and wherever the
  state-lookup this needs would live (`hs-state`/track 02 may also need to expose "prior content
  for this (type, state_key) as of this event" if it doesn't already).

### Not bugs — expected/benign 404s seen in the console

- `GET /_matrix/client/v3/room_keys/version` → 404: normal for any account with no key backup set
  up yet (every homeserver implementation 404s this until a backup exists).
- `GET /_matrix/client/v3/thirdparty/protocols` → 404: no bridges/appservices registered in this
  test setup; expected.
- `GET /_matrix/client/unstable/org.matrix.msc2965/auth_metadata` → 404: OIDC/MSC2965 discovery
  isn't implemented (legacy `m.login.password` is, and that's what this session used); Element
  falls back correctly and login still works.

## Known-broken without CORS (historical, now fixed)

Prior to the CORS fix, `curl -i -X OPTIONS .../login -H 'Origin: http://x' -H
'Access-Control-Request-Method: POST'` returned `405` with no `Access-Control-*` headers at all,
meaning no browser client on a different origin could make a single request. This is fixed and
verified live (see the top of this file). `scripts/element-proxy.mjs` was the workaround for that
gap; it is no longer necessary for the normal path but is left in place.
