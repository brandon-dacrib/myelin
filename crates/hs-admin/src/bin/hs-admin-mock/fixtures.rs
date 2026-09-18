//! Realistic fixture data for every resource in `openapi/openapi.yaml`, seeded once at startup.
//! Everything here is invented (no real user data); the shapes match the resource schemas in the
//! OpenAPI document closely enough for track 16 to build real UI against.

use std::collections::HashMap;

use serde_json::{Value, json};

pub fn seed() -> HashMap<String, Vec<Value>> {
    let mut db = HashMap::new();
    db.insert("users".to_string(), users());
    db.insert("rooms".to_string(), rooms());
    db.insert("media".to_string(), media());
    db.insert("destinations".to_string(), destinations());
    db.insert("server_keys".to_string(), server_keys());
    db.insert("reports".to_string(), reports());
    db.insert("registration_tokens".to_string(), registration_tokens());
    db.insert("tasks".to_string(), tasks());
    db.insert("appservices".to_string(), appservices());
    db.insert("bridge_types".to_string(), bridge_types());
    db.insert("replicas".to_string(), replicas());
    db.insert("shards".to_string(), shards());
    db.insert("server_notices".to_string(), server_notices());
    db.insert("config_sections".to_string(), config_sections());
    db
}

fn users() -> Vec<Value> {
    vec![
        json!({
            "user_id": "@ops:example.org", "display_name": "Operations", "avatar_url": null,
            "admin": true, "deactivated": false, "erased": false, "locked": false,
            "suspended": false, "shadow_banned": false, "user_type": null, "consent_version": "1.0",
            "appservice_id": null, "created_at": "2026-01-05T09:00:00.000Z",
            "last_seen_at": "2026-09-18T08:12:00.000Z", "device_count": 2, "room_count": 14, "media_count": 3
        }),
        json!({
            "user_id": "@alice:example.org", "display_name": "Alice", "avatar_url": "mxc://example.org/alice",
            "admin": false, "deactivated": false, "erased": false, "locked": false,
            "suspended": false, "shadow_banned": false, "user_type": null, "consent_version": "1.0",
            "appservice_id": null, "created_at": "2026-02-11T14:23:00.000Z",
            "last_seen_at": "2026-09-18T07:55:00.000Z", "device_count": 3, "room_count": 9, "media_count": 12
        }),
        json!({
            "user_id": "@bob:example.org", "display_name": "Bob", "avatar_url": null,
            "admin": false, "deactivated": false, "erased": false, "locked": false,
            "suspended": false, "shadow_banned": false, "user_type": null, "consent_version": null,
            "appservice_id": null, "created_at": "2026-03-02T10:00:00.000Z",
            "last_seen_at": "2026-09-17T21:40:00.000Z", "device_count": 1, "room_count": 4, "media_count": 0
        }),
        json!({
            "user_id": "@mallory:example.org", "display_name": "Mallory", "avatar_url": null,
            "admin": false, "deactivated": false, "erased": false, "locked": false,
            "suspended": false, "shadow_banned": false, "user_type": null, "consent_version": "1.0",
            "appservice_id": null, "created_at": "2026-06-19T18:00:00.000Z",
            "last_seen_at": "2026-09-16T03:12:00.000Z", "device_count": 5, "room_count": 22, "media_count": 41
        }),
        json!({
            "user_id": "@telegram_12345:example.org", "display_name": "Telegram Bridge User", "avatar_url": null,
            "admin": false, "deactivated": false, "erased": false, "locked": false,
            "suspended": false, "shadow_banned": false, "user_type": "bot", "consent_version": null,
            "appservice_id": "telegram", "created_at": "2026-04-01T00:00:00.000Z",
            "last_seen_at": "2026-09-18T08:00:00.000Z", "device_count": 1, "room_count": 30, "media_count": 5
        }),
    ]
}

