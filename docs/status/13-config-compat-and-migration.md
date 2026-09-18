# 13. Configuration, compatibility and migration: status

Track brief: `docs/workstreams/13-config-compat-and-migration.md`. Owner
crates/files: `crates/hs-config`, `crates/hs-compat`,
`tools/synapse_inventory.py`, `docs/synapse-inventory.md`.

Last updated: 2026-09-18 (day one, session 1).

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
  section for the regeneration procedure). 25 mapped, 22 mapped-diff, 182
  unsupported among the top-level options; 1 mapped-diff (`msc3861`), 50
  unsupported among experimental flags.
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
  cites — the doc says this too).
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

## Shared dependencies added

None. `serde_yaml_ng` and `schemars` were already in
`[workspace.dependencies]` (used by `hs-config`); `hmac`, `sha1`, `hex`
and `rand` were also already present (used by `hs-compat`'s shared-secret
protocol). `hs-compat`'s own `Cargo.toml` adds a path dependency on
`hs-config`, which is this track's own crate, not a new shared dependency.

## How to verify everything in this file

```
cargo fmt -p hs-config -p hs-compat -- --check
cargo clippy -p hs-config -p hs-compat --all-targets -- -D warnings
cargo test -p hs-config -p hs-compat
cargo run -p hs-config --bin gen_config_docs   # regenerates docs/config.md; git diff should be empty if it's current
```

All green as of this writing: 115 tests (69 + 3 + 1 doctest in
`hs-config`; 35 + 7 in `hs-compat`), no clippy warnings, `cargo fmt`
clean.
