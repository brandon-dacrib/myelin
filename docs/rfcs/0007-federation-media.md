# RFC 0007. Remote media fetching over federation: multipart responses, redirects, and the remote cache

Status: proposed, design only — blocked on track 06's federation client existing. Owner: track 09
(media). Consumers: track 06 (the client this design's `RemoteMediaFetcher` trait is written
against), track 13 (Synapse remote-media-cache import), track 14 (bridge direct-media
conformance, `hs-bridge-conformance`).

Companion artifacts: `crates/hs-media/src/multipart.rs` (the `multipart/mixed` parser this design
already has, fuzzed — `crates/hs-media/fuzz/fuzz_targets/multipart_parse.rs`),
`crates/hs-media/src/metadata.rs` (`MediaRecord::server_name` is already origin-agnostic: a local
upload's `server_name` is this server's own name, a remote-cached item's is the origin's — no
schema change needed for this RFC), `docs/rfcs/0006-url-previews.md` section 4 (the DNS-pinning
fetch primitive this design reuses for following a redirect), `PLAN.md` section 8.4 ("Federation
endpoints the bridge itself serves for direct media" — the direct-media conformance surface this
design must also satisfy).

## 1. Motivation

When a room references media (`mxc://origin.example/abc123`) uploaded to a server other than this
one, this server must fetch it from `origin.example` to serve it locally (`GET
.../download/{serverName}/{mediaId}` where `serverName != this server`). Since spec v1.11
(MSC3916), the correct path is the federation media API —
`GET /_matrix/federation/v1/media/download/{mediaId}` and `.../thumbnail/{mediaId}`, both X-Matrix
signed requests — which replies with either a `multipart/mixed` body (a JSON metadata part plus
the content part) or a redirect to a URL the content can be fetched from directly (typically a CDN
the origin server offloads bandwidth to). A server that predates v1.11 does not implement this
endpoint at all; this server must still be able to fetch its media, via the older, unauthenticated
`GET /_matrix/media/v3/download/{serverName}/{mediaId}` proxy path.

This RFC is written now, ahead of track 06's federation client (which owns X-Matrix request
signing, `.well-known` discovery and server key verification — the pieces an authenticated
federation request needs that have nothing to do with media specifically), so that:

1. `crate::multipart` (the response parser) is already built, tested and fuzzed against hostile
   input today, independent of having a caller yet — a malicious or compromised remote homeserver
   is exactly the attacker `docs/workstreams/09-media.md`'s "Risks" section names, and that parser
   is real, not a stub (`crates/hs-media/src/multipart.rs`'s module doc has the fuzz-safety
   argument).
2. The trait boundary between "what track 06's client provides" and "what this crate needs" is
   decided in writing before either side has to guess at the other's shape.

## 2. Threat model

**In scope:**

- **A malicious or compromised origin server serves a malformed `multipart/mixed` body** to crash
  or hang this server's parser. Defense: `crate::multipart::parse`'s bounded-iteration argument
  (module doc) plus `fuzz/fuzz_targets/multipart_parse.rs`.
- **A malicious or compromised origin server redirects the fetch to an internal address** on
  *this* server's network (SSRF via federation media redirect — a variant of RFC 0006's threat
  model, but the untrusted party here is a peer homeserver, not a client). Defense: redirects are
  followed through the exact same DNS-pinned, IP-blocklist-checked, hop-capped fetch primitive RFC
  0006 section 4 defines (`crate::fetch`, shared) — a federation redirect gets no more trust than
  a URL-preview redirect, deliberately.
- **A malicious or compromised origin server serves an oversized or decompression-bomb-shaped
  file** as "someone's avatar". Defense: the same streaming size cap and
  `crate::sniff::DecodeLimits` this crate already applies to local uploads and URL-preview images
  apply identically here — remote-sourced bytes are never treated as more trusted than
  locally-uploaded ones.
- **An origin server is slow, unreachable, or intermittently failing**, and every room member's
  client re-requesting the same dead `mxc://` URI turns into a fetch storm against it. Defense: a
  per-origin-server failure/backoff state (section 4.4) — coordinated with, not duplicated from,
  track 06's general federation retry policy.
- **A pre-1.11 server's unauthenticated legacy media proxy is abused as an open relay** (this
  server fetching *arbitrary* attacker-chosen content through it, since the legacy path takes no
  server-to-server authentication). Out of this crate's control on the far end, but this server
  only ever calls that path with a `(serverName, mediaId)` this server itself needs to resolve — it
  is never handed an arbitrary URL to relay, so it cannot itself be turned into an open relay by a
  client.

**Out of scope:** verifying the *content* of remote media is what a room's state claims it is
(there is no such claim in the spec — media IDs are opaque, unlike event content hashes); that is
a non-goal shared with Synapse.

## 3. Design

### 3.1 The trait this crate needs from track 06