fn rooms() -> Vec<Value> {
    vec![
        json!({
            "room_id": "!general:example.org", "name": "General", "topic": "Company-wide chat",
            "avatar_url": null, "canonical_alias": "#general:example.org",
            "joined_members_count": 84, "local_members_count": 71, "state_events_count": 512,
            "version": "11", "creator": "@ops:example.org", "encrypted": false, "join_rule": "public",
            "guest_access": "forbidden", "history_visibility": "shared", "federatable": true,
            "public": true, "room_type": null, "blocked": false, "blocked_reason": null,
            "tombstoned": false, "replacement_room_id": null, "forgotten": false
        }),
        json!({
            "room_id": "!security:example.org", "name": "Security incidents", "topic": null,
            "avatar_url": null, "canonical_alias": "#security:example.org",
            "joined_members_count": 6, "local_members_count": 6, "state_events_count": 88,
            "version": "11", "creator": "@ops:example.org", "encrypted": true, "join_rule": "invite",
            "guest_access": "forbidden", "history_visibility": "invited", "federatable": false,
            "public": false, "room_type": null, "blocked": false, "blocked_reason": null,
            "tombstoned": false, "replacement_room_id": null, "forgotten": false
        }),
        json!({
            "room_id": "!spam-central:evil.example", "name": "Free Crypto Giveaway!!!", "topic": null,
            "avatar_url": null, "canonical_alias": null,
            "joined_members_count": 3, "local_members_count": 1, "state_events_count": 20,
            "version": "10", "creator": "@mallory:example.org", "encrypted": false, "join_rule": "public",
            "guest_access": "can_join", "history_visibility": "world_readable", "federatable": true,
            "public": true, "room_type": null, "blocked": false, "blocked_reason": null,
            "tombstoned": false, "replacement_room_id": null, "forgotten": false
        }),
        json!({
            "room_id": "!archived:example.org", "name": "Old project room", "topic": "Archived 2025",
            "avatar_url": null, "canonical_alias": "#archived:example.org",
            "joined_members_count": 0, "local_members_count": 0, "state_events_count": 340,
            "version": "9", "creator": "@bob:example.org", "encrypted": false, "join_rule": "invite",
            "guest_access": "forbidden", "history_visibility": "shared", "federatable": true,
            "public": false, "room_type": null, "blocked": false, "blocked_reason": null,
            "tombstoned": true, "replacement_room_id": "!general:example.org", "forgotten": true
        }),
    ]
}

fn media() -> Vec<Value> {
    vec![
        json!({"server_name": "example.org", "media_id": "abc123", "origin": "local", "uploader": "@alice:example.org", "upload_name": "vacation.jpg", "content_type": "image/jpeg", "size_bytes": 2_400_000, "created_at": "2026-08-01T10:00:00.000Z", "quarantined": false, "protected": false}),
        json!({"server_name": "example.org", "media_id": "def456", "origin": "local", "uploader": "@mallory:example.org", "upload_name": "definitely-not-malware.exe", "content_type": "application/octet-stream", "size_bytes": 900_000, "created_at": "2026-09-10T02:00:00.000Z", "quarantined": true, "protected": false}),
        json!({"server_name": "matrix.org", "media_id": "ghi789", "origin": "remote", "uploader": null, "upload_name": "avatar.png", "content_type": "image/png", "size_bytes": 40_000, "created_at": "2026-07-15T12:00:00.000Z", "quarantined": false, "protected": true}),
        json!({"server_name": "example.org", "media_id": "jkl012", "origin": "local", "uploader": "@ops:example.org", "upload_name": "server-notice-banner.png", "content_type": "image/png", "size_bytes": 12_000, "created_at": "2026-01-06T09:00:00.000Z", "quarantined": false, "protected": true}),
    ]
}

fn destinations() -> Vec<Value> {
    vec![
        json!({"server_name": "matrix.org", "last_successful_at": "2026-09-18T08:10:00.000Z", "failing_since": null, "retry_last_at": null, "retry_interval_ms": null, "pending_pdu_count": 0, "pending_edu_count": 0}),
        json!({"server_name": "flaky.example", "last_successful_at": "2026-09-15T02:00:00.000Z", "failing_since": "2026-09-15T02:05:00.000Z", "retry_last_at": "2026-09-18T07:00:00.000Z", "retry_interval_ms": 3_600_000, "pending_pdu_count": 214, "pending_edu_count": 6}),
        json!({"server_name": "gone.example", "last_successful_at": "2026-06-01T00:00:00.000Z", "failing_since": "2026-06-01T00:10:00.000Z", "retry_last_at": "2026-09-17T00:00:00.000Z", "retry_interval_ms": 86_400_000, "pending_pdu_count": 5012, "pending_edu_count": 0}),
    ]
}

