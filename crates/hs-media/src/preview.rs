//! `GET .../preview_url` (spec: "Getting URL previews"): fetch a caller-supplied URL, extract
//! OpenGraph metadata, and (if it advertises an `og:image`) cache a local copy of that image so
//! the response's `og:image` is an `mxc://` URI like everything else this crate serves.
//!
//! # What this guards against, and what it does not (state precisely, per this crate's own
//! convention for a security control)
//!
//! This endpoint fetches a URL the *caller* names, on the server's behalf, from inside whatever
//! network the server itself can reach — the textbook server-side request forgery setup. What is
//! guarded here:
//!
//! - **Private/loopback/link-local address ranges.** [`PreviewIpPolicy`] blocks by CIDR, using
//!   `hs_config::MediaConfig::url_preview_ip_range_blocklist` (defaults: the same RFC 1918 /
//!   loopback / link-local / CGNAT / unique-local ranges `hs-federation`'s own
//!   `ip_range_blocklist` blocks for outbound federation requests — see [`PreviewIpPolicy`]'s doc
//!   for exactly why this crate does not reuse `hs_federation::client::IpPolicy`'s *type*
//!   verbatim while deliberately reusing its *design*).
//! - **DNS-rebinding / TOCTOU between the check and the connection.** [`guarded_fetch`] resolves
//!   the target host itself (`tokio::net::lookup_host`), checks *every* resolved address against
//!   the blocklist (refusing if *any* resolved address is blocked — stricter than checking only
//!   one, since the caller does not control which address wins a race), and then pins the actual
//!   TCP connection to that checked address via `reqwest::ClientBuilder::resolve` — the same
//!   resolve-then-pin shape `hs-federation`'s `client::Client::client_for` uses for its own
//!   outbound requests, so a second DNS answer returned only at connect time can never smuggle in
//!   an address that was never checked.
//! - **Redirects to a blocked address.** Automatic redirect-following is disabled
//!   (`redirect::Policy::none()`); [`guarded_fetch`] follows redirects itself, in a bounded loop,
//!   re-running the full resolve-and-check step for every hop's target host. A naive
//!   `reqwest::redirect::Policy::custom` callback cannot do this correctly — it fires before the
//!   redirect target's own DNS resolution, so it cannot see the address it is about to connect to
//!   any more than the initial, un-redirected request could.
//! - **An unbounded response.** [`FetchLimits::max_body_bytes`] caps the page fetch and the
//!   `og:image` fetch independently; a `Content-Length` over the cap is rejected before any body
//!   read, and a chunked/absent-length response is aborted mid-stream the moment the cap is
//!   crossed.
//! - **A slow or hanging target.** [`FetchLimits::timeout`] bounds each HTTP request (not the
//!   whole `preview_url` call end to end — a page fetch followed by an image fetch is two
//!   separately-timed requests).
//! - **Credential leakage via embedded userinfo.** A URL of the form
//!   `http://admin:s3cr3t@internal-host/` is rejected outright ([`FetchError::UserinfoNotAllowed`])
//!   rather than having its userinfo silently stripped — see that variant's doc.
//! - **Trusting the remote `Content-Type` for the cached image.** The fetched `og:image` bytes
//!   are sniffed with [`crate::sniff::sniff_format`] (the same decode-time verification every
//!   ordinary upload gets — see that module's doc) rather than trusted from the response header;
//!   bytes that do not sniff as a supported image format are dropped from the response instead of
//!   being cached and served under this server's own `mxc://` namespace.
//!
//! What is **not** guarded here, stated precisely rather than left implicit:
//!
//! - **The blocklist is best-effort CIDR matching, not a full network-topology model.** A
//!   deployment behind a NAT/proxy where "the public internet" and "an internal service" are not
//!   distinguishable by IP range alone (e.g. a cloud metadata endpoint reachable at a
//!   *non*-private-looking address, or a split-horizon DNS setup) is not defended against by this
//!   module; `url_preview_ip_range_blocklist` is an operator-configured list and it is the
//!   operator's job to include anything reachable from this deployment that should not be.
//! - **No content sanitization of `og:title`/`og:description` beyond decoding HTML entities.**
//!   These are returned as plain JSON strings; nothing here HTML-escapes or otherwise sandboxes
//!   them further, because they leave this crate as JSON, not as HTML this server itself renders
//!   — the client displaying them is responsible for treating them as untrusted text, exactly as
//!   it already must for any other user-supplied string in a Matrix response.
//! - **No protection against the target being slow-but-under-the-cap** (a "slow loris" style
//!   trickle that never crosses `max_body_bytes` before `timeout` fires) beyond the timeout
//!   itself; this is judged an acceptable, bounded cost (one blocked request for at most
//!   `FetchLimits::timeout`), not a resource-exhaustion vector, since concurrency here is bounded
//!   by ordinary HTTP server concurrency limits, not by anything specific to this endpoint.
//! - **No cross-server/cluster de-duplication of concurrent identical requests** ("thundering
//!   herd" on a newly-shared link) — [`crate::metadata::MetadataStore`]'s preview cache
//!   (populated only *after* a fetch completes) reduces repeat cost but does not prevent N
//!   concurrent first-time requests for the same URL from each doing their own fetch.
//!
//! # The cache
//!
//! Responses are cached by the SHA-256 hex of the requested URL
//! ([`crate::metadata::MetadataStore::get_preview_cache`]/`put_preview_cache`), for
//! [`DEFAULT_PREVIEW_CACHE_TTL_MS`]. The cache lives in the same durable metadata store as every
//! other media row (see that method's doc for the one stated limitation: no capacity bound yet).

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use ipnet::IpNet;
use object_store::{ObjectStore, ObjectStoreExt};
use serde_json::{Map, Value};

