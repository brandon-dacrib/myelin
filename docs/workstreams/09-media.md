# 09. Media

Wave 1, starts day one. Independent of everything except the auth middleware and, later, the federation client.

**Expert profile.** Object storage, image processing, HTTP streaming and range requests, content security (SSRF, decoder attack surface).

**Mission.** An object-storage-first media repository with authenticated media as the primary path, Synapse-compatible behavior at the edges, and bridge direct media working. See `PLAN.md` section 4 (D6) and 8.1 item 4.

**Owns.** `hs-media`: the repository over `object_store` (local filesystem, S3-compatible, GCS, Azure), uploads (synchronous and async `create` plus `PUT`), downloads and thumbnails on the authenticated `client/v1/media` routes and the legacy routes behind a flag with the same freeze semantics Synapse has, thumbnail generation (crop and scale, sizes, formats, animated images), `Content-Disposition` and `Content-Security-Policy` rules, URL previews (OpenGraph and oEmbed, IP-range blocklists, DNS pinning, size limits), remote media fetching through 06 (multipart responses, redirects) and the remote cache with expiry, quarantine and protection, retention, per-user and per-server upload limits with the `m.upload.size` capability, media metadata tables with 01, the Synapse media-directory layout adapter for 13, direct-media conformance with 11, the federation media endpoints with 06, media admin endpoints with 15.

**Provides.** The media API used by avatars and previews; the layout adapter.

**Consumes.** 07 middleware, 06 client, 01, 13.

**Day-one work.** Repository and both `object_store` backends; upload, download and thumbnails on the `image` crate; decoder fuzz targets; a test corpus of images including malformed and adversarial files; the security rules (never serve SVG or HTML inline, size limits, range requests, content sniffing rules).

**Phase 0 deliverables.** Core repository, thumbnails, async upload, authenticated download, tests, thumbnail throughput benchmarks on arm64.

**Phase 1 and 2 deliverables.** URL previews, remote media and cache, quarantine and retention, admin endpoints, the Synapse layout adapter, direct media, federation media endpoints, upload limits, metrics.

**Definition of done.** Complement media tests green; fuzzers running; the bridge direct-media conformance test green; differential tests against Synapse for headers and thumbnail dimensions.

**References.** Spec media sections including authenticated media (v1.11) and async uploads (v1.7); `refs/synapse/synapse/media/` (thumbnailer semantics and `filepath.py` for the on-disk layout; behavior only, AGPL); `refs/palpo/crates/server/src/media/`; MSC3916, MSC2246.

**Open questions to settle first.** Pre-generation versus on-demand thumbnails; WebP and AVIF output; cache tiering on small hosts.

**Risks.** Decoders and URL previews are the two classic attack surfaces; both are fuzzed and previews are sandboxed by network policy in the cluster mode.
