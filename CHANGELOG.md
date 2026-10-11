# Changelog

What Myelin can actually do, and when it learned to do it.

This file records capability, not commits — and only capability that was verified by running the
server, a real client, or a conformance suite, because this project has repeatedly found that
"implemented and tested" and "works" are different claims. Where something is built but not
reachable, or reachable but unproven, it says so.

Versions follow [semantic versioning](https://semver.org). Nothing is released yet.

## Unreleased

- A room's memory no longer follows its history: a room actor keeps `server.rooms.event_cache_size` recent events in memory (1,000 by default, hot) and reads the rest from the database, an unused room can be unloaded after `server.rooms.idle_unload_after`, and `/metrics` reports `process_resident_memory_bytes`, `hs_room_events_cached`, `hs_room_resident_rooms` and the cache's misses and evictions. A 50,000-event room went from 386 MiB to 114 MiB resident; what remains is the state store's copy of every event, still rebuilt on load (RFC 0024). (2026-10-10, `agent/room-memory`, decision 0042.)
- Every setting does something: `server.admin_contact` is published as `/.well-known/matrix/support`, `auth.password.enabled: false` stops password login (a password still confirms a sensitive change), `media.remote_media_retention` deletes cached remote media nobody asked for in that long, hourly; `server.report_stats`, `auth.enable_legacy_login`, `auth.session_secret(_file)` and `appservices.enabled` are gone, and a configuration that still has them starts with a warning (decision 0034). The web interface shows a user's server-wide message limit beside their override, and an offering's settings say what saving does to bridges people already have (2026-10-08, `agent/web-items`).
- A user's profile and a room alias of another server are read through this server (`GET /profile/{userId}`, `GET /directory/room/{alias}`), and so is another server's public room list (`/publicRooms?server=`); the federation `/query/profile` answers the stored display name and avatar, and the federation `/publicRooms` lists the rooms published to the directory.
- A join, leave, knock or invite that is not canonical JSON for its room version is `400 M_BAD_JSON` before its signature is checked; an unsigned invite is `403`.
- Backfill from events of another room answers nothing; `/state` and `/state_ids` at a rejected event are `404`; an event citing a rejected event is readable and reaches `/sync`.
- `tests/federation-synapse/run.sh`: a repeatable Myelin<->Synapse interop run (real Synapse in Docker, private CA, nginx terminating TLS for Myelin), one PASS/FAIL line per step of the basic federation story; clean-skips without Docker. Status 06 records the 2026-10-02 attempt.

Everything below exists on `main` and has never been tagged. The container image is published
continuously to `ghcr.io/brandon-dacrib/myelin` as `main` and `sha-<commit>`, and the Helm chart
to `oci://ghcr.io/brandon-dacrib/charts/hs` as a pre-release, `0.1.0-main.<run>.g<commit>`, that
pulls the `sha-<commit>` image from the same commit (`helm install --devel`).

### Encryption

- **A remote user's device list is kept here while a room is shared** (2026-10-02,
  `agent/e2ee-sytest`): fetched whole from their server's `/user/devices` on first need, kept
  current from `m.device_list_update` and `m.signing_key_update` (an update that skipped a
  position fetches the list again), and answers `/keys/query` without a request, so a client
  still gets keys while the other server is down. A device renamed or added is announced to other
  servers, `/keys/query` carries `unsigned.device_display_name`, a device created at login is a
  device-list change, and resetting cross-signing keys that are already set up asks for
  re-authentication (first-time setup does not). The client-server e2e routes answer under
  `/_matrix/client/unstable` too, which is where Sytest's cross-signing tests call them.
  Verified by `crates/hs-e2e/tests/remote_device_lists.rs` and the two-server tests in
  `crates/hs-cli/tests/federation_edus.rs` on the real binary; the Sytest re-run is still owed
  (`docs/status/08-e2ee.md`, 2026-10-02).

### Installing and administering it

- **The fuzz targets run on every push again.** The `fuzz` workflow had been red since it was
  split from `ci` on 2026-10-01: CI's prebuilt cargo-fuzz is a musl binary and cargo-fuzz builds
  for the triple it was compiled for by default, so every build targeted
  `x86_64-unknown-linux-musl`, where AddressSanitizer cannot link. The runner now builds for
  rustc's host, and a failed build prints its errors instead of its last three lines
  (2026-10-02, status 12; green run 37034728978).
- **A migration from Synapse keeps people's encryption, notifications and rooms on other
  servers.** The importer now also copies each device's end-to-end keys (identity, one-time and
  fallback), cross-signing keys with the signatures on them, server-side key backups under the
  same version numbers, push rules and pushers, sync filters under the ids clients cached, read
  receipts, and rooms the server's users joined over federation (started from the join, with the
  state Synapse held for it). Verified by running: a real Synapse 1.161 populated by real clients'
  requests was migrated into the real binary, which then answered `/keys/query` with the same
  signed keys, handed out the same one-time key, served the key backup, the push rules, the
  pusher and filter `0`, carried both receipts in `/sync`; and two real Synapses federating over
  TLS on one machine gave a server whose rooms on the other server were migrated, verified,
  served and written to. A room is copied a page at a time, and each room's events per second,
  bytes per second and the server's peak memory are logged and in `/metrics`; one room of
  100,000 events and 2,000 members was measured (`docs/status/13-config-compat-and-migration.md`).
- **An admin token can be narrower than an administrator** (2026-10-02): Settings, Admin
  tokens mints a token with a chosen set of the six scopes, each explained in a sentence, and
  shows it once; `hs admin-token create --scope bridges:read` does the same from a shell;
  `POST /api/v1/admin-tokens` is the API. A request outside the token's scopes is refused with
  `403 insufficient-scope` naming the scope, and the refusals are counted in
  `hs_admin_scope_refusals_total`. The mint and the revocation are in the audit log with the
  scopes. Verified on the real binary: a `bridges:read` token is served the bridge listings,
  refused the users list, and refused everything once revoked.
- **The federation destinations list sorts and pages past 50, and the cluster reports its
  heartbeat sequence** (2026-10-02, API only): `GET /api/v1/federation/destinations?sort=-failing_since`
  and the other fields the document lists, failing first by default; `GET /api/v1/cluster`
  carries `heartbeat_seq` and `drain_released_at_once_count`, and each replica its
  `heartbeat_seq`. The pages that would show them are not built yet.
- **Every admin API operation enforces the scope its OpenAPI document gives it** (2026-10-01):
  26 bridge operations took `admin:*` instead of `bridges:*` and `users.logout` `admin:write`
  instead of `moderation:write`; a test now asks the router about all 154 authenticated
  operations. The interface shows Rooms and Media to `moderation:read`. No token narrower than
  `admin:read`+`admin:write` can be minted yet, so this matters once scoped tokens exist.
- **An account can be edited after it is made** (2026-10-02, `agent/web-items`). "Edit" on a
  user's page grants or revokes server administrator, which until now was set only at creation,
  and offers display name, avatar and kind of account; only what changed is sent, and a field
  this server cannot change yet is refused beside the field in the server's own words rather
  than silently ignored. Verified against `hs serve` (`web/e2e-real/web-items.spec.ts`).
- **A bridge's registration can be edited and its connection tested from its page**
  (2026-10-02, `agent/web-items`). "Edit" changes the url, rate limiting and the namespaces as
  rules with an exclusive switch, explained in place, and the page lists the namespaces; "Test
  connection" pings the bridge now and says whether it answered, with the server's reason on the
  page when it did not. Verified against `hs serve` with a stub bridge
  (`web/e2e-real/web-items.spec.ts`).
- **The Overview shows the server's own health checks** (2026-10-02, `agent/web-items`).
  `GET /server/health`, which nothing called before, is a card at the top of Health: the
  overall state, each check named in words with what its state means, and an Attention row
  when the server is degraded or down. Verified against `hs serve`.
