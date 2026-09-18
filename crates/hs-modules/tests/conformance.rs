//! Module-protocol conformance tests (track 15's definition of done: "module protocol
//! conformance tests with sample modules (a spam checker and a password auth provider)").
//!
//! Each sample module is a tiny axum server implementing the HTTP-callback protocol
//! (`hs_modules::callback`) directly (not through `hs-modules` itself, since the whole point is
//! that a module can be written without depending on this crate at all — any language that can
//! speak JSON over HTTP qualifies). `hs_modules::client::HttpCallbackClient` is the thing under
//! test: it must call these modules correctly and fall back correctly when they don't answer.

use std::net::SocketAddr;

use axum::extract::Json;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::post;
use hs_modules::ModuleHooks;
use hs_modules::client::HttpCallbackClient;
use hs_modules::hooks::{AuthResult, CheckResult, EventForCheck};
use serde_json::{Value, json};

async fn spawn(router: axum::Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr: SocketAddr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    format!("http://{addr}")
}

/// A spam checker that denies any event whose `content.body` contains "spam", and has no
/// opinion (404) on anything else it might be asked, matching the protocol's "module doesn't
/// implement this hook" convention.
fn spam_checker_module() -> axum::Router {
    async fn check_event_for_spam(Json(body): Json<Value>) -> impl IntoResponse {
        let event = &body["payload"];
        let text = event["content"]["body"].as_str().unwrap_or_default();
        let result = if text.to_lowercase().contains("spam") {
            CheckResult::Deny {
                errcode: Some("M_FORBIDDEN".to_string()),
                reason: Some("spam-like content".to_string()),
            }
        } else {
            CheckResult::Allow
        };
        Json(json!({"protocol_version": "1", "result": result}))
    }
    axum::Router::new().route("/check_event_for_spam", post(check_event_for_spam))
}

/// A password auth provider that accepts exactly one hardcoded user/password pair and has no
/// opinion on anyone else.
fn password_auth_provider_module() -> axum::Router {
    async fn check_password(Json(body): Json<Value>) -> impl IntoResponse {
        let payload = &body["payload"];
        let user_id = payload["user_id"].as_str().unwrap_or_default();
        let password = payload["password"].as_str().unwrap_or_default();
        if user_id == "@ops:example.org" && password == "correct-horse-battery-staple" {
            let result = AuthResult {
                user_id: user_id.to_string(),
                display_name: Some("Operations".to_string()),
            };
            (
                StatusCode::OK,
                Json(json!({"protocol_version": "1", "result": result})),
            )
                .into_response()
        } else {
            // No opinion: defers to the next provider or the server's own check.
            StatusCode::NOT_FOUND.into_response()
        }
    }
    axum::Router::new().route("/check_password", post(check_password))
}

fn sample_event(body: &str) -> EventForCheck {
    EventForCheck {
        event_id: "$1".into(),
        room_id: "!r:example.org".into(),
        sender: "@a:example.org".into(),
        event_type: "m.room.message".into(),
        state_key: None,
        content: json!({"body": body}),
    }
}

#[tokio::test]
async fn spam_checker_denies_spam_and_allows_everything_else() {
    let base_url = spawn(spam_checker_module()).await;
    let client = HttpCallbackClient::new(base_url);

    let spam_result = client
        .check_event_for_spam(&sample_event("Buy crypto NOW, guaranteed spam returns!"))
        .await;
    assert!(
        !spam_result.is_allowed(),
        "expected spam to be denied, got {spam_result:?}"
    );

    let clean_result = client
        .check_event_for_spam(&sample_event("Good morning team"))
        .await;
    assert_eq!(clean_result, CheckResult::Allow);

    // The module has no opinion on `should_federate_room` (404 for that path): falls back to the
    // permissive default rather than erroring.
    assert!(client.should_federate_room("!r:example.org").await);
}

#[tokio::test]
async fn password_auth_provider_accepts_known_credentials_and_defers_otherwise() {
    let base_url = spawn(password_auth_provider_module()).await;
    let client = HttpCallbackClient::new(base_url);

    let ok = client
        .check_password("@ops:example.org", "correct-horse-battery-staple")
        .await;
    assert_eq!(
        ok,
        Some(AuthResult {
            user_id: "@ops:example.org".to_string(),
            display_name: Some("Operations".to_string())
        })
    );

    let wrong_password = client.check_password("@ops:example.org", "wrong").await;
    assert_eq!(
        wrong_password, None,
        "wrong password should defer (None), not error"
    );

    let unknown_user = client
        .check_password("@nobody:example.org", "whatever")
        .await;
    assert_eq!(unknown_user, None);
}

#[tokio::test]
async fn chain_composes_a_spam_checker_and_a_password_provider() {
    let spam_url = spawn(spam_checker_module()).await;
    let auth_url = spawn(password_auth_provider_module()).await;
    let chain = hs_modules::ModuleChain::new(vec![
        std::sync::Arc::new(HttpCallbackClient::new(spam_url)),
        std::sync::Arc::new(HttpCallbackClient::new(auth_url)),
    ]);

    assert!(
        !chain
            .check_event_for_spam(&sample_event("spam spam spam"))
            .await
            .is_allowed()
    );
    assert_eq!(
        chain
            .check_password("@ops:example.org", "correct-horse-battery-staple")
            .await,
        Some(AuthResult {
            user_id: "@ops:example.org".to_string(),
            display_name: Some("Operations".to_string())
        })
    );
}
