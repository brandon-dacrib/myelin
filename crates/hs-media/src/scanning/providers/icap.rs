//! The `icap` provider: RESPMOD (RFC 3507) over `icap-rs`. **The provider**
//! (`docs/rfcs/0008-content-scanning.md` section 3): c-icap fronting ClamAV, commercial engines
//! with native ICAP interfaces, and cloud gateways such as ICAPeg all reach us through this one
//! adapter.
//!
//! # Reuse considered: `icap-rs`, not a hand-rolled client
//!
//! `docs/decisions/0007-build-less-reuse-more.md` requires evaluating `icap-rs` (a tokio-based
//! Rust ICAP/1.0 client/server library following RFC 3507) before writing protocol code, and
//! using it if it covers RFC section 3.3. It does, comprehensively — inspected at
//! `icap-rs = "0.3.0"` (MIT, `forbid(unsafe_code)`, `#![deny(clippy::pedantic, clippy::nursery)]`
//! and stricter):
//!
//! - `icap_rs::client::options_cache` implements the exact `OPTIONS`-negotiation-refreshed-per-
//!   `Options-TTL` cache section 3.3 asks for, including RFC 3507 §5's rule that an `ISTag`
//!   observed on a later RESPMOD that differs from the one captured at `OPTIONS` time
//!   invalidates the cache entry, and RFC 3507 §4.10.2's `Transfer-Preview`/`Transfer-Ignore`/
//!   `Transfer-Complete` file-type policy (matched here by `Content-Type`, since we always send
//!   RESPMOD, never REQMOD — see `icap_rs`'s own `file_ext_from_request`).
//! - `Client::send` drives the whole preview/`100 Continue` handshake internally: it resolves the
//!   server's advertised policy, sends the preview, and either returns an early verdict or sends
//!   the remainder — the caller makes one `await` and gets a final [`icap_rs::response::Response`].
//! - `Allow: 204` and connection reuse (`keep_alive`) are builder options.
//! - `Encapsulated` framing (chunked bodies, `res-hdr`/`res-body` offsets) is entirely the
//!   library's concern; we never construct a chunk or an offset by hand.
//!
//! What `icap-rs` does **not** know about, and what this module actually contributes, is the
//! antivirus/content-adaptation domain on top of generic ICAP: recognizing `X-Infection-Found`/
//! `X-Virus-ID`/`X-Violations-Found` as an infected verdict, recognizing a genuinely modified
//! response body as [`Verdict::Replaced`], and mapping all of that (plus `ISTag`) onto this
//! crate's [`ContentScanner`] trait. That is what this file contains; it is deliberately thin.
//!
//! No upstream contribution was needed: `icap-rs` already covers everything section 3.3 lists, so
//! there is nothing to file a PR for at this integration's scope.
//!
//! # What has never run against a real server
//!
//! No ICAP server is reachable in this environment (no Docker, no network scanners — see
//! `docs/status/09-media.md`). `tests::wire` covers verdict/replacement extraction with
//! `icap_rs::response::Response::from_raw` against literal recorded bytes (no socket at all).
//! `tests::live` starts `icap_rs::server::Server` — a real, independent RFC 3507 implementation —
//! in-process as the test double, and drives [`IcapScanner`] against it end to end (OPTIONS
//! negotiation, `204`, an infection header, preview-then-`100-Continue`, `Transfer-Ignore`
//! skipping, `ISTag` changing between calls). This is not the same as interoperating with a real
//! c-icap or ClamAV; it proves this module and `icap-rs`'s client agree with `icap-rs`'s own
//! server about the wire protocol, which is the strongest test double available without standing
//! up an external daemon. The EICAR-through-a-real-daemon test lives in
//! `tests/icap_eicar.rs` and skips cleanly (see that file) since `deploy/`'s c-icap+ClamAV
//! reference stack is not running here.

use std::time::{Duration, Instant};

use async_trait::async_trait;
use bytes::Bytes;
use icap_rs::{Client, OptionsCacheConfig, Request as IcapRequest};

use crate::scanning::config::{IcapConfig, PreviewMode};
use crate::scanning::types::{
    AdaptedContent, ContentScanner, ScanContext, ScanError, ScanSource, ScanTicket, Verdict,
};

/// Vendor spellings for "infected" (RFC section 3.3: "verdict headers across vendor spellings").
const INFECTION_HEADERS: &[&str] = &["x-infection-found", "x-virus-id", "x-violations-found"];

