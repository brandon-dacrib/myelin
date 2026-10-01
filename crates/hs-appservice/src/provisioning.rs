//! Asking a bridge who has signed in to it: the network half of the admin API's
//! `GET /appservices/{id}/logins` (`hs_admin::bridge_logins` is the half that needs none).
//!
//! A mautrix `bridgev2` bridge answers `GET /_matrix/provision/v3/whoami?user_id=...` on its
//! appservice listener (the registration's `url`), with the `provisioning.shared_secret` from
//! its config as a bearer token, by listing the user's logins and their state. [`BridgeLogins`]
//! asks it, keeps each answer for [`CACHE_TTL`] per (bridge, user) so that a page refreshed in a
//! loop does not become load on the bridge, counts every answer in
//! `hs_admin_bridge_login_queries_total{type,outcome}`, and logs a bridge it could not ask. A
//! failure is an answer with `error` set, not an error of the request: the bridge being
//! unreachable is what is being reported, as with a ping. Failures are not cached, so the next
//! look asks again.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use hs_admin::bridge_logins::{self, LoginsPlan, WhoamiRequest};
pub use hs_admin::bridge_logins::{CACHE_TTL, WHOAMI_PATH};
use hs_admin::model::{AdminBridgeLogins, AdminBridgeLoginsError};
use hs_admin::sources::SourceError;
use serde_json::Value;

use crate::metrics::AppserviceMetrics;

/// How long a bridge has to answer, connection included.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// More cached answers than this and the expired ones are swept on the next insert.
const CACHE_SWEEP_AT: usize = 1024;

/// See the module docs.
pub struct BridgeLogins {
    client: reqwest::Client,
    ttl: Duration,
    cache: Mutex<HashMap<(String, String), (Instant, AdminBridgeLogins)>>,
    metrics: Option<AppserviceMetrics>,
}

impl Default for BridgeLogins {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for BridgeLogins {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BridgeLogins")
            .field("ttl", &self.ttl)
            .finish_non_exhaustive()
    }
}

impl BridgeLogins {
    /// Asks with a [`REQUEST_TIMEOUT`] and keeps answers for [`CACHE_TTL`].
    #[must_use]
    pub fn new() -> Self {
        Self::with_timeout(REQUEST_TIMEOUT)
    }

    /// As [`BridgeLogins::new`], with another request timeout.
    #[must_use]
    pub fn with_timeout(timeout: Duration) -> Self {
        let client = reqwest::Client::builder()
            .timeout(timeout)
            .connect_timeout(timeout)
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap_or_default();
        Self {
            client,
            ttl: CACHE_TTL,
            cache: Mutex::new(HashMap::new()),
            metrics: None,
        }
    }

    /// Keeps answers for `ttl` instead (zero: not at all).
    #[must_use]
    pub fn with_ttl(mut self, ttl: Duration) -> Self {
        self.ttl = ttl;
        self
    }

    /// Counts answers into `metrics`' `hs_admin_bridge_login_queries_total`.
    #[must_use]
    pub fn with_metrics(mut self, metrics: AppserviceMetrics) -> Self {
        self.metrics = Some(metrics);
        self
    }

    fn count(&self, bridge_type: Option<&str>, outcome: &str) {
        if let Some(metrics) = &self.metrics {
            metrics.record_login_query(bridge_type.unwrap_or("custom"), outcome);
        }
    }

    fn cached(&self, key: &(String, String)) -> Option<AdminBridgeLogins> {
        let cache = self
            .cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let (at, answer) = cache.get(key)?;
        (at.elapsed() < self.ttl).then(|| AdminBridgeLogins {
            cached: true,
            ..answer.clone()
        })
    }

    fn keep(&self, key: (String, String), answer: &AdminBridgeLogins) {
        if self.ttl.is_zero() {
            return;
        }
        let mut cache = self
            .cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if cache.len() >= CACHE_SWEEP_AT {
            let ttl = self.ttl;
            cache.retain(|_, (at, _)| at.elapsed() < ttl);
        }
        cache.insert(key, (Instant::now(), answer.clone()));
    }