fn server_keys() -> Vec<Value> {
    vec![
        json!({"key_id": "ed25519:auto1", "algorithm": "ed25519", "public_key": "MOCKKEYbase64==", "valid_until_at": "2026-12-01T00:00:00.000Z", "old": false}),
    ]
}

fn reports() -> Vec<Value> {
    vec![
        json!({"id": "01J8RPT0000000000000000A", "kind": "event", "status": "open", "room_id": "!spam-central:evil.example", "event_id": "$spamevent1", "reporter_id": "@alice:example.org", "reported_user_id": "@mallory:example.org", "reason": "Unsolicited crypto spam", "score": -80, "received_at": "2026-09-17T11:00:00.000Z", "resolution": null, "resolution_note": null}),
        json!({"id": "01J8RPT0000000000000000B", "kind": "user", "status": "resolved", "room_id": null, "event_id": null, "reporter_id": "@bob:example.org", "reported_user_id": "@mallory:example.org", "reason": "Repeated harassment in DMs", "score": null, "received_at": "2026-09-10T09:30:00.000Z", "resolution": "warned", "resolution_note": "First warning issued."}),
        json!({"id": "01J8RPT0000000000000000C", "kind": "event", "status": "open", "room_id": "!general:example.org", "event_id": "$rudeevent1", "reporter_id": "@ops:example.org", "reported_user_id": "@bob:example.org", "reason": "Off-topic advertising", "score": -10, "received_at": "2026-09-18T06:45:00.000Z", "resolution": null, "resolution_note": null}),
    ]
}

fn registration_tokens() -> Vec<Value> {
    vec![
        json!({"token": "welcome-2026", "uses_allowed": 100, "pending": 4, "completed": 61, "expires_at": "2026-12-31T23:59:59.000Z", "created_at": "2026-01-01T00:00:00.000Z"}),
        json!({"token": "eng-team-onboarding", "uses_allowed": 10, "pending": 0, "completed": 10, "expires_at": null, "created_at": "2026-05-01T00:00:00.000Z"}),
    ]
}

fn tasks() -> Vec<Value> {
    vec![
        json!({"id": "01J8RTASK000000000000001", "action": "media.purge_remote_cache", "status": "succeeded", "resource": null, "progress": null, "result": {"purged_count": 812}, "error": null, "created_at": "2026-09-17T03:00:00.000Z", "started_at": "2026-09-17T03:00:01.000Z", "finished_at": "2026-09-17T03:04:22.000Z", "scheduled_for": null, "created_by": {"kind": "system", "id": "system"}}),
        json!({"id": "01J8RTASK000000000000002", "action": "room.purge_history", "status": "running", "resource": {"type": "room", "id": "!archived:example.org"}, "progress": {"current": 3400, "total": 12000, "unit": "events", "message": "Purging events before cutoff"}, "result": null, "error": null, "created_at": "2026-09-18T07:00:00.000Z", "started_at": "2026-09-18T07:00:02.000Z", "finished_at": null, "scheduled_for": null, "created_by": {"kind": "user", "id": "@ops:example.org"}}),
        json!({"id": "01J8RTASK000000000000003", "action": "appservice.replay", "status": "scheduled", "resource": {"type": "appservice", "id": "telegram"}, "progress": null, "result": null, "error": null, "created_at": "2026-09-18T08:11:00.000Z", "started_at": null, "finished_at": null, "scheduled_for": null, "created_by": {"kind": "user", "id": "@ops:example.org"}}),
    ]
}

