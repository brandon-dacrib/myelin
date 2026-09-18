# 09 Media: status

Track brief: `docs/workstreams/09-media.md`. Owner crate: `hs-media`.

Last updated: 2026-09-18 (session 1, working from a clean restart after a prior attempt was
interrupted before writing any code beyond a placeholder `lib.rs` and a `benches/thumbnails.rs`
stub).

## Done

This session delivered the brief's day-one work and Phase 0 deliverables in full, plus the
Synapse layout adapter and both Phase 1/2 design RFCs. Not delivered: URL previews and federation
media fetching *implementations* (design-only, by the brief's own ordering — federation media is
explicitly blocked on track 06), GCS/Azure backends, admin endpoints, retention sweeping, direct
media, metrics. See "Next" for the complete list.

- **`crates/hs-media/src/security.rs`**: the media security rules as rustdoc and tests (this
  track's item 1). `INLINE_SAFE_CONTENT_TYPES` (Synapse's `INLINE_CONTENT_TYPES` allowlist,
  behavior only) plus an `ALWAYS_ATTACHMENT` list (`text/html`, `image/svg+xml`, ...) checked
  first as defense in depth; `CSP_HEADER_VALUE` sent on every response unconditionally;
  `X-Content-Type-Options: nosniff` always sent; `content_disposition` (RFC 6266 quoting, RFC 5987
  `filename*=UTF-8''...` for non-ASCII, control-character/path-separator stripping,
  `CRLF`-injection-safe); `response_headers` composes all of it; `parse_range`/`ByteRange`/
  `RangeOutcome` for `Range:` (suffix ranges, open-ended ranges, clamping, first-of-multi-range,
  `416` on an unsatisfiable range). 32 tests.
- **`crates/hs-media/src/id.rs`**: `MediaId::generate` (24 random alphanumeric characters,
  Synapse's `random_string(24)` alphabet — behavior only) and `MediaId::parse` (validates an
  externally supplied ID, rejecting path traversal and any character unsafe as an object-store key
  or filesystem component).
- **`crates/hs-media/src/store.rs`**: `store::build` turns `hs_config::media::MediaStorageBackend`
  into `Arc<dyn object_store::ObjectStore>` — `Local` (creates its directory) and `S3` (via
  `object_store::aws::AmazonS3Builder`, no network needed to construct) implemented; `Gcs`/`Azure`
  return a clean `MediaError::Store("not yet implemented")` (the crate features `gcp`/`azure`
  already existed in the scaffold, off by default). `content_key`/`thumbnail_key` shard media by
  the first four characters (`b0`/`b1`/rest), independent of — not a copy of — Synapse's own
  on-disk scheme (`crate::synapse_layout` is what actually reproduces Synapse's scheme, for
  reading, not writing).
