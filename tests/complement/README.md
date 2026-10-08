# Complement

Layer L3 of `PLAN.md` section 12: Synapse's own black-box Go integration suite
(`refs/complement/`), run against this server's Docker image.

**As of 2026-09-18, the image builds and Complement runs against it for real.** See the top of
`docs/status/14-test-and-conformance.md` for the actual pass/fail/skip numbers (two full runs of
`tests/csapi`, 106 top-level tests each), the harness bugs found and fixed to get there, and how
long the image build takes on this machine. The one-command reproduction is:

```bash
./tests/complement/build.sh complement-hs-reimplement:dev
cd ../../refs/complement && COMPLEMENT_BASE_IMAGE=complement-hs-reimplement:dev \
  go test -v -timeout 30m ./tests/csapi/...
```

or `./tests/complement/run_single_node.sh` for the wrapper that also applies `blacklist.txt` and
runs the full `./tests/...` package (not yet run to completion this session — see the status
file's "What a next session should do first").

## Layout

| Path | What |
|---|---|
| `Dockerfile.template` | The Complement image: build stage (`cargo build --release --jobs 4 -p hs-cli`, the real `hs` binary), runtime stage (the binary, `stunnel4` terminating TLS on 8448 in front of `hs`'s plaintext 8008, `openssl`/`curl`, a `HEALTHCHECK`, `EXPOSE 8008 8448`). |
| `stunnel.conf.template` | The `stunnel` config `startup.sh` fills in with the per-container-signed cert path. |
| `startup.sh` | Container entrypoint: signs a federation TLS cert against Complement's mounted CA (`/complement/ca/{ca.crt,ca.key}`), trusts that CA itself, writes a native `hs-config` YAML to `/data/config.yaml`, starts `stunnel`, then `exec`s `hs serve -c /data/config.yaml`. |
| `build.sh` | Streams a `tar` of the repo (excluding `target/`, `.git/`, `.claude/`, `web/node_modules/`, `refs/` and other build-irrelevant, multi-gigabyte directories) to `docker build`'s stdin, rather than using `.` as the build context directly — see the status file for why. Gives each image tag its own BuildKit `target/` cache (see "One image, one cache, one run at a time" below). Skips cleanly (exit 0) if Docker is unavailable, fails loudly if Docker is available but the build itself fails. |
| `run_single_node.sh` | The common case: one server process per Complement blueprint. Builds the image, applies `blacklist.txt` as a `go test -skip` regex, runs `go test ./tests/...` from `refs/matrix-spec`'s sibling checkout `refs/complement`, under `lock.sh`. |
| `lock.sh` | Runs a command while holding the repository's Complement lock, so runs from different worktrees or agents on one Docker daemon take turns. |
| `run_cluster.sh` | The seam for track 03/12's cluster deployment topology (sharded rooms, lease handover, multi-container blueprints) — see that script's header for why this is further from working than single-node mode. |
| `skip_regex.sh` | Turns `blacklist.txt` into the `|`-joined regex `run_single_node.sh` passes to `go test -skip`. |
| `blacklist.txt` | This server's Complement blacklist — see below. |
| `patches/`, `apply_patches.sh` | Harness fixes to races in upstream tests, applied idempotently to a Complement checkout — see "Patches to upstream tests" below. |

## Image contract (from `refs/complement/README.md`, "Image requirements")

- `EXPOSE 8008` (plain HTTP, client-server + appservice traffic) and `EXPOSE 8448` (HTTPS,
  federation traffic).
- Accept the server name via the `SERVER_NAME` environment variable at container start (not at
  build time — the same image is reused for every blueprint's differently-named homeservers).
- `GET /_matrix/client/versions` returns `200 OK` once healthy; a `HEALTHCHECK` should reflect
  that.
- Trust and sign against `/complement/ca/ca.crt` / `/complement/ca/ca.key` for the federation TLS
  certificate (`startup.sh` does this with the exact `openssl` recipe the upstream README
  documents).
- Use `complement` as the registration shared secret for a Synapse-compatible
  `/_synapse/admin/v1/register` endpoint, once one exists (`docs/rfcs/0004-admin-api.md`'s
  Synapse compatibility surface); Complement skips those tests if the endpoint 404s, so this is
  not blocking before that endpoint lands.
- Manage its own storage inside the container — the embedded `hs-kv` backend (`PLAN.md` section 7)
  is exactly the single-binary mode this wants; no external Postgres is needed for single-node
  Complement runs.
- Tolerate `CMD`/`ENTRYPOINT` being invoked more than once per container lifetime (Complement may
  restart a container within a test run).

## Running once a server binary exists

```bash
# from the repository root
./tests/complement/run_single_node.sh
# or, to pass extra `go test` flags:
./tests/complement/run_single_node.sh -- -run 'TestRegistration'
```

Requires Docker (running, not just installed), Go, and `refs/complement` (`tools/fetch-refs.sh`,
network required). Every script in this directory checks for these and exits 0 with an
explanation rather than failing if one is missing, per this track's brief ("design tests so the
Docker-dependent ones are skipped cleanly when it is absent").

`COMPLEMENT_BASE_IMAGE` and `COMPLEMENT_DIR` can be overridden; see `run_single_node.sh`.

## One image, one cache, one run at a time

Several agents share the desktop's one Docker daemon. Two lessons of 2026-10-04/05:

- **Two `go test` runs of one Complement package at once break each other**: they share
  container and network names. `run_single_node.sh` runs `go test` under `lock.sh`; a run by hand
  should too:

  ```bash
  tests/complement/lock.sh go test -v -run 'TestRestrictedRooms' ./tests/...   # in the checkout
  ```

  The lock is the directory `.git/myelin-complement.lock` in the repository's common git
  directory, so every worktree shares it; `owner` in it says who holds it (PID, start time,
  working directory, command), and a waiting run prints that once. The lock is released however
  the command ends; INT, TERM and HUP are passed on to it. A lock whose owner PID is gone (a
  `kill -9`, a reboot) is taken over, and that is logged. `COMPLEMENT_LOCK_DIR` and
  `COMPLEMENT_LOCK_POLL` (seconds, default 20) override the defaults.
- **A BuildKit `target/` cache shared by every branch's image once linked another branch's crate
  into an image.** `build.sh` now gives each image tag its own cache,
  `myelin-complement-target-<tag>`, so build one tag per branch
  (`./tests/complement/build.sh complement-hs-reimplement:<agent name>`, and run with
  `COMPLEMENT_BASE_IMAGE` set to it). The first build into a new cache is a cold release build
  (about 20 minutes here). `--shared-cache` uses the old shared cache (`myelin-complement-target`),
  right only when one branch builds at a time; `TARGET_CACHE_ID=<id>` names any other;
  `--dry-run` prints the tag, cache id and command. Each cache holds a few GB: list them with
  `docker buildx du --verbose | grep -B6 myelin-complement-target` and remove a finished
  branch's with `docker buildx prune -f --filter id=<ID>`.

## Blacklist management

`blacklist.txt` holds one Go test-name regex per line (matched with `go test -skip`), rather than
Complement's per-implementation build-tag convention (`synapse_blacklist`, `dendrite_blacklist`,
...) — those tags are registered upstream, in Complement's own test files, for the four
implementations it already knows about; a runtime `-skip` regex needs no upstream changes and
works for a server Complement has never heard of. `skip_regex.sh` turns the file into the regex
`run_single_node.sh` passes through.

It starts empty (see the file for why) — once a real run exists, add one line per test that fails
for a *known, understood* reason (not yet implemented, deliberately different behavior with an
RFC explaining why, a Complement bug), with a `#` comment naming that reason. Palpo's self-reported
"672 pass, 0 fail, 14 skip" (`refs/palpo/tests/complement/` and its results file) is the bar this
track's brief sets for a new Rust server; an empty or ever-growing blacklist without comments
explaining each entry does not meet it.

## Patches to upstream tests

A few upstream tests assert on one homeserver what has only been guaranteed on another, and
lose that race on a loaded host whatever the server does. Rather than blacklist them (and stop
measuring the behaviour they test), `patches/` holds a fix per race that waits for the
precondition the assertion depends on; no patch relaxes an assertion. `apply_patches.sh
[<complement dir>]` applies them idempotently (an applied patch is left alone, a stale one is an
error naming it). `tools/fetch-refs.sh` runs it after cloning `refs/complement`, and
`run_single_node.sh` before every run; when running `go test` by hand in a checkout made some
other way, run it once first:

```bash
tests/complement/apply_patches.sh refs/complement
```

| Patch | Tests | What it waits for |
|---|---|---|
| `0001-nocreators-wait-for-hs2-power-levels.patch` | `TestRestrictedRoomsLocalJoinNoCreatorsUsesPowerLevelsV11`/`V12`, `TestKnockRestrictedRoomsLocalJoinNoCreatorsUsesPowerLevelsV11`/`V12` | bob, on hs2, syncs until alice's power-levels change (sent on hs1 with `SendEventSynced`, which waits on hs1 only) has reached hs2, before charlie's local join through hs2. Without it, charlie's join was refused 15 ms after hs1 accepted the change; all four failed on `c2d74174`, and pass with the patch (status 14, 2026-10-04 session). |

## Single-node vs cluster mode

- **Single-node** (`run_single_node.sh`): one server process, embedded storage, the default and
  by far most common Complement topology. This is where effort should go first.
- **Cluster** (`run_cluster.sh`): several server processes sharing shard ownership over rooms
  (track 03), which Complement's stock harness does not model directly — it needs a
  multi-container blueprint or compose topology this track does not own. See that script's header
  for the current state (a documented placeholder, not a working implementation) and
  `docs/workstreams/03-cluster.md` / `12-platform-and-kubernetes.md` for who owns the missing
  piece.

## References

- `refs/complement/README.md` — the full image contract and `ENVIRONMENT.md` for every
  `COMPLEMENT_*` variable.
- `refs/synapse/docker/complement/`, `refs/synapse/scripts-dev/complement.sh` — Synapse's own
  Complement image and runner (AGPL-3.0, structure read for reference only, no code copied).
- `refs/palpo/tests/complement/` — a Rust server's Complement rig (Apache-2.0), the closest
  existing analog to what this directory should eventually look like.
