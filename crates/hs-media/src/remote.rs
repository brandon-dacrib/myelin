//! Remote media: fetching another server's media over federation, and the cache that keeps it.
//!
//! A client asks this server for `mxc://origin/mediaId` where `origin` is not this server. The
//! first request fetches it from `origin` and stores it under `(origin, mediaId)` in the same
//! object store and metadata table as a local upload (`docs/rfcs/0007-federation-media.md`
//! section 3.4); every later request is served from that copy, with `origin` never asked again
//! -- it may be down, and the copy is what this server's users already saw.
//!
//! # How a fetch goes ([`fetch_remote`])
//!
//! 1. `GET /_matrix/federation/v1/media/download/{mediaId}`, `X-Matrix` signed (spec v1.11,
//!    MSC3916). The answer is `multipart/mixed`: a JSON part (`{}` today), then either the
//!    content with its own `Content-Type` and `Content-Disposition`, or a part carrying only a
//!    `Location` header -- the redirect form, used by servers that offload downloads to a CDN.
//!    A `3xx` status with a `Location` is read the same way.
//! 2. A redirect is followed through [`crate::preview::guarded_fetch`], the URL-preview fetcher:
//!    the address is resolved, checked against `media.url_preview_ip_range_blocklist` on every
//!    hop, and pinned. A peer homeserver gets no more trust than a URL a client pasted.
//! 3. If the origin does not implement the federation media API (a server older than v1.11:
//!    `404 M_UNRECOGNIZED`, `400 M_UNRECOGNIZED`, `405`, `501`), the legacy, unauthenticated
//!    `GET /_matrix/media/v3/download/{origin}/{mediaId}?allow_remote=false` is asked instead.
//!    A `404 M_NOT_FOUND` from the federation API is an answer, not a sign of an old server,
//!    and is not retried on the legacy path.
//!
//! A client that asked on the legacy, unauthenticated `/_matrix/media/v3` paths gets the
//! reverse order ([`fetch_remote_legacy_first`]): the origin's legacy path first, then the
//! federation API, as Synapse routes that client API.
//!
//! Every path is capped at `media.max_upload_size`: nothing arrives from another server that
//! this server would not have accepted from its own user.
//!
//! # The cache ([`RemoteMedia`], `MediaRepository::resolve_record`)
//!
//! - A cached row is served exactly as a local one is: a quarantined copy looks like not-found
//!   and is **not** fetched again (that would undo the quarantine), and the admin API's
//!   `media.purge_remote_cache` and `media.delete` remove the copy, after which the next request
//!   fetches it afresh.
//! - One fetch per item at a time: concurrent requests for the same uncached item wait for the
//!   first and are then served from the cache it filled.
//! - A failure is not cached. The federation client's per-destination backoff already makes a
//!   request for a dead origin fail fast rather than hammering it.
//!
//! What this module does not do yet: evict by `media.remote_media_retention` on its own (the
//! admin purge is the way to drop copies), scan fetched content (`ScanSourceKind::Federation`
//! exists for it), or ask the origin for a thumbnail -- a remote thumbnail is generated here from
//! the fetched original, which is what Synapse does too.
//!
//! Transport (the signed request, discovery, the destination's backoff) is not this crate's:
//! [`RemoteMediaTransport`] is implemented over `hs-federation`'s client in `hs-cli`.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use prometheus_client::metrics::counter::Counter;
use prometheus_client::metrics::family::Family;
use prometheus_client::registry::Registry;

use crate::id::MediaId;
use crate::multipart;
use crate::preview::{FetchError, FetchLimits, PreviewIpPolicy, guarded_fetch};

/// A remote server's raw answer, as the transport hands it back.
#[derive(Debug, Clone, Default)]
pub struct RemoteResponse {
    /// The HTTP status.
    pub status: u16,
    /// `Content-Type`.
    pub content_type: Option<String>,
    /// `Content-Disposition` (the legacy path; the federation API puts it inside the body).
    pub content_disposition: Option<String>,
    /// `Location`, on a redirect.
    pub location: Option<String>,
    /// The body, already capped by the transport.
    pub body: Bytes,
}

