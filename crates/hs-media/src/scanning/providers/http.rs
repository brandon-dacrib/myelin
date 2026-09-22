//! The `http` provider: a JSON submit-and-poll contract over HTTP, for cloud scanners with no
//! ICAP fronting — RFC section 3.1's example is CrowdStrike Falcon, which is submit-then-poll and
//! is why [`Verdict::Pending`] exists in the trait at all rather than being retrofitted later.
//!
//! # Wire contract
//!
//! `POST <submit_url>` with the raw content bytes as the body (`Content-Type` set to the
//! uploader's declared type), plus these request headers:
//!
//! - `X-Scan-Sha256`: lower-case hex SHA-256 of the body.
//! - `X-Scan-Media-Id`, `X-Scan-Server-Name`: the media identifier being scanned.
//! - `Authorization: Bearer <token>`, if [`HttpConfig::auth_token`] is set.
//!
//! The response body is JSON, `{"status": "...", ...}`, one of:
//!
//! ```json
//! {"status": "clean"}
//! {"status": "infected", "signature": "...", "details": "..."}
//! {"status": "unscannable", "reason": "encrypted"}
//! {"status": "pending", "ticket": "abc123", "retry_after_ms": 2000}
//! {"status": "replaced", "content_base64": "...", "content_type": "image/png", "by": "cloud-scanner", "reason": "exif-stripped"}
//! ```
//!
//! Polling (`GET <poll_url or submit_url>?ticket=<ticket>`) returns the same shape.
//!
//! This crate defines the contract (there is no existing standard to adopt here — CrowdStrike
//! Falcon's own API is proprietary and not a wire-compatible target this session could implement
//! against without credentials); an operator fronts a real cloud API with a small adapter service
//! speaking this JSON shape, the same pattern `hs-modules`' `HttpCallbackClient` already
//! establishes for module hooks.

use std::time::Instant;

use async_trait::async_trait;
use base64::Engine as _;
use bytes::Bytes;
use serde::Deserialize;

use crate::scanning::config::HttpConfig;
use crate::scanning::types::{
    AdaptedContent, ContentScanner, ScanContext, ScanError, ScanSource, ScanTicket,
    UnscannableReason, Verdict,
};

#[derive(Debug, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
enum WireVerdict {
    Clean,
    Infected {
        signature: String,
        #[serde(default)]
        details: Option<String>,
    },
    Unscannable {
        reason: WireUnscannableReason,
    },
    Pending {
        ticket: String,
        retry_after_ms: u64,
    },
    Replaced {
        content_base64: String,
        #[serde(default)]
        content_type: Option<String>,
        by: String,
        #[serde(default)]
        reason: Option<String>,
    },
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
enum WireUnscannableReason {
    Encrypted,
    TooLarge,
    TooDeep,
    UnsupportedFormat,
    Other(String),
}

impl From<WireVerdict> for Result<Verdict, ScanError> {
    fn from(w: WireVerdict) -> Self {
        Ok(match w {
            WireVerdict::Clean => Verdict::Clean,
            WireVerdict::Infected { signature, details } => {
                Verdict::Infected { signature, details }
            }
            WireVerdict::Unscannable { reason } => Verdict::Unscannable {
                reason: match reason {
                    WireUnscannableReason::Encrypted => UnscannableReason::Encrypted,
                    WireUnscannableReason::TooLarge => UnscannableReason::TooLarge,
                    WireUnscannableReason::TooDeep => UnscannableReason::TooDeep,
                    WireUnscannableReason::UnsupportedFormat => {
                        UnscannableReason::UnsupportedFormat
                    }
                    WireUnscannableReason::Other(s) => UnscannableReason::Other(s),
                },
            },
            WireVerdict::Pending {
                ticket,
                retry_after_ms,
            } => Verdict::Pending {
                ticket: ScanTicket(ticket),
                retry_after: std::time::Duration::from_millis(retry_after_ms),
            },
            WireVerdict::Replaced {
                content_base64,
                content_type,
                by,
                reason,
            } => {
                let bytes = match base64::engine::general_purpose::STANDARD.decode(&content_base64)
                {
                    Ok(b) => b,
                    Err(e) => {
                        return Err(ScanError::Protocol(format!(
                            "invalid base64 in replaced content: {e}"
                        )));
                    }
                };
                Verdict::Replaced {
                    content: AdaptedContent {
                        bytes: Bytes::from(bytes),
                        content_type,
                    },
                    by,
                    reason,
                }
            }
        })
    }
}

/// The `http` provider.
pub struct HttpScanner {
    client: reqwest::Client,
    config: HttpConfig,
}

impl HttpScanner {
    /// Builds a provider for the given endpoint.
    #[must_use]
    pub fn new(config: HttpConfig) -> Self {
        Self {
            client: hs_http::client::builder().build().unwrap_or_default(),
            config,
        }
    }