- **An account can be found by the exact email, phone or sign-in identity it holds, and Add
  user says as you type whether a username is free** (2026-10-02, `agent/web-items`). The Users
  page's "Find by email, phone or sign-in provider" opens the one matching account or says
  nobody has it; the username check says free, taken, or that this server cannot check in
  advance (its directory answers 503 today), honestly. Verified against `hs serve`.
- **A room's page shows its lifecycle** (2026-10-02, `agent/web-items`). An upgraded room says
  it is closed and links the room that replaced it, guest access is a fact in words, and Block
  asks why, showing the reason on the badge and keeping it with the room and in the audit log.
  Verified against `hs serve` with a room upgraded through the client API.
- **Five places that showed wire names now read in words** (2026-10-02, `agent/web-items`):
  the audit log's action filter suggests every audited action with its reading, a bridge
  deployment's phase is "Running", "Starting" or "Not running properly", a notice or message
  with no text says what it is, the sign-in page says who can sign in without naming a database
  flag, and a task's result keys are phrases. Verified against `hs serve`.
- **The management interface explains itself** (2026-10-01, `agent/web-admin-ui`). Every
  configuration setting carries a badge from the server's own classification (applies on save,
  needs a restart, or per replica from the file or environment), each class is explained once
  per section, and a save names the settings it applied and the ones waiting for a restart; the
  administered settings' descriptions were rewritten to say what each does and costs. Federation
  shows a destination in catch-up and what that means; the Migration page names all thirteen
  streams the importer copies and lists what a migration leaves behind before it starts; the
  bridge offering page shows whether each person has signed in, and an older bridge
  registration takes its provisioning secret from the Sign in tab. A deactivated account can be
  reactivated from its page. Verified against `hs serve` (`web/e2e-real/explained-pages.spec.ts`
  and the Configuration and bridge-offering suites, 9 of 9) and the mock suites.
- **The chart install is a CD gate.** CD installs the Helm chart on a kind cluster with the
  freshly built amd64 image, waits for Ready, reads the setup link from the pod log, checks
  `/health/ready` and the management interface through a port-forward, and creates the first
  administrator through the link, before any image tag or chart is published. The check is
  `deploy/helm/hs/ci/install-smoke.sh`, runnable by hand against any cluster; the packaged chart
  no longer includes `ci/`. Verified 2026-09-26 on a local kind cluster with the image built
  from the tree (17 s to Ready), with the published `main` image, and with a deliberately wrong
  image to see the failure path; the workflow step itself has not yet run on GitHub's runners.
- **The Helm chart is on the registry.** `helm install myelin oci://ghcr.io/brandon-dacrib/charts/hs
  --devel --set serverName=example.org` is a running server. Every push to `main` publishes the
  chart as a pre-release, `0.1.0-main.<run>.g<commit>`, whose default image is the one built
  from that same commit, after CD has pulled the chart back and checked that image exists; the
  package is public. Verified 2026-09-26, twice, on a real cluster: Ready in about two minutes,
  `/health/ready` answering from inside the cluster, the setup link in the pod's log. Upgrading
  the standing demo to it was refused, which found that the volume claim template carried the
  chart version and app version as labels, immutable in a StatefulSet and different in every
  published chart, so no upgrade from one published chart to the next would have worked; fixed
  the same day (an install made before it needs one `kubectl delete statefulset
  --cascade=orphan` before its next upgrade). `--devel` is needed until the first tag.
- **A locked-out administrator gets back in with one command.** `hs recover`, run where the
  server keeps its signing key (`kubectl exec <pod> -- hs recover`, `docker exec <container> hs
  recover`, `hs recover --data-dir ./data` on a host), signs a request with that key and prints
  a one-time link. Opening it shows the administrator accounts, takes a new password for one,
  signs out every session that account had, and signs the operator in. The link expires in
  fifteen minutes, works once, and a newer one replaces it; a server with no active
  administrator is handed its setup link instead. The key is the credential because holding it
  already means being the server, so this adds no new power; a request from any other key, or
  more than five minutes from the server's clock, or replayed, is refused with nothing said.
  Issuance and the reset are in the audit log and the server log, without the token or the
  password. Verified 2026-09-26 by a test that drives the real binary with a real key on disk
  through the whole thing, a stranger's key included, and by `npm run check` and a Playwright
  run of the page. `docs/recovery.md` is the runbook. Before this the way back in was a
  registration shared secret, a restart, a second administrator and a deactivation.
- **The install is reachable through an Ingress, and the setup link works through it.** The
  chart's Ingress and HTTPRoute route the management interface (`/admin/`), its API (`/api/v1`)
  and the Synapse-compatible admin API (`/_synapse`) on the client host, on by default
  (`ingress.admin`, `gatewayApi.admin`). Before, they routed `/_matrix` and `/.well-known/matrix`
  only, so the setup link the install notes tell an operator to open would have been a 404 on
  any cluster with an Ingress. Verified 2026-09-26 with a standing demo on a real cluster:
  Traefik, a Let's Encrypt certificate, a LAN hostname, every routed path answering over HTTPS,
  Prometheus scraping the chart's ServiceMonitor, and the setup page opened in a browser at the
  public address (`docs/status/12-platform-and-kubernetes.md`).
- **Installing on Kubernetes is one value.** `helm install myelin deploy/helm/hs --set
  serverName=example.org` is a running server: one replica, a volume holding the database, the
  signing key and media, probes, a Service, and the same one-time setup link in the pod's log.
  Verified 2026-09-26 on a real cluster with the published image: install to Ready, the real
  interface at `/admin/`, the first administrator made through the link, the pod deleted and
  the signing key unchanged, a `helm upgrade` that replaced the pod and the key unchanged again.
  Before this the chart demanded a hand-made signing-key Secret and a rendered config file and
  defaulted to an image tag that did not exist. Helm-managed settings now reach the server as
  environment variables, which outrank the database, so they hold on every upgrade and the
  interface shows them as pinned. Later the same day the chart reached the registry too (the
  bullet above).
- **A replica that is shutting down says so before it drains.** `/health/ready` answered 200
  for the whole of a shutdown, including a cluster drain of up to twenty seconds, so a Service
  kept routing new requests to a pod that was handing its rooms away. Readiness is withdrawn
  first now; liveness and ordinary requests are untouched. Tested through the real HTTP path.
- **Installation is one command, and the first administrator is one link.**
  `docker run -p 8008:8008 -v myelin:/data -e HS__SERVER__SERVER_NAME=example.org <image>` is a
  working server with no configuration file. While it has no administrator it logs a one-time
  setup link at every start; opening it asks for a username and a password and signs you in to
  the admin interface as the server's first administrator. Verified in a real browser against
  the real binary, from an empty data directory to the Users page showing the new account, and
  by a test that drives the real binary across restarts: same link until used, never offered
  after. Before this the route to an administrator was a shared secret, `hs register --admin`, a
  `curl` to `/login`, and a pasted token.
- **The admin interface is actually in the binary.** Every binary and every published image
  before 2026-09-21 served a placeholder at `/admin/` reading "the management interface has not
  been built into this binary yet": the interface existed in `web/` and nothing embedded it. The
  image now builds it, release builds fail rather than embed the placeholder, and CD refuses to
  publish an image whose `/admin/` is not the interface. It adds 0.9 MB.
- **An administrator can add people.** Registration is closed by default, and until 2026-09-21
  nothing in the admin API or the interface could create an account on a real server: the
  operation existed, and the real user directory had never implemented it, so it answered 503.
  The Users page now has "Add user": username, optional display name, a password you type or
  generate, and an administrator switch that says what it means. It ends on a hand-over view
  with the user ID and password to copy, because the password is about to be unrecoverable and
  still has to reach a person. Refusals land beside the field they are about.
