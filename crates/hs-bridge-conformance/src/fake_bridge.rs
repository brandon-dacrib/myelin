//! A real HTTP server standing in for a `mautrix-go`/`mautrix-python` bridge's own listener:
//! everything in the Application Service API that the *bridge* serves
//! (`PUT /transactions/{txnId}`, `POST /ping`) and everything a bridge answers about itself
//! (`GET /users/{userId}`, `GET /rooms/{roomAlias}`), all under `/_matrix/app/v1`, matching the
//! real path shape so [`hs_appservice::scheduler::HttpTransactionSender`],
//! [`hs_appservice::ping::HttpPingTransport`] and [`hs_appservice::query::HttpQueryTransport`] hit
//! it exactly as they would a real bridge — this crate never special-cases "is this the fake
//! bridge" on the sending side.
//!
//! Unlike `hs-testkit`'s `FakeAppservice` (which is mounted in-process with `tower::ServiceExt`
//! for handler-level tests), this one is bound to a real loopback TCP port with `axum::serve`, so
//! a `reqwest`-based sender genuinely round-trips over HTTP — the point of a *conformance* suite
//! is that wire serialization is exercised for real, not skipped by an in-process shortcut.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use axum::extract::{Extension, Path};
use axum::http::StatusCode;
use axum::routing::{get, post, put};
use axum::{Json, Router};
use serde_json::Value;

/// One recorded `PUT /transactions/{txnId}` call.
#[derive(Debug, Clone)]
pub struct RecordedTransaction {
    /// The `txnId` path segment.
    pub txn_id: String,
    /// The full JSON body, exactly as received.
    pub body: Value,
}

/// One recorded `POST /ping` call.
#[derive(Debug, Clone)]
pub struct RecordedPing {
    /// The `transaction_id` field, if the caller sent one.
    pub transaction_id: Option<String>,
}

#[derive(Default)]
struct State {
    transactions: Vec<RecordedTransaction>,
    pings: Vec<RecordedPing>,
    /// User IDs this fake bridge claims it can provision (drives `GET /users/{userId}`).
    known_users: std::collections::HashSet<String>,
    /// Room aliases this fake bridge claims it can provision.
    known_aliases: std::collections::HashSet<String>,
    /// If set, every `PUT /transactions` and `POST /ping` call returns this status instead of
    /// 200 — for exercising the scheduler's/ping service's failure and retry paths against a
    /// real HTTP round trip rather than a mock transport.
    fail_with_status: Option<u16>,
}

/// A synthetic bridge HTTP server: bind it, hand its base URL to `hs-appservice`'s senders, and
/// inspect what arrived.
#[derive(Clone)]
pub struct FakeBridge {
    state: Arc<Mutex<State>>,
}

impl Default for FakeBridge {
    fn default() -> Self {
        Self::new()
    }
}

impl FakeBridge {
    /// A fresh fake bridge with nothing recorded and no known users/aliases.
    #[must_use]
    pub fn new() -> Self {
        Self {
            state: Arc::new(Mutex::new(State::default())),
        }
    }

    /// Marks `user_id` as one this fake bridge claims to provision.
    pub fn add_known_user(&self, user_id: impl Into<String>) {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .known_users
            .insert(user_id.into());
    }

    /// Marks `room_alias` as one this fake bridge claims to provision.
    pub fn add_known_alias(&self, room_alias: impl Into<String>) {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .known_aliases
            .insert(room_alias.into());
    }

    /// Makes every subsequent transaction/ping call fail with `status` instead of succeeding.
    pub fn fail_with(&self, status: u16) {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .fail_with_status = Some(status);
    }

    /// Every transaction recorded so far, oldest first.
    #[must_use]
    pub fn transactions(&self) -> Vec<RecordedTransaction> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .transactions
            .clone()
    }

    /// Every ping recorded so far, oldest first.
    #[must_use]
    pub fn pings(&self) -> Vec<RecordedPing> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .pings
            .clone()
    }

    fn router(&self) -> Router {
        Router::new()
            .route(
                "/_matrix/app/v1/transactions/{txnId}",
                put(put_transaction),
            )
            .route("/_matrix/app/v1/ping", post(ping))
            .route("/_matrix/app/v1/users/{userId}", get(query_user))
            .route("/_matrix/app/v1/rooms/{roomAlias}", get(query_room_alias))
            .layer(Extension(self.state.clone()))
    }

    /// Binds to an ephemeral loopback port, serves in a background task, and returns the base
    /// URL (`http://127.0.0.1:PORT`) to hand to `hs-appservice`'s registration `url` field.
    pub async fn spawn(&self) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("binding a loopback port never fails in test environments");
        let addr = listener.local_addr().expect("bound listener has an addr");
        let app = self.router();
        tokio::spawn(async move {
            axum::serve(listener, app)
                .await
                .expect("fake bridge server");
        });
        format!("http://{addr}")
    }
}

fn maybe_fail(state: &Mutex<State>) -> Option<StatusCode> {
    state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .fail_with_status
        .map(|s| StatusCode::from_u16(s).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR))
}

async fn put_transaction(
    Path(txn_id): Path<String>,
    Extension(state): Extension<Arc<Mutex<State>>>,
    Json(body): Json<Value>,
) -> (StatusCode, Json<Value>) {
    if let Some(status) = maybe_fail(&state) {
        return (status, Json(serde_json::json!({"errcode": "M_UNKNOWN"})));
    }
    state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .transactions
        .push(RecordedTransaction { txn_id, body });
    (StatusCode::OK, Json(serde_json::json!({})))
}

async fn ping(
    Extension(state): Extension<Arc<Mutex<State>>>,
    body: Option<Json<HashMap<String, Value>>>,
) -> (StatusCode, Json<Value>) {
    if let Some(status) = maybe_fail(&state) {
        return (status, Json(serde_json::json!({"errcode": "M_UNKNOWN"})));
    }
    let transaction_id = body
        .and_then(|Json(m)| m.get("transaction_id").and_then(Value::as_str).map(str::to_string));
    state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .pings
        .push(RecordedPing { transaction_id });
    (StatusCode::OK, Json(serde_json::json!({})))
}

async fn query_user(
    Path(user_id): Path<String>,
    Extension(state): Extension<Arc<Mutex<State>>>,
) -> StatusCode {
    let known = state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .known_users
        .contains(&user_id);
    if known {
        StatusCode::OK
    } else {
        StatusCode::NOT_FOUND
    }
}

async fn query_room_alias(
    Path(room_alias): Path<String>,
    Extension(state): Extension<Arc<Mutex<State>>>,
) -> StatusCode {
    let known = state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .known_aliases
        .contains(&room_alias);
    if known {
        StatusCode::OK
    } else {
        StatusCode::NOT_FOUND
    }
}
