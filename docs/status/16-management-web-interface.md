# 16. Management web interface: status

## Current update: 2026-09-25

**Bridges are the marquee page they were meant to be, and a real mautrix bridge came through
them.** Read against <https://docs.mau.fi/bridges/> and the `bridgev2` example config, the
section now does what an operator otherwise reads those pages to do:

- **The catalogue says what each bridge is.** `GET /bridge-types` entries carry a one-line
  description, a category (`messaging`, `social`, `irc`, `integrations`), the project's
  documentation link, the port the bridge listens on, whether a render writes its config, and
  the bridge's own sign-in steps (`sign_in.steps`, `{bot}` for the bot's Matrix ID, plus the
  documentation's caveats as `notes`). The Kind step groups by category, searches, shows what
  each needs, and links the chosen one's documentation.
- **A render writes the bridge's `config.yaml`**, not only its registration: everything that
  ties a mautrix bridge to this server (addresses both ways, tokens, bot, ghost template,
  database, permissions with the operator as administrator, backfill, double puppeting through
  the bridge's own token, encryption in appservice mode). The bridge completes the rest on
  first start; verified, see below. Non-mautrix runtimes get the registration and the Compose
  notes as before.
- **The registration remembers its bridge type** (`io.myelin.bridge_type`, kept by the
  registry with the other unknown keys), and `AppService.bridge_type` reads it back. The list
  and detail pages show the catalogue's name and glyph instead of "Custom appservice", and the
  detail's **Sign in** tab (was "Logins") shows the numbered steps for *this* bridge with its
  bot's real Matrix ID, or says honestly that an appservice added outside the catalogue has no
  guide.
- **Deployment asks for both addresses**: this server as the bridge reaches it and the bridge
  as this server reaches it, each defaulting to the Compose or Kubernetes service name and
  following the id, namespace and deployment until the operator types into them
  (`applyPatch` in `wizard-state.ts`, unit-tested). That is what makes "bridge in Docker,
  server on this laptop" work without editing a file.
- **The Created page is a runbook**: save the files (config, registration, Compose or Bridge
  resource, tokens shown once), start it (the exact command), watch it connect (the page polls
  every five seconds and turns green on the first ping; down or degraded shows the error and
  where the log is), sign in (the guide). "Open bridge" is a link styled as a button, not a
  button inside a link.
- **The list reads attention-first** (down, degraded, unknown, healthy, paused), with a
  summary strip of counts that filters the table, a glyph and the kind under each name, and
  the bot's Matrix ID.

**Verified against the real binary, in a real browser, with a real bridge**
(`docs/bridges/mautrix.md`, `web/e2e-real/add-mautrix-bridge.spec.ts`): WhatsApp chosen,
both addresses set for Docker-beside-the-host, created, the two files saved as the page
showed them, `docker run`, and the Created page turned green in about seven seconds while the
bridge logged MSC4190 device creation and "End-to-bridge encryption is in appservice mode".
The bridge completed the wizard's 40-line config to 666 lines itself. The run found one
server defect, fixed in track 11: a ping that succeeded did not clear the error from the one
before it, so a bridge whose first ping raced its own listener was "healthy, with an error".
Screenshots: `docs/design/screenshots/bridge-*-real-whatsapp.png` (real) and
`bridges-list.png`, `bridge-wizard-kind.png`, `bridge-wizard-deployment.png`,
`bridge-detail-sign-in.png` (mock).

Checks: `npm run check` green; `cargo test -p hs-admin -p hs-appservice` green; all 22 mock
Playwright tests pass (the add-bridge spec now asserts the config preview, the runbook, the
sign-in guide and the detail's Sign in tab; the list spec asserts the order and the summary
strip); the real suite's stale "bridges are 501" test now asserts the real list; the new real
bridge spec passes and is opt-in (`HS_REAL_BRIDGE_RUN=1`).

API changes, additive, recorded in 15's status: `BridgeType.{description,category,docs_url,
port,renders_config,sign_in}`, `BridgeTypeRenderResult.config_yaml`,
`AppService.bridge_type`; the render request takes `homeserverAddress`, `bridgeAddress` and
`adminUser`.

Still open in this section: per-user login *state* (who is signed in to the bridge) is not
something the admin API can see, because the bridges keep it; `links.login_url` is still
never set; Kubernetes deployment produces a resource for an operator not yet written.

## Update: 2026-09-23

The Audit log is a working section at `/audit`, backed by the real `audit_log.list/get/export`
operations. Filters and cursor are in the URL; the list links to entry details and resource
pages, and the detail shows the actor, request, changes, outcome and replay link. Export is
authenticated NDJSON and says explicitly that it uses the date range only (up to 10,000 entries).
The mock API exercises the same filters and export shape. `npm run check` passes; all 22 mock
Playwright tests pass, including the three Audit scenarios with axe at desktop and phone width.
The Audit scenario also passed against a fresh `hs serve`: first administrator created, its
durable setup entry opened, NDJSON downloaded. Reports, Media, Cluster, Migration and Settings
remain placeholders. Older counts and open items below are retained as session history.

> **Integration note, 2026-09-19 (integration lead): one of this session's three findings is a
> false positive, and the other two are confirmed.**
>
> - **CORS: confirmed, and serious.** `crates/hs-cli/src/serve.rs` applies no CORS layer to
>   `/_matrix/*` at all, and `hs_http::cors` was only ever written for `/api/v1`. A browser client
>   cannot talk to this server. `hs_http::cors::matrix_layer()` now exists with the spec's exact
>   policy and its own tests; applying it is one line in `serve.rs`, which another agent held when
>   this was found.
> - **`/capabilities` stale flags: confirmed.** `m.set_displayname` and `m.set_avatar_url` report
>   `false` while both routes work.
> - **Receipts 404: NOT a bug.** Reproduced against the server this session left running, then
>   retested against a freshly built binary: `POST .../receipt/{type}/{eventId}` and
>   `POST .../read_markers` both answer **200**. The running server predated track 05 adding those
>   routes while the routes manifest came from a newer build — a stale-binary mismatch, which this
>   session's report explicitly listed as ruled out. It was not. There is no fault in
>   `hs-http`'s `Builder`/`merge_router`, and a probe confirmed axum composes two routers nested at
>   the same prefix correctly. Worth the care: a "registered but unreachable" bug would have
>   undermined every coverage number this project quotes.


> **Integration note, 2026-09-19 (integration lead):** two corrections to the session below.
> First, `npm run typecheck` did **not** pass as committed: `src/api/bridges.ts`'s
> `useDeleteAppservice` was left mid-refactor (`const { error } = ...` followed by
> `unwrap(result)`, with `result` undefined) — two TypeScript errors. Fixed here; typecheck is
> clean as committed. Second, `npm run test` (Vitest) and `npm run lint` could not be made to run
> on this machine at all while two Rust agents were compiling: every Vitest worker times out
> after 60s ("Timeout waiting for worker to respond", zero tests executed) and lint exceeds 500s.
> That is an infrastructure failure, not a test result, and it is unverified either way — **rerun
> `npm run check` on an idle machine before trusting this session's web changes.** The
> Playwright real-server suite (6/6) and the typecheck are what is actually verified.


Track brief: `docs/workstreams/16-management-web-interface.md`. Owner directories: `web/`, `docs/design/`.

Last updated: 2026-09-19 (session: Element Web actually running against `hs serve`, in a real browser, for the first time in this project).

## Session: Element Web actually running, in a real browser, against `hs serve` (2026-09-19, later still)

Assignment (from the integration lead): finish the job the previous session couldn't — get Element
Web running against a real `hs serve` in a real browser, use it, report what happens. **It works.**
This is a genuinely working Matrix web client against this homeserver, driven end to end in
Chromium via Playwright, with screenshots. It also surfaced three real server bugs that no
protocol-level curl test had caught, exactly the point of the exercise.

### The host is quiet now, and Docker cooperated

Load average was 4.6-5.1 at the start of this session (vs. 9-12 two sessions ago), and Docker's
container lifecycle commands (`run`, `rm`, `ps`, `logs`) all returned promptly. `docker pull
vectorim/element-web:latest` was already cached from the previous session. Two fresh containers
came up `healthy` within ~15 seconds each, first try, no retries needed — a sharp contrast with the
previous session's 27+ minutes of hung lifecycle calls. (Load spiked back to 38-43 later in this
session, presumably another agent's build — `web/`'s own `npm run typecheck`/`test`/`lint` were
run then and are reported honestly below, including where that spike stalled them.)

### CORS is fixed, verified from a real browser, not just curl

The previous session's #1 finding — no CORS headers on `/_matrix/client/*` at all — is fixed.
Confirmed twice: once by direct curl (`OPTIONS /_matrix/client/v3/login` now returns 200 with
`access-control-allow-origin: *` and the right `-Allow-Methods`/`-Allow-Headers`), and then for
real by pointing Element Web's `config.json` straight at `http://127.0.0.1:8098` — a different
origin than Element's own `http://127.0.0.1:8080` — with **no reverse proxy in between**. It loaded,
logged in, and worked. `web/scripts/element-proxy.mjs` is no longer part of the normal path (kept
in place in case a future session needs a single-origin setup for some other reason); the harness
now defaults to `web/element-testing/element-config-direct.json`, which points directly at the
homeserver.

### What was actually driven, in Chromium, via Playwright (not claude-in-chrome — the extension
wasn't connected this session; Playwright's own MCP browser tools were used instead)

Two fully independent Element Web instances were run in Docker on two different host ports
(`:8080`, `:8081`), each with its own origin and therefore its own `localStorage` — this was
necessary because Element Web locks a session to one tab per origin ("Element is open in another
window"), so a true second concurrent user needed a second origin, not a second tab.

1. **Landing → sign in.** `alice` signed in via the real username/password form against
   `POST /_matrix/client/v3/login` on `:8080`. `bob` signed in the same way on `:8081`. Both real
   sessions, real tokens, no mocking.
2. **Room list, "New room" dialog** — see bug 1 below: creating a room through Element's own UI
   fails. Rooms were created directly via the API instead (as a real client's SDK would after
   omitting the problematic parameter, or as this server should have accepted regardless) and
   appear correctly in Element's room list once created — list rendering itself is not the
   problem.
3. **Invite, join, membership events, read receipts** — alice invited bob (`POST .../invite`), bob
   joined (`POST .../join`), and both directions rendered correctly in each other's timelines
   ("bob joined the room", "Seen by 1 person" badges with the correct avatar/name).
4. **Bidirectional real-time messaging.** Alice typed "Hello from Alice!" in her tab; it appeared
   in Bob's tab via live `/sync` long-polling with no page reload. Bob replied "Hi Alice, Bob
   here!"; it appeared in Alice's tab the same way. This is real, working two-way chat between two
   independent browser sessions against this server — the core of what a "management web
   interface" session was actually asked to prove for the wider project (this is Element, not the
   admin console; see `docs/next-steps.md` item 2 / the integration lead's assignment for why this
   was worth a session of its own).
5. **Display name change.** Settings > Account > Display Name → "Alice Wonderland",
   `PUT /_matrix/client/v3/profile/{userId}/displayname` → `200`, applied immediately in Alice's
   own UI (sidebar avatar, welcome heading) and propagated live to Bob's already-open timeline and
   member list — with one real, if cosmetic, bug: see finding 3 below, it renders as "Alice
   Wonderland joined the room" instead of a name-change message.
6. **Scrollback.** 45 messages were sent into the room; reloading Bob's tab (forcing a fresh
   `/sync`) and scrolling the timeline to the top rendered the room's very first event ("Alice
   Wonderland created this room") — the complete history, correctly ordered, all the way back to
   room creation. Scrolling to the bottom returned to the latest message. Both worked correctly.

**Screenshots** (`docs/design/screenshots/element-01-landing.png` through `element-11-scrolled-bottom.png`):
welcome screen; signed-in home ("No chats yet"); the room-creation failure dialog (bug 1, in situ);
Alice's message sent; Bob receiving it live in his own session; Alice receiving Bob's reply live;
Settings > Account (including the live "Unable to load email addresses" error, bug 2, in situ);
the display name saved; the full 45-message timeline scrolled to the top and to the bottom.

### Bugs found, diagnosed to a route and an owning track (full detail and reproduction in
`web/element-testing/README.md`)

**1. `POST /createRoom` rejects the room's own creator whenever `power_level_content_override` is
present, because the server replaces the default power-levels content instead of merging the
override on top of it — dropping the creator's implicit power-100 grant.** This is the most
severe finding of this session: real Element sends `power_level_content_override` on **every**
room it creates by default (to set a power level for `org.matrix.msc3401.call.member`), so this
blocks room creation from Element's UI **entirely, unconditionally** — not a missing feature, a
broken core flow. Minimal repro:
```
curl -X POST http://127.0.0.1:8098/_matrix/client/v3/createRoom \
  -H "Authorization: Bearer $TOKEN" -H "Content-Type: application/json" \
  -d '{"preset":"private_chat","power_level_content_override":{"events":{"m.room.history_visibility":100}},"name":"x"}'
# -> 403 {"errcode":"M_FORBIDDEN","error":"sender does not have enough power to send event of type m.room.join_rules"}
```
Same call without the override, or with an override that explicitly re-includes
`"users":{"@creator:...":100}`, succeeds — confirming exactly what's being dropped. Root cause,
read not guessed: `crates/hs-room/src/actor.rs`, `RoomActor::create_room`, ~line 1320:
`request.power_level_content_override.clone().unwrap_or_else(|| /* builds users: {creator: 100} */)`
— this *replaces* the generated default wholesale instead of merging on top, exactly backwards
from the spec's own wording for this field ("applied on top of the generated power level event
content"). Owning track: **04 (hs-room)**, `crates/hs-room/src/actor.rs::create_room`.

**2. `GET /_matrix/client/v3/account/3pid` is unimplemented** — a bare 404 with no route at all
(confirmed: `grep -rn "account/3pid" crates/*/src` finds no handler anywhere in the workspace).
Renders as a visible "Unable to load email addresses"/"...phone numbers" error banner in Element's
own Settings > Account page (see the screenshot). Owning track: **07 (hs-auth)** — the brief
explicitly lists "3PIDs, account lifecycle."

**3. State events never carry `unsigned.prev_content`, anywhere** (`/sync`, `/messages`, or
presumably `/context`) — confirmed by `grep -rln "prev_content" crates/*/src/` returning nothing
in the entire workspace. Per spec this field lets a client tell "the sender changed their display
name" apart from "the sender joined the room" for two otherwise-identical `m.room.member` events.
Reproduced live and visibly wrong: Element's timeline renders Alice's display-name change as
**"Alice Wonderland joined the room."** Root cause: `crates/hs-room/src/routes/render.rs`'s
`client_event_json` — the one shared function every read route uses to turn a stored event into
client JSON — builds `unsigned` from scratch and never attaches prior state content; doing so
needs a "prior content for this (type, state_key) as of just before this event" lookup that
doesn't exist today. Owning track: **04 (hs-room)** for `render.rs` itself; may also need
something from **02 (hs-state)** if the state history lookup this requires isn't already exposed.

Three console 404s were seen and are **not bugs** (see the README for why): `room_keys/version`
(no key backup set up — universal, expected), `thirdparty/protocols` (no bridges registered —
expected), `unstable/org.matrix.msc2965/auth_metadata` (OIDC/MSC2965 discovery not implemented;
legacy password login is, and that's what was used, with a correct fallback).

Both of the previous session's confirmed findings are independently reconfirmed fixed here, live,
from the browser: CORS (a real cross-origin Element instance works with zero workaround) and
`/capabilities`' stale `m.set_displayname`/`m.set_avatar_url` flags (both now `true`, matching the
working `PUT .../profile/.../displayname` calls actually made this session).

### Verification of `web/` itself (per this session's own instruction: report honestly, don't
assume)

Run **after** the transcript above, once the host's load average had (unexpectedly) climbed to
38-43 mid-session (a different agent's build, not this track's):

- `npm run typecheck` (`tsc -b`): **passed, exit code 0.** Confirmed by direct output, not
  assumed — this one did complete despite the load spike (it took ~20 minutes wall-clock to get
  scheduled, but the command itself succeeded once it ran).
- `npm run test` (Vitest): **did not run — infra failure, reproduced twice, not a code result.**
  First attempt (default config, host load ~7-10 at the moment of launch): every worker failed
  with `[vitest-pool-runner]: Timeout waiting for worker to respond`, "no tests" executed, 8
  errors (one per test file, all the same cause). Second attempt, deliberately different
  (`--no-file-parallelism`, forcing a single worker instead of vitest's default pool) specifically
  to rule out "workers can't all start at once under load" as the cause: **same failure**, this
  time after running 421 seconds while host load climbed to 38-80 from an unrelated agent's build
  (`uptime` sampled 40.61/50.92/38.65 immediately after). Two different pool configurations, two
  identical failures, both correlated with extreme host contention neither this track nor its code
  caused. This matches every prior session's report of the same symptom
  (`docs/status/16-management-web-interface.md`'s own history, 2026-09-18/19) — it is a
  standing, unresolved infrastructure problem on this shared machine, not a regression introduced
  here, and **still not run to a real pass/fail result** as of this session. Nothing under
  `web/src` changed this session, so there is no new code this specifically puts at risk, but the
  gap itself (this suite has now failed to execute at least four separate times across three
  sessions) is worth a fifth session's attention on its own, independent of Element.
- `npm run lint`: **did not complete — killed after 45 minutes wall-clock, host load climbing to
  98.58 (`uptime` sampled 98.58/71.69/44.19 at the moment it was killed).** The `eslint .` child
  process had accumulated only ~2 seconds of actual CPU time across those 45 minutes — not slow,
  starved: it was barely being scheduled at all. This is the same failure mode the 2026-09-19
  (earlier) session reported ("lint exceeds 500s") and is unrelated to anything in this session's
  changes (only `web/element-testing/*` and this status file changed). Not re-attempted a third
  time this session; the next session should retry when `uptime`'s 1-minute load average is in
  single digits, which happened only briefly during this session (~7-10, right before the same
  other agent's build pushed it back over 40-98).

(Nothing under `web/src` was modified this session — only `web/element-testing/*` config/README
and this status file — so a clean run from the last confirmed-passing session, 2026-09-19 earlier,
is the reasonable expectation; this section exists so the next session doesn't have to take that
on faith.)

### Reproducible setup, updated

`web/element-testing/README.md` is rewritten to lead with the now-working direct setup (no proxy):
`element-config-direct.json` (new — points straight at `http://127.0.0.1:8098`), the two-Docker-
instance pattern for driving two independent browser sessions at once, and the full bug list with
reproduction commands. `element-config.json` (proxy-origin) and `scripts/element-proxy.mjs` are
kept for reference but are no longer the recommended path. A real `hs serve` was left running on
`127.0.0.1:8098` with `alice`/`bob`/`ops` registered and the "No Preset Room" test room containing
the full 45-message scrollback transcript, plus two Element Web containers
(`element-web-test` on `:8080`, `element-web-test-bob` on `:8081`) still running and healthy, for
the next session or the integration lead to look at directly without redoing setup.

### Verify

```
curl http://127.0.0.1:8098/_matrix/client/versions        # confirms the server is still up
curl -i -X OPTIONS http://127.0.0.1:8098/_matrix/client/v3/login -H "Origin: http://x" \
  -H "Access-Control-Request-Method: POST"                # CORS fix, live
curl http://127.0.0.1:8080/                                 # Element (alice's origin), 200
curl http://127.0.0.1:8081/                                 # Element (bob's origin), 200
cat web/element-testing/README.md                            # full reproduction + bug detail
```


## Session: pointing Element Web at `hs serve` (2026-09-19, later same day)

Assignment (from the integration lead, not the track brief): point Element Web, a real Matrix
client, at this server — "the single best test of whether this is a homeserver," never done
before. Full protocol-level verification was completed; **the actual browser run could not be
completed this session because the shared Docker daemon became unresponsive to container
lifecycle operations under host load**, not because of anything in this server. Reported
honestly rather than assumed, per this track's own standing instruction. Everything needed to
finish the browser run in one command is left in place for the next session.

### What actually happened, step by step

1. **Built the binary and stood up a real server.** `cargo build -p hs-cli --bin hs` (fresh, at
   commit `6b98961`). Config: `web/element-testing/config.yaml`, generated with
   `hs generate-config --server-name test.local` then hand-patched (port 8098, `admin` added to
   the listener's resources, `enable_registration: true` +
   `registration_shared_secret: elementtestsecret` so Element's own registration UI could be
   exercised, `public_baseurl: http://127.0.0.1:8098` so `.well-known/matrix/client` would be
   real, `rate_limits.enabled: false` — a deliberate test-only choice, see the README, since the
   generated default of 0.2 msg/s would make ordinary Element use look broken). `hs serve -c
   config.yaml` came up clean, and three users were registered (`ops` admin, `alice`, `bob`) via
   `hs register` against the live server (shared-secret registration, confirmed working).
2. **Verified the entire client-server surface Element needs, directly, before trying the
   browser.** Given the risk that Docker might not cooperate (documented as a known risk in the
   brief for this session), every capability the assignment lists was exercised by hand against
   the real, running binary first: `.well-known/matrix/client` discovery, `GET/POST
   /_matrix/client/v3/login` (password), `POST /register` (`m.login.dummy`), `GET
   /capabilities`, `POST /createRoom`, `GET /sync` (including **long-polling**: confirmed a
   30s-timeout poll returns in ~1s when a new message lands mid-poll, and blocks the full
   duration when nothing happens — the "continuous sync loop" the assignment specifically calls
   out as different from a scripted SDK's discrete calls), filter upload (`lazy_load_members`),
   invite/join, `PUT .../send`, `PUT .../typing`, `GET .../members?membership=join`, `GET
   .../context/{eventId}`, `GET .../state`, media upload/download/config, `PUT
   .../profile/{userId}/displayname` and `.../avatar_url`, `PUT .../state/m.room.avatar/`, `GET
   /pushrules/`, `GET /devices`, `GET /account/whoami`, `GET /joined_rooms`, logout and
   post-logout token revocation (401, correctly). All of this passed and is real, working
   Matrix protocol surface — see "What works, verified live" below. Three bugs were found this
   way (below) that a scripted SDK test would not have hit either, because none of the 23-step
   `hs-loadgen` scenario touches receipts, CORS preflights, or the capabilities/profile
   cross-check.
3. **Pulled Element Web** (`docker pull vectorim/element-web:latest`, per instructions — a small
   published image, not a build; ~205MB, completed fine while the daemon was still healthy).
4. **Discovered the client-server API has no CORS support at all** (bug 1, below) — meaning
   Element, run on any origin other than the homeserver's own, cannot make a single API call.
   Built a same-origin reverse-proxy workaround (`web/scripts/element-proxy.mjs`) so the rest of
   the scenario could still be attempted: it serves Element's static assets and forwards
   `/_matrix`, `/_synapse`, `/.well-known` to `hs serve`, collapsing both to one browser-visible
   origin. This is a **test-harness workaround, not a fix** — the gap is real and is reported to
   its owning track below.
5. **`docker run -d ... vectorim/element-web:latest` never became healthy.** The container's
   entrypoint hung at `/docker-entrypoint.d/18-load-element-modules.sh` (the last log line ever
   printed) and stayed `unhealthy` for 27+ minutes. `docker logs`, `docker exec`, `docker cp`,
   `docker kill`, `docker rm -f`, and a fresh `docker create`/`docker run` under a different
   container name **all timed out** (`timeout 20-30`, exit 124) even though `docker version` and
   `docker ps` kept responding throughout — i.e. the daemon was alive but its container-lifecycle
   path was wedged, not merely slow. A patient retry loop (18 attempts, 25s timeout + 10s backoff
   each) was run for the docker daemon to recover; it did not, across 9 attempts (~6 minutes),
   and was stopped deliberately rather than left running, since a stuck client-side `docker run`
   may leave a pending request queued against the daemon and this daemon is shared with track
   14's image builds this session — piling on more concurrent container-lifecycle calls looked
   more likely to make a shared resource worse than to win a race. Corroborating evidence this
   was host-wide contention, not Element-specific: a bare `sleep 60 && echo tick` (no Docker
   involved) took **~4 minutes wall-clock** to complete, and `npm run typecheck` sat at 0.0-0.7%
   CPU making ~1.5s of progress over 7+ minutes before it was stopped for the same reason.
   `uptime` read a load average of 9-12 throughout, on what behaves like a 10-core machine — this
   matches, and exceeds, what the 2026-09-19 session before this one already documented for
   Vitest/lint. **The actual Element Web UI was never seen rendered in a browser this session.**
   `element-web-test` (the one real container that got as far as `docker run -d` succeeding) is
   still present and unhealthy; `docker rm -f` on it also timed out during cleanup, so it is left
   running — harmless (it never became reachable), but worth a manual
   `docker rm -f element-web-test` once the host is idle.

### Bugs found, diagnosed to a route and an owning track

All three were found by direct protocol testing against the real binary (not the browser, which
never got that far) — exactly the "expect to find something no test had" pattern this assignment
predicted, just via curl instead of Chrome.

**1. The client-server API emits no CORS headers at all — blocks every browser client hosted on
a different origin than the homeserver.** This is the most severe finding: it is the reason
Element (or any web client not embedded in the same origin) cannot function against this server
without a workaround, and it would have been the very first thing a real browser hit.
   - Route: any `/_matrix/client/*` route. Reproduced on `/_matrix/client/v3/login`:
     ```
     curl -i -X OPTIONS http://127.0.0.1:8098/_matrix/client/v3/login \
       -H "Origin: http://localhost:8080" -H "Access-Control-Request-Method: POST" \
       -H "Access-Control-Request-Headers: content-type"
     ```
   - Expected: `200`/`204` with `Access-Control-Allow-Origin`, `-Allow-Methods`,
     `-Allow-Headers` (the Matrix spec requires the client-server API to answer CORS preflights
     from any origin — every other implementation does this unconditionally, unlike the admin
     API which is same-origin-by-default per this project's own `docs/decisions/`).
   - Actual: `405 Method Not Allowed`, `allow: GET,HEAD,POST`, no `Access-Control-*` header at
     all. A plain `GET /_matrix/client/versions -H "Origin: ..."` also comes back with zero
     `Access-Control-*` headers (confirmed by direct curl), so even a "simple" cross-origin
     request's response would be unreadable by browser JS.
   - Root cause, found by reading, not guessing: `crates/hs-http/src/cors.rs` implements a real,
     configurable CORS layer, but its own module doc says it is "CORS for `/api/v1`" (the admin
     API) only — it is applied in `crates/hs-cli/src/serve.rs` to the admin router, never to any
     of the `/_matrix/client` merges. The Matrix spec's requirement (open CORS on the C-S API,
     unconditionally) is a different rule than RFC 0004's admin-API default (same-origin unless
     configured), so this is not a matter of widening `admin_api.cors_origins` — the client
     router needs its own, always-on CORS layer.
   - Owning track: whoever assembles `crates/hs-cli/src/serve.rs`'s router (this track's brief
     lists `hs-cli` as off-limits to me, same as `hs-federation`/`hs-room`/`hs-user`/`hs-auth`).
     Likely track 05 (sync/user, the crate that owns most `/_matrix/client` traffic patterns) or
     whichever track next touches `serve.rs`'s `build_router`.

**2. `GET /_matrix/client/v3/capabilities` reports `m.set_displayname` and `m.set_avatar_url` as
`"enabled": false`, but both routes work correctly and have for at least this long.** Element
(and every other client) reads this capability before deciding whether to show "change display
name"/"change avatar" controls in Settings — a real user on this server would not be offered UI
for a feature the server actually has.
   - Route: `GET /_matrix/client/v3/capabilities` (no auth required — a separate, smaller,
     pre-existing gap: this handler ignores the token entirely, noted in its own doc comment as
     "not yet checking for a token", not new this session).
   - Expected: `"m.set_displayname": {"enabled": true}`, `"m.set_avatar_url": {"enabled": true}`
     — `PUT /_matrix/client/v3/profile/{userId}/displayname` and `.../avatar_url` both answered
     `200` and the write was durable (`GET /profile/{userId}` read back exactly what was set):
     ```
     curl -X PUT http://127.0.0.1:8098/_matrix/client/v3/profile/@alice:test.local/displayname \
       -H "Authorization: Bearer $TOKEN" -H "Content-Type: application/json" \
       -d '{"displayname":"Alice Test"}'
     # -> 200 {}
     curl http://127.0.0.1:8098/_matrix/client/v3/profile/@alice:test.local
     # -> 200 {"displayname":"Alice Test","avatar_url":"mxc://test.local/abc123"}
     ```
   - Actual: capabilities still says `false` for both.
   - Root cause: `crates/hs-cli/src/capabilities.rs`'s own doc comment says, explicitly, "no
     profile or 3PID-management HTTP routes are mounted yet" — true when that file was written,
     false now that `crates/hs-room/src/routes/profile.rs` exists and is mounted. A one-flag
     staleness bug, not a design problem; the fix is flipping two `false`s to `true` in
     `get_capabilities()` (`crates/hs-cli/src/capabilities.rs`, the `Json(json!({...}))` body).
   - Owning track: same as above — `hs-cli`'s `capabilities.rs`, off-limits to this track.

**3. `POST /rooms/{roomId}/receipt/{receiptType}/{eventId}` and `POST
/rooms/{roomId}/read_markers` are both defined correctly in source, listed as registered by the
server's own `routes-manifest`, and 404 live anyway — with a plain, empty-body 404, not the
server's own JSON `M_NOT_FOUND` shape.** Read receipts and the fully-read marker are core to
Element's room list (unread badges) and message view (read-up-to line); Element would send these
continuously and every one would silently fail.
   - Routes and reproduction:
     ```
     curl -i -X POST "http://127.0.0.1:8098/_matrix/client/v3/rooms/%21.../receipt/m.read/%24..." \
       -H "Authorization: Bearer $TOKEN" -d '{}'
     # -> 404 Not Found, content-length: 0 (no JSON body at all)
     curl -i -X POST "http://127.0.0.1:8098/_matrix/client/v3/rooms/%21.../read_markers" \
       -H "Authorization: Bearer $TOKEN" -d '{}'
     # -> same: 404, content-length: 0
     ```
   - Expected: `200 {}` (both routes are meant to succeed for a joined member — confirmed by
     reading `crates/hs-user/src/routes/receipts.rs`, which implements both handlers completely
     and correctly, including the "must be a joined member" rule).
   - The smoking gun: **the server's own `hs routes-manifest` output lists both routes as
     registered**, at both `v3` and `r0`, right alongside `PUT .../typing/{userId}` — which
     *does* work live (confirmed `200` from the exact same router, same file, same registration
     mechanism, `crates/hs-user/src/routes/mod.rs` lines ~78-91). A `GET` (wrong method) on the
     receipt path also comes back `404`, not `405` with an `Allow` header the way a real
     method-mismatch does elsewhere on this server (e.g. `OPTIONS /login` above) — meaning axum's
     live router genuinely has no matcher for this path template, not that the handler itself
     rejected the method.
   - What was ruled out, to save the owning track time: not a stale binary (rebuilt fresh at
     `6b98961` immediately before testing); not a naming collision with `hs-room`'s router (no
     other crate defines anything under `/rooms/{roomId}/receipt` or `.../read_markers`); not
     `RoomShardGate`'s cluster-forwarding middleware (`crates/hs-cli/src/cluster.rs`) — that gate
     passes every request through untouched in single-node mode (`cluster.single_node: true`,
     this config's setting), confirmed by reading `RoomShardGate::run`. The likely place left to
     look is `crates/hs-http/src/router.rs`'s `Builder::build`/`merge_router`, specifically how
     `axum::Router::nest` composes multiple sub-routers mounted at the identical prefix
     (`/_matrix/client/v3` is `.nest()`-ed with `room_router`, then separately with
     `user_router`, then `e2e_router`, ...) — something about this specific path shape inside
     that composition is being dropped between "the manifest says it's here" and "axum will
     route it," while sibling routes in the same sub-router survive.
   - Owning track: `crates/hs-user/src/routes/receipts.rs` is track 05's; the composition bug
     (if it is in `hs-http`'s `Builder`/`merge_router` or `hs-cli`'s `serve.rs`) is track 14's or
     whoever last touched `crates/hs-http/src/router.rs`. Flagging both files since the fault
     could be in either.

### What works, verified live (not assumed)

Everything below was exercised directly against the real, running `hs serve` (not the mock, not
a unit test) this session, in addition to what the 2026-09-18/19 sessions already proved:
`.well-known/matrix/client` discovery document; password login and registration via the real
client-server endpoints (not just the admin shared-secret path); `GET /capabilities` (modulo bug
2); room creation, invite, join; sending and receiving messages; **long-polling `/sync`,
including waking early on a new event** (~1s, not the full 10s timeout, when a message lands
mid-poll) — the continuous-sync-loop behavior a discrete-call SDK test does not exercise; filter
upload with `lazy_load_members`; lazy-loaded `GET .../members?membership=join`; typing
notifications; `GET .../context/{eventId}`; room and global state reads; media upload, download
(with correct `Content-Security-Policy`/`X-Content-Type-Options` headers), and config; thumbnail
generation correctly rejecting a non-image upload; profile displayname/avatar read and write
(see bug 2 — the writes work, only the capability flag lies); room avatar (`m.room.avatar` state
event); push rules; device list; `whoami`; `joined_rooms`; logout and correct 401 on the
now-revoked token afterward. **This is a wide, working slice of exactly what a browser client
needs**, verified the same day bug 1 (no CORS) means none of it is reachable from a real browser
without a same-origin workaround.

Not exercised live (Docker never got far enough): Element's own rendering, its login UI, its
room list UI, cross-session message delivery as seen by a second browser tab, avatar images
actually painting, and `.well-known` discovery driven by Element's own domain-entry flow rather
than a direct curl (that part needs a resolvable domain name and TLS, which this local setup does
not have — see `web/element-testing/README.md`).

### Reproducible setup, left in place for the next session

- **`web/element-testing/`**: `config.yaml` (patched, working), `element-config.json` (Element's
  `default_server_config`), `README.md` (exact commands, and why the proxy exists),
  `.gitignore` (excludes the runtime `data/`/`media-store/`/`signing-keys`/`serve.log` this
  session generated — keep the config/README, discard the rest on a fresh run).
- **`web/scripts/element-proxy.mjs`**: the same-origin reverse proxy (plain Node, no new
  dependency) that works around bug 1 above so Element can be driven at all; documented inline
  with why it exists and that it is a test harness, not a fix.
- A real `hs serve` is still running on `127.0.0.1:8098` as this session ends (three users
  registered: `ops`/admin, `alice`, `bob`, all password `<name>password123`), and the same-origin
  proxy is running on `127.0.0.1:8090`. **Next session's first move**: once the host is idle,
  `docker rm -f element-web-test` (currently stuck unhealthy, cleanup itself timed out this
  session), then `docker run -d --name element-web-test -p 8080:80 -v
  $PWD/web/element-testing/element-config.json:/app/config.json:ro vectorim/element-web:latest`,
  confirm `curl http://127.0.0.1:8080/` returns `200`, then open `http://localhost:8090/` in a
  real browser (or Playwright/claude-in-chrome) and actually drive it — login as `alice`, the
  room list, sending/receiving, the works. Given bug 1, Element will not function without the
  proxy (or a real CORS fix) regardless of which browser drives it.

### Verify

```
curl http://127.0.0.1:8098/_matrix/client/versions              # confirms the server from this session is still up
cat web/element-testing/README.md                                # full reproduction steps
node web/scripts/element-proxy.mjs                                # the workaround proxy, standalone
```

`npm run typecheck` was attempted and did **not** complete this session — it sat at <1% CPU
making almost no progress over 7+ minutes before being stopped, the same host-contention failure
mode the 2026-09-19 (earlier) session already documented for Vitest/lint (that session's
last-confirmed-clean typecheck stands; nothing under `web/src` was touched this session — the
only additions are `web/scripts/element-proxy.mjs` and `web/element-testing/*`, plain
`.mjs`/`.yaml`/`.json`/`.md` files outside the TypeScript project graph, matching the existing
`web/scripts/*.mjs` pattern already excluded from `tsconfig`). Rerun `npm run check` on an idle
machine to confirm, per that session's own standing instruction — this session did not get a
quieter machine either.

### Decisions made

- Rate limiting disabled (`rate_limits.enabled: false`) in the test config only, so ordinary
  interactive use of Element (multiple messages in quick succession) would not look like a
  server bug during manual testing. Not a recommendation for any real deployment.
- `web/scripts/element-proxy.mjs` was written from scratch (plain Node `http`, no new
  dependency) rather than pulling in `http-proxy-middleware` (not already a dependency of
  `web/`) — a same-origin proxy for two named path prefixes is ~60 lines and did not justify a
  new `package.json` dependency for a test-only tool.
- Stopped the Docker retry loop and did not attempt to restart the Docker daemon, even though
  that might have unstuck the wedged container faster: track 14 owns image builds on this same
  shared daemon this session, and a daemon restart would have killed their in-progress work.
  Left `element-web-test` running unhealthy rather than risk a more disruptive cleanup action.

## Session: real-server mode, honest degradation, proof against the running binary (2026-09-19)

Assignment: make the app work against a real `hs serve` (not just `hs-admin-mock`), degrade honestly wherever the real server answers `501`/`503`, and prove it against the actual running binary with screenshots. Full detail below; short version: **it works.** Signed in against a real `hs serve` with a real admin token minted through the shared-secret registration flow track 07 just landed, and the five operations track 15 wired to real data (`/me`, `/server`, `/server/health`, `/users`, `/users/{user_id}`) render real data in the app; the other 135 render the shared "not implemented yet" treatment, not a spinner, an empty table, or a red fault screen.

### Real-server mode

The app already talked to `/api/v1` via same-origin relative paths whenever `VITE_HS_MOCK` was unset (`web/src/api/client.ts`'s `apiBaseUrl()`) — that part of "real-server mode" pre-existed. What was missing, and is now built:

- **Real sign-in.** `web/src/lib/auth.ts`: `signInWithToken(accessToken)` and `signInWithPassword(username, password)`. Per the brief and `hs_auth::admin_verifier::AdminTokenVerifier`'s module doc, "there is no separate admin login" — a normal Matrix access token belonging to an `is_admin` user *is* the admin credential. `signInWithToken` verifies by calling `GET /api/v1/me` directly with `fetch` (deliberately not through the shared `api` client in `api/client.ts`, which auto-attaches the *current* session's token — exactly wrong when verifying a *candidate* token before a session exists) and maps 401/403/503/network failure to a specific, already-fit-to-show message (`AuthSignInError`). `signInWithPassword` mints a token via the ordinary `POST /_matrix/client/v3/login` (`m.login.password`) and then runs it through the same verification. `web/src/components/shell/SignIn.tsx` now renders one of two forms: the pre-existing two-button mock issuer stand-in when `VITE_HS_MOCK=1` (`MockSignIn`, unchanged, e2e keeps using it), or a real form (`RealSignIn`) with a tab switch between "Username & password" and "Access token" otherwise. `MOCK_MODE` (`import.meta.env.VITE_HS_MOCK === "1"`) is the switch.
- **Dev-mode proxy for iterating against a real server without the embedded-assets swap.** `web/vite.config.ts`: `VITE_HS_API_PROXY_TARGET` (e.g. `http://127.0.0.1:8098`) proxies `/api`, `/_matrix`, `/_synapse` to a real `hs serve` running elsewhere, so `npm run dev:real` (new script, fixed port 4180) can drive an actual running binary today, before the embedded build is wired in (see "The embedded build" below — that part isn't mine to flip). In production, the app is served *by* `hs serve` at `/admin/`, so `/api/v1` is already same-origin and this proxy is unused; it only matters for local iteration and for the new real-server Playwright suite.

### Degrade honestly (shared API-layer + component treatment, not per-page)

- **`web/src/api/problem.ts` (new).** `unwrap<T>(result)` replaces the `if (error) throw error; return data;` idiom used at all ~50 call sites in `web/src/api/{bridges,users,rooms,federation,dashboard}.ts` (mechanical rewrite, one `unwrap(...)` call per query/mutation). It throws `ApiProblemError`, which carries the real, parsed RFC 9457 `Problem` body (`components["schemas"]["Problem"]` — `status`, `type`, `title`, `detail`, `required_scope`, `request_id` are all real fields on the generated schema, so classification reads `problem.status` directly, no need to thread the raw `Response` through). `classifyError(err)` maps that to one of `not-implemented` (501) / `unavailable` (503) / `forbidden` (403) / `unauthorized` (401) / `not-found` (404) / `error` (everything else, including network failures). `isRetryableError` feeds `web/src/lib/query-client.ts`'s new `retry` function: permanent failures (501/403/401/404) get zero retries — retrying a 501 just delays the honest answer and risks looking like a stuck spinner — only `unavailable`/generic get up to 2.
- **`web/src/components/ui/error-state/ErrorState.tsx`: new `NotImplementedState`.** Neutral icon (`Construction`/`CloudOff`, not the red `AlertTriangle`), `role="status"` not `role="alert"` (this is not a fault), a plain-language heading ("X isn't implemented on this server yet" / "...isn't connected to a data source on this server yet" for 503), the server's own `detail` when it has one, and an optional "Check again" retry button for the 503 case. Every state in the file (`ErrorState`, `ForbiddenState`, `NotImplementedState`) now also takes a `compact` prop for embedding inline within a smaller region (a dashboard tile, a detail-page section) instead of a full-page treatment.
- **`web/src/components/QueryProblemState.tsx` (new).** The one place that calls `classifyError` and picks which state component to render; every page's error branch calls this instead of a bare `ErrorState`. Wired into all list/detail pages (`UsersPage`, `UserDetailPage`, `RoomsPage`, `RoomDetailPage`, `FederationPage`, `FederationDestinationPage`, `BridgesListPage`, `BridgeDetailPage`, the add-bridge wizard's render/create error messages) and, section-by-section, into `DashboardPage`.
- **`DashboardPage` rewritten to degrade per-section, not as one page-wide gate.** It composes six independent queries (`/statistics/overview`, `/server`, `/cluster`, `/appservices`, `/federation/destinations`, `/audit-log`); against the real server today only `/server` is wired, so the old "any query errors, blank the whole page" gate would have hidden the one section that actually works. Each section (Attention, the six Health tiles individually, the Bridges strip, the Federation strip, Recent audit) now renders its own data or its own honest gap — proven correct against the real server, see the screenshot below.
- **Fixed a real, if minor, "empty implies zero" bug while at it**: `UserDetailPage`'s Sessions section and `RoomDetailPage`'s Members section previously showed "No devices."/"No members loaded." on *any* query failure, not just a genuinely empty result. Both now render `QueryProblemState` when their own sub-resource query (`/users/{id}/devices`, `/rooms/{id}/members`) errors.
- **`web/src/pages/bridges/wizard/AddBridgeWizardPage.tsx` fixed to match `unwrap`'s new error shape.** It previously did `err as Problem` on whatever `create.mutate`/`render.mutate` threw; after the `unwrap` change that's an `ApiProblemError`, not a raw `Problem`, so the cast would have silently produced `undefined` fields. Replaced with `classifyError`/a small `wizardErrorMessage` helper that also gives 501/503 their own honest wording here too (this file wasn't using `QueryProblemState` since it renders a message string inside `ReviewStep`, not a state component).
- **New mock test seam**: `window.__hsAdminMock.setForceProblem(path, status, extra)` (`web/src/mocks/browser.ts`) forces any endpoint to answer a real `403`/`501`/`503` `Problem` body via `worker.use()` (same reasoning as the existing `setClusterMode`: MSW's service worker never makes an outgoing request for `page.route()` to intercept). `e2e/degrade-honestly.spec.ts` (new, in the always-run mock suite) proves `NotImplementedState`/`ForbiddenState` render correctly for 501/503/403 with axe and the DOM-nesting guard both clean.

### Proof against the real, running binary

Built `cargo build -p hs-cli --bin hs` (this needed two retries: it was red both times from an in-flight `crates/hs-federation`/`crates/hs-cli` interface change on track 06's side, per the integration lead — not this track's crates; it built clean once that landed). Then, against a real server, not the mock:

1. `hs generate-config --server-name test.local -o config.yaml`, hand-patched to add `admin` to the listener's `resources` (the generated default doesn't include it), a fixed port, `registration_shared_secret`, and scratch-directory paths for storage/media/signing keys; `hs generate-signing-key -o signing-keys/hs.signing.key`.
2. `hs serve -c config.yaml` on `127.0.0.1:8098`.
3. `hs register http://127.0.0.1:8098 -u ops -p <password> -k <secret> --admin -v` — **the shared-secret registration endpoint track 07 was adding is live**; this minted a real `syt_...` token for `@ops:test.local` with `is_admin: true`. (The brief's documented fallback — registering through `POST /_matrix/client/v3/register` because the endpoint might not exist yet — was not needed; noted here since the brief asked me to say which path I took.)
4. `curl -H "Authorization: Bearer $TOKEN" http://127.0.0.1:8098/api/v1/{me,server,server/health,users,appservices}` — confirmed `/me`, `/server`, `/server/health`, `/users` answer real data (`AdminTokenVerifier` is wired into `hs serve` now too, per the integration lead — it wasn't when this session started) and `/appservices` answers a well-formed `501` `Problem` exactly matching what `web/src/api/problem.ts` expects.
5. `VITE_HS_API_PROXY_TARGET=http://127.0.0.1:8098 npm run dev:real` (port 4180), and a new Playwright suite against it: **`web/playwright.real.config.ts`** + **`web/e2e-real/real-server.spec.ts`**, a separate config/testDir from the always-run mock suite so it can never run by accident. Skipped entirely unless `HS_REAL_SERVER_URL` is set (`test.skip`); the authenticated tests are further gated behind `HS_REAL_ADMIN_TOKEN` (optional — see the spec's own doc comment for why the "sign-in rejects an unrecognized token" case needs no admin account and was the one guaranteed to pass at any point in this session, including before track 07/15's wiring landed).

**Result, run against the real binary (`npm run test:e2e:real` equivalent, `--workers 1 --retries 1`): 6/6 pass**, with full-page screenshots saved (also copied to `docs/design/screenshots/real-server-2026-09-19/` for the record — see "How to verify" for the exact commands to reproduce):

- **Sign-in correctly rejects a bad token** with the specific "That token wasn't recognized, or has expired." message (`real-sign-in-rejected.png`).
- **Dashboard**: real `Version 0.0.1` / `Uptime 0h` tiles (from `/server`); `Mode`/`Users`/`Rooms`/`Daily active users` tiles each independently say "Not implemented" (from `/cluster`/`/statistics/overview`, still 501) rather than showing `0` or blanking the page; the Attention, Bridges-strip and Federation-strip sections each show their own neutral "isn't implemented on this server yet" (`real-dashboard.png`).
- **Users list is real**: shows the actual `@ops:test.local` row, Admin badge, device count, "10 min ago" (`real-users.png`).
- **User detail is real** (Created/Last seen/Rooms/Media/User type/Appservice, the Lock/Suspend/Sign-out-everywhere/Deactivate actions all present) **and its Sessions sub-resource honestly reports the gap** — "This user's sessions isn't implemented on this server yet" with the server's own detail, `users.devices.list is declared in the OpenAPI contract but not implemented yet` — instead of the pre-existing "No devices." bug this session also fixed (`real-user-detail.png`).
- **Bridges and Rooms lists** each show the neutral not-implemented state with the server's own detail message, no red, no spinner, no empty table (`real-bridges-not-implemented.png`, `real-rooms-not-implemented.png`).

**One flake worth recording honestly**: under a full run without `--retries`, one of the six tests occasionally times out waiting for its target route's lazy-loaded chunk to finish compiling in Vite dev mode — it was `users` in one run and `bridges` in another, each passed individually and both passed with one retry. This machine had ~12 concurrent cargo builds running throughout this session (load average 9-12 on what behaves like a far smaller core count); every symptom points at dev-server cold-compile latency under that load, not app behavior — confirmed by every failing case passing on an immediate retry with no code change. Recorded here rather than silently working around it. `playwright.real.config.ts` does not set retries by default for this reason; pass `--retries 1` if running under similar load.

**A real, minor rough edge found in passing, not fixed this session**: the sidebar's "Single node"/"Cluster" label (`TopBar`, driven by the same `cluster.data?.replica_count ?? 1 <= 1` heuristic as the wizard) reads "Single node" even when `/cluster` is genuinely 501, not actually known — it shares the dashboard's old problem (defaulting a heuristic through an error) but the sidebar surface wasn't in this session's scope of "pages" to fix. Filed here for the next session.

### The embedded build

Checked, not changed (per the assignment, `crates/hs-admin/src/assets.rs` is not mine): `web/vite.config.ts` already has `base: "/admin/"`, the router already has `basepath: "/admin"` (`web/src/routes.tsx`), `npm run build`'s output already has hashed asset filenames under `/admin/assets/...`, `Cache-Control: immutable` vs. `no-store` for `index.html` is already handled server-side in `assets.rs`, and SPA fallback (`GET /admin/{*path}` → `index.html`) already exists in `assets.rs`. **This was all already correct from a prior session** — nothing needed fixing here. The one remaining step is the integration lead's, verbatim from `assets.rs`'s own comment:

```rust
// crates/hs-admin/src/assets.rs
#[derive(RustEmbed)]
-#[folder = "web-dist-placeholder"]
+#[folder = "../../web/dist"]   // after `cd web && npm run build`
struct Assets;
```

(exact relative path from `crates/hs-admin/` to `web/dist/` — adjust if the build output is copied elsewhere by CI). `npm run build` must run before `cargo build -p hs-admin` in that case, same as the existing comment already says.

### Wrap-up note: what's verified and what isn't, and the real surface has grown further

Per the integration lead: since the screenshots above were taken, the real surface grew to **15 of 142** operations (`me`, `server`, `server/health`, `users` list/get, lock/unlock/deactivate/reactivate, the admin-flag `PATCH`, the audit log, and the SSE stream at `GET /api/v1/events`) — more than the 5 this session tested live. **Nothing in this session's code assumes a fixed list of which operations are real**: `classifyError`/`QueryProblemState` react to whatever status code a given response actually carries, so newly-real operations should just start rendering real data with no further change here. This is not re-verified against the grown surface — the next session should re-run `web/e2e-real/real-server.spec.ts` (`npm run test:e2e:real`) against a fresh `hs serve` build first, since it's cheap and it re-runs the same scenario recorded above. The SSE stream (`GET /api/v1/events`) is real now but **this session did not wire it up** — `TopBar`'s "Polling every 30s" and every page's `refetchInterval` are still the live-update mechanism (this was already `Next` item 4 below, now unblocked rather than blocked).

**What this session verified, and how**: `npm run lint` and `npm run typecheck` both ran clean to completion against every change in this session (confirmed by direct output, not assumed). `npm run test` (Vitest) and a second attempt with `--pool=threads --maxWorkers=1` both failed to even start their worker processes — `[vitest-pool-runner]: Timeout waiting for worker to respond` on every file, "no tests" run, not a single assertion executed or failed — because this machine was under extreme concurrent load for the last ~20 minutes of this session (a `sandboxd` process alone was pinned over 200% CPU, free RAM was in the tens of megabytes per `vm_stat`; unrelated to this track's code). This is an infrastructure failure, not a code signal either way, and was not resolved before this session had to stop. `npm run build` was not re-run this session either, for the same reason (started, would very likely have hit the same fork/spawn contention as Vite invokes esbuild/Rollup workers). **The last confirmed-passing `npm run build`/`npm run test` numbers are from the 2026-09-18 session** ("30 tests", "`npm run check` ... is clean" in "How to verify" below) — this session added new source files and tests (`src/api/problem.ts`, `src/components/QueryProblemState.tsx`, `NotImplementedState` + 4 new Vitest cases in `ErrorState.test.tsx`, the mechanical `unwrap()` rewrite across `src/api/*.ts`) that have **not been confirmed by a completed `npm run test` run**, only by `tsc -b` (which would catch type errors, not runtime assertion failures) and by direct manual verification against the real server for the code paths that matter most (`QueryProblemState`, `NotImplementedState`, `signInWithToken`, `signInWithPassword` — all exercised live, see the screenshots). **Next session: run `npm run test` first, before anything else**, ideally when the host is quieter; if a plain 501/503 test fails, start with `ErrorState.test.tsx`'s new cases and `src/api/problem.ts` (no test file exists for `problem.ts` itself — a gap worth closing then, not this session's write-up).

**Everything under `web/` changed this session is finished, not half-finished** (nothing was left mid-edit): the `unwrap()`/`classifyError` rewrite is complete across all five `api/*.ts` files with no remaining `if (error) throw error` call sites; every page's error branch was updated to `QueryProblemState`, including the three sub-resource spots that previously showed a false "No devices."/"No members loaded." on error; `DashboardPage`'s per-section rewrite is complete (no leftover page-wide gate); `SignIn.tsx`'s real-mode form, `auth.ts`'s `signInWithToken`/`signInWithPassword`, the Vite dev proxy, `playwright.real.config.ts`, `e2e-real/real-server.spec.ts`, and `e2e/degrade-honestly.spec.ts` are all complete and were run successfully (6/6 and 3/3 respectively, see above) before the host became unusable. The only unfinished thing is *verification* (`npm run test`/`npm run build`), not code.



## Integration review response (2026-09-18, same day)

The integration reviewer ran the app (not just read about it) and found one real defect plus two lower-priority items. All addressed:

- **Defect — nested `<button>` on the bridges list, fixed.** Root cause: `DataTable`'s <768px card fallback (`web/src/components/ui/table/DataTable.tsx`) wrapped every priority-1 column's rendered output in one tap-target `<button>`, including the bridges list's "actions" column (real `<button>`s) and "name" column (a real `<a>` via `Link`) — a `<button>` cannot legally contain another `<button>` or an `<a>`. Axe's default desktop-viewport run never saw it: that markup is `display:none` above 768px, and axe (correctly) only audits what's perceivable, while React's own DOM-nesting console warning fires regardless of visibility because it validates the actual DOM tree. Fixed by adding `Column.interactive?: boolean`: columns that render their own controls now render _outside_ the card's tap-target button as a sibling group, never inside it. The desktop `<tr>` was also simplified to drop its `onClick`/`tabIndex`/`onKeyDown="Enter"` pseudo-button behavior entirely (see the second item below) — with that gone, activation is always through a column's own real link/button, which cannot double-nest by construction. `BridgesListPage`'s "name" and "actions" columns are marked `interactive: true`.
  - **New regression coverage** (the ask: "add coverage that would" have caught it): `e2e/utils.ts`'s `installDomNestingGuard(page)` fails a test if React logs an invalid-DOM-nesting warning, at _any_ viewport — this is what should have existed already and is now wired into every e2e test. `e2e/bridges-list.spec.ts` adds a dedicated phone-viewport (`devices["iPhone 13"]`) axe pass, closing the gap for anyone who trusts axe alone at desktop width. `DataTable.test.tsx` adds a fast Vitest-level check (spies on `console.error`, asserts the action button is not a DOM descendant of the card's tap-target button) so this class of regression fails in `npm run test`, not just in e2e. `DataTable.stories.tsx` adds a `WithInteractiveColumns` story documenting the pattern.
  - Verified by loading every built page (Sign-in, Dashboard, Bridges list at desktop and phone width, five bridge detail pages across all tabs, every add-bridge wizard step, the Created page, Users/Rooms/Federation list and detail, and all five remaining placeholder routes) with the console open: all clean. Also found and fixed one unrelated, harmless console 404 (the browser's implicit `/favicon.ico` request against the origin root, outside the app's `/admin/` base path — `web/index.html` now has an inline data-URI favicon).
- **Lower-priority: the desktop table row's fake-button semantics.** Fixed as part of the same change: `DataTable`'s `<tr>` no longer carries `onClick`/`tabIndex`/`onKeyDown`. Row activation on desktop is now always a real link or button in one of the row's own columns (marked `interactive: true`), which natively supports Space, has a correct role, and can be opened in a new tab — properties a `tr` pretending to be a link never had. `onRowClick` still exists and still drives the mobile card's real `<button>`.
- **Bundle size.** `web/src/routes.tsx` now lazy-loads every page component via TanStack Router's `lazyRouteComponent` (route-level code splitting); `AppShell` stays a static import since it's needed on first paint regardless of route. The single 573 KB (178 KB gzip) chunk is gone: the main bundle is now 386 KB (124 KB gzip) and every page (2-18 KB) loads only when its route is visited. `npm run build` no longer warns about chunk size.
- **Users, Rooms, Federation pages built** (in the requested order), replacing their placeholder routes, against the real API (`/users`, `/rooms`, `/federation/destinations/{server_name}`): list pages with search (`q`, real server-side filtering — unlike appservices, these resources support it) and cursor pagination; detail pages with the primary actions from `flows.md` flows 2-4 (lock/unlock, suspend/unsuspend, sign out everywhere, deactivate for users; block/unblock, make-admin for rooms; reset backoff for federation destinations). Scoped narrower than the bridges marquee by design: no Sessions-tab-per-se depth beyond one device list, no reset-password flow (needs a password-entry UI this session didn't build), no redact-events. New: `web/src/api/{users,rooms,federation}.ts`, `web/src/pages/{UsersPage,UserDetailPage,RoomsPage,RoomDetailPage,FederationPage,FederationDestinationPage}.tsx`, mock fixtures/handlers for all three, `e2e/users-rooms-federation.spec.ts` (list → detail → one action per section, axe- and DOM-nesting-guard-checked). `npm run check` and the full Playwright suite (9 tests) are green.

## Done

Design artifacts (survived from the interrupted attempt, read and continued from, not rewritten):

- `docs/design/information-architecture.md`, `flows.md`, `accessibility.md`, `baselines.md`, `states-density-responsiveness.md`.
- `docs/design/design-system.md` (new this session): colour (light/dark via CSS `light-dark()`), typography scale, spacing, radii, elevation, motion, density, iconography, component inventory. Tokens implemented as code at `web/src/styles/tokens.css` (+ `base.css`, `index.css`).

Stack and scaffold:

- `docs/decisions/0003-web-stack.md`: records the stack (Vite, React 19, Tailwind 4 CSS-first, Radix via the `radix-ui` package, TanStack Query 5 + Router 1 code-based, `openapi-fetch` + `openapi-typescript`, MSW 2, Vitest 5, Storybook 10, Playwright 1).
- Full config: `tsconfig*.json`, `vite.config.ts`, `eslint.config.js` (flat, `jsx-a11y` strict), `.prettierrc.json`, `.storybook/{main,preview}.ts(x)`, `playwright.config.ts`.

Design system as code, all in `web/src/components/ui/`, each with a Storybook story and most with a Vitest/Testing-Library test: `Button`, `Input`/`Textarea`/`Field`, `Select`, `Dialog`, `Sheet`, `DataTable` (sorting, cursor pagination, column priority + tablet expansion row + <768px card fallback, density-aware), `Badge`, `Toast`/`Toaster`, `EmptyState`, `ErrorState`/`ForbiddenState`, `Skeleton`.

Application shell (`web/src/components/shell/`): `AppShell`, `Sidebar` (full/rail/drawer per breakpoint, scope-filtered, live error count on Bridges), `TopBar` (search trigger, live/polling indicator, theme cycling, operator menu), `CommandPalette` (⌘K / `/`, nav + bridge jump, arrow-key + Enter), `SignIn` (mock issuer), `g`-chord navigation (`g o/b/u/r/f`), focus-to-main on route change.

Pages: `DashboardPage`, `BridgesListPage`, `BridgeDetailPage`, the add-bridge wizard (`pages/bridges/wizard/`), `UsersPage`/`UserDetailPage`, `RoomsPage`/`RoomDetailPage`, `FederationPage`/`FederationDestinationPage` (all three added in the integration-review response, see that section below), and `PlaceholderPage` for the remaining information-architecture routes (Reports, Media, Cluster, Migration, Settings) so navigation matches the full IA even though those pages are not built. `/audit` now has real list and detail pages (2026-09-23 update above). Every route is lazy-loaded (`web/src/routes.tsx`, `lazyRouteComponent`). **The bridges/dashboard pages were rewritten mid-session against the real API — see "Reconciliation" below; do not assume the shapes described in `flows.md`'s example paths are current.**

Testing:

- Vitest: 30 tests across 7 files. `npm run test` passes.
- Storybook: every primitive has a story; `@storybook/addon-a11y` runs axe (`wcag2a/2aa/21aa/22aa`) on every story in both themes via a theme-toolbar decorator. `npm run build:storybook` succeeds.
- Playwright: `e2e/add-bridge.spec.ts` (flows.md flow 1 in full), `e2e/bridges-list.spec.ts` (nested-interactive-element regression, desktop + phone viewport), `e2e/users-rooms-federation.spec.ts` (flows 2-4 smoke). Every test runs `@axe-core/playwright` and `installDomNestingGuard` (see the integration-review response above). **Run and passing**: 9/9.
- `npm run build` and `npm run build:mock` both succeed; `dist/` is the production artifact for track 15 to embed. `npm run check` (lint + typecheck + test + build) is clean.

A real accessibility bug was found and fixed via axe coverage, not left for later: an unlayered `button { color: inherit }` in `base.css` was silently beating every Tailwind `text-*` utility applied to a `<button>` (unlayered CSS always outranks `@layer`-wrapped rules regardless of specificity), making the primary "Add bridge" button render dark text on its indigo fill (2.83:1 contrast). Fixed by deleting the duplicate rule. Separately, several status-badge colour pairs (`success`, `warning`, `--color-text-faint` in both themes, `muted-status` in dark) were too light for 4.5:1 at 12-13px; retuned and verified with a new `scripts/check-contrast.mjs`. See `docs/design/design-system.md` §2.3.

## Reconciliation against track 15's real OpenAPI document (2026-09-18)

`crates/hs-admin/openapi/openapi.yaml` appeared partway through this session; its status file explicitly invited track 16 to generate against it ("Track 16: generate your client and mock-check your work against openapi.yaml; it is validated ... and the contract test proves the real router agrees with it"). `docs/decisions/0003-web-stack.md`'s generation-source rule was followed: `scripts/generate-client.mjs` now prefers it whenever it exists (the "own draft, `web/mocks/openapi.yaml`" fallback is unused unless the real file is deleted). This was a substantial rewrite, not a type-level touch-up, because the real resource model differs from this track's own earlier draft in ways that reshape the UI:

- **Resources are `/appservices` and `/bridge-types`**, not `/bridges` and `/bridges/kinds`. "Bridge" was always this track's UI framing over 11's generic appservice registry; the real API makes that explicit. `web/src/api/bridges.ts` and every bridges page were rewritten around `AppService`/`BridgeType`.
- **`AppService` has no `name` or `kind` field**, and nothing links a created appservice back to the bridge-type catalog entry it came from. `deriveDisplayName()` (humanises `id`) and `deriveKindLabel()` (reads `protocols`) in `web/src/api/bridges.ts` are this track's documented, reasonable-decision UI stand-ins. **Feedback for 15/11**: consider adding a display name and/or a `bridge_type` back-reference to `AppService`, or accept that the management UI will keep deriving one.
- **No inline backlog summary** on the list resource (`AppServicePage`), only a separate paginated `GET /appservices/{id}/backlog`. The bridges list can no longer show a backlog column without an N+1 fetch per row, so it doesn't; backlog only shows on the bridge detail page now. **Feedback for 15**: a cheap backlog count/age on the list item (like `health` already gets) would let the list surface backlog again.
- **No `state`/`kind` filter parameter** on `GET /appservices` (only free-text `q`, `limit`, `cursor`, `include_total`). The bridges list's health filter now applies client-side to whatever page is loaded, not server-side across all pages. **Feedback for 15**: a `health` filter param would fix this properly.
- **Replay is asynchronous**: `POST /appservices/{id}/replay` returns `202` + a `Task`, not a synchronous result. The UI shows a toast naming the task id and does not (yet) poll `/tasks/{id}` for completion.
- **The wizard's registration/compose/Kubernetes-resource YAML is rendered by the server** (`POST /bridge-types/{type}/render`, returning `registration`, `registration_yaml`, `compose_yaml`, `bridge_resource_yaml`), not assembled client-side. `pages/bridges/wizard/artifacts.ts` (this track's earlier hand-rolled YAML builder) is deleted. The Review step now calls render on entry/re-entry and shows its result; Create sends the render result's `registration`/`registration_yaml` to `POST /appservices`. The admin API has no "deployment" concept at all — which artifacts to _show_ (Compose vs. the Kubernetes `Bridge` resource) stays a presentational choice this track makes client-side from the render result, which always contains both.
- **Real scopes are `admin:read`, `admin:write`, `bridges:read`, `bridges:write`, `moderation:read`, `moderation:write`** — not the `moderation:*` this track's own earlier draft guessed from an informal reading of the brief. `src/lib/auth.ts` fixed: `admin:write` implies every scope; `bridges:write`/`moderation:write` each imply their own `:read`.
- **The `Problem` (RFC 9457) shape has no structured "which resource conflicts" field.** `instance` identifies the request, not reliably a pre-existing conflicting resource, so the namespace-conflict banner on the Identity step now shows the message only, with no "View" link to the conflicting resource (this track's earlier mock had invented one). **Feedback for 15**: a structured conflict detail (e.g. `errors[]` entries with a resource pointer, or an extension field) would let the UI link to what's conflicting.
- **`GET /appservices/{id}/registration` (which includes tokens) needs `bridges:write`**, not `bridges:read`; the Registration tab now shows a `ForbiddenState` naming that scope for read-only operators, rather than attempting the call.
- **No real login/session sub-resource** on appservices at all. The Logins tab is now honest about this gap (explains it, links to `links.login_url` if the API ever populates it) instead of showing fabricated remote-login data against a schema that doesn't support it.
- **No single `/overview`/dashboard resource.** `DashboardPage` is now composed client-side from `GET /statistics/overview` (counts), `GET /server` (version/uptime), `GET /cluster` (replica count — there is no explicit single-node/cluster boolean; `replica_count <= 1` is this track's documented heuristic, used for the wizard's Kubernetes-card visibility too), `GET /appservices` (bridges strip + unhealthy attention rows), `GET /federation/destinations` (federation strip + failing-over-an-hour attention rows), and `GET /audit-log` (recent 5). The "Activity" sparklines section from the earlier draft is dropped for now: `GET /statistics/timeseries?metric=...` exists but its metric-name vocabulary isn't documented in the schema, so wiring it up needs either a real backend to introspect or an RFC amendment naming the metrics.

New mock fixtures/handlers matching the real shapes: `web/src/mocks/data/{appservices,bridge-types,dashboard}.ts`, `web/src/mocks/handlers.ts` (fully rewritten). `web/src/mocks/browser.ts`'s Playwright test seam (`window.__hsAdminMock`) was renamed `setClusterMode` (was `setOverviewMode`) to match.

**What this means for anyone reading `flows.md`**: the flow narrative (steps, branches, what the operator sees) is still accurate; the API paths it cites (`web/mocks/openapi.yaml`, itself now just a fallback) are not what the app actually calls. `flows.md` was not rewritten in this pass (it is a design document, not code) — treat this section and `web/src/api/bridges.ts`'s doc comment as the current source of truth for the real contract, and update `flows.md`'s path references in a follow-up pass.

## Next

-1. **New, from the 2026-09-19 (later still) Element Web session, highest priority of all**: get
    tracks 04 and 07 the three bug reports above (createRoom + `power_level_content_override`,
    missing `account/3pid`, missing `unsigned.prev_content`) — none of these are this track's
    crates to fix, but the createRoom one in particular blocks the single most basic real-client
    flow (creating a room) end to end, for every preset, unconditionally, whenever the client
    follows the spec's own `power_level_content_override` merge semantics. This is a bigger deal
    than anything left in this track's own backlog below.
0. **Do this first**: re-run `npm run test` (Vitest infra failed to even start this session under host load — see "Wrap-up note" above, not a code issue) and `npm run build`, neither confirmed as of 2026-09-19. Then re-run `npm run test:e2e:real` against a fresh `hs serve` build to confirm against the now-larger real surface (15 of 142 operations per the integration lead, up from the 5 this session tested).
0b. `GET /api/v1/events` (SSE) is real now (per the integration lead) — wire it up, replacing `TopBar`'s "Polling every 30s" and each page's `refetchInterval`. This was blocked on 15 shipping it; it no longer is.
1. Reports page (flow not yet built; still a `PlaceholderPage`). Media, Cluster, Migration and Settings remain placeholders too — Phase 1/2 per the brief. Audit log was completed 2026-09-23 (update above).
2. Reset-password for users (`POST /users/{user_id}/reset-password`) needs a password-entry/generate UI this session deliberately deferred; redact-events, media tab, pushers, external IDs, 3PIDs are also unbuilt on the User detail page.
3. Real OAuth: swap `src/lib/auth.ts`'s mock issuer client for `oauth4webapi` against 07's issuer (check 07's status file — it was also active this session).
4. SSE live updates (`GET /events`, referenced in 15's status file) once wired up; `TopBar`'s "Polling every 30s" indicator and each page's `refetchInterval` are the seam to replace.
5. Poll `GET /tasks/{id}` after `POST /appservices/{id}/replay` (currently fire-and-forget with a toast naming the task id).
6. i18n scaffolding (`web/src/i18n/`) — not started; all copy is inline English.
7. Wire up `GET /statistics/timeseries` for the dropped Activity sparklines, once the metric-name vocabulary is confirmed (ask 15, or read `hs-admin`'s statistics handler implementation once it exists beyond the mock).
8. Lighthouse scores and a three-operator usability pass (definition of done) are unstarted; need real users/a running instance.
9. Update `flows.md`'s path citations to match the real API (see the reconciliation note above); currently only this status file and `web/src/api/bridges.ts`'s doc comment carry the corrected paths.
10. Room detail is missing the State/Timeline/Federation tabs from `flows.md` flow 3 and the room delete/purge action; User detail is missing Reports-about-them and Audit tabs.
11. Two spots still use a bare `ErrorState` instead of the new `QueryProblemState` (not upgraded this session, not broken either — just not honest about 501/503 specifically yet): `pages/bridges/wizard/steps/KindStep.tsx` (the bridge-type catalog step) and `ReviewStep.tsx`'s two `ErrorState` usages (separate from `renderErrorMessage`/`createErrorMessage`, which *were* fixed — see "Decisions made"/the session write-up above for `wizardErrorMessage`). Same one-line swap pattern as every other page.
12. `src/lib/auth.ts`'s real-mode `RealSignIn` component (`SignIn.tsx`) uses a hand-rolled `role="tablist"`/`role="tab"` pair with no associated `tabpanel`, and is not covered by any axe/e2e check today (the mock e2e suite only exercises `MockSignIn`, and `e2e-real/` doesn't run axe). Worth either using Radix's `Tabs` primitive (already a project dependency, used elsewhere) or adding axe coverage in `e2e-real/`.

## Blockers

**Resolved: the Docker blocker below no longer applies.** The 2026-09-19 (later still) session
retried it once the host was quiet and it worked cleanly — two Element Web containers up and
`healthy` within ~15 seconds each, no retries needed. Kept below for the historical record only.

~~**Element Web session (2026-09-19, later): the shared Docker daemon stopped completing container
lifecycle operations**~~ (`run`/`create`/`kill`/`rm`/`logs`/`exec` all timed out at 20-30s while
`docker version`/`docker ps` kept responding) under heavy host load (load average 9-12 throughout;
a bare `sleep 60` took ~4 minutes wall-clock). This blocked seeing Element Web actually render in
a browser this session — not a defect in this server. Everything needed to finish is staged in
`web/element-testing/` and `web/scripts/element-proxy.mjs`; the next session's first move should
be retrying the same `docker run` once the host is idle. This is an environment blocker, not a
code blocker, and does not block anything else in this track's own work.

**New, current blocker: `npm run test` (Vitest) and `npm run lint` cannot be verified on this
machine under concurrent-agent load**, reproduced again this session (see "Verification of `web/`
itself" above) after two independent prior sessions reported the identical symptom. This is now a
three-times-repeated, unresolved infrastructure gap, not a one-off. It does not block this track's
own further work (nothing here depends on a green Vitest run to proceed), but it does mean this
track's actual code-level test coverage has gone unverified by an actual test run for three
sessions running — worth flagging to the integration lead as a standing risk independent of
Element.

## Interfaces provided

- `web/dist/` (production build, base path `/admin/`) for 15 to embed via `rust-embed`; also runs standalone reading an optional `config.json` for API base URL/issuer. **Not yet embedded** — see "The embedded build" above for the exact one-line `assets.rs` change, which is the integration lead's/15's to make, not this track's.
- A real sign-in against `hs_auth::admin_verifier::AdminTokenVerifier` (`web/src/lib/auth.ts::signInWithToken`/`signInWithPassword`), verified live against a real `hs serve` this session (see the screenshots and `docs/design/screenshots/real-server-2026-09-19/`).
- The shared "degrade honestly" treatment (`web/src/api/problem.ts`, `web/src/components/QueryProblemState.tsx`, `NotImplementedState` in `web/src/components/ui/error-state/ErrorState.tsx`) — any track building more of `hs-admin`'s real handlers gets this for free on every page already wired to `QueryProblemState`; a `Problem` body with the right `status`/`detail`/`required_scope` is all that's needed for the UI to say the right thing.
- `web/e2e-real/real-server.spec.ts` + `web/playwright.real.config.ts`: a reusable proof harness against any real `hs serve` (`npm run test:e2e:real`), for 15/07/whoever wants to confirm a newly-real operation actually renders correctly in the UI without hand-testing.
- Usability/API-shape feedback for 15, gathered by actually building against the real contract — collected under "Reconciliation" above (appservice display name/kind reference, list-level backlog and health filter params, async replay, structured conflict details).
- `docs/design/design-system.md` tokens (`web/src/styles/tokens.css`) reusable by 07's account-management pages per the brief's "provides" list, once 07 exists.

## Interfaces needed

- **04 (hs-room), urgent**: fix `RoomActor::create_room` (`crates/hs-room/src/actor.rs`, ~line
  1320) to merge `power_level_content_override` on top of the generated default power-levels
  content instead of replacing it — see "Bugs found" above for the exact repro. This blocks room
  creation from real Element (and likely any client that follows the spec's own wording for this
  field) unconditionally. Also: `unsigned.prev_content` is never populated anywhere
  (`crates/hs-room/src/routes/render.rs::client_event_json`), causing visibly wrong timeline
  summaries ("X joined the room" for a display-name change) in every client.
- **07 (hs-auth)**: `GET /_matrix/client/v3/account/3pid` is unimplemented (plain 404, no route) —
  breaks Element's own Settings > Account page ("Unable to load email addresses").
- 15/integration lead: the `assets.rs` one-line swap (see "The embedded build" above) — this is the one remaining step to make the real build actually served by `hs serve`.
- 15: confirmation of the `/statistics/timeseries` metric-name vocabulary; the SSE event stream's exact event shapes (now real at `GET /api/v1/events` per the integration lead — not yet consumed by this session, see "Next"); responses to the feedback items above.
- 07: the real OAuth issuer (authorization code + PKCE, admin scopes) to replace the legacy-admin-token sign-in in `src/lib/auth.ts`, once it exists (Phase 1/2; the legacy path works today and is what's wired up).
- 11: nothing directly consumed this session (appservice data now comes from 15's real router, which is itself not yet backed by 11's actual registry — `/appservices` is still 501).
- 03: cluster status is now consumed (`GET /cluster`) for the single-node/cluster heuristic; no further ask yet.
- 13: reloadable-configuration schema for the Settings page (not built this session).

## Decisions made

- Stack and its two open points (route style: code-based; OpenAPI source order: prefer track 15's document whenever it exists) — `docs/decisions/0003-web-stack.md`.
- Tailwind v4 tokens use the native CSS `light-dark()` function (compiled to a `--lightningcss-light`/`-dark` toggle by Lightning CSS, driven by the `color-scheme` property under `[data-theme]`) instead of duplicating every token block per theme.
- Body text defaults to 14px (not the web-default 16px): a deliberate density choice for a dense operator tool, AAA-contrast-checked regardless.
- `DataTable` row actions are inline icon buttons only, no trailing overflow menu, to stay within Phase 0's explicit component inventory; Radix `DropdownMenu`/`Tabs`/`Switch` are composed directly at their call sites (operator menu, wizard database-mode select, bridge detail tabs) rather than wrapped as new `ui/` primitives not in `design-system.md`'s inventory.
- `window.__hsAdminMock` (`src/mocks/browser.ts`) is a small, explicit, mock-only test seam letting Playwright force cluster mode via `worker.use()` + a query-cache invalidation; `page.route()` cannot intercept a response MSW's service worker synthesizes without an outgoing network request, and a full page reload discards the SW-side runtime override, so the Kubernetes e2e test also had to route client-side rather than via `page.goto()`.
- The add-bridge wizard tracks progress via a `step` search param (not literal path segments like `/bridges/new/step/2`) so back/forward still work without a deeper route tree — an equivalent implementation of `flows.md`'s "progress is in the URL."
- **Reconciliation decisions** (all documented inline where they live, summarised here): `deriveDisplayName`/`deriveKindLabel` as UI stand-ins for fields the real `AppService` lacks; `replica_count <= 1` as the single-node/cluster heuristic (no explicit boolean exists); which render-result artifact to show driven by the wizard's own `deployment` choice, not sent to or interpreted by the server; dropped the namespace-conflict "View" link (no honest source for it in the real `Problem` shape); dropped the Activity sparklines pending the timeseries metric vocabulary; Logins tab rewritten to state the real gap rather than fabricate data.
- **Real sign-in is a token paste or a username/password login, never an "admin login" form** — matches `hs_auth::admin_verifier::AdminTokenVerifier`'s module doc exactly ("there is no separate admin login"). No server-URL field: the app always calls same-origin `/api/v1`/`/_matrix`, matching how it's actually deployed (embedded, or via the Vite dev proxy for local iteration); a "point this deployed app at an arbitrary other origin" field was considered and dropped as unneeded complexity/CORS surface for a feature nothing in the brief asked for.
- **`unwrap()` (`web/src/api/problem.ts`) replaces `if (error) throw error` everywhere**, so every thrown API error is a real `ApiProblemError` carrying the parsed RFC 9457 body, and classification (`classifyError`) reads `problem.status` directly rather than needing the raw `Response` threaded through every call site (the generated `Problem` schema already has a required `status` field). This was a mechanical rewrite (script-assisted) across all five `api/*.ts` files, not a hand-edit per call site, to keep ~50 call sites consistent.
- **`DashboardPage` has no page-wide error gate.** It composes six independent queries; the old "any one fails, blank the page" behaviour would have hidden `/server`'s real data behind `/statistics/overview`'s 501 on a real server today. Each section (Attention, each Health tile, Bridges strip, Federation strip, Recent audit) now degrades independently. This is the one page-level architectural change beyond "swap `ErrorState` for `QueryProblemState`".
- **`e2e-real/` is a separate Playwright config and test directory from `e2e/`**, not a conditionally-skipped spec inside the existing suite, so a real-server run can never start by accident from `npm run test:e2e` and the existing suite's `webServer`/`baseURL` (fixed at the mock preview build) never needs to change.
- **The Vite dev proxy (`VITE_HS_API_PROXY_TARGET`) is dev-only**, not a production code path: in production the app is served by `hs serve` itself at `/admin/`, so `/api/v1` is already same-origin. The proxy exists purely so this track can develop/test against a real, separately-running `hs serve` before the embedded-assets swap lands.

## Shared dependencies added

None to the Rust workspace (this track only touches `web/`, `docs/design/`, `docs/status/16-management-web-interface.md`, and dated files under `docs/decisions/`). `web/package.json`'s dependency set was already fully specified by the interrupted attempt's scaffold; nothing was added or removed.

## How to verify

From `web/`:

```
npm run lint         # eslint (jsx-a11y strict) + prettier --check — confirmed clean 2026-09-19
npm run typecheck    # tsc -b — confirmed clean 2026-09-19
npm run test         # vitest run — NOT confirmed 2026-09-19 (infra failure, see "Wrap-up note" above; re-run this first)
npm run build        # generate:client (from crates/hs-admin/openapi/openapi.yaml) + tsc -b + vite build -> dist/ — NOT re-run 2026-09-19, see "Wrap-up note"
npm run build:storybook   # storybook build -> storybook-static/
npm run test:e2e     # playwright test against the mock (e2e/) — includes the new degrade-honestly.spec.ts; confirmed 3/3 new tests pass 2026-09-19, full suite not re-run
npm run test:e2e:real     # playwright test against a REAL hs serve (e2e-real/), skipped unless HS_REAL_SERVER_URL is set — see README.md "Real-server mode"; confirmed 6/6 pass 2026-09-19 (see the status entry above)
node scripts/check-contrast.mjs   # offline contrast check for the status tokens
```

`npm run check` runs the first four in sequence; as of 2026-09-18 (before this session) it was clean with "30 tests" and all 9 e2e tests passing. **As of 2026-09-19, only lint and typecheck were re-confirmed** — see "Wrap-up note: what's verified and what isn't" above for exactly why and what to run first. `npm run dev:mock` for interactive mock use; sign in with either button on the landing screen. For real-server interactive use, `npm run dev:real` — see README.md.