    fn poll_url(&self) -> &str {
        self.config
            .poll_url
            .as_deref()
            .unwrap_or(&self.config.submit_url)
    }

    fn apply_auth(&self, builder: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match &self.config.auth_token {
            Some(token) => builder.bearer_auth(token),
            None => builder,
        }
    }

    async fn parse_response(resp: reqwest::Response) -> Result<Verdict, ScanError> {
        if !resp.status().is_success() {
            return Err(ScanError::Protocol(format!(
                "scanner returned HTTP {}",
                resp.status()
            )));
        }
        let wire: WireVerdict = resp
            .json()
            .await
            .map_err(|e| ScanError::Protocol(format!("invalid scan response: {e}")))?;
        wire.into()
    }
}

#[async_trait]
impl ContentScanner for HttpScanner {
    fn id(&self) -> &str {
        "http"
    }

    async fn engine_version(&self) -> Option<String> {
        // No standard cloud-scanner mechanism to cheaply learn an engine/signature version
        // without a real submission (unlike ICAP's `ISTag`, negotiated as part of `OPTIONS`).
        // Verdicts from this provider are therefore cached only under `cache.unversioned_ttl`
        // (see `crate::scanning::cache`) until a specific deployment's wire contract adds one.
        None
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

        let remaining = ctx
            .deadline
            .checked_duration_since(Instant::now())
            .ok_or(ScanError::Timeout)?;

        let mut builder = self
            .client
            .post(&self.config.submit_url)
            .header(reqwest::header::CONTENT_TYPE, content.content_type.clone())
            .header("X-Scan-Sha256", content.sha256_hex.clone())
            .header("X-Scan-Media-Id", ctx.media_id.clone())
            .header("X-Scan-Server-Name", ctx.server_name.clone())
            .timeout(remaining)
            .body(body.to_vec());
        builder = self.apply_auth(builder);

        let resp = builder.send().await.map_err(classify_reqwest_error)?;
        Self::parse_response(resp).await
    }