- **`/sync` shows a user what they are allowed to see.** It applied no history-visibility check
  at all, so somebody who had left a room (or been removed from it) could be sent what was said
  after they went, and somebody joining a members-only-history room what was said before they
  arrived. It now applies the same per-event rule `/messages` always has.
- **`/sync` stopped losing things.** Four separate ways a client could silently miss events --
  a gap answered with the oldest page instead of the newest, a new room resumed from the wrong
  place, an event landing mid-response, presence not crossing a join -- plus one way it never
  waited at all. All found by reading a Complement log for mechanisms rather than totals.
- **Accepting an invitation brings you the room.** It used to bring your own join event and
  nothing else -- no state, so no `m.room.encryption`, and Element offered to send plain text
  into an encrypted room. The same went for coming back to a room you had left. Found by
  accepting an invitation in Element; a regression of the day's own `/sync` work, which no test
  here or in Complement had covered. Declining an invitation works again too (it had stopped
  moving the room to `leave`, which Complement did catch), and a room sent whole no longer says
  everything twice.
- **Devices find out that they are verified, and about each other.** Signing a device recorded
  no device-list change, so nobody re-fetched it: Element never learned the server had its own
  device's signature, flagged every message its own user sent as "not verified by its owner",
  and never offered key backup. `device_lists.changed` also never named the people you had just
  come to share a room with, or you -- which is how one of your devices hears of another. With
  these a new Element account gets a green shield and the backup prompt.
- **People have names in the rooms they make, and a direct chat is one.** Room creation wrote
  the creator's membership, and its invitations, without their profile -- the creator was a raw
  user ID to everyone in the room -- and dropped `is_direct`, so the invitee's client filed a
  direct chat as a room.
- **A client sees its own join in its very next `/sync`.** It could be answered from a moment
  before -- the room had the join, the feeds `/sync` reads did not yet -- which a bot or a test
  that asks `timeout=0` straight after joining would notice, and two tests did. A sync now waits
  for the feeds to have caught up with everything the rooms had published when it arrived.
- **The server stops when it is told to.** With anybody signed in it took up to thirty seconds,
  because graceful shutdown waited for every open `/sync` long-poll to run out its client's
  timeout: 29.3 seconds measured for a single idle client, which is Kubernetes' whole default
  grace period, and long enough that a quick restart found the database still locked. Waiting
  clients are now answered first, and ask again of whatever replaces this server.
- **The user directory no longer lets anyone list everyone.** A search finds people you share a
  room with and members of public rooms, as the specification requires; finding everybody is an
  explicit setting (`auth.user_directory_search_all_users`), off by default because bridged
  contacts are local accounts too.
- **The room page lists a room's members, and the Federation page lists the servers this one
  has tried to reach.** Members come from the room's current state with the name and avatar
  each member event carries, joined first. Destinations come from the outbound client's own
  backoff records -- when each last succeeded, since when it has been failing, when it was
  last tried and how long the backoff is -- and an administrator can reset one so the next
  request is tried at once. The Overview's last two "not implemented" panels are gone with
  these; it counts failing destinations, and says zero when there are none.
- **A lost phone and a forgotten password are an administrator's to fix.** A user's page lists
  their devices (it used to show an error there, against a real server), signs one out or all
  of them, and resets their password: the server's own password policy applies, everything is
  signed out unless asked otherwise, and the password reaches neither the audit log nor the
  event stream. Verified against the real binary: the signed-out phone's token stops working,
  the laptop's keeps working until the reset, the old password is refused and the new one
  signs in.
- **The Overview page has numbers on it.** Users, rooms, daily and monthly active users, and
  whether this is one server or a cluster, counted from the real stores. A number nothing can
  count yet is left out of the response and shown as a dash, never as zero, and the page's
  "nothing needs your attention" names what it was unable to check. Seen in a real browser
  against the real binary.
- **Logs are readable where logs end up.** A first boot logged 72 lines, 67 of them the storage
  engine reporting flushes; it logs five. The text format wrote ANSI colour codes into pipes and
  files, so `docker logs` and `kubectl logs` were full of escape sequences; colour is now for
  terminals, and `NO_COLOR` is honoured.
- **The Helm chart's defaults could never have worked, and now do.** It pulled an image from a
  repository that is not ours, and rendered no media path, so every upload failed under its own
  `readOnlyRootFilesystem: true`.

### Bridges