/// Why the transport could not get an answer at all.
#[derive(Debug, Clone, thiserror::Error)]
pub enum TransportError {
    /// The body was larger than the cap the caller gave.
    #[error("the response exceeded the size limit")]
    TooLarge,
    /// Anything else: federation disabled or denied for this destination, the destination
    /// backing off, discovery or connection failure.
    #[error("{0}")]
    Failed(String),
}

/// What this crate needs from the federation client to fetch remote media.
#[async_trait]
pub trait RemoteMediaTransport: Send + Sync {
    /// `GET path` against `origin`, `X-Matrix` signed when `signed` is true (the federation
    /// media API) and bare when it is not (the legacy media API). The body is capped at
    /// `max_bytes`; redirects are returned, not followed.
    async fn get(
        &self,
        origin: &str,
        path: &str,
        signed: bool,
        max_bytes: usize,
    ) -> Result<RemoteResponse, TransportError>;
}

/// How a remote item was obtained, for the metrics and the log.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FetchVia {
    /// The federation media API, content in the multipart body.
    Federation,
    /// The federation media API, content behind the redirect it named.
    Redirect,
    /// The legacy `/_matrix/media/v3/download` path.
    Legacy,
}

impl FetchVia {
    /// The metric label.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            FetchVia::Federation => "federation",
            FetchVia::Redirect => "redirect",
            FetchVia::Legacy => "legacy",
        }
    }
}

/// One remote item, fetched.
#[derive(Debug, Clone)]
pub struct FetchedMedia {
    /// The type the origin declared (`application/octet-stream` when it declared none).
    pub content_type: String,
    /// The filename from the origin's `Content-Disposition`, if any.
    pub upload_name: Option<String>,
    /// The bytes.
    pub bytes: Bytes,
    /// Which path produced it.
    pub via: FetchVia,
}

/// Why a remote fetch failed.
#[derive(Debug, Clone, thiserror::Error)]
pub enum RemoteFetchError {
    /// The origin says it has no such item.
    #[error("the origin has no such media")]
    NotFound,
    /// The item is larger than `media.max_upload_size`.
    #[error("the remote media exceeds the {0} byte limit")]
    TooLarge(u64),
    /// Anything else: the origin unreachable, a refusal, a malformed answer, a blocked redirect.
    #[error("{0}")]
    Failed(String),
}

/// Bounds on one fetch.
#[derive(Debug, Clone)]
pub struct RemoteFetchLimits {
    /// The largest item accepted, in bytes (`media.max_upload_size`).
    pub max_bytes: u64,
    /// The address policy a redirect target is checked against
    /// (`media.url_preview_ip_range_blocklist`).
    pub redirect_policy: PreviewIpPolicy,
    /// The per-hop timeout when following a redirect.
    pub redirect_timeout: Duration,
}

/// Fetches `media_id` from `origin`. See the module doc for the order things are tried in.
///
/// # Errors
/// See [`RemoteFetchError`].
pub async fn fetch_remote(
    transport: &dyn RemoteMediaTransport,
    origin: &str,
    media_id: &MediaId,
    limits: &RemoteFetchLimits,
) -> Result<FetchedMedia, RemoteFetchError> {
    let max = usize::try_from(limits.max_bytes).unwrap_or(usize::MAX);
    // The multipart framing and the JSON part add a little to the content itself.
    let framed_max = max.saturating_add(64 * 1024);
    let path = format!("/_matrix/federation/v1/media/download/{media_id}");
    let response = transport
        .get(origin, &path, true, framed_max)
        .await
        .map_err(|e| transport_error(e, limits.max_bytes))?;

    match response.status {
        200..=299 => read_federation_answer(&response, limits).await,
        300..=399 => match &response.location {
            Some(location) => follow_redirect(location, limits).await,
            None => Err(RemoteFetchError::Failed(format!(
                "the origin answered {} with no Location",
                response.status
            ))),
        },
        _ if is_unsupported(&response) => {
            tracing::debug!(
                origin,
                media_id = %media_id,
                status = response.status,
                "the origin does not serve the federation media API; trying the legacy path"
            );
            fetch_legacy(transport, origin, media_id, limits).await
        }
        404 => Err(RemoteFetchError::NotFound),
        status => Err(RemoteFetchError::Failed(format!(
            "the origin answered {status}: {}",
            snippet(&response.body)
        ))),
    }
}

