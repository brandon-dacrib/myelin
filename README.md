# Myelin

A modern Matrix homeserver in Rust, built to be operated.

Myelin is the sheath that wraps a nerve fibre so a signal travels an order of magnitude faster,
without altering the signal itself. That is the ambition: the Matrix protocol exactly as
specified, carried a great deal faster, in the naming tradition of Synapse and Dendrite.

[![Release](https://img.shields.io/github/v/release/brandon-dacrib/myelin?label=release)](https://github.com/brandon-dacrib/myelin/releases)
[![CI](https://github.com/brandon-dacrib/myelin/actions/workflows/ci.yml/badge.svg)](https://github.com/brandon-dacrib/myelin/actions/workflows/ci.yml)
[![License](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](LICENSE)

## Why

**Operations is the product.** Install is one value, a server name, and there is a working
server with its key, database and media on one volume and a link in the log that makes the
first administrator. There is no configuration file: every setting lives in the database, is
edited in the web interface with history and revert, and says whether it applies at once or at
the next restart. Scale is a replica count behind one Service, with no worker types and no
routing map. It is one static binary, the same on Kubernetes and on a small ARM host.

- **Administered through a web interface** on a public, documented admin API with scoped
  tokens, an audit log and a live event stream. Everything an administrator does has a page
  that explains itself.
- **Bridges are first-class.** The interface knows sixteen bridges people actually run, renders
  each one's config and registration, deploys it as a pod, and can offer a bridge to every user
  so each gets their own instance by messaging a bot.
- **Synapse-compatible** at the API and the operations level: `/_synapse/admin` serves
  synapse-admin, a `homeserver.yaml` translates, and an online migration copies a running
  Synapse's accounts, keys, rooms and media and cuts over.
- **Measured against the official suites**, and against a real Synapse, with the numbers below.

`docs/landscape.md` sets this against Synapse, Dendrite, Conduit, continuwuity, tuwunel and
Palpo; `docs/decisions/0008-the-standout-is-operations.md` is the decision to make operations
the product.

## Quick start

**Docker**

```sh
docker run -d --name myelin -p 8008:8008 -v myelin:/data \
  -e HS__SERVER__SERVER_NAME=example.org \
  -e HS__SERVER__PUBLIC_BASEURL=https://matrix.example.org \
  ghcr.io/brandon-dacrib/myelin:latest
docker logs myelin 2>&1 | grep setup_link
```

Open the link, choose a username and a password, and you are the server's administrator,
signed in to the interface at `/admin/`. The link works once and is offered at every start
until somebody uses it.

**Kubernetes**

```sh
helm install myelin oci://ghcr.io/brandon-dacrib/charts/hs --set serverName=example.org
kubectl logs statefulset/myelin-hs | grep setup_link
```

**A host**: download the binary for your platform from the
[releases](https://github.com/brandon-dacrib/myelin/releases) and run
`hs serve --data-dir ./data --server-name example.org`.

**[docs/deployment.md](docs/deployment.md) is the deployment guide**: Docker Compose with a
reverse proxy, a systemd unit, the Helm chart, TLS, federation and DNS, hardening, backup,
monitoring, upgrades. **[docs/user-guide.md](docs/user-guide.md)** is the guide to the admin
interface, with screenshots from the real server. Locked out later: `hs recover`, run where the
server keeps its key, prints a one-time link back in (`docs/recovery.md`).

## What works today

- **Element signs in and talks to it.** Rooms, live messages between sessions, encrypted rooms,
  invites, history, spaces, search, threads, push rules, email notifications.
- **Two encrypted clients exchange a message the server cannot read.** Device keys,
  cross-signing, atomic one-time-key claims, key backup, to-device, dehydrated devices.
- **It federates with the rest of Matrix.** Joins both ways including restricted rooms, backfill,
  invites, knocks, leaves, bans, redactions, typing, receipts, presence, device lists,
  to-device, media, server ACLs, room versions 1 through 12, and a 46-check interop harness
  against a real Synapse that passes single-node and as a two-replica cluster.
- **A WhatsApp bridge is a wizard away.** Choosing a bridge renders its own `config.yaml` and
  registration, the page turns green when it connects, and the sign-in steps are there with the
  bot's real name. A bridge offered to everyone gives each person their own instance.
- **Removing a user is one checkbox**, with "also erase their data": every session, device,
  key, address, profile and membership.
- **It runs for real.** PostgreSQL over TLS or an embedded store; a distroless non-root image on
  amd64 and arm64; a Helm chart with probes, alerts and a ServiceMonitor; a Kubernetes operator
  for bridges; two replicas sharing rooms through rolling updates and pod loss, measured in CD
  on every release. A standing demo has federated with matrix.org since 2026-10-10.
- **Every merge to `main` is a release**, and CD refuses to publish an image that has not
  booted, served the admin interface, logged a setup link and installed from the chart.

## Measured, not claimed

| Suite | Result |
|---|---|
| Real Synapse interop (`tests/federation-synapse/run.sh`) | **46 / 46** checks single-node on CI against Synapse `latest` (2026-10-19), **51 / 51** as a two-replica cluster on PostgreSQL (2026-10-09) |
| Complement `csapi` | 383 / 386 assertions, 105 / 106 tests (2026-10-05); the one left is skipped for Synapse and Dendrite too |
| Complement federation | **316 / 317** assertions, 89 / 90 tests; restricted rooms, invites and knocks 18 / 18 (2026-10-09) |
| Sytest, whole suite | **754 / 772** (3 failed, 15 skipped); client-server 523 / 534, federation 103 / 105, appservices 22 / 22 (2026-10-09) |
| Spec routes served | 185 / 235; client-server 151 / 166, server-server 34 / 36; the rest are APIs a homeserver calls rather than serves (2026-10-09) |
| `cargo fuzz` | 8 targets, 18.7 million executions under ASan, no crash (2026-10-01) |
| Rust | 27 crates, about 375k lines including tests, 3,235 tests (2026-10-09) |

The failures that remain are races in the tests themselves or a behaviour no server
implements, each with its evidence in `docs/status/06-federation.md`. Per-test results are in
`docs/status/`; `docs/status/dashboard.md` is the generated measurement.

## How far along

Roughly 70% of a homeserver somebody else could run. The basis for each figure is in
`docs/next-steps.md` and the track's file in `docs/status/`.

| Area | Done | In one line |
|---|---|---|
| Client-server API | ~93% | Sytest's client-server group 523 of 534; registration, login, devices, presence, room state, joins, typing, receipts, keys, backup, cross-signing, to-device, directory all at 100%; left: MSC4222 `state_after`, `/messages` filtering by relation type |
| Storage, rooms, state resolution | ~85% | Two backends through one conformance suite, PostgreSQL over TLS, a cold boot in under a second, 18.7 million fuzz inputs without a crash |
| Configuration and first run | ~95% | Database-backed, edited in the interface with history and revert; every setting classified hot, restart or bootstrap, and the API says which |
| Admin API | ~92% | 165 of 165 operations have a real handler with real-server tests; scopes enforced and contract-tested; 69 of 77 `/_synapse/admin` routes; left: the event stream is not filtered per scope |
| Management web interface | ~88% | Users, rooms, bridges, federation, media, invite links, tokens, notices, reports, tasks, statistics, cluster, migration, configuration and the audit log, real against the real server, browser-tested in CI; left: no operator other than the author has tried it |
| Bridges | ~91% | heisenbridge and mautrix-whatsapp end to end with real phones; offerings deployed as pods by the operator; release tags pinned and tested weekly; Hookshot offered and deployed from a rendered config (public URL, GitHub App, its connections listed from room state) and run for real against its image: a webhook made in a room, posted to, and listed (2026-10-20); email (matrimail, community software on an image this repository builds) offered, started from its rendered files and answering its bot and its provisioning API for real (2026-10-20); left: the IRC bridge not run for real, no real mailbox signed in through matrimail |
| Operations (HA, scale-out) | ~80% | Day two measured on real pods in CD: 1 request lost in 953 on a graceful delete, 0 in 1,803 on a kill, 0 in 1,077 scaling 2→3→1; backup and restore rehearsed in both layouts; left: scaling 1→2 can break `/sync` for an account (`docs/status/12-platform-and-kubernetes.md`) |
| Synapse migration | ~92% | Rehearsed end to end against a real Synapse 1.162 on PostgreSQL: accounts, devices, tokens, keys, backups, rooms, media, with the cutover; left: unread counts, notice rooms, a run against a large Synapse |
| Federation | ~95% | Sytest 103 of 105, Complement 89 of 90, the interop harness against Synapse; left: the three tests above |

## Documentation

- [docs/README.md](docs/README.md): the index.
- [docs/deployment.md](docs/deployment.md): deploying, securing, backing up, monitoring and
  upgrading. [docs/install-and-upgrade.md](docs/install-and-upgrade.md): every Helm command.
- [docs/user-guide.md](docs/user-guide.md): the admin interface, with screenshots.
- [docs/config.md](docs/config.md): every setting, generated from the schema.
- [docs/bridges/](docs/bridges/): mautrix, heisenbridge, iMessage, Hookshot and matrimail (email), as actually run.
- [docs/compat/](docs/compat/): the Synapse migration runbook, config table and admin routes.
- [docs/scaling.md](docs/scaling.md): what a replica buys, and what it does not.
- [CHANGELOG.md](CHANGELOG.md): what has been built, and what was verified rather than written.
- [PLAN.md](PLAN.md), [docs/decisions/](docs/decisions/), [docs/rfcs/](docs/rfcs/): the design,
  the dated decisions and the interface proposals. [docs/next-steps.md](docs/next-steps.md):
  where things are and what comes next.

## Contributing

[CONTRIBUTING.md](CONTRIBUTING.md) has the build, the checks CI runs and what a change needs.
Security problems go through [SECURITY.md](SECURITY.md). The binary and the crates keep the
`hs-` prefix from before the project had a name.

## License

Apache-2.0. See [LICENSE](LICENSE) and `docs/decisions/0001-license.md`.
