# Information architecture

Track 16. Date: 2026-09-17. Status: draft for review with tracks 11 and 15. Companion documents: `flows.md`, `states-density-responsiveness.md`, `accessibility.md`, `design-system.md`, `baselines.md`.

## 1. Principles

1. **A product, not a console.** Pages are organized by what an operator is trying to do, not by which API resource exists. The API (track 15) is designed from these flows; where the flow needs something the API lacks, that is an API change, not a UI workaround.
2. **Glance, then detail, then act.** Every section opens with a view that answers "is anything wrong here" before it asks the operator to search. Detail pages lead with status and the most likely next action.
3. **Two clicks to any routine task.** From the overview, any routine action (pause a bridge, deactivate a user, retry a destination, approve a report) is reachable in two clicks. Hard tasks (moderation with scope, migration, cluster operations) get a guided path instead.
4. **Explain before destroy.** Every destructive action states what will happen, to whom, and whether it can be undone, in plain language, before the confirm. Irreversible actions require typing the resource name.
5. **Live by default.** Views subscribe to the admin event stream; the operator never has to refresh to see the truth, and always knows when the view is stale.
6. **Calm.** Neutral surfaces, colour reserved for status, no decoration, consistent density. The interface should feel like a well-kept instrument panel, not a marketing site and not a spreadsheet.
7. **Everything is a link.** Every identifier (user, room, bridge, destination, event, task, replica) is a link to its detail page and can be copied in one click.

## 2. Who it is for

| Persona | Deployment | What they need most |
|---|---|---|
| **Solo operator** | `hs serve --single-node` on a small ARM host, a handful of users, one or two mautrix bridges | Add a bridge without reading four wikis; see that WhatsApp is connected; fix a login that expired; know when disk is filling. |
| **Team or company admin** | Small HA cluster, SSO, 100 to 5,000 users, several bridges, occasional abuse | Find a user fast, understand what they did, act with the right scope; understand a room they were told about; watch federation with partners; delegate moderation with limited scopes. |
| **Platform SRE** | Sharded cluster, migration from Synapse, on-call | Cluster and shard map, replica health, migration progress and cutover, federation backlog, audit log, reloadable configuration with validation. |

Scopes from track 15 (`admin:read`, `admin:write`, `bridges:read`, `bridges:write`, `moderation:*`) map onto these personas; the navigation reflects what the signed-in operator can do.

## 3. Top-level sections

The sidebar, in order. The order is by frequency of use, then by severity of what can go wrong.

| Section | Route | Answers |
|---|---|---|
| Overview | `/` | Is the server fine right now? What needs my attention? |
| Bridges | `/bridges` | Are the bridges connected and keeping up? How do I add or fix one? |
| Users | `/users` | Who is this user, what have they done, what can I do about it? |
| Rooms | `/rooms` | What is this room, who is in it, is it a problem? |
| Reports | `/reports` | What have users flagged, and what did we do about it? |
| Federation | `/federation` | Which servers are we talking to, and which are failing? |
| Media | `/media` | What is stored, what is quarantined, how much space? |
| Cluster | `/cluster` | Which replicas own what; are leases healthy? (Hidden in single-node mode; replaced by "This host".) |
| Migration | `/migration` | How far along is the Synapse import; what is left before cutover? (Shown only when an import exists or the server runs beside a Synapse database.) |
| Audit log | `/audit` | Who changed what, when, through which client. |
| Settings | `/settings` | Reloadable configuration, registration tokens, server notices, scheduled tasks, appearance, API tokens. |

Every route is under the app base `/admin/`. Appservices that are not bridges (a custom bot, a hookshot instance, a double-puppeting registration) live in the Bridges section as their own kind; there is no separate "Appservices" section because operators think of the registry as one list.

## 4. Page inventory

### Overview (`/`)

- **Attention** list (top): bridges in error, destinations failing for more than an hour, reports awaiting action, migration awaiting cutover, tasks failed, certificate or key expiry within 14 days, disk above threshold. Each row is a sentence with one action ("Bridge WhatsApp needs a new login. Open bridge"). Empty state: "Nothing needs your attention."
- **Health** tiles: version and update available, uptime, replicas (or host mode), store backend and size, media store size, federation signing key expiry.
- **Activity**: daily active users, messages per minute, joins, federation transactions in and out; 24 h sparklines with 7 d comparison.
- **Bridges** strip: one pill per bridge with state and backlog; click through.
- **Federation** strip: destinations healthy, degraded, failing; click through.
- **Recent audit** entries (5).

### Bridges (`/bridges`)

