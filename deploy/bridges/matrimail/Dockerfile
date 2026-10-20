# matrimail, the Matrix <-> email bridge, as an image this repository builds and publishes
# (ghcr.io/brandon-dacrib/matrimail), because its project publishes none: no release, no
# package (decision 0055). The source is the active fork, moiri-gamboni/matrimail, at the
# commit MATRIMAIL_COMMIT pins; the catalogue entry `matrimail` (crates/hs-admin/src/
# bridge_types.rs) pins the same commit as the image's tag, and a test there keeps the two
# equal. .github/workflows/matrimail-image.yml builds this for linux/amd64 and linux/arm64.
#
# matrimail is AGPL-3.0-or-later (portions from mautrix-whatsapp, AGPL-3.0-or-later, by Tulir
# Asokan and contributors). This file builds it unchanged from its source and copies its
# LICENSE into the image; the source for any published image is the repository and commit the
# image's labels name. Nothing of Myelin's is linked into it.
#
# Why not the project's own Dockerfile: its final stage is distroless (no shell), and the
# operator's init container runs a `sh` script in the bridge's image to write /data's files
# (crates/hs-operator/src/bridge.rs, COPY_FILES_SCRIPT); and its binary runs as a fixed
# non-root user that cannot write a volume the cluster mounted as root. This one ends in
# bookworm-slim and starts the way mautrix's images do: as root, `chown /data`, then the bridge
# as UID 1337 (docker-run.sh beside this file).
#
# Local build, on this project's desktop (Docker Hub through Google's mirror, a DOCKER_CONFIG
# without a credential helper, as tests/sytest/build.sh does):
#
#   docker build -f deploy/bridges/matrimail/Dockerfile -t matrimail:24cd0f9 deploy/bridges/matrimail
#
# Build arguments: MATRIMAIL_REPO and MATRIMAIL_COMMIT (the source), GO_IMAGE and
# RUNTIME_IMAGE (the bases; the Hub's own names work where the Hub is reachable).
ARG GO_IMAGE=mirror.gcr.io/library/golang:1.25-bookworm
ARG RUNTIME_IMAGE=mirror.gcr.io/library/debian:bookworm-slim

FROM ${GO_IMAGE} AS builder
ARG MATRIMAIL_REPO=https://github.com/moiri-gamboni/matrimail
ARG MATRIMAIL_COMMIT=24cd0f981f803897f7b71693c1c84738e78cf9d7
# build-essential for go-sqlite3 (cgo); no libolm: the build uses mautrix-go's pure-Go Olm
# (`goolm`), which is what the project's own Makefile does. Never `nocrypto`: the bridge's
# rooms are encrypted.
RUN apt-get update -y \
    && apt-get install -y --no-install-recommends git ca-certificates build-essential \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /src
# The one commit, fetched by its hash (GitHub serves reachable commits by hash), so that the
# build is the same whatever the fork's branches do later.
RUN git init -q . \
    && git remote add origin "${MATRIMAIL_REPO}" \
    && git fetch -q --depth 1 origin "${MATRIMAIL_COMMIT}" \
    && git checkout -q --detach FETCH_HEAD \
    && test "$(git rev-parse HEAD)" = "${MATRIMAIL_COMMIT}"
ARG TARGETARCH
RUN --mount=type=cache,target=/go/pkg/mod \
    go mod download
RUN --mount=type=cache,target=/root/.cache/go-build,id=matrimail-go-build-${TARGETARCH} \
    --mount=type=cache,target=/go/pkg/mod \
    CGO_ENABLED=1 go build -tags goolm \
      -ldflags "-s -w -X main.Tag=myelin -X main.Commit=${MATRIMAIL_COMMIT} -X main.BuildTime=$(date -u +%Y-%m-%dT%H:%M:%SZ)" \
      -o /out/matrimail ./cmd/matrimail \
    && mkdir -p /out/doc && cp LICENSE /out/doc/LICENSE

FROM ${RUNTIME_IMAGE}
ARG MATRIMAIL_REPO=https://github.com/moiri-gamboni/matrimail
ARG MATRIMAIL_COMMIT=24cd0f981f803897f7b71693c1c84738e78cf9d7
LABEL org.opencontainers.image.title="matrimail" \
      org.opencontainers.image.description="matrimail, the Matrix <-> email bridge (mautrix bridgev2), built by the Myelin project from its source" \
      org.opencontainers.image.source="${MATRIMAIL_REPO}" \
      org.opencontainers.image.revision="${MATRIMAIL_COMMIT}" \
      org.opencontainers.image.licenses="AGPL-3.0-or-later"
# ca-certificates for IMAP, SMTP and the Gmail API over TLS; tzdata for the dates it writes.
RUN apt-get update -y \
    && apt-get install -y --no-install-recommends ca-certificates tzdata \
    && rm -rf /var/lib/apt/lists/*
COPY --from=builder /out/matrimail /usr/bin/matrimail
COPY --from=builder /out/doc/LICENSE /usr/share/doc/matrimail/LICENSE
COPY docker-run.sh /docker-run.sh
# The user the bridge runs as (docker-run.sh drops to it after chowning /data), as mautrix's.
ENV UID=1337 GID=1337
# `/` is the working directory on purpose: matrimail keeps the passphrase that encrypts the
# mailbox credentials at `./data/passphrase` when MATRIMAIL_PASSPHRASE is not set, which is
# then /data/passphrase, a file Myelin writes with the bridge's other files.
WORKDIR /
VOLUME ["/data"]
ENTRYPOINT ["/docker-run.sh"]
