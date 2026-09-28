# Migrating from Synapse: runbook

How to move a Synapse deployment onto Myelin with the Migration page (`/admin/migration`) or the
admin API's Migration area (`/api/v1/migration*`). Nothing on either side is edited by hand:
the source is set through the configuration API, and Synapse's database is only ever read.

What is copied is listed under "What moves", what is not under "What does not move yet". The
table-by-table mapping is `synapse-importer-mapping.md`.

## Before you start

1. **Same server name.** A migration keeps the server name: user ids, room ids and aliases are
   Synapse's. Install Myelin with `server.server_name` set to Synapse's `server_name`. A start
   against a Synapse for another name is refused, and says both names.
2. **Synapse's signing key.** Give Myelin Synapse's signing key so that it keeps signing as the
   server other servers already know: put Synapse's `*.signing.key` file in a directory and set
   `server.signing_key_path` to that directory (a bootstrap setting, decision 0010). Myelin reads
   Synapse's key format as it is.
3. **The password pepper.** If Synapse set `password_config.pepper`, set the same value as
   `auth.password.pepper` in the Configuration page. Password hashes are copied as they are, and
   Synapse's bcrypt hashes verify unchanged.
4. **Synapse on PostgreSQL.** The importer reads PostgreSQL. A Synapse on SQLite is moved to
   PostgreSQL first with Synapse's own `synapse_port_db`.
5. **A role that can read Synapse's database.** A read-only role is enough.
6. **The media store, mounted.** To copy media files, mount Synapse's `media_store_path` into
   the Myelin container or host (read-only is enough). Without it, media records are copied but
   their files are not, and each is logged.
7. **A fresh Myelin.** Migrate into a server that has only its first administrator. An account
   that differs from a Synapse account only by case is refused and logged.

## 1. Point at Synapse

On the Migration page, step 1 "Point at Synapse": host, port, database, user, password, media
store path and rows per batch. Saving writes the `migration` section of the configuration
(`PATCH /api/v1/config/migration` with `{"synapse": {...}}`); the password is stored and only
ever answered as `{"$secret": true}`. From the API:

```sh
curl -X PATCH -H "authorization: Bearer $ADMIN" -H 'content-type: application/json' \
  https://matrix.example.org/api/v1/config/migration -d '{"synapse": {
    "database": {"host": "synapse-db", "port": 5432, "database": "synapse",
                 "user": "synapse_ro", "password": "..."},
    "media_store_path": "/mnt/synapse/media_store", "batch_size": 500}}'
```

## 2. Copy, while Synapse keeps running

"Start copying" (`POST /api/v1/migration/start`, `{}` or
`{"source_secret_ref": "/migration/synapse"}`). The start connects to Synapse and checks the
server name before anything is copied; an unreachable database or a wrong name is refused with a
`400` saying so. The copy then runs as a task (`migration.copy`, on the Tasks page too), in this
order: accounts (with password hashes, flags and profiles), devices, access tokens, account data
and room tags, rooms, media. The page shows each stream's rows copied, not copied on purpose,
failed, and the rate, and an estimate of the time left.

- **Pause / Resume** stop the copy between two batches and carry it on from there
  (`POST /migration/pause`, `/migration/resume`).
- **A restart** of Myelin carries a running copy, verification or cutover on from its last
  checkpoint, logged as such; the task the old process was running is marked interrupted.
- **Abort** (`POST /migration/abort`) abandons the migration. Synapse was only ever read, so
  there is nothing to undo there; what was copied stays, and starting again carries on from it.
- Every row is idempotent: one already here is recognized and counted, never copied twice.

When everything has been copied once the status is `ready_for_cutover`.

## 3. Verify

"Verify" (`POST /api/v1/migration/verify`, `202` and a task) counts every stream in Synapse and
looks each row up here, then compares field by field: every account's sample (password hash,
display name, avatar, administrator, deactivation), each device's name, the account and device
every access token signs in, each piece of account data, every room's events and its current
state against Synapse's `current_state_events`, and media files byte for byte (a sample). The
findings are on the page and in `GET /api/v1/migration` (`verification`). Run it as often as you
like while Synapse is still in service; a difference is a bug report, not something to live with.