/// Fetches `media_id` from `origin` for a client that asked on the legacy, unauthenticated
/// `/_matrix/media/v3/download` (or `thumbnail`) path: the origin's own legacy path is asked
/// first, as Synapse does for that route (`use_federation_endpoint=False` in its media
/// repository, read for behaviour), and the federation media API only if that gives nothing.
/// A server that serves its media only one way is reached either way; the order follows the
/// client's own choice of API.
///
/// # Errors
/// See [`RemoteFetchError`]: [`RemoteFetchError::TooLarge`] from the legacy path is final; any
/// other legacy failure is followed by the federation API, whose error is the one returned.
pub async fn fetch_remote_legacy_first(
    transport: &dyn RemoteMediaTransport,
    origin: &str,
    media_id: &MediaId,
    limits: &RemoteFetchLimits,
) -> Result<FetchedMedia, RemoteFetchError> {
    match fetch_legacy(transport, origin, media_id, limits).await {
        Ok(fetched) => Ok(fetched),
        Err(RemoteFetchError::TooLarge(limit)) => Err(RemoteFetchError::TooLarge(limit)),
        Err(legacy_error) => {
            tracing::debug!(
                origin,
                media_id = %media_id,
                error = %legacy_error,
                "the origin's legacy media path gave nothing; trying the federation media API"
            );
            fetch_remote(transport, origin, media_id, limits).await
        }
    }
}

/// Whether an error answer says "I do not implement this endpoint" rather than anything about
/// the item: a Synapse or Conduit from before v1.11 answers an unknown path with
/// `404`/`400 M_UNRECOGNIZED`, and some proxies with `405` or `501`.
fn is_unsupported(response: &RemoteResponse) -> bool {
    match response.status {
        405 | 501 => true,
        400 | 404 => {
            let errcode = serde_json::from_slice::<serde_json::Value>(&response.body)
                .ok()
                .and_then(|v| v.get("errcode").and_then(|e| e.as_str()).map(str::to_owned));
            match errcode.as_deref() {
                Some("M_UNRECOGNIZED") => true,
                // A bare 404 with no Matrix body is a web server that knows no such path.
                None => response.status == 404,
                Some(_) => false,
            }
        }
        _ => false,
    }
}

async fn read_federation_answer(
    response: &RemoteResponse,
    limits: &RemoteFetchLimits,
) -> Result<FetchedMedia, RemoteFetchError> {
    let content_type = response.content_type.as_deref().unwrap_or("");
    let boundary = multipart::boundary_from_content_type(content_type)
        .map_err(|e| RemoteFetchError::Failed(e.to_string()))?;
    let parsed = multipart::parse(&boundary, &response.body)
        .map_err(|e| RemoteFetchError::Failed(e.to_string()))?;
    let Some(content) = parsed.parts.get(1) else {
        return Err(RemoteFetchError::Failed(
            "the multipart answer has no content part".into(),
        ));
    };
    if let Some(location) = content.header("Location") {
        return follow_redirect(location, limits).await;
    }
    if content.body.len() as u64 > limits.max_bytes {
        return Err(RemoteFetchError::TooLarge(limits.max_bytes));
    }
    Ok(FetchedMedia {
        content_type: declared_type(content.header("Content-Type")),
        upload_name: content
            .header("Content-Disposition")
            .and_then(filename_from_disposition),
        bytes: Bytes::copy_from_slice(content.body),
        via: FetchVia::Federation,
    })
}

