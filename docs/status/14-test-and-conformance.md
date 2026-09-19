# 14 Test and conformance (integration lead): status

## Re-measurement (2026-09-19, this session): csapi moved, federation ran for the first time

Per `docs/next-steps.md` item 1: the 2026-09-18 numbers below predate history-visibility
enforcement, the room directory, `/createRoom` validation, `/forget`, profiles, inbound
federation, E2EE sync fields and PostgreSQL. This session re-measured against a pinned commit
(built first, before anything else, so the measurement isn't chasing a moving working tree):

**`git rev-parse HEAD` at build time: `576e1e1a72d2be1147a33c3865333d7165ca69db`.**

### 1. `csapi`, before vs. after

| Run | Commit | Leaf-level (every assertion) | Top-level (Go `func Test*`) |
|---|---|---|---|
| 2026-09-18 (session 1, run 2) | untracked, ~`6d6d7be` | 293 total: 125 pass, 161 fail, 7 skip | 106 total: 30 pass, 74 fail, 2 skip |
| **2026-09-19 (this session)** | **`576e1e1`** | **293 total: 148 pass, 138 fail, 7 skip** | **106 total: 35 pass, 69 fail, 2 skip** |

Leaf pass rate: 42.7% → 50.5%. Top-level: 28.3% → 33.0%. Reproduction (unchanged from
2026-09-18, confirmed still exact):

```bash
./tests/complement/build.sh complement-hs-reimplement:dev   # ~10-19 min this session (see below)
cd refs/complement && COMPLEMENT_BASE_IMAGE=complement-hs-reimplement:dev \
  go test -v -timeout 30m ./tests/csapi/...                 # ~14 min
```

Cross-check: the same commit's image, run again a few minutes later as part of the full
`./tests/...` sweep below (16 Go packages racing for the same 10 cores, so a noisier
measurement), landed at 146/142/7 leaf and 34/70/2 top — within 2 assertions of the dedicated
run above. That's ordinary integration-test flake (timing-sensitive `MustSyncUntil` polling under
load), not a harness bug; the dedicated single-package run is the one to trust as the headline
number.

**Wall-clock, for budgeting the next session:** image build 10m17s cold (first build this
session) and 18m50s on a rebuild later in the session — both slower than 2026-09-18's ~4 min,
because five other tracks' agents were running concurrent `cargo`/`docker build` on the same
10-core box the whole time (confirmed: `ps` showed a second, simultaneous `complement-hs-e2ee:dev`
build throughout). Budget 10-20 min per image build here, not 4. `csapi` alone: ~14 min (815s).
The full `./tests/...` sweep (16 packages): 16m32s wall clock (parallel package execution, so
per-package times sum to more than that).

### 2. The federation package, run for the first time

`./tests/...` (not just `./tests/csapi/...`) was run against the same pinned image. It recurses
into every Go package under `tests/`, so one invocation covered `tests/csapi` (above),
`tests/msc2836` and 13 other `tests/mscNNNN` packages, and — the actual target — the top-level
`tests` package (two-homeserver federation blueprints, direct messaging, knocking, media,
restricted rooms; 90 `func Test*`, one of which, `TestMain`, is the test-binary entrypoint and not
a real test, leaving 89).

**Top-level `tests` package: 89 real tests, 5 pass / 84 fail / 0 skip. Leaf-level: 186
assertions, 41 pass / 139 fail / 6 skip.** This is the first number that has ever existed for this
package. The five passes are real and worth naming because they're exactly what you'd expect to
survive the bug below: `TestInboundFederationKeys` (a pure key-fetch test, no outbound trust
needed), `TestWriteMDirectAccountData` and `TestMSC4291RoomIDAsHashOfCreateEvent` (single-server,
no real federation), `TestKnockRoomsInPublicRoomsDirectory(InMSC3787Room)` (local directory
listing).

