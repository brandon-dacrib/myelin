use std::sync::Arc;

use axum::http::StatusCode;
use serde_json::json;

use super::*;
use crate::audit::{AuditFilter, AuditSink, InMemoryAuditSink};
use crate::handler_kit::testing::{call, state};
use crate::media::InMemoryMediaSource;
use crate::sources::InMemoryUserDirectory;
use crate::tasks::TaskRegistry;

const ALICE: &str = "@alice:example.org";
const ALICE_PATH: &str = "%40alice%3Aexample.org";

fn alice() -> AdminUser {
    AdminUser {
        user_id: ALICE.to_owned(),
        created_at: "2026-09-01T00:00:00.000Z".to_owned(),
        ..AdminUser::default()
    }
}

fn upload(id: &str, uploader: &str, size: u64, protected: bool) -> AdminMediaItem {
    AdminMediaItem {
        server_name: "example.org".into(),
        media_id: id.into(),
        origin: "local".into(),
        uploader: Some(uploader.into()),
        upload_name: None,
        content_type: Some("image/png".into()),
        size_bytes: size,
        created_at: format!("2026-09-2{}T00:00:00.000Z", id.len() % 10),
        last_accessed_at: None,
        quarantined: false,
        protected,
    }
}

struct Fixture {
    state: AdminState,
    audit: Arc<InMemoryAuditSink>,
    moderation: Arc<InMemoryUserModeration>,
    activity: Arc<InMemoryUserActivity>,
    tasks: Arc<TaskRegistry>,
}

fn fixture() -> Fixture {
    let (state, audit) = state();
    let moderation = Arc::new(InMemoryUserModeration::new().with_user(
        ALICE,
        vec![AdminSession {
            device_id: "PHONE".into(),
            ip: Some("192.0.2.1".into()),
            last_seen_at: Some("2026-09-27T10:00:00.000Z".into()),
            ..AdminSession::default()
        }],
    ));
    let activity = Arc::new(
        InMemoryUserActivity::new()
            .with_membership(AdminUserMembership {
                room_id: "!b:example.org".into(),
                user_id: ALICE.into(),
                membership: "leave".into(),
                ..AdminUserMembership::default()
            })
            .with_membership(AdminUserMembership {
                room_id: "!a:example.org".into(),
                room_name: Some("Lobby".into()),
                user_id: ALICE.into(),
                membership: "join".into(),
                ..AdminUserMembership::default()
            })
            .with_event(ALICE, "!a:example.org", "$1")
            .with_event(ALICE, "!a:example.org", "$2")
            .with_event(ALICE, "!b:example.org", "$3")
            .with_event("@bob:example.org", "!a:example.org", "$4"),
    );
    let tasks = TaskRegistry::in_memory();
    let media = InMemoryMediaSource::new()
        .with_item(upload("one", ALICE, 100, false))
        .with_item(upload("two", ALICE, 50, true))
        .with_item(upload("three", "@bob:example.org", 7, false));
    let state = state
        .with_users(Arc::new(InMemoryUserDirectory::new().with_user(alice())))
        .with_user_moderation(moderation.clone())
        .with_user_activity(activity.clone())
        .with_media(Arc::new(media))
        .with_tasks(tasks.clone());
    Fixture {
        state,
        audit,
        moderation,
        activity,
        tasks,
    }
}

async fn audited(audit: &InMemoryAuditSink, action: &str) -> Vec<crate::model::AuditEntry> {
    audit
        .query(&AuditFilter {
            action: Some(action.to_owned()),
            limit: 10,
            ..AuditFilter::default()
        })
        .await
        .unwrap()
}

