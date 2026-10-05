# Myelin

A modern Matrix homeserver in Rust.

Myelin is the sheath that wraps a nerve fibre so a signal travels an order of magnitude faster,
without altering the signal itself. That is this project's ambition: the Matrix protocol exactly
as specified, carried a great deal faster, named in the tradition its predecessors set (Synapse,
Dendrite).

**What sets it apart is operations.** Install is one value: a server name, to `docker run` or
to `helm install`, and there is a working server with its key, database and media on one volume
and a link in the log that makes the first administrator. Scale is a replica count behind one
Service, with no worker types and no routing map. Administration is a web interface on a public
admin API with an audit log, and the configuration lives in the database where that interface
edits it. It is built for Kubernetes from birth and is the same static binary on a small ARM
host. Synapse-compatible at the API and operations level with a migration path; bridges are
first-class; specification coverage is measured mechanically. `docs/landscape.md` sets this
against Synapse, Dendrite, Conduit, continuwuity, tuwunel and Palpo, and
`docs/decisions/0008-the-standout-is-operations.md` is the decision to make this the product.

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
(`docs/bridges/mautrix.md`). A bridge is sent what the bridge specifications ask for: events,
typing, receipts, presence, to-device messages and device-list changes (MSC2409, MSC3202,
MSC4203), each exactly once across a restart; mautrix-whatsapp runs its encryption in
appservice mode against it.

**And a bridge can be offered to everyone.** An administrator switches WhatsApp on for the
server; a person gets their own bridge by messaging `@whatsappbot`, which sets it up and
invites them to it with the sign-in steps; the administrator sees everybody's bridge on one
page, and under it "Next steps for <person>": their own bot's name, the steps to relay, a
copy-as-message button, and "this is you" when it is the administrator's own. Each instance
has its own registration, ghosts and process, so one person's trouble touches nobody else. A Helm deployment can declare its offerings (`bridges.offerings` in the chart's values: a catalogue type and the offering's settings), created once and then administered in the interface; a bridge registered by hand for a network that is now offered is named on its page and on the offering's, with what to do. Verified against the real server, with a real heisenbridge started from
the files the server rendered (`docs/rfcs/0017-the-server-deploys-its-own-bridges.md`). The
server deploying each instance as a pod is built and waits for its first run on a cluster.
One thing was wrong until 2026-10-02: the chat the server opened for a person was created
by the bot, and a mautrix bridge only takes bare commands in a chat the *person* started, so
`login qr` typed there was decrypted and silently dropped. Reproduced with the real
mautrix-whatsapp and an encrypting client (the appservice-mode encryption worked end to end
both ways), fixed so the chat is started as the person with the bot invited, and tested against the real
bridge (`crates/hs-bridge-conformance/tests/real_mautrix_login.rs`). In a chat made before the
fix, `!wa login qr` works.

**Removing a user is one checkbox.** Deactivating an account on its page offers "Also erase
their data": the password and every session, every device and its encryption keys, email,
phone and single-sign-on links, the display name and avatar, and membership of every room, with
the account marked erased and never reactivated; messages stay unless redacted. Verified on the
real binary and in the browser against it (`web/e2e-real/user-erase.spec.ts`).

**Two encrypted clients exchange a message this server cannot read.** `matrix-rust-sdk` with
encryption enabled: keys upload, cross-signing bootstraps, one-time keys are claimed atomically,
Megolm establishes, the recipient decrypts. `cargo test -p hs-loadgen --test real_client_encrypted`.

**Unread messages reach an inbox.** A client that registers an email pusher (Element's
"Enable email notifications") gets the email Synapse sends: the room's name linked to the room
in the configured web client, the sender and a snippet of each message (none in an encrypted
room), and the unread count. The first email goes at once; while a room stays unread, the
next waits ten minutes, then an hour, up to a day, and reading the room starts it over. Set
up in the `email` section of Configuration (`docs/config.md`); verified on the real binary
against a real SMTP server (`crates/hs-cli/tests/email_pushers.rs`).

**Measured against the official suite, not against itself.**

