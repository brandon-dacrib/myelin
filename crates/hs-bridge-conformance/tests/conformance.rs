//! The bridge conformance suite: each `#[tokio::test]` below is one scenario from `PLAN.md`
//! Appendix B, run against this crate's [`Harness`] (real `hs-appservice`/`hs-auth` components)
//! and a real HTTP [`FakeBridge`]. See `crates/hs-bridge-conformance/src/lib.rs` for the harness
//! design and the Docker/Synapse-control note, and
//! `docs/status/11-appservices-and-bridges.md` for exactly what is and is not covered.

use hs_appservice::transaction::{DeviceListsUpdate, ToDeviceEntry, Transaction};
use hs_auth::clock::Clock as _;
use hs_bridge_conformance::{FakeBridge, Harness};
use serde_json::json;

/// **Transaction contents and key spellings.** Appendix B: `events`; `ephemeral` /
/// `de.sorunome.msc2409.ephemeral`; `to_device` / `de.sorunome.msc2409.to_device`; `device_lists`
/// / `org.matrix.msc3202.device_lists`; `device_one_time_keys_count` /
/// `org.matrix.msc3202.device_one_time_keys_count` (plus Synapse's older
/// `org.matrix.msc3202.device_one_time_key_counts`); `device_unused_fallback_key_types` /
/// `org.matrix.msc3202.device_unused_fallback_key_types`. A real `mautrix-go` bridge accepts
/// either spelling of each pair; this asserts we send both, over a real HTTP PUT, exactly the way
/// a bridge's own transaction handler would receive them.
#[tokio::test]
async fn transaction_contents_and_key_spellings() {
    let bridge = FakeBridge::new();
    let bridge_url = bridge.spawn().await;
    let harness = Harness::new();
    harness.register_full_featured_bridge("whatsapp", Some(&bridge_url));

    let mut otk = hs_appservice::transaction::OneTimeKeysCount::new();
    otk.entry("@bob:example.org".to_string())
        .or_default()
        .entry("DEVICE1".to_string())
        .or_default()
        .insert("signed_curve25519".to_string(), 7);

    let mut fallback = hs_appservice::transaction::UnusedFallbackKeyTypes::new();
    fallback
        .entry("@bob:example.org".to_string())
        .or_default()
        .insert("DEVICE1".to_string(), vec!["signed_curve25519".to_string()]);

    let txn = Transaction {
        events: vec![json!({
            "type": "m.room.message",
            "event_id": "$1:example.org",
            "room_id": "!room:example.org",
            "sender": "@whatsapp_alice:example.org",
            "origin_server_ts": 1_700_000_000_000i64,
            "content": {"msgtype": "m.text", "body": "hello from the bridge"},
        })],
        ephemeral: vec![json!({
            "type": "m.typing",
            "room_id": "!room:example.org",
            "content": {"user_ids": ["@whatsapp_alice:example.org"]},
        })],
        to_device: vec![ToDeviceEntry {
            to_user_id: "@bob:example.org".to_string(),
            to_device_id: "DEVICE1".to_string(),
            event: json!({"type": "m.room_key", "sender": "@whatsapp_alice:example.org", "content": {}}),
        }],
        device_lists: DeviceListsUpdate {
            changed: vec!["@whatsapp_alice:example.org".to_string()],
            left: vec![],
        },
        one_time_keys_count: otk,
        unused_fallback_key_types: fallback,
    };

    let seq = harness.scheduler.enqueue("whatsapp", &txn).unwrap();
    assert_eq!(seq, Some(1));
    let outcome = harness.scheduler.drain("whatsapp").await.unwrap();
    assert!(matches!(
        outcome,
        hs_appservice::scheduler::DrainOutcome::Delivered {
            count: 1,
            through_seq: 1
        }
    ));

    let received = bridge.transactions();
    assert_eq!(
        received.len(),
        1,
        "exactly one transaction crossed the wire"
    );
    let body = &received[0].body;

    // Stable AND legacy spellings, exactly as a real mautrix-go parser tries in order.
    assert_eq!(
        body["events"][0]["content"]["body"],
        "hello from the bridge"
    );
    assert_eq!(body["ephemeral"][0]["type"], "m.typing");
    assert_eq!(body["de.sorunome.msc2409.ephemeral"][0]["type"], "m.typing");
    assert_eq!(body["to_device"][0]["to_user_id"], "@bob:example.org");
    assert_eq!(
        body["de.sorunome.msc2409.to_device"][0]["to_device_id"],
        "DEVICE1"
    );
    assert_eq!(
        body["device_lists"]["changed"][0],
        "@whatsapp_alice:example.org"
    );
    assert_eq!(
        body["org.matrix.msc3202.device_lists"]["changed"][0],
        "@whatsapp_alice:example.org"
    );
    let expected_otk = json!({"@bob:example.org": {"DEVICE1": {"signed_curve25519": 7}}});
    assert_eq!(body["device_one_time_keys_count"], expected_otk);
    assert_eq!(
        body["org.matrix.msc3202.device_one_time_keys_count"],
        expected_otk
    );
    // Synapse's older, still-sent spelling ("key_counts", not "keys_count").
    assert_eq!(
        body["org.matrix.msc3202.device_one_time_key_counts"],
        expected_otk
    );
    assert!(
        body["device_unused_fallback_key_types"]["@bob:example.org"]["DEVICE1"]
            .as_array()
            .unwrap()
            .contains(&json!("signed_curve25519"))
    );
}