- **A bridge is sent typing, receipts, presence, to-device messages and device-list changes,
  each once.** MSC2409, MSC3202 and MSC4203 for a registration that asks for them, read from
  server-wide streams at a durable position per appservice, stored in the same transaction as
  the queued transaction body so a restart resends nothing and misses nothing; a paused bridge
  holds them and resume delivers them. Verified 2026-09-30 against the real binary with a
  stand-in bridge across a restart and a pause, and with a real mautrix-whatsapp in
  appservice-mode encryption, which received its device-list change, one-time-key counts, the
  ephemeral data and an `m.room_key_request` it handed to its Olm machine. Not yet done:
  `device_lists.left` is never filled (Synapse's gap too), and no cluster run of this pump.
- **Bridges are offerings, one per person, and the server deploys them.** RFC 0017
  (`docs/rfcs/0017-the-server-deploys-its-own-bridges.md`), built 2026-09-26 and run
  end to end against the real binary on 2026-09-27: an offering made through the admin API,
  a person's instance walking from requested to ready, its files rendered with its own
  tokens, the bot opening a direct chat with the sign-in steps, `@whatsappbot` answering a
  real invitation and `@bridges` taking commands, and a real heisenbridge started from the
  rendered registration reaching ready; the interface's offerings flow passes as a browser
  test against the real server. **On 2026-10-01 the in-cluster runtime ran too**: on a kind
  cluster with the chart, a heisenbridge offering made through the admin API with the
  `cluster` runtime went from requested to ready in 47 seconds with a real pod the operator
  deployed, its bot registered through the server, and removing it removed the pod, volume,
  Service and Secret (`deploy/operator/ci/kind-smoke.sh --heisenbridge`). The paragraph below
  describes the design. An
  administrator offers a bridge type (WhatsApp, with an image tag, who may use it, and whether
  it runs in this cluster or somewhere else); each person gets their own instance, with its own
  registration, ghosts, process and volume, by messaging the bridge's familiar address
  (`@whatsappbot:server`) or the manager bot (`@bridges:server`), or an administrator makes one
  for them. The manager inside the server (`crates/hs-bridges`) renders the instance's files with
  its own tokens, registers it, asks the operator to run it (a `Bridge` resource, which
  `hs operator` turns into a claim, a one-replica Deployment and a Service, and reports back),
  waits for the pod to be Ready and for the bridge to answer the server's ping, then has the
  instance's bot open a direct chat with its owner and send the sign-in steps. Ten new admin
  operations; the interface's Bridges section is offerings first, with everybody's instance,
  failed first, on the offering's page; the chart installs the operator and its RBAC by default.
  The register-a-bridge wizard below still exists, under Registrations, for a bridge somebody
  runs themselves.
- **Adding a bridge is a wizard, and it ends in a running bridge.** The interface's catalogue
  says what each of fourteen bridges is, what it needs and how to sign in to it (from the
  bridges' own documentation); choosing one renders the bridge's `config.yaml` and its
  registration from one set of choices, already pointed at this server, with the operator as
  the bridge's administrator, double puppeting through the bridge's own token and encryption
  in appservice mode; the Deployment step asks where each side is, so a bridge in Docker
  beside a server on a laptop needs no edited file; the Created page is a runbook that turns
  green on the bridge's first ping and then gives the sign-in steps with the bot's real Matrix
  ID. Verified 2026-09-25 with mautrix-whatsapp: added through the wizard in a real browser,
  started from the two files the page showed, connected in about seven seconds, MSC4190 device
  made, MSC3202 keys queried, encryption in appservice mode, "Bridge started". The bridge
  completed the wizard's 40-line config to 666 lines itself. No message has crossed it yet:
  signing in needs a phone. `docs/bridges/mautrix.md`. Found on the way and fixed: a ping that
  succeeded did not clear the error from the one before it.
- **A real bridge works.** heisenbridge, the IRC bouncer bridge, against the real binary and a
  local IRC server: it registers its bot, drives the server as an appservice, is sent every event
  in its rooms, answers commands, and relays both ways -- an IRC user appears in Matrix as a ghost
  the bridge created, and a Matrix message appears on IRC. The first thing it did failed:
  `/register` had no `m.login.application_service` branch, so on a server with registration
  closed (the default) a bridge could not create its own bot. It has one now, authenticated by
  the `as_token`, with no user-interactive auth, refusing a username outside the appservice's
  namespace with `M_EXCLUSIVE`. Reproduction in `docs/bridges/heisenbridge.md`.
- **"Add bridge" renders a real registration.** The wizard's catalogue is fourteen bridges
  people run, with their images, ports and needs; choosing one and reviewing produces a
  registration this server accepts as it is, with freshly minted tokens shown once, plus the
  registration file, a Compose service and a `Bridge` resource. Namespaces are written for the
  server's own name. Watched end to end in a browser: the bridge created from the wizard
  authenticated as its bot a moment later.
- **The Bridges section of the interface is real.** All thirteen appservice operations the
  interface calls are served from the bridge registry: the list and each bridge's health,
  backlog and registration; registering one from a registration file in either notation;
  pause, resume, ping, rotating its tokens, replaying dead-lettered transactions, editing and
  removing it. Every change is audited. Watched in a browser against the real server with a
  real bridge: pausing it held the next message, resuming delivered it and the bridge answered.
  A failed ping now makes a bridge's health say so (it used to read "healthy" beside the
  error), and every timestamp the admin API writes has its three millisecond digits (a whole
  second used to lose them).
- **The admin API says which accounts belong to a bridge.** `appservice_id` on a user was a
  documented gap; it is set on every account an appservice registers.
- **Bridges are sent what happens in their rooms.** An appservice could register, ping and be
  masqueraded through, and was sent no event, ever: the transaction scheduler delivered a queue
  nothing filled. Now a pump reads every room from a durable cursor, decides who wants each
  event by the rule bridges are written against (a bridge hears everything in a room its bot or
  one of its ghosts is in), and one worker per appservice delivers it, in order, with retries.
  The first start records where things stand rather than replaying a server's history into a
  bridge registered today. Verified with a test bridge receiving transactions from the real
  binary, including across a restart.
- **A server with a bridge configured starts more than once.** Every boot re-added the
  registration file to a registry that had kept it, and the second boot refused it as a conflict
  with itself.
- **A restart no longer silences every existing room.** Rooms loaded from disk never forwarded
  their events to the stream `/sync`, push and bridges follow; only rooms created in the running
  process did. After any restart, nothing said in a room that already existed reached anybody
  until they reloaded. Every test had created its rooms in the process that read them; the
  bridge test was the first to restart a server and then send something.

### Verified against a real client

- **Element Web works.** The browser client most Matrix users run signs in, lists rooms, sends and
  receives messages live between two independent sessions, propagates a display-name change to an
  already-open tab, and pages back through history. Screenshots in
  `docs/design/screenshots/`, reproduction in `web/element-testing/README.md`.
  - Re-verified 2026-09-21 with two Element sessions against the real binary, after the day's
    `/sync` changes: creating an encrypted room from Element's own dialog (which used to fail, and
    no longer does), inviting by user ID, accepting, and an encrypted reply read on the other
    side. That session found four bugs the test suites had not; see `docs/next-steps.md`,
    "Run 6, and what opening Element found". Not yet tried in Element: verifying one session from
    another, key backup restore, rooms of more than two.
- **End-to-end encryption works.** Two `matrix-rust-sdk` clients with encryption enabled exchange a
  message this server can never read: device and one-time keys upload, cross-signing bootstraps,
  keys are claimed atomically, the Megolm session establishes, and the recipient decrypts.
  `cargo test -p hs-loadgen --test real_client_encrypted`.
- **A scripted client does a day's work.** 28 steps against the real binary: register, log in on a
  second device, create a room, invite, join, sync, send both ways, set a display name, rename and
  re-topic, list members, page `/messages` backwards from a token `/sync` issued, filter, send read
  receipts, typing, and log out — then get refused when reusing the revoked token.
  `cargo test -p hs-loadgen --test real_client`.

### Conformance

- **Sytest: 548 of 772** (client-server 385 of 543, federation 78 of 105), measured 2026-10-02 on
  `main` at `09f24ee` with the night's seven branches merged; 448 the evening before, 407 that
  morning, never run before 2026-10-01. Thirty-three of the 190 failures are the suite's own
  timeouts under load and are expected to pass on a quiet machine. Per-test results, by name, in
  `docs/status/sytest/`.
- **Complement `csapi`: 317 of 384 assertions**, 78 of 106 top-level tests, measured 2026-09-26 (run 11, `9672d61`: the two "after joining new room" subtests of `TestMessagesOverFederation` moved to passing with the history before a join fetched; nothing moved the other way). Before that 314 of 384, measured 2026-09-21
  and again, identical by name, on 2026-09-26;
  241 of 370 that morning, 191 of 296 the run before that. The first run this project ever took was 125; the suite had never
  been run before that.
- **Complement federation package: 73 of 250 assertions**, 12 of 88 top-level, measured 2026-09-26; 59 of 246 (6 of 88) on
  2026-09-21, which was the first time the whole package ran rather than however far it got
  before crashing. Measured with certificate verification *on*, trusting Complement's CA the way
  a deployment trusts a private one, after the harness stopped disabling verification.
- **Spec coverage: 138 of 235 routes (58.7%)** — client-server 108/166, server-server 30/36.
  Generated from the route manifest the binary itself emits, so it cannot overclaim.
- **Sytest: 407 of 772 tests**, 317 fail, 48 skip, measured 2026-10-01 — the first time the
  suite ran at all. By feature group: client-server 59%, application services 40%, federation
  14%. Run in Docker on Sytest's own image with certificates verified against Sytest's CA
  (`tests/sytest/`; every test by name in `docs/status/sytest/`). It found two bugs, both fixed:
  a room member without the redact power level could redact anybody's message, and every
  password hash or login leaked 19 MiB of Argon2 working memory on glibc 2.36 (Debian 12), which
  took a test server past 10 GB.
- **Fuzzing:** all eight `cargo fuzz` targets (federation PDU, EDU, X-Matrix header, key server
  and `.well-known` parsers; media decoding, thumbnailing and multipart) ran ten minutes each,
  18.7 million inputs, no crash; CI fuzzes each for a minute on every push.

### Federation

- **A redacted message says what redacted it, and redactions meet their events in any order.**
  Every client read of a redacted event (`/sync`, `/messages`, `/event`, `/context`, `/state`,
  search) carries `unsigned.redacted_because` (the redaction) and `unsigned.redacted_by`; a
  redaction that arrives over federation before the message it redacts takes effect when the
  message comes, across a restart too; a member of a version-1 or -2 room can redact their own
  message (refused until now, on every server); and a redacted event is served to other servers
  redacted, not whole. Also fixed: a backward `/messages` from a `/sync` token left out the
  room's newest event. Between servers: `send_join` answers the state's whole auth chain (it was
  empty for a room whose auth events are all current state), `/event` and `/backfill` answer the
  spec's transaction shape, `make_join` refuses a room this server has left and a user of
  another server, a join through a server that leaves out `room_version` or lacks the v2
  `send_join` goes through, another server's refusal reaches the client as it came instead of
  a `502`, typing and receipts from a server a room's ACL bans are dropped, an event the auth
  rules reject is kept as rejected so a later reference to it is answered consistently, an event
  whose content hash fails is taken redacted (as the spec says) instead of refused, and the
  notary's key responses survive a restart. Verified 2026-10-01 with two real servers
  (`federation_room_versions.rs`, `federation_reads.rs`) and Sytest: federation 50 of 105 → 73
  of 105, the whole suite 448 → 486 of 772 (status 06, seventeenth session).
