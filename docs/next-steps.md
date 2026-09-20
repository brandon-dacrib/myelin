# Where this is, and what comes next

Written 2026-09-20 by the integration lead. `PLAN.md` is the design and rarely changes; this file is the resume point and changes every session. `docs/status/dashboard.md` is the generated measurement; per-track detail lives in `docs/status/NN-*.md`.

The project is **Myelin**, and it is public: <https://github.com/brandon-dacrib/myelin>. The crates still carry the `hs-` prefix from before it had a name.

## The state of things

**A real client works.** Element Web — the actual browser client most Matrix users run — signs in against this server, shows a room list, sends and receives messages live between two independent sessions, propagates a display-name change to an already-open tab, and scrolls back through history. Screenshots in `docs/design/screenshots/`, reproduction in `web/element-testing/README.md`. That was the project's stated definition of success from day one. The loud exception is closed: `/createRoom` merged its power-level override backwards, which made creating a room from the UI fail unconditionally, and it no longer does. `unsigned.prev_content` and `GET /account/3pid` went with it. **None of the three has been re-checked against a real Element session** — the unit and end-to-end tests cover them, and nobody has opened a browser since.

**Configuration lives in the database** (RFC 0016). The file is a bootstrap and a seed; the database outranks it, `HS__` variables outrank the database, and the admin API refuses a write the environment would shadow rather than storing one that gets ignored. The web interface has a Configuration section that builds its forms from the server's own JSON Schema, and `hs config show|get|set|unset|import|export|history` is the same thing without a browser.

**A first run is one command.** `hs serve --data-dir ./data --server-name example.org` in an empty directory produces a working server — database, signing key, media path, all underneath that directory. It was 158 lines of generated YAML with four mandatory hand-edits.

**Complement, `csapi`: 191 of 296 assertions pass** (53 of 106 top-level), up from 148/293 and 125/293 on the two runs before it.

**Complement, federation package: 52 of 212 assertions** (6 of 88 top-level). That is now a real measurement rather than a TLS wall: `federation.custom_ca_certificates` works, and the harness no longer disables certificate verification — it trusts Complement's CA the way a deployment would trust a private one.

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

**Those numbers are from before this session and have not been re-measured.** The `-skip` should no longer be needed — `/get_missing_events` answered newest-first, which is what dereferenced a nil state key in Complement's Go binary and killed the run 21 tests in, and it now answers oldest-first with a test pinning it. That is a local test, not a Complement run. Re-running both suites without the `-skip` is the first thing worth doing next, because it re-grades everything below it.

## What to do next, in order

### 1. Re-measure, then chase what the measurement says

Nothing below this line is trustworthy until the two Complement suites run again, without the
`-skip`, on this code. Three fixes this session were aimed squarely at conformance and none has
been graded: the `/createRoom` power-level merge, `unsigned.prev_content` across every endpoint
that renders an event including `/sync`, and the `/get_missing_events` ordering that was crashing
the federation suite. Open a browser at Element too, and create a room in it — the bug that made
that impossible is fixed and unobserved.

Two known conformance gaps in `/get_missing_events` that the ordering fix did not touch, both
visible in `TestInboundCanReturnMissingEvents` once it stops crashing: `min_depth` is parsed
nowhere and ignored, and history visibility is not applied per event, so the `joined` and
`invited` halves of that test will want redacted copies of events from before the requester
joined and will get full ones.

### 2. Make it fun to administer — the half that is left

First run is done (see the state of things). The admin interface can now *change* the
configuration rather than only display it, which was the stated product priority. What it still
cannot do, in rough order of how often an operator will hit it:

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
- **Be trusted after a blip.** Running `e2e-real` as a whole suite, two pages land on the sign-in
  screen; the trace shows their `GET /api/v1/me` never completed (status `-1`, not `401`) and the
  app concluded there was no session. A failed request is not a rejected token, and an operator
  should be told the server is unreachable rather than silently signed out. Each test passes
  alone, so this reproduces only under the full suite.
- **Have the config pages checked by axe.** Every other e2e flow runs axe at each step; the
  Configuration pages have never been through it.

Still on the first-run side, all left where their owners can see them: `README.md`'s quickstart is
still the four-edit flow and could become one `docker run`; `deploy/Dockerfile`'s `CMD` still
points at a config file; and `deploy/helm/hs/templates/configmap.yaml` never sets
`media.storage.path`, so uploads fall back to `./media-store` relative to the working directory
and fail on a read-only root filesystem. That last one is a real bug, found while doing this and
not caused by it.

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
| A failed `/me` signs the operator out | `web` | a blip looks like a rejected token |
| Receipts and presence in memory | `hs-user` | a restart forgets read state |
| Postgres `tls`/`pool_size`/schema | `hs-kv`, `hs-cli` | encrypt in front of the database for now |
| `/createRoom` not shard-gated | `hs-cli` | first actor may be built on a non-owner |
| Config pages never checked by axe | `web` | the only e2e flow without an accessibility pass |
| Sytest never run | `tests/sytest` | CPAN dependencies absent |
| `cargo fuzz` never executed | `fuzz/` | no nightly toolchain |

## Conventions worth keeping

- **Verify by running.** Every claim above was checked against the binary, a real client, or a conformance suite. Reports that were taken on trust have been wrong repeatedly — a "clean typecheck" with two errors, a gap reported open that had been closed hours earlier, a receipts bug that was a stale binary.
- **Point real things at it.** Every serious bug this project has found came from Complement, a real SDK, or a real browser — never from its own tests. The signing bug had passed every test for weeks because the signer and the verifier shared the same wrong assumption.
- **Run the gates CI runs.** `cargo test -p <crate>` cannot see what `--workspace --all-targets` sees: feature unification, cross-crate visibility, dead code. Seven consecutive red CI runs came from exactly that gap.
- **Wait for conditions, not durations.** Five cluster tests advanced a virtual clock a fixed number of rounds and asserted a background task had kept up. They passed locally every time and failed on CI, including one that needed *real* time because the work finishes on a blocking thread.
- **Keep the repository off iCloud.** `git status` took 600 seconds there and takes 0.24 here.
- **Registered is not working.** 24 of 142 admin operations are genuinely served; the rest answer 501, or 503 when a seam exists but nothing implements it.