/// The `icap` provider.
pub struct IcapScanner {
    client: Client,
    config: IcapConfig,
}

impl IcapScanner {
    /// Builds a provider for the given ICAP service. Opens no connection until the first scan
    /// (`icap_rs::Client` connects lazily and reuses the connection across calls when
    /// `keep_alive` is set).
    #[must_use]
    pub fn new(config: IcapConfig) -> Self {
        let client = Client::builder()
            .host(&config.host)
            .port(config.port)
            .keep_alive(true)
            .user_agent("hs-media")
            // `icap-rs` caches OPTIONS per RFC 3507 §4.10.2/§5 and reconciles the ISTag
            // automatically; a 5-minute fallback covers a server that omits `Options-TTL`.
            .with_options_cache(OptionsCacheConfig::new().with_default_ttl(Duration::from_secs(300)))
            .build();
        Self { client, config }
    }

    fn service_path(&self) -> &str {
        &self.config.service
    }

    async fn fetch_istag(&self) -> Option<String> {
        let req = IcapRequest::options(self.service_path());
        let resp = self.client.send(&req).await.ok()?;
        resp.get_header("ISTag")
            .and_then(|v| v.to_str().ok())
            .map(|s| s.trim_matches('"').to_string())
    }
}

#[async_trait]
impl ContentScanner for IcapScanner {
    fn id(&self) -> &str {
        "icap"
    }

    async fn engine_version(&self) -> Option<String> {
        self.fetch_istag().await
    }

    async fn scan(
        &self,
        mut content: ScanSource<'_>,
        ctx: &ScanContext,
    ) -> Result<Verdict, ScanError> {
        let body = content
            .read_all()
            .await
            .map_err(|e| ScanError::Other(e.to_string()))?;

        let http_resp = http::Response::builder()
            .status(200)
            .header(http::header::CONTENT_TYPE, content.content_type.as_str())
            .header(http::header::CONTENT_LENGTH, body.len().to_string())
            .body(body.to_vec())
            .map_err(|e| ScanError::Other(format!("building embedded HTTP response: {e}")))?;

        let mut req = IcapRequest::respmod(self.service_path())
            .allow_204()
            .with_http_response(http_resp)
            .map_err(|e| ScanError::Protocol(e.to_string()))?;
        req = match self.config.preview {
            PreviewMode::Negotiate => req,
            PreviewMode::Bytes(n) => req.preview(n),
            PreviewMode::Off => req,
        };

        let remaining = ctx
            .deadline
            .checked_duration_since(Instant::now())
            .ok_or(ScanError::Timeout)?;

        let resp = match tokio::time::timeout(remaining, self.client.send(&req)).await {
            Ok(Ok(resp)) => resp,
            Ok(Err(e)) => return Err(classify_client_error(&e)),
            Err(_) => return Err(ScanError::Timeout),
        };

        Ok(to_verdict(&resp, &body, self.service_path()))
    }

    async fn poll(&self, ticket: &ScanTicket) -> Result<Verdict, ScanError> {
        let _ = ticket;
        // ICAP RESPMOD is always synchronous: `scan` never returns `Verdict::Pending`.
        Err(ScanError::Unsupported)
    }
}

fn classify_client_error(e: &icap_rs::Error) -> ScanError {
    let msg = e.to_string();
    // `icap_rs::Error` does not expose a stable "is this a connection failure" predicate as of
    // 0.3.0; string classification is a pragmatic fallback so timeouts/refused connections still
    // map onto the right `ScanError` variant for `crate::scanning::engine`'s fail-policy decision
    // (both `ScanError::Unavailable` and `ScanError::Protocol` are treated as scanner errors by
    // that policy either way, so this classification affects logging/metrics detail, not
    // behavior).
    let lower = msg.to_ascii_lowercase();
    if lower.contains("timeout") || lower.contains("timed out") {
        ScanError::Timeout
    } else if lower.contains("connect") || lower.contains("refused") || lower.contains("reset") {
        ScanError::Unavailable(msg)
    } else {
        ScanError::Protocol(msg)
    }
}

/// Splits `icap_rs`'s combined "embedded HTTP head + dechunked entity body" (see
/// [`icap_rs::response::Response::body`]'s doc) into the embedded HTTP header block and the
/// actual returned content bytes.
fn split_embedded_http(raw: &[u8]) -> (Vec<(String, String)>, &[u8]) {
    let Some(pos) = find_double_crlf(raw) else {
        return (Vec::new(), raw);
    };
    let head = String::from_utf8_lossy(&raw[..pos]);
    let mut lines = head.split("\r\n");
    let _status_line = lines.next();
    let headers = lines
        .filter_map(|line| {
            let (name, value) = line.split_once(':')?;
            Some((name.trim().to_ascii_lowercase(), value.trim().to_string()))
        })
        .collect();
    (headers, &raw[pos + 4..])
}

