//! A behavioral test suite generic over `S: AuthStore`, run against both
//! [`super::memory::InMemoryAuthStore`] and [`super::tables::TablesAuthStore`] (over
//! `hs_kv::memory::MemoryBackend`) so the two implementations cannot silently drift apart. Each
//! function here is a plain `async fn`, not itself a `#[tokio::test]` — the two store modules'
//! `tests` submodules each provide a one-line `#[tokio::test]` wrapper per function, so a test
//! failure is reported against a name that says which store it was ("...::memory::tests::..." vs
//! "...::tables::tests::...").
//!
//! This is the port of the test bodies that originally lived only in `memory.rs` (session 2 and
//! earlier), parameterized over the store trait instead of the concrete `InMemoryAuthStore` type.

use ruma::{device_id, user_id};

use super::{AccessTokenRecord, AuthStore, DeviceRecord, LoginTokenRecord, StoreError, UserRecord};
use crate::token::TokenHash;

pub(crate) async fn create_user_then_get_round_trips<S: AuthStore>(s: &S) {
    let uid = user_id!("@alice:example.org").to_owned();
    s.create_user(UserRecord::new(uid.clone(), 1000))
        .await
        .unwrap();
    let got = s.get_user(&uid).await.unwrap().unwrap();
    assert_eq!(got.user_id, uid);
    assert!(!got.is_admin);
}

pub(crate) async fn create_user_conflict_is_case_insensitive<S: AuthStore>(s: &S) {
    s.create_user(UserRecord::new(
        user_id!("@Alice:example.org").to_owned(),
        1,
    ))
    .await
    .unwrap();
    let err = s
        .create_user(UserRecord::new(
            user_id!("@alice:example.org").to_owned(),
            2,
        ))
        .await;
    assert!(matches!(err, Err(StoreError::Conflict(_))));
}

pub(crate) async fn create_user_exact_duplicate_is_conflict<S: AuthStore>(s: &S) {
    let uid = user_id!("@dupe:example.org").to_owned();
    s.create_user(UserRecord::new(uid.clone(), 1))
        .await
        .unwrap();
    let err = s.create_user(UserRecord::new(uid, 2)).await;
    assert!(matches!(err, Err(StoreError::Conflict(_))));
}

pub(crate) async fn list_users_is_empty_for_a_fresh_store<S: AuthStore>(s: &S) {
    assert!(s.list_users().await.unwrap().is_empty());
}

pub(crate) async fn list_users_is_sorted_by_user_id<S: AuthStore>(s: &S) {
    s.create_user(UserRecord::new(user_id!("@zeta:example.org").to_owned(), 3))
        .await
        .unwrap();
    s.create_user(UserRecord::new(
        user_id!("@alice:example.org").to_owned(),
        1,
    ))
    .await
    .unwrap();
    s.create_user(UserRecord::new(
        user_id!("@mallory:example.org").to_owned(),
        2,
    ))
    .await
    .unwrap();

    let users = s.list_users().await.unwrap();
    let ids: Vec<&str> = users.iter().map(|u| u.user_id.as_str()).collect();
    assert_eq!(
        ids,
        vec![
            "@alice:example.org",
            "@mallory:example.org",
            "@zeta:example.org",
        ]
    );
}

pub(crate) async fn is_localpart_available_reflects_existing_users<S: AuthStore>(s: &S) {
    assert!(s.is_localpart_available("alice").await.unwrap());
    s.create_user(UserRecord::new(
        user_id!("@alice:example.org").to_owned(),
        1,
    ))
    .await
    .unwrap();
    assert!(!s.is_localpart_available("alice").await.unwrap());
    assert!(!s.is_localpart_available("ALICE").await.unwrap());
}

