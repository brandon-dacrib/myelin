# Sytest

Layer L4 of `PLAN.md` section 12: Sytest, Synapse's older integration suite, run against the real
`hs` binary. It runs in Docker, on Sytest's own image (`matrixdotorg/sytest`, which carries Perl
and every CPAN module Sytest needs), so nothing is installed on the host.

```bash
tests/sytest/build.sh myelin-sytest:dev        # release `hs` on Debian bookworm + Sytest's image
SYTEST_IMAGE_TAG=myelin-sytest:dev tests/sytest/run.sh                    # the whole suite
SYTEST_IMAGE_TAG=myelin-sytest:dev tests/sytest/run.sh tests/11register.pl  # one file
```

The whole suite takes 35-60 minutes on the shared desktop; `build.sh` took 20 minutes to almost
two hours there, depending on load (a release build of `hs-cli` inside the Docker VM).

`run.sh` skips cleanly (exit 0) without Docker. It clones Sytest into `refs/sytest` if that is
missing and records the commit it ran. Results go to `target/sytest/<UTC timestamp>/`:

| File | What |
|---|---|
| `results.tap` | Sytest's TAP output |
| `results.txt` | one line per test: `PASS`, `FAIL`, `SKIP` or `XFAIL` (failed, and Sytest marks it expected to fail), then the name |
| `summary.txt` | counts and the most common failure reasons (`summarize.py`); a server or haproxy that died mid-run is named at the top |
| `are-we-synapse-yet.txt` | pass rate per feature group (`are-we-synapse-yet.py`) |
| `run-tests.stderr` | Sytest's progress output |
| `memory.log` | every 15 s, each server's resident, file-backed and swapped memory and thread count |
| `server-N/` | each homeserver's `hs.log`, `haproxy.log` and `myelin.yaml` |

Dated results are committed under `docs/status/sytest/`.

Environment for `run.sh`:

| Variable | Default | What |
|---|---|---|
| `SYTEST_IMAGE_TAG` | `myelin-sytest:dev` | image from `build.sh` |
| `SYTEST_DIR` | `refs/sytest` | Sytest checkout to mount |
| `SYTEST_LOGS` | `target/sytest/<timestamp>` | results directory |
| `SYTEST_HS_BINARY` | (the image's) | a bookworm `hs` to run instead, so a server change needs no image rebuild |
| `SYTEST_EXTRA_ENV` | | space-separated `NAME=value` for the servers, e.g. `RUST_BACKTRACE=1` |
| `TIMEOUT_FACTOR` | `1` | Sytest's multiplier for every wait, including a server's 60 s to start |
| `MYELIN_MEMORY_INTERVAL` | `15` | seconds between `memory.log` samples |
| `SYTEST_WORK_SIZE` | `3g` | the tmpfs holding the servers' data |

`build.sh` itself is incremental (the registry and `target/` are BuildKit cache mounts, so a
rebuild after a server change compiles only what changed), which is the simplest way to test a
change. A bookworm `hs` for `SYTEST_HS_BINARY`, reusing a cargo cache between builds, is the
other way, and does not need the Sytest image rebuilt at all:

```bash
docker run --rm -v "$PWD:/src:ro" -v myelin-sytest-cargo-target:/target \
  -v myelin-sytest-cargo-registry:/usr/local/cargo/registry -e CARGO_TARGET_DIR=/target -w /src \
  mirror.gcr.io/library/rust:1.98-slim-bookworm cargo build --release --locked -p hs-cli --bin hs
docker run --rm -v myelin-sytest-cargo-target:/target -v "$PWD/target:/out" \
  mirror.gcr.io/library/rust:1.98-slim-bookworm cp /target/release/hs /out/hs-bookworm
SYTEST_HS_BINARY="$PWD/target/hs-bookworm" tests/sytest/run.sh
```

## How it fits together

- `Dockerfile` / `build.sh`: builds `hs` in `rust:1.98-slim-bookworm` (the same Debian as Sytest's
  image, so glibc matches) and copies it into `matrixdotorg/sytest:bookworm`. `build.sh` streams a
  tar of the repository (no `target/`), uses BuildKit (for the cache mounts) with a `DOCKER_CONFIG`
  that has no credential helper, and pulls the bases from `mirror.gcr.io`, because Docker Hub pulls through
  the desktop's keychain helper fail in agent sessions.
- `run.sh`: mounts the Sytest checkout at `/sytest`, this directory at `/myelin` and the results
  directory at `/logs`, puts the servers' working directory on a tmpfs (on the container's
  overlay filesystem a first boot's fsyncs took longer than Sytest's 60 s start limit), and runs
  `myelin_sytest.sh` in the image.
- `myelin_sytest.sh`: trusts Sytest's test CA, starts the memory sampler, runs
  `run-tests.pl -I Myelin -O tap --all --exclude-deprecated` as Sytest's own
  `scripts/dendrite_sytest.sh` does, then copies each server's logs out. `sytest-blacklist` in
  this directory, if present and non-empty, is passed with `-B`; there is none, so every test
  runs.
- `plugins/myelin/lib/SyTest/{HomeserverFactory,Homeserver}/Myelin.pm`: the plugin. Sytest finds
  it through `SYTEST_PLUGINS` (`run-tests.pl` adds `$SYTEST_PLUGINS/*/lib` to its search path and
  `Module::Pluggable` finds `SyTest::HomeserverFactory::Myelin`). Each Sytest homeserver is `hs
  serve` on a plaintext port (client, federation, media and health on one router) with haproxy
  (one thread: with one per core its watchdog killed it under load) in front terminating TLS on
  the port Sytest uses, since `hs serve` does not terminate TLS. The server name is
  `localhost:<TLS port>`. Certificates are signed by Sytest's test CA and verified everywhere:
  federation trusts it through `federation.custom_ca_certificates`, the appservice client through
  the container's trust store. The rest follows what Sytest's Synapse configuration changes: open
  registration, guest access, shared secret `reg_secret`, no rate limits, no IP blocklists, public rooms over
  federation, and the appservice registrations Sytest writes for server 0
  (`appservices.registration_files`).
- `summarize.py`: TAP to `results.txt` and `summary.txt`.
- `are-we-synapse-yet.py` and `are-we-synapse-yet.list`: Conduit's copies (Apache-2.0) of
  Dendrite's grouping of Sytest's tests into features; attribution in the script's header.

The plugin's shape follows Sytest's `Dendrite.pm` and the haproxy front of `Synapse.pm` (both
Apache-2.0, New Vector Ltd); the configuration is this project's own.
