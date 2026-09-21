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
- **The user directory no longer lets anyone list everyone.** A search finds people you share a
  room with and members of public rooms, as the specification requires; finding everybody is an
  explicit setting (`auth.user_directory_search_all_users`), off by default because bridged
  contacts are local accounts too.
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

- **Complement `csapi`: 301 of 384 assertions**, 76 of 106 top-level tests, measured 2026-09-21;
  241 of 370 that morning, 191 of 296 the run before that. The first run this project ever took was 125; the suite had never
  been run before that.
- **Complement federation package: 59 of 246 assertions**, 6 of 88 top-level, and for the first
  time the whole package rather than however far it got before crashing. Measured with
  certificate verification *on*, trusting Complement's CA the way a deployment trusts a private
  one, after the harness stopped disabling verification.
- **Spec coverage: 138 of 235 routes (58.7%)** — client-server 108/166, server-server 30/36.
  Generated from the route manifest the binary itself emits, so it cannot overclaim.

### Federation

- Inbound transactions (`PUT /send/{txnId}`): content hashes and signatures verified against the
  *sender's* server, the spec's 50 PDU / 100 EDU limits enforced, processed in order, idempotent by
  transaction id.
- `make_join` and `send_join` build and authorize real joins against real room state and persist
  them; a remote's join reads back with the remote's own signature intact.
- Backfill resolves missing ancestors with layered limits (100 events per fetch, 10 rounds, 500
  events, 20 seconds), so a hostile peer cannot force unbounded work.
- A real `/_matrix/federation/v2` router, replacing three v2 endpoints that had been registered
  under v1 with a literal `/v2/` path segment.
- Private certificate authorities can be trusted (`federation.custom_ca_certificates`); running
  without certificate verification remains possible and now warns loudly at startup.
- Two instances of this server complete a real join handshake over TLS —
  `crates/hs-federation/scripts/two-server-federation.sh`. One direction only; the other needs a
  room-bootstrap API (`docs/rfcs/0015`).
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

26 crates, ~126,000 lines of Rust, 1,562 passing tests, plus a TypeScript management interface
with its own unit and end-to-end suites.

## Notes on how this was built

Myelin was built by a fleet of specialist agents working in parallel on separate tracks, with an
integration lead reviewing, verifying and committing their work. The rules that emerged are
recorded in `docs/workstreams/README.md` and the conventions section of `docs/next-steps.md`. The
short version, because it shaped everything above: **verify by running**. Every serious bug this
project found came from a real client, a conformance suite, or a real deployment — never from its
own tests.
