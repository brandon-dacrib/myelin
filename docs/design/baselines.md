# Baselines: what exists today, and what this interface must exceed

Track 16. Date: 2026-09-17. Sources: Element Admin (element-hq/element-admin, the ESS admin console, v0.1.x), synapse-admin (Awesome-Technologies and the etke.cc fork), `refs/palpo/web-admin/`, the mautrix `bridgev2` framework in `refs/mautrix-go/` and the Synapse admin API docs in `refs/synapse/docs/admin_api/` (behavioral reference only).

## Element Admin

What it is: the admin console that ships with Element Server Suite. A React single-page app that authenticates through Matrix Authentication Service (OIDC, PKCE) and talks to the Synapse admin API and the MAS admin API.

What it does well:

- Single sign-on through the same issuer the clients use; no separate admin password.
- A short, curated page list (dashboard, users, rooms, registration tokens) that a first-time operator can read in a minute.
- Consistent, restrained visual language from Element's design system (Compound).

Where it stops:

- No bridges or appservices at all. Bridge operators go back to `kubectl`, YAML and bot commands.
- No federation view, no media view, no moderation queue, no audit log, no migration.
- Nothing is live: every page is a fetch on mount.
- Tables have no saved filters, no bulk actions, no keyboard navigation.

## synapse-admin

What it is: a `react-admin` application over the Synapse admin API, the most widely deployed Synapse admin UI, maintained today mostly by etke.cc.

What it does well:

- Coverage. Nearly every Synapse admin endpoint is reachable: users (create, edit, deactivate, erase, shadow-ban, rate limits, devices, pushers, media, joined rooms, login-as), rooms (list, detail, members, state, forward extremities, make admin, block, delete), event reports, room directory, federation destinations with retry, registration tokens, server notices, user media statistics, CSV import.
- Dense list views with sorting and column choice.

Where it stops:

- It is a generated CRUD console. Every action is a form; nothing is organized around the task an operator arrives with ("this user is spamming", "why is WhatsApp lagging", "is federation with matrix.org broken").
- No dashboard, no notion of attention or priority; an operator cannot tell in one glance whether the server is fine.
- Empty states are blank tables. Errors are raw JSON in a toast.
- No bridge awareness beyond appservice-owned users appearing in the users list.
- Accessibility is whatever `react-admin` gives by default; nothing is audited.
- Visuals are the Material default; it looks like every other admin template.

## Palpo web-admin

`refs/palpo/web-admin/` is an early, minimal Node application (accounts, workflow, outbound, store modules) with a plain HTML front end. Useful as a reminder that a Rust server's admin UI usually arrives last and thin; not a design baseline.

## What bridge operators actually do (mautrix)

Measured from `refs/mautrix-go/bridgev2/` (commands, `status/bridgestate.go`, `matrix/provisioning.go`) and the mautrix documentation. A bridge operator's week is made of:

1. **Installing** a bridge: write a config, generate a registration (`as_token`, `hs_token`, `sender_localpart`, `namespaces`), add it to the homeserver, restart the homeserver (with Synapse) and the bridge, then check the bridge's management room for `STARTING` / `RUNNING`.
2. **Double puppeting**: either a shared non-exclusive registration or the legacy shared-secret module; getting it wrong is the top support question.
3. **Encryption**: enabling `de.sorunome.msc2409.push_ephemeral`, `org.matrix.msc3202`, `io.element.msc4190` in the registration and the matching flags on the server; PLAN section 8.3 makes these defaults.
4. **Logging in** the remote account: `login`, `relogin`, `logout`, `list-logins`, `set-preferred-login` bot commands, or the provisioning API (`GET /_matrix/provision/v3/login/flows`, `POST .../login/start/{flow}`, `.../login/step/...` with `user_input`, `cookies`, `display_and_wait` and `complete` step types). Login belongs to the bridge; the interface must link to it, not reimplement it.
5. **Watching state**: bridge state events `STARTING`, `UNCONFIGURED`, `RUNNING`, `BRIDGE_UNREACHABLE`, and per-login `CONNECTING`, `BACKFILLING`, `CONNECTED`, `TRANSIENT_DISCONNECT`, `BAD_CREDENTIALS`, `UNKNOWN_ERROR`, `LOGGED_OUT`, each with an optional `error` code, `message`, `user_action` (`OPEN_NATIVE`, `RELOGIN`, `RESTART`), `remote_name` and `remote_profile`.
6. **Diagnosing lag**: is the homeserver's transaction queue to the bridge backing up (backlog depth and age), is the bridge answering `/ping`, are transactions failing (dead letters), or is the remote network disconnected. Today this needs Prometheus and logs.
7. **Maintenance**: rotating tokens, restarting, upgrading the image, `sync-portal`, `delete-portal`, relay mode, management room.

## What this interface must exceed, concretely

| Baseline gap | This interface |
|---|---|
| No bridge management anywhere | Bridges is the marquee section: health, backlog, dead letters with replay, add-bridge wizard producing a `Bridge` resource or registration plus Compose snippet, token rotation, deep links to bridge login. |
| No glanceable state | Overview page answers "is the server fine" in one screen: attention list, health tiles, activity, bridges, federation. |
| CRUD forms instead of tasks | Every top flow in `flows.md` is designed as a guided path with the action two clicks from the list. Destructive actions explain consequences before the confirm. |
| Fetch on mount | Live by default through the admin API's SSE stream; a connection indicator tells the operator when the view is stale. |
| Blank empty states, raw error JSON | Designed empty, error, forbidden, loading and offline states for every page (`states-density-responsiveness.md`). |
| Generic template look | A design system with its own tokens and restraint (`design-system.md`); calm, dense where it should be, readable in light and dark. |
| Unaudited accessibility | WCAG 2.2 AA commitments enforced by axe in Storybook and Playwright plus a manual screen-reader pass per release (`accessibility.md`). |
| No keyboard model | Command palette, `g` shortcuts, full keyboard operability of tables and dialogs. |
| No migration story | A guided migration from Synapse with live progress and a rollback explanation. |