pub(crate) async fn user_flag_setters_round_trip<S: AuthStore>(s: &S) {
    let uid = user_id!("@flags:example.org").to_owned();
    s.create_user(UserRecord::new(uid.clone(), 1))
        .await
        .unwrap();

    s.set_password_hash(&uid, Some("hash".to_string()))
        .await
        .unwrap();
    s.set_admin(&uid, true).await.unwrap();
    s.set_locked(&uid, true).await.unwrap();
    s.set_suspended(&uid, true).await.unwrap();
    s.set_deactivated(&uid, true).await.unwrap();

    let got = s.get_user(&uid).await.unwrap().unwrap();
    assert_eq!(got.password_hash.as_deref(), Some("hash"));
    assert!(got.is_admin);
    assert!(got.locked);
    assert!(got.suspended);
    assert!(got.deactivated);
}

pub(crate) async fn set_password_hash_on_missing_user_is_not_found<S: AuthStore>(s: &S) {
    let err = s
        .set_password_hash(user_id!("@ghost:example.org"), Some("x".to_string()))
        .await;
    assert!(matches!(err, Err(StoreError::NotFound(_))));
}

/// `UserStore::set_profile_display_name`/`set_profile_avatar_url` round-trip through
/// `get_user`, and are unset (`None`) on a freshly created account -- distinct from
/// `DeviceStore::set_display_name`, which this test does not touch.
pub(crate) async fn profile_fields_round_trip<S: AuthStore>(s: &S) {
    let uid = user_id!("@profile:example.org").to_owned();
    s.create_user(UserRecord::new(uid.clone(), 1))
        .await
        .unwrap();

    let fresh = s.get_user(&uid).await.unwrap().unwrap();
    assert_eq!(fresh.display_name, None);
    assert_eq!(fresh.avatar_url, None);

    s.set_profile_display_name(&uid, Some("Alice".to_string()))
        .await
        .unwrap();
    s.set_profile_avatar_url(&uid, Some("mxc://example.org/abc".to_string()))
        .await
        .unwrap();

    let got = s.get_user(&uid).await.unwrap().unwrap();
    assert_eq!(got.display_name.as_deref(), Some("Alice"));
    assert_eq!(got.avatar_url.as_deref(), Some("mxc://example.org/abc"));

    // Clearing sets the field back to `None`, it does not error.
    s.set_profile_display_name(&uid, None).await.unwrap();
    let cleared = s.get_user(&uid).await.unwrap().unwrap();
    assert_eq!(cleared.display_name, None);
    assert_eq!(cleared.avatar_url.as_deref(), Some("mxc://example.org/abc"));
}

pub(crate) async fn set_profile_fields_on_missing_user_is_not_found<S: AuthStore>(s: &S) {
    let err = s
        .set_profile_display_name(user_id!("@ghost:example.org"), Some("x".to_string()))
        .await;
    assert!(matches!(err, Err(StoreError::NotFound(_))));
    let err = s
        .set_profile_avatar_url(
            user_id!("@ghost:example.org"),
            Some("mxc://x/y".to_string()),
        )
        .await;
    assert!(matches!(err, Err(StoreError::NotFound(_))));
}

pub(crate) async fn device_and_token_lifecycle<S: AuthStore>(s: &S) {
    let uid = user_id!("@bob:example.org").to_owned();
    let did = device_id!("DEV1").to_owned();
    s.upsert_device(DeviceRecord {
        user_id: uid.clone(),
        device_id: did.clone(),
        display_name: Some("phone".to_string()),
        last_seen_ms: None,
        last_seen_ip: None,
    })
    .await
    .unwrap();
    assert_eq!(s.list_devices(&uid).await.unwrap().len(), 1);

    let hash = TokenHash::of("syt_whatever");
    s.put_access_token(AccessTokenRecord {
        hash,
        user_id: uid.clone(),
        device_id: Some(did.clone()),
        expires_at_ms: None,
        refresh_token_hash: None,
        last_used_ms: None,
    })
    .await
    .unwrap();
    assert!(s.get_access_token(&hash).await.unwrap().is_some());

    s.delete_access_tokens_for_device(&uid, &did).await.unwrap();
    assert!(s.get_access_token(&hash).await.unwrap().is_none());

    s.delete_device(&uid, &did).await.unwrap();
    assert!(s.list_devices(&uid).await.unwrap().is_empty());
}