use hs_config::MediaConfig;
use hs_kv::KvBackend;

use crate::error::MediaError;
use crate::id::MediaId;
use crate::metadata::{MediaRecord, MetadataStore};
use crate::sniff::DecodeLimits;

/// How long a URL preview response stays cached before it is fetched again. The spec leaves this
/// entirely to the server; this is a conservative middle ground between "never refetch a link
/// people keep sharing" and "an operator changing their `og:title` sees it reflected same-day."
pub const DEFAULT_PREVIEW_CACHE_TTL_MS: u64 = 60 * 60 * 1000;

/// The SSRF blocklist: a list of blocked [`IpNet`] ranges, checked against every DNS-resolved
/// candidate address for a preview target (see the module doc's "DNS-rebinding" bullet).
///
/// Deliberately the same *design* as `hs_federation::client::IpPolicy` (parse a list of CIDR
/// strings, skip unparsable ones — `hs_config::MediaConfig::validate` is where a malformed CIDR
/// is rejected at config-load time, same division of responsibility `hs-federation` uses), built
/// from `hs_config::MediaConfig::url_preview_ip_range_blocklist` rather than
/// `FederationConfig::ip_range_blocklist` (a different config field, a deliberately *identical*
/// default range list — see `hs_config::media::default_preview_blocklist`). This crate does not
/// add a dependency on `hs-federation` for the ~15-line struct itself: `hs-federation`'s
/// `IpPolicy` also carries an `allowlist` half this crate's config has no equivalent for, and
/// federation's own struct is not `pub` re-exported for cross-crate reuse today — reusing the
/// *shape* here, in a crate `hs-federation` never depends on, was judged simpler than either
/// changing `hs-federation`'s public surface or pulling its whole dependency graph into `hs-media`
/// for one type. If a shared `hs-net-policy`-style crate is ever carved out, this is the type that
/// should move into it.
#[derive(Debug, Clone, Default)]
pub struct PreviewIpPolicy {
    blocklist: Vec<IpNet>,
}

impl PreviewIpPolicy {
    /// Parses CIDR strings, silently skipping any that do not parse (by the time this runs,
    /// `hs_config::MediaConfig::validate` is assumed to have already rejected malformed entries at
    /// config-load time; a defensive skip here is cheap insurance against a panic on bad input
    /// reaching this far, matching `hs_federation::client::IpPolicy::from_cidrs`'s exact same
    /// reasoning).
    #[must_use]
    pub fn from_cidrs(blocklist: &[String]) -> Self {
        Self {
            blocklist: blocklist.iter().filter_map(|s| s.parse().ok()).collect(),
        }
    }

    /// Whether `addr` may be connected to.
    #[must_use]
    pub fn allows(&self, addr: IpAddr) -> bool {
        !self.blocklist.iter().any(|net| net.contains(&addr))
    }
}

/// Bounds on one [`guarded_fetch`] call.
#[derive(Debug, Clone, Copy)]
pub struct FetchLimits {
    /// Maximum response body size, in bytes. Enforced against `Content-Length` up front (if
    /// present) and against the actual bytes read either way.
    pub max_body_bytes: usize,
    /// Per-request timeout (each redirect hop is a separate request, separately timed).
    pub timeout: Duration,
    /// Maximum number of redirects to follow before giving up.
    pub max_redirects: u8,
}

impl Default for FetchLimits {
    fn default() -> Self {
        Self {
            max_body_bytes: 10 * 1024 * 1024,
            timeout: Duration::from_secs(10),
            max_redirects: 5,
        }
    }
}

impl FetchLimits {
    /// The limits an operator configured, rather than this module's own defaults.
    ///
    /// `max_redirects` has no configuration field: it is a structural safety bound on how far one
    /// request may be chased, not a tuning knob, and Synapse has no equivalent setting either.
    /// The other two come from `media.url_preview_timeout` and `media.url_preview_max_fetch_size`,
    /// whose defaults deliberately match Synapse's (30s, 10 MiB) — so wiring this changes the
    /// effective timeout from this module's old hardcoded 10s to 30s, which is the documented
    /// intent, not a regression.
    #[must_use]
    pub fn from_config(config: &hs_config::MediaConfig) -> Self {
        Self {
            max_body_bytes: usize::try_from(config.url_preview_max_fetch_size.as_u64())
                .unwrap_or(usize::MAX),
            timeout: config.url_preview_timeout.into(),
            max_redirects: Self::default().max_redirects,
        }
    }
}

/// Why a [`guarded_fetch`] failed.
#[derive(Debug, thiserror::Error)]
pub enum FetchError {
    /// Not a syntactically valid URL.
    #[error("invalid URL: {0}")]
    InvalidUrl(String),
    /// A scheme other than `http`/`https`.
    #[error("only http and https URLs may be previewed")]
    UnsupportedScheme,
    /// A URL with no host component.
    #[error("URL has no host")]
    NoHost,
    /// DNS resolution failed or returned no addresses.
    #[error("could not resolve host `{0}`: {1}")]
    Dns(String, String),
    /// Every resolved address (or the literal IP address given) is in the blocklist.
    #[error("`{0}` resolves only to addresses outside the allowed range")]
    Blocked(String),
    /// The HTTP request itself failed (connect, TLS, I/O).
    #[error("request failed: {0}")]
    Request(String),
    /// The response exceeded [`FetchLimits::max_body_bytes`].
    #[error("response exceeded the {0} byte limit")]
    TooLarge(usize),
    /// More redirects than [`FetchLimits::max_redirects`].
    #[error("too many redirects")]
    TooManyRedirects,
    /// A redirect (`3xx`) with no usable `Location` header.
    #[error("redirect with no Location header")]
    RedirectWithoutLocation,
    /// A non-success status code.
    #[error("server returned status {0}")]
    BadStatus(u16),
    /// A URL carrying a userinfo component (`scheme://user:pass@host/...`) — rejected outright
    /// rather than silently stripped, per `docs/rfcs/0006-url-previews.md` section 4.5: a client
    /// sending one is either confused or testing for the credential-leak bug some HTTP client
    /// libraries have (forwarding that component as an `Authorization`-equivalent to whatever
    /// host the URL names), and neither case benefits from the server guessing what was meant.
    #[error("URLs with embedded credentials are not allowed")]
    UserinfoNotAllowed,
}