async fn follow_redirect(
    location: &str,
    limits: &RemoteFetchLimits,
) -> Result<FetchedMedia, RemoteFetchError> {
    let fetch_limits = FetchLimits {
        max_body_bytes: usize::try_from(limits.max_bytes).unwrap_or(usize::MAX),
        timeout: limits.redirect_timeout,
        max_redirects: FetchLimits::default().max_redirects,
    };
    let fetched = guarded_fetch(location, &limits.redirect_policy, &fetch_limits)
        .await
        .map_err(|e| match e {
            FetchError::TooLarge(_) => RemoteFetchError::TooLarge(limits.max_bytes),
            FetchError::BadStatus(404) => RemoteFetchError::NotFound,
            other => RemoteFetchError::Failed(format!("following the redirect: {other}")),
        })?;
    Ok(FetchedMedia {
        content_type: declared_type(fetched.content_type.as_deref()),
        upload_name: None,
        bytes: fetched.bytes,
        via: FetchVia::Redirect,
    })
}

async fn fetch_legacy(
    transport: &dyn RemoteMediaTransport,
    origin: &str,
    media_id: &MediaId,
    limits: &RemoteFetchLimits,
) -> Result<FetchedMedia, RemoteFetchError> {
    let max = usize::try_from(limits.max_bytes).unwrap_or(usize::MAX);
    let path = format!(
        "/_matrix/media/v3/download/{}/{media_id}?allow_remote=false&allow_redirect=true",
        percent_encode_path_segment(origin)
    );
    let response = transport
        .get(origin, &path, false, max)
        .await
        .map_err(|e| transport_error(e, limits.max_bytes))?;
    match response.status {
        200..=299 => Ok(FetchedMedia {
            content_type: declared_type(response.content_type.as_deref()),
            upload_name: response
                .content_disposition
                .as_deref()
                .and_then(filename_from_disposition),
            bytes: response.body,
            via: FetchVia::Legacy,
        }),
        300..=399 => match &response.location {
            Some(location) => follow_redirect(location, limits).await,
            None => Err(RemoteFetchError::Failed(format!(
                "the origin's legacy path answered {} with no Location",
                response.status
            ))),
        },
        404 => Err(RemoteFetchError::NotFound),
        status => Err(RemoteFetchError::Failed(format!(
            "the origin's legacy path answered {status}: {}",
            snippet(&response.body)
        ))),
    }
}

fn transport_error(e: TransportError, max: u64) -> RemoteFetchError {
    match e {
        TransportError::TooLarge => RemoteFetchError::TooLarge(max),
        TransportError::Failed(message) => RemoteFetchError::Failed(message),
    }
}

fn declared_type(content_type: Option<&str>) -> String {
    content_type
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .unwrap_or("application/octet-stream")
        .to_owned()
}

fn snippet(body: &[u8]) -> String {
    let text = String::from_utf8_lossy(&body[..body.len().min(200)]);
    text.into_owned()
}