pub(crate) async fn device_display_name_and_seen_round_trip<S: AuthStore>(s: &S) {
    let uid = user_id!("@carla:example.org").to_owned();
    let did = device_id!("DEVX").to_owned();
    s.upsert_device(DeviceRecord {
        user_id: uid.clone(),
        device_id: did.clone(),
        display_name: None,
        last_seen_ms: None,
        last_seen_ip: None,
    })
    .await
    .unwrap();

    s.set_display_name(&uid, &did, Some("laptop".to_string()))
        .await
        .unwrap();
    let device = s.get_device(&uid, &did).await.unwrap().unwrap();
    assert_eq!(device.display_name.as_deref(), Some("laptop"));

    s.record_seen(&uid, &did, 500, Some("1.2.3.4".to_string()))
        .await
        .unwrap();
    let device = s.get_device(&uid, &did).await.unwrap().unwrap();
    assert_eq!(device.last_seen_ms, Some(500));
    assert_eq!(device.last_seen_ip.as_deref(), Some("1.2.3.4"));
}

pub(crate) async fn set_display_name_on_missing_device_is_not_found<S: AuthStore>(s: &S) {
    let uid = user_id!("@nodev:example.org").to_owned();
    let did = device_id!("NOPE").to_owned();
    let err = s.set_display_name(&uid, &did, None).await;
    assert!(matches!(err, Err(StoreError::NotFound(_))));
}

pub(crate) async fn list_devices_is_sorted_by_device_id<S: AuthStore>(s: &S) {
    let uid = user_id!("@sorter:example.org").to_owned();
    for did in ["ZZZ", "AAA", "MMM"] {
        s.upsert_device(DeviceRecord {
            user_id: uid.clone(),
            device_id: ruma::DeviceId::new(),
            display_name: Some(did.to_string()),
            last_seen_ms: None,
            last_seen_ip: None,
        })
        .await
        .unwrap();
    }
    // Use fixed ids instead of random ones so ordering is deterministic.
    for did in ["c", "a", "b"] {
        let device_id: ruma::OwnedDeviceId = did.into();
        s.upsert_device(DeviceRecord {
            user_id: uid.clone(),
            device_id,
            display_name: None,
            last_seen_ms: None,
            last_seen_ip: None,
        })
        .await
        .unwrap();
    }
    let devices = s.list_devices(&uid).await.unwrap();
    let fixed: Vec<&str> = devices
        .iter()
        .map(|d| d.device_id.as_str())
        .filter(|id| *id == "a" || *id == "b" || *id == "c")
        .collect();
    assert_eq!(fixed, vec!["a", "b", "c"]);
}

pub(crate) async fn devices_do_not_leak_across_users<S: AuthStore>(s: &S) {
    let alice = user_id!("@alice2:example.org").to_owned();
    let bob = user_id!("@bob2:example.org").to_owned();
    for uid in [&alice, &bob] {
        s.upsert_device(DeviceRecord {
            user_id: uid.clone(),
            device_id: device_id!("SHARED").to_owned(),
            display_name: None,
            last_seen_ms: None,
            last_seen_ip: None,
        })
        .await
        .unwrap();
    }
    assert_eq!(s.list_devices(&alice).await.unwrap().len(), 1);
    assert_eq!(s.list_devices(&bob).await.unwrap().len(), 1);
    s.delete_device(&alice, device_id!("SHARED")).await.unwrap();
    assert!(s.list_devices(&alice).await.unwrap().is_empty());
    assert_eq!(s.list_devices(&bob).await.unwrap().len(), 1);
}