```rust
/// What `hs-media` needs from the federation client to fetch remote media. Implemented by
/// track 06's client; `hs-media` depends only on this trait, not on `hs-federation` directly,
/// so the two tracks can iterate independently until this is frozen.
#[async_trait::async_trait]
pub trait FederationMediaClient: Send + Sync {
    /// Performs `GET /_matrix/federation/v1/media/download/{mediaId}` (or `.../thumbnail/{mediaId}`
    /// with the size/method query parameters, for a remote thumbnail request this server has not
    /// cached), X-Matrix signed, against `origin`. Returns the raw response: this crate is
    /// responsible for parsing the `multipart/mixed` body or following a redirect — see
    /// `FederationMediaResponse` below — since only it knows the size/decode limits to apply.
    async fn fetch_media(
        &self,
        origin: &ruma::ServerName,
        media_id: &str,
        thumbnail: Option<ThumbnailRequest>,
    ) -> Result<FederationMediaResponse, FederationFetchError>;
}

/// A `ThumbnailRequest`'s fields mirror `GET .../thumbnail`'s query parameters exactly
/// (`crate::thumbnail::{ThumbnailMethod, parse_method}` already model these).
pub struct ThumbnailRequest { pub width: u32, pub height: u32, pub method: crate::thumbnail::ThumbnailMethod }

/// Either the response body (for this crate to parse as `multipart/mixed`) or a redirect target
/// (for this crate to fetch via the shared DNS-pinned primitive, RFC 0006 section 4) — track 06's
/// client does not itself decide which; MSC3916 lets the origin server choose per request.
pub enum FederationMediaResponse {
    Multipart { content_type: String, body: bytes::Bytes },
    Redirect { location: String },
    /// The origin returned 404/`M_NOT_FOUND`, or does not implement this endpoint at all (older
    /// than v1.11) — the caller falls back to the legacy proxy path (section 3.3).
    NotFoundOrUnsupported,
}
```

`FederationFetchError` covers transport failure, X-Matrix verification failure, and timeout —
track 06's own error type, not redefined here.

### 3.2 The fetch flow

1. Call `FederationMediaClient::fetch_media`.
2. `Multipart { content_type, body }`: extract the boundary
   (`crate::multipart::boundary_from_content_type`), parse (`crate::multipart::parse`), take the
   second part's bytes as the content and its `Content-Type` header as the declared type (the
   first part's small JSON metadata object is currently defined by the spec to carry nothing this
   crate needs yet — reserved for future use, per MSC3916).
3. `Redirect { location }`: fetch `location` through the shared DNS-pinned primitive (RFC 0006
   section 4.2–4.4), capped the same way (5 hops, byte limit, timeout).
4. `NotFoundOrUnsupported`: fall back to `GET
   https://{origin}/_matrix/media/v3/download/{serverName}/{mediaId}` (the legacy, unauthenticated
   proxy path — still real traffic on real deployments, since not every federated server has
   upgraded) through the same shared fetch primitive (it is exactly as untrusted as any other
   external URL, so it gets exactly the same defenses).
5. Whatever bytes result, run them through `crate::sniff::decode_with_limits` at first-thumbnail
   time exactly as for local media (never at fetch time unconditionally — a remote PDF or video
   this server never thumbnails should not pay a decode cost it does not need), and store them via
   `crate::repository::MediaRepository` under `(origin_server_name, media_id)` — the schema
   already supports this (`crate::metadata::MediaRecord::server_name`).

### 3.3 Legacy proxy fallback

`GET /_matrix/media/v3/download/{serverName}/{mediaId}` on the origin, no X-Matrix signing (this
is the pre-1.11 shape every homeserver has always implemented for its own legacy clients, and
which other pre-1.11 servers relied on for exactly this cross-server fetch). Subject to the same
size/timeout/redirect defenses as everything else in this RFC. Track 13's Synapse-compatibility
work is the reference for exactly when a peer should be assumed pre-1.11 (a cached "does not
support the federation media API" flag per origin, refreshed occasionally, rather than a version
string comparison this server cannot verify independently).

### 3.4 The remote media cache

Fetched bytes are cached exactly like local media (same store, same metadata schema — the design
decision from `crates/hs-media/src/metadata.rs`'s module doc: "keyed the same way regardless of
origin" — was made specifically so this RFC needs no schema change). Eviction:
`hs_config::media::MediaConfig::remote_media_retention` (already exists, `None` = keep forever,
matching a `remote_media_lifetime` Synapse config already maps to). A background sweep (owned by
whichever track ends up running scheduled jobs — `PLAN.md`'s background-job leasing, likely track
03/12's concern, not this crate's) deletes cache rows and their object-store bytes past that
retention window; this RFC only defines *what* the retention policy governs, not the job runner.

### 3.5 Serving remote media *to* other servers (the other direction)

Out of this RFC's primary scope but noted for completeness: this server must also *respond* to
`GET /_matrix/federation/v1/media/{download,thumbnail}/...` for its own local media, which means
`crate::multipart` needs a *builder*, not just the parser this RFC has today. Tracked as a
follow-up in this crate's own status file, not blocked on track 06 (serving is this server's own
HTTP handler, symmetric to but independent of the fetching flow above) — likely implemented
alongside this RFC once track 06 exists to test the X-Matrix-authenticated request path
end-to-end.

## 4. Interfaces needed

- **track 06 (federation)**: `FederationMediaClient` (section 3.1) — the exact trait shape is
  this RFC's proposal; track 06 reviews and either accepts it or counter-proposes before either
  side implements against it, per this repository's RFC process.
- **RFC 0006 (this track, already proposed)**: the shared `crate::fetch` DNS-pinned primitive —
  implement once, used by both RFCs' redirect-following.
- **11 (appservices/bridges)**: direct media (`PLAN.md` section 8.4's bridge conformance surface)
  is a bridge acting as a tiny federation server; this RFC's fetch flow (section 3.2) is exactly
  what `hs-bridge-conformance` will exercise end to end once both this RFC and track 06 land.

## 5. Open questions for the implementing session

- Whether the per-origin failure/backoff state (threat model, "slow or unreachable origin") lives
  in this crate or is entirely track 06's general concern — leaning towards track 06 owning a
  generic per-origin circuit breaker every federation surface (not just media) benefits from,
  with this crate as one more caller of it, but not decided here.
- Exact `multipart/mixed` first-part JSON shape once the spec or a future MSC defines real content
  for it (currently `{}`) — `crate::multipart::Part::header`/`.body` already expose it generically,
  so no parser change is anticipated, only what this crate's fetch flow *does* with it.
