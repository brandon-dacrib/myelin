# Where this is, and what comes next

Written 2026-09-20 by the integration lead, last revised 2026-09-21. `PLAN.md` is the design and rarely changes; this file is the resume point and changes every session. `docs/status/dashboard.md` is the generated measurement; per-track detail lives in `docs/status/NN-*.md`.

The project is **Myelin**, and it is public: <https://github.com/brandon-dacrib/myelin>. The crates still carry the `hs-` prefix from before it had a name.

## The state of things

**A real client works.** Element Web — the actual browser client most Matrix users run — signs in against this server, shows a room list, sends and receives messages live between two independent sessions, propagates a display-name change to an already-open tab, and scrolls back through history. Screenshots in `docs/design/screenshots/`, reproduction in `web/element-testing/README.md`. That was the project's stated definition of success from day one. The loud exception is closed: `/createRoom` merged its power-level override backwards, which made creating a room from the UI fail unconditionally, and it no longer does. `unsigned.prev_content` and `GET /account/3pid` went with it. **None of the three has been re-checked against a real Element session** — the unit and end-to-end tests cover them, and nobody has opened a browser since.

**Configuration lives in the database** (RFC 0016). The file is a bootstrap and a seed; the database outranks it, `HS__` variables outrank the database, and the admin API refuses a write the environment would shadow rather than storing one that gets ignored. The web interface has a Configuration section that builds its forms from the server's own JSON Schema, and `hs config show|get|set|unset|import|export|history` is the same thing without a browser.

**A first run is one command, and the first administrator is one link.** `hs serve --data-dir ./data --server-name example.org` in an empty directory produces a working server — database, signing key, media path, all underneath that directory — and so does `docker run -p 8008:8008 -v myelin:/data -e HS__SERVER__SERVER_NAME=example.org <image>`, which CD now boots verbatim before it will publish. It was 158 lines of generated YAML with four mandatory hand-edits.

While the server has no administrator it logs a one-time setup link at every start (`/admin/setup#token=...`, `hs_auth::setup`). Opening it asks for a username and a password and signs you in as the first administrator. Watched working in a real browser against the real binary on 2026-09-21, from an empty directory to the Users page showing the new account. It was: configure a shared secret, `hs register --admin`, `curl /login`, paste a token.

**The admin interface ships.** Until 2026-09-21 it did not: every binary and every published image served a placeholder at `/admin/` saying the interface had not been built in, because nothing embedded `web/dist`. `crates/hs-admin/build.rs` now stages the built interface (or the placeholder, for a Rust-only checkout, and says so at startup); release builds set `HS_ADMIN_WEB_DIST` and *fail* without a built interface; CD refuses to publish an image whose `/admin/` is not the interface. Verified on the published artifact: `ghcr.io/brandon-dacrib/myelin:main`, pulled from the registry on 2026-09-21 and run with the README's exact command, serves the interface at `/admin/`, answers `needs_setup: true`, and logs the setup link. What has still never run is the `v*` binaries job's new Node step, which only a tag exercises.

**Complement, `csapi`: 289 of 384 assertions pass** (72 of 106 top-level), measured 2026-09-21 at
`213bc77`. The same morning it was 241 of 370 (61 of 104); before that 191/296, 148/293 and
125/293. The denominator grew because the harness image now configures Complement's shared
secret, which un-skipped two tests: `TestCanRegisterAdmin` passes, and `TestServerNotices` runs
for the first time and fails, since server notices do not exist here.

Read a run by name, not by total: `python3 tools/complement_triage.py <log>` prints each failing
test with its first reason, `--diff` lists what moved against
`docs/status/complement-csapi-results.txt`, and `--write-baseline` makes a run the new baseline.
The total hides things. One of the day's runs went *up* by three while a test went from PASS to
FAIL, and that was a real regression.

**What the wobble was.** Two consecutive runs of the same commit used to differ by a top-level
test in each direction -- `TestRoomsInvite` and `TestPushSync` -- and this file called that
noise: subtests run in parallel and lose races under load. They did lose races, but the load was
ours. The log had waits that saw **4,381 and 12,040 `/sync` responses** in five seconds: for any
user whose own presence record was the newest they could see, `/sync` returned at once, empty,
with an unmoved token, forever. Which user that was depended on who synced last, which is
exactly what made it look random. When a number wobbles, look for the mechanism before filing it
under noise; "Seen 4381 /sync responses" had been in every log.

