# 13. Configuration, compatibility and migration: status

Track brief: `docs/workstreams/13-config-compat-and-migration.md`. Owner
crates/files: `crates/hs-config`, `crates/hs-compat`,
`tools/synapse_inventory.py`, `docs/synapse-inventory.md`.

## 2026-09-29: configuration history and hot reload merged

History/revert (`eedb090`) and hot reload (`12a19eb`) are on pushed `main`. Saves and reverts
validate the same database revision they replace, including changes from another writer.
The real-binary history and reload tests passed in the final full gate; the Configuration page's
five real-server browser flows passed separately. The full gate had 2,343 passing Rust tests,
443 web unit tests and 50 mock browser flows. Details and remaining limits are in the
[integration review](reviews/admin-completion-2026-09-29.md) and status 15.

Last updated: 2026-09-29 (integration complete; earlier work: the Migration admin area and the online importer, first section; queue items 2b and 2c, second). Before that: 2026-09-27 (decision 0010: bootstrap-only settings, importer-only registration files; see "Where this stopped"). Before that: 2026-09-19 (session 2 — catching up `hs-config`/`hs-compat` with day one's
shipped work: URL-preview config fields, the `serve_server_wellknown`/`federation_custom_ca_list`/
`max_spider_size` translation-table corrections, and a first slice of `/_synapse/admin` routes).

## 2026-09-28: the reload boundary says what the server does (decision 0016)

`crates/hs-config/src/reload.rs` claimed `rate_limits`, `federation`, `telemetry` and
`appservices` were reloadable while nothing in `hs serve` re-read any of them. It is now
setting-granular and truthful: `HOT_SETTINGS` (JSON Pointers) lists only what a running server
re-reads -- today `/rate_limits`, `/migration`, the three federation allow/block lists and `/telemetry/logging/level` -- and grows as something is wired
(`crates/hs-cli/src/live_config.rs`). `sections_requiring_restart` ignores hot settings, and
`hot_sections_changed` says which sections a running server has to re-read. This supersedes the
"reload boundary is section-granular" decision below. See status 15 for the rest.

Found on the way, through the interface against a real server: saving one field of a rate-limit
bucket (`rate_limits.message.burst_count` alone, which is what the form sends) was refused
"missing field `per_second`" unless the file spelled the whole bucket out. Each bucket is now
read through its own `partial_<bucket>` function (`crates/hs-config/src/ratelimit.rs`) that fills
a left-out field from that bucket's default, so a partial `HS__` override works too. This
supersedes the "`RateLimitBucket`'s two fields are not individually `serde(default)`-ed" decision
below.

## 2026-09-28: the Migration admin area and the online importer (8/8)

Before this there was no importer, only its design (`docs/compat/synapse-importer-mapping.md`).
Now a running server copies a Synapse deployment into itself through the admin API, and the
Migration page walks an operator from pointing at Synapse to cutover without a file edited.
Runbook: `docs/compat/synapse-migration-runbook.md`.

**Done and verified:**

1. **`hs-config`: the `migration` section** (`crates/hs-config/src/migration.rs`):
   `migration.synapse = {database: {host, port, database, user, password}, media_store_path,
   batch_size}`. Administered and reloadable; the password is a secret (redacted on read, kept on
   save). `MigrationStartRequest.source_secret_ref` is a pointer into it (`/migration/synapse`
   by default), so no connection string is ever in the migration API.
2. **`hs-compat::migration`** (`crates/hs-compat/src/migration/`): `source.rs` reads Synapse's
   PostgreSQL (`tokio-postgres`, rows as `to_jsonb` so a column an older or newer Synapse lacks
   is absent rather than an error, keyset-paged on a stable key); `order.rs` plans each room's
   replay (topological over `prev_events` and `auth_events`, ties by depth then stream order;
   rejected events and outliers left out; a room without its own `m.room.create` is one joined
   over federation and is skipped); `engine.rs` is `Migrator`, the state machine (`idle`,
   `copying`, `paused`, `ready_for_cutover`, `verifying`, `cutting_over`, `completed`, `failed`,
   `aborted`), the copy (six streams, a checkpoint per stream after each batch, every write
   idempotent), verification (every row looked up, samples compared field by field, every
   room's current state against `current_state_events`, media bytes) and cutover (a final pass,
   then verification; `completed` only if it passes). A `run` counter makes a stopped copy
   unable to overwrite what replaced it. `Migrator` implements `hs_admin::migration::MigrationSource`
   and runs each step as a task (`migration.copy`, `.verify`, `.cutover`); `recover()` at startup
   carries a running step on from its checkpoint.
3. **`hs-admin::migration`**: the eight handlers (`migration.get/log/start/pause/resume/abort/
   verify/cutover`), audited and published (`migration.started`, ...), `202` with `Location` for
   verify and cutover. `openapi.yaml`: additive fields on `MigrationStatus` (`task_id`,
   `started_at/by`, `updated_at`, `completed_at`, `cutover_by`, `verification`, and per stream
   `skipped_count`, `failed_count`, `done`) and a `MigrationVerification` schema.
4. **`hs-room`**: `RoomRegistry::import_shell`, `discard_import_shell`,
   `RoomActorHandle::import_event` (authorized and stored as an event arriving over federation
   is, but no `RoomUpdate` published: the federation sender, bridges and `/sync` never see old
   history as new) and `import_redaction`. Test: `crates/hs-room/tests/import.rs`.