- **What Sytest's first run found between servers is fixed.** The key server answers the
  deprecated `/_matrix/key/v2/server/{keyId}` and acts as a notary (`/_matrix/key/v2/query`,
  both spellings): another server's keys from the cache inbound verification fills, co-signed,
  the last held response answered when the origin is down. A room's `m.room.server_acl` is
  enforced -- it was not, anywhere -- on every room-scoped federation endpoint and on each PDU
  in `/send`, counted in `hs_federation_acl_refusals_total{endpoint}`. A PDU the auth rules
  reject is answered `{}` in `/send`. Rooms of version 1 and 2 are joined over federation, from
  either side, and a redaction that arrives over federation is applied. Verified 2026-10-01
  with two real binaries (`federation_keys.rs`, `federation_room_versions.rs`) and Sytest:
  federation 15 of 105 → 50 of 105, the whole suite 407 → 448 of 772. Also fixed: IDs in
  outbound federation request paths are percent-encoded, so a version-3 room's invites, joins
  and leaves no longer fail on an event ID with a `/` in it (about half of them).
- **Restricted rooms, invites, leaves and knocks cross servers, and so do the ephemeral
  things.** A local user joins a restricted room without naming an authoriser, and through
  another server when nobody here may invite; invites (v1 and v2), leaves and knocks are served
  and sent between two servers; typing, receipts, presence, device-list changes, signing-key
  updates and to-device messages cross servers both ways, and in cluster mode an EDU taken by a
  replica that does not send for its destination is forwarded over the mesh to the one that
  does. Another server's media is fetched over the signed federation media endpoints (with the
  legacy fallback), served from the held copy afterwards, and this server's media is served to
  others as `multipart/mixed`. Spaces answer: the client `/hierarchy` walks `m.space.child` and
  asks a child's servers over federation. Measured 2026-09-30 on Complement's restricted-room,
  invite and knock tests: 16 of 18 (96 of 98 with subtests), from 5 of 18 four days earlier;
  the five space tests 5 of 5. Found on the way and fixed: the state-resolution adapter
  truncated every real `origin_server_ts` to `u32::MAX`, so a leave forked from a power-levels
  change lost to the join it superseded about half the time. Not yet: a rejoining server does
  not fetch what it missed while out, a destination down for longer than its queue is not
  caught up, and none of it has been tried against Synapse.
- **An event queued for a server that is down survives a restart, and arrives.** The outbound
  queue and every destination's retry state (failing since, next attempt, last error) are in
  the database: a PDU is written before any worker sees it and removed only when the
  destination accepted it, and the sender resumes every queued destination at start.
  Verified 2026-09-27 with two real binaries over TLS: the receiving server's port closed, a
  message sent, the sending server killed and restarted over its data directory, the admin
  API still showing the destination failing since the same moment with one pending event,
  the port opened, and the message arriving in the recipient's `/sync` exactly once. In
  cluster mode only the replica that owns a destination's shard sends to it; the others queue
  and do not send (scripted ownership in a test; not yet watched on a cluster).
- **A server down for longer than its queue holds is caught up from the rooms.** Each
  destination's outbound queue is bounded (`federation.max_queued_pdus_per_destination`,
  10,000); past that it is dropped and, once the destination answers, it is sent the latest
  event of each room it is behind in and fetches the rest itself, as Synapse does. Before
  this the queue had no bound. Logged, counted (`hs_federation_catch_up_*`) and shown in the
  admin API as `catch_up_since`. Verified 2026-09-30 with two real binaries over TLS: the
  receiving server stopped, eight messages sent against a bound of three, the server started
  again, and all eight in the recipient's history in order. An event the sender was never
  handed (a lagged update stream) is still reached only as an ancestor of a later one.
- Inbound transactions (`PUT /send/{txnId}`): content hashes and signatures verified against the
  *sender's* server, the spec's 50 PDU / 100 EDU limits enforced, processed in order, idempotent by
  transaction id.
- `make_join` and `send_join` build and authorize real joins against real room state and persist
  them; a remote's join reads back with the remote's own signature intact.
- Backfill resolves missing ancestors with layered limits (100 events per fetch, 10 rounds, 500
  events, 20 seconds), so a hostile peer cannot force unbounded work. Since 2026-09-26 the first
  request for a gap is the one every other implementation expects, `POST /get_missing_events`
  with this server's extremities and the event that exposed the gap; `/backfill` rounds follow
  only if that does not close it. Complement's reference server serves nothing else for this,
  so the loop could not begin against it before. Federation run 7 (`63c226f`): 75 of 250
  assertions and 14 of 88 tests, from 73 and 12, with `TestGetMissingEventsGapFilling` and
  `TestOutboundFederationEventSizeGetMissingEvents` moved to passing and nothing regressed.
- A real `/_matrix/federation/v2` router, replacing three v2 endpoints that had been registered
  under v1 with a literal `/v2/` path segment.
- Private certificate authorities can be trusted (`federation.custom_ca_certificates`); running
  without certificate verification remains possible and now warns loudly at startup.
- **A user joins a room hosted on another server, and messages flow both ways** (2026-09-25).
  `POST /join/{roomIdOrAlias}?server_name=` on a room this server does not hold runs the real
  `make_join`/`send_join` handshake against each named server in turn (a remote alias is
  resolved through its server's directory first), verifies every event that comes back, and
  makes the room resident from that snapshot (`docs/rfcs/0015`): the joining user's next `/sync`
  carries the room's state and their join, `/state` and `/members` read it, and it survives
  eviction and reload. Every event a local user sends in a room with remote members is then
  sent to those servers over `PUT /send/{txnId}` -- one worker per destination, fifty PDUs a
  transaction, in order, retried with backoff -- and a resident forwards a join it accepts to
  the room's other servers. Verified by two in-process servers over plain HTTP
  (`crates/hs-cli/tests/federation_two_servers.rs`) and by two real binaries over TLS with a
  private CA (`crates/hs-federation/scripts/two-server-federation.sh`): it passed on 2026-09-25, join through `/join`, state on B, a message each way. Between two
  instances of this server; a Synapse on the other end has not been tried. Not yet: EDUs,
  invites, leaves and knocks over federation; the outbound queue is in memory.
- **A server is served only the history it was there for** (2026-09-26). `/backfill` and
  `/get_missing_events` checked only that the requesting server had a member in the room and then
  served every event whole, so a server whose user joined a members-only room today could fetch
  everything said before. Both now serve an event the requesting server was not in the room for
  in its redacted form (still signed, still verifiable), per the room's history visibility as of
  the event, the way Synapse does. `/get_missing_events` also honours `min_depth`, a floor below
  which nothing is returned; it used to be parsed nowhere. Both tested end to end through the
  signed federation router.
- **An event that raced a member's join is visible to them** (2026-09-26). In a `shared` room a
  member could see an event if they were joined in the state at it or joined later in the
  timeline; an event sent on a branch that had not seen their join was neither, and was
  hidden from them or not depending on which server's events arrived first (the federation
  package's `TestNetworkPartitionOrdering` moved between two runs of the same code, and this
  was why). Joined when the event arrived counts now. Somebody who had left still does not
  see what came after they left. The federation package, re-run from the fixed commit, has the
  test passing again.
