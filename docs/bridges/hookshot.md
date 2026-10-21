# Hookshot: one bridge, many connections

matrix-hookshot (`ghcr.io/matrix-org/matrix-hookshot:7.5.0`, the catalogue's pin) is the
integrations bridge: GitHub, GitLab, Jira, RSS and Atom feeds, and plain webhooks, each posting
into a room. It is unlike a mautrix bridge in one way that shapes everything here: **it is one
shared bridge for the whole server with many connections**, not one bridge per person with one
account each. Nobody signs in to it. A room connects what it wants, and the offering's page
lists those connections.

Since 2026-10-20 the server deploys it like any other offering (RFC 0017): the catalogue entry
`matrix-hookshot` is `deployable`, its `config.yml` is rendered whole from the offering's
settings, the operator runs it as `Bridge/bridge-hookshot`, and the one thing an administrator
has to bring is a public URL that reaches its webhooks listener.

## Offering it

In the interface: Bridges → Offer a bridge → Hookshot. The Options step asks for:

| Setting | What it is | Where it goes |
|---|---|---|
| **Public URL** (required) | A URL the outside reaches the bridge's webhooks listener on, `https://hooks.example.org` (a path is fine: `https://example.org/hookshot`). | `generic.urlPrefix` is this URL + `/webhook/`: the start of every webhook URL the bridge hands out. GitHub Apps post to this URL + `/github/webhook`. |
| **GitHub App** (optional: App ID, webhook secret, private key) | A GitHub App the bridge acts as, for connecting repositories properly (`!hookshot github repo org/repo`). All three parts or none. | `github.auth.id`, `github.webhook.secret`, and the key as `/data/github-key.pem`. |
| **Who can have one** | Everyone on the server, or the people listed. | Hookshot's `permissions`: each actor at level `admin` for every service (commands, logging in to GitHub, connecting and removing connections in any room). |

The same from the admin API:

```sh
curl -X PUT "$HS/api/v1/bridge-offerings/matrix-hookshot" -H "authorization: Bearer $TOKEN" \
  -H 'content-type: application/json' \
  -d '{"runtime": "cluster", "options": {"public_url": "https://hooks.example.org"}}'
```

A `PUT` without `options.public_url` (or with one that is not `http(s)://...`) is refused at
`/options/public_url`; half a GitHub App is refused at `/options/github`. The encryption, double
puppeting and backfill options mean nothing to Hookshot and are ignored.

## What the server renders

The files (`POST /bridge-offerings/matrix-hookshot/instances/_/files`, or the Files button on
the offering's page; the `files` map in the answer has every one by its name in `/data`):

- `config.yml`, what the image's own command reads (`node /bin/matrix-hookshot/App/BridgeApp.js
  /data/config.yml /data/registration.yml`): `bridge` (this server's name, the address the
  bridge reaches it at, `mediaUrl` = the server's public base URL, port 9993, bound on every
  interface), `passFile: /data/passkey.pem`, `permissions`, `generic` (enabled, the URL prefix,
  `userIdPrefix: _webhooks_`, JavaScript transformation functions allowed), `feeds` (enabled, a
  poll every ten minutes), `github` when the offering has an App, `metrics`, and two listeners:
  **webhooks on 9000** and **metrics on 9001**. No `encryption` (it needs Redis) and no
  `provisioning` (removed in Hookshot 7; a config with it is refused).
- `registration.yml`: the bot `@hookshot:server` and the ghost prefixes every service makes
  users under, all exclusive: `@hookshot_.*`, `@_github_.*`, `@_gitlab_.*`, `@_jira_.*`,
  `@_webhooks_.*` and `@feeds:server`, as Hookshot's own `registration.sample.yml` claims them.
  `de.sorunome.msc2409.push_ephemeral: true`.
- `passkey.pem`: the RSA private key Hookshot encrypts the tokens it keeps with. Hookshot does
  not generate one (it stops at start without the file), so the server mints a 4096-bit key with
  the instance's tokens and **keeps it on the instance's row**, the way a mautrix bridge's
  `pickle_key` is kept: every render writes the same key, and a roll never makes the bridge's
  stored GitHub logins unreadable. (`hs_bridges::passkey`; an instance from before this was
  kept has none, and the compose text says how to make one with `openssl genpkey`.)
- `github-key.pem`, when the offering has a GitHub App.

The `Bridge` the manager writes names the two extra listeners:

```yaml
spec:
  port: 9993
  extraPorts:
    - name: webhooks
      port: 9000
    - name: metrics
      port: 9001
```

and the operator puts each on the container and on the Service `bridge-hookshot` under its
name and number, after the `appservice` port. A cluster whose Bridge CRD predates `extraPorts`
refuses the `Bridge` (the instance's page says to apply `deploy/crds/bridge.yaml` or upgrade the
chart); a `Bridge` without extra ports is unaffected.

## Reaching the webhooks listener from outside

The operator deploys the Service; the public URL is the administrator's. Point an Ingress for
the public URL's host at `bridge-hookshot:webhooks` (port 9000). With the Tailscale operator and
a Funnel, as the demo does (`deploy/demo/hookshot/bridge.yaml` has the hand-made version):

```yaml
apiVersion: networking.k8s.io/v1
kind: Ingress
metadata:
  name: hookshot-funnel
  namespace: myelin
  annotations:
    tailscale.com/funnel: "true"
spec:
  ingressClassName: tailscale
  defaultBackend:
    service:
      name: bridge-hookshot
      port:
        name: webhooks
  tls:
    - hosts:
        - hookshot
```

With any other ingress controller, an ordinary Ingress whose host is the public URL's host (and
whose path, when the public URL has one, is that path, rewritten away: Hookshot serves webhooks
at `/webhook/...` and `/github/webhook` from the root of its listener) with the same backend. The
metrics port is the pod's and needs no Ingress: a ServiceMonitor or a scrape of
`bridge-hookshot:metrics` reads `hookshot_*`.

A bridge run elsewhere (`runtime: elsewhere`, the Compose service in the files) publishes 9000
itself; the compose text says so.

## Connecting things

Everything is done in rooms, by anyone the offering allows. Invite `@hookshot:server` to a room
and make sure it may send state events (the Moderator level in most clients: each connection is
a state event). Then:

- **A plain webhook**: `!hookshot webhook <name>`. The bot answers with a URL under the public
  URL (`https://hooks.example.org/webhook/<secret id>`). Anything that can post JSON to a URL
  then posts into the room: a GitHub or GitLab repository webhook, a monitoring alert, a script.
  A payload's `text` is the message; `username` names who it is from; anything else is shown as
  data, or shaped by a transformation function (`!hookshot webhook <name> ...`; the demo's
  `deploy/demo/hookshot/github-comments.js` turns GitHub comment and review events into one
  notice each).
- **A GitHub repository through plain webhooks** (no GitHub App needed): make the room's
  webhook, then on GitHub, Settings → Webhooks → Add webhook, the URL the bot gave, content type
  `application/json`, the events wanted.
- **A GitHub repository through the App** (the offering has one): `!hookshot github repo
  org/repo` after the App is installed on the repository. Issues, pull requests, reviews,
  releases and workflow runs arrive; the people who commented are ghost users
  (`@_github_<login>:server`).
- **A feed**: `!hookshot feed <url>`; `!hookshot feed list`. Without Redis the bridge forgets
  seen entries over a restart and starts from the feed's current state.
- `!hookshot help` lists the rest (GitLab and Jira need their sections in the config, which the
  rendered one does not write yet).

## Making a webhook from the server

Since 2026-10-20 (decision 0056) a webhook is also made from the offering's page or the admin
API, for one of the **sources** below: the server acts as the bridge's bot and does exactly
what `!hookshot webhook <name>` does, with the source's transformation function written into
the connection. One Hookshot, one public URL, many webhooks, each in its room.

In the interface: Bridges → Hookshot → Connections → **Add a webhook**: pick the source (what
will post to it), the room (one the bot is in and may send state events to, or a new private
room the bot makes and invites people to), and a name (3 to 64 characters, Hookshot's limit).
The next screen is the one time the URL is shown: copy it, with the sender's setup under it
(and, for Gmail, the Apps Script with the URL already in it). Each listed connection has a
**Remove**, confirmed first.

The same from the API:

```sh
curl -X POST "$HS/api/v1/bridge-offerings/matrix-hookshot/connections" \
  -H "authorization: Bearer $TOKEN" -H 'content-type: application/json' \
  -H "idempotency-key: $(uuidgen)" \
  -d '{"source": "datadog", "name": "datadog-alerts",
       "room": {"create": {"name": "Alerts", "invite": ["@alice:example.org"]}}}'
```

answers `201` with `{room_id, name, source, url, setup, files}`; `"room": {"id": "!..."}`
uses a room the bot is already in instead. `GET .../webhook-sources` lists the sources with
their setup text; `DELETE .../connections/{room_id}/{state_key}` removes any connection (a
webhook's, a repository's, a feed's) the way the bridge does. Both writes are audited
(`bridge_connection.created`, `bridge_connection.deleted`: the room, the name and the source,
never the URL), need `bridges:write`, and are counted in
`hs_bridge_webhook_connections_total{bridge_type,source,outcome}` (`created`, `removed`,
`failed`).

What the server writes, in this order (read from Hookshot 7.5.0's
`GenericHookConnection.createConnectionForState`):

1. The room, if asked: `POST /createRoom` as the bot (`private_chat`, the name, topic and
   invites), so the bot is its creator and may write its state. For an existing room, the bot
   has to be in it already (invite `@hookshot:server`) and allowed to send state events.
2. A hook id: a version 4 UUID, as Hookshot mints with `randomUUID()`.
3. The bot's room account data of type `uk.half-shot.matrix-hookshot.generic.hook`, with
   `{"<hook id>": "<name>"}` merged in. **Before the state event**: Hookshot reads the mapping
   when it sees the event and, for an event its own bot sent, refuses to make an id up.
4. The state event `uk.half-shot.matrix-hookshot.generic.hook`, state key = the name, content
   `{"name": ..., "transformationFunction": <the source's>, "io.myelin.source": "<source>"}`
   (Hookshot reads the keys it knows and ignores the rest; the marker is how the listing says
   the source). The bridge picks the connection up live from its appservice transaction
   (`Bridge.onRoomEvent` → `createConnectionForState`): no restart, and the URL works from
   the moment the answer arrives.
5. A notice in the room, without the URL.

Removing empties the state event (the bridge purges the connection when it sees that, as its
own remove does, and writes `{"disabled": true}` back into the event, which the listing reads
as `removed` too) and then drops the hook id from the account data, so the URL stops working
even if the bridge missed the event. A removed name can be used again. The metric's `via`
label says which path asked (`admin` here, `user` for the page every user has, below).

The secret is in the bridge's account data and nowhere the admin API reads back: the create
answer is the one place it is shown, the listing never has it, the audit log never has it.

## For everyone: `/integrations/` and the user API

Since 2026-10-21 (decision 0057) a person on the server adds a webhook to a room of their own
without an administrator. The page is **`https://<server>/integrations/`**, served by the
binary from the same build as `/admin/`, signed in with the person's own Matrix account (the
ordinary `/login`; nobody needs to be an administrator): **My webhooks** lists the webhooks in
the rooms they are in (who made each, when); **Add a webhook** is the same three steps as the
admin page (what will post, which room, a name) and then the URL once with the sender's setup
(and `Code.gs` for Gmail); **Remove** asks first. The rooms offered are the person's joined
rooms that can take one; the others say why not (the level it takes, or that the room is
encrypted). An administrator sends people there: the offering's page links to it, and the
admin sign-in says so for someone who is not an administrator.

Behind it is a client-server extension under an unstable namespace, authenticated by the
person's access token like any client endpoint (not `/api/v1`, whose verifier refuses a
non-administrator's token by design), and not part of the admin OpenAPI:

| Route | What it does |
|---|---|
| `GET /_matrix/client/unstable/io.myelin.webhooks/sources` | The sources, as the admin API lists them (`{items: [...]}`). |
| `GET .../rooms` | `{rooms: [{room_id, name, bot_joined, may_add, reason}]}`: the caller's joined rooms (up to 200), each saying whether a webhook may be added there and, if not, why. |
| `GET .../webhooks` | `{items: [{room_id, room_name, name, source, created_by, created_at}]}`: the active webhook connections in rooms the caller is joined to. Never the URL. |
| `POST .../webhooks` | `{source, name, room: {id} \| {create: {name, topic?}}}` → `201` with `{room_id, name, source, url, setup, files}`, the URL's one showing. |
| `DELETE .../webhooks/{room_id}/{name}` | Removes a webhook the way the bridge does; `200 {}`. |

Errors are Matrix errors: `M_FORBIDDEN` (not in the room, not enough power, encrypted, may
not invite the bot), `M_NOT_FOUND` (no Hookshot offered, none with a public URL, the bridge
not registered yet, no such webhook: each says so and says to ask an administrator),
`M_INVALID_PARAM` (a bad source, name or room, naming the field), `M_LIMIT_EXCEEDED` with
`retry_after_ms` (ten creations at once per user, one more every six minutes), `409` on a
name the room already uses.

```sh
curl -X POST "$HS/_matrix/client/unstable/io.myelin.webhooks/webhooks" \
  -H "authorization: Bearer $MY_ACCESS_TOKEN" -H 'content-type: application/json' \
  -d '{"source": "linear", "name": "linear-issues", "room": {"id": "!mine:example.org"}}'
```

What the server does for an existing room, in this order, from the room's current state as
the caller sees it:

1. The caller is a joined member with the power to send the connection's state event
   (`events["uk.half-shot.matrix-hookshot.generic.hook"]`, else `state_default`, usually 50:
   the Moderator level); the room is not encrypted (Hookshot here runs without encryption
   and would post in the clear); otherwise `M_FORBIDDEN` saying which.
2. If `@hookshot:server` is not in the room: the caller may invite, the server invites it
   **as the caller**, and the bot joins.
3. If the bot is below the level the event takes: the caller can change power levels (and
   is at least that level), and the server raises the bot to exactly that level, as the
   caller. A room the person made in their client passes all of this: they are at 100.
4. The connection, exactly as the admin API makes one (the hook id in the bot's account data
   first, then the state event, then the notice), with `io.myelin.created_by` and
   `io.myelin.created_at` in the state event (Hookshot ignores keys it does not know; the
   listings read them back), and the notice naming the maker.

For a new room: the bot makes it (private, the name and topic given), invites the caller,
gives them power 100 in it, and the server **joins the caller as themselves**, so it is in
their client at once rather than in an invite list. Every make and remove is an entry in the
admin audit log with the user as actor (`bridge_connection.created`/`.deleted`, never the
URL), a line at INFO, and a count in `hs_bridge_webhook_connections_total{via="user"}`
(`via="admin"` for the admin API).

**The bot's own command keeps working.** Invite `@hookshot:server` to a room and send
`!hookshot webhook <name>`: the bot answers with a URL in its chat with you, and what posts
to it is shown as Hookshot renders it (`text`, `username`, otherwise the data), with no
transformation function. `/integrations/` is the way to get the library's formatting for a
sender.

## Sources

Each source is a directory in `crates/hs-bridges/hookshot-sources/<id>/`: `README.md` (the
sender's setup, which the create answer returns with the URL filled in), `transform.js` (the
transformation function Hookshot runs on every payload: `data` is the decoded body, the script
sets `result = {version: "v2", plain, msgtype}` or `{version: "v2", empty: true}`; QuickJS,
500 ms, no network; Markdown in `plain` is rendered by Hookshot), and `sample-*.json` payloads
that `cargo test -p hs-bridges hookshot_sources` runs the transform on with `node` (skipped,
saying so, without `node`). A new source is a new directory and an entry in
`hookshot_sources::SOURCES`.

### `generic`: plain JSON

No transformation: Hookshot's own rendering. `text` is the message (Markdown), `username` is
shown in bold before it, `html` is the message's HTML, anything else is posted as data
("Received webhook data:" and the JSON). A form or plain-text POST works too. For a script, a
cron job, or a service whose payload already is a message.

### `alertmanager`: Prometheus Alertmanager

The webhook receiver (`webhook_configs: [{url, send_resolved: true}]` on a receiver, and a
route to it). One notice per notification: `**FIRING** 2 firing (alertname=HighLatency,
namespace=myelin)` linked to the Alertmanager, then a line per alert with its severity,
summary (`summary`, else `description`, else `message`), its `instance`/`job`/`namespace`/
`pod`/`service` labels, and links to its source and runbook; `**RESOLVED** ...` when the group
resolves; `... and N more` when `max_alerts` cut some off. An empty `alerts` is dropped.

### `datadog`: Datadog monitors

Datadog's Webhooks integration with a custom payload template (the README gives it:
`$EVENT_TITLE`, `$TEXT_ONLY_MSG`, `$ALERT_TRANSITION`, `$PRIORITY`, `$ALERT_TYPE`,
`$HOSTNAME`, `$ALERT_SCOPE`, `$TAGS`, `$LINK`, `$ALERT_ID`, `$DATE`), and
`@webhook-<name>` in each monitor's message. `**Alert** P2: [title](link)`, the message, then
`host web-1 · scope host:web-1 · tags env:prod service:web`; `**Recovered**`, `**Warning**`,
`**No data**` by the transition. Datadog's **Test** button sends a payload without the
template's values, which is dropped.

### `github-comments`: GitHub comments

The demo's transformation function, moved here (`deploy/demo/hookshot/github-comments.js` is
a pointer). A plain repository webhook (no GitHub App) for the events **Issue comments**,
**Pull request review comments** and **Pull request reviews**: `**login** commented on PR
[#12 Title](link) on `path`:` and the comment; `**login** approved PR [#12 Title](link)`.
GitHub's ping lands as "GitHub webhook for owner/repo is connected."; every other event is
dropped.

### `gmail`: Gmail, through Apps Script

Gmail has no outgoing webhooks, so the source ships `Code.gs`, a Google Apps Script (the create
answer's `files` carries it with the URL in it): on a time-based trigger it posts every thread
under one Gmail label (`to-matrix`, applied by a Gmail filter) once, as `{from, subject,
snippet, link, labels, date}`, and labels it `to-matrix/posted`. `**Ada Lovelace**:
[Subject](link to the thread)`, the first 240 characters of the latest message, the thread's
other labels. `testWebhook` posts "Gmail script connected"; `install` makes the trigger. Only
the sender, subject, snippet and link leave the mailbox.

### `linear`: Linear

Linear's webhooks for **Issues**, **Comments** and **Projects**: `**Ada** moved to **In
Progress**: [ENG-42 Title](url)`, created (with state, assignee, priority), assigned,
renamed, priority/due date/estimate changed, deleted; comments made, edited, deleted with
their text; projects created, their state/name/lead/target date changed, deleted. Updates that
change nothing the room would show (a description edit, a label) and the other event types
are dropped. Linear signs deliveries (`Linear-Signature`); **Hookshot's generic webhook does
not verify it**: the URL's last part is the only secret, and the README says so.

### `notion`: Notion

Notion's webhook subscriptions carry ids and a type, not content, so the notice is `Notion:
page content changed · <id> · 2026-10-20 10:00:10 UTC · [open](notion.so/<id>)` (and how many
blocks changed; for a comment, a link to its page). The subscription's first delivery is its
**verification token**, which the room shows to be pasted back into Notion. Notion's
`X-Notion-Signature` is not verified by Hookshot either.

## Seeing what is connected

Hookshot 7 has no provisioning API (the `provisioning` config section is refused; `provisioning`
is no listener resource, only `webhooks`, `widgets` and `metrics`), so the server reads the
connections from the rooms: `GET /api/v1/bridge-offerings/matrix-hookshot/connections` lists
the bot's rooms (as the bot, with the instance's token) and every `uk.half-shot.matrix-hookshot.*`
state event in them, one entry each: `service` (`webhook`, `github`, `gitlab`, `jira`, `feed`,
...), `event_type`, `state_key`, `room_id`, `room_name`, `name` (a webhook's `name`, a
repository's `org/repo`, a feed's URL), `state` (`active`, or `removed` for an event that has
been emptied or that the bridge has purged with `{"disabled": true}`) and `source` (the webhook source a connection made through the server
was made from; none for one made in the room). The answer's `rooms` are the rooms the bot is
in, connections or not: what "Add a webhook" offers. The offering's page shows this table under
the bridge's panel.

A webhook's secret id is **not** in the state event: the bridge keeps it in its own room
account data (type `uk.half-shot.matrix-hookshot.generic.hook`, hook id to name), so it is not
in this list either; the URL is in the room, where the bot said it.

## Verified

- `cargo test -p hs-admin hookshot`: the rendered config and registration (every line above),
  the files, the extra ports, the old wizard's placeholder.
- `cargo test -p hs-bridges hookshot`: an offering refused without a public URL, with a bare
  host, or with half a GitHub App; deployed as `bridge-hookshot` with its minted passkey, the
  same key on every render, the extra ports in the `Bridge`; the people list and the GitHub App
  reaching the config; the connections read from the fake server's room state.
- `cargo test -p hs-operator`: extra ports on the container and the Service, the CRD's schema,
  a port name Kubernetes would refuse.
- `cargo test -p hs-bridges hookshot_sources`: every source's transform on every sample with
  `node` (the Alertmanager group's two alerts and its resolved notice, Datadog's alert,
  recovery and empty test, GitHub's comment, review, ping and dropped push, Gmail's thread,
  test and empty, Linear's move, comment, project and dropped label, Notion's verification
  token, page, comment and dropped unknown); each transform compiles as Hookshot checks it
  and uses nothing QuickJS lacks; the setup and files filled in; a hook id a v4 UUID.
- `cargo test -p hs-bridges user_webhooks`: the rules (the room's creator may add and the
  bot is invited and raised; a bot already in with power needs nothing; a plain member is
  refused with the level it takes; the event type's own level over `state_default`, and a
  grant the caller cannot make; not in the room; an encrypted room; the invite level; a
  version 12 creator's infinite power; a room without a power levels event; active webhooks
  read and removed ones not; power levels rewritten with the bot kept), the limiter (the
  burst, the refusal with its wait, a second user's own bucket, the refill), the manager's
  refusals as Matrix errors.
- `cargo test -p hs-cli --test user_webhooks` (in-process server, 3 s): without a token
  `401`; before the offering `404` saying to ask an administrator; after it the sources;
  alice's room offered with `may_add`, Bob's view of it refused with "power level 50"; Bob's
  `POST` `403`; alice's `201` with the URL, the bot joined and at 50, the state event with
  her name and the source, the notice naming her without the URL; the same name `409`; a new
  room joined by her at 100 with its name and topic; her two listed with `created_by`, Bob's
  one; the audit log with her as actor (`kind: user`) and no secret; the metric with
  `via="user"`; Bob's `DELETE` `403`, hers `200` and the event emptied, again `404`; an
  encrypted room `403` saying so; a bad name `M_INVALID_PARAM` at `/name`; the eleventh
  creation `M_LIMIT_EXCEEDED` with `retry_after_ms`; `/integrations/` served.
- `cargo test -p hs-bridges connection`: a connection made as the bot against a fake server
  (the room with its name, topic and invites; the account data written before the state
  event; the transformation function and `io.myelin.source` in the content; the notice; the
  conflict on a name in use; a refusal at each field; the listing with the source and the
  bot's rooms, without the id); removed (the event emptied, the id dropped, the other
  webhook's kept, not found twice); the name reused with a fresh id; the metric.
- `crates/hs-bridge-conformance/tests/real_hookshot.rs`, **run against the real image on
  2026-10-20** (58 s): the real `hs` binary offers Hookshot with a public URL, renders the
  files, the 7.5.0 image runs from exactly them (`Loading token key file /data/passkey.pem`,
  the two listeners, `Bridge is now ready`) and the instance is `ready`; alice invites the bot,
  sends `!hookshot webhook test`, the bot writes the connection's state event, confirms in the
  room and says the secret URL in its admin room (the chat it opens with her, which the test
  joins); a POST of `{"text": ...}` to that URL lands in the room; and
  `GET /bridge-offerings/matrix-hookshot/connections` lists the webhook, active, in the room
  "Deploys". Then (**2026-10-20, evening, 14 s for both halves**) the other way round: a
  webhook made through the API from the `datadog` source in a new room the bot creates and
  invites alice to; the bridge picks the connection up live from the state event (no
  restart); Datadog's sample posted to the URL lands as the source's notice with its Markdown
  rendered; the listing says `source: datadog`; `DELETE` makes the URL answer an error within
  a second and the listing say `removed` (Hookshot writes `{"disabled": true}` back, which
  the listing reads as removed). Then (**2026-10-21, 14.5 s for all three parts**) as a
  plain user: alice makes a room "Mine" in her client; `GET .../io.myelin.webhooks/rooms`
  offers it (`may_add`, the bot not in it); `POST .../webhooks` with her own token, the
  `linear` source and that room answers `201` with the URL; the bot is in the room and at
  power 50, the state event names her and the source; Linear's `sample-issue-update.json`
  posted to the URL lands within a second as `**Ada Lovelace** moved to **In Progress**:
  [ENG-42 ...]` with the Markdown rendered (the real bridge picked the connection up live
  from the state event, as for an administrator's); `GET .../webhooks` lists it with
  `created_by` her and no secret; the admin audit log has her as actor; `DELETE` with her
  token makes the URL answer an error within a second and the listing drop it. Skipped,
  saying why, without Docker, the image or the binary. On the owner's
  desktop `docker pull` from ghcr.io reaches for the keychain and fails in a session; pulling
  with a `DOCKER_CONFIG` that has no credential store **and** a `PATH` without
  `docker-credential-osxkeychain` works anonymously (the CLI falls back to the helper whenever
  it can find one).
