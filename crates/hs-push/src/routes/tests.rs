//! HTTP-level tests of the `/pushrules` and `/pushers` surface: the router fragment with
//! in-memory stores and one registered user, driven through `tower::ServiceExt::oneshot`.
//!
//! The error-case table mirrors Sytest's `tests/61push/80torture.pl` (Apache-2.0): every path
//! shape it says must be a `400`, and the two that must be a `404`.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use hs_auth::state::AuthState;
use hs_auth::store::{AccessTokenRecord, UserRecord};
use hs_auth::token::TokenHash;
use hs_kv::memory::MemoryBackend;
use ruma::user_id;
use serde_json::{Value, json};
use tower::ServiceExt;

use crate::counts::memory::InMemoryCountsStore;
use crate::notification_log::memory::InMemoryNotificationLogStore;
use crate::pushers::http::{HttpPusherClient, RetryPolicy};
use crate::pushers::memory::InMemoryPusherStore;
use crate::rulesets::CachedRulesetStore;
use crate::rulesets::tables::TablesRulesetStore;
use crate::state::PushState;

const TOKEN: &str = "syt_alice_token";

/// A `PushState` over in-memory stores with `@alice:example.org` logged in as [`TOKEN`].
pub(crate) async fn state() -> PushState<MemoryBackend> {
    let auth = AuthState::in_memory();
    let alice = user_id!("@alice:example.org").to_owned();
    auth.store
        .create_user(UserRecord::new(alice.clone(), 0))
        .await
        .unwrap();
    auth.store
        .put_access_token(AccessTokenRecord {
            hash: TokenHash::of(TOKEN),
            user_id: alice,
            device_id: None,
            expires_at_ms: None,
            refresh_token_hash: None,
            last_used_ms: None,
        })
        .await
        .unwrap();
    PushState {
        auth,
        rulesets: Arc::new(CachedRulesetStore::new(
            TablesRulesetStore::open(MemoryBackend::new()).unwrap(),
        )),
        pushers: Arc::new(InMemoryPusherStore::new()),
        counts: Arc::new(InMemoryCountsStore::new()),
        notification_log: Arc::new(InMemoryNotificationLogStore::new()),
        http_pushers: Arc::new(HttpPusherClient::new(RetryPolicy::default())),
        pipeline: None,
    }
}

pub(crate) async fn call(
    state: &PushState<MemoryBackend>,
    method: &str,
    path: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let (router, _) = super::router::<MemoryBackend>();
    let app = router.with_state(state.clone());
    let mut request = Request::builder()
        .method(method)
        .uri(path)
        .header("authorization", format!("Bearer {TOKEN}"));
    if body.is_some() {
        request = request.header("content-type", "application/json");
    }
    let request = request
        .body(Body::from(body.map(|b| b.to_string()).unwrap_or_default()))
        .unwrap();
    let response = app.oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json = serde_json::from_slice(&bytes)
        .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into_owned()));
    (status, json)
}