- **A rejoin goes through the room, not through a stale copy of it** (2026-09-26). A server
  holds its copy of a room after its last user leaves, and stops receiving events for it. A join
  made against that copy is a join against the room as it was then: authorized against rules
  that may have changed, citing extremities the room has moved past, and never bringing back
  what was missed. When nobody of this server is joined and members of other servers are, a
  join now goes through one of those servers exactly as a first join does, and the answer
  carries the room's current state. The two-server test has bob leave, alice rename the room
  while he is out, and bob rejoin: B's copy carries the new name at once, and A knows bob is
  back before the join returns. Also: a `/backfill` or `/get_missing_events` the other server
  refuses is now logged as a refusal, where the client used to read a `403` as "the remote has
  nothing".
- **A room joined elsewhere has its history** (2026-09-26). Until now the timeline of such a
  room began at the join: `/messages` backwards stopped there and said it was the start of the
  room, and `/sync` offered no `prev_batch` to ask from. Now a backward page that reaches the
  oldest event this server holds, while the room's history continues before it, fetches one
  batch of a hundred from a server in the room (`GET /_matrix/federation/v1/backfill`, every
  event verified as an inbound PDU is) before answering, and pages again: the events go into the
  timeline below everything held, in the resident's own order, with the state at each computed
  by walking back from the join (exact while the history is linear and within reach; the doc of
  `RoomActor::accept_backfilled_events` says where it is not), durable across reload, and
  published nowhere -- history is not news to `/sync`, push, a bridge or the outbound sender. A
  page that reaches the held edge with more history behind it keeps its `end`; one whose fetch
  brought nothing omits it, so a client is never handed the same token forever while a peer is
  down. Verified by the two-server test: a hundred and twenty messages sent before the join
  read back in three pages of fifty, newest first, down to the room's creation, with nothing
  arriving in the next incremental sync. Not done: the gap left by leaving a room and rejoining
  it later (history is fetched before the *oldest* held event, not into the middle), and the
  state at a backfilled event is walked, not asked for (`/state_ids`).
- **Fetched history has the sending server's state, and is authorized** (2026-09-30). The
  state at a backfilled batch is asked of the server that sent it (`/state_ids` at the oldest
  event, `/event` or `/state` for what is not held) and derived forward through the batch, so a
  topic or membership set long before the fetched history is there at every event; every
  backfilled event is checked with the same auth rules as an inbound one, and one they refuse
  is not stored. This server's own `/state_ids` answered empty for every room of version 3 or
  later and both state endpoints answered the state after the event instead of before it; both
  fixed. Verified with two real servers: B's `/context` state at a fetched message shows a topic
  set before anything B fetched.
- The joining side sends `?ver=` with every supported room version, which Synapse requires
  before it will hand out a join template, and carries the user's profile on the join.
- **Event signing was wrong from the beginning and is fixed.** The spec signs the *redacted* form
  of an event; this server signed the full one, so every event it ever originated would have been
  rejected by a compliant homeserver. Nothing caught it because the signer and the verifier shared
  the same wrong assumption.

### Client-server API