pub(crate) async fn access_token_bulk_deletes<S: AuthStore>(s: &S) {
    let uid = user_id!("@bulk:example.org").to_owned();
    let other = user_id!("@other:example.org").to_owned();
    let h1 = TokenHash::of("syt_bulk_1");
    let h2 = TokenHash::of("syt_bulk_2");
    let h3 = TokenHash::of("syt_bulk_other");

    for (hash, user_id) in [(h1, &uid), (h2, &uid), (h3, &other)] {
        s.put_access_token(AccessTokenRecord {
            hash,
            user_id: user_id.clone(),
            device_id: None,
            expires_at_ms: None,
            refresh_token_hash: None,
            last_used_ms: None,
        })
        .await
        .unwrap();
    }

    let removed = s
        .delete_other_access_tokens_for_user(&uid, &h1)
        .await
        .unwrap();
    assert_eq!(removed, 1);
    assert!(s.get_access_token(&h1).await.unwrap().is_some());
    assert!(s.get_access_token(&h2).await.unwrap().is_none());
    assert!(s.get_access_token(&h3).await.unwrap().is_some());

    let removed = s.delete_all_access_tokens_for_user(&uid).await.unwrap();
    assert_eq!(removed, 1);
    assert!(s.get_access_token(&h1).await.unwrap().is_none());
    // A different user's token must be untouched by another user's bulk delete.
    assert!(s.get_access_token(&h3).await.unwrap().is_some());
}

pub(crate) async fn mark_access_token_used_updates_last_used<S: AuthStore>(s: &S) {
    let uid = user_id!("@used:example.org").to_owned();
    let hash = TokenHash::of("syt_used");
    s.put_access_token(AccessTokenRecord {
        hash,
        user_id: uid,
        device_id: None,
        expires_at_ms: None,
        refresh_token_hash: None,
        last_used_ms: None,
    })
    .await
    .unwrap();
    s.mark_access_token_used(&hash, 12345).await.unwrap();
    let rec = s.get_access_token(&hash).await.unwrap().unwrap();
    assert_eq!(rec.last_used_ms, Some(12345));
}

pub(crate) async fn mark_access_token_used_on_missing_token_is_not_an_error<S: AuthStore>(s: &S) {
    let hash = TokenHash::of("syt_never_existed");
    s.mark_access_token_used(&hash, 1).await.unwrap();
}

pub(crate) async fn refresh_token_lifecycle<S: AuthStore>(s: &S) {
    let uid = user_id!("@refresh:example.org").to_owned();
    let did = device_id!("REFDEV").to_owned();
    let access_hash = TokenHash::of("syt_for_refresh");
    let hash = TokenHash::of("syr_one");
    s.put_refresh_token(super::RefreshTokenRecord {
        hash,
        user_id: uid.clone(),
        device_id: did,
        access_token_hash: access_hash,
        used: false,
        replaced_by: None,
        expires_at_ms: None,
        ultimate_session_expiry_ms: None,
    })
    .await
    .unwrap();

    let replacement = TokenHash::of("syr_two");
    s.mark_refresh_token_used(&hash, replacement).await.unwrap();
    let rec = s.get_refresh_token(&hash).await.unwrap().unwrap();
    assert!(rec.used);
    assert_eq!(rec.replaced_by, Some(replacement));

    let removed = s.delete_all_refresh_tokens_for_user(&uid).await.unwrap();
    assert_eq!(removed, 1);
    assert!(s.get_refresh_token(&hash).await.unwrap().is_none());
}

pub(crate) async fn mark_refresh_token_used_on_missing_is_not_found<S: AuthStore>(s: &S) {
    let hash = TokenHash::of("syr_never_existed");
    let err = s
        .mark_refresh_token_used(&hash, TokenHash::of("syr_x"))
        .await;
    assert!(matches!(err, Err(StoreError::NotFound(_))));
}

pub(crate) async fn delete_refresh_token_is_idempotent<S: AuthStore>(s: &S) {
    let hash = TokenHash::of("syr_delete_me");
    s.delete_refresh_token(&hash).await.unwrap();
    s.delete_refresh_token(&hash).await.unwrap();
}