    /// Who has signed in to the bridge `appservice_id` (whose registration, in its JSON form
    /// with unrecognised keys, is `registration`), asked about `user_id` or, absent, a per-user
    /// instance's owner. See [`hs_admin::sources::AppserviceDirectory::logins`].
    ///
    /// # Errors
    /// Only [`hs_admin::bridge_logins::plan`]'s: a `user_id` that is missing or not a Matrix
    /// ID. Everything about the bridge itself is in the answer.
    pub async fn query(
        &self,
        appservice_id: &str,
        registration: &Value,
        user_id: Option<&str>,
    ) -> Result<AdminBridgeLogins, SourceError> {
        let request = match bridge_logins::plan(appservice_id, registration, user_id)? {
            LoginsPlan::Unsupported(answer) => {
                self.count(answer.bridge_type.as_deref(), "unsupported");
                return Ok(answer);
            }
            LoginsPlan::Ask(request) => request,
        };
        let key = (appservice_id.to_owned(), request.user_id.clone());
        if let Some(answer) = self.cached(&key) {
            self.count(Some(&request.bridge_type), "cached");
            return Ok(answer);
        }
        match self.ask(&request).await {
            Ok(answer) => {
                self.count(Some(&request.bridge_type), "answered");
                self.keep(key, &answer);
                Ok(answer)
            }
            Err(error) => {
                tracing::warn!(
                    appservice = %request.appservice_id,
                    bridge_type = %request.bridge_type,
                    user_id = %request.user_id,
                    url = %request.url,
                    reason = %error.reason,
                    status = error.status,
                    detail = %error.detail,
                    "could not ask a bridge who has signed in"
                );
                self.count(Some(&request.bridge_type), &error.reason);
                Ok(bridge_logins::failed(&request, error))
            }
        }
    }

    async fn ask(
        &self,
        request: &WhoamiRequest,
    ) -> Result<AdminBridgeLogins, AdminBridgeLoginsError> {
        let response = self
            .client
            .get(&request.url)
            .query(&[("user_id", request.user_id.as_str())])
            .bearer_auth(&request.secret)
            .header(reqwest::header::ACCEPT, "application/json")
            .send()
            .await
            .map_err(|e| transport_error(&e))?;
        let status = response.status();
        let body = response.text().await.map_err(|e| transport_error(&e))?;
        if !status.is_success() {
            return Err(bridge_logins::refused(status.as_u16(), &body));
        }
        let parsed: Value = serde_json::from_str(&body).map_err(|e| AdminBridgeLoginsError {
            status: 502,
            reason: "invalid_answer".to_owned(),
            detail: format!("the bridge's {WHOAMI_PATH} answered something that is not JSON: {e}"),
        })?;
        let now_ms = i64::try_from(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis(),
        )
        .unwrap_or(i64::MAX);
        bridge_logins::answered(request, &parsed, now_ms)
    }
}