| | |
|---|---|
| Complement `csapi` | 343 / 384 assertions (82 / 106 tests), measured 2026-10-01 on `main` at `2a0b362`, twice (the second run lost one test to a race this server has, now a known gap); 317 / 384 (78 / 106) on 2026-09-26 |
| Complement federation, whole package | 225 / 314 assertions (50 / 90 tests), measured 2026-10-01 on `main` at `2a0b362`, twice (49 / 90 the second time: a race in the test); 75 / 250 (14 / 88) on 2026-09-26 |
| Complement federation, restricted rooms, invites and knocks | 17 / 18 tests in both whole-package runs of 2026-10-01; the one left is a race in the test itself |
| Complement `TestSearch`, `TestMessagesOverFederation` | 1 / 1 each, in both runs of 2026-10-01 (0 / 1 before 2026-09-30) |
| Sytest, whole suite | 548 / 772 (34 skipped) on 2026-10-02 with every branch merged, from 448 the evening before and 407 that morning; client-server group 385 of 543, federation group 15 → 78 of 105; per-test results in `docs/status/sytest/` |
| `cargo fuzz` | 8 targets, 18.7 million executions under ASan, no crash (2026-10-01) |
| Spec routes served | 138 / 235 (58.7%) — client-server 108/166, server-server 30/36 |
| Rust | 27 crates, ~250k lines including tests, 2,534 tests |

**It runs for real.** PostgreSQL (over TLS, with libpq's five modes) or an embedded store, a
distroless non-root image on amd64 and arm64, a Helm chart, and a Kubernetes operator. Two
replicas share a room without forking its history, and have run as two pods on a real cluster.
CD refuses to publish an image that has not booted and answered `/health/live` on both
architectures, and runs the web interface's checks and browser flows on every push.

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
`HS__SERVER__PUBLIC_BASEURL` and the link is rooted there instead of at `localhost`. Without it,
the link names the address the server listens on, and the line after it says so: open it with
the host replaced by wherever you reach the server, since the token is what matters.

Locked out later, with nobody able to sign in as an administrator? `docker exec myelin hs
recover` prints a one-time link that resets an administrator's password and signs you in. It
works because the command runs where the server keeps its signing key and signs its request
with it, so only whoever holds the key can get a link; it expires in fifteen minutes and works
once. `docs/recovery.md` has the details, and the same command works in a pod
(`kubectl exec <pod> -- hs recover`) and on a host (`hs recover --data-dir ./data`).

CD boots the image with exactly this command before it will publish it, and refuses to publish
one that does not answer `/health/live`, serve the admin interface and log a setup link. It then
installs the Helm chart on a kind cluster with that same image, waits for Ready, and creates the
first administrator through the setup link, before the image is tagged or the chart published.

Without Docker, `hs serve --data-dir ./data --server-name example.org` is the same thing.

The server reaches other hosts over IPv4 only unless you say otherwise. Many container
networks resolve a dual-stack host's IPv6 address and have no route to it, and a server that
connects to the first address it is given fails the request. On a host with working IPv6, turn
`network.outbound.ipv4_only` off in the Configuration section (it applies at once, no restart);
the server then tries every address a name resolves to and falls back to the next when one does
not connect. The startup log says which it is (`outbound: IPv4 only`), and `/metrics` counts
connections and addresses that did not connect by family. When the policy hides the only
address a peer answers on (a peer that listens on IPv6 alone, reached by a name with both
families), the log says so: one warning per host every ten minutes, naming the host, the IPv4
addresses that failed, the IPv6 addresses it did not try, and the setting.

On Kubernetes it is one value:

```sh
helm install myelin oci://ghcr.io/brandon-dacrib/charts/hs --devel --set serverName=example.org
```

`--devel` because nothing is tagged yet: every push to `main` publishes the chart as a
pre-release (`0.1.0-main.<run>.g<commit>`) whose default image is the one built from that same
commit, and a plain `helm install` takes releases only. From a checkout,
`helm install myelin deploy/helm/hs --set serverName=example.org` is the same chart with the
`main` image.

That produces a single replica with a 10 GiB volume holding the database, the signing key and
media, probes, a Service, and the same setup link in the pod's log (`helm install` prints the
`kubectl logs` line that finds it). Verified on 2026-09-26 against a real cluster with the
published image: install to Ready, the interface at `/admin/`, the first administrator made
through the link, the pod deleted and the signing key unchanged, a `helm upgrade` that replaced
the pod and the key unchanged again. The same day it was installed for keeps behind a Traefik
Ingress with a Let's Encrypt certificate, scraped by Prometheus through the chart's
ServiceMonitor, and its setup page opened in a browser at the public hostname; that found the
Ingress routing `/_matrix` only, which would have made the setup link a 404, and it routes the
interface now. Locked out of it later: `kubectl exec <pod> -- hs recover`, and the link it
prints. The chart has been on the registry since 2026-09-26, published by every push to
`main`, and the sentence above was run twice against the same cluster that day: Install
complete in about two minutes, the pod on the image built from the chart's own commit,
`/health/ready` answering, the setup link in the log. Upgrading the standing demo to it was
refused, which found that the chart's volume claim template carried labels that change with
every publish and that Kubernetes never lets change; fixed the same day, and
`docs/status/12-platform-and-kubernetes.md` has the one manual step an install made before the
fix needs. Cluster mode
(`mode=cluster`, PostgreSQL or CloudNativePG, media on S3, a shared signing-key Secret) has run
as two pods on a real cluster (2026-09-28): a client's `/sync` served from either pod, rooms
handed between them during a rolling update and a pod loss. That run found requests landing
mid-handoff failing; the fix is on `main` and the re-run with it is the top of
`docs/next-steps.md`. `docs/scaling.md` says exactly what adding a
replica buys (rooms and clients in flight, availability) and what it does not (one room's
throughput, database capacity), and which of that is built today.

