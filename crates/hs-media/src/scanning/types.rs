//! The provider interface (`docs/rfcs/0008-content-scanning.md`, section 2), implemented exactly
//! as specified: [`ContentScanner`], [`Verdict`], [`UnscannableReason`], [`ScanSource`],
//! [`ScanContext`], [`ScanTicket`] and [`ScanError`].
//!
//! The asynchronous poll path ([`ContentScanner::poll`]) is present from the start, not
//! retrofitted: [`Verdict::Pending`] and [`ScanTicket`] exist because the `http` provider's
//! CrowdStrike-shaped submit-and-poll flow needs them, and every other provider simply never
//! returns `Pending` (its `poll` is never called).

use std::time::{Duration, Instant};

use async_trait::async_trait;
use bytes::Bytes;

/// Where an upload being scanned came from, per RFC section 4's four scan points. Carried on
/// [`ScanContext`] so a provider or the engine's policy (appservice bypass, audit context) can
/// tell them apart.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ScanSourceKind {
    /// A local client's upload (`POST .../upload`, or async-upload completion).
    Local,
    /// An appservice/bridge upload through the ordinary upload path.
    Appservice,
    /// Media fetched over federation, before it enters the remote cache.
    Federation,
}

impl ScanSourceKind {
    /// The low-cardinality label used in metrics and audit entries.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            ScanSourceKind::Local => "local",
            ScanSourceKind::Appservice => "appservice",
            ScanSourceKind::Federation => "federation",
        }
    }
}

/// The deadline, uploader, origin and media identifier a [`ContentScanner`] scans under.
///
/// `deadline` is a [`std::time::Instant`] (not a `Duration`) deliberately: a provider that calls
/// `poll` in a loop must be able to check "am I still inside budget" without re-deriving a
/// deadline from a duration captured at a different instant, which is the classic bug that lets a
/// slow initial `scan` call silently eat into what should have been the poll loop's own budget.
#[derive(Debug, Clone)]
pub struct ScanContext {
    /// Wall-clock deadline for this scan (including any poll loop). Implementations MUST respect
    /// it (RFC section 2).
    pub deadline: Instant,
    /// The uploading user (`@alice:example.org`), if known. `None` for federation-fetched media,
    /// which has no local uploader.
    pub uploader: Option<String>,
    /// Where the content came from.
    pub source: ScanSourceKind,
    /// The media ID being scanned (for audit entries and provider-side correlation).
    pub media_id: String,
    /// The `server_name` the media is filed under (this server's own name for a local/appservice
    /// upload, the origin's for federation-fetched media).
    pub server_name: String,
}

/// A source of scan content: streamed chunks plus the metadata every provider needs before or
/// while reading them.
///
/// Kept as a `next_chunk`-style pull interface (not `futures::Stream`) so this crate does not need
/// a stream-combinator dependency: every provider only ever wants "read the next chunk, until
/// none remain", never `.map`/`.filter`/`.zip` over it.
pub struct ScanSource<'a> {
    /// The `Content-Type` the uploader declared. Providers are not expected to re-sniff it.
    pub content_type: String,
    /// Total size in bytes, if known up front (it always is in this crate: every call site has
    /// already buffered or measured the content before scanning — see
    /// `crate::scanning::engine`'s module doc for why streaming from the client's socket directly
    /// into the scanner is not yet implemented).
    pub size: Option<u64>,
    /// Lower-case hex-encoded SHA-256 of the full content, used as the verdict cache key's first
    /// component.
    pub sha256_hex: String,
    chunks: Box<dyn ChunkSource + Send + 'a>,
}

impl<'a> ScanSource<'a> {
    /// Builds a source from an already-buffered chunk producer.
    pub fn new(
        content_type: impl Into<String>,
        size: Option<u64>,
        sha256_hex: impl Into<String>,
        chunks: Box<dyn ChunkSource + Send + 'a>,
    ) -> Self {
        Self {
            content_type: content_type.into(),
            size,
            sha256_hex: sha256_hex.into(),
            chunks,
        }
    }

    /// Builds a source over an in-memory buffer, split into `chunk_size`-byte pieces (the SHA-256
    /// is computed here, over the whole buffer, once). This is what every scan point in this
    /// crate uses today, since uploads are already fully buffered by the time they reach
    /// `crate::repository` (see [`ScanSource`]'s field docs) — a provider still receives the
    /// content in chunks over its own wire protocol, which is what RFC section 2's "MUST stream"
    /// obligation is actually about (never holding the whole body in one write to clamd/ICAP/an
    /// HTTP body), even though the source of those chunks is one buffer rather than a live
    /// socket.
    #[must_use]
    pub fn from_bytes(content_type: impl Into<String>, bytes: Bytes, chunk_size: usize) -> Self {
        use sha2::{Digest, Sha256};
        let mut hasher = Sha256::new();
        hasher.update(&bytes);
        let sha256_hex = hex_encode(&hasher.finalize());
        let size = Some(bytes.len() as u64);
        Self::new(
            content_type,
            size,
            sha256_hex,
            Box::new(BytesChunkSource {
                bytes,
                offset: 0,
                chunk_size: chunk_size.max(1),
            }),
        )
    }

