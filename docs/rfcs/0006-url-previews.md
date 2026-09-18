# RFC 0006. URL previews: OpenGraph, oEmbed, and the SSRF defenses that make fetching arbitrary URLs safe

Status: proposed, design only — no code yet. Owner: track 09 (media). Consumers: track 07 (whose
`Requester` gates the endpoint), track 13 (the `homeserver.yaml` translator's
`url_preview_*` options), track 14 (differential tests against Synapse's preview shape).

Companion artifacts: `crates/hs-media` (the crate this will live in, `crate::preview` once
implemented), `hs_config::media::MediaConfig` (`url_preview_enabled`,
`url_preview_ip_range_blocklist` — already exist), `docs/workstreams/09-media.md`'s "Risks"
("URL previews are ... a classic attack surface; ... previews are sandboxed by network policy").

## 1. Motivation

`GET .../preview_url?url=...` (spec: "Content repository", `url_preview_enabled`) makes this
homeserver fetch a URL a client supplied and return an OpenGraph-shaped JSON summary — title,
description, an image URL this server itself fetches and re-hosts. This is, structurally, a
server-side-request-forgery (SSRF) oracle by design: the server's own network position is used to
fetch a URL an unauthenticated-to-the-target-service, possibly malicious client chose. Every other
piece of this crate defends against a hostile *upload*; this is the one surface that defends
against a hostile *client request*, and it is why `docs/workstreams/09-media.md` calls previews out
by name as a classic attack surface alongside decoders. This RFC is the design; it is written now,
ahead of implementation, so the security properties are decided before any fetch code exists —
matching the "test corpus + fuzz targets before endpoints" ordering this track has followed
throughout.

Not implemented yet: this crate has no HTTP client dependency today (day-one scope was
object-store and image decoding only). Implementing this RFC adds `reqwest` (already an
`[workspace.dependencies]` entry, added by track 15) as a real dependency of `hs-media`, plus a
DNS resolver capable of returning individual resolved IPs before connecting (`hickory-resolver` or
equivalent — not yet in the workspace; note in the implementing session's status file).

## 2. Threat model

**In scope — what this design defends against:**

- **SSRF to internal infrastructure.** A client asks the server to preview
  `http://169.254.169.254/latest/meta-data/` (cloud instance metadata), `http://localhost:6379/`
  (an internal Redis, if one is ever added), or any RFC 1918 address behind the deployment's
  network perimeter. Defense: an IP-range blocklist checked against every resolved address, not the
  hostname (section 4.1).
- **DNS rebinding (TOCTOU).** A hostname resolves to a public IP when checked, then a *second*
  resolution at actual connect time (which most HTTP clients perform transparently) returns an
  internal IP the attacker's DNS server was waiting to serve. Defense: DNS pinning — resolve once,
  connect to that exact IP, never let the HTTP client re-resolve (section 4.2).
- **Blocklist bypass via redirect.** The initial URL passes every check; the server it points to
  responds `302 Location: http://169.254.169.254/`. Defense: redirects are followed manually, with
  the full IP-blocklist-plus-DNS-pinning check re-applied to every hop, capped at a small maximum
  (section 4.3) — never handed to the HTTP client's own auto-redirect-follow, which would skip the
  re-check entirely.
- **Resource exhaustion.** A slow-loris response, an unbounded response body, or a decompression
  bomb disguised as `og:image`. Defense: connect/read timeouts, a hard response-size cap enforced
  while streaming (not after buffering), and `crate::sniff::DecodeLimits` on any fetched image
  exactly as for an upload (section 4.4).
- **Credential leakage via embedded userinfo.** A URL of the form
  `http://admin:s3cr3t@internal-host/` — if naively passed to an HTTP client, some client
  libraries will send that `Authorization`-equivalent to whatever host the URL names. Defense: the
  userinfo component is stripped and the request is rejected outright if present (section 4.5) —
  Synapse does the same (observed 1.161 behavior, no code copied).
- **Content-type confusion in the fetched page/image.** The fetched HTML is parsed only for
  OpenGraph/oEmbed metadata (never executed, never rendered), and any fetched `og:image` goes
  through the exact same `crate::sniff`/`crate::security` pipeline as an upload (never served with
  a browser-trusted inline disposition if it turns out to be an SVG or HTML masquerading as an
  image — the adversarial fixtures in `crates/hs-media/tests/fixtures/images/` apply here too).

**Out of scope for this RFC** (left to the implementing session or a follow-up):
- Consent/GDPR UI for previews (Synapse has no server-side consent gate beyond
  `url_preview_enabled`; this design matches that scope).
- An oEmbed provider allowlist beyond "fetch whatever `<link rel="alternate"
  type="application/json+oembed">` the page itself advertises" (a curated provider list, as some
  clients maintain, is a quality improvement, not a security requirement, since the discovery URL
  is still subject to every defense in section 4).

## 3. The endpoint

`GET .../preview_url?url=<url>&ts=<optional timestamp>` on the same authenticated/legacy split as
the rest of this crate's media routes (`crate::router`): `client/v1/media/preview_url`
authenticated, `media/v3/preview_url` behind `allow_legacy_unauthenticated_media` — Synapse serves
`preview_url` unauthenticated on the legacy path today, and this crate's freeze semantics
(`crate::routes::legacy`) apply identically: a preview generated before the authenticated-media
cutover stays reachable on the legacy path, previews requested after it do not.

Response shape (spec, matching OpenGraph naming even for non-HTML sources): a JSON object with
`og:title`, `og:description`, `og:image` (an `mxc://` URI for a server-re-hosted, already
thumbnailed copy — never the original external URL, so a client is never told to fetch external
content directly), `og:image:width`/`og:image:height`, `matrix:image:size`. oEmbed-sourced
previews (`og:type` unavailable) map onto the same fields as closely as the oEmbed response allows.

## 4. Design

### 4.1 IP-range blocklist, checked against resolved addresses

`hs_config::media::MediaConfig::url_preview_ip_range_blocklist` already exists (CIDR list,
defaulted to RFC 1918 + link-local + loopback + CGNAT + IPv6 equivalents —
`crates/hs-config/src/media.rs`). The check happens after DNS resolution, against every address a
hostname resolves to (not just the first): if *any* resolved address falls inside the blocklist,
the whole preview request is refused (`M_UNKNOWN`, generic — never confirm or deny what the
blocked address was, to avoid the endpoint being usable as an internal-network port scanner by
timing/error-message differences).

### 4.2 DNS pinning

Resolve the hostname once, up front, to a list of IPs (both A and AAAA). Filter out blocked ones
(4.1). If none remain, refuse. Otherwise pick one (first-of-list is fine — no load-balancing
requirement here) and connect *to that literal IP*, setting the `Host` header (and TLS SNI, for
`https://`) to the original hostname. This is what makes the check in 4.1 mean anything: an HTTP
client that re-resolves DNS at connect time (the default behavior of most clients, including
`reqwest`'s connection pool under some configurations) would let an attacker's DNS server answer
differently the second time. `reqwest::ClientBuilder::resolve` (a static hostname-to-IP override)
is the mechanism: build a fresh client (or override) per request with the pinned IP, rather than a
long-lived shared client whose resolution behavior this design cannot fully control otherwise.

### 4.3 Manual, re-checked redirect following

`reqwest::redirect::Policy::none()` on the client; this code follows `3xx` responses itself, up to
a small cap (5, matching common browser/library defaults) — each hop repeats 4.1 and 4.2 in full
against the new `Location`, not just the first URL. A relative `Location` is resolved against the
current URL per RFC 7231 before the next hop's checks run. Exceeding the cap is a clean failure,
not an error that leaks how many hops occurred.

### 4.4 Bounded fetch

- Connect timeout and total request timeout, both configurable (new `MediaConfig` fields:
  `url_preview_timeout`, reusing `hs_config::Duration`).
- A byte cap on the response body (new field: `url_preview_max_fetch_size`, a sane default around
  10 MiB), enforced while streaming — the connection is dropped the instant the cap is exceeded,
  never after buffering the whole (attacker-controlled-length) body first.
- HTML is parsed for OpenGraph/oEmbed-discovery tags only from the first N KiB actually needed to
  reach `</head>` (most real pages put OpenGraph tags there) — not the whole document — bounding
  parse cost independent of the byte cap above.
- Any fetched `og:image` (or oEmbed `thumbnail_url`) is fetched through this exact same pipeline
  recursively (4.1 through 4.4 apply to it too — it is itself a URL from an untrusted source) and
  then decoded through `crate::sniff::decode_with_limits` before being thumbnailed and stored
  through `crate::repository::MediaRepository` under the `url_cache` layout Synapse also uses
  (`crate::synapse_layout`'s `UrlCache`/`UrlCacheThumbnail` kinds already model this on the read
  side for the importer; the write side is what this RFC adds).

### 4.5 Userinfo and scheme rejection

Before any DNS resolution: reject a URL whose scheme is not `http` or `https` (no `file://`,
`ftp://`, `gopher://`, ...), and reject a URL carrying a userinfo component
(`scheme://user:pass@host/...`) outright rather than silently stripping it — a client sending one
is either confused or testing for the credential-leak bug, and neither case benefits from the
server guessing what was meant.

### 4.6 Caching and concurrency

Cache key: the normalized request URL (scheme + host lowercased, default ports stripped, fragment
removed — query string kept, since it is often meaningful). Concurrent requests for the same URL
within the same fetch window share one in-flight fetch (a simple keyed mutex/notify, not a new
dependency) rather than each hitting the network — the same protection Synapse's
`_expiring_cache`-backed in-flight de-duplication provides, referenced here for behavior only.
Cache lifetime: a new `MediaConfig` field (`url_preview_cache_lifetime`, defaulting to a day or so,
matching Synapse's default) separate from `remote_media_retention` (which governs the *re-hosted
image*'s lifetime, already modeled).

## 5. Interfaces needed

- **track 06 (federation)**: none directly — URL previews are a client-facing feature, not a
  federation one. Listed here only because both this RFC and RFC 0007 add outbound-HTTP-fetch
  surface to `hs-media`, and the DNS-pinning/redirect-following primitive this RFC builds
  (section 4.2, 4.3) is exactly what RFC 0007 reuses for following a federation media redirect —
  implement it once, in a shared internal module (`crate::fetch`, proposed name), both RFCs depend
  on it.
- **track 13 (config/compat)**: the new `MediaConfig` fields this RFC proposes
  (`url_preview_timeout`, `url_preview_max_fetch_size`, `url_preview_cache_lifetime`) need entries
  in the Synapse `homeserver.yaml` translation table alongside the two that already exist.

## 6. Open questions for the implementing session

- Whether to maintain a curated oEmbed provider list (some clients do) or rely entirely on
  page-advertised discovery links — this RFC's security properties hold either way; it is a
  coverage/quality decision, not a safety one.
- Whether IPv6 zone identifiers and IPv4-mapped IPv6 addresses (`::ffff:10.0.0.1`) need explicit
  normalization before the blocklist check in 4.1 — likely yes, flagged here so it is not
  rediscovered as a bypass later.