async fn settled(tasks: &TaskRegistry, id: &str) -> crate::model::Task {
    for _ in 0..200 {
        let task = tasks.get(id).await.unwrap().unwrap();
        if crate::tasks::is_terminal(task.status) {
            return task;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    panic!("task {id} never finished");
}

#[tokio::test]
async fn suspend_and_unsuspend_flip_the_flag_with_audit_and_event() {
    let f = fixture();
    let mut events = f.state.events.subscribe();
    let path = format!("/api/v1/users/{ALICE_PATH}/suspend");
    // A reader cannot suspend.
    let (status, _, _) = call(&f.state, "POST", &path, Some("mod-read"), None, None).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _, _) = call(
        &f.state,
        "POST",
        &path,
        Some("admin"),
        Some(json!({"reason": "spam wave"})),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(f.moderation.is_suspended(ALICE));
    let entries = audited(&f.audit, "users.suspend").await;
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].changes[0].pointer, "/suspended");
    let event = events.recv().await.unwrap();
    assert_eq!(event.r#type, "user.suspended");
    assert_eq!(event.data["reason"], "spam wave");

    let (status, _, _) = call(
        &f.state,
        "POST",
        &format!("/api/v1/users/{ALICE_PATH}/unsuspend"),
        Some("admin"),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(!f.moderation.is_suspended(ALICE));
    assert_eq!(events.recv().await.unwrap().r#type, "user.unsuspended");

    // Suspension is a moderator's tool (RFC 0004 section 8): `moderation:write` alone suffices.
    let (status, _, _) = call(&f.state, "POST", &path, Some("mod-write"), None, None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(f.moderation.is_suspended(ALICE));
    let (status, _, _) = call(
        &f.state,
        "POST",
        &format!("/api/v1/users/{ALICE_PATH}/unsuspend"),
        Some("mod-write"),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(!f.moderation.is_suspended(ALICE));

    let (status, _, _) = call(
        &f.state,
        "POST",
        "/api/v1/users/%40nobody%3Aexample.org/suspend",
        Some("admin"),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn a_moderator_shadow_bans_and_lifts_it() {
    let f = fixture();
    let (status, _, _) = call(
        &f.state,
        "POST",
        &format!("/api/v1/users/{ALICE_PATH}/shadow-ban"),
        Some("mod-write"),
        None,
        Some("k1"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(f.moderation.is_shadow_banned(ALICE));
    // A replay with the same key is answered from the cache, and records nothing more.
    let (status, headers, _) = call(
        &f.state,
        "POST",
        &format!("/api/v1/users/{ALICE_PATH}/shadow-ban"),
        Some("mod-write"),
        None,
        Some("k1"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers["idempotency-replayed"], "true");
    assert_eq!(audited(&f.audit, "users.shadow_ban").await.len(), 1);
    let (status, _, _) = call(
        &f.state,
        "POST",
        &format!("/api/v1/users/{ALICE_PATH}/unshadow-ban"),
        Some("mod-write"),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(!f.moderation.is_shadow_banned(ALICE));
    assert_eq!(audited(&f.audit, "users.unshadow_ban").await.len(), 1);
}

#[tokio::test]
async fn a_rate_limit_override_is_set_read_and_cleared() {
    let f = fixture();
    let path = format!("/api/v1/users/{ALICE_PATH}/rate-limit");
    let (status, _, none) = call(&f.state, "GET", &path, Some("read"), None, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(none, json!({}));

    let (status, _, problem) = call(
        &f.state,
        "PUT",
        &path,
        Some("admin"),
        Some(json!({"burst_count": 3})),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(problem["errors"][0]["pointer"], "/messages_per_second");
    let (status, _, _) = call(
        &f.state,
        "PUT",
        &path,
        Some("admin"),
        Some(json!({"messages_per_second": 1, "burst_count": 0})),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    let (status, _, set) = call(
        &f.state,
        "PUT",
        &path,
        Some("admin"),
        Some(json!({"messages_per_second": 0.5})),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{set}");
    assert_eq!(set, json!({"messages_per_second": 0.5, "burst_count": 10}));
    let (_, _, read) = call(&f.state, "GET", &path, Some("read"), None, None).await;
    assert_eq!(read, set);
    assert_eq!(audited(&f.audit, "users.rate_limit.put").await.len(), 1);

    let (status, _, _) = call(&f.state, "DELETE", &path, Some("read"), None, None).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _, _) = call(&f.state, "DELETE", &path, Some("admin"), None, None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (_, _, read) = call(&f.state, "GET", &path, Some("read"), None, None).await;
    assert_eq!(read, json!({}));
    let entries = audited(&f.audit, "users.rate_limit.delete").await;
    assert_eq!(entries[0].changes[0].from, Some(set));
}

#[tokio::test]
async fn the_rate_limit_answer_carries_the_server_wide_limits_it_replaces() {
    let f = fixture();
    let config = crate::sources::InMemoryConfigSource::new()
        .with_file(
            "/etc/hs/config.yaml",
            json!({"server": {"server_name": "example.org"}}),
        )
        .with_database(json!({
            "rate_limits": {"message": {"per_second": 0.5}, "admin_redaction": {"burst_count": 40}}
        }));
    let state = f.state.clone().with_config(Arc::new(config));
    let path = format!("/api/v1/users/{ALICE_PATH}/rate-limit");
    let (status, _, read) = call(&state, "GET", &path, Some("read"), None, None).await;
    assert_eq!(status, StatusCode::OK, "{read}");
    // No override: no override fields, and the bucket written in part keeps its other default.
    assert_eq!(read.get("messages_per_second"), None);
    assert_eq!(
        read["server_wide"],
        json!({
            "enabled": true,
            "message": {"per_second": 0.5, "burst_count": 10},
            "admin_redaction": {"per_second": 1.0, "burst_count": 40},
        })
    );

    let (status, _, _) = call(
        &state,
        "PUT",
        &path,
        Some("admin"),
        Some(json!({"messages_per_second": 0})),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (_, _, read) = call(&state, "GET", &path, Some("read"), None, None).await;
    assert_eq!(read["messages_per_second"], json!(0.0));
    assert_eq!(read["burst_count"], 10);
    assert_eq!(read["server_wide"]["message"]["per_second"], json!(0.5));

    // Without a configuration source the override is still answered, and nothing else.
    let (_, _, bare) = call(&f.state, "GET", &path, Some("read"), None, None).await;
    assert_eq!(bare, json!({"messages_per_second": 0.0, "burst_count": 10}));
}

#[tokio::test]
async fn login_as_mints_a_support_session_and_never_records_the_token() {
    let f = fixture();
    let mut events = f.state.events.subscribe();
    let path = format!("/api/v1/users/{ALICE_PATH}/login-as");
    let (status, _, _) = call(&f.state, "POST", &path, Some("mod-write"), None, None).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _, _) = call(
        &f.state,
        "POST",
        &path,
        Some("admin"),
        Some(json!({"valid_for_seconds": 999_999})),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, _, token) = call(
        &f.state,
        "POST",
        &path,
        Some("admin"),
        Some(json!({"reason": "ticket 42"})),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{token}");
    let secret = token["access_token"].as_str().unwrap().to_owned();
    assert_eq!(token["user_id"], ALICE);

    let entries = audited(&f.audit, "users.login_as").await;
    assert_eq!(entries.len(), 1);
    let recorded = serde_json::to_string(&entries[0]).unwrap();
    assert!(
        !recorded.contains(&secret),
        "the audit entry holds the token"
    );
    let event = events.recv().await.unwrap();
    assert_eq!(event.r#type, "user.impersonated");
    assert_eq!(event.data["reason"], "ticket 42");
    assert!(!event.data.to_string().contains(&secret));

    // It shows among the user's sessions, as a support session.
    let (_, _, sessions) = call(
        &f.state,
        "GET",
        &format!("/api/v1/users/{ALICE_PATH}/sessions"),
        Some("read"),
        None,
        None,
    )
    .await;
    let items = sessions["items"].as_array().unwrap();
    assert_eq!(items.len(), 2);
    assert_eq!(items[1]["device_id"], token["device_id"]);
    assert_eq!(items[1]["support_session"], true);
}

#[tokio::test]
async fn login_as_refuses_a_deactivated_account() {
    let f = fixture();
    let state = f
        .state
        .with_users(Arc::new(InMemoryUserDirectory::new().with_user(
            AdminUser {
                deactivated: true,
                ..alice()
            },
        )));
    let (status, _, _) = call(
        &state,
        "POST",
        &format!("/api/v1/users/{ALICE_PATH}/login-as"),
        Some("admin"),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert!(audited(&f.audit, "users.login_as").await.is_empty());
}

#[tokio::test]
async fn memberships_statistics_and_media_read_what_the_sources_say() {
    let f = fixture();
    let (status, _, page) = call(
        &f.state,
        "GET",
        &format!("/api/v1/users/{ALICE_PATH}/memberships?include_total=true"),
        Some("read"),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(page["total"], 2);
    assert_eq!(page["items"][0]["membership"], "join", "joined rooms first");
    assert_eq!(page["items"][0]["room_name"], "Lobby");
    let (_, _, only_left) = call(
        &f.state,
        "GET",
        &format!("/api/v1/users/{ALICE_PATH}/memberships?membership=leave"),
        Some("read"),
        None,
        None,
    )
    .await;
    assert_eq!(only_left["items"].as_array().unwrap().len(), 1);
    let (status, _, _) = call(
        &f.state,
        "GET",
        &format!("/api/v1/users/{ALICE_PATH}/memberships?membership=maybe"),
        Some("read"),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    let (status, _, stats) = call(
        &f.state,
        "GET",
        &format!("/api/v1/users/{ALICE_PATH}/statistics"),
        Some("read"),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(stats["joins_count"], 1);
    assert_eq!(stats["events_sent_count"], 3);
    assert_eq!(stats["media_count"], 2);
    assert_eq!(stats["media_bytes"], 150);
    assert_eq!(stats["session_count"], 1);

    let (status, _, media) = call(
        &f.state,
        "GET",
        &format!("/api/v1/users/{ALICE_PATH}/media"),
        Some("read"),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let ids: Vec<&str> = media["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["media_id"].as_str().unwrap())
        .collect();
    assert_eq!(ids.len(), 2);
    assert!(!ids.contains(&"three"), "bob's upload is not alice's");

    let (status, _, _) = call(
        &f.state,
        "GET",
        "/api/v1/users/%40nobody%3Aexample.org/statistics",
        Some("read"),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn deleting_a_users_media_is_a_task_that_keeps_protected_items() {
    let f = fixture();
    let (status, headers, task) = call(
        &f.state,
        "DELETE",
        &format!("/api/v1/users/{ALICE_PATH}/media"),
        Some("mod-write"),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{task}");
    let id = task["id"].as_str().unwrap();
    assert_eq!(headers["location"], format!("/api/v1/tasks/{id}"));
    let done = settled(&f.tasks, id).await;
    let result = done.result.unwrap();
    assert_eq!(result["deleted"], 1);
    assert_eq!(result["bytes"], 100);
    assert_eq!(result["skipped_protected"], 1);
    assert_eq!(done.progress.unwrap().current, 2);
    assert_eq!(audited(&f.audit, "users.media.delete").await.len(), 1);
    let (_, _, left) = call(
        &f.state,
        "GET",
        &format!("/api/v1/users/{ALICE_PATH}/media"),
        Some("read"),
        None,
        None,
    )
    .await;
    assert_eq!(left["items"][0]["media_id"], "two");
}

#[tokio::test]
async fn redacting_a_users_events_is_a_task_with_progress() {
    let f = fixture();
    let path = format!("/api/v1/users/{ALICE_PATH}/redact-events");
    let (status, _, _) = call(
        &f.state,
        "POST",
        &path,
        Some("mod-write"),
        Some(json!({"limit": 0})),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, _, task) = call(
        &f.state,
        "POST",
        &path,
        Some("mod-write"),
        Some(json!({"room_id": "!a:example.org", "reason": "spam"})),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{task}");
    assert_eq!(task["action"], "user.redact_events");
    let done = settled(&f.tasks, task["id"].as_str().unwrap()).await;
    let result = done.result.unwrap();
    assert_eq!(result["redacted"], 2);
    assert_eq!(result["total"], 2);
    let progress = done.progress.unwrap();
    assert_eq!((progress.current, progress.total), (2, Some(2)));
    let redacted: Vec<String> = f
        .activity
        .redacted()
        .into_iter()
        .map(|t| t.event_id)
        .collect();
    assert_eq!(redacted, vec!["$2", "$1"], "newest first, bob's untouched");
    assert_eq!(audited(&f.audit, "users.redact_events").await.len(), 1);
}

#[tokio::test]
async fn unwired_sources_answer_503() {
    let (state, _) = state();
    let state = state.with_users(Arc::new(InMemoryUserDirectory::new().with_user(alice())));
    for (method, suffix) in [
        ("POST", "suspend"),
        ("GET", "rate-limit"),
        ("GET", "sessions"),
        ("GET", "memberships"),
        ("POST", "redact-events"),
    ] {
        let (status, _, _) = call(
            &state,
            method,
            &format!("/api/v1/users/{ALICE_PATH}/{suffix}"),
            Some("admin"),
            None,
            None,
        )
        .await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{suffix}");
    }
}