pub(crate) async fn login_token_is_single_use<S: AuthStore>(s: &S) {
    let uid = user_id!("@carol:example.org").to_owned();
    let hash = TokenHash::of("syl_whatever");
    s.put_login_token(LoginTokenRecord {
        hash,
        user_id: uid,
        expires_at_ms: 10_000,
        used: false,
    })
    .await
    .unwrap();
    let first = s.consume_login_token(&hash, 1_000).await.unwrap();
    assert!(first.is_some());
    let second = s.consume_login_token(&hash, 1_000).await.unwrap();
    assert!(second.is_none());
}

pub(crate) async fn login_token_expiry_is_enforced<S: AuthStore>(s: &S) {
    let uid = user_id!("@dave:example.org").to_owned();
    let hash = TokenHash::of("syl_whatever2");
    s.put_login_token(LoginTokenRecord {
        hash,
        user_id: uid,
        expires_at_ms: 1_000,
        used: false,
    })
    .await
    .unwrap();
    let result = s.consume_login_token(&hash, 5_000).await.unwrap();
    assert!(result.is_none());
}

pub(crate) async fn consume_login_token_missing_returns_none<S: AuthStore>(s: &S) {
    let hash = TokenHash::of("syl_never_existed");
    let result = s.consume_login_token(&hash, 1_000).await.unwrap();
    assert!(result.is_none());
}

pub(crate) async fn threepid_lookup_is_case_insensitive_on_address<S: AuthStore>(s: &S) {
    let uid = user_id!("@eve:example.org").to_owned();
    s.bind_threepid(&uid, "email", "Eve@Example.Org")
        .await
        .unwrap();
    assert_eq!(
        s.get_user_by_threepid("email", "eve@example.org")
            .await
            .unwrap(),
        Some(uid)
    );
    assert!(
        s.get_user_by_threepid("msisdn", "eve@example.org")
            .await
            .unwrap()
            .is_none()
    );
}

pub(crate) async fn uia_session_tracks_completed_stages_and_data<S: AuthStore>(s: &S) {
    let id = s.create_session(0).await.unwrap();
    assert!(s.session_exists(&id, 100, 10_000).await.unwrap());
    assert!(!s.session_exists(&id, 20_000, 10_000).await.unwrap());

    s.mark_stage_complete(&id, "m.login.dummy").await.unwrap();
    s.mark_stage_complete(&id, "m.login.dummy").await.unwrap(); // idempotent
    assert_eq!(
        s.completed_stages(&id).await.unwrap(),
        vec!["m.login.dummy".to_string()]
    );

    s.set_session_data(&id, "username", serde_json::json!("alice"))
        .await
        .unwrap();
    assert_eq!(
        s.get_session_data(&id, "username").await.unwrap(),
        Some(serde_json::json!("alice"))
    );
}

pub(crate) async fn uia_session_exists_is_false_for_unknown_id<S: AuthStore>(s: &S) {
    assert!(!s.session_exists("does-not-exist", 0, 10_000).await.unwrap());
}

pub(crate) async fn uia_operations_on_missing_session_are_not_found<S: AuthStore>(s: &S) {
    assert!(matches!(
        s.mark_stage_complete("nope", "m.login.dummy").await,
        Err(StoreError::NotFound(_))
    ));
    assert!(matches!(
        s.completed_stages("nope").await,
        Err(StoreError::NotFound(_))
    ));
    assert!(matches!(
        s.set_session_data("nope", "k", serde_json::json!(1)).await,
        Err(StoreError::NotFound(_))
    ));
    assert!(matches!(
        s.get_session_data("nope", "k").await,
        Err(StoreError::NotFound(_))
    ));
}

/// Runs every shared test body against `make_store`, a factory so each test gets a fresh, empty
/// store (some backends, like `TablesAuthStore` over a real `hs-kv` backend, are not cheap to
/// reset in place). Call this once per store implementation from a single `#[tokio::test]`.
pub(crate) async fn setup_token_is_absent_until_inserted_and_the_first_insert_wins<S: AuthStore>(
    s: &S,
) {
    assert_eq!(s.setup_token().await.unwrap(), None);
    assert_eq!(s.setup_token_or_insert("first").await.unwrap(), "first");
    // A second replica booting with its own candidate is told the first one's.
    assert_eq!(s.setup_token_or_insert("second").await.unwrap(), "first");
    assert_eq!(s.setup_token().await.unwrap().as_deref(), Some("first"));
}

