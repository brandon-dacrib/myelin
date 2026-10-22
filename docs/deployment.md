# Deployment guide

How to run Myelin for other people: on Docker, on a host from the released binary, or on
Kubernetes; behind a reverse proxy with a certificate; federating; backed up; monitored; and
upgraded. Each section says what was verified and how. The Kubernetes specifics (every Helm
command, in order) are in [install-and-upgrade.md](install-and-upgrade.md); the settings
reference is [config.md](config.md); the admin interface, from the first sign-in on, is
[user-guide.md](user-guide.md).

Contents:

- [Before you start](#before-you-start)
- [Choose a layout](#choose-a-layout)
- [Docker](#docker)
- [A host, from the released binary](#a-host-from-the-released-binary)
- [Kubernetes](#kubernetes)
- [Settings: where they live and how to change them](#settings-where-they-live-and-how-to-change-them)
- [Listeners, TLS and the reverse proxy](#listeners-tls-and-the-reverse-proxy)
- [Federation](#federation)
- [Email](#email)
- [Hardening checklist](#hardening-checklist)
- [Backup and restore](#backup-and-restore)
- [Monitoring](#monitoring)
- [Upgrading and rolling back](#upgrading-and-rolling-back)
- [Migrating from Synapse](#migrating-from-synapse)
- [Troubleshooting](#troubleshooting)

## Before you start

You need:

- **A server name.** The part after the colon in user IDs (`@alice:example.org`). It is baked
  into every event the server signs and **cannot be changed later**, on this or any other
  homeserver. Pick the domain you want people's IDs to carry, usually the bare domain
  (`example.org`), even when the server itself will live at `matrix.example.org`.
- **A public address** people's clients reach the server at (`https://matrix.example.org`),
  with a DNS name and a certificate. Everything in this guide assumes a reverse proxy or an
  Ingress terminates TLS in front of the server; the server can also terminate TLS itself
  ([Listeners, TLS and the reverse proxy](#listeners-tls-and-the-reverse-proxy)).
- **Somewhere to keep the data.** A single-node server keeps its database, signing key and
  media in one directory or volume. Budget for media: that is what grows.
- **Memory.** 1 GiB is enough for a small server; the server keeps rooms resident while they
  are in use and unloads them under memory pressure, so give it a memory limit it can read
  (a container limit) and it will stay inside it. The standing demo, which federates with
  matrix.org rooms and runs six bridges, runs with a 1.5 GiB limit.

What you get out of the box: registration closed, rate limits on, federation on, link previews
off, `/metrics` off, every other setting a sane default that the admin interface explains in
place. There is no configuration file to write for a single-node server.

## Choose a layout

| Layout | When | Data | Guide section |
|---|---|---|---|
| **Single node, embedded database** | One machine or one pod; most servers | One directory or volume: `db/`, `keys/`, `media/` | [Docker](#docker), [a host](#a-host-from-the-released-binary), [Kubernetes](#kubernetes) |
| **Single node, PostgreSQL** | You already run PostgreSQL and want its backups and tooling | PostgreSQL plus a media directory plus the key | `storage.backend: postgres` in the bootstrap file (`config.md`, section `storage`); otherwise as single node |
| **Cluster** | Several replicas behind one Service for availability and spread | PostgreSQL, media on S3 or a shared volume, one signing-key Secret, a mesh certificate | [install-and-upgrade.md, cluster mode](install-and-upgrade.md#cluster-mode-several-replicas-postgresql); [scaling.md](scaling.md) says what a replica buys |

Start single node unless you know you need the cluster. The embedded database is Fjall, in
process; a cold first boot takes under a second, and the whole server is one directory to copy.

## Docker

The image is `ghcr.io/brandon-dacrib/myelin`, built for `linux/amd64` and `linux/arm64`, on a
distroless base with no shell, running as a non-root user (uid 65532). Tags: `X.Y.Z` is a
release, `X.Y` its line, `latest` the newest release, `main` the newest merge, `sha-<commit>`
one commit. **Pin a release**; the examples below use `V`.

### One container

```sh
V=0.3.32      # https://github.com/brandon-dacrib/myelin/releases
docker run -d --name myelin --restart unless-stopped \
  -p 8008:8008 -v myelin:/data \
  -e HS__SERVER__SERVER_NAME=example.org \
  -e HS__SERVER__PUBLIC_BASEURL=https://matrix.example.org \
  ghcr.io/brandon-dacrib/myelin:$V
```

That is a working server. The `myelin` volume holds the database, the signing key (generated
on the first start) and media. `HS__SERVER__PUBLIC_BASEURL` is the address clients will use;
set it now so the setup link and the `.well-known` documents are right from the first start.

Make the first administrator. The log ends with a one-time setup link, offered at every start
until somebody uses it:

```sh
docker logs myelin 2>&1 | grep setup_link
```

Open it, choose a username and password, and you are signed in to the admin interface at
`/admin/`. Only someone who can read the log can get this link. The
[user guide](user-guide.md#1-first-sign-in) continues from here.

Put the reverse proxy in front ([below](#listeners-tls-and-the-reverse-proxy)), check
federation ([below](#federation)), and you are done.

### Docker Compose, with a reverse proxy

`deploy/compose/` is a complete two-service example: the server and
[Caddy](https://caddyserver.com), which gets a Let's Encrypt certificate for
`matrix.example.org` by itself and forwards to the server. It mounts a small bootstrap file so
the server trusts the proxy's `X-Forwarded-For` (otherwise every client looks like the proxy to
the per-address rate limits).

```sh
git clone https://github.com/brandon-dacrib/myelin && cd myelin/deploy/compose
# edit compose.yaml, Caddyfile and hs.yaml: replace example.org and matrix.example.org
MYELIN_VERSION=0.3.32 docker compose up -d
docker compose logs hs | grep setup_link
```

The files are commented; `docker compose config` validates them. If `example.org` is not the
same host as `matrix.example.org`, whatever serves `https://example.org` must also serve the
two `/.well-known/matrix/*` documents, or reverse-proxy that path to the server
([Federation](#federation) has the details).

### Inside the container

- `docker exec myelin hs recover` prints a one-time link that gets an administrator back in
  when nobody can sign in ([recovery.md](recovery.md)).
- `docker exec myelin hs config show` prints the effective configuration and where each value
  came from, and `hs config set <path> <value>` changes one without a browser, **with the
  container stopped**: the embedded database is held by the running server (the command says
  `Locked` otherwise). While it runs, use the interface or the admin API.
- There is no shell in the image. To look at the volume, mount it into another container
  (`docker run --rm -v myelin:/data alpine ls -la /data`).

A stop sends `SIGTERM`; the server withdraws readiness, finishes in-flight requests and exits.
`docker stop` waits ten seconds by default, which is enough.

## A host, from the released binary

Every release carries a static binary for `x86_64-unknown-linux-gnu`,
`aarch64-unknown-linux-gnu` and `aarch64-apple-darwin`, with a SHA-256 beside it. CD boots each
one and checks it serves the admin interface and logs a setup link before the release is made.

```sh
V=0.3.32
T=x86_64-unknown-linux-gnu                           # or aarch64-unknown-linux-gnu, aarch64-apple-darwin
curl -LO "https://github.com/brandon-dacrib/myelin/releases/download/v$V/myelin-v$V-$T.tar.gz"
curl -LO "https://github.com/brandon-dacrib/myelin/releases/download/v$V/myelin-v$V-$T.tar.gz.sha256"
shasum -a 256 -c "myelin-v$V-$T.tar.gz.sha256"
tar xzf "myelin-v$V-$T.tar.gz"
sudo install -m 0755 "myelin-v$V-$T/hs" /usr/local/bin/hs
hs version
```

Run it as its own user, with one data directory:

```sh
sudo useradd --system --home /var/lib/myelin --create-home --shell /usr/sbin/nologin myelin
sudo -u myelin hs serve --data-dir /var/lib/myelin --server-name example.org
```

The data directory gets `db/`, `keys/` and `media/`; the server name is recorded in the database
on the first start, so later starts need only `--data-dir`. The setup link is in the output.

### As a systemd service

`deploy/systemd/myelin.service` is a unit for this layout: it runs `hs serve` as the `myelin`
user with `HS_DATA_DIR=/var/lib/myelin`, reads `HS__` variables from
`/etc/myelin/environment`, restarts on failure and applies systemd's sandboxing (private
`/tmp`, read-only system directories, no new privileges).

```sh
sudo install -d -o myelin -g myelin /var/lib/myelin
sudo install -d /etc/myelin
printf 'HS__SERVER__SERVER_NAME=example.org\nHS__SERVER__PUBLIC_BASEURL=https://matrix.example.org\n' | sudo tee /etc/myelin/environment
sudo cp deploy/systemd/myelin.service /etc/systemd/system/
sudo systemctl daemon-reload
sudo systemctl enable --now myelin
journalctl -u myelin | grep setup_link
```

To serve anything beyond one plaintext listener on 8008 (TLS, a second port for the admin
interface, trusting a proxy's forwarded headers), write a bootstrap file and point the unit at
it: `hs generate-config --server-name example.org --data-dir /var/lib/myelin -o /etc/myelin/hs.yaml`
writes the few settings read before the database opens, add a `listeners:` section
([below](#listeners-tls-and-the-reverse-proxy)), and change the unit's `ExecStart` to
`hs serve -c /etc/myelin/hs.yaml`.

`hs recover --data-dir /var/lib/myelin` gets a locked-out administrator back in while the
server runs. `hs config --data-dir /var/lib/myelin show` prints the configuration, with the
service stopped: the embedded database is held by the running server.

## Kubernetes

One value installs it:

```sh
V=0.3.32
helm install myelin oci://ghcr.io/brandon-dacrib/charts/hs --version "$V" \
  -n myelin --create-namespace --set serverName=example.org --wait
kubectl -n myelin logs statefulset/myelin-hs | grep setup_link
```

**[install-and-upgrade.md](install-and-upgrade.md) is the Kubernetes guide**: the smallest
install, a server on the internet (Ingress, cert-manager, ServiceMonitor), cluster mode with
PostgreSQL, upgrade, rollback, uninstall and the one-time steps for old installs, each verified
on a real cluster with the published chart. The chart's own README (`deploy/helm/hs/README.md`)
covers the Bridge CRD, tracing, alerts, what a disruption costs, backup and restore, and rotating
the mesh certificates. The operator that deploys bridges as pods is part of the chart.

CD installs every release's chart on a kind cluster, waits for Ready and makes the first
administrator through the setup link before the release is tagged.

## Settings: where they live and how to change them

Myelin is administered through its admin interface and API, not through files (decision 0010).
Every setting is one of two kinds:

- **Bootstrap** settings are the few a process needs before it can open the database: where
  the database is, the listeners, the server name, the signing key's path, a replica's cluster
  identity. They come from the bootstrap file (`hs serve -c`), `HS__` environment variables or
  the command line, are never stored in the database, and are shown read-only in the interface.
- **Administered** settings, everything else, live in the database. Change them in the
  interface's Configuration section, with `PATCH /api/v1/config/{section}` or with
  `hs config set`. Each keeps its history and can be reverted. Each is marked **hot** (applies
  to the running server at once) or **restart** (stored now, read at the next start); the
  interface shows which on every setting, and [config.md](config.md) lists them all.

Precedence, lowest to highest: the bootstrap file, then the database, then `HS__` variables.
A file's administered settings seed the database on the first start only, so changing the file
later does not undo what was changed in the interface. An `HS__` variable pins its setting:
the interface shows it read-only, because an edit would be undone at the next start.

`HS__SECTION__FIELD` names one setting (`HS__AUTH__ENABLE_REGISTRATION=true`,
`HS__RATE_LIMITS__LOGIN__PER_SECOND=0.5`); the value is a YAML scalar. A list or a map (the
listeners, trusted key servers) cannot be set from the environment; put it in the bootstrap
file, or in the interface.

Without a browser: `hs config show` prints the effective configuration with each value's
origin, `hs config set auth.enable_registration true` stores one, `hs config export` dumps the
stored configuration for backup and `hs config import` replays it. On the embedded database
these need the server stopped (it holds the database; the command says `Locked`); on
PostgreSQL they work alongside it.

## Listeners, TLS and the reverse proxy

By default the server binds one listener on port 8008 on every address, plaintext, serving
clients, federation, media, health checks, the admin interface and, once enabled, `/metrics`.
Put a reverse proxy in front of it that terminates TLS and forwards everything.

The listeners are a bootstrap setting, so anything else is a bootstrap file:

```yaml
# /etc/myelin/hs.yaml (hs serve -c /etc/myelin/hs.yaml), or mounted into the container
server:
  server_name: example.org
listeners:
  listeners:
    - port: 8008
      bind_addresses: ["127.0.0.1"]       # only the proxy on this host reaches it
      resources: [client, federation, media, health, admin]
      x_forwarded: true                   # believe X-Forwarded-For from the proxy
    - port: 9090
      bind_addresses: ["127.0.0.1"]
      resources: [metrics, health]
```

- `x_forwarded: true` makes the per-address rate limits (and the audit log) see the client's
  address instead of the proxy's. Set it only on a listener that nothing but the proxy can
  reach.
- `resources` says which route families the listener serves: `client`, `federation`, `media`,
  `health`, `admin` (the interface at `/admin/` and the API at `/api/v1`), `metrics`. Keep
  `/metrics` on a port the internet cannot reach: it needs no authentication.
- `tls: { certificate_path: ..., private_key_path: ... }` on a listener makes the server
  terminate TLS on it itself (PEM chain and key); a listener without `tls` is plaintext. The
  files are read at start, so a wrong path stops the start with its name.

The paths the proxy forwards, all to the same listener: `/_matrix`, `/.well-known/matrix`,
`/_synapse` (the Synapse-compatible admin and client routes), `/admin` and `/api/v1` (the
management interface and API), `/integrations` (the self-service webhooks page every user
signs in to), and `/` if you want the server's landing page. Allow request
bodies at least as large as `media.max_upload_size` (50 MiB by default), and long timeouts on
`/_matrix/client/*/sync`, which is a long poll of up to 30 seconds plus the client's own
timeout.

**Caddy** (gets its certificate by itself):

```
matrix.example.org {
    reverse_proxy 127.0.0.1:8008
    request_body {
        max_size 50MB
    }
}
example.org {
    handle /.well-known/matrix/* {
        reverse_proxy 127.0.0.1:8008
    }
    # ... whatever else example.org serves
}
```

**nginx** (`deploy/compose/nginx.conf` has the whole file, validated with `nginx -t`):

```nginx
server {
    listen 443 ssl http2;
    listen [::]:443 ssl http2;
    server_name matrix.example.org;
    ssl_certificate     /etc/letsencrypt/live/matrix.example.org/fullchain.pem;
    ssl_certificate_key /etc/letsencrypt/live/matrix.example.org/privkey.pem;
    client_max_body_size 50m;

    location / {
        proxy_pass http://127.0.0.1:8008;
        proxy_http_version 1.1;
        proxy_set_header Host $host;
        proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;
        proxy_set_header X-Forwarded-Proto $scheme;
        proxy_read_timeout 120s;
    }
}
```

**Traefik, an Ingress or a Gateway** on Kubernetes: the chart renders the Ingress
(`ingress.enabled`), with the right paths, when told the host and the certificate Secret.

The admin interface is a normal web application on `/admin/`, authenticated with the
administrator's account. It is safe to expose at the public address, which is where the setup
and recovery links point; if you would rather keep it off the internet, serve `admin` on its
own listener and reach that over a VPN or an SSH tunnel.

## Federation

Other servers find yours by the server name. With `server.public_baseurl` set to an `https://`
address, the server publishes both discovery documents itself:

- `GET /.well-known/matrix/client` → `{"m.homeserver":{"base_url":"https://matrix.example.org"}}`
- `GET /.well-known/matrix/server` → `{"m.server":"matrix.example.org:443"}`

The second is derived from the base URL (its host, and its port or 443) unless
`server.well_known_server` says otherwise (`matrix.example.org:8448` when federation is reached
on another port; the empty string to publish none, when a proxy serves one). Both documents must
be reachable **at the server name's host**: when `example.org` is not the same machine as
`matrix.example.org`, make whatever answers `https://example.org` serve them, or proxy
`/.well-known/matrix/` to the server as the Caddy and nginx examples above do. An alternative is
a DNS record, `_matrix-fed._tcp.example.org SRV 10 0 443 matrix.example.org.`, which needs no
document.

With that in place, the proxy on 443 is the federation port; nothing listens on 8448 unless you
add a listener. Check from outside:

```sh
curl https://example.org/.well-known/matrix/server
curl https://matrix.example.org/_matrix/federation/v1/version
curl https://matrix.example.org/_matrix/key/v2/server     # this server's signing key
```

and with the [Matrix federation tester](https://federationtester.matrix.org/), which checks the
same things the way other servers do. Then join a room on another server from a client, send a
message, and watch it arrive.

Outbound federation verifies certificates against the public roots; a private CA goes in
`federation.custom_ca_certificates`, never `verify_certificates: false`. Missing keys are asked
of `federation.trusted_key_servers` (matrix.org by default, as Synapse). `federation.enabled:
false` makes a server whose users talk only to each other. The server connects over IPv4 only
unless `network.outbound.ipv4_only` is turned off, because many container networks resolve an
IPv6 address they cannot route; turn it off on a host with working IPv6.

Federation has been measured against the official suites and against a real Synapse (the
numbers and the interop harness are in the README). The Federation page of the admin interface
shows every server this one talks to, with its catch-up state.

## Email

Set `email.smtp` (host, port, security, username, password) in Configuration and the server
sends the unread-notification emails a client asks for (Element's "Enable email notifications"):
the room, the sender and a snippet, with the same back-off Synapse uses. Until `smtp.host` is
set nothing is sent; email pushers are stored and wait. The settings are hot: no restart. Verified
against a real SMTP server (`crates/hs-cli/tests/email_pushers.rs`).

## Hardening checklist

The defaults are already the safe choice; this is what to check before telling people the
address.

- **Registration is closed** (`auth.enable_registration: false`). Make accounts in the
  interface, or hand out invite links and registration tokens from the Users page. If you open
  registration, pair it with registration tokens or a CAPTCHA, or expect spam accounts.
- **Rate limits are on** (`rate_limits.enabled: true`), per account and per address. Behind a
  proxy, set `x_forwarded: true` on the listener the proxy reaches, or every client shares the
  proxy's address and its limits.
- **Link previews are off** (`media.url_preview_enabled: false`). On, the server fetches every
  link people post; the private address ranges are blocked by default.
- **`/metrics` serves nothing** until `telemetry.metrics.enabled` is on, and then it needs no
  authentication: serve it on a listener the internet cannot reach.
- **The admin interface** is on `/admin/` and the API on `/api/v1`, behind the administrator's
  password and API tokens that carry their scopes. The audit log records every change.
- **The setup and recovery links** are in the log and work once. Whoever can read the log can
  become an administrator; treat the log accordingly.
- **The signing key** is the server's identity (`keys/` in the data directory, or the Secret in
  cluster mode). Back it up with the data and never regenerate it: a server with a new key is a
  different server to the rest of Matrix.
- **Media**: `media.max_upload_size` (50 MiB) bounds what one upload may be;
  `media.remote_media_retention` bounds how long other servers' media is cached;
  `media.scanning` can hand every upload to a scanner before it is stored
  (`deploy/media-scanning/`).
- **Bridges** run as separate processes, each with its own registration and tokens, and each
  person's offered bridge is its own instance, so one person's trouble touches nobody else.
- **Administrator contact**: `server.admin_contact` publishes who to ask for help or report
  abuse to, in `/.well-known/matrix/support`.

## Backup and restore

What to copy depends on the layout.

**Single node, embedded database**: the data directory (or the `myelin` volume, or the
`data-myelin-hs-0` claim) is the whole server: `db/`, `keys/`, `media/`. Copy it with the server
stopped for a clean backup, or snapshot the volume while it runs for a crash-consistent one
(the database recovers from its journal on the next start, as after a power loss). Restore by
putting the directory back and starting the server: same key, same accounts, the messages up to
the backup and nothing after.

```sh
docker stop myelin
docker run --rm -v myelin:/data -v "$PWD":/backup alpine tar czf /backup/myelin-$(date +%F).tar.gz -C /data .
docker start myelin
```

**Single node, PostgreSQL**: `pg_dump -Fc` of the database, the media directory, and the
`keys/` directory. **Cluster**: the same three, where the key is the Secret
(`kubectl get secret <name> -o yaml`). Restore in the order key, database (`pg_restore
--no-owner`), media, then start.

The signing key is the part people forget. Without it the restored server cannot prove it is
the same server, and every event it ever signed fails verification elsewhere.

`hs config export` writes the stored configuration as YAML; keep one with the backup so the
settings can be replayed into a fresh database with `hs config import` if ever needed.

A backup and restore of both layouts is rehearsed in CD on every release
(`deploy/helm/hs/ci/backup-restore-smoke.sh`).

## Monitoring

- **Health**: `GET /health/live` answers while the process runs; `GET /health/ready` answers
  once it can serve (storage open, the cluster joined) and stops answering when it begins to
  shut down. Point the load balancer or the probes at `/health/ready`.
- **Logs** go to stdout, text by default, JSON lines with `telemetry.logging.json: true`
  (restart). `telemetry.logging.level` is hot; `RUST_LOG`, when set, outranks it. Every request
  carries a request id, and a problem the operator should act on is logged at `WARN` with its
  setting named.
- **Metrics**: `telemetry.metrics.enabled: true` (restart, or `HS__TELEMETRY__METRICS__ENABLED=true`;
  until then `/metrics` answers with an empty body) serves Prometheus text on `/metrics`: HTTP requests by route and status, rooms resident and
  loads, the sync hub's lag, the federation sender's queues and back-offs, appservice
  deliveries, process memory against its budget, and the store. Under Synapse-compatible names
  too (`synapse_compat_names`), so Synapse dashboards keep working. The chart's
  `serviceMonitor.enabled` scrapes it and `prometheusRule.enabled` installs fifteen alert rules,
  each proven to fire; `deploy/observability/grafana/hs-overview.json` is the dashboard.
  [ops/memory.md](ops/memory.md) says which memory metrics to watch and what they mean.
- **Traces**: `telemetry.tracing` exports OpenTelemetry traces over OTLP/gRPC; every request is
  a trace with its store transactions, room operations and federation calls inside it, and
  background work links to what caused it. [ops/tracing.md](ops/tracing.md).
- **Errors**: `telemetry.sentry` reports panics and errors to a Sentry DSN.
- **The interface** itself: the Overview shows what needs attention; Statistics, Tasks,
  Federation and each bridge's page show the state of their parts; the Cluster page shows the
  replicas and who owns what.

## Upgrading and rolling back

Every merge to `main` is a release: `X.Y.Z`, with a GitHub release, an image, a chart and
binaries from that one commit. `CHANGELOG.md` describes each line under its `.0` heading and
each release's notes list its merges. Read the notes between your release and the target; a
note that says storage changed means take a backup first and do not plan on rolling back
without it.

- **Docker**: pull the new tag, stop and remove the container, run the same `docker run` with
  the new tag against the same volume. The server restarts once; clients reconnect.
- **The binary**: install the new `hs`, `systemctl restart myelin`.
- **Kubernetes**: `helm upgrade` with the new `--version`, as in
  [install-and-upgrade.md](install-and-upgrade.md#upgrade). In cluster mode the pods roll one
  at a time and each hands its rooms to the others first.

Check afterwards: `hs version` or `/_matrix/federation/v1/version` reports the new version, the
log has no `ERROR` lines, `/_matrix/key/v2/server` returns the same key as before, a client
syncs.

**Rolling back** is running the previous version against the same data. Sync tokens the newer
version handed out may be refused and clients do an initial sync again; across a storage change,
restore the backup instead.

## Migrating from Synapse

The admin interface's Migration page copies a running Synapse's accounts, devices, tokens,
end-to-end keys, rooms, media and the rest over the network, shows what will not move before it
starts, and brings over what Synapse wrote after the copy at cutover. A `homeserver.yaml` can be
translated with `hs serve --synapse-config`, with a report of every key it mapped, ignored or
could not honour. The runbook is [compat/synapse-migration-runbook.md](compat/synapse-migration-runbook.md),
the key-by-key table [compat/synapse-config-table.md](compat/synapse-config-table.md), and
`/_synapse/admin` is served well enough that synapse-admin's screens work against it.

## Troubleshooting

| You see | It means | Do this |
|---|---|---|
| No `setup_link` in the log | An administrator already exists | Sign in as them, or `hs recover` ([recovery.md](recovery.md)) |
| The setup link points at `localhost` | `server.public_baseurl` is not set | Set it (`HS__SERVER__PUBLIC_BASEURL`), or open the link with the host replaced; the token is what matters |
| Clients cannot find the server from the bare domain | `/.well-known/matrix/client` is not served at the server name's host | Proxy `/.well-known/matrix/` on `example.org` to the server, or serve the document there ([Federation](#federation)) |
| Other servers get `401 Failed to find any key`, or rooms elsewhere cannot be joined | They cannot reach `/.well-known/matrix/server` at the server name, or the proxy does not forward `/_matrix/key` | Check the three `curl`s in [Federation](#federation) from outside, and the federation tester |
| `429` for everyone behind the proxy | The rate limits see the proxy's address | `x_forwarded: true` on the listener the proxy reaches |
| `unknown room` or a join to a remote room hangs on a container host | The host resolves an IPv6 address it cannot route (or the reverse) | The log names the host and the setting; see `network.outbound.ipv4_only` |
| The process is killed at its memory limit | Rooms resident exceed the budget | Raise the limit, or lower `server.rooms.memory_unload_fraction` and `storage.embedded.cache_size`; [ops/memory.md](ops/memory.md) |
| A setting is read-only in the interface | It is bootstrap, or pinned by an `HS__` variable | Change it where it is set and restart |
| Locked out of the admin interface | | `hs recover` where the key is ([recovery.md](recovery.md)) |
| Different signing key after a restore or upgrade | `keys/` (or the key Secret) did not come along | Stop; put the key back. A server with a new key is a different server |

When something is wrong and the log does not say why, turn `telemetry.logging.level` up to
`debug` (hot, no restart), reproduce, and turn it back. Bugs go to
<https://github.com/brandon-dacrib/myelin/issues> with the version, the layout and the log
lines; security problems to the address in `SECURITY.md`.
