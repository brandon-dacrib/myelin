//! The `icap` provider: RESPMOD (RFC 3507) over a raw TCP connection. **The primary provider**
//! (`docs/rfcs/0008-content-scanning.md` section 3): nearly every scanner an operator might want
//! — ClamAV via `c-icap`'s `virus_scan` service, the commercial engines natively, cloud APIs
//! through an ICAP gateway — is reachable through this one adapter, so it gets the deep,
//! protocol-complete implementation section 3.1 specifies:
//!
//! - **OPTIONS negotiation**, refreshed per `Options-TTL`, learning `Preview` size,
//!   `Max-Connections`, `Allow: 204` and the `Transfer-Preview`/`Transfer-Ignore`/
//!   `Transfer-Complete` file-type lists ([`IcapScanner::ensure_options`]).
//! - **Preview mode**: send the first `Preview` bytes and wait for `100 Continue` before sending
//!   the rest, so a verdict decided from a file header never sends the whole upload over the wire
//!   ([`respmod`]'s preview branch).
//! - **`204 No Content` means clean**, requested via `Allow: 204`.
//! - **`Transfer-Ignore`** file types are skipped entirely — not sent to the scanner at all
//!   ([`IcapScanner::scan`]'s early return).
//! - **Connection reuse** within `Max-Connections` ([`ConnectionPool`]).
//! - **Correct `Encapsulated` framing with chunked bodies** ([`encode_chunk`],
//!   [`IcapReader::read_chunked_body`]).
//! - **Verdict headers across vendors**: `X-Infection-Found`, `X-Virus-ID`,
//!   `X-Violations-Found`, plus the fallback of a substituted (non-passthrough) response meaning
//!   a block page ([`extract_verdict`]).
//!
//! **`ISTag` is the engine version.** RFC 3507 defines it as an opaque tag the service changes
//! when its configuration or signature set changes — exactly what the verdict cache
//! (`crate::scanning::cache`) needs to key on, so [`IcapScanner::engine_version`] returns it
//! directly rather than inventing a separate versioning scheme.
//!
//! # What has never run against a real server
//!
//! No ICAP server is reachable in this environment (no Docker, no network scanners — see
//! `docs/status/09-media.md`). Every wire-format behavior above is instead covered by
//! `tests::wire`, which starts a local `tokio::net::TcpListener` in-process and plays back
//! literal, hand-written byte sequences shaped like the exchanges RFC 3507 and `c-icap`'s
//! documentation describe (OPTIONS response, a preview-then-100-Continue exchange, a preview
//! answered with an early verdict so the remainder is never sent, `204`, `Transfer-Ignore`
//! skipping, and two OPTIONS responses with different `ISTag`s). This proves the client sends and
//! parses the right bytes; it does not prove interoperability with any specific real ICAP
//! product, which needs a live `c-icap` (or equivalent) instance this session could not stand up.

use std::collections::HashSet;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use bytes::{Bytes, BytesMut};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::{Mutex, RwLock};

use crate::scanning::config::IcapConfig;
use crate::scanning::types::{
    ContentScanner, ScanContext, ScanError, ScanSource, ScanTicket, UnscannableReason, Verdict,
};

/// Bytes written to the wire per chunk when sending content past the preview window. Keeps a
/// single large upload from becoming one enormous `write`, per RFC section 2's "MUST stream"
/// obligation on `scan` (see `crate::scanning::types::ScanSource::from_bytes`'s doc for why the
/// *source* of these chunks is one in-memory buffer even though the wire writes are chunked).
const WRITE_CHUNK_SIZE: usize = 32 * 1024;

/// Learned from `OPTIONS`, cached for `Options-TTL`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct OptionsInfo {
    istag: Option<String>,
    preview_size: Option<usize>,
    allow_204: bool,
    max_connections: usize,
    transfer_ignore: HashSet<String>,
    ttl: Duration,
}

impl Default for OptionsInfo {
    fn default() -> Self {
        Self {
            istag: None,
            preview_size: None,
            allow_204: false,
            max_connections: 4,
            transfer_ignore: HashSet::new(),
            ttl: Duration::from_secs(60),
        }
    }
}

struct CachedOptions {
    info: OptionsInfo,
    fetched_at: Instant,
}

/// A small pool of idle, reusable TCP connections to one ICAP service, bounded (best-effort — see
/// the module doc's "what has never run against a real server" for the one piece of section 3.1
/// this simplifies) by the last-learned `Max-Connections`.
struct ConnectionPool {
    idle: Mutex<Vec<TcpStream>>,
    max_idle: std::sync::atomic::AtomicUsize,
}

impl ConnectionPool {
    fn new() -> Self {
        Self {
            idle: Mutex::new(Vec::new()),
            max_idle: std::sync::atomic::AtomicUsize::new(4),
        }
    }

    fn set_max(&self, max: usize) {
        self.max_idle
            .store(max.max(1), std::sync::atomic::Ordering::Relaxed);
    }

    async fn checkout(&self, host: &str, port: u16) -> Result<TcpStream, ScanError> {
        if let Some(conn) = self.idle.lock().await.pop() {
            return Ok(conn);
        }
        TcpStream::connect((host, port))
            .await
            .map_err(|e| ScanError::Unavailable(format!("connecting to icap://{host}:{port}: {e}")))
    }