/// One successfully fetched resource.
#[derive(Debug, Clone)]
pub struct FetchedResource {
    /// The URL the content was ultimately fetched from (after following any redirects).
    pub final_url: String,
    /// The response's own `Content-Type` header, if any (never trusted for anything security
    /// relevant — see the module doc's "trusting the remote Content-Type" bullet).
    pub content_type: Option<String>,
    /// The response body.
    pub bytes: Bytes,
}

/// Fetches `url`, enforcing every guard described in the module doc: scheme, resolved-address
/// blocklist (re-checked on every redirect hop, with the connection pinned to the address that
/// was actually checked), a bounded number of redirects, a body size cap and a per-request
/// timeout.
///
/// # Errors
/// See [`FetchError`].
pub async fn guarded_fetch(
    url: &str,
    policy: &PreviewIpPolicy,
    limits: &FetchLimits,
) -> Result<FetchedResource, FetchError> {
    let mut current = url.to_string();
    for _hop in 0..=limits.max_redirects {
        let parsed =
            reqwest::Url::parse(&current).map_err(|e| FetchError::InvalidUrl(e.to_string()))?;
        if parsed.scheme() != "http" && parsed.scheme() != "https" {
            return Err(FetchError::UnsupportedScheme);
        }
        if !parsed.username().is_empty() || parsed.password().is_some() {
            return Err(FetchError::UserinfoNotAllowed);
        }
        let host = parsed.host_str().ok_or(FetchError::NoHost)?.to_string();
        let port = parsed
            .port_or_known_default()
            .ok_or(FetchError::UnsupportedScheme)?;

        let connect_addrs: Vec<SocketAddr> = resolve_and_check(&host, port, policy)
            .await?
            .into_iter()
            .map(|ip| SocketAddr::new(ip, port))
            .collect();

        // Built per request because the checked addresses are pinned per host; the shared
        // builder keeps that from re-reading the root store every time. Every checked address
        // is pinned, not the first, so the connector can fall back across them (and the
        // outbound address policy drops the IPv6 ones when it is IPv4 only).
        let client = hs_http::client::pinned_builder(&host, &connect_addrs)
            .timeout(limits.timeout)
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|e| FetchError::Request(e.to_string()))?;

        let response = client
            .get(parsed.clone())
            .send()
            .await
            .map_err(|e| FetchError::Request(e.to_string()))?;

        let status = response.status();
        if status.is_redirection() {
            let location = response
                .headers()
                .get(reqwest::header::LOCATION)
                .and_then(|v| v.to_str().ok())
                .ok_or(FetchError::RedirectWithoutLocation)?;
            let next = parsed
                .join(location)
                .map_err(|e| FetchError::InvalidUrl(e.to_string()))?;
            current = next.to_string();
            continue;
        }
        if !status.is_success() {
            return Err(FetchError::BadStatus(status.as_u16()));
        }

        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        let final_url = response.url().to_string();
        let bytes = read_capped(response, limits.max_body_bytes).await?;
        return Ok(FetchedResource {
            final_url,
            content_type,
            bytes,
        });
    }
    Err(FetchError::TooManyRedirects)
}

/// Resolves `host` (or parses it directly if it is already an IP literal), checks *every*
/// candidate address against `policy`, and returns one allowed address to pin the connection to.
/// Refuses if *any* resolved address is blocked, not just if all are — see the module doc.
/// Every address `host` resolves to, in the resolver's order, once every one of them passed
/// `policy`; the caller pins its connection to exactly these.
async fn resolve_and_check(
    host: &str,
    port: u16,
    policy: &PreviewIpPolicy,
) -> Result<Vec<IpAddr>, FetchError> {
    let candidates: Vec<IpAddr> = if let Ok(literal) = host.parse::<IpAddr>() {
        vec![literal]
    } else {
        tokio::net::lookup_host((host, port))
            .await
            .map_err(|e| FetchError::Dns(host.to_string(), e.to_string()))?
            .map(|addr| addr.ip())
            .collect()
    };
    if candidates.is_empty() {
        return Err(FetchError::Dns(host.to_string(), "no addresses".into()));
    }
    if !candidates.iter().all(|ip| policy.allows(*ip)) {
        return Err(FetchError::Blocked(host.to_string()));
    }
    Ok(candidates)
}

async fn read_capped(mut response: reqwest::Response, cap: usize) -> Result<Bytes, FetchError> {
    if let Some(len) = response.content_length()
        && len as usize > cap
    {
        return Err(FetchError::TooLarge(cap));
    }
    let mut buf: Vec<u8> = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|e| FetchError::Request(e.to_string()))?
    {
        buf.extend_from_slice(&chunk);
        if buf.len() > cap {
            return Err(FetchError::TooLarge(cap));
        }
    }
    Ok(Bytes::from(buf))
}