    /// Reads the next chunk, or `None` once every chunk has been read.
    ///
    /// # Errors
    /// Propagates whatever the underlying [`ChunkSource`] returns.
    pub async fn next_chunk(&mut self) -> std::io::Result<Option<Bytes>> {
        self.chunks.next_chunk().await
    }

    /// Reads every remaining chunk and concatenates them. Convenience for providers (`command`,
    /// `http`) whose transport has no benefit from chunk-at-a-time sending; still exercises the
    /// same pull interface every provider uses, so a future genuinely-streaming source works with
    /// these providers unchanged.
    ///
    /// # Errors
    /// Propagates whatever the underlying [`ChunkSource`] returns.
    pub async fn read_all(&mut self) -> std::io::Result<Bytes> {
        let mut buf = Vec::new();
        while let Some(chunk) = self.next_chunk().await? {
            buf.extend_from_slice(&chunk);
        }
        Ok(Bytes::from(buf))
    }
}

/// A pull source of content chunks. See [`ScanSource`]'s doc for why this exists instead of a
/// `futures::Stream` bound.
#[async_trait]
pub trait ChunkSource {
    /// Returns the next chunk, or `Ok(None)` once exhausted. Must not be called again after
    /// returning `Ok(None)` or `Err`.
    async fn next_chunk(&mut self) -> std::io::Result<Option<Bytes>>;
}

struct BytesChunkSource {
    bytes: Bytes,
    offset: usize,
    chunk_size: usize,
}

#[async_trait]
impl ChunkSource for BytesChunkSource {
    async fn next_chunk(&mut self) -> std::io::Result<Option<Bytes>> {
        if self.offset >= self.bytes.len() {
            return Ok(None);
        }
        let end = (self.offset + self.chunk_size).min(self.bytes.len());
        let chunk = self.bytes.slice(self.offset..end);
        self.offset = end;
        Ok(Some(chunk))
    }
}

fn hex_encode(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// Why content could not be scanned at all (as opposed to being scanned and found clean or
/// infected). RFC section 2, spelled out exactly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UnscannableReason {
    /// The content is end-to-end encrypted; the server holds ciphertext and no key. See RFC
    /// section 7 — this is the reason that must never be reported as [`Verdict::Clean`].
    Encrypted,
    /// Content exceeded the configured maximum scan size.
    TooLarge,
    /// An archive/container nested too deeply to safely recurse into.
    TooDeep,
    /// The provider does not recognize the content's format.
    UnsupportedFormat,
    /// Any other provider-specific reason, with a human-readable explanation.
    Other(String),
}

/// A ticket identifying a pending, asynchronous scan (the `http` provider's submit-and-poll
/// flow). Opaque to this crate; a provider defines its own meaning (a job ID, a URL, ...).
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct ScanTicket(pub String);

/// The outcome of a scan. RFC section 2, spelled out exactly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// The content was scanned and found clean.
    Clean,
    /// The content was scanned and found infected.
    Infected {
        /// The signature or threat name the scanner reported.
        signature: String,
        /// Any additional detail the scanner provided.
        details: Option<String>,
    },
    /// The content could not be scanned at all.
    Unscannable {
        /// Why.
        reason: UnscannableReason,
    },
    /// The scan was accepted but has not completed; poll `ticket` after `retry_after`.
    Pending {
        /// The ticket to pass to [`ContentScanner::poll`].
        ticket: ScanTicket,
        /// How long to wait before polling.
        retry_after: Duration,
    },
    /// The service returned **modified content** instead of a pass/fail verdict (RFC section
    /// 3.4). ICAP is a content-adaptation protocol, not only an antivirus one: a RESPMOD service
    /// may strip EXIF/GPS from a photograph, redact a document, transcode a format, or substitute
    /// a block page — all of these come back as `Replaced`, distinguished from
    /// [`Verdict::Infected`] because the content is not rejected, it is rewritten.
    ///
    /// Applying a `Replaced` verdict has hard limits the caller (`crate::scanning::engine`) must
    /// enforce, not this type: upload-only (never at download time), never for encrypted media,
    /// and only when the operator has explicitly opted in
    /// (`crate::scanning::config::ScanningConfig::allow_replacement`).
    Replaced {
        /// The adapted content.
        content: AdaptedContent,
        /// Which service performed the adaptation (this crate's provider id, e.g. `"icap"`).
        by: String,
        /// A human-readable reason, if the service supplied one.
        reason: Option<String>,
    },
}

/// The replacement bytes a [`Verdict::Replaced`] carries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdaptedContent {
    /// The adapted content bytes.
    pub bytes: Bytes,
    /// The adapted content's `Content-Type`, if the service changed it (a transcoding service
    /// might; an EXIF-stripping one usually would not). `None` means "unchanged".
    pub content_type: Option<String>,
}