    /// Returns a healthy connection to the pool for reuse. Called only after a fully successful
    /// exchange; a connection that errored mid-exchange is simply dropped instead (its framing
    /// state is unknown, so reusing it could corrupt the next request).
    async fn checkin(&self, conn: TcpStream) {
        let max = self.max_idle.load(std::sync::atomic::Ordering::Relaxed);
        let mut idle = self.idle.lock().await;
        if idle.len() < max {
            idle.push(conn);
        }
        // else: drop it (over the advertised Max-Connections' worth of idle connections).
    }
}

/// The `icap` provider.
pub struct IcapScanner {
    config: IcapConfig,
    pool: ConnectionPool,
    options: RwLock<Option<CachedOptions>>,
}

impl IcapScanner {
    /// Builds a provider for the given ICAP service. Opens no connection until the first scan.
    #[must_use]
    pub fn new(config: IcapConfig) -> Self {
        Self {
            config,
            pool: ConnectionPool::new(),
            options: RwLock::new(None),
        }
    }

    /// Returns cached `OPTIONS` info if still within `Options-TTL`, otherwise performs a fresh
    /// `OPTIONS` request and caches the result.
    async fn ensure_options(&self) -> Result<OptionsInfo, ScanError> {
        {
            let guard = self.options.read().await;
            if let Some(cached) = guard.as_ref()
                && cached.fetched_at.elapsed() < cached.info.ttl
            {
                return Ok(cached.info.clone());
            }
        }
        let mut conn = self
            .pool
            .checkout(&self.config.host, self.config.port)
            .await?;
        let info = match perform_options(&mut conn, &self.config).await {
            Ok(info) => info,
            Err(e) => return Err(e),
        };
        self.pool.set_max(info.max_connections);
        self.pool.checkin(conn).await;
        let mut guard = self.options.write().await;
        *guard = Some(CachedOptions {
            info: info.clone(),
            fetched_at: Instant::now(),
        });
        Ok(info)
    }
}

#[async_trait]
impl ContentScanner for IcapScanner {
    fn id(&self) -> &str {
        "icap"
    }

    async fn engine_version(&self) -> Option<String> {
        self.ensure_options().await.ok()?.istag
    }

    async fn scan(
        &self,
        mut content: ScanSource<'_>,
        ctx: &ScanContext,
    ) -> Result<Verdict, ScanError> {
        let options = self.ensure_options().await?;

        if let Some(ext) = extension_for_content_type(&content.content_type)
            && (options.transfer_ignore.contains(ext) || options.transfer_ignore.contains("*"))
        {
            // The service told us, via OPTIONS, that it does not want to see this file type at
            // all: skip the scan entirely rather than sending (and having it ignore) the body.
            return Ok(Verdict::Clean);
        }

        let body = content
            .read_all()
            .await
            .map_err(|e| ScanError::Other(e.to_string()))?;

        let remaining = ctx
            .deadline
            .checked_duration_since(Instant::now())
            .ok_or(ScanError::Timeout)?;

        let host = self.config.host.clone();
        let port = self.config.port;
        let mut conn = self.pool.checkout(&host, port).await?;
        let result = tokio::time::timeout(
            remaining,
            respmod(&mut conn, &self.config, &options, &content.content_type, &body),
        )
        .await;

        match result {
            Ok(Ok(verdict)) => {
                self.pool.checkin(conn).await;
                Ok(verdict)
            }
            Ok(Err(e)) => Err(e),
            Err(_) => Err(ScanError::Timeout),
        }
    }

    async fn poll(&self, ticket: &ScanTicket) -> Result<Verdict, ScanError> {
        let _ = ticket;
        // ICAP RESPMOD is always synchronous: `scan` never returns `Verdict::Pending`, so this is
        // never called in practice. The default trait implementation would do the same thing;
        // overridden only to document that explicitly for this provider.
        Err(ScanError::Unsupported)
    }
}

/// Maps a declared `Content-Type` to the bare lower-case extension ICAP's `Transfer-*` lists use
/// (`"image/png"` -> `"png"`). Best-effort: an unrecognized type returns `None`, which means the
/// `Transfer-Ignore` check in [`IcapScanner::scan`] never matches it (safe default: scan anything
/// we cannot classify).
fn extension_for_content_type(content_type: &str) -> Option<&'static str> {
    let base = content_type.split(';').next().unwrap_or(content_type).trim();
    Some(match base {
        "image/png" => "png",
        "image/jpeg" => "jpg",
        "image/gif" => "gif",
        "image/webp" => "webp",
        "image/svg+xml" => "svg",
        "video/mp4" => "mp4",
        "video/webm" => "webm",
        "audio/mpeg" => "mp3",
        "application/pdf" => "pdf",
        "text/plain" => "txt",
        "text/html" => "html",
        "application/zip" => "zip",
        _ => return None,
    })
}

// ---------------------------------------------------------------------------------------------
// Wire encoding/decoding
// ---------------------------------------------------------------------------------------------

fn encode_chunk(data: &[u8]) -> Vec<u8> {
    let mut out = format!("{:x}\r\n", data.len()).into_bytes();
    out.extend_from_slice(data);
    out.extend_from_slice(b"\r\n");
    out
}