- **List** (`/bridges`): every registry entry. Columns: name, kind (bridge type or "Appservice"), state (with the mautrix state vocabulary), backlog (depth and age), last success, logins connected / total, actions. Filters: state, kind, paused. Sort: attention first by default. Row actions: pause, resume, open logs, replay dead letters.
- **Add bridge** (`/bridges/new`): the wizard in `flows.md` flow 1.
- **Bridge detail** (`/bridges/:id`): header with name, kind, state pill, `user_action` callout when the bridge asks for one, primary actions (Open bridge login, Pause, Rotate tokens, Remove). Tabs:
  - **Overview**: state timeline (last 24 h), backlog depth and age, transaction latency, error rate, last error with message, ping round-trip, deployment (Kubernetes `Bridge` resource status or "self-managed"), links to logs and metrics.
  - **Logins**: per-Matrix-user remote logins with state, remote name and profile, `user_action`; "Open login flow" deep link to the bridge's provisioning login (`/_matrix/provision/v3/login/flows` through the bridge's own URL) or bot command instructions.
  - **Registration**: `id`, `url`, `sender_localpart`, namespaces (users, aliases, rooms with exclusive flags), `rate_limited`, feature flags (`receive_ephemeral`, MSC3202, MSC4190), protocols; tokens masked with reveal and rotate; export registration YAML; the Compose or Kubernetes snippet regenerated from current values.
  - **Transactions**: recent transactions with status; dead letters with reason and replay (single or all); pause and resume the queue.
  - **Rooms and users**: portal rooms and puppet users owned by this bridge's namespaces, with counts and links.
  - **Danger**: pause, remove (with the consequence list: puppets stay, rooms stay, registration removed, tokens revoked).

### Users (`/users`)

- **List**: search box first (Matrix ID, display name, email, external ID); columns: user, display name, kind (person, bot, bridge puppet with bridge name, appservice sender), status (active, locked, suspended, deactivated, shadow-banned), admin, last seen, created, devices. Filters: status, kind, admin, bridge. Bulk: lock, suspend, deactivate, send notice. Create user.
- **User detail** (`/users/:id`): header with avatar, display name, ID, status pills, primary actions (Lock, Suspend, Deactivate, Reset password, Send notice). Tabs: **Overview** (profile, 3PIDs, external IDs, registration source, rate limits, admin flag, consent, account validity), **Sessions** (devices and access tokens; sign out one or all), **Rooms** (joined and invited; leave or kick from here), **Media** (uploads with quarantine and delete), **Reports** (by and about), **Pushers**, **Audit** (every admin action on this user), **Danger** (deactivate with erase, redact everything they sent with scope).

### Rooms (`/rooms`)

- **List**: search (ID, alias, name); columns: room, kind (room, space, DM, bridge portal with bridge), members local / total, joined servers, version, encryption, public, state events, last activity. Filters: kind, version, encrypted, public, blocked, bridge. Bulk: block, delete.
- **Room detail** (`/rooms/:id`): header with name, alias, ID, kind, pills (version, encrypted, public, blocked), actions (Block, Make me admin, Delete). Tabs: **Overview** (creator, created, topic, join rules, history visibility, guest access, power levels summary, federation servers, bridge portal owner, size stats), **Members** (with server, power level, bridge puppet flag; kick, ban), **State** (browse current state by type, with raw JSON), **Timeline** (recent events around a point, event context; redact), **Reports**, **Federation** (servers in the room and their health), **Danger** (block, delete with purge and optional new room).

### Reports (`/reports`)