/// Synapse's `OG_TAG_NAME_MAXLEN`: a longer tag name is dropped from a preview.
pub const OG_TAG_NAME_MAXLEN: usize = 50;
/// Synapse's `OG_TAG_VALUE_MAXLEN`: a longer value is dropped from a preview.
pub const OG_TAG_VALUE_MAXLEN: usize = 1000;
/// How many `og:` tags one page may contribute, as Synapse caps it (`_get_meta_tags`).
const MAX_OG_TAGS: usize = 50;

/// OpenGraph metadata pulled out of a fetched HTML page.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OgTags {
    /// `og:title`, falling back to `<title>` if no `og:title` meta tag was present.
    pub title: Option<String>,
    /// `og:description`.
    pub description: Option<String>,
    /// `og:image`, resolved to an absolute URL against the page's own URL if it was relative.
    pub image: Option<String>,
    /// Every other `og:` property the page sets (`og:type`, `og:url`, `og:site_name`, ...),
    /// entity-decoded, first occurrence of each, at most 50: a preview carries them as they are,
    /// as Synapse's does. The `og:image:` ones describe the remote image and are replaced when
    /// the image is cached here.
    pub other: std::collections::BTreeMap<String, String>,
}

/// Extracts [`OgTags`] from `html`. A deliberately lenient, attribute-order-independent
/// `<meta>`/`<title>` scan (see the module doc for why this is not a full HTML/DOM parse).
#[must_use]
pub fn extract_og_tags(html: &str, base_url: &str) -> OgTags {
    let mut title = None;
    let mut description = None;
    let mut image = None;
    let mut other = std::collections::BTreeMap::new();

    for tag in META_TAG_RE.find_iter(html) {
        let attrs = parse_attrs(tag.as_str());
        let key = attrs
            .get("property")
            .or_else(|| attrs.get("name"))
            .map(|s| s.to_ascii_lowercase());
        let Some(content) = attrs.get("content") else {
            continue;
        };
        match key.as_deref() {
            Some("og:title") if title.is_none() => title = Some(decode_entities(content)),
            Some("og:description") if description.is_none() => {
                description = Some(decode_entities(content));
            }
            Some("og:image") if image.is_none() => {
                let raw = decode_entities(content);
                image = Some(resolve_url(base_url, &raw).unwrap_or(raw));
            }
            // A second `og:title`/`og:description`/`og:image`: the first one stands.
            Some("og:title" | "og:description" | "og:image") => {}
            Some(key)
                if key.starts_with("og:")
                    && other.len() < MAX_OG_TAGS
                    && !other.contains_key(key) =>
            {
                other.insert(key.to_owned(), decode_entities(content));
            }
            _ => {}
        }
    }

    if title.is_none()
        && let Some(caps) = TITLE_TAG_RE.captures(html)
    {
        let text = caps.get(1).map_or("", |m| m.as_str()).trim();
        if !text.is_empty() {
            title = Some(decode_entities(text));
        }
    }

    OgTags {
        title,
        description,
        image,
        other,
    }
}

fn resolve_url(base: &str, maybe_relative: &str) -> Option<String> {
    let base = reqwest::Url::parse(base).ok()?;
    base.join(maybe_relative).ok().map(|u| u.to_string())
}

fn parse_attrs(tag: &str) -> HashMap<String, String> {
    let mut out = HashMap::new();
    for caps in ATTR_RE.captures_iter(tag) {
        let (name, value) = if let (Some(n), Some(v)) = (caps.get(1), caps.get(2)) {
            (n, v)
        } else if let (Some(n), Some(v)) = (caps.get(3), caps.get(4)) {
            (n, v)
        } else {
            continue;
        };
        out.insert(
            name.as_str().to_ascii_lowercase(),
            value.as_str().to_string(),
        );
    }
    out
}

/// Decodes the handful of HTML entities that actually show up in OG tag content: the five
/// predefined XML entities plus decimal/hex numeric character references. Not a general HTML
/// entity table (`&nbsp;`, `&copy;`, ...) — good enough for the common case, and an unrecognized
/// named entity is left as literal text rather than silently dropped.
fn decode_entities(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        let tail = &rest[amp..];
        let Some(semi) = tail.find(';') else {
            out.push_str(tail);
            rest = "";
            break;
        };
        let entity = &tail[1..semi];
        let replacement = match entity {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" | "#39" | "#x27" | "#X27" => Some('\''),
            _ if entity.starts_with('#') => {
                let (digits, radix) = if let Some(hex) = entity[1..].strip_prefix(['x', 'X']) {
                    (hex, 16)
                } else {
                    (&entity[1..], 10)
                };
                u32::from_str_radix(digits, radix)
                    .ok()
                    .and_then(char::from_u32)
            }
            _ => None,
        };
        match replacement {
            Some(c) => out.push(c),
            None => out.push_str(&tail[..=semi.min(tail.len() - 1)]),
        }
        rest = &tail[semi + 1..];
    }
    out.push_str(rest);
    out
}

static META_TAG_RE: std::sync::LazyLock<regex::Regex> =
    std::sync::LazyLock::new(|| regex::Regex::new(r"(?is)<meta\b[^>]*>").unwrap());
static TITLE_TAG_RE: std::sync::LazyLock<regex::Regex> =
    std::sync::LazyLock::new(|| regex::Regex::new(r"(?is)<title[^>]*>(.*?)</title>").unwrap());