#[tokio::test]
async fn a_room_rule_shows_up_everywhere_the_spec_lists_rules() {
    let state = state().await;
    let (status, body) = call(
        &state,
        "PUT",
        "/pushrules/global/room/%23spam%3Aexample.com",
        Some(json!({"actions": ["notify"]})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let (status, rule) = call(
        &state,
        "GET",
        "/pushrules/global/room/%23spam%3Aexample.com",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(rule["rule_id"], "#spam:example.com");
    assert_eq!(rule["enabled"], true);
    assert_eq!(rule["default"], false);
    assert_eq!(rule["actions"], json!(["notify"]));

    let (status, list) = call(&state, "GET", "/pushrules/global/room/", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(list[0]["rule_id"], "#spam:example.com");

    let (status, scope) = call(&state, "GET", "/pushrules/global/", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(scope["room"][0]["rule_id"], "#spam:example.com");
    assert!(scope["override"].is_array());

    let (status, all) = call(&state, "GET", "/pushrules/", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(all["global"]["room"][0]["rule_id"], "#spam:example.com");

    let (status, enabled) = call(
        &state,
        "GET",
        "/pushrules/global/room/%23spam%3Aexample.com/enabled",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(enabled, json!({"enabled": true}));

    let (status, actions) = call(
        &state,
        "GET",
        "/pushrules/global/room/%23spam%3Aexample.com/actions",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(actions, json!({"actions": ["notify"]}));
}

#[tokio::test]
async fn before_after_delete_and_disable() {
    let state = state().await;
    for id in ["%23a", "%23b"] {
        let (status, _) = call(
            &state,
            "PUT",
            &format!("/pushrules/global/room/{id}"),
            Some(json!({"actions": ["notify"]})),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
    }
    let (status, _) = call(
        &state,
        "PUT",
        "/pushrules/global/room/%23c?before=%23a",
        Some(json!({"actions": ["notify"]})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let ids = |v: &Value| {
        v["global"]["room"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["rule_id"].as_str().unwrap().to_owned())
            .collect::<Vec<_>>()
    };
    let (_, all) = call(&state, "GET", "/pushrules/", None).await;
    assert_eq!(ids(&all), ["#b", "#c", "#a"]);

    let (status, _) = call(&state, "DELETE", "/pushrules/global/room/%23a", None).await;
    assert_eq!(status, StatusCode::OK);
    let (_, all) = call(&state, "GET", "/pushrules/", None).await;
    assert_eq!(ids(&all), ["#b", "#c"]);

    let (status, _) = call(
        &state,
        "PUT",
        "/pushrules/global/room/%23b/enabled",
        Some(json!({"enabled": false})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (_, rule) = call(&state, "GET", "/pushrules/global/room/%23b", None).await;
    assert_eq!(rule["enabled"], false);

    let (status, _) = call(
        &state,
        "PUT",
        "/pushrules/global/underride/.m.rule.message/actions",
        Some(json!({"actions": ["dont_notify"]})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (_, actions) = call(
        &state,
        "GET",
        "/pushrules/global/underride/.m.rule.message/actions",
        None,
    )
    .await;
    assert_eq!(actions, json!({"actions": ["dont_notify"]}));
}

#[tokio::test]
async fn malformed_puts_are_400() {
    let state = state().await;
    let cases: [(&str, Value); 15] = [
        ("/pushrules/", json!({})),
        (
            "/pushrules/not_a_scope/room/%23spam%3Aexample.com",
            json!({}),
        ),
        ("/pushrules/global", json!({})),
        ("/pushrules/global/room", json!({})),
        ("/pushrules/global/room/", json!({})),
        ("/pushrules/global/not_a_template/foo", json!({})),
        ("/pushrules/global/room/%23fo%5Co%3Aexample.com", json!({})),
        (
            "/pushrules/global/override/my_id",
            json!({"actions": ["notify"]}),
        ),
        (
            "/pushrules/global/underride/my_id",
            json!({"actions": ["notify"]}),
        ),
        (
            "/pushrules/global/underride/my_id",
            json!({"actions": ["notify"], "conditions": [{}]}),
        ),
        (
            "/pushrules/global/content/my_id",
            json!({"actions": ["notify"]}),
        ),
        ("/pushrules/global/room/%23my_room%3Aexample.com", json!({})),
        (
            "/pushrules/global/room/%23my_room%3Aexample.com",
            json!({"actions": ["not_an_action"]}),
        ),
        (
            "/pushrules/global/override/.m.rule.master/not_an_attr",
            json!({"enabled": true}),
        ),
        (
            "/pushrules/global/override/.m.rule.master/enabled",
            json!({"enabled": "not a boolean"}),
        ),
    ];
    for (path, body) in cases {
        let (status, _) = call(&state, "PUT", path, Some(body)).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "PUT {path}");
    }
}

#[tokio::test]
async fn malformed_gets_are_400_and_unknown_rules_404() {
    let state = state().await;
    for path in [
        "/pushrules/global",
        "/pushrules/global/room",
        "/pushrules/not_a_scope/",
        "/pushrules/global/not_a_template/",
        "/pushrules/global/override/.m.rule.master/not_an_attr",
    ] {
        let (status, _) = call(&state, "GET", path, None).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "GET {path}");
    }
    let (status, _) = call(&state, "GET", "/pushrules", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = call(
        &state,
        "GET",
        "/pushrules/global/override/not_a_rule_id",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = call(
        &state,
        "PUT",
        "/pushrules/global/override/.not.a.default.rule/actions",
        Some(json!({"actions": ["notify"]})),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = call(
        &state,
        "PUT",
        "/pushrules/global/sender/%40bob%3Aexample.com/actions",
        Some(json!({"actions": ["notify"]})),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

/// MSC4306's `postcontent` kind is a kind like the others on every path (Complement's
/// `disableMsc4306PushRules` asks for its rules and expects a `404` when a server has none),
/// with no rules here and none a client may add (Synapse's `test_no_user_defined_postcontent_rules`).
#[tokio::test]
async fn postcontent_is_a_kind_with_no_rules_and_none_a_client_may_add() {
    let state = state().await;
    let (status, body) = call(&state, "GET", "/pushrules/global/postcontent/", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, json!([]));
    for path in [
        "/pushrules/global/postcontent/.io.element.msc4306.rule.subscribed_thread",
        "/pushrules/global/postcontent/.io.element.msc4306.rule.unsubscribed_thread",
        "/pushrules/global/postcontent/.io.element.msc4306.rule.subscribed_thread/enabled",
    ] {
        let (status, _) = call(&state, "GET", path, None).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "GET {path}");
    }
    let (status, _) = call(
        &state,
        "PUT",
        "/pushrules/global/postcontent/.io.element.msc4306.rule.subscribed_thread/enabled",
        Some(json!({"enabled": false})),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, body) = call(
        &state,
        "PUT",
        "/pushrules/global/postcontent/some.user.rule",
        Some(json!({"actions": ["notify"], "conditions": []})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["errcode"], "M_INVALID_PARAM");
    let (_, all) = call(&state, "GET", "/pushrules/", None).await;
    assert!(
        all["global"].get("postcontent").is_none(),
        "an empty postcontent list stays out of the spec's shape"
    );
}

#[tokio::test]
async fn an_unstable_action_is_refused_so_clients_can_tell() {
    let state = state().await;
    let (status, _) = call(
        &state,
        "PUT",
        "/pushrules/global/content/anything",
        Some(json!({"pattern": "*", "actions": ["org.matrix.msc2625.mark_unread"]})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn notifications_page_newest_first_with_a_token() {
    use crate::notification_log::NewNotification;
    let state = state().await;
    let alice = user_id!("@alice:example.org");
    let room = ruma::room_id!("!room:example.org");
    for i in 1..=3u64 {
        let event_id = ruma::OwnedEventId::try_from(format!("${i}:example.org")).unwrap();
        state
            .notification_log
            .append(
                alice,
                NewNotification {
                    room_id: room.to_owned(),
                    event_id: event_id.clone(),
                    event: json!({"event_id": event_id, "type": "m.room.message"}),
                    actions: vec![ruma::push::Action::Notify],
                    profile_tag: None,
                    ts_ms: 1000 * i,
                    pos: i64::try_from(i).ok(),
                    thread: None,
                },
            )
            .await
            .unwrap();
    }
    let (status, body) = call(&state, "GET", "/notifications?limit=2", None).await;
    assert_eq!(status, StatusCode::OK);
    let page = body["notifications"].as_array().unwrap();
    assert_eq!(page.len(), 2);
    assert_eq!(page[0]["event"]["event_id"], "$3:example.org");
    assert_eq!(page[0]["read"], false);
    assert!(page[0].get("profile_tag").is_some());
    assert_eq!(page[0]["ts"], 3000);
    let token = body["next_token"].as_str().unwrap().to_owned();
    let (_, rest) = call(&state, "GET", &format!("/notifications?from={token}"), None).await;
    let rest = rest["notifications"].as_array().unwrap();
    assert_eq!(rest.len(), 1);
    assert_eq!(rest[0]["event"]["event_id"], "$1:example.org");
}

/// An email pusher is refused unless its address is bound to the account, and comes back as
/// `kind: email` once it is.
#[tokio::test]
async fn an_email_pusher_needs_an_address_the_account_owns() {
    let state = state().await;
    let body = |address: &str| {
        json!({
            "pushkey": address,
            "app_id": "m.email",
            "kind": "email",
            "app_display_name": "Email Notifications",
            "device_display_name": address,
            "lang": "en",
            "data": {},
        })
    };
    let (status, error) = call(
        &state,
        "POST",
        "/pushers/set",
        Some(body("alice@example.org")),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{error}");
    assert_eq!(error["errcode"], "M_INVALID_PARAM");
    let (status, error) = call(&state, "POST", "/pushers/set", Some(body("not-an-address"))).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{error}");

    state
        .auth
        .store
        .add_threepid(hs_auth::store::ThreepidRecord {
            user_id: user_id!("@alice:example.org").to_owned(),
            medium: "email".to_owned(),
            address: "alice@example.org".to_owned(),
            added_at_ms: 1,
            validated_at_ms: 1,
        })
        .await
        .unwrap();
    let (status, _) = call(
        &state,
        "POST",
        "/pushers/set",
        Some(body("Alice@Example.org")),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "the address compares case-insensitively"
    );
    let (_, pushers) = call(&state, "GET", "/pushers", None).await;
    assert_eq!(pushers["pushers"][0]["kind"], "email");
    assert_eq!(pushers["pushers"][0]["pushkey"], "Alice@Example.org");
}
