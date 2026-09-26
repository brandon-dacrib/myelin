# Changelog

What Myelin can actually do, and when it learned to do it.

This file records capability, not commits — and only capability that was verified by running the
server, a real client, or a conformance suite, because this project has repeatedly found that
"implemented and tested" and "works" are different claims. Where something is built but not
reachable, or reachable but unproven, it says so.

Versions follow [semantic versioning](https://semver.org). Nothing is released yet.

## Unreleased

Everything below exists on `main` and has never been tagged. The container image is published
continuously to `ghcr.io/brandon-dacrib/myelin` as `main` and `sha-<commit>`.

### Installing and administering it

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

### Federation

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
  so the loop could not begin against it before. Unit-tested; not yet re-measured.
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
- **An event that raced a member's join is visible to them** (2026-09-26). In a `shared` room a
  member could see an event if they were joined in the state at it or joined later in the
  timeline; an event sent on a branch that had not seen their join was neither, and was
  hidden from them or not depending on which server's events arrived first (the federation
  package's `TestNetworkPartitionOrdering` moved between two runs of the same code, and this
  was why). Joined when the event arrived counts now. Somebody who had left still does not
  see what came after they left. Unit-tested; the federation package has not been re-run
  from the fixed commit yet.
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
- The joining side sends `?ver=` with every supported room version, which Synapse requires
  before it will hand out a join template, and carries the user's profile on the join.
- **Event signing was wrong from the beginning and is fixed.** The spec signs the *redacted* form
  of an event; this server signed the full one, so every event it ever originated would have been
  rejected by a compliant homeserver. Nothing caught it because the signer and the verifier shared
  the same wrong assumption.

### Client-server API

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

- **Runs on PostgreSQL.** Boots, registers, serves, and survives a restart with its data intact.
  The embedded single-node backend remains the default.
- **Two replicas no longer fork a room's history.** A shard gate forwards or refuses requests for
  rooms this replica does not own, with a fencing check inside the transaction that commits a
  write. Before this, concurrent sends through two replicas silently produced two divergent
  histories with no error to any client.
- Admin API: 34 of 145 operations genuinely served (`tools/admin_api_coverage.py`) — users, rooms, moderation actions, a durable
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
  dependency audit.
- CD publishes multi-architecture images with an SBOM and build provenance, and **refuses to
  publish an image that has not booted and answered `/health/live` and `/_matrix/client/versions`
  on both architectures**. Releases are gated on CI being green for that exact commit.
- Binaries for Linux (amd64, arm64) and Apple silicon, plus the Helm chart as an OCI artifact, are
  wired to `v*` tags and have not been exercised yet.
- The published `main` image was pulled from GHCR and run as a new user would: generate a config,
  start the container, health up in about a second, then register a user, call `/account/whoami`
  and create a room. All of it worked. Getting there took four steps and an edit to a 158-line
  YAML file, which is the measured version of the complaint in `docs/next-steps.md` item 2.

### Numbers

26 crates, ~154,000 lines of Rust (`wc -l` over `crates/**/*.rs`, tests included), 1,678 passing tests, plus a TypeScript management interface
with its own unit and end-to-end suites.

## Notes on how this was built

Myelin was built by a fleet of specialist agents working in parallel on separate tracks, with an
integration lead reviewing, verifying and committing their work. The rules that emerged are
recorded in `docs/workstreams/README.md` and the conventions section of `docs/next-steps.md`. The
short version, because it shaped everything above: **verify by running**. Every serious bug this
project found came from a real client, a conformance suite, or a real deployment — never from its
own tests.