    async fn poll(&self, ticket: &ScanTicket) -> Result<Verdict, ScanError> {
        let mut builder = self
            .client
            .get(self.poll_url())
            .query(&[("ticket", ticket.0.as_str())]);
        builder = self.apply_auth(builder);
        let resp = builder.send().await.map_err(classify_reqwest_error)?;
        Self::parse_response(resp).await
    }
}

fn classify_reqwest_error(e: reqwest::Error) -> ScanError {
    if e.is_timeout() {
        ScanError::Timeout
    } else if e.is_connect() {
        ScanError::Unavailable(e.to_string())
    } else {
        ScanError::Protocol(e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scanning::types::ScanSourceKind;
    use axum::routing::{get, post};
    use axum::{Json, Router};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::Duration as StdDuration;
    use tokio::net::TcpListener;

    fn ctx() -> ScanContext {
        ScanContext {
            deadline: Instant::now() + StdDuration::from_secs(5),
            uploader: Some("@alice:example.org".into()),
            source: ScanSourceKind::Local,
            media_id: "abc".into(),
            server_name: "example.org".into(),
        }
    }

    async fn start_mock(router: Router) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        format!("http://{addr}")
    }

    #[tokio::test]
    async fn clean_verdict_round_trips() {
        let router = Router::new().route(
            "/submit",
            post(|| async { Json(serde_json::json!({"status": "clean"})) }),
        );
        let base = start_mock(router).await;
        let scanner = HttpScanner::new(HttpConfig {
            submit_url: format!("{base}/submit"),
            poll_url: None,
            auth_token: None,
        });
        let source = ScanSource::from_bytes("text/plain", Bytes::from_static(b"hello"), 16);
        let verdict = scanner.scan(source, &ctx()).await.unwrap();
        assert_eq!(verdict, Verdict::Clean);
    }

    #[tokio::test]
    async fn infected_verdict_round_trips() {
        let router = Router::new().route(
            "/submit",
            post(|| async {
                Json(serde_json::json!({
                    "status": "infected",
                    "signature": "Eicar-Test-Signature",
                    "details": "matched EICAR"
                }))
            }),
        );
        let base = start_mock(router).await;
        let scanner = HttpScanner::new(HttpConfig {
            submit_url: format!("{base}/submit"),
            poll_url: None,
            auth_token: None,
        });
        let source =
            ScanSource::from_bytes("application/octet-stream", Bytes::from_static(b"X5O!"), 16);
        let verdict = scanner.scan(source, &ctx()).await.unwrap();
        assert_eq!(
            verdict,
            Verdict::Infected {
                signature: "Eicar-Test-Signature".into(),
                details: Some("matched EICAR".into()),
            }
        );
    }

    #[tokio::test]
    async fn pending_then_poll_clean() {
        let calls = Arc::new(AtomicU64::new(0));
        let calls_submit = Arc::clone(&calls);
        let router = Router::new()
            .route(
                "/submit",
                post(move || {
                    let calls = Arc::clone(&calls_submit);
                    async move {
                        calls.fetch_add(1, Ordering::SeqCst);
                        Json(serde_json::json!({
                            "status": "pending",
                            "ticket": "job-1",
                            "retry_after_ms": 1
                        }))
                    }
                }),
            )
            .route(
                "/poll",
                get(
                    |axum::extract::Query(q): axum::extract::Query<
                        std::collections::HashMap<String, String>,
                    >| async move {
                        assert_eq!(q.get("ticket").map(String::as_str), Some("job-1"));
                        Json(serde_json::json!({"status": "clean"}))
                    },
                ),
            );
        let base = start_mock(router).await;
        let scanner = HttpScanner::new(HttpConfig {
            submit_url: format!("{base}/submit"),
            poll_url: Some(format!("{base}/poll")),
            auth_token: None,
        });
        let source = ScanSource::from_bytes("text/plain", Bytes::from_static(b"hello"), 16);
        let verdict = scanner.scan(source, &ctx()).await.unwrap();
        let ticket = match verdict {
            Verdict::Pending { ticket, .. } => ticket,
            other => panic!("expected Pending, got {other:?}"),
        };
        let resolved = scanner.poll(&ticket).await.unwrap();
        assert_eq!(resolved, Verdict::Clean);
    }

    #[tokio::test]
    async fn pending_then_poll_infected() {
        let router = Router::new()
            .route(
                "/submit",
                post(|| async {
                    Json(serde_json::json!({"status": "pending", "ticket": "job-2", "retry_after_ms": 1}))
                }),
            )
            .route(
                "/poll",
                get(|| async {
                    Json(serde_json::json!({
                        "status": "infected",
                        "signature": "Win32.Test",
                        "details": null
                    }))
                }),
            );
        let base = start_mock(router).await;
        let scanner = HttpScanner::new(HttpConfig {
            submit_url: format!("{base}/submit"),
            poll_url: Some(format!("{base}/poll")),
            auth_token: None,
        });
        let source = ScanSource::from_bytes("text/plain", Bytes::from_static(b"hello"), 16);
        let verdict = scanner.scan(source, &ctx()).await.unwrap();
        let ticket = match verdict {
            Verdict::Pending { ticket, .. } => ticket,
            other => panic!("expected Pending, got {other:?}"),
        };
        let resolved = scanner.poll(&ticket).await.unwrap();
        assert_eq!(
            resolved,
            Verdict::Infected {
                signature: "Win32.Test".into(),
                details: None,
            }
        );
    }

    #[tokio::test]
    async fn unscannable_encrypted_round_trips() {
        let router = Router::new().route(
            "/submit",
            post(|| async {
                Json(serde_json::json!({"status": "unscannable", "reason": "encrypted"}))
            }),
        );
        let base = start_mock(router).await;
        let scanner = HttpScanner::new(HttpConfig {
            submit_url: format!("{base}/submit"),
            poll_url: None,
            auth_token: None,
        });
        let source = ScanSource::from_bytes(
            "application/octet-stream",
            Bytes::from_static(b"ciphertext"),
            16,
        );
        let verdict = scanner.scan(source, &ctx()).await.unwrap();
        assert_eq!(
            verdict,
            Verdict::Unscannable {
                reason: UnscannableReason::Encrypted
            }
        );
    }

    #[tokio::test]
    async fn replaced_verdict_decodes_base64_content() {
        let content_b64 = base64::engine::general_purpose::STANDARD.encode(b"stripped-bytes");
        let router = Router::new().route(
            "/submit",
            post({
                let content_b64 = content_b64.clone();
                move || {
                    let content_b64 = content_b64.clone();
                    async move {
                        Json(serde_json::json!({
                            "status": "replaced",
                            "content_base64": content_b64,
                            "content_type": "image/png",
                            "by": "cloud-scanner",
                            "reason": "exif-stripped"
                        }))
                    }
                }
            }),
        );
        let base = start_mock(router).await;
        let scanner = HttpScanner::new(HttpConfig {
            submit_url: format!("{base}/submit"),
            poll_url: None,
            auth_token: None,
        });
        let source = ScanSource::from_bytes("image/png", Bytes::from_static(b"original-bytes"), 16);
        let verdict = scanner.scan(source, &ctx()).await.unwrap();
        match verdict {
            Verdict::Replaced {
                content,
                by,
                reason,
            } => {
                assert_eq!(content.bytes.as_ref(), b"stripped-bytes");
                assert_eq!(content.content_type.as_deref(), Some("image/png"));
                assert_eq!(by, "cloud-scanner");
                assert_eq!(reason.as_deref(), Some("exif-stripped"));
            }
            other => panic!("expected Replaced, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn connection_refused_is_unavailable() {
        let scanner = HttpScanner::new(HttpConfig {
            submit_url: "http://127.0.0.1:1/submit".to_string(),
            poll_url: None,
            auth_token: None,
        });
        let source = ScanSource::from_bytes("text/plain", Bytes::from_static(b"x"), 16);
        let err = scanner.scan(source, &ctx()).await.unwrap_err();
        assert!(matches!(err, ScanError::Unavailable(_)));
    }

    #[tokio::test]
    async fn auth_token_is_sent_as_bearer() {
        let router = Router::new().route(
            "/submit",
            post(|headers: axum::http::HeaderMap| async move {
                assert_eq!(
                    headers.get(axum::http::header::AUTHORIZATION).unwrap(),
                    "Bearer secret-token"
                );
                Json(serde_json::json!({"status": "clean"}))
            }),
        );
        let base = start_mock(router).await;
        let scanner = HttpScanner::new(HttpConfig {
            submit_url: format!("{base}/submit"),
            poll_url: None,
            auth_token: Some("secret-token".to_string()),
        });
        let source = ScanSource::from_bytes("text/plain", Bytes::from_static(b"hello"), 16);
        let verdict = scanner.scan(source, &ctx()).await.unwrap();
        assert_eq!(verdict, Verdict::Clean);
    }
}