**What fixing it uncovered.** `/sync` had been resending every room's entire state on every
incremental sync, and that was quietly papering over four other defects, each of which surfaced
as a named Complement regression the moment the one before it was fixed:

- A client that missed more than one page of a room was sent the *oldest* page and a token
  positioned at the end. The rest was never delivered, and `prev_batch` pointed the wrong way.
- A room created or joined after the client's token was resumed from the user's own join, so the
  create event, the power levels and that join were in no timeline at all.
- An event landing while a sync response was being built was folded into a feed entry the
  response had already reported as consumed. It was in no timeline, ever.
- Presence has one sequence for the whole server while a record's audience grows as people join
  rooms, so neither the joiner nor the people already there were told about each other.

All four are fixed, each with a test that states the invariant rather than the instance (*the
token a sync hands back must not itself count as news*; *say how far you have read before you
read*). What is left of the same family, known and not yet done: a very large ("hot") room
joined after the token is still resumed from the join rather than sent whole, and a requester
with no device -- some appservice callers -- never records a feed cursor at all.

**Complement, federation package: 59 of 246 assertions** (6 of 88 top-level), measured 2026-09-21 — and for the first time that is the *whole* package. The suite used to segfault Complement's own Go binary 21 tests in and silently discard everything after, so every federation number before this one was "however far it got before dying". There are no panics in the log now and `-skip` is retired.

**Spec coverage: 138 of 235 routes (58.7%)** — client-server 108/166, server-server 30/36. Generated from the manifest the binary emits, so it cannot overclaim. Registered still is not the same as working.

**It ships.** CI is green on amd64 and arm64. CD publishes a multi-arch image to `ghcr.io/brandon-dacrib/myelin`, and refuses to publish one that has not booted and answered `/health/live` and `/_matrix/client/versions` on both architectures. Binaries, the Helm chart and a GitHub release are wired to `v*` tags and have not been exercised — tagging `v0.0.1` is how you find out whether they work.

Reproduce the conformance numbers:

```
./tests/complement/build.sh complement-hs-reimplement:dev
cd refs/complement
COMPLEMENT_BASE_IMAGE=complement-hs-reimplement:dev go test -v -timeout 30m ./tests/csapi/...
COMPLEMENT_BASE_IMAGE=complement-hs-reimplement:dev go test -v -timeout 30m -skip 'TestInboundCanReturnMissingEvents' ./tests
```

A cold image build is ~4 minutes idle, up to 19 under load; each suite run is ~13-15 minutes.

Both numbers above are from these commands, run on 2026-09-21 against this code.

The re-measurement paid for itself immediately: `TestRoomCreate` still failed after `/createRoom`
was fixed, and chasing why found that `invite_state` omitted the invitee's own `m.room.member`
event — 100 of the 127 missing-key failures in the whole suite, and the largest single cluster in
it. Fixing that turned seven top-level tests green in one commit (`TestRoomsInvite`,
`TestRoomCreate`, `TestRoomSummary`, `TestLeaveEventInviteRejection`,
`TestTentativeEventualJoiningAfterRejecting` and both `TestFetchHistoricalInvited*`) and moved
csapi from 221 to 239 assertions. Nothing regressed.

**One conformance failure is a deliberate refusal.** `TestInboundCanReturnMissingEvents` now runs
to completion and fails on content: its first four events are exactly right, and everything after
is shifted by one because we also send `m.room.guest_access` for a `public_chat` room. The spec's
`createRoom` preset table says `public_chat` sets `guest_access: forbidden`; Synapse appears to
skip the event when it would set the default, and Complement is written to Synapse. Dropping a
spec-mandated event to win four subtests is bending the server to the test, so it stands. It is a
two-line change if the conformance points are wanted instead.

## How far along is this?

A number, because it gets asked. **Roughly 55-60% of a homeserver somebody else could run** --
but the number is only meaningful broken up, because the parts are nowhere near each other:

| Area | Where it is | Basis |
|---|---|---|
| Client-server API | ~70% | 289/384 csapi assertions, 72/106 top-level; a real Element session signs in, creates rooms, sends, invites, scrolls back. The number understates the day: four of the fixes behind it were `/sync` silently losing events, which no percentage shows |
| Storage, rooms, state resolution | ~85% | the engine underneath; 1600+ tests, two backends through one conformance suite, state bake-off done |
| Configuration and first run | ~90% | database-backed, editable in the UI, one command from nothing to a working server |
| Admin API | ~23% | 34 of 145 operations have a real handler (`python3 tools/admin_api_coverage.py`, which counts them from source); the rest answer an honest 501. By area: Config 6/6, Server 5/5, AuditLog 3/3, Setup 2/2, Users 10/41, Rooms 5/23, Statistics 1/4, Cluster 1/6, and **Bridges 0/16**, Federation 0/7, Media 0/9, RegistrationTokens 0/5 |
| Management web interface | ~60% | users, rooms, federation, bridges, and configuration are real; arrays-of-objects and several resources are not |
| **Federation** | **~15%** | 59/246 assertions, 6/88 top-level; a two-server join works one way only |
| Bridges | ~20% | the appservice surface exists; no real bridge has ever been pointed at it, and **none of the 16 bridge operations the interface's Bridges section calls is served** — that section works against the mock only |
| Operations (HA, scale-out) | ~40% | it runs on Kubernetes with a chart and a tested image; the cluster path has never carried real traffic |

Federation is the honest answer to "when could I use this". Everything else is far enough along
that the gaps are specific and listed; federation is the one where "6 of 88" means a user on this
server cannot really talk to the rest of Matrix yet. That, not the client-server percentage, is
what stands between this and a server somebody else would run.

What is *not* in those percentages, and should temper them: no security review, no load testing
beyond a loadgen harness, `cargo fuzz` never run, Sytest never run, and no real bridge
(mautrix-*) has ever connected. Each of those has historically found things.

## What to do next, in order

### 1. Keep pulling on the measurement

`python3 tools/complement_triage.py <log>` against run 4 (2026-09-21), largest first. Count by
test, not by log line: one polling test can print the same line twenty times.

- **`TestServerNotices` (9)**: newly running, not newly broken. Server notices are unimplemented
  (`ServerNotices` is 0 of 2 in the admin API too).
- **`TestSearch` (8)**: `/search` needs a cross-room index the room-actor model has no place for.
- **`TestDeviceListUpdates` (8)**: device-list tracking when users join and leave rooms.
- **`TestMessagesOverFederation` (6), `TestPushRuleRoomUpgrade` (6)**: both die joining a room
  over federation with `404 room not found` -- item 3, the room bootstrap API.
- **`TestArchivedRoomsHistory` (6)**: `rooms.leave` in `/sync` carries state it should not, and
  the wrong timeline.