**Root cause of the other 84, diagnosed and confirmed, not just observed:** `hs-federation`'s
outbound HTTP client (`crates/hs-federation/src/client.rs::client_for`) cannot validate any
certificate that isn't in the public root bundle. The workspace's `reqwest` dependency
(`Cargo.toml` line 90: `features = ["json", "rustls-tls"]`) pulls in `rustls-tls-webpki-roots`,
which trusts only ~140 baked-in public CAs and — unlike a native-roots or platform-verifier
backend — never reads the OS trust store. `tests/complement/startup.sh` already runs
`update-ca-certificates` to trust Complement's generated CA system-wide (right thing to do), but
that step is a no-op for this specific TLS stack. `federation.verify_certificates` defaults to
`true` (correct, matches Synapse's own default), and there is no equivalent of Synapse's
`federation_custom_ca_list` to add Complement's CA as an extra trusted root — that config surface
does not exist anywhere in `hs-config`/`hs-federation` today (confirmed by grep). The evidence
trail: `TestSyncTimelineGap` (csapi) and 27 federation-package failures show
`M_UNAUTHORIZED: signature verification failed` on `make_join`/`send_join`/profile-query calls to
a synthetic or second-homeserver peer, and the raw container logs show the matching cause 24
times: `http: TLS handshake error from 127.0.0.1:PORT: remote error: tls: unknown certificate
authority` — our outbound client rejecting the peer's Complement-CA-signed certificate. Confirmed
against `refs/synapse/docker/complement/conf/workers-shared-extra.yaml.j2`: Synapse's own
Complement config sets `federation_custom_ca_list: [/complement/ca/ca.crt]` *and*
`federation_ip_range_blacklist: []` — the second one matters too, since this server's default
`ip_range_blocklist` (`172.16.0.0/12` among others) covers the private ranges Docker's bridge
network and `host.docker.internal` commonly live in.

**Validated, not just theorized.** Added a harness-only workaround to `tests/complement/startup.sh`
(the file this track owns) — `federation.verify_certificates: false` and
`federation.ip_range_blocklist: []` in the generated `config.yaml`, matching the *effect* of
Synapse's Complement config since there's no `custom_ca_list` equivalent to reach for — rebuilt,
and re-ran the three tests above that had failed with the TLS symptom
(`TestOutboundFederationSend`, `TestInboundFederationRejectsEventsWithRejectedAuthEvents`,
`TestFederationRoomsInvite`). **All three still fail, but the TLS error is gone in all three**,
replaced by distinct, further-along failures: a real PDU signature-verification rejection
(`M_BAD_JSON: signature from host.docker.internal:1024/ed25519:... does not verify` on
`send_join` — a second, different bug, see track 06 below), cross-server room-alias-join 404s, and
the same `MustSyncUntil` timeout cluster #1 below. This is strong evidence the TLS gap was gating
the *majority* of the 84 failures, but the rebuilt image no longer corresponds to the pinned
commit (the working tree had moved under five concurrent agents by rebuild time, confirmed by an
18m50s rebuild instead of a cached few-hundred-ms one), so **no corrected top-level federation
number is claimed here** — only that one exists to be measured, and re-running the full
`./tests/...` sweep with this harness fix in place, against a freshly pinned commit, is the
single highest-value thing the next session can do. The harness fix itself is left in place
(it's in-scope, `tests/complement/startup.sh`, and makes every future federation run meaningful
instead of universally TLS-blocked); `verify_certificates: false` is called out in the file's own
comment as a test-only substitute for real CA trust, not a production recommendation.

### 3. Updated triage by owning track (replaces the 2026-09-18 list below)

**Track 04 (room-and-events) — closed since 2026-09-18:** history-visibility enforcement, room
directory (`TestPublicRooms` now passes, not in either failing list), `/createRoom` parameter
validation, `/forget` membership validation are all gone from the failing list as originally
reported. **New, found this session:**
- `/forget` now fails differently: `TestRoomForget` — "Did not expect room X in left" (a `/sync`
  `left` section regression, not the original 200-vs-400 bug).
- State/membership idempotency: `TestRoomCreationReportsEventsToMyself` — setting the same state
  twice, or joining twice, mints a *new* event ID both times instead of being a no-op.
- `unsigned.transaction_id` never populated on the sender's own echo
  (`TestTxnInEvent`/`TestTxnScopeOnLocalEcho`/`TestTxnIdWithRefreshToken`).
- `/relations/{eventId}` and `/threads` both 404 (`TestRelations`, `TestThreadsEndpoint`) — not
  built yet.
- `/upgrade` 404 (`TestPushRuleRoomUpgrade`, `TestSearch`'s upgrade step) — not built yet.
- `/search` 404 (`TestSearch`) — not built yet.
- Room alias/canonical-alias validation gaps: `PUT canonical_alias` on a nonexistent alias
  returns 200 (want 400); `GET /aliases` on a room the caller isn't in returns 200 with the list
  (want 403); `DELETE /directory/room/{alias}` by a non-owner returns 200 (want 403).
- `GET .../state/m.room.power_levels` omits the `ban` key when a client asks for the raw
  content; `format=event` on `/state/m.room.member` omits the `sender` wrapper; `/joined_members`
  omits `avatar_url` and doesn't 403 after the caller has left.
- `/messages?filter={"contains_url":true}` doesn't filter (`TestRoomImageRoundtrip`: 8 events
  back, want 1).
- `membership_on_events_test.go`: per-event `unsigned`/membership tagging comes back empty where
  the test expects `leave`/`join`.
- Oversized event body: `M_BAD_JSON`/400 returned where the spec test wants 413
  (`invalid_test.go`).

**Track 05 (sync, also owns presence) — the single largest remaining cluster, new this
session:** a broad set of `MustSyncUntil: timed out` failures across almost every test that
depends on an incremental `/sync` reflecting a change made moments earlier: profile updates
(`TestAvatarUrlUpdate`, `TestDisplayNameUpdate`, `user_directory_display_names_test.go`), typing
EDUs (`TestTyping`, `TestLeakyTyping`), device-list changes surfacing in sync
(`TestDeviceListUpdates`), ignored-users filtering (`TestInviteFromIgnoredUsersDoesNotAppearInSync`),
invites (`TestRoomsInvite`, federation package's `TestFederationRoomsInvite`), presence-in-sync
(`TestPresenceSyncDifferentRooms`, `TestSync`'s presence subtests), room summaries
(`TestRoomSummary`), threaded receipts, invite-rejection (`TestLeaveEventInviteRejection`), and
push-rule-carryover-on-upgrade. This is numerically the biggest single bucket in the whole run —
worth its own investigation rather than filing as N separate bugs. Also still present:
presence endpoints 404 (`TestPresence`, `TestMembersLocal`); `filter` doesn't reject invalid
room/sender IDs (`TestFilter`, want 4xx got 200); no push rules ever appear in a sync response
(`TestPushSync`). **Confirmed unresolved from 2026-09-18:** the `hsu1_...` sync-token vs.
`/messages`/`/keys/changes` pagination-token mismatch (`TestLeftRoomFixture`,
`TestSendAndFetchMessage`, `TestRoomMessagesLazyLoading(LocalUser)`, `TestKeyChangesLocal`) — this
is `docs/next-steps.md` item 2, already tracked, not new.

**Track 06 (federation) — mixed.** The good news: `send_join`/`/send`/`make_join` are real and,
per section 2 above, once the harness's TLS gap is worked around, get *past* the TLS failure into
real protocol territory. The bad news, newly surfaced by that same workaround: `send_join` can
reject a legitimately-signed join with `M_BAD_JSON: signature ... does not verify`
(`federation_room_event_auth_test.go`) — a genuine remaining signature/canonical-JSON bug, not the
TLS one. **The TLS/CA gap itself (section 2) is this track's (or `hs-config`'s) to close
properly**: add a `federation.custom_ca_list`-equivalent config option and/or switch the
`reqwest` build to a native-roots or platform-verifier backend so `update-ca-certificates`
(already run by this harness) actually takes effect for outbound federation TLS. Also
`ip_range_blocklist`'s production-safe default currently blocks the private ranges any
multi-container or Complement-style deployment needs — Synapse ships `federation_ip_range_
blacklist: []` in its own Complement config for the same reason; this project has no equivalent
override path other than what this session added to the test harness.

**Track 08 (e2ee) — unresolved, with a discrepancy worth a fresh look.** Cross-user `/keys/query`
still returns an empty `device_keys` map for a same-server second user
(`upload_keys_test.go:138`). Malformed-shape rejection (`{"device_id": true}` instead of an array)
returns 200 where 400 is expected, for *both* `/keys/upload` and `/keys/query`
(`TestUploadKey`, `TestKeysQueryWithDeviceIDAsObjectFails`) — notable because
`crates/hs-e2e/src/routes/keys_query.rs`'s own doc comment says this exact Complement test is
handled ("this server rejects it outright, matching the spec and Complement's
`TestKeysQueryWithDeviceIDAsObjectFails`"), and a static read of `build_keys_query_response`
agrees it should 400 on a `Value::Object` device filter — but the live run says otherwise. Worth
checking whether the code doing the rejecting is actually being reached (routing, an extractor
ordering issue, or a stale build) before assuming the comment is simply wrong. `/keys/claim`
ordering also still returns the wrong slot's content (`TestKeyClaimOrdering`) — unresolved from
2026-09-18.

**Track 09 (media) — narrower than 2026-09-18 reported, one wiring fix away.** Async upload
(MSC2246) IS implemented and mounted now (`hs_media::router::authenticated_router`'s `/create` at
`/_matrix/client/v1/media/create`, `legacy_router`'s at `/_matrix/media/v3/create`) — but
Complement's own `CSAPI.CreateMedia` helper (`refs/complement/client/client.go:97`, unmodified
upstream) calls the older unstable path `POST /_matrix/media/v1/create`, which this server mounts
nowhere (`TestAsyncUpload`, all sub-cases, 404). This reads like a one-line additional route
mount, not a rebuild of the feature. URL preview genuinely doesn't exist yet
(`GET /_matrix/media/v3/preview_url` 404, `TestUrlPreview`) — `docs/rfcs/0006-url-previews.md` is
design-only, matching 2026-09-18.

**Track 07 (auth) — several small validation gaps, some new detail.** `/register/available`
doesn't reject an invalid username shape (200 `available:true`, want 400); registration UIA flow
returns 401 instead of 400 for "no session provided" and 200 instead of 401 for
"auth-requires-session"; mixed-case usernames aren't lower-cased on register
(`@user-UPPER` stored verbatim, want `@user-upper`). `GET /capabilities` doesn't require auth
(200 with no token, want 401). `DELETE /device/{deviceId}` with a malformed body returns 400
where UIA should be attempted first (401) — unresolved from 2026-09-18. `GET
/_matrix/client/v3/devices` after a multi-device logout still shows one extra device — unresolved
from 2026-09-18.

**Track 10 (push) — new detail.** `/sync` never includes push rules in any form
(`push_test.go`: "no pushrules found in sync response" on every subtest) — worth checking whether
this is a `hs-push` gap or a `hs-user`/`hs-room` sync-wiring gap (the account-data-shaped push
rules payload has to reach `/sync` through the same actor the `MustSyncUntil` cluster above lives
in, so these may share a cause).

### 4. Blacklist decision

**No entries added.** `blacklist.txt` stays empty. Every failure this session examined is either
a bug (something the code is trying and failing to do — e.g., `/keys/query`'s own doc comment
disagreeing with its own behavior) or a feature this project's own docs describe as future work,
not declined work (`/search`, `/threads`, `/relations`, URL previews, presence, room upgrades all
appear as owned, planned surface in their tracks' briefs; none carries a "we've decided not to
build this" note anywhere in `docs/`). The two candidates that *are* documented as deliberate,
conscious cuts — `hs-e2e`'s unenforced cross-signing signature verification and its skipped UIA
re-auth on `/keys/device_signing/upload` (both called "cut-for-time, not oversight" in
`docs/status/08-e2ee.md`) — have no matching failure in either Complement run this session, so
blacklisting them now would be speculative, not justified by an observed failure. If a future run
surfaces a test that fails specifically because of one of those two documented cuts, that's the
first honest blacklist entry.

---

## Complement: the honest number (2026-09-18, this session) [superseded by the section above]

**The image builds and Complement runs against it.** This had never happened before this session
(`Dockerfile.template` referenced a crate named `hs-server` that never existed; the real binary is
`hs` from `crates/hs-cli`). Reproduce with:

```bash
./tests/complement/build.sh complement-hs-reimplement:dev   # ~4 min cold, ~1s once cargo's layer is cached
cd refs/complement && COMPLEMENT_BASE_IMAGE=complement-hs-reimplement:dev \
  go test -v -timeout 30m ./tests/csapi/...
# or the full wrapper (adds the blacklist -skip regex): ./tests/complement/run_single_node.sh
```

**Scope of this number: `tests/csapi` only** (106 top-level Go test functions, the core Matrix
client-server API suite), not the top-level `./tests/...` package (federation-heavy, two-homeserver
blueprints, ~90 more top-level tests) or any `tests/mscNNNN` package. `./tests/csapi/...` was
chosen because it is the largest single package that exercises this server without needing
federation to work first, and because a first number needed to exist before a second, harder one
did. Both runs below used the embedded (Fjall) storage backend, single node, default blacklist
(empty — nothing is skipped by this harness's own `-skip` regex; the two `SKIP`s below are
Complement's own, for the shared-secret-registration and server-notices tests this server
correctly reports as unsupported by 404/absence).

Two full runs were done, back to back, because the workspace is being edited live by five other
tracks and the first run's image was built moments before track 04 fixed a routing bug this run's
own diagnosis had just found (see below) — rebuilding and rerunning showed the fix land in real
numbers, which is a better demonstration of the harness working than either number alone.

| Run | Image built from | Leaf-level (every individual assertion) | Top-level (Go `func Test*`) |
|---|---|---|---|
| 1 | working tree at 2026-09-18 ~22:50 (before the state-key routing fix) | 283 total: **106 pass, 171 fail, 6 skip** | 106 total: **26 pass, 78 fail, 2 skip** |
| 2 | working tree at 2026-09-18 ~23:40 (after it) | 293 total: **125 pass, 161 fail, 7 skip** | 106 total: **30 pass, 74 fail, 2 skip** |

Full logs: `/tmp/complement-csapi.log` (run 1), `/tmp/complement-csapi2.log` (run 2) — not
committed (scratch files outside the repo's tracked paths); rerun the reproduction command above to
regenerate. Leaf-level counts are every individual `--- PASS/FAIL/SKIP` line at maximum
indentation (i.e., counting `TestFoo/sub/subsub` once, not also its parents `TestFoo` and
`TestFoo/sub`); top-level counts every `func Test*` Go treats as its own top-level test (each of
which may itself represent a table of many spec assertions).

**Both runs are against whatever was on disk in this live, multi-agent-edited working tree at
build time, not a specific git commit** — `tests/complement/build.sh` streams the actual working
directory (tracked and untracked changes both), not `git archive`, so uncommitted work from other
tracks' concurrent sessions is included. `git rev-parse HEAD` read at report time (after both test
runs finished) was `6d6d7beff41f3832d1efc8ebd2d8930adfec51c8`, but that is the commit *after* the
runs, not what was actually tested; do not treat it as precise provenance, only as a lower bound on
recency. A rerun against a later HEAD (this session was told mid-run that inbound federation
`/send`, `make_join`/`send_join`, `.well-known`, profiles and room v12 landed since) would very
likely score higher, especially on the federation-touching tests below.

### Top failures, grouped by owning track (from run 2's leaf-level failure messages)

1. **Track 04 (room-and-events) — room-state and lifecycle validation gaps.** `POST /createRoom`
   with invalid `room_version`/params returns `200` where the spec test expects `400`; extensible
   `m.topic` (`m.topic.m.text`, MSC3765) is not populated on room creation; `POST
   /rooms/{roomId}/forget` returns `200` instead of `400`/`403` for a non-existent or non-member
   room. (`apidoc_room_create_test.go`, `apidoc_room_forget_test.go`.)
2. **Track 04/02 (room-and-events / state) — history-visibility is not enforced on read.**
   `GET /rooms/{roomId}/messages` and `GET /rooms/{roomId}/event/{eventId}` return `200` with full
   event content for a room the requesting user has left and that is not world-readable, where the
   spec test expects `403`/`404`. This also explains several downstream `MustSyncUntil: timed out`
   failures in the same test files, since the test's next step depends on the prior assertion.
   (`apidoc_room_history_visibility_test.go`.)
3. **Track 04 (room-and-events) — room directory.** `PUT
   /_matrix/client/v3/directory/list/room/{roomId}` returns `404` (route not mounted, or not
   wired to the room registry), so `GET /publicRooms` never sees the room and the test's
   `RetryUntil` polling loop times out. (`power_levels_test.go`'s use of `public_rooms_test.go`
   helpers; also directly in `TestPublicRooms`.)
4. **Track 08 (e2ee) — key management round-tripping.** `POST /keys/query` for another user's
   device keys comes back empty (`device_keys.@user-2:hs1` missing) in a cross-user query; `POST
   /keys/claim`'s returned one-time-key content/signature does not match what was uploaded;
   `POST /keys/upload` accepts a malformed device-ID-as-object shape the test expects rejected
   with `400`. (`upload_keys_test.go`, `user_query_keys_test.go`.)
5. **Track 09 (media) — async upload and URL preview unimplemented.** `POST
   /_matrix/media/v1/create` (MSC2246 async upload) returns `404`; `GET
   /_matrix/media/v3/preview_url` returns `404`. (`media_async_uploads_test.go`,
   `url_preview_test.go`.)
6. **Track 05 (sync, which per this project's ownership map also owns presence) — presence
   endpoint absent or incomplete.** `GET`/`PUT /_matrix/client/v3/presence/{userId}/status` return
   `404` or a non-JSON body rather than a presence document. (`apidoc_presence_test.go`.)
7. **Track 06 (federation) — cross-server join signature verification, seen from inside a csapi
   test.** One csapi test (`TestSync`'s federation-join helper) hit `make_join` on a *second*
   homeserver via `X-Matrix` and got `401 M_UNAUTHORIZED: signature verification failed`. Per the
   coordinator's update, inbound `/send`, `make_join`/`send_join` landed on HEAD after this run's
   image was built; this failure may already be fixed and should be the first thing a rerun
   checks.
8. **Track 07 (auth) — minor device/session validation gaps.** `DELETE /device/{deviceId}` with a
   malformed body returns `400` where UIA should be attempted first (`401`); `GET
   /_matrix/client/v3/devices` after logging out other sessions returns one more device than
   expected, suggesting a device isn't being cleaned up on that logout path.
   (`apidoc_device_management_test.go`, `apidoc_logout_test.go`.)

### Harness bugs found and fixed this session (all within `tests/complement/`, so fixed directly)

- **`tests/complement/Dockerfile.template` referenced a nonexistent `hs-server` crate.** Fixed to
  build the real `[[bin]] name = "hs"` from `crates/hs-cli` (`cargo build --release --jobs 4 -p
  hs-cli`; `--jobs 4` deliberately, not the full core count, per this track's brief about sharing
  a 10-core machine with five other agents' `cargo` invocations).
- **`hs serve` does not terminate TLS** (`crates/hs-cli/src/serve.rs` logs a warning and binds
  plaintext even with a listener's `tls:` block set — confirmed by reading the code). Complement
  requires HTTPS on 8448. Fixed by running `stunnel4` (chosen over `nginx`/`nginx-light` for
  footprint and because it needs no HTTP-proxy configuration, only "TLS in, plaintext out") inside
  the image, terminating TLS on 8448 and forwarding to `hs` on 8008 — safe because `hs serve`'s one
  `axum::Router` already answers every resource (client *and* federation) on whichever port it's
  bound to (see `crate::serve::build_router`'s doc comment), so `hs` only needs one plaintext
  listener. `tests/complement/stunnel.conf.template` + `startup.sh` do the cert-signing (unchanged
  from the original scaffold's already-correct `openssl` recipe) and stunnel config generation.
- **The build context was `.` (repository root)**, and `target/` alone is 27 GB on this shared
  workspace; a root `.dockerignore` is not a file this track owns. Fixed by having `build.sh`
  stream a `tar` (excluding `target`, `.git`, `web/node_modules`, `refs`, `media-store`,
  `.conformance-run`) to `docker build`'s stdin instead of using `.` as the context directly.
  Aside: macOS's `bsdtar` auto-skips any directory containing a `CACHEDIR.TAG` (which every Cargo
  `target/` directory has, including nested ones like `crates/hs-federation/fuzz/target`), so the
  explicit `target` exclude turned out to be redundant but is kept for portability to GNU tar.
- **`tests/complement/skip_regex.sh` failed under `set -o pipefail` whenever the blacklist had zero
  active (non-comment, non-blank) lines** — exactly the documented, intended common case — because
  `grep -v` with no matching lines exits 1, and `pipefail` propagates that through the pipeline
  even though the final `paste` succeeds. This aborted `run_single_node.sh` (`SKIP_REGEX="$(...)"`
  under `set -e`) before it ever built the image, so the checked-in wrapper script had never
  actually reached `go test` even after the image built. Fixed with `{ grep ... || true; }` on
  both filtering stages.
- **`VOLUME /data`** in the Dockerfile made Complement print "volumes can lead to unpredictable
  behaviour due to test pollution" on every run (Complement's own contract-linting warning).
  Removed — storage is embedded in the container's own writable layer, so no volume was ever
  needed for the "manage its own storage" requirement.

### Build time on this machine (a finding worth recording per the coordinator's request)

Cold (`cargo build --release --jobs 4 -p hs-cli` inside the build stage, no warm target cache in
the image): **~4 minutes** (`4m 00s` reported by `cargo` itself, ~251s total Docker stage time,
including dependency compilation from scratch — the base `rust:1.98-slim` image has no crates.io
cache). A rebuild that only touches the final runtime stage (no source change): a few hundred
milliseconds, fully cached. A rebuild after a source change elsewhere in the workspace: not
measured directly, but expect somewhere between these two depending on how much of the dependency
graph the change invalidates — Docker's layer cache does not help across *source* changes the way
`cargo`'s own incremental compilation would, since `COPY . .` invalidates every layer after it
whenever any file changes. Each `go test` run of `./tests/csapi/...` (106 top-level tests, one or
two containers each, ~5s container-deploy overhead per test) took **~15 minutes** end to end
(947s and 909s respectively).

### What a next session should do first

1. Rerun against current HEAD (the federation landing mentioned above) — the `make_join` signature
   failure and possibly others in the list above may already be gone.
2. Run the full top-level `./tests/...` package (federation-heavy, two-homeserver blueprints), not
   just `./tests/csapi/...` — this session did not have time for it after two `csapi` runs.
3. Once the top-5 list above is address by each owning track, populate `blacklist.txt` with the
   *remaining* known-and-understood gaps (each with a one-line reason), the way Palpo's own
   blacklist is grown, rather than leaving it empty forever.

---

Track brief: `docs/workstreams/14-test-and-conformance.md`. Owner crates: `hs-testkit`,
`hs-spec-coverage`, `hs-loadgen` (not started, see "Decisions made"), `tests/` (Complement, Sytest,
differential, oracle harnesses).

Last updated: 2026-09-18 (day one, session 1). A previous attempt was interrupted before writing
anything; this session started from the empty placeholders it left behind.

## Done

- **`hs-testkit`** (`crates/hs-testkit/`): fake clock ([`clock::FakeClock`], compatible with
  `tokio::time`'s paused-time mode — reads `tokio::time::Instant`, so it advances exactly when
  `tokio::time::advance` does); the scenario DSL ([`scenario::Scenario`], generic over any
  `axum::Router<()>` via `tower::ServiceExt::oneshot`, tracking named users' `user_id`,
  `access_token`, `device_id`, `refresh_token` across calls, with `register`/`login`/`whoami`/
  `refresh`/`logout`/`sync`/`send` verbs); Matrix error construction/assertion helpers
  (`matrix_error::MatrixErrorExpectation`); an append-only `RecordLog` built on
  `hs_kv::memory::MemoryBackend` (per this track's brief: "use it in the testkit rather than
  writing your own fake store") that every fake sink below is built on; a fake appservice receiver
  (`PUT /transactions/{txnId}`), a fake federation peer (records any request, answers queued or
  default responses), a fake push gateway (`POST /_matrix/push/v1/notify`, configurable rejected
  pushkeys), and a fake SMTP sink (in-memory recorder, no protocol-level SMTP server — see that
  module's doc comment for why). 19 unit tests plus an integration test
  (`tests/hs_auth_round_trip.rs`) that drives `hs-auth`'s real router (`hs_auth::routes::router()`)
  through register → whoami → register (second user) → login → whoami → refresh → whoami →
  logout → whoami(fails, `M_MISSING_TOKEN`) → whoami with an injected bogus token
  (`M_UNKNOWN_TOKEN`), plus two more tests for wrong-password and double-registration error
  shapes. `cargo fmt` clean, `cargo clippy -p hs-testkit --all-targets -- -D warnings` clean, all
  19 tests pass.
- **`hs-spec-coverage`** (`crates/hs-spec-coverage/`): a library plus binary. `spec` parses the
  top-level `*.yaml` files under each of the five `refs/matrix-spec/data/api/<family>/` directories
  (client-server, server-server, application-service, identity, push-gateway), reading each file's
  own `servers[0].variables.basePath.default` and prepending it to every `paths` entry, so a
  parsed route's `path` is already what a router would register (`/_matrix/client/v3/login`, not
  `/login`). `manifest` is this crate's own copy of the `routes.json` schema from
  `docs/rfcs/0005-routes-json-manifest.md` (RFC 0005, already drafted and largely implemented by
  track 15's `hs-http::router::Builder`/`RouteManifest` before this session started — see
  "Decisions made"); its test suite includes the RFC's own example JSON parsed verbatim. `coverage`
  diffs the two by exact `(method, path)` match per family/surface, producing registered/missing/
  extra counts. `report` renders Markdown. The `hs-spec-coverage` binary (`--spec-dir --routes
  --out --json-out --missing-limit --fail-on-missing`) ties it together. 21 unit tests plus
  `tests/real_spec.rs` (6 tests) against the actual `refs/matrix-spec` checkout already present in
  this workspace: parses 235 routes across the five APIs, confirms the well-known legacy auth
  paths and a federation `send` route are present with the right base path, and — the most
  concrete demonstration — diffs the spec against a hand-built manifest of `hs-auth`'s 16 real
  registered routes, showing 15 registered (all real spec routes) and one genuine "extra"
  (`GET /password_policy`, which `hs-auth` serves but does not appear in this spec checkout — a
  real, tool-caught mismatch, not a bug; see "Decisions made"). `cargo fmt`/`clippy -D warnings`
  clean, all 42 tests pass. Ran the binary directly against the real spec tree: 0/235 routes
  registered (0.0%) with no manifest supplied, which is the honest answer on a day nothing mounts
  the Matrix surfaces yet.
- **`tests/differential/`**: the recording proxy (`lib/proxy.py`, a real forwarding HTTP proxy
  logging every request/response pair to JSONL — point any client at it, not just this harness's
  own driver), normalizers for nondeterministic fields (`lib/normalize.py`: timestamps, event IDs,
  room IDs, tokens, and unordered scalar arrays, with two independent `Normalizer` instances per
  diff so relationships between IDs within one run are preserved through matching placeholders),
  the workload driver (`lib/driver.py`, `{{var}}` substitution and JSONPath-lite extraction between
  steps), the diff report (`lib/diff.py`), `record.py` (drives a workload through the proxy to
  produce a baseline) and `replay.py` (runs a workload directly against a candidate and diffs
  against a recorded baseline, positionally aligned by construction — see that file's docstring),
  and the five first workloads (`workloads/{registration,room_creation,messaging,membership,
  sync}.json`). `run_differential.py` detects Docker is off here (`docker info` fails) and exits 0
  with an explanation; confirmed by running it. The harness's own correctness is proven
  independently of Docker/Synapse by `tests/test_pipeline.py` (5 tests, using two small local
  `http.server` instances standing in for "two homeservers," one deliberately corrupted to confirm
  real divergences are caught) plus `tests/test_normalize.py` (9 tests) — 14 tests total, all pass
  (`python3 -m unittest discover -s tests/differential/tests`). `README.md` documents the exact
  Synapse 1.161 baseline-recording procedure for when Docker is available.
- **`tests/complement/`**: `Dockerfile.template` (multi-stage: `cargo build --release -p
  hs-server` placeholder, then a runtime image with `openssl`/`curl`, `EXPOSE 8008 8448`, a
  `HEALTHCHECK` against `/_matrix/client/versions`), `startup.sh` (signs a federation TLS cert
  against Complement's mounted `/complement/ca/{ca.crt,ca.key}` using the exact `openssl` recipe
  `refs/complement/README.md`'s "Complement PKI" section documents, trusts that CA itself, then
  execs the placeholder server invocation), `build.sh`/`run_single_node.sh` (Docker/Go/checkout
  precondition checks, clean skip if any is missing — confirmed: skips here), `run_cluster.sh` (a
  documented seam for track 03/12's cluster topology, not a working implementation — confirmed:
  skips here), `blacklist.txt` + `skip_regex.sh` (a runtime `go test -skip` regex instead of
  Complement's per-implementation build-tag convention, which needs upstream registration this
  server doesn't have). **Untested**: no `hs-server` binary exists to build an image from.
- **`tests/sytest/`**: a real Sytest plugin using Sytest's actual `Module::Pluggable` discovery
  mechanism (`SYTEST_PLUGINS=.../plugins`, each plugin a `<name>/lib/SyTest::HomeserverFactory::*`
  + `SyTest::Homeserver::*` pair) —
  `plugins/hs-reimplement/lib/SyTest/{HomeserverFactory,Homeserver}/HsReimplement.pm`, adapted in
  shape (with attribution) from `refs/sytest/lib/SyTest/Homeserver/Dendrite.pm` (Apache-2.0), the
  closest existing analog (a single monolith binary, not a Python framework). `run_sytest.sh`
  checks for the Sytest checkout, Perl, Sytest's CPAN dependencies, and an `hs-server` binary,
  skipping cleanly if any is missing — confirmed: skips here (Sytest's CPAN deps are not
  installed). Brace/paren-balance checked (`perl -c` itself needs the CPAN deps this environment
  lacks, so full compilation was not verified — see "Decisions made").
- **`tests/oracle/`**: `state_oracle.py` (drives `synapse.state.v2.resolve_events_with_store`,
  with an in-memory `StateResolutionStore` implementation whose auth-chain-difference algorithm is
  the Matrix spec's own definition, adapted in shape from Synapse's own test helper) and
  `push_oracle.py` (drives `synapse.synapse_rust.push.PushRuleEvaluator`), two illustrative
  fixtures, and `run_oracle.sh`. **Never executed end to end**: both need an installed
  `matrix-synapse` package (its compiled Rust extension — `synapse.api.room_versions` and push
  evaluation both moved out of pure Python in this Synapse checkout), which needs network this
  environment does not have. Both scripts detect `ModuleNotFoundError` and exit 0 with setup
  instructions — confirmed: both skip cleanly. `py_compile` clean; fixtures are valid JSON.
- **`tools/dashboard.py`**: the parity dashboard generator. Parses every `docs/status/*.md` file
  (title, `Last updated:`/`Updated:` line, Done/In progress/Blockers bullet counts — handles both
  phrasings other tracks use), runs `hs-spec-coverage` as a subprocess against the real spec tree
  (using any `routes.json` found in the repo, or an empty manifest if none exists — none does
  yet), and detects each `PLAN.md` section 12 layer's status (`live` / `scaffold, untested` /
  `not started`) from what exists on disk rather than a hand-maintained table. Ran it for real:
  `docs/status/dashboard.md` now exists (58 lines as of this writing), showing 0/235 spec routes
  registered, L1–L2 live, L3–L5 scaffolded-and-honestly-labeled, L6–L12 not started, and every
  reporting track's Done/In-progress/Blockers counts.
- `hs-loadgen`: explicitly skipped per this session's instructions ("skip hs-loadgen unless
  everything above is done" — everything above *is* done, but `hs-loadgen` needs
  `matrix-rust-sdk`, called out as "a heavy dependency on this shared machine," and there is
  nothing running yet for a load generator to point at).

## In progress

Nothing left mid-change; every file above is in a working, tested (or honestly-marked-untested)
state.

## Next

- **Highest value: re-run the top-level federation `tests` package** with the
  `verify_certificates: false` / `ip_range_blocklist: []` harness fix now in `startup.sh`, against
  a freshly pinned commit (build first, record `git rev-parse HEAD`, exactly as this session did).
  Expect a large jump from 5/89 — the fix removed the dominant blocker in a 3-test spot check —
  but the number has never been measured with the fix in place; do not guess it, measure it.
  Budget ~20 min for the image build (contended) and ~15-20 min for the run.
  A real fix belongs in `hs-federation`/`hs-config` (a `federation.custom_ca_list` equivalent, or
  switching the workspace `reqwest` feature set off pure `webpki-roots`), not permanently in this
  harness — see this file's "Complement" section above.
- Chase the `MustSyncUntil` timeout cluster (track 05) — it's the single largest bucket in this
  session's csapi run and appears again in the federation package; likely one or a small number
  of root causes given how many unrelated-looking tests share the exact same symptom.
- Re-run `csapi` again once track 04/05/07/08/09's items from this session's triage land, the same
  way this session re-ran 2026-09-18's number.
- Fill in the still-`TODO` `tests/sytest/plugins/hs-reimplement/lib/SyTest/Homeserver/
  HsReimplement.pm` the same way `tests/complement/`'s scaffold was filled in this session (real
  `hs` binary, real config) — Sytest's own CPAN dependencies still aren't installed in this
  environment, so it can be wired up but not run end to end yet.
- Once `hs-http`'s `Builder` (or `hs-admin-mock`) is wired into a binary that writes a real
  `routes.json`: point `hs-spec-coverage`/`tools/dashboard.py` at it and watch the coverage number
  move off 0%. (Unrelated to Complement — `hs serve` already writes `routes.json` via
  `--routes-manifest`; this bullet is about whichever track's status file still says 0%.)
- When network is available: record a real Synapse 1.161 baseline (`tests/differential/README.md`)
  and validate the two `tests/oracle/` fixtures against an installed `matrix-synapse`.
- `hs-loadgen` once the rest of Phase 0 is further along and a server exists to load-test.
- Re-run `tools/dashboard.py` after any status file changes; it is meant to be regenerated often,
  not hand-edited.

## Blockers

None for this session's own scope. Complement is unblocked as of this session (Docker + a real
binary both exist now). Still blocked on inputs outside this track's control: network access
(`pip install matrix-synapse`, Sytest's CPAN deps) for the oracle harness and the Sytest plugin's
own end-to-end run.

## Interfaces provided

- `hs-testkit`: `Scenario`, `FakeClock`, `RecordLog`, `FakeAppservice`, `FakeFederationPeer`,
  `FakePushGateway`, `FakeSmtpSink`, `matrix_error::{assert_matrix_error, MatrixErrorExpectation}`.
  Every crate in the workspace should write its integration tests against this
  (`docs/decisions/0002-workspace-conventions.md`).
- `hs-spec-coverage`: the `hs-spec-coverage` binary and its library (`ApiFamily`, `SpecRoute`,
  `RouteManifest`, `CoverageReport`, `render_markdown`) — any track can run
  `cargo run -p hs-spec-coverage -- --routes <their routes.json>` to see their own coverage.
- `docs/rfcs/0005-routes-json-manifest.md`'s `routes.json` format is the contract
  `hs-spec-coverage::manifest::RouteManifest` consumes; see "Decisions made" for the two surface
  values this crate adds to it.
- `tools/dashboard.py` / `docs/status/dashboard.md`: the project's one status report
  (`docs/workstreams/README.md` rule 5). Run `python3 tools/dashboard.py` after any status file
  changes.
- `tests/complement/`, `tests/sytest/`, `tests/differential/`, `tests/oracle/`: ready to receive a
  server binary; each has a `README.md` describing exactly what to fill in.

## Interfaces needed

- A server binary (whichever track/integration step assembles `hs-http`'s listener +
  `hs-auth`'s router + the rest into one process) to point Complement, Sytest, and the
  differential replayer at.
- `hs-http`'s or `hs-admin`'s `routes.json` output, once something writes one to disk (RFC 0005's
  format is already implemented in `hs-http::router`; nothing calls `write_to_file` yet from a
  binary this session could find).
- Track 03/12's cluster deployment manifests, for `tests/complement/run_cluster.sh` to actually
  drive (currently a documented seam only).
- Network access, for `tools/fetch-refs.sh`-cloned-but-unbuildable pieces: Sytest's CPAN
  dependencies and an installed `matrix-synapse` for `tests/oracle/`.

## Decisions made

- **`docs/rfcs/0005-routes-json-manifest.md` surface extension**: RFC 0005 (drafted by track 15
  before this track started, which the RFC itself anticipated: "track 14 may amend it once it
  starts") lists `matrix-client`, `matrix-federation`, `matrix-appservice`, `synapse-admin-compat`,
  `admin` as its `surface` enum, explicitly open for new values. `hs-spec-coverage` adds
  `matrix-identity` and `matrix-push-gateway` (`ApiFamily::manifest_surface`) for the two Matrix
  APIs this track's brief also requires coverage for that RFC 0005 did not enumerate (identity,
  push-gateway). No format change, no other track needs to do anything differently; recorded here
  as the RFC invited.
- **`hs-spec-coverage` defines its own `routes.json` schema** (`manifest.rs`) rather than
  depending on `hs-http` for `RouteManifest`. Keeps this crate consumable by any future
  `routes.json` producer, not just `hs-http::router::Builder`, and avoids a cross-track compile
  dependency for what is fundamentally a data-interchange format. Verified compatible with RFC
  0005's own example JSON (a unit test parses it verbatim) and with `hs-http::router`'s actual
  field names/enum casing (read, not depended on).
- **`tools/fetch-refs.sh`'s Synapse clone is unpinned** (`git clone --depth 1`, default branch, no
  `SYNAPSE_VERSION` tag): `tests/differential/README.md` and the project generally say "Synapse
  1.161," but the checkout in `refs/synapse` may already be ahead of that tag. `tests/oracle/`'s
  README calls this out explicitly (read the *installed* package's source when validating, not
  `refs/synapse`). Not fixed here (outside this track's file-ownership scope to change
  `tools/fetch-refs.sh` without checking who else depends on its current behavior) but worth
  another track or a later session pinning if reproducibility across sessions matters more than
  always testing against Synapse's latest development.
- **Synapse's push rule evaluation and room-version table are no longer pure Python**: as of the
  `refs/synapse` checkout in this workspace, `synapse.api.room_versions` re-exports from a
  compiled `synapse.synapse_rust.*` extension, and push rule evaluation
  (`synapse.synapse_rust.push.PushRuleEvaluator`) is entirely in that extension. This means
  `tests/oracle/`'s scripts need an *installed* `matrix-synapse` wheel, not just the `refs/synapse`
  source checkout other tracks read for behavior — recorded so a future session doesn't waste time
  trying to make the source-checkout-only path work.
- **`hs-loadgen` skipped this session**, per explicit instruction and because `matrix-rust-sdk` is
  a heavy dependency with nothing yet to load-test.
- **Fake SMTP sink is not a protocol-level SMTP server** (`hs-testkit::fake_smtp`): an in-memory
  recorder matching the minimal shape any future mailer trait needs, rather than carrying a real
  SMTP implementation as a dependency on a shared, resource-constrained machine for a feature no
  crate sends mail through yet.
- **`tests/complement/startup.sh` now sets `federation.verify_certificates: false` and
  `federation.ip_range_blocklist: []`** in the generated `config.yaml` (2026-09-19). This is a
  test-harness-only workaround for a real gap in `hs-federation`/`hs-config` (no
  `federation_custom_ca_list` equivalent, and `reqwest`'s `rustls-tls` feature never reads the OS
  trust store `update-ca-certificates` populates) — see this file's "Complement" section above for
  the full diagnosis. Matches the *effect* of Synapse's own Complement config
  (`federation_custom_ca_list` + `federation_ip_range_blacklist: []`) without the mechanism, since
  this project doesn't have that mechanism yet. Flagged in the script's own comment as not a
  production recommendation; the real fix is track 06/`hs-config`'s to build, at which point this
  workaround should be replaced with a proper `custom_ca_list` entry pointing at
  `/complement/ca/ca.crt` (matching Synapse) rather than disabling verification outright.
- **No `blacklist.txt` entries added this session** despite 138+139 failing assertions across two
  packages: every failure traced to either an active bug or documented future work, not a
  conscious "we will not implement this." See this file's "Blacklist decision" section above for
  the two candidates considered and rejected as unjustified without a matching failure.

## Shared dependencies added

None. `hs-testkit` and `hs-spec-coverage` use only workspace dependencies already present
(`axum`, `http`, `http-body-util`, `tower`, `tokio`, `serde`, `serde_json`, `serde_yaml_ng`,
`bytes`, `thiserror`, `tracing`) plus path dependencies on `hs-kv` (regular) and `hs-auth`
(dev-only, for the integration test). No new `[workspace.dependencies]` entries were needed.

## How to verify

```bash
# hs-testkit: fake clock, scenario DSL, fake doubles, and the hs-auth round-trip proof
cargo fmt -p hs-testkit -p hs-spec-coverage
cargo clippy -p hs-testkit --all-targets -- -D warnings
cargo test -p hs-testkit --all-targets   # 19 unit + 3 integration tests

# hs-spec-coverage: OpenAPI parsing, routes.json schema, coverage diff, Markdown report
cargo clippy -p hs-spec-coverage --all-targets -- -D warnings
cargo test -p hs-spec-coverage --all-targets   # 36 unit + 6 real-spec integration tests
cargo run -p hs-spec-coverage -- --spec-dir refs/matrix-spec/data/api   # 0/235 registered, honest

# Differential harness (no Docker/Synapse needed for these):
python3 -m unittest discover -s tests/differential/tests -v   # 14 tests
python3 tests/differential/run_differential.py                # clean skip, exit 0

# Complement (real as of 2026-09-18/19; ~10-20 min image build under contention, ~14 min per
# csapi run, ~15-20 min for the top-level federation `tests` package):
./tests/complement/build.sh complement-hs-reimplement:dev
cd refs/complement && COMPLEMENT_BASE_IMAGE=complement-hs-reimplement:dev \
  go test -v -timeout 30m ./tests/csapi/...          # csapi: 148 pass / 138 fail / 7 skip (leaf)
cd refs/complement && COMPLEMENT_BASE_IMAGE=complement-hs-reimplement:dev \
  go test -v -timeout 30m ./tests/...                # everything, incl. federation; ~16-20 min

# Sytest / oracle scaffolds (clean-skip without their respective dependencies):
./tests/complement/run_cluster.sh
./tests/sytest/run_sytest.sh
./tests/oracle/run_oracle.sh

# The parity dashboard:
python3 tools/dashboard.py && cat docs/status/dashboard.md
```
