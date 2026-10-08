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
#
# The cargo registry and the build's `target/` are BuildKit cache mounts, kept by the daemon
# between builds: the first build compiles everything, the next one only what changed, in the
# same way a `cargo build` in a checkout does. Before 2026-10-02 every image build was a cold
# release build of the whole dependency graph (the README's `myelin-sytest-cargo-target` recipe
# existed to avoid exactly that, by hand, for `SYTEST_HS_BINARY`). The binary is copied out of
# the mount in the same RUN, since a cache mount is not part of the layer.
ARG RUST_IMAGE=rust:1.98-slim-bookworm
ARG SYTEST_IMAGE=matrixdotorg/sytest:bookworm

FROM ${RUST_IMAGE} AS build
WORKDIR /workspace
RUN apt-get update && apt-get install -y --no-install-recommends pkg-config libssl-dev \
    && rm -rf /var/lib/apt/lists/*
COPY . .
# Four jobs, not every core: the desktop is shared with other agents' builds.
#
# The `target/` mount's id is TARGET_CACHE_ID, which build.sh sets per image tag (one cache shared
# by every branch's image once linked another branch's crate into an image, 2026-10-04/05); a
# plain `docker build` without the argument gets the old shared id.
ARG TARGET_CACHE_ID=myelin-sytest-target
RUN --mount=type=cache,target=/usr/local/cargo/registry,id=myelin-cargo-registry \
    --mount=type=cache,target=/workspace/target,id=${TARGET_CACHE_ID} \
    CARGO_PROFILE_RELEASE_DEBUG=0 cargo build --release --jobs 4 -p hs-cli --bin hs \
    && cp target/release/hs /hs

FROM ${SYTEST_IMAGE}
COPY --from=build /hs /usr/local/bin/hs
# `run.sh` mounts the Sytest checkout at /sytest and this directory (plugin, runner, blacklist)
# at /myelin; results land in /logs.
ENTRYPOINT ["/bin/bash", "/myelin/myelin_sytest.sh"]