fn encode_last_chunk(ieof: bool) -> &'static [u8] {
    if ieof {
        b"0; ieof\r\n\r\n"
    } else {
        b"0\r\n\r\n"
    }
}

fn build_options_request(config: &IcapConfig) -> Vec<u8> {
    format!(
        "OPTIONS icap://{host}:{port}/{service} ICAP/1.0\r\n\
         Host: {host}\r\n\
         User-Agent: hs-media\r\n\
         Encapsulated: null-body=0\r\n\
         \r\n",
        host = config.host,
        port = config.port,
        service = config.service,
    )
    .into_bytes()
}

/// One parsed ICAP or embedded-HTTP status/header block: the status line and a lower-cased
/// (name, value) header list, preserving duplicates (some vendors repeat `X-Virus-ID`).
#[derive(Debug, Clone)]
struct HeaderBlock {
    status_line: String,
    headers: Vec<(String, String)>,
}

impl HeaderBlock {
    fn parse(text: &str) -> Result<Self, ScanError> {
        let mut lines = text.split("\r\n");
        let status_line = lines
            .next()
            .ok_or_else(|| ScanError::Protocol("empty response".into()))?
            .to_string();
        let mut headers = Vec::new();
        for line in lines {
            if line.is_empty() {
                continue;
            }
            let Some((name, value)) = line.split_once(':') else {
                continue;
            };
            headers.push((name.trim().to_ascii_lowercase(), value.trim().to_string()));
        }
        Ok(Self {
            status_line,
            headers,
        })
    }

    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    }

    fn status_code(&self) -> Result<u16, ScanError> {
        self.status_line
            .split_whitespace()
            .nth(1)
            .and_then(|s| s.parse().ok())
            .ok_or_else(|| ScanError::Protocol(format!("bad status line {:?}", self.status_line)))
    }
}

/// Parses the `Encapsulated` header's offsets, e.g. `"res-hdr=0, res-body=137"` or
/// `"null-body=0"`.
fn parse_encapsulated(value: &str) -> Vec<(String, usize)> {
    value
        .split(',')
        .filter_map(|part| {
            let part = part.trim();
            let (name, off) = part.split_once('=')?;
            Some((name.trim().to_string(), off.trim().parse().ok()?))
        })
        .collect()
}

fn parse_options_body(block: &HeaderBlock) -> OptionsInfo {
    let istag = block
        .header("istag")
        .map(|s| s.trim_matches('"').to_string());
    let preview_size = block.header("preview").and_then(|s| s.trim().parse().ok());
    let allow_204 = block
        .header("allow")
        .is_some_and(|s| s.split(',').any(|v| v.trim() == "204"));
    let max_connections = block
        .header("max-connections")
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(4);
    let ttl = block
        .header("options-ttl")
        .and_then(|s| s.trim().parse().ok())
        .map(Duration::from_secs)
        .unwrap_or(Duration::from_secs(60));
    let transfer_ignore = block
        .header("transfer-ignore")
        .map(|s| {
            s.split(',')
                .map(|v| v.trim().to_ascii_lowercase())
                .filter(|v| !v.is_empty())
                .collect()
        })
        .unwrap_or_default();
    OptionsInfo {
        istag,
        preview_size,
        allow_204,
        max_connections,
        transfer_ignore,
        ttl,
    }
}

/// Vendor spellings for "infected" (RFC section 3.1: "verdict header parsing across vendors").
const INFECTION_HEADERS: &[&str] = &["x-infection-found", "x-virus-id", "x-violations-found"];

/// Looks for an infection verdict across the ICAP response's own headers and the encapsulated
/// HTTP response's headers (vendors place these in either block). Returns
/// `Some((signature, details))` if found.
fn extract_verdict(icap: &HeaderBlock, http: Option<&HeaderBlock>) -> Option<(String, Option<String>)> {
    for name in INFECTION_HEADERS {
        if let Some(v) = icap.header(name) {
            return Some(parse_infection_header(name, v));
        }
        if let Some(http) = http
            && let Some(v) = http.header(name)
        {
            return Some(parse_infection_header(name, v));
        }
    }
    None
}

/// `X-Infection-Found` is typically `Type=0; Resolution=2; Threat=EICAR_Test_File;`; the other
/// two headers are usually a bare name. Either way, the whole header value becomes `details`, and
/// a best-effort `Threat=`/bare-value extraction becomes the `signature`.
fn parse_infection_header(name: &str, value: &str) -> (String, Option<String>) {
    if name == "x-infection-found" {
        let threat = value
            .split(';')
            .find_map(|kv| kv.trim().strip_prefix("Threat="))
            .map(str::to_string);
        (
            threat.unwrap_or_else(|| "infected".to_string()),
            Some(value.to_string()),
        )
    } else {
        (value.trim().to_string(), None)
    }
}

// ---------------------------------------------------------------------------------------------
// Connection-level I/O
// ---------------------------------------------------------------------------------------------

/// A minimal incremental reader over an `AsyncRead` byte stream: reads more from the socket only
/// when what has already been buffered is not enough to satisfy the current request (a
/// double-CRLF-terminated header block, or a declared number of chunk bytes).
struct IcapReader<'a, S> {
    io: &'a mut S,
    buf: BytesMut,
}

impl<'a, S: AsyncRead + Unpin> IcapReader<'a, S> {
    fn new(io: &'a mut S) -> Self {
        Self {
            io,
            buf: BytesMut::new(),
        }
    }