impl Verdict {
    /// True for [`Verdict::Clean`] and an [`Verdict::Unscannable`] reason this crate always
    /// allows through unconditionally (currently none — even `Unscannable` is policy-gated, see
    /// `crate::scanning::config::UnscannablePolicy`). Kept as a narrow helper for tests, not a
    /// general policy decision (that lives in `crate::scanning::engine`).
    #[must_use]
    pub fn is_terminal(&self) -> bool {
        !matches!(self, Verdict::Pending { .. })
    }
}

/// Failure of the scan machinery itself, distinct from [`Verdict::Infected`] (a successful scan
/// that found a problem). RFC section 9's failure-mode tests are about this type.
#[derive(Debug, thiserror::Error)]
pub enum ScanError {
    /// The scan did not complete within `ScanContext::deadline`.
    #[error("scan timed out")]
    Timeout,
    /// The scanner could not be reached at all (connection refused, DNS failure, ...).
    #[error("scanner unavailable: {0}")]
    Unavailable(String),
    /// The scanner responded, but not in a way this provider understands.
    #[error("malformed response from scanner: {0}")]
    Protocol(String),
    /// [`ContentScanner::poll`] was called against a provider/ticket that does not support
    /// polling (the default `poll` implementation's error).
    #[error("this provider does not support polling for a pending verdict")]
    Unsupported,
    /// Any other provider-specific failure.
    #[error("scanner error: {0}")]
    Other(String),
}

/// A pluggable malware/content scanner. RFC section 2, implemented exactly.
///
/// Implementations MUST respect [`ScanContext::deadline`] and MUST stream rather than buffering
/// the whole body where the transport allows it (RFC section 2's obligation on `scan`).
#[async_trait]
pub trait ContentScanner: Send + Sync {
    /// Stable identity, recorded in verdict cache keys and audit entries, e.g. `"clamav"`.
    fn id(&self) -> &str;

    /// Signature or engine version, mixed into the cache key so a signature update invalidates
    /// prior verdicts. `None` means the provider cannot report one, and verdicts are then cached
    /// only for `cache.unversioned_ttl`.
    async fn engine_version(&self) -> Option<String>;

    /// Scans the content.
    ///
    /// # Errors
    /// Returns [`ScanError`] if the scanner could not be reached, timed out, or answered in a way
    /// this provider could not parse.
    async fn scan(&self, content: ScanSource<'_>, ctx: &ScanContext) -> Result<Verdict, ScanError>;

    /// Polls a previously returned [`Verdict::Pending`]. Providers that are always synchronous
    /// leave this at its default, which returns [`ScanError::Unsupported`].
    ///
    /// # Errors
    /// [`ScanError::Unsupported`] by default; otherwise as [`ContentScanner::scan`].
    async fn poll(&self, ticket: &ScanTicket) -> Result<Verdict, ScanError> {
        let _ = ticket;
        Err(ScanError::Unsupported)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn from_bytes_chunks_and_hashes_correctly() {
        let bytes = Bytes::from_static(b"0123456789");
        let mut source = ScanSource::from_bytes("text/plain", bytes.clone(), 3);
        assert_eq!(source.size, Some(10));

        let mut expected_hasher = <sha2::Sha256 as sha2::Digest>::new();
        sha2::Digest::update(&mut expected_hasher, &bytes);
        let expected = hex_encode(&sha2::Digest::finalize(expected_hasher));
        assert_eq!(source.sha256_hex, expected);

        let mut chunks = Vec::new();
        while let Some(chunk) = source.next_chunk().await.unwrap() {
            chunks.push(chunk);
        }
        assert_eq!(chunks.len(), 4); // 3,3,3,1
        let joined: Vec<u8> = chunks.into_iter().flat_map(|c| c.to_vec()).collect();
        assert_eq!(joined, bytes.to_vec());
    }

    #[tokio::test]
    async fn read_all_reassembles_the_buffer() {
        let bytes = Bytes::from_static(b"hello world");
        let mut source = ScanSource::from_bytes("text/plain", bytes.clone(), 4);
        let all = source.read_all().await.unwrap();
        assert_eq!(all, bytes);
    }

    #[test]
    fn pending_is_not_terminal() {
        let v = Verdict::Pending {
            ticket: ScanTicket("t1".into()),
            retry_after: Duration::from_secs(1),
        };
        assert!(!v.is_terminal());
        assert!(Verdict::Clean.is_terminal());
    }

    struct DefaultPollScanner;

    #[async_trait]
    impl ContentScanner for DefaultPollScanner {
        fn id(&self) -> &str {
            "default-poll-test"
        }
        async fn engine_version(&self) -> Option<String> {
            None
        }
        async fn scan(
            &self,
            _content: ScanSource<'_>,
            _ctx: &ScanContext,
        ) -> Result<Verdict, ScanError> {
            Ok(Verdict::Clean)
        }
    }

    #[tokio::test]
    async fn default_poll_is_unsupported() {
        let scanner = DefaultPollScanner;
        let err = scanner.poll(&ScanTicket("x".into())).await.unwrap_err();
        assert!(matches!(err, ScanError::Unsupported));
    }
}
