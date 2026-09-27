//! Reporting over real HTTP (`POST /rooms/{roomId}/report/{eventId}`, `POST
//! /rooms/{roomId}/report`, `POST /users/{userId}/report`), and what the admin API's report
//! source then shows a moderator: the report kept, the reported event beside it, and a decision
//! recorded once.

use std::sync::Arc;

use axum::http::{Method, StatusCode};
use hs_admin::reports::{
    ReportFilter, ReportKind, ReportResolution, ReportResolve, ReportSource, ReportStatus,
};
use hs_auth::state::AuthState;
use hs_kv::memory::MemoryBackend;
use hs_room::identity::HomeserverIdentity;
use hs_room::registry::RoomRegistry;
use hs_room::reports::RoomReports;
use hs_room::state::RoomState;
use hs_testkit::Scenario;
use serde_json::json;

fn app() -> (axum::Router, Arc<RoomRegistry<MemoryBackend>>) {
    let auth = AuthState::in_memory();
    let identity = HomeserverIdentity::for_tests("example.org");
    let registry =
        Arc::new(RoomRegistry::open(MemoryBackend::new(), identity.clone()).expect("registry"));
    let state = RoomState {
        auth: auth.clone(),
        rooms: registry.clone(),
        identity,
        remote_join: None,
    };
    let (rooms, _) = hs_room::routes::router::<MemoryBackend>();
    (
        hs_auth::routes::router()
            .with_state(auth)
            .merge(rooms.with_state(state)),
        registry,
    )
}

#[tokio::test]
async fn reports_are_kept_for_moderators_with_the_event_they_are_about() {
    let (router, registry) = app();
    let mut scenario = Scenario::new(router);
    for (name, password) in [
        ("alice", "alice-password-1"),
        ("mallory", "mallory-password-1"),
        ("eve", "eve-password-1"),
    ] {
        scenario.register(name, name, password).await.assert_ok();
    }

    let created = scenario
        .send(
            Some("alice"),
            Method::POST,
            "/createRoom",
            Some(json!({"preset": "public_chat", "name": "Lobby"})),
        )
        .await;
    created.assert_ok();
    let room_id = created.str_field("room_id").to_owned();
    scenario
        .send(
            Some("mallory"),
            Method::POST,
            &format!("/rooms/{room_id}/join"),
            Some(json!({})),
        )
        .await
        .assert_ok();
    let spam = scenario
        .send(
            Some("mallory"),
            Method::PUT,
            &format!("/rooms/{room_id}/send/m.room.message/t1"),
            Some(json!({"msgtype": "m.text", "body": "buy cheap watches"})),
        )
        .await;
    spam.assert_ok();
    let spam_id = spam.str_field("event_id").to_owned();
    let report_event = format!("/rooms/{room_id}/report/{spam_id}");

    // A score outside the spec's range is refused, and nothing is kept.
    scenario
        .send(
            Some("alice"),
            Method::POST,
            &report_event,
            Some(json!({"reason": "spam", "score": 7})),
        )
        .await
        .assert_matrix_error(StatusCode::BAD_REQUEST, "M_INVALID_PARAM");
    // An event that does not exist is not found.
    scenario
        .send(
            Some("alice"),
            Method::POST,
            &format!("/rooms/{room_id}/report/$nope"),
            Some(json!({})),
        )
        .await
        .assert_status(StatusCode::NOT_FOUND);

    let filed = scenario
        .send(
            Some("alice"),
            Method::POST,
            &report_event,
            Some(json!({"reason": "spam", "score": -100})),
        )
        .await;
    filed.assert_ok();
    assert_eq!(filed.json, json!({}));

    // A whole room, and a person: both need a reason.
    scenario
        .send(
            Some("eve"),
            Method::POST,
            &format!("/rooms/{room_id}/report"),
            Some(json!({})),
        )
        .await
        .assert_matrix_error(StatusCode::BAD_REQUEST, "M_MISSING_PARAM");
    scenario
        .send(
            Some("eve"),
            Method::POST,
            &format!("/rooms/{room_id}/report"),
            Some(json!({"reason": "this whole room is a scam"})),
        )
        .await
        .assert_ok();
    scenario
        .send(
            Some("eve"),
            Method::POST,
            "/rooms/!missing:example.org/report",
            Some(json!({"reason": "?"})),
        )
        .await
        .assert_status(StatusCode::NOT_FOUND);
    scenario
        .send(
            Some("alice"),
            Method::POST,
            "/users/@mallory:example.org/report",
            Some(json!({"reason": "keeps messaging me"})),
        )
        .await
        .assert_ok();
    scenario
        .send(
            Some("alice"),
            Method::POST,
            "/users/@nobody:example.org/report",
            Some(json!({"reason": "?"})),
        )
        .await
        .assert_status(StatusCode::NOT_FOUND);
    // Someone on another server cannot be checked from here; the report is kept.
    scenario
        .send(
            Some("alice"),
            Method::POST,
            "/users/@someone:elsewhere.example/report",
            Some(json!({"reason": "spam invites"})),
        )
        .await
        .assert_ok();

    // What the Reports inbox reads.
    let source = RoomReports::new(registry);
    let all = source.list(&ReportFilter::default()).await.unwrap();
    assert_eq!(all.len(), 4, "{all:#?}");
    assert_eq!(
        all[0].reported_user_id.as_deref(),
        Some("@someone:elsewhere.example")
    );
    let event_reports = source
        .list(&ReportFilter {
            kind: Some(ReportKind::Event),
            ..ReportFilter::default()
        })
        .await
        .unwrap();
    let event_report = &event_reports[0];
    assert_eq!(event_report.reporter_id, "@alice:example.org");
    assert_eq!(
        event_report.reported_user_id.as_deref(),
        Some("@mallory:example.org")
    );
    assert_eq!(event_report.score, Some(-100));
    assert_eq!(event_report.event, None, "lists do not carry the event");

    let shown = source.get(&event_report.id).await.unwrap().unwrap();
    let event = shown.event.expect("the moderator sees what was reported");
    assert_eq!(event["content"]["body"], "buy cheap watches");
    assert_eq!(event["sender"], "@mallory:example.org");
    assert_eq!(event["redacted"], false);

    // Redacted since: the moderator sees it redacted.
    scenario
        .send(
            Some("mallory"),
            Method::PUT,
            &format!("/rooms/{room_id}/redact/{spam_id}/r1"),
            Some(json!({})),
        )
        .await
        .assert_ok();
    let shown = source.get(&event_report.id).await.unwrap().unwrap();
    let event = shown.event.unwrap();
    assert_eq!(event["redacted"], true, "{event}");
    assert_eq!(event["content"], json!({}));

    let resolved = source
        .resolve(
            &event_report.id,
            &ReportResolve {
                resolution: ReportResolution::Redacted,
                note: None,
            },
            "@ops:example.org",
        )
        .await
        .unwrap();
    assert_eq!(resolved.status, ReportStatus::Resolved);
    assert_eq!(source.open_count().await.unwrap(), 3);
}