5. **`hs-cli::migration`**: `StoreTarget` writes into `hs-auth` (accounts with Synapse's bcrypt
   hash, flags and profile; devices; tokens by their SHA-256, so Synapse's token strings keep
   signing in), `hs-user` (account data), `hs-room` (then one head update to the session hub, so
   members' `/sync` lists the room) and `hs-media` (same media ids); `TablesMigrationStore`
   (keyspaces `hs_compat.migration` and `hs_compat.migration_log`); `StoreSourceConfigs` reads
   the source from the configuration store at start (`StoreConfigSource::current_config`);
   `MigrationMetrics` (`hs_migration_rows_copied/skipped/failed/source{stream}`,
   `hs_migration_status{status}`). Wired in `serve.rs`, with `recover()` spawned at boot.
6. **The Migration page** (`web/src/pages/migration/MigrationPage.tsx`, track 16's area, built
   here): source form, copy with pause/resume/abort, verification findings, the cutover
   checklist, the log. The sidebar shows Migration once a migration exists, or on a server with
   at most one account.
7. **The fixture** `crates/hs-compat/tests/fixtures/synapse-small`: a real Synapse 1.161 on
   PostgreSQL populated through its own client API (4 accounts, 6 devices and tokens, 2 rooms
   with 43 events including an edit and a redaction, account data and a tag, 2 uploads), with our
   own minimal `schema.sql` (column names only; nothing copied from Synapse) and the data rows.

**Verified by:**

- `cargo test -p hs-cli --test migration` (the real `hs` binary, 1 test): source set through
  `config.update` (password never answered back), start, `ready_for_cutover` with the expected
  counts, a restart (status kept), verify (a task; passed), cutover (a task; `completed`), a
  restart; then alice's Synapse token still works (`whoami` with her device), her Synapse
  password signs in, dave stays deactivated, both rooms in her `/sync`, the lobby's history with
  the redacted message redacted, her profile, `#lobby:fixture.test` resolves, her account data
  and tag, bob's picture byte for byte; the audit log, the migration log and the metrics.
- `cargo test -p hs-compat --test migration` (5 tests, a real Synapse database, an in-memory
  target): the reader, a copy verified then tampered with and caught, cutover's final pass
  repairing it, pause/resume/abort without copying twice, a copy interrupted by a restart
  carrying on after its checkpoint, refusals (no source, another server's Synapse, an
  unreachable database).
- `cargo test -p hs-admin migration` (5), `cargo test -p hs-room --test import` (2),
  `hs-compat` unit tests (`order`, `source`).
- `web/`: `MigrationPage.test.tsx` (4), `e2e/migration.spec.ts` (mock, axe on each state), and
  `e2e-real/migration.spec.ts` against `hs serve` and the fixture database (passed; screenshots
  `docs/design/screenshots/migration-*-real.png`).

**Left** (also in `docs/next-steps.md`'s Known gaps): end-to-end keys and backups, push rules and
pushers, receipts, filters and remote media are not copied; rooms joined over federation are
skipped (their members rejoin); a room is replayed whole in memory, and nothing has measured a
large Synapse; one replica runs the migration (whichever answered `start`), and its steps are
serialized by a lock in that process only.

**Decisions made:** PostgreSQL sources only (SQLite goes through `synapse_port_db` first). The
source lives in the configuration (`migration` section), not in the migration API. Abort keeps
what was copied (Synapse is never written, so nothing needs undoing there, and a new start carries
on from it). The final pass at cutover re-reads everything rather than tracking per-table
watermarks: simpler, idempotent, and correct for updated rows; slower for a large deployment.
Imported room history is not announced (`import_event`), so nothing is re-sent over federation or
to bridges. `hs-compat` now depends on `hs-admin` (for `MigrationSource` and the task registry);
no cycle, `hs-admin` depends on neither.

**Shared dependencies added:** none at the workspace level; `hs-compat` uses the workspace's
`tokio-postgres`, `async-trait`, `tokio` and `tracing`.

## 2026-09-28: queue items 2b and 2c (the ICAP preview control, RFC 0020, the 0010 sweep)

**Done and verified against the real `hs` binary:**

1. **`PreviewMode` is reshaped (2b; track 13's call, since it is the file format).**
   `crates/hs-config/src/scanning.rs` writes its serde by hand. The derived externally tagged
   form read `{bytes: N}` only from JSON, but every configuration (file, database section,
   admin API patch) is parsed through `serde_yaml_ng`, which wanted a YAML tag (`!bytes 4096`),
   so a forced preview size could not be set at all. It now reads `negotiate`, `off`,
   `{bytes: N}` and a bare number, from YAML and JSON, and always writes `negotiate`, `off` or
   `{bytes: N}`. The JSON Schema is unchanged in shape. Test:
   `a_forced_preview_size_is_read_from_every_form_and_written_as_a_map`.
2. **The web's schema fixture is pinned to `schema_for!(Config)`.**
   `crates/hs-config/tests/web_schema_fixture.rs` fails when
   `web/src/test/fixtures/hs-config-schema.json` differs from the live schema, and says how to
   regenerate it (`HS_UPDATE_WEB_SCHEMA_FIXTURE=1 cargo test -p hs-config --test web_schema_fixture`).
   The fixture is regenerated; `config-model.ts` handles the unit-or-`{bytes: N}` choice, so
   every setting in the real schema has a real control.
3. **RFC 0020 (a hidden secret inside a list entry is lost on save)** is implemented in both
   forms, server side in `hs-admin` (`SecretPaths::restore_echoed_secrets`) and in the web
   (`markSecretOrigins`). See the RFC's "As implemented". `config.update` logs how many secrets
   it kept (`kept hidden secrets inside list entries ...`, `info`).
4. **Decision 0010 docs sweep (2c).** The Helm chart's `values.yaml` and `configmap.yaml`
   comments say which settings are bootstrap and that the rest are administered in the
   interface; `docs/bridges/heisenbridge.md` and `mautrix.md` register bridges through the admin
   API, not `registration_files`; `deploy/media-scanning/README.md` sets scanning through the
   API; `gen_config_docs` marks bootstrap sections and settings, and `docs/config.md` is
   regenerated. The README already described a server with no configuration file.
5. **The Configuration page shows the bootstrap split** (per-setting `bootstrap` flag, "Set at
   install", `listeners` as a bootstrap section), against the real binary:
   `web/e2e-real/configuration.spec.ts` (3 flows: the preview choice stored as `{bytes: 4096}`
   then `off`; bootstrap settings shown and refused with 409; two OIDC providers reordered and
   renamed with both secrets kept). Passed 3/3, twice in a row, against `hs serve`.
6. **The bootstrap split on PostgreSQL**:
   `hs-cli` `bootstrap::tests::on_postgres_two_replicas_keep_their_own_bootstrap_and_share_the_rest`
   boots two replicas on one PostgreSQL database; each keeps its own listeners and mesh
   identity, they share the administered settings, a bootstrap setting cannot be stored, and a
   pre-0010 row holding one is purged at the next boot. It needs a server at
   `HS_BOOTSTRAP_TEST_POSTGRES_DSN` (default `postgres://postgres:hspg@127.0.0.1:5439/postgres`)
   and skips, saying so, without one.

Verify: `cargo test -p hs-config`; `cargo test -p hs-admin config_schema`;
`docker run --rm -d --name hs-pg -e POSTGRES_PASSWORD=hspg -p 127.0.0.1:5439:5432 postgres:17`
then `cargo test -p hs-cli --lib on_postgres`; in `web/`, `npm run check` and, with a running
`hs serve`, `HS_REAL_SERVER_URL=... HS_REAL_ADMIN_TOKEN=... npx playwright test --config
playwright.real.config.ts e2e-real/configuration.spec.ts`.

## Where this stopped (2026-09-27, branch `agent/bootstrap-only-config`, decision 0010 server side)

**Done and verified** (commands below all passed on this branch):

1. **Bootstrap vs. administered, explicit in code.** `crates/hs-config/src/bootstrap.rs` defines
   the bootstrap set (`BOOTSTRAP_SETTINGS`, each with a `BootstrapReason`): `/storage`,
   `/listeners`, `/server/server_name`, `/server/signing_key_path`, `/cluster/single_node`,
   `/cluster/mesh` (port, advertise_address, tls, shared secret), `/appservices/registration_files`.
   Everything else is administered. `ConfigStore::seed` strips them (the server name is kept as
   the store's identity, `ConfigMeta::server_name`, not as a setting); `patch_section` refuses
   them (`StoreError::BootstrapSetting` / `BootstrapSection`, whose message names `HS__` and the
   bootstrap file); `Layers` ignores any in the database layer; `ConfigStore::purge_bootstrap`
   removes them from a pre-0010 store with a history entry per section (actor `PURGE_ACTOR`) and
   `hs serve`'s boot runs it and reports what it removed. `BOOTSTRAP_SECTIONS` is now
   `storage` and `listeners`. The admin API refuses them in `config.update` (409) and
   `config.validate` (invalid), and `GET /config/schema` marks each one `editable: false,
   bootstrap: true` (new `ConfigSettingInfo.bootstrap`, in `openapi.yaml`). `hs config set` and
   `hs config import` refuse / skip them. This fixes the two-replica bug in
   `docs/next-steps.md` ("Found on the way, for track 03 and 13").
2. **`appservices.registration_files` is importer-only.** `crates/hs-cli/src/appservices.rs`:
   each listed file is imported once (an appservice already registered under the same id is
   left alone), recorded in the config store (`ImportRecord`, `import/<kind>/<key>`), logged,
   and audited as `appservices.import` (actor system); later starts skip it without reading it,
   so API edits and removals survive restarts and a deleted file no longer blocks startup.
   `hs-compat` reclassifies `app_service_config_files` as Mapped (diff) in `classification.rs`
   and `docs/compat/synapse-config-table.md` (summary now 25 / 25 / 179). The bridge e2e test
   (`crates/hs-cli/tests/e2e.rs`, `a_bridge_is_sent_what_happens_...`) checks the audit entry,
   deletes the registration file before its restart, and still gets delivery.
3. **Two administered settings that were file-only are now settings.** `media.scanning` moved
   into `crates/hs-config/src/scanning.rs` (hs-media re-exports the same types from
   `hs_media::scanning::config`; `http.auth_token` is `x-secret`), and
   `server.unstable_features` feeds `GET /versions`. `--media-scanning-config` and
   `--capabilities-config` still work, deprecated, with a startup warning.

Verified: `cargo test -p hs-config` (126 + 3 + 1), `cargo test -p hs-admin` (176 + 5 + 2),
`cargo test -p hs-compat` (50 + 7), `cargo test -p hs-cli --lib` (145),
`cargo test -p hs-cli --test bridge_offerings` (3), `cargo test -p hs-cli --test e2e --
a_bridge_is_sent` (1), `cargo test -p hs-media -- scanning` (74), `cargo clippy -p hs-config -p
hs-media -p hs-cli -p hs-admin -p hs-compat --all-targets -- -D warnings`, `cargo fmt --all
--check`, all with `CARGO_PROFILE_DEV_DEBUG=0`. Tests named by the task:
`store::tests::bootstrap_settings_are_not_seeded_and_the_server_name_becomes_the_identity`,
`layered::tests::two_replicas_seeding_one_database_keep_their_own_listeners_and_mesh_port`,
`bootstrap::tests::two_replicas_booting_on_one_database_keep_their_own_listeners_and_mesh_port`
(hs-cli, real embedded store), `appservices::tests::a_registration_file_is_imported_once_and_then_managed_through_the_registry`.

**Not done / next steps, in order:**

1. **Docs sweep (task item 3), not started.** README ("Behind a reverse proxy, set
   `HS__SERVER__PUBLIC_BASEURL`" can stay, it is pre-setup, but say it pins the setting),
   `deploy/helm/hs/values.yaml` and `templates/configmap.yaml` comments (the ConfigMap comment
   says the file "seeds the database"; listeners no longer seed), `deploy/media-scanning/README.md`
   (point at `media.scanning` in the interface, not `--media-scanning-config`),
   `docs/bridges/*.md` (bridges via the Bridges section; registration files are a one-time
   import), `docs/next-steps.md` (strike the "Found on the way" per-replica item and the
   `registration_files` half of queue item 2), regenerate `docs/config.md` with
   `cargo run -p hs-config --bin gen_config_docs` (new `media.scanning`, `server.unstable_features`).
2. **Web (track 16), not started.** `web/src/api/config-schema.ts` `KNOWN_BOOTSTRAP_SECTIONS`
   should be `["storage", "listeners"]`; read the new per-setting `bootstrap` flag into
   `ConfigSettingInfo` and have `SettingRow.tsx` say "set at install (bootstrap file, HS__
   variable or Helm values)" instead of "This server will not accept a change"; mock
   `web/src/mocks/data/config.ts` `BOOTSTRAP` and `editable` to match; regenerate
   `web/src/api/schema.d.ts` (`npm run generate:client`). `npm ci` was run in this worktree's
   `web/` but nothing there was changed or tested.
3. The chart's ConfigMap still renders `listeners` into the file layer, which is correct (the
   file is where bootstrap lives); nothing to change in templates, only comments.
4. Not run: the full `cargo test --workspace`, the other `hs-cli` integration tests
   (`federation_*`), Complement, and anything against PostgreSQL (the purge is backend-agnostic
   `hs-kv` code, tested on memory and Fjall only).

**Decisions made** (also in decision 0010's Consequences): the bootstrap list above; the server
name is recorded once as the database's identity and a file or `--server-name` that disagrees is
overridden with a note (an `HS__SERVER__SERVER_NAME` that disagrees still wins, with a note, as
before); `*_file` secret references and `media.storage` stay administered (they are the same on
every replica); a registration file's import is keyed by the path as written and an edited file
is never re-read.

## Session 2 (2026-09-19)

A great deal landed elsewhere in the tree on day one that this track's config surface and
translation table had not caught up with: `hs serve` gained PostgreSQL storage, `.well-known`,
profiles, a real admin API with an audit log, inbound federation with backfill, URL previews,
async media upload and federation CA configuration — several with config gaps explicitly
deferred to this track, and at least one translation-table row (`serve_server_wellknown`) that had
already been corrected in code (`crates/hs-compat/src/classification.rs`) but not in the
human-maintained markdown table, which is a real "table overclaims/underclaims" bug in its own
right per this track's own stated risk. This session:

1. **Added the three RFC 0006 URL-preview fields to `hs_config::MediaConfig`**
   (`crates/hs-config/src/media.rs`): `url_preview_timeout` (`Duration`, default 30s),
   `url_preview_max_fetch_size` (`ByteSize`, default 10 MiB), `url_preview_cache_lifetime`
   (`Duration`, default 1h). Defaults were not guessed: `refs/synapse/synapse/http/client.py`'s
   `get_file` hardcodes a 30-second body-read timeout (`timeout_deferred(..., timeout=30, ...)`,
   checked), `refs/synapse/synapse/config/repository.py` hardcodes `max_spider_size`'s own default
   at `"10M"` (checked), and `refs/synapse/synapse/media/url_previewer.py` hardcodes a one-hour
   cache lifetime (`ONE_HOUR = 60 * 60 * 1000`, checked) — none of the three are configurable in
   Synapse itself, all three are new, real settings here. Validation added (both must be
   non-zero). 4 new unit tests in `crates/hs-config/src/media.rs`. **`hs-media` (track 09) is not
   edited by this track** (out of ownership) — the one-line consumer change it needs, verbatim
   from its own status file's request: in `crates/hs-media/src/preview.rs`, replace
   `FetchLimits::default()` with a constructor built from `config.url_preview_timeout`/
   `url_preview_max_fetch_size` (e.g. add `FetchLimits::from_config(config: &MediaConfig)`), and
   replace the `DEFAULT_PREVIEW_CACHE_TTL_MS` argument to `metadata.put_preview_cache` with
   `config.url_preview_cache_lifetime.as_millis()`. This also changes the *effective* default
   behavior once wired: `FetchLimits::default()` today hardcodes 10s/10 MiB; the new config
   default is 30s/10 MiB (the max-fetch-size default is unchanged, only the timeout moves to match
   Synapse's real behavior) — worth a one-line mention in `hs-media`'s own status file when it
   wires this through, since it is a small, deliberate behavior change, not just a refactor.
2. **Investigated the PostgreSQL `pool_size`/`tls` gap named in the brief and found it already
   closed at the config-schema level** (superseded 2026-09-30: `tls` became `ssl_mode`, kept as an
   alias, and `pool_size`, `schema` and `ssl_mode` all reach the connection; `database.args.sslmode`
   and `sslrootcert` translate; see status 01 and the translation table): both fields already exist on
   `hs_config::storage::PostgresStorageConfig` (added before this session). The actual gap is
   entirely in the *consumers*, not the schema: `hs_kv::postgres_backend::PostgresBackend::open`
   hardcodes `max_size(16)` and `postgres::NoTls` regardless of these fields (track 01's own status
   file names this, item 5 of "Wiring the integration lead must add"), and
   `crates/hs-cli/src/storage.rs`'s `open_postgres` hardcodes `"public"` for the schema parameter
   and returns `StorageOpenError::PostgresTlsUnsupported` rather than silently ignoring `tls: true`
   (checked, both files). Doc comments on both fields in `crates/hs-config/src/storage.rs` were
   updated to cite exactly this (which file hardcodes what, checked) so a future reader does not
   have to re-derive it. **A `schema` field for `PostgresStorageConfig` was drafted and then
   reverted** — see "Decisions made" below for why; it is real future work, not done this session.
3. **Audited and corrected the Synapse translation table**
   (`crates/hs-compat/src/classification.rs` and `docs/compat/synapse-config-table.md`, which must
   agree — every change below was applied to both files):
   - **`serve_server_wellknown`**: the markdown table still said `Unsupported` while
     `classification.rs` already said `MappedDiff` (the integration lead had corrected the `.rs`
     file but not the `.md` one — the exact "table overclaims/underclaims" failure mode this
     track's brief warns about, caught by cross-checking the two files against each other rather
     than trusting either alone). Fixed the markdown to match, and went further: the translator
     (`crates/hs-compat/src/translate.rs`) had **no translation arm for this key at all** despite
     being classified `Mapped (diff)` with a named native path — a real, silent gap where a
     mapped key was reported in the `TranslationReport` but never actually written to the output
     `Config`. Fixed: `translate()`'s pass 1 now extracts `server_name` before everything else
     (so key order in the source file cannot matter), and a new `derive_well_known_server` helper
     reproduces Synapse's own derivation exactly (`refs/synapse/synapse/rest/well_known.py`'s
     `ServerWellKnownResource.__init__`: `host:443`, or `server_name` verbatim if it already names
     a port — checked). 4 new tests.
   - **`federation_custom_ca_list`**: was `Unsupported`; a native field
     (`federation.custom_ca_certificates`) now exists and is genuinely wired into
     `crates/hs-federation/src/client.rs`'s TLS trust store (checked, not just accepted-and-
     ignored). Reclassified `Mapped (diff)` — **diff, not plain Mapped**, because
     `refs/synapse/synapse/config/tls.py`'s `trustRootFromCertificates` *replaces* Synapse's
     platform trust root with exactly this list (checked in
     `refs/synapse/synapse/crypto/context_factory.py` too — it is the sole `trustRoot` passed to
     `CertificateOptions`), while the native field *adds* these CAs on top of the bundled public
     roots (per `crates/hs-config/src/federation.rs`'s own doc comment, checked) — a real
     trust-semantics divergence an operator migrating a closed private-CA federation should see,
     not a cosmetic difference. Translator arm added. 1 new test.
   - **`max_spider_size`**: was `Unsupported`; now `Mapped` onto the field this session added
     (`media.url_preview_max_fetch_size`), with the same default (`ByteSize::mib(10)` vs Synapse's
     `"10M"`, checked). Translator arm added (reuses the existing `get_bytesize_value` helper). 1
     new test.
   - Every row changed cites the exact file checked to establish the new classification, per this
     track's own caution about the translation table being a claim about behavior — none of the
     three were changed on documentation alone.
4. **A first slice of `/_synapse/admin` compatibility routes, implemented, not just mapped**
   (`crates/hs-compat/src/admin_proxy.rs`, new module, 9 tests): `GET
   /_synapse/admin/v1/server_version`, `GET /_synapse/admin/v2/users`, `GET
   /_synapse/admin/v2/users/{user_id}`, `GET /_synapse/admin/v1/rooms`, `GET
   /_synapse/admin/v1/rooms/{room_id}` — the exact set the task that produced this session named
   ("the version endpoint, user and room queries at minimum"). Design: these forward
   **in-process**, via `tower::Service`/`ServiceExt::oneshot`, into the already-built,
   state-erased `axum::Router` that `hs_admin::router::build_router(state)` returns (the same
   router `hs serve` mounts at `/api/v1`) — no network hop, no dependency on `hs-admin`'s internal
   `AdminState` wiring, and (deliberately) **no Cargo dependency on the `hs-admin` crate at all**:
   every native response is parsed generically as `serde_json::Value` and re-shaped by field name,
   so this module only needs `hs-admin`'s public JSON contract to stay stable, not its Rust types.
   Every JSON shape was checked against `refs/synapse/docs/admin_api/{user_admin_api,rooms}.md`
   and, where the docs were ambiguous or looked inconsistent (they are — see the module's doc
   comment on `creation_ts`), against the actual Synapse source
   (`refs/synapse/synapse/handlers/admin.py`, `refs/synapse/synapse/rest/admin/{users,rooms}.py`)
   rather than trusting the prose. Every native path (`/api/v1/server`, `/api/v1/users`,
   `/api/v1/users/{user_id}`, `/api/v1/rooms`, `/api/v1/rooms/{room_id}`) was checked against
   `crates/hs-admin/src/router.rs` to confirm it is a *real, implemented* handler, not one of the
   many operations that still answer a generic `501`. See "Interfaces provided" for the mounting
   instructions `hs-cli` needs, and this module's own doc comment for the full design rationale
   and every stated limitation (no write routes, no `is_guest` field, `v3`'s different
   `deactivated` semantics not implemented, etc.).

## Done

- **`hs-config`** (Phase 0 deliverable: "native config schema"; week-4
  frozen interface for 12 and everyone else): the full native configuration
  schema as serde structs, covering server identity, listeners, storage
  backend selection, media, federation policy, rate limits, auth,
  appservices, telemetry and cluster. Defaults on every field. Validation
  (`Config::validate`, collects every problem rather than stopping at the
  first). `HS__section__key` environment overrides
  (`crates/hs-config/src/env.rs`). File-backed secrets via `*_file` keys,
  redacted `Debug` output (`crates/hs-config/src/secret.rs`). A documented
  reload boundary (`crates/hs-config/src/reload.rs`:
  `RELOADABLE_SECTIONS` is `rate_limits`, `federation`, `telemetry`,
  `appservices`; everything else needs a restart, with the reasoning in
  that module's doc comment) plus `sections_requiring_restart` to diff two
  configs. `docs/config.md` generated from the schema
  (`cargo run -p hs-config --bin gen_config_docs`; regenerate after any
  schema change — it is not committed-and-forgotten, it reads the schema
  live). 69 unit/integration tests plus 1 doctest, all passing; `cargo
  fmt --check` and `cargo clippy --all-targets -- -D warnings` clean.
- **`docs/compat/synapse-config-table.md`**: the full translation table.
  Every one of the 229 documented top-level options and 51
  `experimental_features` flags from `docs/synapse-inventory.md`,
  classified mapped / mapped-with-a-difference / unsupported-with-a-reason.
  Coverage verified programmatically against the inventory (all 280 rows
  present exactly once — see the file's own "Keeping this current"
  section for the regeneration procedure). As of session 1: 25 mapped, 22
  mapped-diff, 182 unsupported among the top-level options; 1 mapped-diff
  (`msc3861`), 50 unsupported among experimental flags. **As of session 2,
  after the `serve_server_wellknown`/`federation_custom_ca_list`/
  `max_spider_size` corrections below: 26 mapped, 24 mapped-diff, 179
  unsupported among the top-level options** (experimental flags
  unchanged) — the counts in `docs/compat/synapse-config-table.md`'s own
  "Summary" table are the current, live numbers; the ones just given are
  historical, kept for the session-1 record.
- **`hs-compat` translator v0** (Phase 0 deliverable): parses a Synapse
  `homeserver.yaml`, writes every one of the 47 mapped/mapped-diff keys
  (48 counting `experimental_features.msc3861`) onto a native
  `hs_config::Config`, and produces a `TranslationReport` covering every
  key the source file sets. Fails closed on unsupported/unrecognized keys
  unless `TranslateOptions::allow_unsupported` is set (the
  `--allow-unsupported-synapse-config` flag `hs-cli` should implement per
  `docs/compat/cli-shims.md`). Tested over a corpus at
  `crates/hs-compat/testdata/`: `minimal.yaml`, `docker.yaml` (Docker
  generator style), `ansible.yaml` (Ansible-role style), `helm.yaml`
  (ESS-helm style), `nixos.yaml` (NixOS module style), and
  `kitchen-sink.yaml` (every mapped/mapped-diff key that can coexist in
  one valid file, with the handful of mutually-exclusive pairs — inline
  vs. `_file` secret forms, `matrix_authentication_service` vs.
  `experimental_features.msc3861` — covered by dedicated unit tests
  instead; see that file's header comment). 35 unit tests + 7 integration
  tests, all passing.
- **The shared-secret registration protocol**
  (`crates/hs-compat/src/shared_secret.rs`): nonce issuance
  (`NonceRegistry`, single-use, 60s TTL matching Synapse's own
  `NONCE_TIMEOUT`) and HMAC-SHA1 MAC computation/verification
  (constant-time via `hmac::Mac::verify_slice`), byte-for-byte compatible
  with Synapse's `register_new_matrix_user` protocol
  (`refs/synapse/docs/admin_api/register_api.md`). Tested against three
  known vectors computed independently with `openssl sha1 -hmac` (see the
  module's test comments for the exact commands), plus replay/expiry/
  tampering tests. 12 tests, all passing.
- **`docs/compat/cli-shims.md`**: the CLI shim specification for
  `hs-cli` (`hs serve --synapse-config`, `hs register`,
  `hs hash-password`, `hs generate-signing-key`, the Docker entrypoint's
  `SYNAPSE_*` variable handling, and what's deliberately not provided and
  why).
- **`docs/compat/synapse-importer-mapping.md`**: every Synapse storage
  table family relevant to import (users, password hashes, devices,
  tokens, keys, backups, account data, push rules, pushers, filters,
  profiles, 3PIDs, directory, appservice state, signing keys, events,
  state, receipts, media) mapped onto this project's model, read from
  `refs/synapse/synapse/storage/schema/{main,state}/full_schemas/72/` for
  exact table/column names (structure only, per the brief; Synapse is
  AGPL-3.0 and was not copied from). Copy order and transaction
  boundaries (5 stages), per-family incremental watermarks, and the full
  state cross-check procedure (`PLAN.md` 9.4 step 2's mandatory
  requirement) are specified.
- **`docs/compat/synapse-admin-routes.md`**: all 77 `/_synapse/admin`
  routes from `docs/synapse-inventory.md`, mapped onto track 15's native
  `/api/v1` resources (read from `crates/hs-admin/openapi/openapi.yaml`,
  which existed and had real content by the time this was written — see
  "Interfaces needed" below for the caveat). Coverage verified
  programmatically (all 77 present exactly once). 41 mapped, 29
  mapped-diff, 7 unsupported (background updates ×3, account validity,
  the E2EE UIA-bypass route, the live federation event re-fetch tool, and
  `/register` itself, which is handled by the shared-secret protocol
  above rather than forwarded to a native resource).

## In progress

Nothing left mid-way; see Next for what Phase 0 still needs.

## Next

- Mount `crate::admin_proxy::router` in `hs-cli` (not this track's file to edit) — see
  "Interfaces provided" for the exact call. Consider also implementing the write-side
  (`PUT`/`POST`/`PATCH`) counterparts to this session's five read-only admin routes, and more of
  `docs/compat/synapse-admin-routes.md`'s remaining "mapped"/"mapped (diff)" rows the same way
  (same in-process-forwarding technique, no new design needed).
- Wire the `v3` variant of `/_synapse/admin/v3/users` (different `deactivated`-filter semantics,
  see `refs/synapse/docs/admin_api/user_admin_api.md`'s "List Accounts (V3)") if a real client
  needs it — deliberately not done this session to avoid asserting exact `v3` compatibility
  without having actually implemented its different filter behavior.
- The importer's read-only extraction prototype against a Synapse 1.161
  fixture database (Phase 0 deliverable, joint with track 14 for the
  fixture). Blocked on nothing technical; needs a Synapse fixture DB,
  which is track 14's to produce per the brief ("day-one work" for 13
  lists this as importer *design*, which is now done in
  `docs/compat/synapse-importer-mapping.md`; the *prototype* is a Phase 0
  item, not day-one).
- `/_synapse/client/*` pages (password reset, SSO pages, consent,
  unsubscribe) and the Synapse metric-name exporter: Phase 1 items per the
  brief, not blocking anything today.
- The migration runbook and rehearsal tooling (joint with 14).
- Keep `tools/synapse_inventory.py` / `docs/synapse-inventory.md` current
  as the pinned Synapse release moves; re-diff
  `docs/compat/synapse-config-table.md` and
  `docs/compat/synapse-admin-routes.md` against it each time (both files'
  own text says exactly what to re-check).
- Several `hs-config` sections are intentionally thin in Phase 0 and will
  grow their own fields as the owning track builds the feature — every
  such gap is named individually in `docs/compat/synapse-config-table.md`
  under reason code `R-PHASE1`, so there is no separate TODO list to
  maintain here; that table *is* the TODO list, per-Synapse-option.

## Blockers

None.

## Interfaces provided

- `hs-config`'s `Config` type, `Config::load`/`from_yaml`/`from_value`,
  `ConfigError`, the `Validate` trait, and `hs_config::reload` — frozen
  per the week-4 seam in `docs/workstreams/README.md`. Any track adding a
  config need should add a field to the relevant section in
  `crates/hs-config/src` (this track reviews) rather than inventing a
  parallel config file.
- `hs_compat::shared_secret` (nonce + MAC) for 07 to wire into the actual
  `/_synapse/admin/v1/register` HTTP handler once `hs-http`/`hs-auth`
  exist to host it.
- `hs_compat::translate::translate` for `hs-cli` to call from
  `hs serve --synapse-config` (see `docs/compat/cli-shims.md`).
- **New this session**: `hs_compat::admin_proxy::{AdminProxyState, router}` (also re-exported at
  the crate root as `hs_compat::{AdminProxyState, admin_proxy_router}`) — a first slice of
  `/_synapse/admin` routes forwarded onto the native `/api/v1` admin router. Mounting instructions
  for `hs-cli` (not this track's file to edit):
  1. Build the native admin router as `hs serve` already must
     (`let (native_admin_router, _manifest) = hs_admin::router::build_router(admin_state);`).
  2. `let synapse_admin = hs_compat::admin_proxy_router(hs_compat::AdminProxyState::new(native_admin_router.clone()));`
     (clone because `hs-cli` still needs the original to mount at `/api/v1` itself).
  3. Merge `synapse_admin` into whatever router already serves
     `hs_auth::synapse_admin_router()`'s `/_synapse/admin/v1/register` (`crates/hs-cli/src/
     auth_manifest.rs`, checked) — e.g. `existing_synapse_admin_router.merge(synapse_admin)` — so
     all of `/_synapse/admin/*` lives under one merged router the same way `/api/v1/*` does.
  This closes five rows of `docs/compat/synapse-admin-routes.md` from "mapped (diff), not yet
  implemented" to actually working: `GET /_synapse/admin/v1/server_version`, `GET
  /_synapse/admin/v2/users`, `GET /_synapse/admin/v2/users/{user_id}`, `GET
  /_synapse/admin/v1/rooms`, `GET /_synapse/admin/v1/rooms/{room_id}`.
- **New this session**: three fields on `hs_config::MediaConfig` — `url_preview_timeout`,
  `url_preview_max_fetch_size`, `url_preview_cache_lifetime` — see the Session 2 section above for
  defaults, the exact Synapse source lines they were checked against, and the one-line consumer
  change `hs-media` (track 09) needs in `crates/hs-media/src/preview.rs`.

## Interfaces needed

- **15 (Admin API)**: `crates/hs-admin/openapi/openapi.yaml` had
  substantial real content (77+ `/api/v1` paths covering users, rooms,
  media, federation, reports, tasks, etc.) by the time
  `docs/compat/synapse-admin-routes.md` was written, but track 15's own
  status file still says "not usable until this section says so" as of
  its last update. This track's admin-route mapping was written against
  that file's *current* content; if 15 materially renames or restructures
  resources before declaring it frozen, `docs/compat/synapse-admin-routes.md`'s
  `Native` column needs a re-check (grep both files for the routes it
  cites — the doc says this too). **This session's `admin_proxy` module makes the same caveat
  concrete**: it depends only on the *JSON contract* of `GET /api/v1/{server,users,users/{id},
  rooms,rooms/{id}}` (checked live against `crates/hs-admin/src/router.rs`/`model.rs` this
  session, not against the OpenAPI document), so a track-15 change to any of those five response
  shapes needs a matching update to `crates/hs-compat/src/admin_proxy.rs`'s five `admin_*_to_
  synapse_*` translation functions, or the shim will silently serve stale/`null` fields the next
  time it forwards a request (it will not crash — every field read is `Option`-safe by
  construction — but it will quietly stop matching Synapse's documented shape).
- **09 (Media)**: the one-line `crates/hs-media/src/preview.rs` change named in the Session 2
  section above and in "Interfaces provided", now that the three config fields it was blocked on
  exist.
- **01 (Storage)**: wiring `PostgresStorageConfig::pool_size`/`tls` through
  `hs_kv::postgres_backend::PostgresBackend::open`'s actual connection-pool/TLS setup (see the
  Session 2 section above — the config fields already exist and are not this track's gap to
  close).
- **12 (Platform, via hs-cli)**: the same crate's `crates/hs-cli/src/storage.rs` needs the
  matching call-site change once 01 adds real `pool_size`/`tls` support, and, separately, a
  `PostgresStorageConfig::schema` field (drafted and reverted this session — see "Decisions made")
  should be added by whichever session next touches *both* `hs-config`'s `PostgresStorageConfig`
  and `hs-cli`'s two literal constructions of it in the same change, since neither crate can safely
  add the field alone.
- **07 (Auth)**: the actual HTTP handler for
  `/_synapse/admin/v1/register` (this track supplies the verified
  nonce/MAC library; 07 owns the account-creation call it wraps) and
  `RequesterContext`/token-verifier plumbing more generally, whenever the
  translator's output needs to flow into a running server rather than
  just a `Config` value.
- **09 (Media)**: the media layout adapter for lazy media import
  mentioned in `docs/compat/synapse-importer-mapping.md`'s Media section
  — this track specified the policy (lazy mount vs. eager copy), 09 owns
  the adapter that makes Synapse's on-disk layout readable through
  `object_store`.
- **14 (Test/conformance)**: the Synapse 1.161 fixture database for the
  importer prototype (Next, above), and the `hs-testkit` differential
  harness the cutover procedure's verification step
  (`docs/compat/synapse-importer-mapping.md`) will run against.
- **12 (Platform)**: `hs-cli` itself (not owned by this track) to
  implement `docs/compat/cli-shims.md` against the libraries this track
  ships; the Docker entrypoint script to honor the `SYNAPSE_*` variable
  table in that same document.

## Decisions made

- **`hs-config` layering and secret resolution**: config loading is
  file → `HS__` env overrides → typed deserialize → secret-file resolution
  → validation, in that order (`Config::load`/`from_value`). A field and
  its `*_file` sibling are mutually exclusive (`ConfigError::SecretConflict`),
  not "file wins" or "inline wins" — an operator who set both almost
  certainly made a mistake, and guessing which one they meant is worse
  than telling them.
- **Reload boundary is section-granular, not field-granular**: simpler to
  reason about and to test (`hs_config::reload::sections_requiring_restart`)
  than per-field reloadability, at the cost of `cluster.mesh.shared_secret`
  (say) not being independently reloadable from `cluster.room_shards` even
  though only one of them structurally requires a restart. Revisit if a
  real operator need for finer granularity shows up.
- **`RateLimitBucket`'s two fields are not individually
  `serde(default)`-ed**, unlike almost everything else in the schema,
  because each named bucket (`message`, `login`, ...) has its own default
  values and a per-field default function has no way to know which bucket
  it's filling in. Documented on the type itself
  (`crates/hs-config/src/ratelimit.rs`) with the practical consequence:
  an `HS__` override of one field of a bucket needs that bucket already
  spelled out in the base config file.
- **Unsupported-key detection in the translator is presence-based, not
  default-value-based**: a key present in the source file with any value
  — including a value that happens to equal what Synapse's own default
  would be — is treated as a decision the operator made and must
  acknowledge, because reproducing Synapse's default for every one of 182
  unsupported options and keeping that in sync release over release is
  infeasible. Documented in `crates/hs-compat/src/translate.rs`'s module
  doc comment.
- **Two-pass translation** (`tls_certificate_path`/`tls_private_key_path`
  and `enable_media_repo` are applied before the general per-key pass,
  since `listeners` needs the TLS paths and the media-repo toggle needs
  the listeners already built) rather than relying on source-file key
  order, which YAML mappings preserve but which no operator should have
  to think about when writing a config.
- **`/register` (shared-secret registration) is the one Synapse admin
  route not forwarded to a native `/api/v1` resource**: it uses a
  fundamentally different auth mechanism (nonce+HMAC, not a bearer token),
  so it stays inside `hs-compat` end to end rather than being translated
  into a call against the native `POST /users`. Recorded as reason code
  `R-COMPAT-PROTOCOL` in `docs/compat/synapse-admin-routes.md`.
- **`ByteSize`/`Duration` parsing accepts Synapse's own string forms**
  (`"50M"`, `"30s"`) by construction (inherited from the previous
  session's `crates/hs-config/src/{size,duration}.rs}`, extended here only
  by fixing a bug where an all-digit string too large for `u64` reported
  `Syntax` instead of `Overflow` — see that file's `from_str`), which is
  what lets the translator reuse `FromStr` directly on copied Synapse
  values instead of writing a second parser.
- **Pinned Synapse version**: 1.161.0, schema version 94, matching
  `docs/synapse-inventory.md`'s header. Not yet formalized as a
  `docs/decisions/*.md` entry (nothing forced the question yet); worth
  doing before the pinned-version policy the brief calls for is exercised
  by an actual Synapse release bump.
- **(Session 2) `PostgresStorageConfig::schema` was drafted, then reverted, rather than shipped.**
  `hs_kv::postgres_backend::PostgresBackend::open(dsn, schema)` already takes a schema parameter
  that `crates/hs-cli/src/storage.rs` hardcodes to `"public"`, so adding a config field for it
  looked like a clean, small win. It is not safe to do from `hs-config` alone: `
  PostgresStorageConfig` is built as a full struct literal (not `..Default::default()`) in two
  places — `crates/hs-compat/src/translate.rs` (this track's own file, fixed in the same change)
  and `crates/hs-cli/src/storage.rs`'s test module (a crate this track cannot edit) — so a new
  required field there breaks `hs-cli`'s build out from under it with no way for this track to fix
  the break. Verified this would actually happen (added the field, ran `cargo check -p hs-cli`,
  watched it fail on the missing field in that test helper, reverted). The field is real future
  work, but only as a change that touches `hs-config` and `hs-cli` together — see "Interfaces
  needed" above. General lesson recorded here for the next field addition to any struct another
  track constructs by full literal: `grep` for `StructName {` across the whole workspace *before*
  adding a required field, not just within this track's own crates, and prefer `#[serde(default)]`
  is not a Rust-struct-literal escape hatch — it only helps deserialization, not `T { a, b, c }`
  call sites.
- **(Session 2) The `serve_server_wellknown` translation derives `well_known_server` from
  `server_name` rather than leaving the key acknowledged-but-untranslated.** The alternative
  (matching the previous state) was to leave `Mapped (diff)`'s promise of "a matching translation
  function exists" unfulfilled for this one key. Since Synapse's own derivation is fully
  mechanical and already checked against `refs/synapse/synapse/rest/well_known.py`, implementing
  it properly was preferred over either downgrading the classification (which would have been
  factually wrong — a native field genuinely exists and a correct translation is possible) or
  leaving a silent gap between the table's claim and the translator's behavior.
- **(Session 2) `hs_compat::admin_proxy` forwards in-process via `tower::Service::call` on
  `hs-admin`'s already-built `axum::Router`, and deliberately does not depend on the `hs-admin`
  crate.** The alternative designs considered: (a) a real HTTP reverse proxy (a second TCP hop,
  needing the native API's bind address threaded through as new config — rejected as needless
  complexity and a new failure mode for a same-process call); (b) depending on `hs-admin`'s
  `AdminState`/model types directly and calling handler functions in Rust (rejected because
  constructing a real `AdminState` needs the same `UserDirectory`/`RoomDirectory`/`TokenVerifier`
  wiring `hs-cli` already has to do once, so this module would need that entire dependency graph
  threaded through it for no benefit over just handing it the finished router). The chosen design
  needs exactly one thing from `hs-cli`: the `axum::Router` `build_router` already returns.
- **(Session 2) Every JSON field this session's admin shims read from a native response is read
  through `Value::get`/`.unwrap_or(...)`, never indexed or unwrapped.** A native response shape
  changing (an unlikely but real risk while track 15 is still actively developing the same
  handlers — see "Interfaces needed") degrades to a `null`/default field in the Synapse-shaped
  response, never a panic. Chosen over strict deserialization into typed structs (which would
  require depending on `hs-admin`'s types, the thing this design otherwise avoids) precisely
  because a compat shim staying up with degraded fidelity is a better failure mode than a compat
  shim that 500s an admin tool's entire session over one renamed field.

## Shared dependencies added

**Session 2**: `hs-compat`'s own `Cargo.toml` adds `axum`, `tower`, `http-body-util` and `time` as
ordinary dependencies, and `tokio` as a dev-dependency — all four already present in the root
`[workspace.dependencies]` (used throughout the rest of the workspace; `axum`/`tower` for
`admin_proxy`'s in-process request forwarding, `time` for RFC 3339 timestamp parsing in the same
module, `tokio` only for that module's `#[tokio::test]`s). No new entries were added to
`[workspace.dependencies]` itself. `hs-config` added no new dependencies this session.

## How to verify everything in this file

```
cargo fmt -p hs-config -p hs-compat -- --check
cargo clippy -p hs-config -p hs-compat --all-targets -- -D warnings
cargo test -p hs-config -p hs-compat
cargo run -p hs-config --bin gen_config_docs   # regenerates docs/config.md; git diff should be empty if it's current
cargo check --workspace                        # confirms this session's hs-config changes did not
                                                # break any other track's crate (checked this
                                                # session — see the reverted `schema` field above
                                                # for why this check matters, not just a formality)
```

All green as of this writing: hs-config 81 tests (77 lib — including 4 new in `media.rs`'s
`url_preview_*` tests, for 8 total in that module — plus 3 `gen_config_docs` bin tests plus 1
doctest); hs-compat 57 tests (50 lib — including 9 new in the new `admin_proxy` module, 7 of them
`#[tokio::test]` async route tests and 2 synchronous unit tests, plus 6 new in `translate` for the
`serve_server_wellknown`/`federation_custom_ca_list`/`max_spider_size` translation arms — plus 7
corpus integration tests, unchanged). No clippy warnings, `cargo fmt` clean, `cargo check
--workspace` clean (verified this session, not assumed — see the reverted `PostgresStorageConfig::
schema` field in "Decisions made" for why that check earned its place here).
