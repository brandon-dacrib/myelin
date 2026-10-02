# Multi-stage, multi-arch (linux/amd64, linux/arm64) image for the `hs` binary
# (crates/hs-cli), built statically against musl so the final stage needs no libc at all and can
# be `gcr.io/distroless/static` — the smallest, lowest-CVE-surface distroless base there is.
#
# Built and run successfully on 2026-09-19 (see docs/status/12-platform-and-kubernetes.md for the
# transcript): `docker build -f deploy/Dockerfile -t hs:track12-verify .` on linux/arm64
# (OrbStack), then a running container answered `/health/live` and `/_matrix/client/versions`.
# That first build caught one real bug this comment used to paper over: `${RUST_VERSION}` was
# read in the `FROM` line below with no `ARG RUST_VERSION` declared above it, which is out of
# scope for a build arg used in `FROM` (global build args must be declared before the first
# `FROM`, and do not survive into a stage unless re-declared with `ARG NAME` inside it too, which
# this file already does not need since the value is only used in `FROM`). The image had never
# been built before that fix, so `${RUST_VERSION}` silently resolved to `""`, i.e.
# `rust:-slim-bookworm` — not a real tag. Fixed below with a default so `docker build` with no
# `--build-arg` still works.
#
# Build arguments:
#   RUST_VERSION  - pinned to match rust-toolchain.toml's channel; defaults to a recent stable.
#   RUST_IMAGE, NODE_IMAGE, RUNTIME_IMAGE - the base images, for a builder that cannot reach
#                 Docker Hub. On this project's desktop an agent session's `docker pull` from
#                 Docker Hub fails in the keychain credential helper, so a local build passes
#                 the `mirror.gcr.io/library/...` names (as tests/sytest/build.sh does):
#
#     docker buildx build -f deploy/Dockerfile \
#       --build-arg RUST_IMAGE=mirror.gcr.io/library/rust:1.98-slim-bookworm \
#       --build-arg NODE_IMAGE=mirror.gcr.io/library/node:22-bookworm-slim -t myelin:dev .
#
# There is no `# syntax=docker/dockerfile:...` line for the same reason: it fetches the frontend
# from Docker Hub before anything else, and every feature used here (cache mounts, `COPY
# --chown`) has been in BuildKit's built-in frontend since Docker 20.10.
#
ARG RUST_VERSION=1.98
ARG RUST_IMAGE=rust:${RUST_VERSION}-slim-bookworm
ARG NODE_VERSION=22
ARG NODE_IMAGE=node:${NODE_VERSION}-bookworm-slim
ARG RUNTIME_IMAGE=gcr.io/distroless/static-debian12:nonroot
#
# ---------------------------------------------------------------------------------------------
# The management interface. Its output is JavaScript, the same for every architecture, so unlike
# the Rust stage below this one *is* pinned to the build host's platform: there is no reason to
# run `npm ci` and a TypeScript build under QEMU once per target.
#
# `npm run build` regenerates the typed API client from the admin API's OpenAPI document before
# it compiles, which is why that one directory of `crates/` is copied in beside `web/`, at the
# relative path the script expects.
# ---------------------------------------------------------------------------------------------
FROM --platform=$BUILDPLATFORM ${NODE_IMAGE} AS web

WORKDIR /src/web
COPY web/package.json web/package-lock.json ./
RUN --mount=type=cache,target=/root/.npm npm ci
COPY web ./
COPY crates/hs-admin/openapi /src/crates/hs-admin/openapi
RUN npm run build

#
# Deliberately *not* pinned to `--platform=$BUILDPLATFORM`: when built with
# `docker buildx build --platform linux/amd64,linux/arm64`, buildx runs the entire build once per
# requested platform (each under QEMU emulation if the host cannot run it natively), so every
# `RUN` in this stage already executes *as* the target architecture. That means the target's own
# native `musl-gcc` links the target's own native binary — no cross-compiler package, no
# `CARGO_TARGET_*_LINKER` override, no `TARGETARCH`-to-triple mapping to keep in sync by hand.
# Simpler and more robust than cross-compiling from a single amd64 host, at the cost of a slower
# (emulated) arm64 build in CI unless run on a native arm64 runner
# (.github/workflows/ci.yml's `ubuntu-24.04-arm` matrix leg is exactly that).
#
# The Rust build is three stages on one base. `chef` is the toolchain plus cargo-chef; `planner`
# reduces the workspace to a recipe (its manifests and lock file, nothing else); `builder` cooks
# the dependency graph from that recipe in a layer of its own, and only then copies the sources
# in and builds the server. The cooked layer's cache key is the recipe, so a source change
# recompiles the workspace's own crates and nothing beneath them. Until 2026-10-02 this stage
# kept `target/` in a `--mount=type=cache`, which BuildKit holds in the builder's local state and
# never exports: on CD's fresh runner every image build compiled every dependency from scratch
# (66 minutes for one image on 2026-10-01), with the `type=gha` cache holding only layers that
# could never hit because the `COPY crates` above them changed on every commit. A layer can be
# exported, so the cooked dependencies now ride in that cache. The cargo registry keeps its cache
# mount, which only matters on a builder that lives between builds (a desktop).
FROM ${RUST_IMAGE} AS chef

