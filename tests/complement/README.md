# Complement

Layer L3 of `PLAN.md` section 12: Synapse's own black-box Go integration suite
(`refs/complement/`), run against this server's Docker image. **Everything in this directory is
untested** — no `hs-server` binary exists in this workspace yet (the per-track HTTP surfaces,
`hs-http`'s listener and `hs-auth`'s router fragment among them, are library crates with no
assembled listening process), so `build.sh` cannot succeed and nothing here has run against a
real container. This is the scaffold Complement's image contract requires, ready for the `TODO`
markers to be filled in once a server binary exists — see `docs/status/14-test-and-conformance.md`
for what is blocking that.

## Layout

| Path | What |
|---|---|
| `Dockerfile.template` | The Complement image: build stage (`cargo build --release -p hs-server`, a placeholder crate name), runtime stage (the binary plus `openssl`/`curl`, a `HEALTHCHECK`, `EXPOSE 8008 8448`). |
| `startup.sh` | Container entrypoint: signs a federation TLS cert against Complement's mounted CA (`/complement/ca/{ca.crt,ca.key}`), trusts that CA itself, then execs the server. The cert-signing half is real and independently testable once Docker is available (it doesn't need the server binary); the final `exec` line is a placeholder for the real CLI once it exists. |
| `build.sh` | Builds the image; skips cleanly (exit 0) if Docker is unavailable, fails loudly if Docker is available but the build itself fails (a real signal once there's a binary to build). |
| `run_single_node.sh` | The common case: one server process per Complement blueprint. Builds the image, applies `blacklist.txt` as a `go test -skip` regex, runs `go test ./tests/...` from `refs/matrix-spec`'s sibling checkout `refs/complement`. |
| `run_cluster.sh` | The seam for track 03/12's cluster deployment topology (sharded rooms, lease handover, multi-container blueprints) — see that script's header for why this is further from working than single-node mode. |
| `skip_regex.sh` | Turns `blacklist.txt` into the `|`-joined regex `run_single_node.sh` passes to `go test -skip`. |
| `blacklist.txt` | This server's Complement blacklist — see below. |

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