pub(crate) async fn setup_token_is_consumed_once_and_only_by_the_right_token<S: AuthStore>(s: &S) {
    // Nothing stored: nothing to consume, and in particular the empty string does not match
    // "no token".
    assert!(!s.consume_setup_token("").await.unwrap());
    assert!(!s.consume_setup_token("anything").await.unwrap());

    s.setup_token_or_insert("right").await.unwrap();
    assert!(!s.consume_setup_token("wrong").await.unwrap());
    assert!(!s.consume_setup_token("righ").await.unwrap());
    assert!(!s.consume_setup_token("right ").await.unwrap());
    // A wrong guess does not burn the token.
    assert_eq!(s.setup_token().await.unwrap().as_deref(), Some("right"));

    assert!(s.consume_setup_token("right").await.unwrap());
    assert!(!s.consume_setup_token("right").await.unwrap());
    assert_eq!(s.setup_token().await.unwrap(), None);
}

pub(crate) async fn clear_setup_token_withdraws_it_and_is_idempotent<S: AuthStore>(s: &S) {
    s.clear_setup_token().await.unwrap();
    s.setup_token_or_insert("token").await.unwrap();
    s.clear_setup_token().await.unwrap();
    assert_eq!(s.setup_token().await.unwrap(), None);
    assert!(!s.consume_setup_token("token").await.unwrap());
    s.clear_setup_token().await.unwrap();
}

pub(crate) async fn run_all<S: AuthStore>(make_store: impl Fn() -> S) {
    create_user_then_get_round_trips(&make_store()).await;
    create_user_conflict_is_case_insensitive(&make_store()).await;
    create_user_exact_duplicate_is_conflict(&make_store()).await;
    list_users_is_empty_for_a_fresh_store(&make_store()).await;
    list_users_is_sorted_by_user_id(&make_store()).await;
    is_localpart_available_reflects_existing_users(&make_store()).await;
    user_flag_setters_round_trip(&make_store()).await;
    set_password_hash_on_missing_user_is_not_found(&make_store()).await;
    profile_fields_round_trip(&make_store()).await;
    set_profile_fields_on_missing_user_is_not_found(&make_store()).await;
    device_and_token_lifecycle(&make_store()).await;
    device_display_name_and_seen_round_trip(&make_store()).await;
    set_display_name_on_missing_device_is_not_found(&make_store()).await;
    list_devices_is_sorted_by_device_id(&make_store()).await;
    devices_do_not_leak_across_users(&make_store()).await;
    access_token_bulk_deletes(&make_store()).await;
    mark_access_token_used_updates_last_used(&make_store()).await;
    mark_access_token_used_on_missing_token_is_not_an_error(&make_store()).await;
    refresh_token_lifecycle(&make_store()).await;
    mark_refresh_token_used_on_missing_is_not_found(&make_store()).await;
    delete_refresh_token_is_idempotent(&make_store()).await;
    login_token_is_single_use(&make_store()).await;
    login_token_expiry_is_enforced(&make_store()).await;
    consume_login_token_missing_returns_none(&make_store()).await;
    threepid_lookup_is_case_insensitive_on_address(&make_store()).await;
    uia_session_tracks_completed_stages_and_data(&make_store()).await;
    uia_session_exists_is_false_for_unknown_id(&make_store()).await;
    uia_operations_on_missing_session_are_not_found(&make_store()).await;
    setup_token_is_absent_until_inserted_and_the_first_insert_wins(&make_store()).await;
    setup_token_is_consumed_once_and_only_by_the_right_token(&make_store()).await;
    clear_setup_token_withdraws_it_and_is_idempotent(&make_store()).await;
}