- **`TestRoomState` (5)**, **`TestSync` (4)**: mixed; read the reasons.
- **Fixed after run 4 and not yet graded**: `TestThreadsEndpoint` (threads were ordered by
  millisecond timestamp with an event-ID tie-break, so two back-to-back replies were a coin flip
  -- it passed in two runs of four), `TestRoomMembers` (a join's custom content was dropped),
  `TestGetFilteredRoomMembers` (`membership`/`not_membership` were ignored), `TestAsyncUpload`
  (`409 M_CANNOT_OVERWRITE_MEDIA`), and the joiner's half of presence in `TestSync`.
- **`TestChangePasswordPushers` (2)**: a password change should delete pushers made by other
  sessions. Needs pushers to remember which device made them, and a revocation hook from
  `hs-auth` into `hs-push`.
- **`min_depth` on `/get_missing_events`**, still parsed nowhere, and history visibility still not
  applied per event there.

Element has not been opened since the `/sync` changes above. The `matrix-rust-sdk` scenarios pass
against them, which is a real client and a good sign; it is not a browser.

### 2. Make it fun to administer — the half that is left

First run is done (see the state of things). The admin interface can now *change* the
configuration rather than only display it, which was the stated product priority. What it still
cannot do, in rough order of how often an operator will hit it:

- **Finish giving the Overview something to say.** Half done 2026-09-21: `statistics.overview`
  and `cluster.get` are real (`crates/hs-cli/src/overview.rs`), so a new administrator sees
  Users, Rooms, Daily active users, Mode and an uptime in words instead of "Not implemented"
  five times. Counts are shared for a minute rather than redone per poll, and what nothing can
  count yet (media, failing destinations, pending reports) is *absent* from the response rather
  than zero — the page shows a dash, and its all-clear now says what it could not check instead
  of putting a green tick over bridges and federation nobody asked about. What is left on that
  page is the two panels still answering 501: `appservices.list` and
  `federation.destinations.list`.
- ~~Add a user from the interface.~~ **Done 2026-09-21.** `AuthStoreUserDirectory::create_user`
  is real (it inherited a default that answered 503), and the Users page has an "Add user"
  dialog. What it does not do yet: set an email or an external ID at creation (refused with a
  pointer rather than silently dropped), reset an existing user's password, or invite somebody
  by link so that the administrator never sees the password at all — that last one is the
  better design for anything but a household, and wants registration tokens, which are 501.
- **Edit an array of objects as a form.** `listeners.listeners`, `media.thumbnail_sizes` and
  `auth.oidc_providers` fall back to a JSON textarea with live parse errors. Reachable, not
  pleasant; the generic renderer is built to sit underneath hand-tuned editors for exactly these.
- **Switch a tagged-enum backend** — there is no "move from embedded to postgres" flow, only a
  view of whichever variant is live.
- **See which *setting* changed.** `ConfigStore` records the merge patch per revision precisely so
  the interface can show it, and no operation exposes it: `GET /config/{section}/history`
  returning `ChangeRecord[]` would turn the section history into a per-setting one with a revert.
- **Validate across sections.** `POST /config/validate` is sent one section at a time, so a
  constraint spanning two only fails at save.
- **Reload anything.** `config.reload` reports honestly that nothing was hot-applied, because
  nothing in this server re-reads its configuration while running — the rate limiter, the
  federation policy and the telemetry layer are built once at startup. Giving any one of them a
  live read is what makes `reloaded_sections` non-empty.
- **Nothing here, as it turns out** — this bullet used to claim the interface signs an operator out
  when a request merely fails. It does not: `signInWithToken` already distinguishes a failed fetch
  ("Couldn't reach the server") from a 401 ("That token wasn't recognized"). What actually
  happened is that `e2e-real`'s `beforeEach` asserted `toHaveURL(/\/admin\/?$/)` after signing in,
  and `AppShell` renders the sign-in form *at whatever URL you are on* when there is no session —
  so that assertion passed identically whether sign-in worked or not, and a failed sign-in
  surfaced two tests later as a page mysteriously showing sign-in. The assertion now checks that
  the sign-in form is gone. The real open question is narrower and is not in the app: one `fetch`
  through the Vite dev server's `/api/v1` proxy fails, only under the full suite, against a server
  answering 200 to twelve consecutive curls.
- ~~Have the config pages checked by axe.~~ **Done 2026-09-21**: `e2e/configuration.spec.ts` walks
  index, search, a section's form, an edit, the review dialog, a rejected check and a save, with
  axe at each, and the two densest states again at phone width. All clean. The one thing it
  turned up was in the *check*: axe reads contrast from what is painted, so a dialog sampled
  mid-fade reports failures that are gone 600ms later. `expectNoAxeViolations` now waits for the
  page's finite animations to finish, which protects every spec that opens a dialog.

The three first-run leftovers this section used to end with are done: the README quickstart is
one `docker run`, the image's `CMD` is `serve` with `HS_DATA_DIR=/data`, and the Helm chart
renders a media path (and no longer pulls its image from a repository that is not ours — a second
defect found while fixing the first; see the chart's commit). What is left there:

- **The setup link guesses its own address.** It is rooted at `server.public_baseurl` when set
  and at `http://localhost:<first bound port>` otherwise, which is wrong behind `-p 9000:8008`
  or any proxy that has not been described to the server. Correct for the quickstart; the
  operator has to edit the port otherwise.
- **A first boot takes about five seconds in the container**, nearly all of it between generating
  the signing key and binding the listener. Unmeasured; opening ~60 keyspaces with a synchronous
  flush each is the suspect.

### 3. Federation: finish the join

- **Restricted and knock-restricted joins fail across the board** — ten top-level tests, all
  `M_FORBIDDEN: invalid join_authorised_via_users_server`. Tracks 06 and 04.
- **Two-way federation needs a room bootstrap API.** Two instances of this server complete a real
  join handshake over TLS with a private CA, verified end to end — but only one way, because
  `hs-room` can create a new room or extend one it already has, and has no way to build a room
  from a join's verified state snapshot. Specified in
  `docs/rfcs/0015-outbound-join-needs-a-room-bootstrap-api.md`; the script that demonstrates the
  gap is `crates/hs-federation/scripts/two-server-federation.sh`.

### 4. The rest of the Complement triage

Full detail, by owning track, at the top of `docs/status/14-test-and-conformance.md`:

- **Track 02**: room-v12 additional-creator validation answers 403 where the spec wants 400; the create event's `room_id` is missing on `/state`, `/messages`, `/event` and `/context`.
- **Track 09**: federation-fetched media fails outright — thumbnails, content, filenames. Implemented, but broken for remote peers.
- **Tracks 08 and 06**: device-list and to-device delivery over federation time out at full length rather than failing fast, which reads like a delivery gap rather than a validation one.
- **Half-done**: error responses that are not JSON. The unknown-endpoint and wrong-method halves are fixed — `hs_http::fallback`, applied last in `hs serve`, answers `404`/`405 M_UNRECOGNIZED` in the Matrix shape and leaves the admin API's RFC 9457 alone, with an end-to-end test through the real assembled router. What is left is the extractor rejections: routes taking a bare `axum::Json` still answer a plain-text body, where the spec wants `M_NOT_JSON` (Complement's `TestRequestEncodingFails` sends invalid UTF-8 to `/register`). `hs_http::body::PermissiveJson` already does the right thing; the work is finding every bare `Json(...)` extractor on a `/_matrix` route and switching it.

### 5. Housekeeping worth doing deliberately

- **Rename the crates** from `hs-` to the project's own prefix. Mechanical across twenty-six crates, and best done when nothing else is in flight.
- **Tag `v0.0.1`** to exercise the untested half of CD: binaries for three targets, the Helm chart as an OCI artifact, and a GitHub release.
- **`cd.yml` documents an `edge` tag it does not produce** — pushes to `main` tag the image `main`. Fix the tag or the table; they disagree.
- ~~`web`'s unit tests and lint have never run on this machine.~~ **Done, and it was never the machine.** `vite.config.ts` excluded `e2e/**` but not `e2e-real/**`, so vitest collected a Playwright spec and died at import; and `openapi-fetch` builds a `new URL()` per request, so the app's relative `/api/v1` base threw `ERR_INVALID_URL` under jsdom and no page test could ever have passed. `npm run check` — typecheck, lint, test, build — is green, and the suite runs in about three seconds.
- **Receipts and presence are in-memory**, so a restart forgets read state and presence. Postgres ignores `pool_size`, refuses `tls`, and hardcodes the `public` schema. `/createRoom` is not shard-gated. UIA on `/keys/device_signing/upload` needs a coordinated change with the loadgen scenario that bootstraps cross-signing without auth data.

## Known gaps, honestly held

| Gap | Where | Consequence |
|---|---|---|
| `min_depth` ignored on `/get_missing_events` | `hs-cli` | a conformance gap, no longer a crash |
| history visibility not applied per event on `/get_missing_events` | `hs-cli` | pre-join events served unredacted |
| Restricted joins rejected | `hs-federation`, `hs-room` | ten conformance tests, a common room type |
| No room bootstrap from a join | `hs-room` | federation completes one way only |
| Federation media fetch broken | `hs-media` | remote avatars and attachments fail |
| `/search` unimplemented | `hs-room` | needs a cross-room index the actor model has no place for |
| Admin UI cannot edit arrays of objects | `web` | listeners and OIDC providers are a JSON textarea |
| Nothing hot-applies a config change | all | every change needs a restart, and says so |
| One `/api/v1` fetch fails under the full `e2e-real` suite | `web` (dev proxy) | two tests fail together, pass alone |
| CI does not run the Playwright suite | `.github` | two of its tests failed for an unknown length of time before anybody noticed (fixed 2026-09-21) |
| Receipts and presence in memory | `hs-user` | a restart forgets read state |
| Postgres `tls`/`pool_size`/schema | `hs-kv`, `hs-cli` | encrypt in front of the database for now |
| `/createRoom` not shard-gated | `hs-cli` | first actor may be built on a non-owner |
| `e2e/configuration.spec.ts` failed once in 112 runs | `web` | unreproduced, and the machine was running Complement at the time; if it recurs, the error is the first thing to capture |
| Overview's Bridges and Federation panels are 501s | `hs-admin`, `hs-cli` | the first page still says "isn't implemented" twice, and its all-clear is qualified accordingly |
| Overview counts media, failing destinations and reports as unknown | `hs-cli` | three dashes where numbers should be; the sources exist in `hs-media`, `hs-federation` and nowhere respectively |
| Setup link assumes `localhost:<bound port>` without `public_baseurl` | `hs-cli` | wrong behind a remapped port or an undescribed proxy |
| In-process server cannot be restarted over its data directory | `hs-cli` | background tasks hold the store's lock after `shutdown()`; restart tests need the real binary |
| The release binaries job's web build has never run | `.github` | it only runs on a `v*` tag; the image path is verified, this one is not |
| User-directory scope is computed by walking rooms on every search | `hs-user` | fine today; the first thing to index if a public room gets very large |
| `TestThreadsEndpoint` flapped between runs | `hs-room` | ordering tie on a millisecond timestamp; fixed 2026-09-21, not yet graded -- if any test still moves between identical runs, that is a bug to find, not noise |
| A hot room joined after the token is resumed from the join, not sent whole | `hs-user` | the client recovers from `/state` and `/messages`; rare, and written down in `resume_mode` |
| A requester with no device never records a feed cursor | `hs-user` | its feed entries coalesce forever and an incremental sync sees no change; some appservice callers |
| `heartbeat_seq` is derived from wall-clock milliseconds | `hs-cluster` | two ticks in one millisecond read as "no progress", i.e. death; harmless at the production 1s interval, surfaces only in tests |
| Sytest never run | `tests/sytest` | CPAN dependencies absent |
| `cargo fuzz` never executed | `fuzz/` | no nightly toolchain |

## Conventions worth keeping

- **Verify by running.** Every claim above was checked against the binary, a real client, or a conformance suite. Reports that were taken on trust have been wrong repeatedly — a "clean typecheck" with two errors, a gap reported open that had been closed hours earlier, a receipts bug that was a stale binary.
- **Point real things at it.** Every serious bug this project has found came from Complement, a real SDK, or a real browser — never from its own tests. The signing bug had passed every test for weeks because the signer and the verifier shared the same wrong assumption.
- **Run the gates CI runs.** `cargo test -p <crate>` cannot see what `--workspace --all-targets` sees: feature unification, cross-crate visibility, dead code. Seven consecutive red CI runs came from exactly that gap.
- **Wait for conditions, not durations — and grant real time, not just virtual time.** This has now bitten seven times. The 2026-09-21 round found the mechanism: `settle` advanced a virtual clock and called `yield_now()`, which runs async tasks at no wall-clock cost, while the work it was waiting for finishes on `spawn_blocking` threads. Thirty rounds bought 600ms of virtual time and about 30ms of real time. What matters is the *ratio* of real time granted to virtual time advanced, not the number of rounds. One test in that batch could also pass vacuously — it asserted `count > 0` on a count that is zero exactly when the thing under test never happened.
- **Keep the repository off iCloud.** `git status` took 600 seconds there and takes 0.24 here.
- **A check that passes on both branches proves neither.** On 2026-09-21 a test asserting "the
  embedded interface matches what the build script chose" passed, and was read as proof the real
  interface was embedded; it would have passed for the placeholder too, and the binary had the
  placeholder. Look at the decision itself (the build script's output, the log line, the byte on
  the wire), not at a test that is satisfied either way.
- **Registered is not working, and a real handler is not working either.** 34 of 145 admin operations have a real handler (`tools/admin_api_coverage.py` counts them; the figure used to be quoted by hand and was different in every document). The rest answer 501. But `users.create` had a real handler for days while the only real user directory answered it 503 — so "has a handler" is a ceiling, and the floor is an end-to-end test through `hs serve`.