static ATTR_RE: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
    regex::Regex::new(r#"(?is)([a-zA-Z][\w:-]*)\s*=\s*"([^"]*)"|([a-zA-Z][\w:-]*)\s*=\s*'([^']*)'"#)
        .unwrap()
});

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let digest = hasher.finalize();
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

/// Builds a `preview_url` response for `url`: fetch, parse, cache the `og:image` locally if
/// present, and cache the whole response for [`DEFAULT_PREVIEW_CACHE_TTL_MS`]. This is
/// [`crate::repository::MediaRepository::preview_url`]'s implementation, split out into a free
/// function so it can be tested (and reasoned about) independent of the rest of the repository's
/// upload/download surface.
///
/// # Errors
/// [`MediaError::PreviewDisabled`] if previews are turned off; [`MediaError::PreviewBlocked`] or
/// [`MediaError::PreviewFetchFailed`] as [`guarded_fetch`] fails; [`MediaError::Metadata`] on a
/// cache-store backend failure.
#[allow(clippy::too_many_arguments)]
pub async fn preview_url<B: KvBackend>(
    metadata: &MetadataStore<B>,
    object_store: &Arc<dyn ObjectStore>,
    server_name: &str,
    config: &MediaConfig,
    now_ms: u64,
    url: &str,
) -> Result<Value, MediaError> {
    if !config.url_preview_enabled {
        return Err(MediaError::PreviewDisabled);
    }
    let cache_key = sha256_hex(url.as_bytes());
    if let Some(cached) = metadata.get_preview_cache(&cache_key, now_ms)? {
        return serde_json::from_str(&cached)
            .map_err(|e| MediaError::Metadata(format!("decoding cached preview: {e}")));
    }

    let policy = PreviewIpPolicy::from_cidrs(&config.url_preview_ip_range_blocklist);
    let limits = FetchLimits::from_config(config);

    let page = guarded_fetch(url, &policy, &limits)
        .await
        .map_err(to_media_error)?;
    let decode_limits = DecodeLimits::from_config(config);

    let mut response = Map::new();
    if is_image(&page) {
        // The URL is an image itself: Synapse's answer is the image, cached, described by its
        // own name.
        let name = reqwest::Url::parse(&page.final_url).ok().and_then(|u| {
            u.path_segments()
                .and_then(|mut segments| segments.next_back().map(str::to_owned))
                .filter(|s| !s.is_empty())
        });
        if let Some(name) = name {
            response.insert("og:description".to_string(), Value::String(name));
        }
        cache_image(
            metadata,
            object_store,
            server_name,
            now_ms,
            &page,
            decode_limits,
            &mut response,
        )
        .await;
    } else {
        let html = String::from_utf8_lossy(&page.bytes);
        let tags = extract_og_tags(&html, &page.final_url);
        for (key, value) in tags.other {
            response.insert(key, Value::String(value));
        }
        if let Some(t) = tags.title {
            response.insert("og:title".to_string(), Value::String(t));
        }
        if let Some(d) = tags.description {
            response.insert("og:description".to_string(), Value::String(d));
        }
        if let Some(image_url) = tags.image {
            // What the page says about its image describes the remote copy; what is served is
            // the copy cached here, described below, or nothing.
            response.retain(|key, _| !key.starts_with("og:image:"));
            // A failed or non-image `og:image` fetch is not fatal to the whole preview --
            // Synapse's own behavior, and the spec does not require `og:image` to be present at
            // all -- so a guard/sniff failure here just omits the image fields rather than
            // failing the request.
            match guarded_fetch(&image_url, &policy, &limits).await {
                Ok(image) => {
                    cache_image(
                        metadata,
                        object_store,
                        server_name,
                        now_ms,
                        &image,
                        decode_limits,
                        &mut response,
                    )
                    .await;
                }
                Err(error) => tracing::debug!(
                    %error,
                    "a URL preview's og:image could not be fetched; previewing without it"
                ),
            }
        }
    }
    // Synapse drops overlong tags rather than let a page fill the cache and every client's
    // timeline with them.
    response.retain(|key, value| {
        let long = key.len() > OG_TAG_NAME_MAXLEN
            || match value {
                Value::String(s) => s.len() > OG_TAG_VALUE_MAXLEN,
                _ => false,
            };
        !long
    });

    let value = Value::Object(response);
    let serialized = serde_json::to_string(&value)
        .map_err(|e| MediaError::Metadata(format!("encoding preview response: {e}")))?;
    metadata.put_preview_cache(
        &cache_key,
        &serialized,
        now_ms,
        config.url_preview_cache_lifetime.as_millis(),
    )?;
    Ok(value)
}

/// Whether a fetched resource is an image: its bytes say so, whatever its `Content-Type`
/// claims (a page is never "an image" by header alone, and an image served as
/// `application/octet-stream` is still one).
fn is_image(resource: &FetchedResource) -> bool {
    crate::sniff::sniff_format(&resource.bytes).is_some()
}