`CHANGELOG.md` is the full record of what has been built, and is honest about the difference
between a route that is registered and a route that works. `docs/next-steps.md` is what comes next
and the gaps as they actually stand, as a table that is refreshed against the code and closed
one row at a time — the largest being that a user here cannot yet talk to the rest of Matrix the
way a Synapse user can.

## How far along is it

Roughly **70% of a homeserver somebody else could run**, but the number only means something
broken up, because the parts are nowhere near each other. This table is kept current with
`docs/next-steps.md`, which has the basis for each figure.

| Area | Done | Basis |
|---|---|---|
| Client-server API | ~80% | 343/384 Complement csapi assertions, 82/106 tests (2026-10-01; 317/384 on 2026-09-26); Sytest's client-server group 385/543 (2026-10-02; 319/542 on its first run); two Element sessions chat encrypted; spaces (`/hierarchy`) and `/search` answer (Complement's `TestSearch` passes); a member can no longer redact another member's message without the power to (found by Sytest 2026-10-01, fixed the same day); guest access (`auth.allow_guest_access`, off by default) and the legacy `/events` and `/initialSync` are served since 2026-10-02; push rules (19/53), sync (57/84), tagging, ignored users and the user directory are Sytest's largest client-server leftovers |
| Storage, rooms, state resolution | ~85% | 2,500+ tests, two backends through one conformance suite, PostgreSQL over TLS; a cold first boot is 0.6 s (was 9 s: every table now lives in one shared embedded keyspace, decision 0024); `cargo fuzz` ran 18.7 million inputs through the parsers without a crash |
| Configuration and first run | ~95% | database-backed, edited in the UI with per-setting history and revert, one command from nothing to a server; every one of the 71 settings is classified bootstrap, hot or restart and the API says which: 39 apply on the running server (every rate-limit bucket, registration, user directory, media limits and URL previews, `.well-known`, `public_baseurl`), 25 say they need a restart, 7 are per replica |
| Admin API | ~90% | 161 of 161 operations have a real handler (`python3 tools/admin_api_coverage.py`), with real-server tests behind them, and a contract test proves every operation enforces exactly the scope its OpenAPI document says (28 did not until 2026-10-01); that is handler coverage, not a claim that every Synapse admin workflow has an equivalent; a token narrower than a full administrator's cannot be minted yet |
| Management web interface | ~85% | users, rooms, bridges (catalogue, wizard, runbook, sign-in guides and who has signed in, offerings), federation (with catch-up state), media, registration tokens, server notices, reports, tasks, statistics, cluster, migration (every stream named, and what does not move shown before a start), configuration (structured settings as forms, with history, and a badge on every setting saying whether it applies on save, needs a restart or is per replica) and the audit log are real against the real server, and each page explains its controls in place; the browser suites run in CI and fail on a flaky test. The rule since 2026-10-01: all administration is done in the interface, with sane defaults, explained there |
| Bridges | ~85% | heisenbridge works end to end; mautrix-whatsapp, added through the wizard, connects, runs encryption in appservice mode, receives device lists, key counts, to-device messages and ephemeral data, and the admin API asks it who has signed in through its provisioning API; no mautrix bridge has carried a message yet (signing in needs a phone); offering a bridge to everyone, each person getting their own instance by messaging its bot, runs end to end, and the server deploying that instance as a pod through its operator ran against a real (kind) API server on 2026-10-01: heisenbridge `requested → ready` in 47 s |
| Operations (HA, scale-out) | ~60% | one-value `helm install` verified on a real cluster with the published image, including a restart and an upgrade that kept the signing key; the chart is published from `main` and installs from the registry in one sentence; a standing demo behind an Ingress with a real certificate, scraped by Prometheus, rolled to each green `main`; a locked-out administrator gets back in with one command run where the key is; readiness is withdrawn the moment a shutdown begins; two pods on a real cluster serve a client's `/sync` from either, with typing, receipts and presence crossing replicas, joins by alias and room creation placed on the owning replica, the outbound federation queue durable, bounded and caught up after an outage, drain through the admin API, and the last replica stopping in under a second instead of 18; the operator reconciles `Homeserver` and `Bridge` resources against a real API server and CD proves it on kind before tagging; the fix for requests landing mid-handoff has not been run on those pods yet; a non-owner replica still reloads a room per event to answer `/sync` (RFC 0018, in progress) |
| Synapse migration | ~75% | the importer copies accounts, rooms (paged, ~3,000 events/s on a 2,000-member room), end-to-end keys, cross-signing, key backups, push rules, pushers, filters, receipts and rooms joined over federation from a real Synapse 1.161, each kind served by the real server after import; the Synapse config translation table and five `/_synapse/admin` routes; threaded receipts and partial-state rooms do not move |
| **Federation** | **~55%** | Sytest's federation group is **78 of 105** (2026-10-02, `main` at `09f24ee`; 15 on its first run on 2026-09-30): auth 16/20, make_join 3/3, send_join 8/9, send_leave 1/1, invites 9/10, room versions 7/7, key server 6/6, send-to-device 2/2, state 7/10, backfill 3/5, get_missing_events 2/3, the general federation API 9/14, device keys 4/9, the query API 1/5, public rooms 0/1; Complement's federation package 225/314 assertions, 50/90 tests (2026-10-01, before the Sytest fixes), 17/18 on restricted rooms, invites and knocks. What that means: a user joins a room hosted elsewhere through the client API, including restricted rooms and through another server; messages flow both ways between two instances of this server; history from before the join and the gap left by a leave-and-rejoin are fetched, the state at fetched history is asked of the server that sent it and every fetched event is auth-checked; invites, leaves and knocks cross servers, so do typing, receipts, presence, device lists, to-device messages and media; the outbound queue survives a restart and a destination down longer than its queue is caught up from the rooms; the key server answers `/key/v2/server/{keyId}` and the notary, server ACLs are enforced on every room-scoped endpoint and per PDU, version 1 and 2 rooms are joined and backfilled, federation redactions are applied. **What is missing is the part that matters most: it has never federated with a Synapse.** Every number above is Complement, Sytest, or two copies of this server. The real-Synapse interop run (discovery and keys, joins both ways, backfill, invites, bans, redactions, EDUs, media, public rooms, profile and directory queries, rooms of version 10 to 12) started on 2026-10-02 and will be reported here step by step; the query API, device keys over federation and public rooms over federation are being worked on the same day |

Federation is the honest answer to "when could I use this": a user here can join a room on
another instance of this server and talk, and three quarters of Sytest's federation suite passes,
but it has not been pointed at a Synapse, and until a message has crossed between the two, the
federation number above is a number about test suites. The second honest answer is Sytest's
548 of 772, which names every missing piece by test.

## Where things are

- `PLAN.md`: the plan, design decisions, architecture, roadmap.
- `docs/landscape.md`: the other homeservers as they are today, and where this one stands.
- `docs/scaling.md`: what a replica adds, what it does not, and what is built versus designed.
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