/// Server names are `host[:port]`, and a `:` is fine in a path segment; anything else outside
/// the unreserved set is escaped so a hostile name cannot reshape the path.
fn percent_encode_path_segment(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~' | b':' | b'[' | b']')
        {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// The filename in a `Content-Disposition` value: `filename*=UTF-8''...` (RFC 6266, percent
/// encoded) wins over a plain `filename=`.
#[must_use]
pub fn filename_from_disposition(value: &str) -> Option<String> {
    let mut plain = None;
    for param in value.split(';').map(str::trim) {
        let Some((name, raw)) = param.split_once('=') else {
            continue;
        };
        let name = name.trim().to_ascii_lowercase();
        let raw = raw.trim();
        if name == "filename*" {
            let encoded = raw.split_once("''").map_or(raw, |(_, rest)| rest);
            if let Some(decoded) = percent_decode(encoded)
                && !decoded.is_empty()
            {
                return Some(decoded);
            }
        } else if name == "filename" {
            let unquoted = raw.trim_matches('"');
            if !unquoted.is_empty() {
                plain = Some(unquoted.to_owned());
            }
        }
    }
    plain
}

fn percent_decode(s: &str) -> Option<String> {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = s.get(i + 1..i + 3)?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// Labels for [`RemoteMediaMetrics::requests`].
#[derive(Debug, Clone, PartialEq, Eq, Hash, prometheus_client::encoding::EncodeLabelSet)]
pub struct CacheLabels {
    /// `hit` (served from the cached copy) or `miss` (had to be fetched).
    pub result: String,
}

/// Labels for [`RemoteMediaMetrics::fetch_bytes`].
#[derive(Debug, Clone, PartialEq, Eq, Hash, prometheus_client::encoding::EncodeLabelSet)]
pub struct ViaLabels {
    /// `federation`, `redirect` or `legacy`.
    pub via: String,
}

/// Labels for [`RemoteMediaMetrics::fetches`].
#[derive(Debug, Clone, PartialEq, Eq, Hash, prometheus_client::encoding::EncodeLabelSet)]
pub struct FetchLabels {
    /// `success`, `not_found` or `failure`.
    pub outcome: String,
    /// `federation`, `redirect` or `legacy` on success; `none` otherwise.
    pub via: String,
}

/// The remote-media metric families:
///
/// - `hs_media_remote_requests_total{result="hit"|"miss"}`: requests for another server's
///   media, answered from the cache or needing a fetch.
/// - `hs_media_remote_fetches_total{outcome,via}`: fetches, by outcome and the path that served
///   them.
/// - `hs_media_remote_fetch_bytes_total{via}`: bytes fetched.
#[derive(Clone, Default)]
pub struct RemoteMediaMetrics {
    /// `hs_media_remote_requests_total`.
    pub requests: Family<CacheLabels, Counter>,
    /// `hs_media_remote_fetches_total`.
    pub fetches: Family<FetchLabels, Counter>,
    /// `hs_media_remote_fetch_bytes_total`.
    pub fetch_bytes: Family<ViaLabels, Counter>,
}

impl RemoteMediaMetrics {
    /// Registers the families into `metrics`'s shared registry.
    #[must_use]
    pub fn register(metrics: &hs_telemetry::metrics::Metrics) -> Self {
        let this = Self::default();
        metrics.with_registry(|registry: &mut Registry| {
            // No `_total` suffix here: the text encoder appends it to a counter.
            registry.register(
                "hs_media_remote_requests",
                "Requests for another server's media, by whether the cached copy answered (hit) or it had to be fetched (miss).",
                this.requests.clone(),
            );
            registry.register(
                "hs_media_remote_fetches",
                "Fetches of another server's media, by outcome and by the path that answered (federation, redirect, legacy).",
                this.fetches.clone(),
            );
            registry.register(
                "hs_media_remote_fetch_bytes",
                "Bytes of another server's media fetched, by the path that answered (federation, redirect, legacy).",
                this.fetch_bytes.clone(),
            );
        });
        this
    }

    pub(crate) fn hit(&self) {
        self.requests
            .get_or_create(&CacheLabels {
                result: "hit".into(),
            })
            .inc();
    }

    pub(crate) fn miss(&self) {
        self.requests
            .get_or_create(&CacheLabels {
                result: "miss".into(),
            })
            .inc();
    }

    pub(crate) fn fetched(&self, result: &Result<FetchedMedia, RemoteFetchError>) {
        let (outcome, via) = match result {
            Ok(media) => {
                self.fetch_bytes
                    .get_or_create(&ViaLabels {
                        via: media.via.as_str().into(),
                    })
                    .inc_by(media.bytes.len() as u64);
                ("success", media.via.as_str())
            }
            Err(RemoteFetchError::NotFound) => ("not_found", "none"),
            Err(_) => ("failure", "none"),
        };
        self.fetches
            .get_or_create(&FetchLabels {
                outcome: outcome.into(),
                via: via.into(),
            })
            .inc();
    }
}

/// The lock one item's fetch is made under.
type Gate = Arc<tokio::sync::Mutex<()>>;

/// The remote side of a [`crate::MediaRepository`]: the transport, the metrics, and the
/// one-fetch-per-item gate. Installed with `MediaRepository::install_remote_media`.
pub struct RemoteMedia {
    pub(crate) transport: Arc<dyn RemoteMediaTransport>,
    pub(crate) metrics: RemoteMediaMetrics,
    inflight: std::sync::Mutex<HashMap<(String, String), Gate>>,
}

impl RemoteMedia {
    /// Wraps `transport`, counting into `metrics`.
    #[must_use]
    pub fn new(transport: Arc<dyn RemoteMediaTransport>, metrics: RemoteMediaMetrics) -> Self {
        Self {
            transport,
            metrics,
            inflight: std::sync::Mutex::new(HashMap::new()),
        }
    }

    /// The gate for one item: whoever holds its lock is the one fetching it.
    pub(crate) fn gate(&self, origin: &str, media_id: &str) -> Gate {
        let mut map = self
            .inflight
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        map.entry((origin.to_owned(), media_id.to_owned()))
            .or_default()
            .clone()
    }

    /// Drops the gate for one item once its fetch is over; anyone still waiting holds a clone
    /// and finds the cache filled.
    pub(crate) fn release(&self, origin: &str, media_id: &str) {
        let mut map = self
            .inflight
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        map.remove(&(origin.to_owned(), media_id.to_owned()));
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::sync::Mutex;

    /// A transport that answers from a script and records what it was asked.
    #[derive(Default)]
    pub(crate) struct ScriptedTransport {
        pub(crate) answers: Mutex<HashMap<String, RemoteResponse>>,
        pub(crate) asked: Mutex<Vec<(String, bool)>>,
        pub(crate) down: std::sync::atomic::AtomicBool,
    }

    impl ScriptedTransport {
        pub(crate) fn answer(&self, path_prefix: &str, response: RemoteResponse) {
            self.answers
                .lock()
                .unwrap()
                .insert(path_prefix.to_owned(), response);
        }

        pub(crate) fn asked(&self) -> Vec<(String, bool)> {
            self.asked.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl RemoteMediaTransport for ScriptedTransport {
        async fn get(
            &self,
            _origin: &str,
            path: &str,
            signed: bool,
            max_bytes: usize,
        ) -> Result<RemoteResponse, TransportError> {
            self.asked.lock().unwrap().push((path.to_owned(), signed));
            if self.down.load(std::sync::atomic::Ordering::SeqCst) {
                return Err(TransportError::Failed("connection refused".into()));
            }
            let answers = self.answers.lock().unwrap();
            let found = answers
                .iter()
                .find(|(prefix, _)| path.starts_with(prefix.as_str()))
                .map(|(_, r)| r.clone());
            match found {
                Some(r) if r.body.len() > max_bytes => Err(TransportError::TooLarge),
                Some(r) => Ok(r),
                None => Ok(RemoteResponse {
                    status: 404,
                    body: Bytes::from_static(br#"{"errcode":"M_UNRECOGNIZED"}"#),
                    ..RemoteResponse::default()
                }),
            }
        }
    }

    pub(crate) fn multipart_answer(
        content_type: &str,
        filename: &str,
        body: &[u8],
    ) -> RemoteResponse {
        let disposition = format!("inline; filename=\"{filename}\"");
        let (ct, bytes) = multipart::build_media_response(
            &[
                ("Content-Type", content_type),
                ("Content-Disposition", &disposition),
            ],
            body,
        );
        RemoteResponse {
            status: 200,
            content_type: Some(ct),
            body: bytes,
            ..RemoteResponse::default()
        }
    }

    pub(crate) fn limits() -> RemoteFetchLimits {
        RemoteFetchLimits {
            max_bytes: 1_000,
            redirect_policy: PreviewIpPolicy::from_cidrs(&[]),
            redirect_timeout: Duration::from_secs(5),
        }
    }

    fn id(s: &str) -> MediaId {
        MediaId::parse(s).unwrap()
    }

    #[tokio::test]
    async fn the_federation_api_is_asked_first_and_signed() {
        let transport = ScriptedTransport::default();
        transport.answer(
            "/_matrix/federation/v1/media/download/abc",
            multipart_answer("image/png", "cat.png", b"png bytes"),
        );
        let got = fetch_remote(&transport, "a.example", &id("abc"), &limits())
            .await
            .unwrap();
        assert_eq!(got.via, FetchVia::Federation);
        assert_eq!(&got.bytes[..], b"png bytes");
        assert_eq!(got.content_type, "image/png");
        assert_eq!(got.upload_name.as_deref(), Some("cat.png"));
        assert_eq!(
            transport.asked(),
            vec![("/_matrix/federation/v1/media/download/abc".to_owned(), true)]
        );
    }

    #[tokio::test]
    async fn an_old_server_is_asked_on_the_legacy_path_unsigned() {
        let transport = ScriptedTransport::default();
        transport.answer(
            "/_matrix/media/v3/download/a.example/abc",
            RemoteResponse {
                status: 200,
                content_type: Some("text/plain".into()),
                content_disposition: Some("attachment; filename*=UTF-8''na%C3%AFve.txt".into()),
                body: Bytes::from_static(b"old"),
                ..RemoteResponse::default()
            },
        );
        let got = fetch_remote(&transport, "a.example", &id("abc"), &limits())
            .await
            .unwrap();
        assert_eq!(got.via, FetchVia::Legacy);
        assert_eq!(&got.bytes[..], b"old");
        assert_eq!(got.upload_name.as_deref(), Some("naïve.txt"));
        let asked = transport.asked();
        assert_eq!(asked.len(), 2);
        assert!(asked[1].0.contains("allow_remote=false"));
        assert!(!asked[1].1, "the legacy path is not signed");
    }

    /// Complement's `TestMediaWithoutFileName` over federation: its origin answers the legacy
    /// path and refuses the federation API with a bare 400, so a legacy client download must
    /// ask the legacy path first, as Synapse does.
    #[tokio::test]
    async fn a_legacy_client_download_asks_the_legacy_path_first() {
        let transport = ScriptedTransport::default();
        transport.answer(
            "/_matrix/media/v3/download/a.example/abc",
            RemoteResponse {
                status: 200,
                content_type: Some("text/plain".into()),
                body: Bytes::from_static(b"Hello from the other side"),
                ..RemoteResponse::default()
            },
        );
        transport.answer(
            "/_matrix/federation/v1/media/download/abc",
            RemoteResponse {
                status: 400,
                body: Bytes::from_static(b"complement: Invalid Origin"),
                ..RemoteResponse::default()
            },
        );
        let got = fetch_remote_legacy_first(&transport, "a.example", &id("abc"), &limits())
            .await
            .unwrap();
        assert_eq!(got.via, FetchVia::Legacy);
        assert_eq!(got.content_type, "text/plain");
        assert_eq!(transport.asked().len(), 1);
        assert!(!transport.asked()[0].1, "the legacy path is not signed");
    }

    #[tokio::test]
    async fn a_legacy_client_download_falls_back_to_the_federation_api() {
        let transport = ScriptedTransport::default();
        transport.answer(
            "/_matrix/media/v3/download/a.example/abc",
            RemoteResponse {
                status: 404,
                body: Bytes::from_static(br#"{"errcode":"M_NOT_FOUND"}"#),
                ..RemoteResponse::default()
            },
        );
        transport.answer(
            "/_matrix/federation/v1/media/download/abc",
            multipart_answer("image/png", "cat.png", b"png bytes"),
        );
        let got = fetch_remote_legacy_first(&transport, "a.example", &id("abc"), &limits())
            .await
            .unwrap();
        assert_eq!(got.via, FetchVia::Federation);
        assert_eq!(
            transport.asked().last().unwrap(),
            &("/_matrix/federation/v1/media/download/abc".to_owned(), true)
        );
    }

    #[tokio::test]
    async fn not_found_on_the_federation_api_is_final() {
        let transport = ScriptedTransport::default();
        transport.answer(
            "/_matrix/federation/v1/media/download/abc",
            RemoteResponse {
                status: 404,
                body: Bytes::from_static(br#"{"errcode":"M_NOT_FOUND"}"#),
                ..RemoteResponse::default()
            },
        );
        let err = fetch_remote(&transport, "a.example", &id("abc"), &limits())
            .await
            .unwrap_err();
        assert!(matches!(err, RemoteFetchError::NotFound));
        assert_eq!(transport.asked().len(), 1, "no legacy retry");
    }

    #[tokio::test]
    async fn an_item_over_the_limit_is_refused() {
        let transport = ScriptedTransport::default();
        transport.answer(
            "/_matrix/federation/v1/media/download/abc",
            multipart_answer("image/png", "big.png", &[0u8; 1_500]),
        );
        let err = fetch_remote(&transport, "a.example", &id("abc"), &limits())
            .await
            .unwrap_err();
        assert!(matches!(err, RemoteFetchError::TooLarge(1_000)), "{err}");
    }

    #[tokio::test]
    async fn a_redirect_to_a_blocked_address_is_not_followed() {
        let transport = ScriptedTransport::default();
        let (ct, body) = multipart::build_media_response(
            &[("Location", "http://169.254.169.254/latest/meta-data")],
            b"",
        );
        transport.answer(
            "/_matrix/federation/v1/media/download/abc",
            RemoteResponse {
                status: 200,
                content_type: Some(ct),
                body,
                ..RemoteResponse::default()
            },
        );
        let mut limits = limits();
        limits.redirect_policy = PreviewIpPolicy::from_cidrs(&["169.254.0.0/16".to_owned()]);
        let err = fetch_remote(&transport, "a.example", &id("abc"), &limits)
            .await
            .unwrap_err();
        let RemoteFetchError::Failed(message) = err else {
            panic!("expected a failure, got {err:?}");
        };
        assert!(message.contains("outside the allowed range"), "{message}");
    }

    #[tokio::test]
    async fn an_unreachable_origin_is_a_failure_not_a_fallback() {
        let transport = ScriptedTransport::default();
        transport
            .down
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let err = fetch_remote(&transport, "a.example", &id("abc"), &limits())
            .await
            .unwrap_err();
        assert!(matches!(err, RemoteFetchError::Failed(_)));
        assert_eq!(transport.asked().len(), 1);
    }

    #[test]
    fn disposition_filenames() {
        assert_eq!(
            filename_from_disposition("inline; filename=\"a b.png\"").as_deref(),
            Some("a b.png")
        );
        assert_eq!(
            filename_from_disposition("inline; filename=x.png; filename*=utf-8''y%20z.png")
                .as_deref(),
            Some("y z.png")
        );
        assert_eq!(filename_from_disposition("inline"), None);
        assert_eq!(
            filename_from_disposition("inline; filename*=utf-8''%ZZ"),
            None
        );
    }

    #[test]
    fn a_hostile_origin_cannot_reshape_the_legacy_path() {
        assert_eq!(
            percent_encode_path_segment("a.example:8448"),
            "a.example:8448"
        );
        assert_eq!(percent_encode_path_segment("a/../b?x"), "a%2F..%2Fb%3Fx");
    }

    #[test]
    fn metrics_render_without_a_doubled_suffix() {
        let telemetry = hs_telemetry::metrics::Metrics::new();
        let metrics = RemoteMediaMetrics::register(&telemetry);
        metrics.hit();
        metrics.miss();
        metrics.fetched(&Ok(FetchedMedia {
            content_type: "image/png".into(),
            upload_name: None,
            bytes: Bytes::from_static(b"12345"),
            via: FetchVia::Federation,
        }));
        metrics.fetched(&Err(RemoteFetchError::Failed("down".into())));
        let text = telemetry.encode_to_string().unwrap();
        assert!(
            text.contains("hs_media_remote_requests_total{result=\"hit\"} 1"),
            "{text}"
        );
        assert!(text.contains("hs_media_remote_requests_total{result=\"miss\"} 1"));
        assert!(
            text.contains(
                "hs_media_remote_fetches_total{outcome=\"success\",via=\"federation\"} 1"
            )
        );
        assert!(text.contains("hs_media_remote_fetches_total{outcome=\"failure\",via=\"none\"} 1"));
        assert!(text.contains("hs_media_remote_fetch_bytes_total{via=\"federation\"} 5"));
        assert!(!text.contains("_total_total"));
    }
}