/// Stores `image` as local media and describes it in `response` (`og:image` as an `mxc://` URI,
/// `og:image:type`, `og:image:width`/`og:image:height` from its header, `matrix:image:size`).
/// An image that is not one of the formats this server reads, or that declares more than the
/// `media` decode limits allow (`max_image_pixels`, `max_image_dimension`,
/// `max_image_decode_memory`, the same check a thumbnail makes, read from the header without
/// decoding a pixel), is refused: not stored, not described, and the preview goes without it.
async fn cache_image<B: KvBackend>(
    metadata: &MetadataStore<B>,
    object_store: &Arc<dyn ObjectStore>,
    server_name: &str,
    now_ms: u64,
    image: &FetchedResource,
    limits: DecodeLimits,
    response: &mut Map<String, Value>,
) {
    let (width, height, format) = match crate::sniff::probe_with_limits(&image.bytes, limits) {
        Ok(probed) => probed,
        Err(MediaError::ImageRefused {
            width,
            height,
            reason,
        }) => {
            tracing::info!(
                url = %image.final_url,
                image_width = width,
                image_height = height,
                reason = reason.as_str(),
                max_pixels = limits.max_pixels,
                max_dimension = limits.max_width,
                "a URL preview's image is over the image limits; previewing without it"
            );
            return;
        }
        Err(error) => {
            tracing::debug!(
                url = %image.final_url,
                %error,
                "a URL preview's image is not one this server reads; previewing without it"
            );
            return;
        }
    };
    let media_id = MediaId::generate();
    let key = crate::store::content_key(server_name, &media_id);
    if let Err(error) = object_store.put(&key, image.bytes.clone().into()).await {
        tracing::warn!(%error, "could not store a URL preview's image");
        return;
    }
    let content_type = crate::sniff::mime_for_format(format).to_string();
    let record = MediaRecord {
        server_name: server_name.to_string(),
        media_id: media_id.as_str().to_string(),
        content_type: content_type.clone(),
        upload_name: None,
        byte_length: Some(image.bytes.len() as u64),
        created_ms: now_ms,
        uploader: None,
        completed: true,
        expires_at_ms: None,
        quarantined_by: None,
        safe_from_quarantine: false,
        last_accessed_ms: None,
    };
    if let Err(error) = metadata.put_media(&record) {
        tracing::warn!(%error, "could not record a URL preview's image");
        return;
    }
    response.insert(
        "og:image".to_string(),
        Value::String(format!("mxc://{server_name}/{}", media_id.as_str())),
    );
    response.insert("og:image:type".to_string(), Value::String(content_type));
    response.insert("og:image:width".to_string(), Value::Number(width.into()));
    response.insert("og:image:height".to_string(), Value::Number(height.into()));
    response.insert(
        "matrix:image:size".to_string(),
        Value::Number(image.bytes.len().into()),
    );
}

