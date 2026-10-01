# Sytest

Layer L4 of `PLAN.md` section 12: Sytest, Synapse's older integration suite, run against the real
`hs` binary. It runs in Docker, on Sytest's own image (`matrixdotorg/sytest`, which carries Perl
and every CPAN module Sytest needs), so nothing is installed on the host.

```bash
tests/sytest/build.sh myelin-sytest:dev        # release `hs` on Debian bookworm + Sytest's image
SYTEST_IMAGE_TAG=myelin-sytest:dev tests/sytest/run.sh                    # the whole suite
SYTEST_IMAGE_TAG=myelin-sytest:dev tests/sytest/run.sh tests/11register.pl  # one file
```

`run.sh` skips cleanly (exit 0) without Docker. It clones Sytest into `refs/sytest` if that is
missing and records the commit it ran. Results go to `target/sytest/<UTC timestamp>/`:

| File | What |
|---|---|
| `results.tap` | Sytest's TAP output |
| `results.txt` | one line per test: `PASS`, `FAIL`, `SKIP` or `XFAIL` (failed, and Sytest marks it expected to fail), then the name |
| `summary.txt` | counts and the most common failure reasons (`summarize.py`) |
| `are-we-synapse-yet.txt` | pass rate per feature group (`are-we-synapse-yet.py`) |
| `run-tests.stderr` | Sytest's progress output |
| `server-N/` | each homeserver's `hs.log`, `haproxy.log` and `myelin.yaml` |

Dated results are committed under `docs/status/sytest/`.

## How it fits together

- `Dockerfile` / `build.sh`: builds `hs` in `rust:1.98-slim-bookworm` (the same Debian as Sytest's
  image, so glibc matches) and copies it into `matrixdotorg/sytest:bookworm`. `build.sh` streams a
  tar of the repository (no `target/`), uses the classic builder and a `DOCKER_CONFIG` with no
  credential helper, and pulls the bases from `mirror.gcr.io`, because Docker Hub pulls through
  the desktop's keychain helper fail in agent sessions.
- `run.sh`: mounts the Sytest checkout at `/sytest`, this directory at `/myelin` and the results
  directory at `/logs`, and runs `myelin_sytest.sh` in the image.
- `myelin_sytest.sh`: `run-tests.pl -I Myelin -O tap --all --exclude-deprecated`, as Sytest's own
  `scripts/dendrite_sytest.sh` does, then copies each server's logs out. `sytest-blacklist` in
  this directory, if present and non-empty, is passed with `-B`; there is none yet, so every test
  runs.
- `plugins/myelin/lib/SyTest/{HomeserverFactory,Homeserver}/Myelin.pm`: the plugin. Sytest finds
  it through `SYTEST_PLUGINS` (`run-tests.pl` adds `$SYTEST_PLUGINS/*/lib` to its search path and
  `Module::Pluggable` finds `SyTest::HomeserverFactory::Myelin`). Each Sytest homeserver is `hs
  serve` on a plaintext port (client, federation, media and health on one router) with haproxy
  in front terminating TLS on the port Sytest uses, since `hs serve` does not terminate TLS. The
  server name is `localhost:<TLS port>`. The configuration follows what Sytest's Synapse
  configuration changes: open registration, shared secret `reg_secret`, no rate limits, no IP
  blocklists, public rooms over federation, outbound certificate checks off (every certificate is
  self-signed), and the appservice registrations Sytest writes for server 0
  (`appservices.registration_files`).
- `summarize.py`: TAP to `results.txt` and `summary.txt`.
- `are-we-synapse-yet.py` and `are-we-synapse-yet.list`: Conduit's copies (Apache-2.0) of
  Dendrite's grouping of Sytest's tests into features; attribution in the script's header.

The plugin's shape follows Sytest's `Dendrite.pm` and the haproxy front of `Synapse.pm` (both
Apache-2.0, New Vector Ltd); the configuration is this project's own.