- **`crates/hs-media/src/metadata.rs`**: `MediaRecord`/`ThumbnailRecord` over `hs-tables`/`hs-kv`,
  `MetadataStore<B: hs_kv::KvBackend>` generic over the backend (tested against
  `hs_kv::memory::MemoryBackend`, per `docs/workstreams/README.md` rule 2). Keyed
  `(server_name, media_id)` deliberately origin-agnostic (a local upload's `server_name` is this
  server's own name; a federation-cached copy's is the origin's) so RFC 0007's remote cache needs
  no schema change. Quarantine (`quarantined_by`/`safe_from_quarantine`), async-upload
  `completed`/`expires_at_ms`, thumbnail variant listing.
- **`crates/hs-media/src/policy.rs`**: `UploadPolicy` trait (`check` before accepting bytes,
  `record` after they are stored) plus `InMemoryQuotaPolicy` (per-user and per-server cumulative
  byte totals). Pluggable, per the brief.
- **`crates/hs-media/src/sniff.rs`**: `sniff_format`/`format_matches_declared_type` (magic-byte
  detection, never fed back into the served `Content-Type` — see `security.rs`'s Rule 4) and
  `decode_with_limits` (the decompression-bomb defense: `image::Limits` on width/height/allocation,
  applied via `ImageReader::into_decoder` + `ImageDecoder::set_limits` *before* any pixel buffer is
  materialized).
- **`crates/hs-media/src/thumbnail.rs`**: crop (`resize_to_fill`) and scale
  (aspect-preserving, never-upscales) via `image::imageops::FilterType::Lanczos3`;
  `default_sizes()` re-exports `hs_config::media::MediaConfig::default().thumbnail_sizes` (32x32
  crop, 96x96 crop, 320x240/640x480/800x600 scale — the brief's table); `output_format_for` (PNG
  for alpha-capable sources — PNG, GIF, WebP — JPEG otherwise, Synapse's rule, behavior only);
  animated sources (GIF) thumbnail from the first frame only, never animated output;
  `ThumbnailPolicy` (`allow_dynamic` + max bounds) is the brief's "dynamic thumbnails" knob —
  off by default, matching Synapse's `dynamic_thumbnails: false` default.
- **`crates/hs-media/src/repository.rs`**: `MediaRepository<B>` — synchronous `upload`; async
  `create_reservation` (returns `(MediaId, expires_at_ms)`, `DEFAULT_RESERVATION_TTL_MS` = 24h,
  Synapse's default) + `complete_reservation` (checked expiry, checked "not already uploaded");
  `get_record` (quarantine looks identical to not-found to a non-admin caller; not-yet-uploaded
  vs. expired distinguished); `get_content` (range-aware, uses `object_store`'s `get_range` so a
  ranged download never reads the whole object); `get_thumbnail` (cache-or-generate, checks
  `ThumbnailPolicy` first); `set_quarantined`. 15 tests, including a real expiry test with an
  injectable clock.
- **`crates/hs-media/src/state.rs`**: `MediaState<B>` (embeds `hs_auth::state::AuthState`) and
  `MediaRequester` — see "Decisions made" for why this bridge exists and is the pattern other
  tracks composing `hs-auth` into their own state should copy.
- **`crates/hs-media/src/routes/`**: `upload.rs` (`upload_sync`, `create`, `put_upload` — the last
  checks `server_name == this server` and `uploader == requester` before completing a
  reservation, `404`-shaped either way so a reservation's existence is not leaked to a non-owner),
  `download.rs` (`download`, `download_with_filename`, shared `build_response` also used by
  `legacy.rs`), `thumbnail.rs`, `config.rs` (`m.upload.size`), `legacy.rs` (unauthenticated,
  frozen — see "Decisions made").
- **`crates/hs-media/src/router.rs`**: `authenticated_router`/`legacy_router` over
  `hs_http::router::Builder`, spec-relative paths (mounting/prefixing is the listener's job,
  matching `hs-auth`'s own `routes::router()` convention).
- **`crates/hs-media/src/multipart.rs`**: a `multipart/mixed` parser for MSC3916 federation media
  responses, built and fuzzed ahead of having a caller (track 06 does not exist yet) — see
  `docs/rfcs/0007-federation-media.md`. Bounded-iteration argument in the module doc; 15 tests
  including a mutation sweep (every single-byte flip and every truncation of a valid body must not
  panic).
- **`crates/hs-media/src/synapse_layout.rs`**: a read-only adapter over Synapse's on-disk media
  layout (`refs/synapse/synapse/media/filepath.py`, read for behavior — the actual Rust here is an
  independent reimplementation, not a translation) — all six subtrees (`local_content`,
  `remote_content`, `local_thumbnails`, `remote_thumbnail`, `url_cache` including both the dated
  and legacy sharded forms, `url_cache_thumbnails`). `classify_relative_path` is pure and
  filesystem-free (unit-tested directly); `SynapseMediaStore::iter_entries`/`read_content` do the
  real, read-only filesystem walk (integration-tested against a real temp directory tree).
- **`crates/hs-media/src/test_fixtures.rs`** (Cargo feature `test-fixtures`, on by default): valid
  PNG/JPEG/animated-GIF/WebP generators plus every adversarial case this track's brief names by
  name — `decompression_bomb_png` (a hand-crafted, ~50-byte PNG whose `IHDR` declares a 50000x50000
  image, built with `crc32fast` rather than via `image`'s own encoder specifically so generating
  the fixture never performs the large allocation it exists to test against), `truncate`,
  `svg_with_script`, `html_masquerading_as_image`.
- **`crates/hs-media/examples/gen_fixtures.rs`** + **`crates/hs-media/tests/fixtures/images/`**:
  the committed test corpus (10 files: 4 valid formats, decompression bomb, truncated header,
  truncated body, mismatched extension, adversarial SVG, HTML-as-image) and
  **`crates/hs-media/tests/image_corpus.rs`** (11 tests asserting this crate's own decode/sniff/
  thumbnail path handles every one of them correctly — valid ones succeed, adversarial ones fail
  cleanly, nothing panics).
- **`crates/hs-media/tests/s3_backend.rs`**: a real `object_store` S3-compatible put/get/get_range/
  delete round trip, entirely environment-variable-configured
  (`HS_MEDIA_TEST_S3_{BUCKET,ENDPOINT,ACCESS_KEY_ID,SECRET_ACCESS_KEY}`), skipping cleanly
  (`eprintln!` + return, not `#[ignore]`, so the skip is visible in normal `cargo test` output)
  when they are not set — this track's "skip S3 tests cleanly when no credentials are present"
  item, satisfied with an actual network-capable test rather than only a config-construction test.
- **`crates/hs-media/fuzz/`**: a separate `cargo-fuzz`-shaped crate (its own `[workspace]`, per
  the standard `cargo fuzz init` layout — the root workspace's `crates/*` glob does not reach it)
  with three targets: `decode_image` (the decoder path), `multipart_parse` (the federation-response
  parser), `thumbnail_generate` (decode + resize + re-encode). Type-checks cleanly on stable
  (`cargo check` inside `crates/hs-media/fuzz/`); **not actually run** — this sandbox has neither
  `rustup`/a nightly toolchain nor `cargo-fuzz` installed (verified: `which cargo-fuzz` and
  `rustup toolchain list` both fail). Seed corpora (`fuzz/corpus/{decode_image,thumbnail_generate,
  multipart_parse}/`) copied from the committed test fixtures. Whoever has `cargo-fuzz` and a
  nightly toolchain should run `cargo +nightly fuzz run <target>` from `crates/hs-media/fuzz/`
  before trusting the decoders/parser against real adversarial input beyond this session's fixed
  corpus.
- **`crates/hs-media/benches/thumbnails.rs`**: a real Criterion benchmark (the stub `fn main() {}`
  replaced), generating a non-trivial synthetic JPEG source (1920x1080, high-frequency content so
  JPEG's DCT does real work) and benchmarking every one of the five default sizes plus one dynamic
  1600x1600 scale. Actually run this session (`cargo bench -p hs-media --bench thumbnails --
  --sample-size 10 --measurement-time 1`, a reduced-sample invocation appropriate for a shared,
  contended sandbox machine — not a recorded perf baseline, just proof the benchmark executes
  correctly end to end and produces numbers for every size).
- **`docs/rfcs/0006-url-previews.md`**: design (not implemented) for `GET .../preview_url` —
  threat model (SSRF, DNS rebinding, redirect-based blocklist bypass, resource exhaustion,
  userinfo credential leakage), IP-range-blocklist-against-resolved-addresses, DNS pinning,
  manual re-checked redirect following, bounded fetch, caching. Proposes a shared `crate::fetch`
  primitive RFC 0007 also uses.
- **`docs/rfcs/0007-federation-media.md`**: design (not implemented, blocked on track 06) for
  fetching remote media over federation — a `FederationMediaClient` trait this crate needs from
  track 06, the multipart/redirect/legacy-proxy fetch flow, reuse of RFC 0006's fetch primitive
  for SSRF defense against a malicious/compromised origin's redirect, the remote cache (no schema
  change needed — see `metadata.rs` above), and a note that this crate will also eventually need a
  multipart *builder* (only a parser exists today) to serve federation media requests itself.
- 160 tests total (148 unit + 11 image-corpus integration + 1 S3 integration), `cargo fmt -p
  hs-media` and `cargo clippy -p hs-media --all-targets -- -D warnings` both clean.

## In progress

Nothing left mid-flight; this session's scope is complete and green.

## Next

In the brief's stated priority order:

- **URL previews** (RFC 0006 exists; no code). Needs `reqwest` added to `hs-media`'s own
  `Cargo.toml` (already an `[workspace.dependencies]` entry, added by track 15 — no workspace
  change needed) plus a DNS resolver capable of returning individual IPs before connecting
  (not yet in the workspace).
- **Remote media fetching over federation** (RFC 0007 exists; no code) — blocked on track 06's
  federation client existing, per this track's own brief and the dependency graph in
  `docs/workstreams/README.md`.
- **Quarantine and retention as admin operations**: `MediaRepository::set_quarantined` exists and
  is tested; no admin HTTP endpoint calls it yet (track 15's admin API surface). A
  `remote_media_retention`-driven eviction sweep is unimplemented (the config field exists; no
  background job runs it — background-job leasing is track 03/12's infrastructure, this crate
  would only supply the sweep logic once that exists).
- **Direct media for bridges** (track 11 collaboration) — not started; needs track 11's real
  `AppserviceRegistry` (today only `hs_auth::appservice::InMemoryAppserviceRegistry` exists) and,
  per `PLAN.md` 8.4, an embedded mini federation server for `hs-bridge-conformance` to test
  against, which itself depends on RFC 0007.
- **Federation media endpoints this server serves** (as opposed to fetches) — not implemented;
  needs X-Matrix request verification from track 06/07 and the multipart *builder* noted in RFC
  0007 section 3.5.
- **Admin media endpoints** (list, delete, purge, quarantine by room/user) — not started, track 15
  collaboration.
- **Metrics** — not started; would use `hs-telemetry` once this crate's HTTP surface is actually
  mounted by a real listener.
- **GCS and Azure backends** — the `gcp`/`azure` Cargo features exist (inherited from the
  scaffold) but `store::build` returns a clean "not implemented" error for both; only local
  filesystem and S3-compatible were this session's scope.
- **Fuzzing**: run the three targets in `crates/hs-media/fuzz/` for real, for hours/overnight, with
  `cargo-fuzz` and a nightly toolchain (see "Done" above for why this session could not).
- **Pre-generation of thumbnails at upload time** was one of the brief's "open questions to settle
  first"; this session settled it as on-demand-generate-then-cache (see "Decisions made"), so
  pre-generation is not planned unless a later session revisits that decision.

## Blockers

None for this session's delivered scope. Remote media fetching (RFC 0007) is blocked on track 06
existing, as the brief itself anticipates.

## Interfaces provided

- **`hs_media::security`**: `is_inline_safe`, `disposition_kind`, `content_disposition`,
  `response_headers`, `parse_range`/`ByteRange`/`RangeOutcome`, `CSP_HEADER_VALUE`,
  `INLINE_SAFE_CONTENT_TYPES`. Anything serving media-like content (avatars, previews) should route
  its response headers through `response_headers` rather than reinventing the inline/attachment or
  CSP decision.
- **`hs_media::{MediaRepository, MediaState, MediaRequester, MediaError, MediaId}`**: the crate's
  main surface, generic over `B: hs_kv::KvBackend`.
- **`hs_media::router::{authenticated_router, legacy_router}`**: spec-relative `axum::Router`
  fragments plus `hs_http::router::RouteManifest`, ready for whichever crate owns the real client
  listener to mount under `/_matrix/client/v1/media` and `/_matrix/media/v3` with a concrete
  `MediaState<B>` supplied via `.with_state(...)`.
- **`hs_media::synapse_layout`**: `SynapseMediaStore`, `SynapseMediaEntry`,
  `classify_relative_path` — track 13's importer's read-only source for a Synapse media store on
  disk. `docs/compat/synapse-importer-mapping.md` (the policy for what the importer does with what
  this finds — conflict handling, ID remapping) does not exist yet; per this track's instructions,
  writing it is track 13's job, not this crate's — this module is ready to be consumed by it.
- **`hs_media::multipart`**: `parse`, `boundary_from_content_type`, `Part`, `MultipartMixed` — a
  federation-client-independent building block, ready for track 06 (see RFC 0007).
- **`docs/rfcs/0006-url-previews.md`**, **`docs/rfcs/0007-federation-media.md`**: design documents,
  open for review by their listed consumer tracks.

## Interfaces needed

- **06 (federation)**: the `FederationMediaClient` trait RFC 0007 section 3.1 proposes — needed
  before remote media fetching can be implemented at all.
- **07 (auth)**: consumed today via `hs_auth::{state::AuthState, requester::Requester,
  middleware}` — no changes needed, but see "Decisions made" for the `MediaRequester` bridge this
  crate had to add because `Requester: FromRequestParts<AuthState>` is not generic over caller
  state.
- **11 (appservices/bridges)**: the real `AppserviceRegistry` and, per `PLAN.md` 8.4, an embedded
  mini federation server, both needed for direct media.
- **13 (config/compat)**: `docs/compat/synapse-importer-mapping.md` (the importer policy that
  consumes `hs_media::synapse_layout`); the Synapse `homeserver.yaml` mapping for the new
  `MediaConfig` fields RFC 0006 proposes (`url_preview_timeout`, `url_preview_max_fetch_size`,
  `url_preview_cache_lifetime`), once implemented.
- **14 (test/conformance)**: Complement media tests and differential tests against Synapse 1.161
  for headers and thumbnail dimensions (this track's definition of done) — not yet run against
  this crate; this session's 160 tests are this crate's own, not Complement.
- **15 (admin API)**: admin media endpoints (list/delete/purge/quarantine) calling into
  `MediaRepository`.

## Decisions made

- **Media IDs match Synapse's shape exactly (24 random alphanumeric characters)** so imported
  Synapse media IDs and freshly generated ones are indistinguishable, mirroring track 07's same
  decision for token shapes. `MediaId::parse` (for externally supplied IDs — remote servers,
  imported data) is deliberately more permissive than `MediaId::generate`'s own output shape, only
  rejecting what would be unsafe as a path/object-store-key component. See `crate::id`'s module
  doc.
- **`MediaRequester`: a thin per-crate bridge, not a generic `hs-auth` extractor.**
  `hs_auth::requester::Requester` implements `FromRequestParts<AuthState>` for the concrete
  `AuthState` type (track 07 deliberately left mounting/composition to whichever crate owns a
  listener — see `docs/status/07-auth-and-identity.md`'s "Interfaces needed"). Since this crate's
  router state is `MediaState<B>`, not `AuthState`, a handler cannot take `Requester` directly.
  The fix: embed `AuthState` in `MediaState<B>`, implement `FromRef<MediaState<B>> for AuthState`
  (trivial — every field is an `Arc`), and define `MediaRequester(pub Requester)` implementing
  `FromRequestParts<MediaState<B>>` by building an `AuthState` via `FromRef` and delegating. Cheap,
  and the same pattern works for any crate in the same position — recorded here so another track
  hitting the identical issue does not have to rediscover it. `MediaError::Auth` wraps
  `hs_auth::error::MatrixError` (a distinct type from `hs_http::MatrixError` — `hs-auth` predates
  `hs-http`) verbatim so the real status/errcode reach the client rather than collapsing every auth
  failure into a generic `400`.
- **Thumbnails are generated on demand and cached, not pre-generated at upload time.** One of the
  brief's "open questions to settle first". On-demand matches Synapse's default and keeps upload
  latency independent of how many thumbnail sizes are configured; the cache (`MetadataStore` +
  object-store) makes the second request for any given size free regardless.
- **Thumbnail output is always PNG or JPEG, never WebP or AVIF**, even though this crate can
  *decode* WebP (`docs/workstreams/09-media.md`'s "Open questions to settle first" also asked about
  WebP/AVIF *output*). PNG for alpha-capable sources (PNG, GIF, WebP), JPEG otherwise — Synapse's
  rule, behavior only, `crate::thumbnail::output_format_for`. Revisit if a differential test against
  Synapse 1.161 (track 14) shows it now emits WebP thumbnails in some configuration; not observed
  in the reference source read this session (`refs/synapse/synapse/media/thumbnailer.py` was not
  read in full — this decision is based on the crate's own output-format simplicity, not a
  byte-for-byte Synapse behavior claim, unlike the on-disk layout in `synapse_layout.rs`, which
  *is* read directly from `refs/synapse/synapse/media/filepath.py`).
- **Animated sources always thumbnail to a single static frame** (first frame only) — matches
  Synapse's observed behavior and avoids the CPU/memory cost of thumbnailing every frame of a
  large animated GIF a client will only ever see as a static preview.
  `crate::sniff::decode_with_limits`'s doc explains why this falls out naturally from using
  `ImageDecoder`'s ordinary (non-`AnimationDecoder`) path.
- **Quarantine is indistinguishable from not-found to a non-admin caller** (`MediaError::Quarantined`
  maps to the identical `404 M_NOT_FOUND` shape as `MediaError::NotFound` — see
  `crate::error::tests::quarantined_media_looks_identical_to_not_found`), so quarantine state is
  never leaked to a caller checking whether some `mxc://` URI resolves.
- **Legacy media routes are frozen, not simply always-on.** `MediaState::legacy_freeze_ms` (`None`
  by default — matching `allow_legacy_unauthenticated_media: true` with no freeze date, a
  permissive default a real deployment can tighten once it sets a real cutover) — anything created
  at or after the configured instant is `404` on the legacy, unauthenticated path even though it
  remains fully servable on the authenticated one. See `crate::routes::legacy`'s module doc for the
  full rationale (without this, the authenticated-media requirement is pointless — any client can
  fall back to the always-unauthenticated legacy path).
- **`hs-kv`/`hs-tables` calls inside `MetadataStore` are synchronous, called directly from async
  handlers, not wrapped in `tokio::task::spawn_blocking`.** `hs-kv` transactions are deliberately
  synchronous by that crate's own design ("keep transactions short" — no `.await` inside a
  transaction body). For `MemoryBackend` (what this session's tests run against, per
  `docs/workstreams/README.md` rule 2) this is effectively free. A production `FjallBackend` caller
  on the hot HTTP path may want `spawn_blocking` if profiling shows contention; not done this
  session since no production listener exists yet to profile. Noted in `metadata.rs`'s module doc
  as a flagged follow-up, not a silent gap.
- **`futures`, `hex` and `http-body-util`, present in the crate's Cargo.toml scaffold before this
  session, were removed** (unused by anything written this session — verified by grep before
  removing) to keep the dependency tree lean per this track's instructions. `http` was kept even
  though this crate only ever reaches it through `axum::http` today, since header-type code outside
  an axum handler (the RFC 0006/0007 fetch primitive, notably) is likely to want it directly.

## Shared dependencies added

None. Every dependency this session's code uses (`object_store` with its already-enabled `aws`
feature, `image` with its already-enabled `jpeg`/`png`/`gif`/`webp` features, `ruma`, `crc32fast`,
`tempfile`, `criterion`) was already present in the root `Cargo.toml`'s `[workspace.dependencies]`
before this session started (`crc32fast` and `ruma` added by tracks 07 and 02/07 respectively, per
their own status files) — this session only added ordinary path dependencies on `hs-kv`,
`hs-tables`, `hs-auth`, `hs-http` and `hs-config` to `crates/hs-media/Cargo.toml` itself (in-tree
crates, not workspace-level entries) plus `libfuzzer-sys` inside the standalone
`crates/hs-media/fuzz/Cargo.toml`, which is its own separate `[workspace]` by design (see "Done")
and therefore outside the root workspace's dependency table entirely.