fn to_media_error(e: FetchError) -> MediaError {
    match e {
        FetchError::Blocked(_) => MediaError::PreviewBlocked(e.to_string()),
        other => MediaError::PreviewFetchFailed(other.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blocklist_blocks_private_ranges() {
        let policy = PreviewIpPolicy::from_cidrs(&["10.0.0.0/8".to_string()]);
        assert!(!policy.allows("10.1.2.3".parse().unwrap()));
        assert!(policy.allows("8.8.8.8".parse().unwrap()));
    }

    #[test]
    fn default_blocklist_matches_media_config_defaults() {
        let config = MediaConfig::default();
        let policy = PreviewIpPolicy::from_cidrs(&config.url_preview_ip_range_blocklist);
        assert!(!policy.allows("127.0.0.1".parse().unwrap()));
        assert!(!policy.allows("192.168.1.1".parse().unwrap()));
        assert!(!policy.allows("::1".parse().unwrap()));
        assert!(policy.allows("93.184.216.34".parse().unwrap())); // example.com, public
    }

    #[test]
    fn extracts_basic_og_tags() {
        let html = r#"
            <html><head>
            <meta property="og:title" content="Hello &amp; World">
            <meta name="og:description" content="A description">
            <meta property="og:image" content="/img.png">
            </head></html>
        "#;
        let tags = extract_og_tags(html, "https://example.org/page");
        assert_eq!(tags.title.as_deref(), Some("Hello & World"));
        assert_eq!(tags.description.as_deref(), Some("A description"));
        assert_eq!(tags.image.as_deref(), Some("https://example.org/img.png"));
    }

    #[test]
    fn attribute_order_does_not_matter() {
        let html = r#"<meta content="Reordered" property="og:title">"#;
        let tags = extract_og_tags(html, "https://example.org/");
        assert_eq!(tags.title.as_deref(), Some("Reordered"));
    }

    #[test]
    fn single_quoted_attributes_are_parsed() {
        let html = r"<meta property='og:title' content='Single quoted'>";
        let tags = extract_og_tags(html, "https://example.org/");
        assert_eq!(tags.title.as_deref(), Some("Single quoted"));
    }

    #[test]
    fn falls_back_to_title_tag_when_no_og_title() {
        let html = "<html><head><title>Plain Title</title></head></html>";
        let tags = extract_og_tags(html, "https://example.org/");
        assert_eq!(tags.title.as_deref(), Some("Plain Title"));
    }

    #[test]
    fn numeric_character_references_decode() {
        assert_eq!(decode_entities("caf&#233;"), "café");
        assert_eq!(decode_entities("caf&#xE9;"), "café");
    }

    #[tokio::test]
    async fn fetch_refuses_a_private_address() {
        // This is the mutation-tested guard: comment out the `!candidates.iter().all(...)` check
        // in `resolve_and_check` (or replace it with `true`) and this test starts failing,
        // because `127.0.0.1` would then be treated as allowed and the fetch would attempt (and
        // fail differently, e.g. connection-refused, not `Blocked`) instead of being refused up
        // front. See this crate's convention (`docs/decisions/0002-workspace-conventions.md`) of
        // mutation-testing a security control rather than only asserting the happy path.
        let policy = PreviewIpPolicy::from_cidrs(&["127.0.0.0/8".to_string()]);
        let limits = FetchLimits::default();
        let err = guarded_fetch("http://127.0.0.1:1/anything", &policy, &limits)
            .await
            .unwrap_err();
        assert!(matches!(err, FetchError::Blocked(_)), "got {err:?}");
    }

    #[tokio::test]
    async fn fetch_allows_a_public_address_hostname_resolution() {
        // No blocklist entries at all: resolution + the policy check must not spuriously refuse a
        // real public hostname. This does not actually connect anywhere reachable (port 1 refuses
        // instantly almost everywhere), so it only exercises the resolve-and-check path, not a
        // full HTTP round trip -- a real round trip against the network is exercised by
        // `tests::live_preview_url_fetches_and_caches` (see that test's own doc for why it is
        // separately gated).
        let policy = PreviewIpPolicy::default();
        let limits = FetchLimits::default();
        let err = guarded_fetch("http://127.0.0.1:1/", &policy, &limits)
            .await
            .unwrap_err();
        // With an empty blocklist this must fail with a *connection* error, never `Blocked`.
        assert!(!matches!(err, FetchError::Blocked(_)), "got {err:?}");
    }

    #[tokio::test]
    async fn unsupported_scheme_is_refused() {
        let policy = PreviewIpPolicy::default();
        let limits = FetchLimits::default();
        let err = guarded_fetch("ftp://example.org/file", &policy, &limits)
            .await
            .unwrap_err();
        assert!(matches!(err, FetchError::UnsupportedScheme));
    }

    #[tokio::test]
    async fn embedded_credentials_are_refused() {
        let policy = PreviewIpPolicy::default();
        let limits = FetchLimits::default();
        let err = guarded_fetch("http://user:pass@example.org/", &policy, &limits)
            .await
            .unwrap_err();
        assert!(matches!(err, FetchError::UserinfoNotAllowed));
    }

    /// End-to-end: `preview_url` against a real local HTTP server (no network access outside this
    /// process) serving an HTML page with `og:title`/`og:description`/`og:image`, plus a real PNG
    /// at the `og:image` path. Proves the whole pipeline this module exists for: fetch, parse,
    /// fetch-and-sniff the image, store it under this server's own media namespace, and rewrite
    /// `og:image` to an `mxc://` URI with a correct `matrix:image:size`.
    ///
    /// The blocklist used here is intentionally empty (not `MediaConfig::default()`'s real
    /// blocklist, which would refuse `127.0.0.1` and defeat the point of this test) — the
    /// SSRF-guard behavior itself is covered separately and mutation-tested in
    /// `fetch_refuses_a_private_address`, above.
    #[tokio::test]
    async fn full_preview_flow_fetches_parses_and_caches_the_og_image() {
        use axum::Router;
        use axum::response::IntoResponse;
        use axum::routing::get;
        use hs_kv::memory::MemoryBackend;
        use object_store::memory::InMemory;
        use tokio::net::TcpListener;

        let png = Bytes::from(crate::test_fixtures::valid_png());
        let png_for_route = png.clone();
        let router = Router::new()
            .route(
                "/page",
                get(|| async {
                    axum::response::Html(
                        r#"<html><head>
                            <meta property="og:title" content="Test &amp; Page">
                            <meta property="og:description" content="A description">
                            <meta property="og:image" content="/img.png">
                        </head></html>"#,
                    )
                }),
            )
            .route(
                "/img.png",
                get(move || {
                    let bytes = png_for_route.clone();
                    async move {
                        ([(reqwest::header::CONTENT_TYPE, "image/png")], bytes).into_response()
                    }
                }),
            );
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });

        let metadata = MetadataStore::open(MemoryBackend::new()).unwrap();
        let object_store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let config = MediaConfig {
            url_preview_enabled: true,
            url_preview_ip_range_blocklist: vec![],
            ..MediaConfig::default()
        };
        let url = format!("http://{addr}/page");

        let value = preview_url(
            &metadata,
            &object_store,
            "example.org",
            &config,
            1_000,
            &url,
        )
        .await
        .unwrap();
        assert_eq!(value["og:title"], "Test & Page");
        assert_eq!(value["og:description"], "A description");
        let image_uri = value["og:image"].as_str().expect("og:image present");
        assert!(image_uri.starts_with("mxc://example.org/"));
        assert_eq!(
            value["matrix:image:size"].as_u64().unwrap(),
            png.len() as u64
        );

        // Cache proof: kill the server, then request the same URL again. Without the cache this
        // would fail with a connection error; with it, the identical response comes back with no
        // network access at all.
        server.abort();
        let cached = preview_url(
            &metadata,
            &object_store,
            "example.org",
            &config,
            1_001,
            &url,
        )
        .await
        .unwrap();
        assert_eq!(cached, value);
    }

    /// Serves `router` on a loopback port; the server stops when the handle is aborted or
    /// dropped with the runtime.
    async fn serve(router: axum::Router) -> std::net::SocketAddr {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        addr
    }

    /// Previews `url` against a fresh store, with previews on and loopback allowed.
    async fn preview_of(
        url: &str,
        config: MediaConfig,
    ) -> (Value, MetadataStore<hs_kv::memory::MemoryBackend>) {
        let metadata = MetadataStore::open(hs_kv::memory::MemoryBackend::new()).unwrap();
        let object_store: Arc<dyn ObjectStore> = Arc::new(object_store::memory::InMemory::new());
        let config = MediaConfig {
            url_preview_enabled: true,
            url_preview_ip_range_blocklist: vec![],
            ..config
        };
        let value = preview_url(&metadata, &object_store, "example.org", &config, 1_000, url)
            .await
            .unwrap();
        (value, metadata)
    }

    fn png_route(bytes: Bytes) -> axum::routing::MethodRouter {
        use axum::response::IntoResponse;
        axum::routing::get(move || {
            let bytes = bytes.clone();
            async move { ([(reqwest::header::CONTENT_TYPE, "image/png")], bytes).into_response() }
        })
    }

    /// Sytest's `51media/20urlpreview.pl` and Complement's `TestUrlPreview`, exactly: every
    /// `og:` tag the page sets comes back (`og:type` and `og:url` too, not only the three this
    /// module once kept), and the image's own size from its header. The PNG is Sytest's
    /// `tests/51media/test.png` (Apache-2.0, matrix-org/sytest), 279 x 129, 2239 bytes.
    #[tokio::test]
    async fn sytest_preview_page_comes_back_with_every_tag_and_the_image_size() {
        const SYTEST_PNG: &[u8] = include_bytes!("../tests/fixtures/images/sytest_preview.png");
        let router = axum::Router::new()
            .route(
                "/test.html",
                axum::routing::get(|| async {
                    axum::response::Html(
                        r#"<html prefix="og: http://ogp.me/ns#">
<head>
<title>The Rock (1996)</title>
<meta property="og:title" content="The Rock" />
<meta property="og:type" content="video.movie" />
<meta property="og:url" content="http://www.imdb.com/title/tt0117500/" />
<meta property="og:image" content="test.png" />
</head>
<body></body>
</html>"#,
                    )
                }),
            )
            .route("/test.png", png_route(Bytes::from_static(SYTEST_PNG)));
        let addr = serve(router).await;
        let (value, _) =
            preview_of(&format!("http://{addr}/test.html"), MediaConfig::default()).await;
        assert_eq!(value["og:title"], "The Rock");
        assert_eq!(value["og:type"], "video.movie");
        assert_eq!(value["og:url"], "http://www.imdb.com/title/tt0117500/");
        assert_eq!(value["matrix:image:size"], 2239);
        assert_eq!(value["og:image:width"], 279);
        assert_eq!(value["og:image:height"], 129);
        assert_eq!(value["og:image:type"], "image/png");
        assert!(
            value["og:image"]
                .as_str()
                .unwrap()
                .starts_with("mxc://example.org/")
        );
    }

    /// An `og:image` that declares more pixels than the `media` decode limits allow is refused
    /// from its header, the same check a thumbnail makes: not stored, not described, and the
    /// page's own `og:image:*` claims about it are dropped with it.
    #[tokio::test]
    async fn a_preview_of_a_huge_image_is_refused() {
        let bomb = Bytes::from(crate::test_fixtures::decompression_bomb_png(60_000, 60_000));
        let router = axum::Router::new()
            .route(
                "/page",
                axum::routing::get(|| async {
                    axum::response::Html(format!(
                        r#"<meta property="og:title" content="Huge">
                           <meta property="og:image" content="/huge.png">
                           <meta property="og:image:width" content="60000">
                           <meta property="og:site_name" content="{}">"#,
                        "x".repeat(OG_TAG_VALUE_MAXLEN + 1)
                    ))
                }),
            )
            .route("/huge.png", png_route(bomb.clone()));
        let addr = serve(router).await;
        let (value, metadata) =
            preview_of(&format!("http://{addr}/page"), MediaConfig::default()).await;
        assert_eq!(value["og:title"], "Huge");
        for key in [
            "og:image",
            "og:image:width",
            "og:image:height",
            "matrix:image:size",
            // Overlong, dropped as Synapse drops it.
            "og:site_name",
        ] {
            assert!(value.get(key).is_none(), "{key} must be absent: {value}");
        }
        assert!(
            metadata.list_media().unwrap().is_empty(),
            "nothing is stored for a refused image"
        );

        // A URL that is the huge image itself is refused the same way.
        let (direct, _) =
            preview_of(&format!("http://{addr}/huge.png"), MediaConfig::default()).await;
        assert!(direct.get("og:image").is_none(), "{direct}");

        // And the limit is the setting in force: a 4 x 4 image over a 10-pixel limit.
        let small = Bytes::from(crate::test_fixtures::valid_png());
        let addr = serve(axum::Router::new().route("/small.png", png_route(small))).await;
        let (limited, _) = preview_of(
            &format!("http://{addr}/small.png"),
            MediaConfig {
                max_image_pixels: 10,
                ..MediaConfig::default()
            },
        )
        .await;
        assert!(limited.get("og:image").is_none(), "{limited}");
    }

    /// A URL that is an image is previewed as the image: cached, with its size and its name.
    #[tokio::test]
    async fn a_url_that_is_an_image_is_previewed_as_one() {
        let png = Bytes::from(crate::test_fixtures::valid_png());
        let addr = serve(axum::Router::new().route("/cat.png", png_route(png.clone()))).await;
        let (value, _) =
            preview_of(&format!("http://{addr}/cat.png"), MediaConfig::default()).await;
        assert!(
            value["og:image"]
                .as_str()
                .unwrap()
                .starts_with("mxc://example.org/")
        );
        assert_eq!(value["og:image:width"], 4);
        assert_eq!(value["og:image:height"], 4);
        assert_eq!(value["og:description"], "cat.png");
        assert_eq!(value["matrix:image:size"], png.len());
    }

    #[test]
    fn a_repeated_tag_keeps_its_first_value() {
        let html = r#"<meta property="og:type" content="first">
               <meta property="og:type" content="second">"#;
        let tags = extract_og_tags(html, "http://example.org/");
        assert_eq!(tags.other.get("og:type").map(String::as_str), Some("first"));
    }
}