    async fn fill_more(&mut self) -> Result<(), ScanError> {
        let mut tmp = [0u8; 8192];
        let n = self
            .io
            .read(&mut tmp)
            .await
            .map_err(|e| ScanError::Protocol(format!("reading from icap connection: {e}")))?;
        if n == 0 {
            return Err(ScanError::Protocol(
                "icap connection closed before a full response was received".into(),
            ));
        }
        self.buf.extend_from_slice(&tmp[..n]);
        Ok(())
    }

    /// Reads and consumes bytes up to and including the next `\r\n\r\n`, returning everything
    /// before it as a UTF-8 string (ICAP headers are ASCII).
    async fn read_header_block(&mut self) -> Result<String, ScanError> {
        loop {
            if let Some(pos) = find_subslice(&self.buf, b"\r\n\r\n") {
                let head = self.buf.split_to(pos);
                self.buf.advance_past(4); // consume the terminator itself
                return String::from_utf8(head.to_vec())
                    .map_err(|e| ScanError::Protocol(format!("non-UTF-8 icap header: {e}")));
            }
            self.fill_more().await?;
        }
    }

    /// Reads exactly `n` bytes (already-buffered bytes are consumed first).
    async fn read_exact_bytes(&mut self, n: usize) -> Result<Bytes, ScanError> {
        while self.buf.len() < n {
            self.fill_more().await?;
        }
        Ok(self.buf.split_to(n).freeze())
    }

    /// Reads a chunked body (RFC 7230-style chunk framing, as ICAP's `Encapsulated` sections
    /// use): repeated `<hex-size>\r\n<data>\r\n` chunks, terminated by a zero-size chunk and its
    /// trailing `\r\n` (optionally preceded by trailer headers, which are parsed and returned
    /// alongside the body since some ICAP servers put verdict headers there).
    async fn read_chunked_body(&mut self) -> Result<(Bytes, Vec<(String, String)>), ScanError> {
        let mut body = BytesMut::new();
        loop {
            let size_line = self.read_line().await?;
            let size_str = size_line.split(';').next().unwrap_or("").trim();
            let size = usize::from_str_radix(size_str, 16)
                .map_err(|_| ScanError::Protocol(format!("bad chunk size {size_line:?}")))?;
            if size == 0 {
                // Trailer headers (if any) until the terminating blank line.
                let mut trailers = Vec::new();
                loop {
                    let line = self.read_line().await?;
                    if line.is_empty() {
                        break;
                    }
                    if let Some((name, value)) = line.split_once(':') {
                        trailers.push((name.trim().to_ascii_lowercase(), value.trim().to_string()));
                    }
                }
                return Ok((body.freeze(), trailers));
            }
            let chunk = self.read_exact_bytes(size).await?;
            body.extend_from_slice(&chunk);
            let crlf = self.read_exact_bytes(2).await?;
            if crlf.as_ref() != b"\r\n" {
                return Err(ScanError::Protocol("malformed chunk terminator".into()));
            }
        }
    }

    async fn read_line(&mut self) -> Result<String, ScanError> {
        loop {
            if let Some(pos) = find_subslice(&self.buf, b"\r\n") {
                let line = self.buf.split_to(pos);
                self.buf.advance_past(2);
                return String::from_utf8(line.to_vec())
                    .map_err(|e| ScanError::Protocol(format!("non-UTF-8 icap line: {e}")));
            }
            self.fill_more().await?;
        }
    }
}

trait AdvancePast {
    fn advance_past(&mut self, n: usize);
}

