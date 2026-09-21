# syntax=docker/dockerfile:1.7
#
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
#
ARG RUST_VERSION=1.98
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
FROM rust:${RUST_VERSION}-slim-bookworm AS builder

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

WORKDIR /src

# The whole workspace is copied in one shot rather than layering a dummy-crate dependency-warming
# trick on top: the `--mount=type=cache` mounts below on `/usr/local/cargo/registry` and
# `/src/target` already give incremental rebuilds their speedup (BuildKit persists those across
# builds), without the extra Dockerfile complexity and the risk of the dummy build drifting from
# the workspace's real crate list. Falls back gracefully to a slower full rebuild on a builder
# without BuildKit cache-mount support.
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates

ENV CARGO_TERM_COLOR=never

RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    RUST_TARGET="$(cat /rust_target.txt)" && \
    cargo build --release --locked -p hs-cli --target "${RUST_TARGET}" && \
    mkdir -p /out /out/data && \
    cp "target/${RUST_TARGET}/release/hs" /out/hs

# ---------------------------------------------------------------------------
# Runtime: distroless static, non-root, no shell, nothing but the binary and CA certs.
# ---------------------------------------------------------------------------
FROM gcr.io/distroless/static-debian12:nonroot AS runtime

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