- **Room tags, and tags that follow a room upgrade.** `GET`/`PUT`/`DELETE
  /user/{userId}/rooms/{roomId}/tags[/{tag}]` exist (they did not), as the room's `m.tag`
  account data seen one key at a time, and reach `/sync` like any room account data; setting
  account data, a tag or a read marker now wakes a waiting `/sync` instead of letting it time
  out. Verified by Sytest's `42tags.pl` on the real binary (6/8; the two "tags copied to the
  new room" tests ran against a build from before that copy existed). Built and unit-tested,
  not yet run through Sytest: a user who joins an upgraded room's successor gets their tags
  and `m.direct` entry for the old room; `m.ignored_user_list` is honoured by `/sync` (an
  ignored user's messages and invitations are not delivered); the user directory counts a
  world-readable room as public, lets a user find themself while in a public room, and stops
  offering a public room's remote members once nobody local is in it; lazily loaded members
  are sent once per device (`include_redundant_members` asks for them again) and a gapped
  sync names whoever joined or left inside the gap; the sending device sees its
  `unsigned.transaction_id` in `/sync`; a filter naming something that is not a room or user
  id is rejected (status 05, session 14).
- **Push notifications are sent.** Every accepted room event is evaluated against each local
  member's push rules; a match that notifies counts toward `/sync`'s `unread_notifications`
  (per thread too), appears in `GET /notifications`, and is posted to each of the user's HTTP
  pushers with the badge; a read receipt zeroes the room and sends the new badge; a gateway's
  `rejected` pushkeys remove their pushers; a password change that logs the other sessions out
  removes the pushers they registered. Push rules accept any room or sender rule id, list by
  scope and kind (`GET /pushrules/global/`, `/pushrules/global/{kind}/`), and answer `400` for a
  malformed path or an unknown action as Synapse does. Verified by unit tests against a fake
  push gateway and the `/pushrules`/`/pushers` surfaces through the real binary (2026-10-02);
  the Sytest push group (19 of 53 before) has not yet been rerun on this build -- see
  `docs/status/10-push.md`, session 2.
- **`/joined_rooms` lists a room its caller just created or joined, every time.** It waits, as
  `/sync` has since the 2026-09-30 read-your-writes change, for the session hub to have consumed
  everything published before the request (bounded at 500 ms). Before, a client asking the
  moment `createRoom` returned could be short a room: Complement's `TestRoomState` saw it once in
  two runs, and the twenty-rooms-at-once test saw it on CI's arm64 runner on every push of
  2026-10-01 night. Verified by the real-binary test on 2026-10-02.
- **A room upgrade carries its bans, its directory entry and its federation closure**
  (2026-10-02). `POST /rooms/{roomId}/upgrade` sends every ban of the old room into the
  replacement (a banned user cannot follow the tombstone), lists the replacement in the room
  directory in the old room's place, keeps `"m.federate": false`, and gives a moderator who
  upgrades the room their old level back once the copied state is in. The copied state stands
  in for the preset's instead of following it. When one of this server's users joins a
  replacement another server made, this server's aliases for the old room and its directory
  entry follow, and the joining user's own account data on the old room comes with them: a
  direct chat stays one, and its tags come along (as in Synapse). Verified on the real binary;
  Sytest's room-upgrade file was 11 of 21 before (the count after is in status 04 session 18).
- **A redaction is judged by the power levels in force when it was sent** (2026-10-02), not
  when it is applied. A redaction that waits for its event (it arrived first, over federation or
  ahead of a backfill) or arrives late over federation now takes effect if its sender could
  redact *then* -- a moderator demoted since still redacts, a member promoted since does not.
  Verified on the real binary around a power-level change, and in unit tests for the waiting
  and the federation cases.
- **Rooms from before 2026-09-30 whose backfilled history held placed outliers answer the
  state at them again** (2026-10-02). Placement wrote no state row for an outlier it placed
  until the backfill-state work, so such a room's `/state_ids` at that event answered nothing
  and the state after it read as the event alone. A room load now gives each such outlier the
  state after the event before it, writes the rows back once the room is fenced, logs the count
  per room and counts it in `hs_room_outlier_state_rows_repaired_total`.
- **Every `/sync`'s presence and device-list scope is read from the member index**
  (2026-10-02): who shares a room with the syncing user comes from `hs_user.room_members`
  (the index the user directory already reads), bounded by the user's own joined rooms, instead
  of reading each shared room through its actor on every call.
- Rooms: creation, membership, state, timeline, `/messages`, `/context`, relations, threads, room
  upgrade, aliases, the room directory, redactions, and idempotent state sends and joins.
- **History visibility is enforced on every read path.** A user who has left a non-world-readable
  room can no longer read it — found by Complement, and the most serious privacy bug this project
  has had.
- **`/messages` says when it has reached the start of the room** (2026-09-26): the spec's signal
  is no `end` property at all, and this server sent one more token and then `"end": null`, which
  a client that paginates "until no `end` is returned" reads as a token and starts over. Found
  when Complement's `TestMessagesOverFederation`, joining over federation for the first time,
  did exactly that for thirty minutes.
- **Search works** (2026-10-01): `POST /search` finds messages, room names and topics in the
  rooms a user is joined to, only where the room's history visibility lets them see the event,
  by relevance or most recent first, a page at a time, with the messages around each result.
  The index lives in the server's own store and survives a restart without re-reading anything;
  a message is found the moment after it is sent. Verified against the real binary with two
  users and three rooms, across a restart, and by Complement's `TestSearch` (all six subtests). Not yet: history fetched from other servers after
  joining is not searchable, and words are not stemmed.
- **Rooms created at once are as many rooms** (2026-10-01): a version-12 room's id is the hash of
  its first event, and two `createRoom` calls by one user with the same request in the same
  millisecond built the same first event, so both were answered with one room -- the user was in
  one room where they had asked for two. Found by Sytest. The server now refuses an id a room
  already has when it writes the room's first event and builds another. Verified against the
  real binary with twenty identical creates sent at once (twenty rooms, every one in
  `/joined_rooms`; the first burst found 11 ids taken), and by Sytest: the three files holding
  the tests that failed from it went from 8-9 of 11 to 11 of 11, three runs in a row.
- **Guest access can be switched on** (2026-10-01): `auth.allow_guest_access` (off by default,
  applies at once from the Configuration page) lets a client ask for a guest account
  (`POST /register?kind=guest`). A guest may read world-readable rooms, join rooms whose guest
  access is "can join", and talk there, and nothing the spec does not list -- no `createRoom`,
  no invites, no uploads (`403 M_GUEST_ACCESS_FORBIDDEN`). A room that withdraws guest access
  sees this server's guests leave it. A guest becomes a full account by registering with its
  guest token and its own name. Guests are marked in the admin API (`is_guest`) and on the
  Users page. Verified against the real binary and by Sytest's guest tests (0 → 23 of 24).
- **Inviting by email address works** (2026-10-01) once an administrator names the identity
  servers this server may use (`auth.identity_servers`, empty by default, which refuses such
  invites `M_THREEPID_DENIED`). An address its owner has bound is an ordinary invite of them; an
  unbound one is stored with the identity server and held in the room, and becomes an invite --
  checked against the identity server's signature and keys -- when the address is bound
  (`/3pid/onbind`) or claimed with `third_party_signed` on a join. Verified against the real
  binary with a fake identity server, and by Sytest (3PID group 3 → 10 of 19). Not yet: a bound
  invitation for a room on another server.
- **The deprecated event stream answers** (2026-10-01): `GET /events`, `GET /initialSync` and
  `GET /rooms/{roomId}/initialSync`, read from the same feed as `/sync`, were 404; Sytest's
  helpers wait on `/events` in tests about other things (client-server group 319 → 362, whole
  suite 407 → 458 of 772 with guest access, 3PID invites and the two fixes below).
- **Paging back from a sync token starts with what the sync showed** (2026-10-01): a backward
  `/messages` page from a `/sync` `next_batch` left out the newest event the sync had just
  shown; Sytest pages that way.
- **Two rooms created in the same millisecond are two rooms** (2026-10-01): from room version
  12 a room ID is the create event's hash, and one user creating two rooms with the same
  settings at once got one ID, the second written over the first.
- Sync: `/sync` v2 with filters that honour event-type and sender rules, lazy-loaded members, room
  summaries with heroes, typing, presence, read receipts and `m.fully_read`, to-device messages,
  device lists, one-time-key counts, and push rules as account data.
- Profiles that propagate: changing a display name re-stamps the user's membership in every room
  they are joined to, bounded so a user in a thousand rooms does not produce a thousand concurrent
  writes.
- Auth: registration and UIA, login (case-insensitively, matching Synapse), refresh tokens with
  reuse detection, devices, logout that actually deletes the device, and shared-secret admin
  registration compatible with `register_new_matrix_user`.
- E2EE: device and cross-signing keys, atomic one-time-key claims, to-device messaging, key
  backups, device-list tracking — and **cross-signing signatures are cryptographically verified**
  rather than stored on trust.
- Media: upload and download, thumbnails, async upload (MSC2246), URL previews with an SSRF guard
  that refuses private and cloud-metadata addresses, content scanning whose verdict survives a
  crash.
- `.well-known` discovery for clients and servers, served only when configured rather than
  pointing at itself.
- CORS on the Matrix API, without which no browser client could talk to this server at all.

### Operations

- **A room's owner writes a message's fan-out to its members in a few store transactions, and
  sync history has a retention.** The session hub used to read and write each member's feed
  records one store round trip at a time before waking anyone (8 s of the wake latency in a
  303-member room, status 05 session 12); it now reads a room's memberships with one multi-get
  and writes the feed in transactions of up to 100 members. Measured 2026-10-02 on the real
  binary, same build both ways, 302 members: on the embedded store 10.2 ms to 4.1 ms per update
  over a run of 353 updates (15,100 transactions to 200), on PostgreSQL in Docker with 22
  members 794 ms to 292 ms per update, on a desktop at load 25-37; the remaining PostgreSQL cost
  is one statement per written row at commit (RFC 0021). Each user's feed and the server-wide
  hot-room stream are now kept to `server.sync.feed_retention_entries` (10,000) and
  `server.sync.hot_room_stream_retention_entries` (100,000), hot settings; below the kept part
  each room's last position stays, so a client with an older token is sent a room whole rather
  than anything missed (decision 0026). Verified on the real binary: the startup line, a
  `PATCH /api/v1/config/server` taking effect at once, and `hs_user_pruned_entries_total`
  counting what went.
- **A first boot is as quick as any other.** Over an empty data directory the embedded backend
  used to create one Fjall keyspace per table, 109 of them, each several fsyncs under a global
  lock; every table now lives behind a prefix in one shared Fjall keyspace (decision 0024), so a
  first boot creates one. Measured 2026-10-01 on the project's desktop under load, launch to
  `listening`, five runs each: debug 8.8 s to 0.72 s, release 9.4 s to 0.62 s; a later
  boot was and is about half a second. Existing data directories keep their layout and need
  nothing. The `listening` line says `boot_ms`, `cold` and `keyspaces_created`, and
  `hs_boot_duration_seconds{cold}` is on `/metrics`. Verified with the real binary, including a
  `SIGKILL` right after the first boot and a restart that finds the account registered before
  it.
- **Two pods on a real cluster, and what is between them crosses.** Two replicas ran as pods
  on the owner's cluster on 2026-09-28 with a CloudNativePG database and SeaweedFS media: a
  client's `/sync` from either pod, rooms handed between them during a rolling update and a
  pod loss, drain and undrain through the admin API and the Cluster page. That run found
  requests landing mid-handoff failing (322 during the rolling update, 7 of 240 in the
  failover); on `main` since 2026-09-30 such a request waits for the new owner instead
  (decision 0017), typing, receipts and presence cross replicas on the wake the room owner
  already sends (decision 0018, verified as two real `hs serve` on PostgreSQL), and a replica's
  `/metrics` has the `hs_cluster_*` series. The pods have not yet run with that image.
- **PostgreSQL over TLS.** `storage.postgres.ssl_mode` is libpq's five modes over rustls with
  `ssl_root_cert`, `pool_size` and `schema` reach the connection, the chart renders `sslMode`
  and the operator's `Homeserver` has it; `require` against a plain server fails at startup
  naming the setting. Verified 2026-09-30 with the real binary in every mode against a TLS
  PostgreSQL and a plain one. An empty password used to be rendered as `password=` and rejected.
- **`/sync` never repeats an event across two batches, and a join is in the very next sync.**
  A batch carries the rooms with a feed entry at or before its token and each room's timeline
  stops where it was then; a 300-event writer racing a syncing device repeated 159 before and
  none now (2026-09-30). Members of a room that crossed the fan-out threshold used to be left
  "cold" and hear nothing more from it; fixed the same day.
- **Every setting says when a change applies, and most apply at once** (2026-10-01). The
  Configuration section, the admin API (`applies` per setting) and `docs/config.md` give each of
  the 71 settings as bootstrap (7), hot (39) or restart (25), from one table a test keeps
  complete. Newly applied on the running server: every rate limit -- login and registration per
  client address, joins per user, administrators' redactions and inbound federation per origin
  are now enforced at all -- registration on or off, the user directory's search-everyone switch,
  token lifetimes and the password policy, the media upload limit, URL previews and thumbnail
  sizes, the `.well-known` documents and public base URL, `/versions`' unstable features, and
  two federation switches. Verified on the real binary by changing each through the admin API
  and seeing the next request answered under it.
- **Configuration history, revert and hot reload.** The section page shows each setting's old
  and new values and who changed it, with a revert; message rate limits, the federation allow
  and block lists and the log level apply on the running server, and a save says what applied
  and what waits for a restart; other replicas pick a change up within ten seconds. Verified
  2026-09-29 with real-binary tests and five browser flows against the real server.
- **The admin API is complete at the handler level: 160 of 160 operations** (2026-09-29,
  `tools/admin_api_coverage.py`), reports, tasks, statistics, media (listing, quarantine,
  deletion as a cancellable task), registration tokens, server notices, federation keys and
  shared rooms among the last; the interface's Reports, Tasks, Statistics, Media, Cluster and
  Migration pages are real. Handler coverage is a ceiling, not a conformance claim.
- **Replicas know their own address, speak mutual TLS to each other, and a room is created
  by the replica that owns it.** A replica advertises the address it is configured with (the
  chart gives each pod its stable DNS name under the headless Service, so one wildcard
  certificate covers them all); the mesh between replicas is mutual TLS with a CA the
  operator provides, and a replica presenting a certificate from any other CA is refused on
  every call. Creating a room, joining or knocking by room id is routed to the shard's owner
  first, so a room's first actor is never built on a replica that does not own it. Verified
  2026-09-27 as three processes on one PostgreSQL with a private CA: rooms created through
  one replica and forwarded to their owner, concurrent sends through two replicas, identical
  history on both, and the third replica's foreign certificate refused. Not yet run as pods;
  the chart's cluster templates have not been rendered on this side, and
  `values-two-replica-experiment.yaml` is the values file for that run.
- **A second replica is capacity, not only availability: `/sync` works from any replica.**
  A room's owner wakes every other replica over the mesh after each update, the replica
  holding the client's long-poll answers it, and a sync waits (within half a second) for
  everything its peers had published before it arrived, so a write through one replica is in
  the very next sync on another. Verified 2026-09-27 as two processes on one PostgreSQL: every
  cross-replica long-poll woken with the event, 160 of 160 writes seen in the next sync on the
  other replica, about 150 ms from write to woken sync. Not yet run as two pods; typing,
  receipts and presence still stay on the replica that received them.
- **Runs on PostgreSQL.** Boots, registers, serves, and survives a restart with its data intact.
  The embedded single-node backend remains the default.
- **Two replicas no longer fork a room's history.** A shard gate forwards or refuses requests for
  rooms this replica does not own, with a fencing check inside the transaction that commits a
  write. Before this, concurrent sends through two replicas silently produced two divergent
  histories with no error to any client.
- Admin API: 71 of 158 operations genuinely served (`tools/admin_api_coverage.py`, 2026-09-27) — users, rooms, moderation actions, bridges and offerings, configuration, recovery, a durable
  audit log, and an SSE event stream. Every mutation writes exactly one audit entry and publishes
  exactly one event. The rest answer `501`, or `503` naming the capability when a seam exists but
  nothing implements it.
- A management web interface that reads real data and says "not implemented on this server yet"
  rather than showing an empty table.
- Five read-only `/_synapse/admin` routes, so existing Synapse tooling works, forwarding into the
  native admin API rather than reimplementing it.
- Synapse configuration translation with a table that records, per option, whether it is mapped,
  mapped with a difference, or unsupported.
- A distroless container image that runs as a non-root user, a Helm chart, and a Kubernetes
  operator that has reconciled against a real API server.

### Build and release

- CI on amd64 and arm64: fmt, clippy with `-D warnings`, the full workspace test suite, and a
  dependency audit; since 2026-09-30 also the web interface's lint, types, unit tests, build and
  the mock-backed Playwright suite (until then CI ran no web checks at all).
- CD publishes multi-architecture images with an SBOM and build provenance, and **refuses to
  publish an image that has not booted and answered `/health/live` and `/_matrix/client/versions`
  on both architectures**. Releases are gated on CI being green for that exact commit.
- **CD runs the operator against a real API server before tagging an image.** After the chart
  install smoke, the amd64 image leg runs `deploy/operator/ci/kind-smoke.sh --homeserver` on
  the same kind cluster: a `Bridge` through Ready, Degraded (a missing image, with the
  kubelet's reason), Ready again and deletion, and a single-node `Homeserver` through Ready,
  an image roll and deletion. Its first run (2026-10-01) found the `Bridge` controller taking
  a `Homeserver`'s pods for a bridge's when both ran in one namespace; fixed.
- **Binaries for Linux (amd64, arm64) and Apple silicon have been built and booted.** They are
  attached to a release on a `v*` tag, and since 2026-10-01 a manual dispatch of CD with
  `binaries=true images=false` runs the same matrix as a dry run (no tag, no release, archives
  as workflow artifacts). Its first run (36808313763) built the web interface with Node 22 on
  each runner, embedded it, and booted each binary: `/health/live` answered, `/admin/` was the
  interface and the log offered a setup link, in 7.5, 8.5 and 15 minutes. The old check,
  `hs --version || true`, had always passed because `hs` has no `--version`.
- The Helm chart is published as an OCI artifact on every push to `main` (a pre-release, for
  `helm install --devel`) and on a `v*` tag. After a release, `main`'s pre-releases move past it
  by themselves (`0.1.1-main.N` after `v0.1.0`), so `--devel` keeps getting `main` without a
  Chart.yaml bump (`deploy/helm/hs/ci/chart-version.sh`, self-tested in CD). No `v*` tag exists
  yet.
- The published `main` image was pulled from GHCR and run as a new user would: generate a config,
  start the container, health up in about a second, then register a user, call `/account/whoami`
  and create a room. All of it worked. Getting there took four steps and an edit to a 158-line
  YAML file, which is the measured version of the complaint in `docs/next-steps.md` item 2.

### Numbers

27 crates, ~249,000 lines of Rust (`wc -l` over `crates/**/*.rs`, tests included), 2,436 passing
tests (2026-09-30), plus a TypeScript management interface with 443 unit tests and 50 mock-backed
browser flows, both run by CI.

## Notes on how this was built

Myelin was built by a fleet of specialist agents working in parallel on separate tracks, with an
integration lead reviewing, verifying and committing their work. The rules that emerged are
recorded in `docs/workstreams/README.md` and the conventions section of `docs/next-steps.md`. The
short version, because it shaped everything above: **verify by running**. Every serious bug this
project found came from a real client, a conformance suite, or a real deployment — never from its
own tests.
