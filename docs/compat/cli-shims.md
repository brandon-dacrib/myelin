# CLI shim specification

Track 13 (`docs/workstreams/13-config-compat-and-migration.md`), per
`PLAN.md` section 9.1. This is a specification for `hs-cli` (owned by
track 12/the platform track — see `PLAN.md` 5.5's crate table; `hs-cli` is
not one of this track's crates) to implement against the library functions
this track ships in `hs-compat` and `hs-config`. Nothing here is a
description of code that lives in `hs-compat` itself beyond the two
libraries it names explicitly (the translator and the shared-secret
protocol); the shims' argument parsing, process exit codes and stdout/stderr
formatting are `hs-cli`'s to build.

## Summary table (`PLAN.md` 9.1)

| Synapse | Here | Status |
|---|---|---|
| `synapse_homeserver -c homeserver.yaml` | `hs serve --synapse-config homeserver.yaml` | Specified below. Library: `hs_compat::translate::translate`. |
| `register_new_matrix_user` | `hs register` (or a `register_new_matrix_user` symlink/alias — see below) | Specified below. Library: `hs_compat::shared_secret`. |
| `hash_password` | `hs hash-password` | Specified below. No new library code needed (bcrypt hashing is `hs-auth`'s, not this track's). |
| `generate_signing_key` | `hs generate-signing-key` | Specified below. No new library code needed (Ed25519 key generation is `hs-auth`'s or a shared `hs-model` helper, not this track's). |
| `synapse_worker`, `synctl`, `synapse_port_db`, `update_synapse_database`, manhole | Not provided | `PLAN.md` D2/9.1: single-process replicas replace workers; `synctl` supervises workers that don't exist here; `synapse_port_db` has no target (there is one importer, not a schema-preserving port); `update_synapse_database` is a Synapse-schema-specific offline maintenance tool; the manhole is a Python REPL (R-PROC in `docs/compat/synapse-config-table.md`). Worker config files are still *read* by the translator to collapse them into one process's listener/resource config, never executed as separate processes. |
| Docker `SYNAPSE_SERVER_NAME`, `SYNAPSE_CONFIG_PATH`, `SYNAPSE_REPORT_STATS`, `SYNAPSE_WORKER_TYPES`, ... | Honored by the image entrypoint | Specified below (joint with track 12, who owns the entrypoint script and image). |

## `hs serve --synapse-config <path>`

Runs a replica whose configuration comes from translating a Synapse
`homeserver.yaml` on the fly, rather than a native `hs-config` YAML file.

**Behavior:**

1. Read the file at `<path>`.
2. Call `hs_compat::translate::translate(&contents, TranslateOptions {
   allow_unsupported: <the --allow-unsupported-synapse-config flag> })`.
3. On `Err(TranslateError::Unsupported { keys })`: print each blocking key
   and its reason (already formatted, one per line, by `TranslateError`'s
   `Display`) to stderr, print a one-line pointer to
   `docs/compat/synapse-config-table.md` for the full classification, and
   exit non-zero **without starting the server**. This is `PLAN.md` 9.2's
   "nothing is silently ignored" made concrete at the CLI boundary.
4. On `Err(TranslateError::Yaml(_))` or `Err(TranslateError::Config(_))`:
   print the error and exit non-zero, same as a native config that fails
   to parse or validate — the translator's output is a `hs_config::Config`
   like any other, so from this point on there is no difference between
   `--synapse-config` and a native `-c`.
5. On success: print the **translation report**
   (`TranslationReport::to_markdown`, or a `--translation-report json`
   flag emitting the structured form) to stderr or a `--translation-report-out
   <path>` file — every key from the source file, its classification, and
   what it became — then proceed exactly as `hs serve -c <native.yaml>`
   would with the resulting `Config`, including environment overrides
   (`HS__...` variables still apply on top of the translated config, same
   as any other load path) and hot-reload behavior for the reloadable
   sections (`hs_config::reload`).
6. `--allow-unsupported-synapse-config` is required whenever the source
   sets a key `docs/compat/synapse-config-table.md` classifies
   `Unsupported`, or an unrecognized key not in that table at all
   (a typo, or an option from a Synapse release newer than the pinned
   inventory). Without it, step 3 applies. With it, translation proceeds
   and every blocking key still appears in the report, now as a warning
   rather than a hard stop — an operator reading server logs after
   passing the flag once should still see exactly what was dropped.

**Printing the report always, even on the happy path**, matters: an
operator running this for the first time on a real `homeserver.yaml`
needs to see every `Mapped (diff)` key too, not just failures — those are
the ones where behavior might differ subtly even though nothing blocked
startup.

## `hs register` (shared-secret registration; `register_new_matrix_user` compatible)

`register_new_matrix_user` is a script bundled with Synapse that talks to
`GET`/`POST /_synapse/admin/v1/register` (`refs/synapse/docs/admin_api/register_api.md`).
This server serves that exact route (`docs/compat/synapse-admin-routes.md`:
`R-COMPAT-PROTOCOL`, handled inside `hs-compat`, not forwarded to the
native `/api/v1` resources) using `hs_compat::shared_secret`. The CLI shim
is a thin HTTP client against that route, so it works unmodified whether
it's talking to Synapse or to `hs serve`.

**Flags** (matching Synapse's script, so existing invocations and
provisioning scripts do not need to change):

```
hs register [-u USERNAME] [-p PASSWORD] [-a] [-c CONFIG | -k SHARED_SECRET]
             [--user-type USER_TYPE] SERVER_URL
```

- `-u/--user`, `-p/--password`: prompted interactively if omitted (never
  echoed; same as Synapse's script).
- `-a/--admin` / `--no-admin`: sets the `admin` flag in the MAC input.
- `-c/--config`: a *native* `hs-config` YAML or a Synapse `homeserver.yaml`
  (detected the same way `hs serve` would — a `server_name` key present
  either way) to read `auth.registration_shared_secret` /
  `registration_shared_secret` from, for offline/local use against a config
  file rather than prompting for `-k`.
- `-k/--shared-secret`: the secret directly, for scripted use.
- `--user-type`: optional, forwarded into the MAC per
  `hs_compat::shared_secret::compute_mac`'s `user_type` parameter.

**Protocol** (library calls, no reimplementation in `hs-cli` beyond HTTP
plumbing):

1. `GET {SERVER_URL}/_synapse/admin/v1/register` → `{"nonce": "..."}`.
2. `hs_compat::shared_secret::compute_mac(secret.as_bytes(), &nonce, &username, &password, admin, user_type.as_deref())` → hex MAC.
3. `POST {SERVER_URL}/_synapse/admin/v1/register` with `{nonce, username,
   password, admin, mac, user_type?}` → `{access_token, user_id,
   home_server, device_id}` on success, or the server's error JSON
   (`M_UNKNOWN`/`400` for a bad or reused nonce, matching
   `hs_compat::shared_secret::NonceError`'s two cases; `403`/`M_FORBIDDEN`
   for a MAC mismatch, matching `MacVerifyError::Mismatch`) on failure.
4. Print the resulting `user_id` (and, with `-v/--verbose` matching
   Synapse's script, the `access_token` and `device_id`) to stdout.

The server-side handler (owned jointly by this track and 07 per the brief,
served through `hs-compat`) is:

```rust
// Sketch — the real handler lives in hs-compat's HTTP layer, not this doc.
fn handle_get_register(registry: &mut NonceRegistry) -> Nonce {
    Nonce { nonce: registry.issue() }
}

fn handle_post_register(
    registry: &mut NonceRegistry,
    secret: &[u8],
    req: RegistrationRequest,
) -> Result<RegisteredUser, RegistrationError> {
    hs_compat::shared_secret::verify_registration_request(registry, secret, &req)?;
    // ... create the account via hs-auth, exactly as a normal POST /users would ...
}
```

`verify_registration_request` (`crates/hs-compat/src/shared_secret.rs`)
already handles nonce replay/expiry and constant-time MAC verification;
the handler's only job is turning a verified request into an account.
This endpoint is disabled (returns `404`, matching Synapse's own behavior
in MAS-delegation mode) whenever `auth.mas_delegation` is set — shared-secret
registration and MAS delegation are mutually exclusive for the same reason
`hs-config` itself refuses to combine `auth.mas_delegation` with
`auth.oidc_providers` (`crates/hs-config/src/auth.rs`).

## `hs hash-password`

Synapse's `hash_password` script bcrypt-hashes a password (optionally
peppered, `bcrypt_rounds`-costed) the same way the server would at
registration time, for provisioning scripts that write directly into a
password field rather than calling `/register`.

**Flags:** `hs hash-password [-p PASSWORD] [-c CONFIG]` — password from
stdin or a prompt if omitted (never as a bare CLI argument, which would
leak it into shell history and `ps`); `-c` reads `auth.password.pepper`
(native) or `password_config.pepper` (Synapse, via the same
`homeserver.yaml`-or-native detection as `hs register -c`) the same way
the server would when verifying a login.

**Output:** the bcrypt hash on stdout, nothing else — scriptable
(`hs hash-password -p hunter2 -c homeserver.yaml >> seed.sql`-style usage).

This shim calls into whatever `hs-auth` exposes for password hashing
(a plain function, not an HTTP round trip); it is listed here because it is
part of the CLI surface `PLAN.md` 9.1 names, not because this track
implements the hashing itself.

## `hs generate-signing-key`

Synapse's `generate_signing_key` writes a new Ed25519 signing key file in
Synapse's `signing.key` text format (`<algorithm> <key_id>
<base64-private-key>`, one line). This server accepts the same file format
at `server.signing_key_path` when it names a single-file path rather than
a directory (see the `Mapped (diff)` note on `signing_key_path` in
`docs/compat/synapse-config-table.md` — a directory of such files is also
accepted, for multi-key rotation).

**Flags:** `hs generate-signing-key [-o OUTPUT_PATH]` — defaults to
stdout, matching Synapse's script, so `hs generate-signing-key >
homeserver.signing.key` and `hs generate-signing-key -o
/etc/hs/signing-keys/$(date +%s).key` both work.

**Output format:** identical to Synapse's, so a key generated by either
tool is a drop-in read for the other (useful for the rehearsal/rollback
path: Synapse never has its database written to, and if an operator rolls
back per `PLAN.md` 9.4 step 5, Synapse's own signing key file — untouched
throughout — is exactly what Synapse resumes with).

## Docker entrypoint environment variables

Joint with track 12 (who owns the image and entrypoint script); this
track specifies which `SYNAPSE_*` variables are honored and what they
translate to, since that mapping is this track's config-translation
expertise:

| Variable | Effect |
|---|---|
| `SYNAPSE_SERVER_NAME` | If no config file is mounted, generates a minimal native config (`server.server_name` set from this, `storage` defaulting to `Embedded`) — the moral equivalent of Synapse's own `generate-config` on first run. Not used if `SYNAPSE_CONFIG_PATH` points at an existing file. |
| `SYNAPSE_CONFIG_PATH` | Path to either a native `hs-config` YAML or a Synapse `homeserver.yaml`; the entrypoint runs `hs serve --synapse-config` when the latter is detected (see `hs serve --synapse-config` above for detection), `hs serve -c` otherwise. |
| `SYNAPSE_REPORT_STATS` | Maps to `server.report_stats` (`yes`/`no` → `true`/`false`), same value Synapse itself would set from this variable when generating a fresh config. |
| `SYNAPSE_WORKER_TYPES` | **Honored by being ignored, with a notice** (`PLAN.md` 9.1): the entrypoint logs one line noting that worker types have no effect here (R-WORKER, `docs/compat/synapse-config-table.md`) and starts a single replica regardless of the value, rather than failing — an operator's existing Helm chart or Compose file that sets this alongside other, honored variables should not be blocked by the one setting that no longer means anything. |
| `SYNAPSE_LOG_LEVEL`, `SYNAPSE_LOG_SENSITIVE`, `UID`/`GID` | Track 12's own container conventions; not a config-translation concern, listed here only so this document is the complete `SYNAPSE_*` inventory in one place. |

## Not provided, and why

`synapse_worker`: no per-worker process type exists (`PLAN.md` D2).
`synctl`: supervises worker processes that don't exist; container/Kubernetes
lifecycle management replaces it (`PLAN.md` D11, section 7.2). `synapse_port_db`:
migrates a Synapse SQLite database to Postgres, both *within* Synapse's own
schema; this project has one importer (`hs import synapse`) with a
different source-to-target relationship entirely, so there's no matching
tool, and no "port your SQLite Synapse to Postgres first" step is needed —
`hs import synapse` reads either backend directly. `update_synapse_database`:
Synapse-schema-specific offline maintenance (e.g. rebuilding its search
index) with nothing on this side to maintain the same way. The manhole:
R-PROC — no embedded Python REPL; operational introspection goes through
structured tracing and the admin API instead.
