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

## Seeing what is connected

Hookshot 7 has no provisioning API (the `provisioning` config section is refused; `provisioning`
is no listener resource, only `webhooks`, `widgets` and `metrics`), so the server reads the
connections from the rooms: `GET /api/v1/bridge-offerings/matrix-hookshot/connections` lists
the bot's rooms (as the bot, with the instance's token) and every `uk.half-shot.matrix-hookshot.*`
state event in them, one entry each: `service` (`webhook`, `github`, `gitlab`, `jira`, `feed`,
...), `event_type`, `state_key`, `room_id`, `room_name`, `name` (a webhook's `name`, a
repository's `org/repo`, a feed's URL) and `state` (`active`, or `removed` for an event the
bridge has emptied). The offering's page shows this table under the bridge's panel.

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
- `crates/hs-bridge-conformance/tests/real_hookshot.rs`, **run against the real image on
  2026-10-20** (58 s): the real `hs` binary offers Hookshot with a public URL, renders the
  files, the 7.5.0 image runs from exactly them (`Loading token key file /data/passkey.pem`,
  the two listeners, `Bridge is now ready`) and the instance is `ready`; alice invites the bot,
  sends `!hookshot webhook test`, the bot writes the connection's state event, confirms in the
  room and says the secret URL in its admin room (the chat it opens with her, which the test
  joins); a POST of `{"text": ...}` to that URL lands in the room; and
  `GET /bridge-offerings/matrix-hookshot/connections` lists the webhook, active, in the room
  "Deploys". Skipped, saying why, without Docker, the image or the binary. On the owner's
  desktop `docker pull` from ghcr.io reaches for the keychain and fails in a session; pulling
  with a `DOCKER_CONFIG` that has no credential store **and** a `PATH` without
  `docker-credential-osxkeychain` works anonymously (the CLI falls back to the helper whenever
  it can find one).