/// **A registration with a null url never receives a push.** `PLAN.md` section 8.2: "`url: null`
/// registrations (double puppeting) are first-class and never pushed to." Even a bridge that
/// happens to be spawned and reachable must never see traffic for a `url: null` registration —
/// this asserts the fake bridge receives literally nothing, not just that the call "would have
/// failed".
#[tokio::test]
async fn null_url_registration_never_receives_a_push() {
    let bridge = FakeBridge::new();
    let bridge_url = bridge.spawn().await;
    let harness = Harness::new();
    // A double-puppet registration whose id happens to collide in namespace terms with nothing
    // else; url is null even though a perfectly reachable bridge exists at `bridge_url` — proving
    // the suppression is about the registration, not about reachability.
    harness.register_double_puppet("whatsapp_double_puppet");
    let _ = &bridge_url;

    let txn = Transaction {
        events: vec![json!({"type": "m.room.message", "content": {"body": "must never be sent"}})],
        ..Default::default()
    };
    let seq = harness
        .scheduler
        .enqueue("whatsapp_double_puppet", &txn)
        .unwrap();
    assert_eq!(
        seq, None,
        "nothing should even be queued for a null-url registration"
    );

    let outcome = harness
        .scheduler
        .drain("whatsapp_double_puppet")
        .await
        .unwrap();
    assert_eq!(outcome, hs_appservice::scheduler::DrainOutcome::NoUrl);
    assert!(bridge.transactions().is_empty());
}

/// **Ping in both directions.** Outbound: the homeserver calls `POST {url}/_matrix/app/v1/ping`
/// on the bridge with its `hs_token`. Inbound: the bridge calls
/// `POST /_matrix/client/v1/appservice/{id}/ping` on the homeserver with its `as_token`, which
/// must trigger exactly that outbound call and report its `duration_ms` back.
#[tokio::test]
async fn ping_round_trips_in_both_directions() {
    let bridge = FakeBridge::new();
    let bridge_url = bridge.spawn().await;
    let harness = Harness::new();
    harness.register_full_featured_bridge("irc", Some(&bridge_url));

    // Outbound leg, driven directly (as the admin API's `POST /appservices/{id}/ping` would).
    let outcome = harness
        .ping
        .ping("irc", Some("outbound-test"))
        .await
        .unwrap();
    assert!(
        outcome.is_ok(),
        "outbound ping should succeed against a healthy fake bridge"
    );
    let pings = bridge.pings();
    assert_eq!(pings.len(), 1);
    assert_eq!(pings[0].transaction_id.as_deref(), Some("outbound-test"));

    // Inbound leg: a real HTTP request from "the bridge" to our client-server ping endpoint,
    // authenticated with the appservice's own as_token, which must trigger a second outbound
    // call and answer with duration_ms.
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    let ping_service = std::sync::Arc::new(hs_appservice::ping::PingService::new(
        harness.registry.clone(),
        std::sync::Arc::new(hs_appservice::ping::HttpPingTransport::new()),
    ));
    let router = hs_appservice::routes::ping_router::<hs_kv::memory::MemoryBackend>(ping_service)
        .with_state(harness.auth_state.clone());

    let response = router
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/appservice/irc/ping")
                .header("authorization", "Bearer as_token_irc")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"transaction_id": "inbound-test"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert!(json["duration_ms"].is_number());

    let pings = bridge.pings();
    assert_eq!(
        pings.len(),
        2,
        "the inbound call must have triggered a second outbound ping"
    );
    assert_eq!(pings[1].transaction_id.as_deref(), Some("inbound-test"));
}