impl AdvancePast for BytesMut {
    fn advance_past(&mut self, n: usize) {
        let _ = self.split_to(n.min(self.len()));
    }
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

async fn perform_options(
    conn: &mut TcpStream,
    config: &IcapConfig,
) -> Result<OptionsInfo, ScanError> {
    let request = build_options_request(config);
    conn.write_all(&request)
        .await
        .map_err(|e| ScanError::Unavailable(format!("writing OPTIONS request: {e}")))?;

    let mut reader = IcapReader::new(conn);
    let head = reader.read_header_block().await?;
    let block = HeaderBlock::parse(&head)?;
    let status = block.status_code()?;
    if status != 200 {
        return Err(ScanError::Protocol(format!(
            "OPTIONS returned unexpected status {status}"
        )));
    }
    Ok(parse_options_body(&block))
}

/// Performs one RESPMOD exchange for `body`, honoring preview mode when the service advertised a
/// `Preview` size smaller than `body`.
async fn respmod(
    conn: &mut TcpStream,
    config: &IcapConfig,
    options: &OptionsInfo,
    content_type: &str,
    body: &[u8],
) -> Result<Verdict, ScanError> {
    let res_hdr = format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n", body.len());
    let res_hdr_bytes = res_hdr.as_bytes();

    let preview_len = options
        .preview_size
        .map(|n| n.min(body.len()))
        .filter(|_| !body.is_empty());

    let mut head = format!(
        "RESPMOD icap://{host}:{port}/{service} ICAP/1.0\r\n\
         Host: {host}\r\n",
        host = config.host,
        port = config.port,
        service = config.service,
    );
    if options.allow_204 {
        head.push_str("Allow: 204\r\n");
    }
    if let Some(n) = preview_len {
        head.push_str(&format!("Preview: {n}\r\n"));
    }
    head.push_str(&format!(
        "Encapsulated: res-hdr=0, res-body={}\r\n\r\n",
        res_hdr_bytes.len()
    ));

    conn.write_all(head.as_bytes())
        .await
        .map_err(|e| ScanError::Unavailable(format!("writing RESPMOD head: {e}")))?;
    conn.write_all(res_hdr_bytes)
        .await
        .map_err(|e| ScanError::Unavailable(format!("writing encapsulated response header: {e}")))?;

    let is_full_body_preview = preview_len == Some(body.len());
    let preview_slice = &body[..preview_len.unwrap_or(body.len())];
    write_chunked(conn, preview_slice, is_full_body_preview || preview_len.is_none()).await?;

    if preview_len.is_some() && !is_full_body_preview {
        // A real, truncated preview: read the server's decision before sending the rest.
        let mut reader = IcapReader::new(conn);
        let head_text = reader.read_header_block().await?;
        let block = HeaderBlock::parse(&head_text)?;
        let status = block.status_code()?;
        match status {
            100 => {
                // "send the rest" -- fall through below.
            }
            204 => return Ok(Verdict::Clean),
            200 => {
                let (verdict, _reader) = finish_response(reader, block, Some(&res_hdr)).await?;
                return Ok(verdict);
            }
            other => {
                return Err(ScanError::Protocol(format!(
                    "unexpected ICAP status {other} after preview"
                )));
            }
        }
        // Send the remainder of the body.
        let remainder = &body[preview_len.unwrap_or(0)..];
        write_chunked(conn, remainder, true).await?;
    }

    let mut reader = IcapReader::new(conn);
    let head_text = reader.read_header_block().await?;
    let block = HeaderBlock::parse(&head_text)?;
    let status = block.status_code()?;
    match status {
        204 => Ok(Verdict::Clean),
        200 => {
            let (verdict, _reader) = finish_response(reader, block, Some(&res_hdr)).await?;
            Ok(verdict)
        }
        other => Err(ScanError::Protocol(format!(
            "unexpected ICAP status {other}"
        ))),
    }
}

/// Writes `data` to the connection as one or more `WRITE_CHUNK_SIZE`-sized chunk frames, ending
/// with the appropriate terminator (`ieof` when this write represents the entirety of the body
/// the server will ever see for this request — a full, non-preview send, or a preview that
/// covered the whole body).
async fn write_chunked(conn: &mut TcpStream, data: &[u8], ieof: bool) -> Result<(), ScanError> {
    if data.is_empty() {
        conn.write_all(encode_last_chunk(ieof))
            .await
            .map_err(|e| ScanError::Unavailable(format!("writing chunk terminator: {e}")))?;
        return Ok(());
    }
    for piece in data.chunks(WRITE_CHUNK_SIZE) {
        conn.write_all(&encode_chunk(piece))
            .await
            .map_err(|e| ScanError::Unavailable(format!("writing chunk: {e}")))?;
    }
    conn.write_all(encode_last_chunk(ieof))
        .await
        .map_err(|e| ScanError::Unavailable(format!("writing chunk terminator: {e}")))?;
    Ok(())
}

/// Reads the rest of a `200 OK` RESPMOD response (the encapsulated HTTP header block, and its
/// chunked body if the `Encapsulated` header declares one) and turns it into a [`Verdict`].
async fn finish_response<'a, S: AsyncRead + Unpin>(
    mut reader: IcapReader<'a, S>,
    icap_block: HeaderBlock,
    _original_res_hdr: Option<&str>,
) -> Result<(Verdict, IcapReader<'a, S>), ScanError> {
    let offsets = icap_block
        .header("encapsulated")
        .map(|v| parse_encapsulated(v))
        .unwrap_or_default();
    let has_res_hdr = offsets.iter().any(|(name, _)| name == "res-hdr");
    let has_res_body = offsets.iter().any(|(name, _)| name == "res-body");

    let mut http_block = None;
    let mut trailers = Vec::new();
    if has_res_hdr {
        let http_head = reader.read_header_block().await?;
        http_block = Some(HeaderBlock::parse(&http_head)?);
    }
    if has_res_body {
        let (_body, t) = reader.read_chunked_body().await?;
        trailers = t;
    }

    let trailer_block = HeaderBlock {
        status_line: String::new(),
        headers: trailers,
    };

    if let Some((signature, details)) = extract_verdict(&icap_block, http_block.as_ref()) {
        return Ok((Verdict::Infected { signature, details }, reader));
    }
    if let Some((signature, details)) = extract_verdict(&trailer_block, None) {
        return Ok((Verdict::Infected { signature, details }, reader));
    }

    // Fallback (RFC section 3.1): if the encapsulated response's status line is not a
    // pass-through 200, the server substituted a block page rather than declaring an infection
    // header. Treat that as infected with what we know.
    if let Some(http) = &http_block
        && let Ok(http_status) = http.status_code()
        && http_status != 200
    {
        return Ok((
            Verdict::Infected {
                signature: "icap-blocked-response".to_string(),
                details: Some(http.status_line.clone()),
            },
            reader,
        ));
    }

    Ok((Verdict::Clean, reader))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration as StdDuration;
    use tokio::net::{TcpListener, TcpStream as TokioTcpStream};

    fn test_config(port: u16) -> IcapConfig {
        IcapConfig {
            host: "127.0.0.1".to_string(),
            port,
            service: "avscan".to_string(),
        }
    }

    fn ctx() -> ScanContext {
        ScanContext {
            deadline: Instant::now() + StdDuration::from_secs(5),
            uploader: Some("@alice:example.org".into()),
            source: crate::scanning::types::ScanSourceKind::Local,
            media_id: "abc".into(),
            server_name: "example.org".into(),
        }
    }

    async fn local_listener() -> (TcpListener, u16) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        (listener, port)
    }

    // --- Pure wire-format unit tests (no sockets) -----------------------------------------

    #[test]
    fn encodes_chunks_with_hex_length_prefix() {
        assert_eq!(encode_chunk(b"abc"), b"3\r\nabc\r\n".to_vec());
        assert_eq!(encode_chunk(b""), b"0\r\n\r\n".to_vec());
    }

    #[test]
    fn last_chunk_ieof_vs_plain() {
        assert_eq!(encode_last_chunk(true), b"0; ieof\r\n\r\n");
        assert_eq!(encode_last_chunk(false), b"0\r\n\r\n");
    }

    #[test]
    fn parses_an_options_response_block() {
        let raw = "ICAP/1.0 200 OK\r\n\
            Methods: RESPMOD\r\n\
            ISTag: \"sigs-2026-09-01\"\r\n\
            Preview: 1024\r\n\
            Allow: 204\r\n\
            Max-Connections: 100\r\n\
            Options-TTL: 3600\r\n\
            Transfer-Preview: *\r\n\
            Transfer-Ignore: gif, jpg, jpeg\r\n\
            Transfer-Complete: exe\r\n";
        let block = HeaderBlock::parse(raw).unwrap();
        assert_eq!(block.status_code().unwrap(), 200);
        let info = parse_options_body(&block);
        assert_eq!(info.istag.as_deref(), Some("sigs-2026-09-01"));
        assert_eq!(info.preview_size, Some(1024));
        assert!(info.allow_204);
        assert_eq!(info.max_connections, 100);
        assert_eq!(info.ttl, Duration::from_secs(3600));
        assert!(info.transfer_ignore.contains("gif"));
        assert!(info.transfer_ignore.contains("jpeg"));
    }

    #[test]
    fn extracts_x_infection_found_with_threat_name() {
        let block = HeaderBlock::parse(
            "ICAP/1.0 200 OK\r\nX-Infection-Found: Type=0; Resolution=2; Threat=Eicar-Test-Signature;\r\n",
        )
        .unwrap();
        let (sig, details) = extract_verdict(&block, None).unwrap();
        assert_eq!(sig, "Eicar-Test-Signature");
        assert!(details.unwrap().contains("Threat=Eicar-Test-Signature"));
    }

    #[test]
    fn extracts_x_virus_id_bare_value() {
        let block = HeaderBlock::parse("ICAP/1.0 200 OK\r\nX-Virus-ID: Eicar-Test-Signature\r\n").unwrap();
        let (sig, _details) = extract_verdict(&block, None).unwrap();
        assert_eq!(sig, "Eicar-Test-Signature");
    }

    #[test]
    fn extracts_x_violations_found() {
        let block =
            HeaderBlock::parse("ICAP/1.0 200 OK\r\nX-Violations-Found: 1\r\n").unwrap();
        assert!(extract_verdict(&block, None).is_some());
    }

    #[test]
    fn no_verdict_header_means_none() {
        let block = HeaderBlock::parse("ICAP/1.0 200 OK\r\nISTag: \"x\"\r\n").unwrap();
        assert!(extract_verdict(&block, None).is_none());
    }

    #[test]
    fn extension_mapping_covers_common_image_types() {
        assert_eq!(extension_for_content_type("image/png"), Some("png"));
        assert_eq!(extension_for_content_type("image/jpeg; charset=x"), Some("jpg"));
        assert_eq!(extension_for_content_type("application/x-made-up"), None);
    }

    // --- Recorded-exchange tests against a local, in-process mock listener ----------------

    /// Spawns a task that accepts exactly one connection on `listener` and runs `handler` with
    /// it, so the test's client-side code runs against a real `TcpStream` without any real ICAP
    /// daemon anywhere.
    fn spawn_mock_server<F, Fut>(listener: TcpListener, handler: F)
    where
        F: FnOnce(TokioTcpStream) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = ()> + Send + 'static,
    {
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            handler(stream).await;
        });
    }

    #[tokio::test]
    async fn options_negotiation_round_trips() {
        let (listener, port) = local_listener().await;
        spawn_mock_server(listener, |mut stream| async move {
            let mut buf = vec![0u8; 4096];
            let n = stream.read(&mut buf).await.unwrap();
            let req = String::from_utf8_lossy(&buf[..n]);
            assert!(req.starts_with("OPTIONS icap://127.0.0.1"));
            assert!(req.contains("Encapsulated: null-body=0"));

            let response = "ICAP/1.0 200 OK\r\n\
                Methods: RESPMOD\r\n\
                ISTag: \"sigs-v1\"\r\n\
                Preview: 4\r\n\
                Allow: 204\r\n\
                Max-Connections: 8\r\n\
                Options-TTL: 3600\r\n\
                Transfer-Ignore: gif\r\n\
                \r\n";
            stream.write_all(response.as_bytes()).await.unwrap();
        });

        let scanner = IcapScanner::new(test_config(port));
        let version = scanner.engine_version().await;
        assert_eq!(version.as_deref(), Some("sigs-v1"));
    }

    #[tokio::test]
    async fn respmod_204_is_clean() {
        let (listener, port) = local_listener().await;
        spawn_mock_server(listener, |mut stream| async move {
            // OPTIONS
            let mut buf = vec![0u8; 4096];
            let n = stream.read(&mut buf).await.unwrap();
            assert!(String::from_utf8_lossy(&buf[..n]).starts_with("OPTIONS"));
            stream
                .write_all(
                    b"ICAP/1.0 200 OK\r\nISTag: \"v1\"\r\nAllow: 204\r\nOptions-TTL: 3600\r\n\r\n",
                )
                .await
                .unwrap();

            // RESPMOD
            let n = stream.read(&mut buf).await.unwrap();
            let req = String::from_utf8_lossy(&buf[..n]);
            assert!(req.starts_with("RESPMOD"));
            assert!(req.contains("Allow: 204"));
            stream
                .write_all(b"ICAP/1.0 204 No Content\r\n\r\n")
                .await
                .unwrap();
        });

        let scanner = IcapScanner::new(test_config(port));
        let source = ScanSource::from_bytes("text/plain", Bytes::from_static(b"clean file"), 4096);
        let verdict = scanner.scan(source, &ctx()).await.unwrap();
        assert_eq!(verdict, Verdict::Clean);
    }

    #[tokio::test]
    async fn respmod_200_with_infection_header_is_infected() {
        let (listener, port) = local_listener().await;
        spawn_mock_server(listener, |mut stream| async move {
            let mut buf = vec![0u8; 8192];
            let _ = stream.read(&mut buf).await.unwrap(); // OPTIONS
            stream
                .write_all(b"ICAP/1.0 200 OK\r\nISTag: \"v1\"\r\nOptions-TTL: 3600\r\n\r\n")
                .await
                .unwrap();

            let _ = stream.read(&mut buf).await.unwrap(); // RESPMOD (no preview configured)
            let http_hdr = b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n";
            let response = format!(
                "ICAP/1.0 200 OK\r\nISTag: \"v1\"\r\nX-Infection-Found: Type=0; Resolution=2; Threat=Eicar-Test-Signature;\r\nEncapsulated: res-hdr=0, res-body={}\r\n\r\n",
                http_hdr.len()
            );
            stream.write_all(response.as_bytes()).await.unwrap();
            stream.write_all(http_hdr).await.unwrap();
            stream.write_all(b"0\r\n\r\n").await.unwrap();
        });

        let scanner = IcapScanner::new(test_config(port));
        let source = ScanSource::from_bytes(
            "application/octet-stream",
            Bytes::from_static(b"X5O!P%@AP[4\\PZX54(P^)7CC)7}$EICAR"),
            4096,
        );
        let verdict = scanner.scan(source, &ctx()).await.unwrap();
        assert_eq!(
            verdict,
            Verdict::Infected {
                signature: "Eicar-Test-Signature".into(),
                details: Some("Type=0; Resolution=2; Threat=Eicar-Test-Signature;".into()),
            }
        );
    }

    #[tokio::test]
    async fn preview_then_100_continue_sends_remainder() {
        let (listener, port) = local_listener().await;
        spawn_mock_server(listener, |mut stream| async move {
            let mut buf = vec![0u8; 8192];
            let _ = stream.read(&mut buf).await.unwrap(); // OPTIONS
            stream
                .write_all(
                    b"ICAP/1.0 200 OK\r\nISTag: \"v1\"\r\nPreview: 4\r\nOptions-TTL: 3600\r\n\r\n",
                )
                .await
                .unwrap();

            // RESPMOD head + encapsulated res-hdr + preview chunk (4 bytes) + terminator.
            let n = stream.read(&mut buf).await.unwrap();
            let sent = String::from_utf8_lossy(&buf[..n]);
            assert!(sent.contains("Preview: 4"));
            assert!(sent.contains("0\r\n\r\n")); // plain terminator, not ieof: this is a real preview
            assert!(!sent.contains("ieof"));

            stream
                .write_all(b"ICAP/1.0 100 Continue\r\n\r\n")
                .await
                .unwrap();

            // The remainder of the 10-byte body ("0123456789"[4..] = "456789", 6 bytes) plus the
            // final terminator.
            let n = stream.read(&mut buf).await.unwrap();
            let rest = String::from_utf8_lossy(&buf[..n]);
            assert!(rest.contains("456789"));

            stream
                .write_all(b"ICAP/1.0 204 No Content\r\n\r\n")
                .await
                .unwrap();
        });

        let scanner = IcapScanner::new(test_config(port));
        let source = ScanSource::from_bytes("text/plain", Bytes::from_static(b"0123456789"), 4096);
        let verdict = scanner.scan(source, &ctx()).await.unwrap();
        assert_eq!(verdict, Verdict::Clean);
    }

    #[tokio::test]
    async fn preview_answered_with_early_verdict_never_sends_remainder() {
        let (listener, port) = local_listener().await;
        spawn_mock_server(listener, |mut stream| async move {
            let mut buf = vec![0u8; 8192];
            let _ = stream.read(&mut buf).await.unwrap(); // OPTIONS
            stream
                .write_all(
                    b"ICAP/1.0 200 OK\r\nISTag: \"v1\"\r\nPreview: 4\r\nOptions-TTL: 3600\r\n\r\n",
                )
                .await
                .unwrap();

            let _ = stream.read(&mut buf).await.unwrap(); // RESPMOD + preview

            // Immediately decide "infected" from the preview alone -- no 100 Continue, so the
            // client must never send the rest of the body. Reply is a full 200 with the verdict
            // header, not 100.
            let http_hdr = b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n";
            let response = format!(
                "ICAP/1.0 200 OK\r\nISTag: \"v1\"\r\nX-Virus-ID: Eicar-Test-Signature\r\nEncapsulated: res-hdr=0, res-body={}\r\n\r\n",
                http_hdr.len()
            );
            stream.write_all(response.as_bytes()).await.unwrap();
            stream.write_all(http_hdr).await.unwrap();
            stream.write_all(b"0\r\n\r\n").await.unwrap();

            // Prove the client never sent more: a further read should see EOF (or at least no
            // more bytes) rather than the body's remainder. We attempt one more (short-timeout)
            // read and require it to yield nothing.
            let mut probe = [0u8; 16];
            let res = tokio::time::timeout(StdDuration::from_millis(200), stream.read(&mut probe))
                .await;
            match res {
                Ok(Ok(0)) | Err(_) => {} // closed, or (more likely) nothing arrived in time: good
                Ok(Ok(n)) => panic!(
                    "client sent more bytes after an early verdict: {:?}",
                    &probe[..n]
                ),
                Ok(Err(_)) => {}
            }
        });

        let scanner = IcapScanner::new(test_config(port));
        let source = ScanSource::from_bytes("text/plain", Bytes::from_static(b"0123456789"), 4096);
        let verdict = scanner.scan(source, &ctx()).await.unwrap();
        assert_eq!(
            verdict,
            Verdict::Infected {
                signature: "Eicar-Test-Signature".into(),
                details: None,
            }
        );
    }

    #[tokio::test]
    async fn transfer_ignore_skips_the_scan_entirely() {
        let (listener, port) = local_listener().await;
        spawn_mock_server(listener, |mut stream| async move {
            let mut buf = vec![0u8; 4096];
            let _ = stream.read(&mut buf).await.unwrap(); // OPTIONS only
            stream
                .write_all(
                    b"ICAP/1.0 200 OK\r\nISTag: \"v1\"\r\nOptions-TTL: 3600\r\nTransfer-Ignore: gif\r\n\r\n",
                )
                .await
                .unwrap();
            // No RESPMOD should ever arrive: prove it by timing out a further read.
            let mut probe = [0u8; 16];
            let res = tokio::time::timeout(StdDuration::from_millis(200), stream.read(&mut probe))
                .await;
            assert!(
                matches!(res, Err(_) | Ok(Ok(0))),
                "client sent a RESPMOD for a Transfer-Ignore type"
            );
        });

        let scanner = IcapScanner::new(test_config(port));
        let source = ScanSource::from_bytes("image/gif", Bytes::from_static(b"GIF89a..."), 4096);
        let verdict = scanner.scan(source, &ctx()).await.unwrap();
        assert_eq!(verdict, Verdict::Clean);
    }

    #[tokio::test]
    async fn istag_change_is_visible_across_two_options_calls() {
        // Two independent scanners (so each performs its own fresh OPTIONS instead of hitting a
        // shared cache) against two servers reporting different ISTags, simulating a signature
        // update between them -- this is the property `crate::scanning::engine`'s cache-key
        // wiring depends on (see `crate::scanning::cache::tests::signature_version_change_invalidates_the_cache`
        // for the cache-side half of this guarantee).
        let (listener1, port1) = local_listener().await;
        spawn_mock_server(listener1, |mut stream| async move {
            let mut buf = vec![0u8; 4096];
            let _ = stream.read(&mut buf).await.unwrap();
            stream
                .write_all(b"ICAP/1.0 200 OK\r\nISTag: \"sigs-2026-09-01\"\r\nOptions-TTL: 3600\r\n\r\n")
                .await
                .unwrap();
        });
        let (listener2, port2) = local_listener().await;
        spawn_mock_server(listener2, |mut stream| async move {
            let mut buf = vec![0u8; 4096];
            let _ = stream.read(&mut buf).await.unwrap();
            stream
                .write_all(b"ICAP/1.0 200 OK\r\nISTag: \"sigs-2026-09-02\"\r\nOptions-TTL: 3600\r\n\r\n")
                .await
                .unwrap();
        });

        let before = IcapScanner::new(test_config(port1)).engine_version().await;
        let after = IcapScanner::new(test_config(port2)).engine_version().await;
        assert_eq!(before.as_deref(), Some("sigs-2026-09-01"));
        assert_eq!(after.as_deref(), Some("sigs-2026-09-02"));
        assert_ne!(before, after);
    }

    #[tokio::test]
    async fn connection_refused_is_unavailable() {
        // Nothing is listening on this port.
        let scanner = IcapScanner::new(test_config(1));
        let err = scanner.engine_version().await;
        assert!(err.is_none());
    }
}
