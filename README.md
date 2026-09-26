# Myelin

A modern Matrix homeserver in Rust.

Myelin is the sheath that wraps a nerve fibre so a signal travels an order of magnitude faster,
without altering the signal itself. That is this project's ambition: the Matrix protocol exactly
as specified, carried a great deal faster, named in the tradition its predecessors set (Synapse,
Dendrite).

Kubernetes-native, horizontally scalable, and equally at home as a single static binary on a small ARM host. Synapse-compatible at the API and operations level with a migration path. Bridges are first-class. Full specification coverage is enforced mechanically.

## What works today

**Element Web signs in and talks to it.** The browser client most Matrix users run logs in, lists
rooms, sends and receives messages live between two independent sessions, propagates a display-name
change to an already-open tab, creates rooms (encrypted ones included), invites, and pages back
through history. Screenshots are in `docs/design/screenshots/`; the reproduction is
`web/element-testing/README.md`.

**A WhatsApp bridge is a wizard away.** The admin interface's Bridges section knows fourteen
bridges people actually run: what each is, what it needs, and how to sign in to it. Choosing
one renders the bridge's own `config.yaml` and its registration, already pointed at this
server; the Created page says where to put them, how to start the bridge, turns green when it
connects, and gives the sign-in steps for that network with the bot's real name.
mautrix-whatsapp was added exactly that way and connected in seconds
(`docs/bridges/mautrix.md`).

**Two encrypted clients exchange a message this server cannot read.** `matrix-rust-sdk` with
encryption enabled: keys upload, cross-signing bootstraps, one-time keys are claimed atomically,
Megolm establishes, the recipient decrypts. `cargo test -p hs-loadgen --test real_client_encrypted`.

**Measured against the official suite, not against itself.**

| | |
|---|---|
| Complement `csapi` | 314 / 384 assertions (78 / 106 tests) |
| Complement federation | 73 / 250 assertions (12 / 88 tests), measured 2026-09-26; 59 / 246 (6 / 88) five days earlier |
| Spec routes served | 138 / 235 (58.7%) — client-server 108/166, server-server 30/36 |
| Rust | 26 crates, ~154k lines, 1,678 tests |

**It runs for real.** PostgreSQL or an embedded store, a distroless non-root image on amd64 and
arm64, a Helm chart, and a Kubernetes operator. Two replicas share a room without forking its
history. CD refuses to publish an image that has not booted and answered `/health/live` on both
architectures.

```sh
docker run -d --name myelin -p 8008:8008 -v myelin:/data \
  -e HS__SERVER__SERVER_NAME=example.org ghcr.io/brandon-dacrib/myelin:main
```

That is the whole installation. There is no configuration file: the database, the signing key
and uploaded media all live in the `myelin` volume, and every other setting is a default until
you change it in the admin interface, which keeps it in the database.

A new server has no accounts, so it tells you how to make the first one. `docker logs myelin`
ends with a line like

```
WARN this server has no administrator yet: open the setup link to create one. It works once,
     for whoever opens it first setup_link=http://localhost:8008/admin/setup#token=...
```

Open it, choose a username and a password, and you are signed in to the admin interface as the
server's administrator. The link is offered at every start until somebody uses it and never
again after; only someone who can read the server's log can use it. Behind a reverse proxy, set
`HS__SERVER__PUBLIC_BASEURL` and the link is rooted there instead of at `localhost`.

CD boots the image with exactly this command before it will publish it, and refuses to publish
one that does not answer `/health/live`, serve the admin interface and log a setup link.

Without Docker, `hs serve --data-dir ./data --server-name example.org` is the same thing.

`CHANGELOG.md` is the full record of what has been built, and is honest about the difference
between a route that is registered and a route that works. `docs/next-steps.md` is what comes next
and the gaps as they actually stand — the largest being that administering this server should be
pleasant, and is not yet.

## How far along is it

Roughly **60% of a homeserver somebody else could run**, but the number only means something
broken up, because the parts are nowhere near each other. This table is kept current with
`docs/next-steps.md`, which has the basis for each figure.

| Area | Done | Basis |
|---|---|---|
| Client-server API | ~75% | 317/384 Complement csapi assertions; two Element sessions chat encrypted |
| Storage, rooms, state resolution | ~85% | 1,600+ tests, two backends through one conformance suite |
| Configuration and first run | ~90% | database-backed, edited in the UI, one command from nothing to a server |
| Admin API | ~40% | 58 of 145 operations have a real handler; the rest answer an honest 501 |
| Management web interface | ~75% | users, rooms, bridges (catalogue, wizard, runbook, sign-in guides), federation, configuration and the audit log are real against the real server |
| Bridges | ~75% | heisenbridge works end to end; mautrix-whatsapp, added through the wizard, connects and starts encrypted; no mautrix bridge has carried a message yet |
| Operations (HA, scale-out) | ~40% | runs on Kubernetes with a chart and a tested image; the cluster path has not carried real traffic |
| **Federation** | **~30%** | 75/250 assertions, 14/88 tests; a user joins a room hosted elsewhere through the client API, messages flow both ways between two instances of this server, and the room's history from before the join is fetched as the client scrolls back; no EDUs, in-memory outbound queue, not yet tried against Synapse |

Federation is the honest answer to "when could I use this": a user here cannot really talk to
the rest of Matrix yet.

## Where things are

- `PLAN.md`: the plan, design decisions, architecture, roadmap.
- `CHANGELOG.md`: what has been built, and what is verified rather than merely written.
- `docs/next-steps.md`: the current resume point, priorities, and known gaps.
- `docs/workstreams/`: the sixteen expert tracks, their interfaces, and the rules for parallel work.
- `docs/synapse-inventory.md`: the behavioral parity checklist for Synapse 1.161.0 (generated by `tools/synapse_inventory.py`).
- `docs/status/`: one status file per track, kept current by the track.
- `docs/decisions/`: dated decision records. `docs/rfcs/`: interface change proposals.
- `crates/`: the Rust workspace. `web/`: the management web interface.
- `tools/fetch-refs.sh`: clones the reference codebases into `refs/` (git-ignored).

The binary and the crates keep the `hs-` prefix for now; renaming them is mechanical and is
tracked separately, because doing it mid-flight would collide with work in progress.

License: Apache-2.0 (provisional, see `docs/decisions/0001-license.md`).