## 4. Cut over

1. **Stop Synapse** (every process and worker). Anything written to Synapse after the cutover
   is not carried over.
2. Tick the checklist on the page and **Cut over** (`POST /api/v1/migration/cutover`, `202` and
   a task). The cutover reads everything in Synapse again (whatever changed since the copy is
   brought over; what is unchanged is recognized), verifies, and finishes (`completed`) only if
   verification passes. If it does not, nothing is cut over, the status goes back to
   `ready_for_cutover` with the differences, and Synapse can be started again.
3. **Point clients and other servers at Myelin**: the DNS name or ingress that served Synapse,
   and its `.well-known` delegation.

Signed-in clients keep working: their access tokens were copied. People sign in with the
passwords they had.

## Rolling back

Until the cutover, rolling back is carrying on with Synapse: its database was never written to.
After the cutover, starting Synapse again loses whatever happened on Myelin since, and two
servers must never answer for the same name at once.

## What moves

| Synapse | Here |
|---|---|
| `users`, `profiles` | accounts: bcrypt password hash, administrator, guest, deactivated, locked, shadow-banned, creation time, appservice, display name and avatar |
| `devices` (not hidden ones) | devices, with their names and last-seen |
| `access_tokens` (not an administrator's "login as" tokens) | sessions: the same token strings sign in the same account and device |
| `account_data`, `room_account_data`, `room_tags` | global and per-room account data; tags as `m.tag` |
| `rooms`, `events`/`event_json`, `redactions`, `room_aliases` | each room this server's users created, every event with its original id replayed in order through this server's own authorization, redactions applied, aliases, directory listing |
| `local_media_repository`, `media_store/local_content` | local media under the same `mxc://` ids, with their files |

## What does not move yet

- **End-to-end encryption keys** (device keys, one-time keys, cross-signing keys, key backups):
  clients upload device keys again; key backups have to be restored from the client.
- **Push rules and pushers**: people's custom notification settings go back to the defaults.
- **Read receipts**, filters, presence, and **remote media** (cached again on first use).
- **Rooms joined over federation**: a room whose `m.room.create` is not part of its history here
  (it was created on another server) is skipped and logged; its members rejoin it after cutover.
- **Rejected events and outliers** are left out of each room's history, as Synapse held them
  outside it.
- **Appservice registrations** are not read from the database; list the registration files in
  `appservices.registration_files` (imported once, decision 0010) or add the bridges in the
  Bridges section.

## Watching it

- **Status**: `GET /api/v1/migration`; the page polls it every 1.5 seconds while something runs.
- **Log**: `GET /api/v1/migration/log`, the page's Log: each stream's start and end, each row not
  copied and why, each failure, each room's events stored and refused.
- **Audit log**: `migration.start`, `.pause`, `.resume`, `.abort`, `.verify`, `.cutover`.
- **Events** (`GET /api/v1/events`): `migration.started`, `.paused`, `.resumed`, `.aborted`,
  `.verifying`, `.cutting_over`, `.ready_for_cutover`, `.verified`, `.completed`, `.failed`, and
  `task.changed` for the task running each step.
- **Metrics**: `hs_migration_rows_copied`, `hs_migration_rows_skipped`, `hs_migration_rows_failed`
  and `hs_migration_rows_source`, by `stream`; `hs_migration_status`, 1 for the current status.

## Rehearse it

`crates/hs-compat/tests/fixtures/synapse-small` is a real Synapse 1.161 database (see its
README). To rehearse against it with the real binary:

```sh
docker run --rm -d --name hs-mig-pg -e POSTGRES_PASSWORD=hspg -p 127.0.0.1:5439:5432 postgres:17
cargo test -p hs-cli --test migration          # the whole path, through the admin API
cargo test -p hs-compat --test migration       # the engine: pause, resume, abort, restart
```

and, for the page, load `schema.sql` and `data.sql` into a database, boot `hs serve` named
`fixture.test` with the fixture's `signing.key` in its `signing_key_path` directory, create the
administrator, and run `web/e2e-real/migration.spec.ts` with `HS_REAL_MIGRATION_SOURCE` and
`HS_REAL_MIGRATION_FACTS` (see the spec's header).