fn transport_error(error: &reqwest::Error) -> AdminBridgeLoginsError {
    if error.is_timeout() {
        AdminBridgeLoginsError {
            status: 504,
            reason: "timeout".to_owned(),
            detail: format!("the bridge did not answer in time: {error}"),
        }
    } else {
        AdminBridgeLoginsError {
            status: 502,
            reason: "unreachable".to_owned(),
            detail: format!("the server could not reach the bridge: {error}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hs_admin::bridge_types::{BRIDGE_INSTANCE_KEY, BRIDGE_TYPE_KEY, PROVISIONING_SECRET_KEY};
    use serde_json::json;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    const SECRET: &str = "provisioning-secret-for-the-test";

    /// A mautrix bridge's provisioning API, as far as whoami goes: alice is signed in with one
    /// WhatsApp account, everybody else with none. Counts what it was asked.
    #[derive(Clone, Default)]
    struct StandIn {
        asked: Arc<AtomicUsize>,
        delay: Duration,
    }

    async fn whoami(
        axum::extract::State(bridge): axum::extract::State<StandIn>,
        headers: axum::http::HeaderMap,
        axum::extract::Query(query): axum::extract::Query<HashMap<String, String>>,
    ) -> (axum::http::StatusCode, axum::Json<Value>) {
        bridge.asked.fetch_add(1, Ordering::SeqCst);
        tokio::time::sleep(bridge.delay).await;
        let auth = headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default();
        if auth != format!("Bearer {SECRET}") {
            return (
                axum::http::StatusCode::UNAUTHORIZED,
                axum::Json(json!({"errcode": "M_UNKNOWN_TOKEN", "error": "Invalid auth token"})),
            );
        }
        let user = query.get("user_id").cloned().unwrap_or_default();
        let logins = if user == "@alice:x.org" {
            json!([{
                "id": "15551234567",
                "name": "+1 555-123-4567",
                "state_event": "CONNECTED",
                "state_ts": 1_727_000_000,
                "profile": {"phone": "+15551234567"}
            }])
        } else {
            json!([])
        };
        (
            axum::http::StatusCode::OK,
            axum::Json(json!({
                "network": {"displayname": "WhatsApp"},
                "bridge_bot": "@whatsappbot:x.org",
                "logins": logins
            })),
        )
    }

    async fn stand_in(delay: Duration) -> (String, StandIn) {
        let bridge = StandIn {
            asked: Arc::default(),
            delay,
        };
        let app = axum::Router::new()
            .route(WHOAMI_PATH, axum::routing::get(whoami))
            .with_state(bridge.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        (url, bridge)
    }

    fn registration(url: &str, bridge_type: &str, secret: &str) -> Value {
        json!({
            "id": "whatsapp",
            "url": url,
            "as_token": "a",
            "hs_token": "h",
            "sender_localpart": "whatsappbot",
            BRIDGE_TYPE_KEY: bridge_type,
            PROVISIONING_SECRET_KEY: secret,
        })
    }

    fn metrics() -> (prometheus_client::registry::Registry, AppserviceMetrics) {
        let mut registry = prometheus_client::registry::Registry::default();
        let metrics = AppserviceMetrics::register(&mut registry);
        (registry, metrics)
    }

    fn text(registry: &prometheus_client::registry::Registry) -> String {
        let mut text = String::new();
        prometheus_client::encoding::text::encode(&mut text, registry).unwrap();
        text
    }

    #[tokio::test]
    async fn a_mautrix_bridge_says_who_has_signed_in_and_is_asked_once_in_thirty_seconds() {
        let (url, bridge) = stand_in(Duration::ZERO).await;
        let (registry, metrics) = metrics();
        let logins = BridgeLogins::new().with_metrics(metrics);
        let reg = registration(&url, "mautrix-whatsapp", SECRET);

        let answer = logins
            .query("whatsapp", &reg, Some("@alice:x.org"))
            .await
            .unwrap();
        assert!(answer.supported && !answer.cached, "{answer:?}");
        assert_eq!(answer.signed_in, Some(true));
        assert_eq!(answer.logins.len(), 1);
        assert_eq!(answer.logins[0].remote_id, "15551234567");
        assert_eq!(
            answer.logins[0].remote_name.as_deref(),
            Some("+1 555-123-4567")
        );
        assert_eq!(answer.logins[0].state, "connected");
        assert_eq!(
            answer.logins[0].since.as_deref(),
            Some("2024-09-22T10:13:20.000Z")
        );
        assert!(answer.checked_at.is_some());
        assert!(answer.error.is_none());

        // Again within the 30 seconds: the same answer, from the cache, and the bridge was
        // asked once.
        let again = logins
            .query("whatsapp", &reg, Some("@alice:x.org"))
            .await
            .unwrap();
        assert!(again.cached);
        assert_eq!(again.logins, answer.logins);
        assert_eq!(again.checked_at, answer.checked_at);
        assert_eq!(bridge.asked.load(Ordering::SeqCst), 1);

        // Somebody who has not signed in, asked separately (the cache is per user).
        let bob = logins
            .query("whatsapp", &reg, Some("@bob:x.org"))
            .await
            .unwrap();
        assert_eq!(bob.signed_in, Some(false));
        assert!(bob.logins.is_empty() && !bob.cached);
        assert_eq!(bridge.asked.load(Ordering::SeqCst), 2);

        let text = text(&registry);
        assert!(
            text.contains(
                "hs_admin_bridge_login_queries_total{type=\"mautrix-whatsapp\",outcome=\"answered\"} 2"
            ),
            "{text}"
        );
        assert!(
            text.contains(
                "hs_admin_bridge_login_queries_total{type=\"mautrix-whatsapp\",outcome=\"cached\"} 1"
            ),
            "{text}"
        );
    }

    #[tokio::test]
    async fn an_instance_is_asked_about_its_owner_without_being_told() {
        let (url, _bridge) = stand_in(Duration::ZERO).await;
        let mut reg = registration(&url, "mautrix-whatsapp", SECRET);
        reg[BRIDGE_INSTANCE_KEY] = json!("@alice:x.org");
        let answer = BridgeLogins::new()
            .query("whatsapp-alice", &reg, None)
            .await
            .unwrap();
        assert_eq!(answer.user_id.as_deref(), Some("@alice:x.org"));
        assert_eq!(answer.signed_in, Some(true));
    }

    #[tokio::test]
    async fn an_unreachable_or_refusing_or_slow_bridge_is_an_answer_with_an_error() {
        let (registry, metrics) = metrics();
        let logins = BridgeLogins::with_timeout(Duration::from_millis(500)).with_metrics(metrics);

        // Nothing listens there.
        let closed = {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            format!("http://{}", listener.local_addr().unwrap())
        };
        let answer = logins
            .query(
                "whatsapp",
                &registration(&closed, "mautrix-whatsapp", SECRET),
                Some("@alice:x.org"),
            )
            .await
            .expect("an unreachable bridge is reported, not an error of the request");
        assert!(answer.supported);
        assert_eq!(answer.signed_in, None);
        let error = answer.error.unwrap();
        assert_eq!((error.status, error.reason.as_str()), (502, "unreachable"));

        // The wrong secret: the bridge's own refusal, and nothing cached.
        let (url, bridge) = stand_in(Duration::ZERO).await;
        let wrong = registration(&url, "mautrix-whatsapp", "not-the-secret");
        for _ in 0..2 {
            let answer = logins
                .query("whatsapp", &wrong, Some("@alice:x.org"))
                .await
                .unwrap();
            let error = answer.error.unwrap();
            assert_eq!((error.status, error.reason.as_str()), (401, "refused"));
            assert!(error.detail.contains("M_UNKNOWN_TOKEN"), "{}", error.detail);
        }
        assert_eq!(
            bridge.asked.load(Ordering::SeqCst),
            2,
            "failures are not cached"
        );

        // Too slow.
        let (slow, _bridge) = stand_in(Duration::from_secs(5)).await;
        let answer = logins
            .query(
                "whatsapp",
                &registration(&slow, "mautrix-whatsapp", SECRET),
                Some("@alice:x.org"),
            )
            .await
            .unwrap();
        let error = answer.error.unwrap();
        assert_eq!((error.status, error.reason.as_str()), (504, "timeout"));

        let text = text(&registry);
        for outcome in ["unreachable", "refused", "timeout"] {
            assert!(
                text.contains(&format!("type=\"mautrix-whatsapp\",outcome=\"{outcome}\"")),
                "{outcome}: {text}"
            );
        }
    }

    #[tokio::test]
    async fn a_type_without_a_provisioning_api_is_not_asked() {
        let (url, bridge) = stand_in(Duration::ZERO).await;
        let (registry, metrics) = metrics();
        let logins = BridgeLogins::new().with_metrics(metrics);
        let answer = logins
            .query(
                "irc",
                &registration(&url, "heisenbridge", SECRET),
                Some("@alice:x.org"),
            )
            .await
            .unwrap();
        assert!(!answer.supported);
        assert_eq!(answer.provisioning_api, "none");
        assert!(answer.reason.is_some());
        assert_eq!(bridge.asked.load(Ordering::SeqCst), 0);
        let custom = logins
            .query("x", &json!({"id": "x", "url": url}), None)
            .await
            .unwrap();
        assert!(!custom.supported);
        let text = text(&registry);
        assert!(
            text.contains("type=\"heisenbridge\",outcome=\"unsupported\"} 1"),
            "{text}"
        );
        assert!(
            text.contains("type=\"custom\",outcome=\"unsupported\"} 1"),
            "{text}"
        );
    }
}