RUN apt-get update && apt-get install -y --no-install-recommends \
        musl-tools \
        pkg-config \
        ca-certificates \
    && rm -rf /var/lib/apt/lists/*

# `uname -m` on the (possibly emulated) target reports the musl target triple's arch component
# directly (x86_64 or aarch64), so this needs no TARGETARCH build arg at all.
RUN case "$(uname -m)" in \
        x86_64) echo "x86_64-unknown-linux-musl" > /rust_target.txt ;; \
        aarch64) echo "aarch64-unknown-linux-musl" > /rust_target.txt ;; \
        *) echo "unsupported arch: $(uname -m)" >&2; exit 1 ;; \
    esac

RUN rustup target add "$(cat /rust_target.txt)"

# Built from source rather than taken from the `lukemathwalker/cargo-chef` image, so the base
# stays the official `rust` image the rest of this file and tests/sytest/Dockerfile pin. The
# layer is keyed on this instruction and RUST_VERSION, so it is cached until the toolchain moves.
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    cargo install cargo-chef --locked

ENV CARGO_TERM_COLOR=never
WORKDIR /src

FROM chef AS planner
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
RUN cargo chef prepare --recipe-path recipe.json

FROM chef AS builder

COPY --from=planner /src/recipe.json recipe.json
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    cargo chef cook --release --locked -p hs-cli --target "$(cat /rust_target.txt)" \
        --recipe-path recipe.json

COPY Cargo.toml Cargo.lock ./
COPY crates ./crates

# The interface built above, and an instruction to `crates/hs-admin/build.rs` to embed *that* and
# to fail the build if it is not there. Without the variable the build script falls back to a
# placeholder page so that a Rust-only checkout still compiles -- which is how every image before
# this one came to serve "the management interface has not been built into this binary yet".
COPY --from=web /src/web/dist /src/web-dist
ENV HS_ADMIN_WEB_DIST=/src/web-dist

# No cache mount over /src/target here: it would hide the cooked dependencies in this stage's
# own layers, which are the cache.
#
# The cook ran cargo-chef's stub of every workspace build script, and `COPY` keeps the real
# scripts' mtimes, which are older than the cook, so cargo then sees the stub's run as fresh:
# `crates/hs-admin/build.rs` never staged the interface, and the first cooked build failed with
# "folder '.../out/web-dist' does not exist" (2026-10-02). The stubs' compiled binaries and
# fingerprints (host artifacts, under target/release/ because of `--target`) and their runs'
# outputs and fingerprints (under target/<triple>/release/) are removed for every crate that has
# a build script, so each is compiled and run for real.
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    RUST_TARGET="$(cat /rust_target.txt)" && \
    for script in crates/*/build.rs; do \
        crate="$(basename "$(dirname "$script")")"; \
        rm -rf "target/release/build/${crate}-"* \
               "target/release/.fingerprint/${crate}-"* \
               "target/${RUST_TARGET}/release/build/${crate}-"* \
               "target/${RUST_TARGET}/release/.fingerprint/${crate}-"*; \
    done && \
    cargo build --release --locked -p hs-cli --target "${RUST_TARGET}" && \
    mkdir -p /out /out/data && \
    cp "target/${RUST_TARGET}/release/hs" /out/hs

# ---------------------------------------------------------------------------
# Runtime: distroless static, non-root, no shell, nothing but the binary and CA certs.
# ---------------------------------------------------------------------------
FROM ${RUNTIME_IMAGE} AS runtime

# distroless/static-debian12:nonroot already runs as uid/gid 65532 ("nonroot") and ships CA
# certificates (needed for outbound federation TLS and OTLP/Sentry export over HTTPS).
COPY --from=builder /out/hs /usr/local/bin/hs

# An empty /data that the non-root user owns. The runtime image has no shell to `mkdir` or `chown`
# with, so the directory is made in the builder and copied across. It has to exist in the image:
# a named volume mounted over a directory inherits that directory's ownership the first time it
# is used, and one mounted over nothing is root's, which uid 65532 cannot write to.
COPY --from=builder --chown=65532:65532 /out/data /data

# Metadata only; the chart/orchestrator is expected to enforce `readOnlyRootFilesystem: true`,
# `runAsNonRoot: true` and drop all capabilities (deploy/helm's securityContext defaults do this —
# see deploy/helm/hs/values.yaml). This image has no shell and no package manager, so there is
# nothing to `apt-get install` into it at runtime even if the filesystem were writable.
USER 65532:65532
WORKDIR /

# /_matrix client/federation default port and the native admin/health/metrics port
# (hs-config's ListenersConfig default, crates/hs-config/src/listeners.rs) — informational only,
# the actual bound ports come from the mounted config.
EXPOSE 8008 8448

# With no arguments this image is a first run: `hs serve` with `HS_DATA_DIR` set puts the
# database, the signing key and the media store under /data (crates/hs-cli/src/bootstrap.rs), and
# everything else is a default, a `HS__` variable, or a setting changed later in the admin
# interface and kept in the database. So
#
#   docker run -p 8008:8008 -v myelin:/data -e HS__SERVER__SERVER_NAME=example.org <image>
#
# is a working server. The derived paths are only defaults: a deployment that mounts a config
# file and passes `serve -c <file>` (the Helm chart does) sets all three itself and never touches
# /data.
ENV HS_DATA_DIR=/data
VOLUME /data

ENTRYPOINT ["/usr/local/bin/hs"]
CMD ["serve"]