- **Queue**: open reports first; columns: reported (event, user or room), reporter, reason, score, room, age, status. Filters: status, kind, room, reporter.
- **Report detail** (`/reports/:id`): the reported event in context (with the room's state at that point), the reporter, the reported user's recent reports, and actions that resolve the report: redact, kick, ban, suspend, deactivate, ignore, mark resolved with a note. Resolution is recorded in the audit log and shown on the report.

### Federation (`/federation`)

- **Destinations**: columns: server, status (healthy, backing off, failing since), backlog, last success, last failure with reason, retry, version. Filters: status. Actions: retry now, reset backoff.
- **Destination detail** (`/federation/:server`): health timeline, rooms shared, key set (with expiry and notary state), recent transactions, errors, ACL matches.
- **Our keys** (`/federation/keys`): signing keys, expiry, rotation, `.well-known` and delegation check, inbound reachability check.

### Media (`/media`)

- **Overview**: store size by kind (local, remote cache, thumbnails), quota, largest users, retention policy.
- **Browse**: by user, by room, recent, largest; quarantine, unquarantine, delete, purge remote cache older than.
- **Quarantine**: the quarantined list with reason and who.

### Cluster (`/cluster`)

- **Replicas**: each replica with version, uptime, load, owned rooms, owned sessions, sender shards, job leases, drain state. Actions: drain, cordon.
- **Shard map**: how rooms and users distribute across replicas, hot rooms, imbalance warnings.
- **Leases**: current leases, holders, expiry.
- In single-node mode the section is "This host": memory, disk, store size, background jobs.

### Migration (`/migration`)

- **Status**: phases (validate, bulk copy per table, media, delta, cutover, verify) with progress and rates; mismatches found; estimated time to catch-up; the cutover checklist; rollback explanation. See `flows.md` flow 5.

### Audit log (`/audit`)

- Every admin mutation: when, who (user and client), what (resource link), change summary, request ID. Filters: actor, resource, action, date. Export.

### Settings (`/settings`)

- **Configuration**: reloadable sections only (from track 13's schema), shown as forms with validation, a diff before apply, and the effective non-reloadable config read-only with "requires restart" labels.
- **Registration tokens**: list, create, expire.
- **Server notices**: send to user, room or everyone; history.
- **Scheduled tasks**: list with status; retry; cancel.
- **Appearance**: server display name, logo, accent (operator theming); theme preference is per operator.
- **API access**: the signed-in operator's scopes, personal tokens for automation (through the issuer).
- **About**: version, build, licences, link to OpenAPI document.

## 5. Object model as the operator sees it

```
Server ─┬─ Users ─┬─ Devices/Sessions
        │         ├─ Media
        │         └─ Reports (by, about)
        ├─ Rooms ─┬─ Members ─► Users
        │         ├─ State, Timeline
        │         └─ Reports
        ├─ Bridges/Appservices ─┬─ Logins ─► Users (puppets), remote accounts
        │                       ├─ Portal rooms ─► Rooms
        │                       └─ Transactions, Dead letters
        ├─ Federation destinations ─┬─ Keys
        │                           └─ Rooms shared ─► Rooms
        ├─ Cluster replicas ─► Shards (rooms, sessions, senders, jobs)
        ├─ Migration
        ├─ Audit entries ─► any resource
        └─ Settings (config sections, tokens, notices, tasks)
```

Every arrow is a link in both directions in the interface. Identifiers render as a `ResourceLink` with copy-to-clipboard.

## 6. Navigation model

- **Sidebar** (primary): the sections above, with counts for things needing attention (bridges in error, open reports, failing destinations). Collapses to an icon rail at 1024 to 1279 px and to a drawer below 1024 px.
- **Top bar**: server name and mode (single node, cluster of N), global search (⌘K or `/`), live indicator (connected, reconnecting, stale since), theme, operator menu (scopes, sign out).
- **Page header**: breadcrumbs (Section / Resource), title with identifier and copy, status pills, primary actions on the right, tabs below.
- **Command palette**: jump to any user, room, bridge, destination by ID or name; run actions ("Pause bridge WhatsApp", "Retry destination matrix.org"); navigate ("Go to Reports").
- **Keyboard**: `g o` overview, `g b` bridges, `g u` users, `g r` rooms, `g f` federation, `?` shortcuts; tables: arrow keys move, `Enter` opens, `x` selects, `Esc` clears.
- **URLs are state**: filters, sort, tab, selected row and pagination cursor live in the URL, so any view can be shared or bookmarked.
- **Back is safe**: wizards and dialogs are routes where it matters (`/bridges/new/step/2`), so the browser back button behaves.

## 7. Live updates

The admin API's SSE stream (track 15) is opened once by the app shell. Pages subscribe by resource kind; the query cache is invalidated or patched by event type:

| Event kind (proposed to 15) | Consumers |
|---|---|
| `bridge.state`, `bridge.backlog`, `bridge.transaction` | Bridges list and detail, overview strip |
| `federation.destination` | Federation list and detail, overview strip |
| `report.created`, `report.resolved` | Reports queue, sidebar count |
| `user.changed`, `room.changed` | Open detail pages |
| `migration.progress` | Migration page, overview attention |
| `cluster.replica`, `cluster.lease` | Cluster pages, top bar mode |
| `task.changed` | Settings tasks, overview attention |
| `audit.entry` | Audit log, overview recent |
| `server.stats` (every 30 s) | Overview tiles and sparklines |

When the stream drops, the live indicator shows "Reconnecting" then "Stale since 12:04" and pages fall back to polling every 30 s. Nothing blocks on the stream.

## 8. Permissions

The interface asks the issuer for the admin scopes and adapts: sections the operator cannot read are hidden; actions the operator cannot perform are shown disabled with a tooltip naming the missing scope ("Needs `bridges:write`"). A 403 from the API is rendered as a forbidden state naming the scope, never as a generic error.

## 9. Settled open questions

1. **Where the app is served.** Primarily by the homeserver at `/admin/` (embedded build, track 15). The same static build also runs as a separate deployment: the app reads `config.json` next to its `index.html` for the API base URL and issuer, so a cluster can serve it from an ingress or a CDN if it wants to. No server-side rendering. See `docs/rfcs/0016-admin-web-embedding.md`.
2. **How much configuration editing.** Only sections track 13 marks reloadable, edited through generated forms with 13's validation and a diff before apply. Everything else is shown read-only with the file path and "requires restart".
3. **Branding and theming.** Operators set a display name, a logo and an accent from a curated set in Settings, Appearance. Light and dark themes are per operator (system default). No arbitrary CSS; the tokens make the curated set cheap and keep contrast guaranteed.
