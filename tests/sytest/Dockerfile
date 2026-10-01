# Sytest image for this server (PLAN.md section 12 layer L4; docs/workstreams/14-test-and-conformance.md).
#
# Two stages: the release `hs` binary, built on the same Debian release as Sytest's own image so
# its glibc matches, then `matrixdotorg/sytest` (Perl and every CPAN module Sytest needs, plus
# haproxy) with that binary added. Nothing else is baked in: the Sytest checkout, this
# directory's plugin and the in-container runner are mounted by `run.sh`, so changing the plugin
# does not mean rebuilding the binary.
#
# The base images are arguments because Docker Hub pulls fail on this project's desktop (the
# keychain credential helper); `build.sh` passes the `mirror.gcr.io` names. The build context is
# a tar stream from `build.sh`, as for `tests/complement/`, so `target/` (tens of GB) never goes
# to the daemon.
ARG RUST_IMAGE=rust:1.98-slim-bookworm
ARG SYTEST_IMAGE=matrixdotorg/sytest:bookworm

FROM ${RUST_IMAGE} AS build
WORKDIR /workspace
RUN apt-get update && apt-get install -y --no-install-recommends pkg-config libssl-dev \
    && rm -rf /var/lib/apt/lists/*
COPY . .
# Four jobs, not every core: the desktop is shared with other agents' builds.
RUN CARGO_PROFILE_RELEASE_DEBUG=0 cargo build --release --jobs 4 -p hs-cli --bin hs

FROM ${SYTEST_IMAGE}
COPY --from=build /workspace/target/release/hs /usr/local/bin/hs
# `run.sh` mounts the Sytest checkout at /sytest and this directory (plugin, runner, blacklist)
# at /myelin; results land in /logs.
ENTRYPOINT ["/bin/bash", "/myelin/myelin_sytest.sh"]
