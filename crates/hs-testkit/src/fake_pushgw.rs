//! A fake push gateway: the `POST /_matrix/push/v1/notify` endpoint
//! (`refs/matrix-spec/data/api/push-gateway/definitions_..., push-gateway/notify.yaml`) a
//! homeserver calls to deliver a push notification, recording every notification and letting a
//! test configure which `pushkey`s should come back "rejected" (the spec's mechanism for a client
//! app to tell the homeserver a push token is stale and the pusher should be deleted).

use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use axum::Json;
use axum::Router;
use axum::extract::Extension;
use axum::routing::post;
use serde_json::Value;

use crate::record_log::RecordLog;

/// One recorded `POST /_matrix/push/v1/notify` call: the full `notification` object from the
/// request body.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct RecordedNotification {
    /// The raw `notification` JSON object (event id, room id, counts, devices, ...).
    pub notification: Value,
}

struct Inner {
    log: RecordLog,
    rejected_pushkeys: Mutex<HashSet<String>>,
}

/// A fake push gateway.
#[derive(Clone)]
pub struct FakePushGateway {
    inner: Arc<Inner>,
}

impl Default for FakePushGateway {
    fn default() -> Self {
        Self::new()
    }
}

impl FakePushGateway {
    /// A fresh fake gateway that accepts every pushkey.
    #[must_use]
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Inner {
                log: RecordLog::new(),
                rejected_pushkeys: Mutex::new(HashSet::new()),
            }),
        }
    }

    /// From now on, `notify` responses list `pushkey` under `rejected`, simulating a stale push
    /// token the homeserver should stop delivering to.
    pub fn reject_pushkey(&self, pushkey: impl Into<String>) {
        self.inner
            .rejected_pushkeys
            .lock()
            .unwrap()
            .insert(pushkey.into());
    }

    /// The axum router fragment: `POST /_matrix/push/v1/notify`.
    pub fn router(&self) -> Router {
        Router::new()
            .route("/_matrix/push/v1/notify", post(notify))
            .layer(Extension(self.inner.clone()))
    }

    /// Every notification recorded so far, oldest first.
    #[must_use]
    pub fn notifications(&self) -> Vec<RecordedNotification> {
        self.inner
            .log
            .all_as()
            .expect("this fake only ever writes RecordedNotification values")
    }
}

async fn notify(Extension(inner): Extension<Arc<Inner>>, Json(body): Json<Value>) -> Json<Value> {
    let notification = body.get("notification").cloned().unwrap_or(Value::Null);
    let pushkeys: Vec<String> = notification
        .get("devices")
        .and_then(Value::as_array)
        .map(|devices| {
            devices
                .iter()
                .filter_map(|d| d.get("pushkey").and_then(Value::as_str))
                .map(String::from)
                .collect()
        })
        .unwrap_or_default();

    inner.log.record(&RecordedNotification { notification });

    let rejected = inner.rejected_pushkeys.lock().unwrap();
    let rejected: Vec<&str> = pushkeys
        .iter()
        .map(String::as_str)
        .filter(|pk| rejected.contains(*pk))
        .collect();
    Json(serde_json::json!({"rejected": rejected}))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    #[tokio::test]
    async fn records_a_notification_and_accepts_by_default() {
        let gw = FakePushGateway::new();
        let response = gw
            .router()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/_matrix/push/v1/notify")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        r#"{"notification": {"event_id": "$1", "devices": [{"pushkey": "abc"}]}}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["rejected"], serde_json::json!([]));

        assert_eq!(gw.notifications().len(), 1);
        assert_eq!(gw.notifications()[0].notification["event_id"], "$1");
    }

    #[tokio::test]
    async fn rejected_pushkeys_come_back_in_the_response() {
        let gw = FakePushGateway::new();
        gw.reject_pushkey("stale-key");

        let response = gw
            .router()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/_matrix/push/v1/notify")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        r#"{"notification": {"devices": [{"pushkey": "stale-key"}, {"pushkey": "good-key"}]}}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["rejected"], serde_json::json!(["stale-key"]));
    }
}
