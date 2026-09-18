//! A fake application service receiver: the `PUT /transactions/{txnId}` endpoint a homeserver
//! calls to push events to a registered appservice (`refs/matrix-spec/data/api/application-service/transactions.yaml`),
//! recording every transaction body for later assertion instead of doing anything with them.
//!
//! Mount [`FakeAppservice::router`] under whatever base path the appservice registration's `url`
//! points a client at (a [`Scenario`](crate::scenario::Scenario) driving the homeserver-side
//! router and a direct check of [`FakeAppservice::transactions`] together prove "the homeserver
//! pushed the right thing to the appservice").

use axum::Json;
use axum::extract::Path;
use axum::http::StatusCode;
use axum::routing::put;
use axum::{Router, extract::Extension};
use serde_json::Value;
use std::sync::Arc;

use crate::record_log::RecordLog;

/// One recorded `PUT /transactions/{txnId}` call.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct RecordedTransaction {
    /// The `{txnId}` path segment, as sent by the homeserver.
    pub txn_id: String,
    /// The raw JSON body (`{"events": [...], ...}`, plus whatever MSC extensions were present).
    pub body: Value,
}

/// A fake appservice: records every pushed transaction, always answers `200 {}` (matching the
/// spec's expected success response), and never rejects or reorders anything. Tests that need to
/// exercise retry/backoff behavior should wrap the router with their own failure injection rather
/// than this type growing configurable failure modes it does not need yet.
#[derive(Clone)]
pub struct FakeAppservice {
    log: Arc<RecordLog>,
}

impl Default for FakeAppservice {
    fn default() -> Self {
        Self::new()
    }
}

impl FakeAppservice {
    /// A fresh fake appservice with no recorded transactions.
    #[must_use]
    pub fn new() -> Self {
        Self {
            log: Arc::new(RecordLog::new()),
        }
    }

    /// The axum router fragment: one route, `PUT /transactions/{txnId}`. Nest or nest-and-strip a
    /// prefix as the test's registration URL requires.
    pub fn router(&self) -> Router {
        Router::new()
            .route("/transactions/{txnId}", put(put_transaction))
            .layer(Extension(self.log.clone()))
    }

    /// Every transaction recorded so far, oldest first.
    #[must_use]
    pub fn transactions(&self) -> Vec<RecordedTransaction> {
        self.log
            .all_as()
            .expect("this fake only ever writes RecordedTransaction values")
    }

    /// How many transactions have been recorded.
    #[must_use]
    pub fn transaction_count(&self) -> usize {
        self.log.len()
    }
}

async fn put_transaction(
    Path(txn_id): Path<String>,
    Extension(log): Extension<Arc<RecordLog>>,
    Json(body): Json<Value>,
) -> (StatusCode, Json<Value>) {
    log.record(&RecordedTransaction { txn_id, body });
    (StatusCode::OK, Json(serde_json::json!({})))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    #[tokio::test]
    async fn records_a_pushed_transaction() {
        let appservice = FakeAppservice::new();
        let router = appservice.router();

        let response = router
            .oneshot(
                Request::builder()
                    .method("PUT")
                    .uri("/transactions/42")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"events": [{"type": "m.room.message"}]}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let transactions = appservice.transactions();
        assert_eq!(transactions.len(), 1);
        assert_eq!(transactions[0].txn_id, "42");
        assert_eq!(transactions[0].body["events"][0]["type"], "m.room.message");
    }

    #[tokio::test]
    async fn records_transactions_in_order() {
        let appservice = FakeAppservice::new();
        for i in 0..3 {
            let router = appservice.router();
            let response = router
                .oneshot(
                    Request::builder()
                        .method("PUT")
                        .uri(format!("/transactions/{i}"))
                        .header("content-type", "application/json")
                        .body(Body::from("{}"))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
        }
        let transactions = appservice.transactions();
        assert_eq!(
            transactions
                .iter()
                .map(|t| t.txn_id.clone())
                .collect::<Vec<_>>(),
            vec!["0", "1", "2"]
        );
    }
}
