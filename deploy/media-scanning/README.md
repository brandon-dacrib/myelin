# Reference deployment: content scanning (c-icap + ClamAV)

Brings up c-icap fronting ClamAV (`opencloudeu/clamav-icap`, one container bundling both) so
`hs-media`'s `icap` content-scanning provider (`docs/rfcs/0008-content-scanning.md`,
`crates/hs-media/src/scanning/providers/icap.rs`) has something real to scan against. The point of
this directory: enabling content scanning should be a configuration change, not an integration
project.

## Status: untested

Docker was not available in the sandbox this was written in (the same constraint
`deploy/Dockerfile` already documents for the homeserver image itself). This compose file and
config were reviewed line by line against:

- `opencloudeu/clamav-icap`'s documented usage
  (<https://github.com/opencloud-eu/container-clamav-icap>): image name, port, and the two ICAP
  service aliases it exposes (`avscan`, `srv_clamav`).
- `crates/hs-media/src/scanning/providers/icap.rs`'s actual wire behavior (OPTIONS/ISTag caching,
  preview negotiation, `X-Infection-Found`/`X-Virus-ID` header parsing) and
  `crates/hs-media/src/scanning/config.rs`'s `ScanningConfig::from_yaml` shape.

...but never actually started. Before relying on this, at minimum:

```
docker compose -f deploy/media-scanning/compose.yaml up
docker compose -f deploy/media-scanning/compose.yaml logs clamav-icap
```

and confirm the log shows c-icap listening on `1344` and ClamAV's database loaded, then see
"What's still missing" below for what a working stack still does not prove.

## What this contains

- **`compose.yaml`**: the `clamav-icap` service, plus a `homeserver` service that is deliberately
  commented out. See the comment above it in that file: no `hs-*` binary reads `media.scanning`
  from a config file and calls `MediaRepository::with_scanning` at startup yet, because no
  listener crate constructs a `MediaRepository` at all yet
  (`grep -rl MediaRepository::new crates/ | grep -v crates/hs-media` finds nothing outside this
  track's own tests). Uncomment and adapt that service once that startup wiring exists.
- **`media-scanning.yaml`**: the `media.scanning` configuration block, in
  `ScanningConfig::from_yaml`'s exact shape, pointing `icap.host`/`icap.port` at the `clamav-icap`
  service by its compose-network hostname, with `service: avscan`.

## Signature freshness

`opencloudeu/clamav-icap`'s virus definitions are fetched via `freshclam` at **image build time
only** -- there is no automatic re-fetch while a container built from that image keeps running. A
real deployment needs its own answer to "how do virus definitions stay current": a rebuild/repull
cadence (`docker compose pull && docker compose up -d` on a schedule), or a `freshclam` sidecar
writing into a volume the `clamav-icap` container reads from. This reference stack takes no
position on that beyond flagging it -- do not run a long-lived instance of this exact compose file
in production and assume its virus database stays current.

## What's still missing (do not treat this directory as "done")

1. **A real interoperability test against this stack.** `crates/hs-media`'s own tests
   (`scanning::providers::icap::tests::live`, see `docs/status/09-media.md`) run `IcapScanner`
   against `icap-rs`'s own, independent RFC 3507 server implementation, in-process -- proof
   `IcapScanner` and `icap-rs` agree on the wire protocol, **not** proof of interoperability with
   a real c-icap+ClamAV stack. Once Docker is available, add an env-var-gated integration test
   (mirroring `crates/hs-media/tests/s3_backend.rs`'s pattern: skip cleanly with an `eprintln!`
   when the env var is unset, run for real when it is) that submits the EICAR test string through
   this compose stack over ICAP and asserts `Verdict::Infected`.
2. **Track 13's config folding.** `hs_config::MediaConfig` has no `scanning` field yet
   (`crates/hs-media/src/scanning/config.rs`'s module doc) -- `media-scanning.yaml` is what to
   fold in under a `media.scanning` key once that field exists.
3. **Track 12's Helm sub-chart.** `deploy/helm/hs` has no equivalent of this compose stack for a
   Kubernetes deployment (a `c-icap`+ClamAV sub-chart or sidecar, with the homeserver chart's
   `values.yaml` exposing the same `media.scanning` block). Not attempted here: `deploy/helm` is
   track 12's, not this track's, to edit.
4. **The homeserver startup wiring itself** (see `compose.yaml`'s comment): once a listener binary
   exists, it needs to read `media.scanning` from its config, build a
   `hs_media::scanning::ScanEngine` from it (`ScanningConfig::validated` then `ScanEngine::new`),
   and call `MediaRepository::with_scanning` before serving any request. None of that glue exists
   yet; only the pieces it would call do.
