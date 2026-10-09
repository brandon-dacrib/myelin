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
| Real Synapse interop | **46 / 46 checks** against a real Synapse 1.162.0 in Docker, measured 2026-10-09 on branch `agent/federation` (first run ever): keys and notary both ways, joins both ways with backfill, invites, leave/rejoin, kick, ban, redaction, typing, receipts, to-device, authenticated media both ways, public rooms, profile and directory queries both ways, device keys over federation, and rooms of version 10–12 joined both ways plus a restricted join authorised by Synapse. Rerun with `tests/federation-synapse/run.sh` |
| Complement `csapi` | 383 / 386 assertions (105 / 106 tests), measured 2026-10-05 on `main` (status 14, session 11; the one left, `TestDeviceListsUpdateOverFederationOnRoomJoin`, is skipped for Synapse and Dendrite too); 343 / 384 (82 / 106) on 2026-10-01, 317 / 384 (78 / 106) on 2026-09-26 |
| Complement federation, whole package | **316 / 317 assertions (89 / 90 tests, 1 skipped)**, measured 2026-10-09 on `main` at `00fe0c01` (branch `agent/federation-95`, run 14); the one left, `TestDeviceListsUpdateOverFederationOnRoomJoin`, is skipped by Synapse and Dendrite (no server sends a device-list update on a join; Synapse closed the PR that did); 315 / 317 (88 / 90) on 2026-10-05, 225 / 314 (50 / 90) on 2026-10-01, 75 / 250 (14 / 88) on 2026-09-26 |
| Complement federation, restricted rooms, invites and knocks | **18 / 18 tests** on 2026-10-09 (17 / 18 on 2026-10-01: `TestUnbanViaInvite`, fixed 2026-10-05) |
| Complement `TestSearch`, `TestMessagesOverFederation` | 1 / 1 each, in both runs of 2026-10-01 (0 / 1 before 2026-09-30) |
| Sytest, whole suite | **754 / 772 (3 failed, 15 skipped; 99.6% of tests run)** on 2026-10-09 on `main` at `00fe0c01` (the same on 2026-10-05); client-server group 523 of 534, federation group **103 of 105** (15 on its first run, 2026-09-30; 78 on 2026-10-02), application services 22 of 22; the three left are two races in the tests and one Synapse blacklists itself; 548 / 772 on 2026-10-02, 407 that morning; per-test results in `docs/status/sytest/` |
| `cargo fuzz` | 8 targets, 18.7 million executions under ASan, no crash (2026-10-01) |
| Spec routes served | 185 / 235 (78.7%) on 2026-10-09 — client-server 151/166, server-server 34/36 (138 / 235 on 2026-10-05); the rest of the 235 are the appservice, identity and push-gateway APIs, which a homeserver calls rather than serves (`docs/status/dashboard.md`) |
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
| Client-server API | ~93% | Complement csapi 383/386 assertions, 105/106 tests (2026-10-05; 343/384 on 2026-10-01); Sytest's client-server group **523 of 534** (98%, 2026-10-09, `main` at `00fe0c01`; 385/543 on 2026-10-02): registration, login, devices, presence, room state, aliases, joins, events, typing, receipts, account, guest access, room auth, room versions (51/51), device keys, key backup, cross-signing, tagging, OpenID, to-device, ignored users and the user directory all at 100%; sync 83/84, push 50/51, third-party ids 18/19, invites 13/14, room upgrades 18/21, bans 3/5, power levels 0/2 are what is left, and 13 tests are skipped because the server does not declare Sytest's `can_change_power_levels` capability, so those are unmeasured; two Element sessions chat encrypted; spaces (`/hierarchy`) and `/search` answer; MSC4222 `state_after` and `/messages` filtering by relation type are the known gaps beyond the suites |
| Storage, rooms, state resolution | ~85% | 2,500+ tests, two backends through one conformance suite, PostgreSQL over TLS; a cold first boot is 0.6 s (was 9 s: every table now lives in one shared embedded keyspace, decision 0024); `cargo fuzz` ran 18.7 million inputs through the parsers without a crash |
| Configuration and first run | ~95% | database-backed, edited in the UI with per-setting history and revert, one command from nothing to a server; every one of the 87 settings is classified bootstrap, hot or restart and the API says which: 55 apply on the running server (every rate-limit bucket, registration, CAPTCHA, CAS and single sign-on, user directory, media limits and URL previews, `.well-known`, `public_baseurl`), 25 say they need a restart, 7 are per replica |
| Admin API | ~92% | 165 of 165 operations have a real handler (`python3 tools/admin_api_coverage.py`, 2026-10-09), with real-server tests behind them, and a contract test proves every operation enforces exactly the scope its OpenAPI document says; tokens narrower than a full administrator's are minted since 2026-10-02; 69 of 77 `/_synapse/admin` routes serve synapse-admin's screens through the native API (2026-10-09); not yet: the SSE event stream is not filtered per token scope, the eight unmounted synapse-admin routes, and the management interface does not adapt its wording to a narrow token |
| Management web interface | ~88% | users, rooms, bridges (catalogue, wizard, runbook, sign-in guides and who has signed in, offerings), federation (with catch-up state), media, invite links, API tokens, server notices, reports, tasks, statistics, cluster, migration (every stream named, and what does not move shown before a start), configuration (structured settings as forms, with history, and a badge on every setting saying whether it applies on save, needs a restart or is per replica) and the audit log are real against the real server, and each page explains its controls in place; the browser suites run in CI and fail on a flaky test. Walked as a first-time operator on 2026-10-09 against an empty real server: the first screen now says what the server is and what to do first instead of warning about the server's own bridge registration, the sidebar is three groups rather than fourteen entries, "Settings" and "Configuration" are no longer two names for one thing, and the Configuration pages lead with the settings and explain themselves on request (`docs/status/16-management-web-interface.md`, 2026-10-09). The rule since 2026-10-01: all administration is done in the interface, with sane defaults, explained there. Not yet: the Audit log and Migration pages have not had the same walk, a token narrower than a full administrator's is minted but the pages do not yet adapt their wording to it, and no operator who is not the author has tried it |
| Bridges | ~90% | heisenbridge works end to end; mautrix-whatsapp, added through the wizard, connects, runs encryption in appservice mode, and the owner's own WhatsApp bridge on the demo carries messages both ways (its bot decrypted eight messages in one session, and a message queued through the 2026-10-09 outage was delivered after the recovery); the admin API asks a bridge who has signed in through its provisioning API; offering a bridge to everyone, each person getting their own instance by messaging its bot, runs end to end, and the server deploys that instance as a pod through its operator on a real (kind) API server; every offering pins a release tag (decision 0037, 2026-10-09) and the real-bridge tests boot mautrix-whatsapp and mautrix-signal on those pins and sign in; one bridge's slowness never delays another's delivery (2026-10-08); the manager logs every bot device it cross-signs; not yet: only WhatsApp has carried a message with a real phone, hookshot and the IRC bridge have not been run for real, and a bridge's crypto store cannot be backed up from the interface |
| Operations (HA, scale-out) | ~80% | one-value `helm install` verified on a real cluster with the published image, including a restart and an upgrade that kept the signing key; the chart is published from `main` and installs from the registry in one sentence; a standing demo behind an Ingress with a real certificate, scraped by Prometheus, rolled to each green `main`; a locked-out administrator gets back in with one command run where the key is; the operator reconciles `Homeserver` and `Bridge` resources against a real API server and CD proves it on kind before tagging. **Since 2026-10-09, day two is run on real pods, not asserted** (`deploy/helm/hs/ci/`, each in CD on kind): two replicas under continuous traffic through the Service lose **1 request in 953 on a graceful pod delete, 0 in 1,803 on a forced kill, 0 in 1,077 scaling two to three to one**, 4 in 979 on a roll from the 2026-09-30 image, and nothing from the cluster across a three-roll CA rotation (the handoff fix, decision 0017, measured at last; the forward waits out a handoff instead of failing); a backup restores the same server in both layouts (the volume, or `pg_dump` plus media plus the key Secret: same key, same password, the messages up to the backup and nothing after); fifteen alert rules in the chart each proven to fire by `promtool`; probes set from the measured 5-6 s to Ready; `hs serve` terminates TLS itself; the Synapse interop harness runs in CI nightly and the bridge image pins are checked weekly. What keeps it from 95%: **the smoke found that scaling from one replica back to two breaks the account's `/sync` and then some rooms (`unknown event EventSn#...`, `cited event not in history`), reproducibly, and the fix is sync- and room-side** (status 12, 2026-10-09); the one forward in 953 that waits its whole 10 s deadline on a graceful delete; the operator's `Homeserver` cluster mode and the degraded CRD apply have still only run against a fake API server |
| Synapse migration | ~92% | rehearsed end to end against a real Synapse 1.162 on PostgreSQL in Docker (`crates/hs-cli/tests/migration_rehearsal.rs`, 2026-10-09): the importer copies accounts, devices, access and refresh tokens, email addresses and phone numbers, SSO identities, erasures, account data, end-to-end keys, cross-signing, key backups, room keys still waiting for offline phones, push rules, pushers, filters, registration tokens, rooms (version 12 included: the rehearsal found none imported, fixed the same day), receipts in their threads, media and other servers' media, with what was written to Synapse after the copy brought over by the cutover, each kind checked from the client's side afterwards; a `homeserver.yaml` straight out of Synapse's `generate` translates as it is (64 of 229 keys mapped, 38 inert, the rest features this server lacks); 69 of 77 `/_synapse/admin` routes serve synapse-admin's screens through the native API. Not moved: unread counts, Synapse's server-notice rooms as notice rooms, bridges' stream positions, dehydrated devices, thumbnails, a room Synapse is still joining; no run against a large Synapse on a quiet machine yet, and the rehearsal shapes keys as a client would rather than driving a real Element |
| **Federation** | **~95%** | Measured 2026-10-09 (branch `agent/federation-95`, `main` at `00fe0c01`): Sytest's federation group is **103 of 105** (auth 19/20, make_join 3/3, send_join 9/9, send_leave 1/1, invites 10/10, room versions 7/7, key server 6/6, send-to-device 2/2, state 10/10, backfill 5/5, get_missing_events 3/3, the general federation API 14/14, device keys 8/9, the query API 5/5, public rooms 1/1; 78 on 2026-10-02, 15 on its first run); Complement's federation package **316/317 assertions, 89/90 tests**, restricted rooms, invites and knocks 18/18. The three left are not server bugs: Sytest's "Can invite unbound 3pid over federation" and "If a device list update goes missing" are races in the tests (the first starts its `/events` after the event arrived, on Synapse's code path too), and Complement's `TestDeviceListsUpdateOverFederationOnRoomJoin` is skipped by Synapse and Dendrite (no server sends a device-list update on a join). What that means: a user joins a room hosted elsewhere through the client API, including restricted rooms and through another server; messages flow both ways; history from before the join and the gap left by a leave-and-rejoin are fetched, the state at fetched history is asked of the server that sent it and every fetched event is auth-checked; invites, leaves and knocks cross servers, so do typing, receipts, presence, device lists, to-device messages and media; the outbound queue survives a restart and a destination down longer than its queue is caught up from the rooms; the key server answers `/key/v2/server/{keyId}` and the notary, server ACLs are enforced on every room-scoped endpoint and per PDU, version 1 and 2 rooms are joined and backfilled, federation redactions are applied. **It federates with a real Synapse**: a Myelin built from this tree and a Synapse 1.162.0 in Docker, over TLS with a private CA, pass the interop harness's 46 checks (keys and notary both ways, joins both ways with pre-join backfill, invites, leave/rejoin, kick, ban, redaction, typing, receipts, to-device, authenticated media both ways, public rooms, profile and directory queries both ways, device keys over federation, rooms of version 10–12 both ways, a restricted join Synapse authorised), and the same harness against a **two-replica Myelin on PostgreSQL** behind one TLS front (`REPLICAS=2`, 51 / 51: the 46 plus five through the second replica) passes too. Fixed on 2026-10-09: a server could not verify the events it had signed itself; a leave followed at once by a rejoin of an invite-only room let the rejoin through (the `make_join` overtook the leave in the outbound queue; every membership handshake for a room this server holds now waits for its own events to reach the other server first); and in a cluster, a PDU queued for a destination another replica sends for waited for that replica's next rescan, or forever (62 s to Synapse; now the owner is woken over the mesh, 21 ms). Rerun with `tests/federation-synapse/run.sh` (`REPLICAS=2` for the cluster). The Complement and Sytest numbers are the breadth measure; the interop run is the depth one |

Federation is the honest answer to "when could I use this": 103 of Sytest's 105 federation
tests and 89 of Complement's 90 pass, and the whole 46-check interop story (joins both ways,
membership, EDUs, media, public rooms, queries, room versions 10–12, a restricted join) passes
against a real Synapse 1.162.0, single and as a two-replica cluster on PostgreSQL (51 of 51), rerunnable with `tests/federation-synapse/run.sh`.
The tests still failing are races in the tests themselves or a behaviour no server implements,
each with its evidence in `docs/status/06-federation.md`.

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