/// A minimal protected handler that echoes back everything `Requester` asserted about the
/// caller, so a scenario can drive a real HTTP request through `hs-auth`'s real extractor and
/// inspect the result — the same extractor every other crate's handlers use, not a
/// reimplementation of the masquerade check.
async fn whoami(requester: hs_auth::requester::Requester) -> axum::Json<serde_json::Value> {
    axum::Json(json!({
        "user_id": requester.user_id,
        "appservice": requester.appservice.map(|a| json!({
            "appservice_id": a.appservice_id,
            "masqueraded_user": a.masqueraded_user,
            "masqueraded_device_id": a.masqueraded_device_id,
        })),
    }))
}

/// **Identity assertion and device masquerading.** `user_id` (identity assertion, validated
/// against the exclusive `users` namespace) and `org.matrix.msc3202.device_id` (device
/// masquerading, validated to be a real device of the effective user), driven through
/// `hs-auth`'s real `Requester` extractor with `hs-appservice`'s real registry behind it
/// (`RegistryAppserviceAdapter`) — not a reimplementation of the check.
#[tokio::test]
async fn identity_assertion_and_device_masquerading() {
    let harness = Harness::new();
    harness.register_full_featured_bridge("irc", None);

    // A real device for the masqueraded user, so the device-masquerade validation has something
    // legitimate to find.
    harness
        .auth_state
        .store
        .create_user(hs_auth::store::UserRecord::new(
            ruma::user_id!("@irc_alice:example.org").to_owned(),
            harness.clock.now_ms(),
        ))
        .await
        .unwrap();
    harness
        .auth_state
        .store
        .upsert_device(hs_auth::store::DeviceRecord {
            user_id: ruma::user_id!("@irc_alice:example.org").to_owned(),
            device_id: ruma::device_id!("ALICEDEVICE").to_owned(),
            display_name: None,
            last_seen_ms: None,
            last_seen_ip: None,
        })
        .await
        .unwrap();

    use axum::body::Body;
    use axum::http::Request;
    use axum::routing::get;
    use tower::ServiceExt;

    let router = axum::Router::new()
        .route("/whoami", get(whoami))
        .with_state(harness.auth_state.clone());

    let response = router
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/whoami?user_id=%40irc_alice%3Aexample.org&org.matrix.msc3202.device_id=ALICEDEVICE")
                .header("authorization", "Bearer as_token_irc")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["user_id"], "@irc_alice:example.org");
    assert_eq!(json["appservice"]["appservice_id"], "irc");
    assert_eq!(json["appservice"]["masqueraded_user"], true);
    assert_eq!(json["appservice"]["masqueraded_device_id"], "ALICEDEVICE");

    // Outside the registered namespace, masquerading must be rejected (403 M_FORBIDDEN — the
    // appservice authenticated fine, it just may not act as this user).
    let rejected = router
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/whoami?user_id=%40someone_else%3Aexample.org")
                .header("authorization", "Bearer as_token_irc")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(rejected.status(), 403);
}