fn appservices() -> Vec<Value> {
    vec![
        json!({
            "id": "telegram", "sender_localpart": "telegrambot", "url": "http://telegram-bridge.internal:29317",
            "namespaces": {"users": [{"regex": "@telegram_.*:example.org", "exclusive": true}]},
            "rate_limited": false, "protocols": ["telegram"], "paused": false, "health": "healthy",
            "created_at": "2026-04-01T00:00:00.000Z", "links": {"login_url": "http://telegram-bridge.internal:29317/login"}
        }),
        json!({
            "id": "whatsapp", "sender_localpart": "whatsappbot", "url": "http://whatsapp-bridge.internal:29318",
            "namespaces": {"users": [{"regex": "@whatsapp_.*:example.org", "exclusive": true}]},
            "rate_limited": false, "protocols": ["whatsapp"], "paused": true, "health": "paused",
            "created_at": "2026-07-20T00:00:00.000Z", "links": {"login_url": null}
        }),
    ]
}

fn bridge_types() -> Vec<Value> {
    vec![
        json!({"id": "telegram", "name": "Telegram", "upstream_project": "mautrix-telegram", "image": "dock.mau.dev/mautrix/telegram:latest", "default_namespaces": {"users": "@telegram_.*"}, "config_keys": [{"key": "api_id", "description": "Telegram API id", "required": true}, {"key": "api_hash", "description": "Telegram API hash", "required": true}], "supports_double_puppeting": true, "required_features": []}),
        json!({"id": "whatsapp", "name": "WhatsApp", "upstream_project": "mautrix-whatsapp", "image": "dock.mau.dev/mautrix/whatsapp:latest", "default_namespaces": {"users": "@whatsapp_.*"}, "config_keys": [], "supports_double_puppeting": true, "required_features": []}),
        json!({"id": "discord", "name": "Discord", "upstream_project": "mautrix-discord", "image": "dock.mau.dev/mautrix/discord:latest", "default_namespaces": {"users": "@discord_.*"}, "config_keys": [{"key": "bot_token", "description": "Discord bot token", "required": false}], "supports_double_puppeting": false, "required_features": ["appservice.login"]}),
    ]
}

fn replicas() -> Vec<Value> {
    vec![
        json!({"id": "replica-a", "role": "leader", "status": "active", "shard_count": 128, "epoch": 42}),
        json!({"id": "replica-b", "role": "follower", "status": "active", "shard_count": 120, "epoch": 42}),
        json!({"id": "replica-c", "role": "follower", "status": "draining", "shard_count": 8, "epoch": 42}),
    ]
}

fn shards() -> Vec<Value> {
    vec![
        json!({"kind": "room", "id": "shard-001", "owner": "replica-a", "state": "active"}),
        json!({"kind": "room", "id": "shard-002", "owner": "replica-b", "state": "active"}),
        json!({"kind": "user", "id": "shard-101", "owner": "replica-a", "state": "active"}),
        json!({"kind": "user", "id": "shard-102", "owner": "replica-c", "state": "migrating"}),
    ]
}

fn server_notices() -> Vec<Value> {
    vec![
        json!({"event_ids": ["$notice1"], "recipients": ["@alice:example.org", "@bob:example.org"], "sent_at": "2026-09-01T09:00:00.000Z"}),
    ]
}

fn config_sections() -> Vec<Value> {
    vec![
        json!({"name": "rate_limits", "reloadable": true, "source": "/etc/hs/config.yaml", "last_reloaded_at": "2026-09-10T00:00:00.000Z", "values": {"messages_per_second": 10, "burst_count": 50}}),
        json!({"name": "registration", "reloadable": true, "source": "/etc/hs/config.yaml", "last_reloaded_at": "2026-09-10T00:00:00.000Z", "values": {"enable_registration": true, "require_token": true}}),
        json!({"name": "database", "reloadable": false, "source": "/etc/hs/config.yaml", "last_reloaded_at": null, "values": {"host": "db.internal", "port": 5432, "password": {"$secret": true}}}),
    ]
}

pub fn migration_status() -> Value {
    json!({
        "status": "idle", "source": null,
        "streams": [], "estimated_remaining_ms": null, "errors": []
    })
}

pub fn statistics_overview() -> Value {
    json!({
        "users_count": 5, "rooms_count": 4, "media_count": 4, "media_bytes": 3_352_000,
        "daily_active_users": 3, "monthly_active_users": 5,
        "federation_destinations_failing_count": 2, "pending_reports_count": 2
    })
}

pub fn cluster_status() -> Value {
    json!({"mode": "clustered", "epoch": 42, "replica_count": 3, "shard_count": 256})
}
