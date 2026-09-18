# Sytest plugin

Layer L4 of `PLAN.md` section 12: "a Sytest homeserver plugin for our binary; run with Synapse's
expectations... kept until Complement reaches Sytest parity." **Everything in this directory is
untested**: no `hs-server` binary exists in this workspace yet, and this environment does not have
Sytest's CPAN dependencies (`Future`, `IO::Async`, ...) installed, so `run_sytest.sh` has never
gotten past its precondition checks. This is the plugin scaffold, structured the way Sytest
actually discovers plugins (see below), ready for the `TODO` markers in
`plugins/hs-reimplement/lib/SyTest/Homeserver/HsReimplement.pm` to be filled in once a server
binary and its config format (`hs-config`, track 13) exist.

## How Sytest discovers a plugin

`refs/sytest/run-tests.pl` uses `Module::Pluggable` to search `$SYTEST_PLUGINS/*/lib` for
`SyTest::HomeserverFactory::*` packages (see that file's top for the exact search config: each
plugin is a directory containing its own `lib/` tree, mirroring Sytest's own `lib/` layout). This
directory follows that shape exactly:

```
tests/sytest/plugins/hs-reimplement/lib/SyTest/HomeserverFactory/HsReimplement.pm
tests/sytest/plugins/hs-reimplement/lib/SyTest/Homeserver/HsReimplement.pm
```

`SyTest::HomeserverFactory::HsReimplement` (the factory) registers the implementation name
`HsReimplement` and the `--hs-reimplement-binary-directory` option; `SyTest::Homeserver::HsReimplement`
(adapted in shape, with attribution, from `refs/sytest/lib/SyTest/Homeserver/Dendrite.pm` --
Apache-2.0, Sytest project, Copyright New Vector Ltd, the closest existing analog since Dendrite is
also a single from-scratch monolith binary rather than a Python framework) does the actual
start/stop/config-file-writing.

## Running once a server binary and CPAN deps exist

```bash
# from the repository root
cargo build --release   # produces target/release/hs-server (once that binary exists)
tools/fetch-refs.sh      # clones refs/sytest if missing (network required)
cd refs/sytest && perl -MCPAN -e 'CPAN::Shell->install_tested' # or however Sytest's own docs say
                                                                 # to install its dependencies
cd ../..
./tests/sytest/run_sytest.sh
```

`run_sytest.sh` checks for Sytest's checkout, Perl, Sytest's CPAN dependencies, and an
`hs-server` binary before doing anything, and exits 0 with an explanation if any is missing
(matching `tests/complement/`'s and `tests/differential/`'s "skip cleanly" behavior). Extra
arguments pass through to `run-tests.pl`, e.g.:

```bash
./tests/sytest/run_sytest.sh -F as_id -- t/30apidoc/*.pl
```

## What is still a placeholder

- The `hs-server` command-line flags `_start_server` in `HsReimplement.pm` builds
  (`--server-name`, `--client-listen`, `--federation-listen`, `--federation-tls-cert/key`,
  `--data-dir`, `--registration-shared-secret`, `--config`) are a guess matching
  `tests/complement/startup.sh`'s equivalent invocation for consistency between the two harnesses;
  update both together against the real CLI once it exists.
- The config file `HsReimplement.pm` points `hs-server --config` at
  (`$hs_dir/hs-reimplement.yaml`) is never actually written by this plugin yet — `hs-config`
  (track 13) needs to land its schema first so this plugin knows what to put in it (Sytest expects
  per-run overrides like the registration shared secret and listener ports to be configurable this
  way, not just via CLI flags, for the options CLI flags don't cover).
- No blacklist/skip mechanism is wired up yet (Sytest tests are `.pl` files under `refs/sytest/t/`;
  the usual mechanism is `-e <test name>` / a file of test names passed via Sytest's own
  `--exclude-file`). Add one alongside `tests/complement/blacklist.txt`'s equivalent once real runs
  produce real failures to track.

## References

- `refs/sytest/run-tests.pl` — the `Module::Pluggable` plugin discovery this directory follows.
- `refs/sytest/lib/SyTest/Homeserver.pm`, `.../Homeserver/Dendrite.pm`,
  `.../HomeserverFactory.pm`, `.../HomeserverFactory/Dendrite.pm` — the base classes and the
  closest existing analog (Apache-2.0).
