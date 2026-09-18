# Flows

Track 16. Date: 2026-09-17. Status: draft for review with tracks 11, 13 and 15. Each flow is written as the operator experiences it, with the API calls it needs (paths from the OpenAPI draft in `web/mocks/openapi.yaml`, to be reconciled with track 15's document), the branches for empty, error and forbidden, and the Playwright test that will cover it. Every flow must be reachable from the Overview page in at most two clicks.

## Flow 1. Add a bridge

**Who:** solo operator or team admin with `bridges:write`. **Trigger:** "I want WhatsApp in Matrix", or the Bridges list is empty. **Goal:** a registered bridge with a working registration and the artifacts to run it, with double puppeting and encryption already right.

1. **Start.** Overview, Bridges strip, "Add bridge" (or Bridges list, primary action, or the empty state's button). Route `/bridges/new`. The wizard is a full page with a step rail on the left (Kind, Identity, Deployment, Options, Review), a back link, and the current step's form. Progress is in the URL so back works.
2. **Kind.** A grid of bridge types with the network's name, a one-line description and what it needs (a phone, an API token, a bot). Types: mautrix-whatsapp, -telegram, -signal, -discord, -slack, -gmessages, -meta, -instagram, -twitter, -linkedin, -gvoice, -irc, -zulip, matrix-hookshot, heisenbridge, matrix-appservice-irc, and "Custom appservice" for anything else. Choosing a kind fills sensible defaults for every later step. `GET /api/v1/bridges/kinds` supplies the catalogue so 11 can extend it without a UI release.
3. **Identity.** Name (display), `id` (derived, editable, validated unique through `GET /api/v1/appservices/check?id=`), `sender_localpart` (default per kind, e.g. `whatsappbot`), user namespace regex (default per kind, e.g. `@whatsapp_.*:example.org`, shown with an example match), alias namespace, room namespace, optional bot display name and avatar. Namespace conflicts against existing registrations and existing users are checked live (`POST /api/v1/appservices/validate`) and shown inline with the conflicting entry linked.
4. **Deployment.** Two cards: **Kubernetes** (the operator will create a `Bridge` resource; asks for namespace, image tag with the latest known tag prefilled, database mode: own CloudNativePG database or shared cluster schema) and **Self-managed** (the operator runs the bridge themselves; produces `registration.yaml` and a Docker Compose snippet with the homeserver URL, tokens and a bind-mounted config). In single-node mode Kubernetes is hidden.
5. **Options.** All on by default with a sentence each: double puppeting (creates or reuses the shared non-exclusive registration and tells the bridge about it), encryption (MSC2409 ephemeral, MSC3202 device lists and OTK counts, MSC4190 device management; declared in the registration), rate-limit exemption, relay mode off, management room notices on errors only. Advanced disclosure: `url` override, `protocols`, `receive_ephemeral` explicit.
6. **Review.** A read-only summary of every choice grouped by step with "Edit" links; the registration YAML preview; the exact list of what "Create" will do (register appservice, mint tokens, create puppet-namespace reservation, and for Kubernetes: create the `Bridge` resource). Primary action **Create bridge**. `POST /api/v1/bridges` with an `Idempotency-Key`.
7. **Created.** A success page (`/bridges/:id/created`) that shows the artifacts once: for Kubernetes, the `Bridge` resource YAML and `kubectl apply` line, plus the registration if they want it; for self-managed, `registration.yaml`, the Compose snippet and the bridge config fragment (homeserver address, `as_token`, `hs_token`, double-puppet secret). Each block has Copy and Download. A banner explains tokens are shown once and can be rotated later. The page ends with "What happens next": the bridge shows **Waiting for first ping** until it connects, then **Running**; "Open bridge" goes to the detail page.
8. **Detail.** The bridge detail page in the Waiting state, with the login tab explaining that remote-account login happens in the bridge (bot command `login` or the provisioning link) once it is running.

Branches:

- **Forbidden**: without `bridges:write` the Add button is disabled with the scope named; direct navigation shows the forbidden state.
- **Validation errors** (RFC 9457 `errors[]` with `pointer`): mapped to the field, the step rail marks the step, Create is blocked until fixed.
- **Namespace conflict** (409 with `conflicts[]`): shown on Identity with links to the conflicting registration or user and a "Use a different prefix" suggestion.
- **Network error on Create**: the review page keeps state, shows a retryable error; the idempotency key makes the retry safe.
- **Kubernetes operator not installed**: the Kubernetes card is disabled with "Operator not detected on this cluster" and a link to the docs.

Playwright: `e2e/add-bridge.spec.ts` (happy path self-managed, happy path Kubernetes, namespace conflict, forbidden), axe on every step.

## Flow 2. Find and deal with a user

**Who:** team admin with `admin:write` or `moderation:*`. **Trigger:** "user X is spamming", "someone forgot their password", "an employee left". **Goal:** find the right user in seconds, understand what they have done, act with confidence and leave a trail.

1. **Find.** From anywhere: ⌘K, type part of the Matrix ID, display name, email or external ID; or the Users list search box. Results show avatar, ID, kind (person, bot, puppet of bridge X), status pills. `GET /api/v1/users?q=` (server-side search across ID, display name, 3PIDs and external IDs). Enter opens the user.
2. **Understand.** `/users/:id`. Header: avatar, display name, ID with copy, pills (Admin, Locked, Suspended, Deactivated, Shadow-banned, Bridge puppet), "last seen 4 min ago from Element X". The Overview tab shows profile, 3PIDs, external IDs (SSO subject), registration source and date, rate-limit override, account validity, consent, and a **Recent activity** block: messages sent in the last 24 h, rooms joined recently, reports about them, devices added. `GET /api/v1/users/{id}`, `GET /api/v1/users/{id}/activity`.
3. **Assess.** Reports tab lists reports about the user with the reported event in context; Rooms tab lists rooms with a "public" flag and member counts; Sessions tab lists devices with last-seen IP and client.
4. **Act.** Primary actions in the header, in escalating order: **Send notice**, **Lock** (blocks sign-in, sessions stay), **Suspend** (read-only account, MSC4323), **Reset password**, **Sign out everywhere**, and in the Danger tab **Deactivate** (with optional erase) and **Redact recent messages** (with a time window and room scope, MSC4194). Each opens a dialog that states the effect ("Suspended users can read but not send; they see a message explaining why") and takes an optional reason recorded in the audit log. `POST /api/v1/users/{id}/actions/{lock|suspend|deactivate|...}` with idempotency keys.
5. **Confirm.** Toast with undo where the action is reversible (Lock, Suspend); the header pills update live; the audit tab shows the entry with the operator's name.

Branches:

- **No match**: the search shows "No user matches" with the query and a "Create user" action if scope allows.
- **Bridge puppet**: acting on a puppet warns that the bridge owns it and links to the bridge; deactivation is discouraged in favour of logging the remote account out.
- **The operator themselves**: self-lock and self-deactivate are disabled with an explanation.
- **Forbidden**: actions disabled with scope; 403 rendered as a forbidden state.

Playwright: `e2e/users.spec.ts` (search, open, suspend with reason, undo).

## Flow 3. Understand a room

**Who:** team admin or moderator. **Trigger:** a report names a room, an alias appears in a complaint, federation with a room misbehaves. **Goal:** know what the room is, who is in it, where it federates, and whether to act.

1. **Find.** ⌘K or Rooms list search by ID, alias or name. `GET /api/v1/rooms?q=`.
2. **Overview.** `/rooms/:id`. Header: name, canonical alias, ID with copy, pills (Space, DM, Public, Encrypted, Bridge portal of X, Version 12, Blocked). Overview shows creator and creation date, topic, join rule, history visibility, guest access, power-level summary (admins and moderators named), local vs remote members, servers in the room, size (events, state events, media), last activity, and the bridge that owns the room if it is a portal.
3. **Members.** Table with user, server, power level, join date, puppet flag; filter by server or power; kick or ban with reason from the row; bulk ban by server.
4. **State.** Current state grouped by event type; click to see raw JSON; history of a state key (`GET /api/v1/rooms/{id}/state`).
5. **Timeline.** Recent events and event context around an event ID (`GET /api/v1/rooms/{id}/events?around=`); redact from the row.
6. **Federation.** Servers in the room with destination health; the room's ACL.
7. **Act.** Header actions: **Make me admin** (Synapse-style admin join), **Block** (prevents joins; explains that members remain), **Delete** (Danger: purge, optional new room and message, with the member count and federation consequences listed; requires typing the room ID).

Branches: room not found (deleted, with the audit entry that deleted it), room not local (remote-only cache with reduced tabs), forbidden.

Playwright: `e2e/rooms.spec.ts` (search, open, members filter, block with reason).

## Flow 4. Watch federation health

**Who:** team admin or SRE. **Trigger:** "messages to matrix.org are delayed", "our keys are expiring", routine check. **Goal:** know in one glance which destinations are failing and why, act on the ones that need it, and verify our own inbound reachability.

1. **Glance.** Overview federation strip: healthy / backing off / failing counts; failing destinations older than one hour appear in Attention with "Retry" inline.
2. **List.** `/federation`: table sorted by attention (failing first), with status, since, backlog, last success, last failure reason (DNS, TLS, 5xx, key mismatch, ACL), retry countdown. Live through `federation.destination` events. `GET /api/v1/federation/destinations`.
3. **Detail.** `/federation/:server`: health timeline, rooms shared (count with link), their keys with validity and where we fetched them (direct or notary), recent transactions with duration, last errors, and actions: **Retry now**, **Reset backoff**, **Refresh keys**. `POST /api/v1/federation/destinations/{server}/actions/retry`.
4. **Ourselves.** `/federation/keys`: our signing keys with expiry and rotate; `.well-known` delegation check with the resolved address; inbound check (asks the server to fetch itself through federation) with the result; certificate expiry.

Branches: no destinations yet (single-node without federation: "No federation traffic yet. When your users join rooms on other servers they will appear here."), federation disabled in config (explained with the config key), forbidden.

Playwright: `e2e/federation.spec.ts` (list sorted by attention, retry action, keys page).

## Flow 5. Run a migration from Synapse

**Who:** SRE or team admin with `admin:write`. **Trigger:** moving from Synapse. **Goal:** a guided, observable import with a clear cutover moment and a rollback story, run by `hs import synapse` (track 13) and watched here.

1. **Discover.** Migration appears in the sidebar when an import exists (`GET /api/v1/migration` returns a resource) or when the server was started with a Synapse config; the Overview Attention list says "Migration from Synapse in progress: 62%".
2. **Start (from the interface).** `/migration` when no import exists: a page that explains the four phases, asks for the source PostgreSQL URL (or a secret reference), the media directory or bucket, and whether to copy or lazily mount media; validates the source schema version (`POST /api/v1/migration/validate` reports version 94 supported, sizes, estimated duration); primary action **Start import**. Credentials are sent once and never echoed back.
3. **Bulk copy.** The status page shows phase progress with per-table rows (users, devices, tokens, keys, account data, push rules, rooms and events, media): count copied / total, rate, ETA; Synapse keeps running. State recomputation mismatches appear in a **Discrepancies** panel with the room and event and a "report" link (they are bugs, not silent differences).
4. **Catch-up.** Delta rounds shown as "catching up: 1,240 events behind, last round 4 s"; the page tells the operator when the delta is small enough to cut over.
5. **Cutover checklist.** A checklist that the operator ticks: stop Synapse (with the command), run final delta (button, shows result), verify (runs the differential harness against the Synapse read replica; results shown), switch DNS or ingress (instructions), start clients. Each step records who did it and when.
6. **Verify and finish.** Summary: counts imported, duration, discrepancies, what was not carried over (Python modules, workers), and the **rollback** explanation: Synapse's database was never written; to roll back, start Synapse again; activity since cutover is lost.

Branches: unsupported schema version (blocked with the version and the supported list), source unreachable (retry with the error), disk projection exceeding capacity (warning before start), forbidden.

Playwright: `e2e/migration.spec.ts` (status page rendering of each phase from fixtures, cutover checklist ticking).

## Reachability table (two clicks from Overview)

| Task | Path |
|---|---|
| Add a bridge | Overview → Add bridge (strip) |
| Pause a bridge | Overview → bridge pill → Pause |
| Retry a destination | Overview → Attention row → Retry |
| Open a report | Overview → Attention row |
| Suspend a user | ⌘K user → Suspend |
| Block a room | ⌘K room → Block |
| See migration progress | Overview → Attention row |
| Send a server notice | Overview → Settings → Server notices |