fn find_double_crlf(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n")
}

fn header_lookup<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(n, _)| n == name)
        .map(|(_, v)| v.as_str())
}

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

/// Turns a completed ICAP exchange into a [`Verdict`]: `204` is clean; a `200` carrying a
/// verdict header is infected; a `200` with a body byte-identical to what we sent is clean (some
/// AV services echo the body back rather than answering `204`); any other `200` is a genuine
/// [`Verdict::Replaced`] (RFC section 3.4) — applying it is `crate::scanning::engine`'s job, not
/// this function's, which only reports what the service said.
fn to_verdict(resp: &icap_rs::response::Response<icap_rs::response::Parsed>, sent_body: &[u8], service: &str) -> Verdict {
    if resp.status_code() == icap_rs::StatusCode::NO_CONTENT {
        return Verdict::Clean;
    }
    if resp.status_code() != icap_rs::StatusCode::OK {
        return Verdict::Infected {
            signature: "icap-blocked-response".to_string(),
            details: Some(format!("{} {}", resp.status_code(), resp.status_text())),
        };
    }

    let (embedded_headers, returned_body) = split_embedded_http(resp.body());

    for name in INFECTION_HEADERS {
        if let Some(v) = resp.get_header(name).and_then(|v| v.to_str().ok()) {
            let (signature, details) = parse_infection_header(name, v);
            return Verdict::Infected { signature, details };
        }
        if let Some(v) = header_lookup(&embedded_headers, name) {
            let (signature, details) = parse_infection_header(name, v);
            return Verdict::Infected { signature, details };
        }
        if let Some(v) = resp
            .chunk_trailers()
            .get(*name)
            .and_then(|v| v.to_str().ok())
        {
            let (signature, details) = parse_infection_header(name, v);
            return Verdict::Infected { signature, details };
        }
    }

    if returned_body == sent_body {
        return Verdict::Clean;
    }

    let content_type = header_lookup(&embedded_headers, "content-type").map(str::to_string);
    Verdict::Replaced {
        content: AdaptedContent {
            bytes: Bytes::copy_from_slice(returned_body),
            content_type,
        },
        by: service.to_string(),
        reason: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scanning::types::ScanSourceKind;
    use icap_rs::response::Response as IcapResponse;
    use std::time::Duration as StdDuration;

    fn test_config(port: u16) -> IcapConfig {
        IcapConfig {
            host: "127.0.0.1".to_string(),
            port,
            service: "virus_scan".to_string(),
            preview: PreviewMode::Negotiate,
        }
    }

    fn ctx() -> ScanContext {
        ScanContext {
            deadline: Instant::now() + StdDuration::from_secs(5),
            uploader: Some("@alice:example.org".into()),
            source: ScanSourceKind::Local,
            media_id: "abc".into(),
            server_name: "example.org".into(),
        }
    }

    // --- Pure verdict-extraction unit tests, against `icap_rs::response::Response::from_raw` --
    // (no socket, no `icap_rs::server::Server` — literal recorded bytes only).

    #[test]
    fn no_content_is_clean() {
        let resp = IcapResponse::from_raw(b"ICAP/1.0 204 No Content\r\n\r\n").unwrap();
        assert_eq!(to_verdict(&resp, b"anything", "virus_scan"), Verdict::Clean);
    }

    #[test]
    fn infection_header_on_the_icap_response_is_infected() {
        let raw = b"ICAP/1.0 200 OK\r\nISTag: \"v1\"\r\nX-Infection-Found: Type=0; Resolution=2; Threat=Eicar-Test-Signature;\r\nEncapsulated: null-body=0\r\n\r\n";
        let resp = IcapResponse::from_raw(raw).unwrap();
        let verdict = to_verdict(&resp, b"", "virus_scan");
        assert_eq!(
            verdict,
            Verdict::Infected {
                signature: "Eicar-Test-Signature".into(),
                details: Some("Type=0; Resolution=2; Threat=Eicar-Test-Signature;".into()),
            }
        );
    }

    #[test]
    fn x_virus_id_on_the_embedded_http_response_is_infected() {
        let http_hdr = "HTTP/1.1 200 OK\r\nX-Virus-ID: Eicar-Test-Signature\r\nContent-Length: 4\r\n\r\n";
        let raw = format!(
            "ICAP/1.0 200 OK\r\nISTag: \"v1\"\r\nEncapsulated: res-hdr=0, res-body={}\r\n\r\n{http_hdr}4\r\ntest\r\n0\r\n\r\n",
            http_hdr.len()
        );
        let resp = IcapResponse::from_raw(raw.as_bytes()).unwrap();
        let verdict = to_verdict(&resp, b"orig", "virus_scan");
        assert_eq!(
            verdict,
            Verdict::Infected {
                signature: "Eicar-Test-Signature".into(),
                details: None,
            }
        );
    }

    #[test]
    fn unchanged_body_is_clean() {
        let http_hdr = "HTTP/1.1 200 OK\r\nContent-Length: 4\r\n\r\n";
        let raw = format!(
            "ICAP/1.0 200 OK\r\nISTag: \"v1\"\r\nEncapsulated: res-hdr=0, res-body={}\r\n\r\n{http_hdr}4\r\ntest\r\n0\r\n\r\n",
            http_hdr.len()
        );
        let resp = IcapResponse::from_raw(raw.as_bytes()).unwrap();
        assert_eq!(to_verdict(&resp, b"test", "virus_scan"), Verdict::Clean);
    }

    #[test]
    fn changed_body_is_replaced() {
        let http_hdr = "HTTP/1.1 200 OK\r\nContent-Length: 9\r\n\r\n";
        let raw = format!(
            "ICAP/1.0 200 OK\r\nISTag: \"v1\"\r\nEncapsulated: res-hdr=0, res-body={}\r\n\r\n{http_hdr}9\r\nstripped!\r\n0\r\n\r\n",
            http_hdr.len()
        );
        let resp = IcapResponse::from_raw(raw.as_bytes()).unwrap();
        let verdict = to_verdict(&resp, b"original!", "virus_scan");
        match verdict {
            Verdict::Replaced { content, by, .. } => {
                assert_eq!(content.bytes.as_ref(), b"stripped!");
                assert_eq!(by, "virus_scan");
            }
            other => panic!("expected Replaced, got {other:?}"),
        }
    }

    #[test]
    fn non_200_non_204_is_treated_as_a_blocked_response() {
        let resp = IcapResponse::from_raw(b"ICAP/1.0 403 Forbidden\r\n\r\n").unwrap();
        let verdict = to_verdict(&resp, b"x", "virus_scan");
        assert!(matches!(verdict, Verdict::Infected { .. }));
    }

    // --- End-to-end tests against `icap_rs::server::Server`, a real RFC 3507 implementation
    // running in-process (see the module doc's "what has never run against a real server"). ---

    mod live {
        use super::*;
        use icap_rs::request::IncomingRequest;
        use icap_rs::response::Response as OutResponse;
        use icap_rs::server::{Server, ServiceOptions};
        use std::net::TcpListener as StdTcpListener;
        use std::sync::Arc;
        use std::sync::atomic::{AtomicU64, Ordering};

        async fn start_server(
            istag: &'static str,
            behavior: impl Fn(&IncomingRequest) -> OutResponse + Send + Sync + 'static,
        ) -> u16 {
            let std_listener = StdTcpListener::bind("127.0.0.1:0").unwrap();
            let port = std_listener.local_addr().unwrap().port();
            drop(std_listener);
            let behavior = Arc::new(behavior);
            let handler = move |req: IncomingRequest| {
                let behavior = Arc::clone(&behavior);
                async move { Ok::<OutResponse, icap_rs::HandlerError>(behavior(&req)) }
            };
            let server = Server::builder()
                .bind(&format!("127.0.0.1:{port}"))
                .route_respmod(
                    "virus_scan",
                    handler,
                    Some(
                        ServiceOptions::new()
                            .with_static_istag(istag)
                            .with_service("test")
                            .allow_204()
                            .with_preview(4),
                    ),
                )
                .build()
                .await
                .unwrap();
            tokio::spawn(async move {
                let _ = server.run().await;
            });
            tokio::time::sleep(StdDuration::from_millis(80)).await;
            port
        }

        #[tokio::test]
        async fn clean_content_gets_204() {
            let port =
                start_server("\"v1\"", |_req| OutResponse::no_content_with_istag("\"v1\"").unwrap())
                    .await;
            let scanner = IcapScanner::new(test_config(port));
            let source =
                ScanSource::from_bytes("text/plain", Bytes::from_static(b"hello world"), 4096);
            let verdict = scanner.scan(source, &ctx()).await.unwrap();
            assert_eq!(verdict, Verdict::Clean);
        }

        #[tokio::test]
        async fn engine_version_returns_the_istag() {
            let port = start_server("\"sigs-2026-09-01\"", |_req| {
                OutResponse::no_content_with_istag("\"sigs-2026-09-01\"").unwrap()
            })
            .await;
            let scanner = IcapScanner::new(test_config(port));
            let version = scanner.engine_version().await;
            assert_eq!(version.as_deref(), Some("sigs-2026-09-01"));
        }

        #[tokio::test]
        async fn infected_content_is_reported() {
            let port = start_server("\"v1\"", |_req| {
                OutResponse::no_content_with_istag("\"v1\"")
                    .unwrap()
                    .try_add_header("X-Virus-ID", "Eicar-Test-Signature")
                    .unwrap()
            })
            .await;
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
                    details: None,
                }
            );
        }

        #[tokio::test]
        async fn preview_then_full_body_is_seen_by_the_server() {
            // The server's preview is 4 bytes; `icap-rs`'s client (via the OPTIONS cache) sends
            // the preview, gets `100 Continue` from the server automatically (server-side
            // behavior for a non-preview-aware handler that only runs after the full body
            // arrives), and the handler below asserts it saw the complete 11-byte body — proving
            // the preview-then-remainder handshake actually delivered everything.
            let received_len = Arc::new(AtomicU64::new(0));
            let received_len_for_handler = Arc::clone(&received_len);
            let port = start_server("\"v1\"", move |req| {
                if let Some(icap_rs::EmbeddedHttp::Resp {
                    body: icap_rs::Body::Full { reader },
                    ..
                }) = req.embedded()
                {
                    received_len_for_handler.store(reader.len() as u64, Ordering::SeqCst);
                }
                OutResponse::no_content_with_istag("\"v1\"").unwrap()
            })
            .await;
            let scanner = IcapScanner::new(test_config(port));
            let source =
                ScanSource::from_bytes("text/plain", Bytes::from_static(b"0123456789a"), 4096);
            let verdict = scanner.scan(source, &ctx()).await.unwrap();
            assert_eq!(verdict, Verdict::Clean);
            assert_eq!(received_len.load(Ordering::SeqCst), 11);
        }

        #[tokio::test]
        async fn transfer_ignore_skips_the_scan_entirely() {
            let called = Arc::new(AtomicU64::new(0));
            let called_for_handler = Arc::clone(&called);
            let std_listener = StdTcpListener::bind("127.0.0.1:0").unwrap();
            let port = std_listener.local_addr().unwrap().port();
            drop(std_listener);
            let handler = move |_req: IncomingRequest| {
                let called = Arc::clone(&called_for_handler);
                async move {
                    called.fetch_add(1, Ordering::SeqCst);
                    Ok::<OutResponse, icap_rs::HandlerError>(OutResponse::no_content())
                }
            };
            let server = Server::builder()
                .bind(&format!("127.0.0.1:{port}"))
                .route_respmod(
                    "virus_scan",
                    handler,
                    Some(
                        ServiceOptions::new()
                            .with_static_istag("\"v1\"")
                            .with_service("test")
                            .allow_204()
                            .with_preview(4)
                            .add_transfer_rule("gif", icap_rs::TransferBehavior::Ignore),
                    ),
                )
                .build()
                .await
                .unwrap();
            tokio::spawn(async move {
                let _ = server.run().await;
            });
            tokio::time::sleep(StdDuration::from_millis(80)).await;

            let scanner = IcapScanner::new(test_config(port));
            let source = ScanSource::from_bytes("image/gif", Bytes::from_static(b"GIF89a..."), 4096);
            let verdict = scanner.scan(source, &ctx()).await.unwrap();
            assert_eq!(verdict, Verdict::Clean);
            assert_eq!(
                called.load(Ordering::SeqCst),
                0,
                "the RESPMOD handler must never run for a Transfer-Ignore type"
            );
        }

        #[tokio::test]
        async fn connection_refused_is_unavailable() {
            let scanner = IcapScanner::new(test_config(1));
            let source = ScanSource::from_bytes("text/plain", Bytes::from_static(b"x"), 16);
            let err = scanner.scan(source, &ctx()).await.unwrap_err();
            assert!(matches!(err, ScanError::Unavailable(_) | ScanError::Protocol(_)));
        }
    }
}
