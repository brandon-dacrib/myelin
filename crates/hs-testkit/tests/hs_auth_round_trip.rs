//! Proves the [`hs_testkit::scenario`] DSL by driving `hs-auth`'s real router
//! (`hs_auth::routes::router()`, mounted at bare spec-relative paths — see that crate's module
//! docs) through a full register, login, whoami, refresh, logout sequence for two independent
//! users. This is the acceptance test the crate-level docs promise; nothing here is synthetic:
//! the router is `hs-auth`'s own `AuthState::in_memory()` stack, exercised over real HTTP
//! request/response objects.

use axum::http::StatusCode;
use hs_auth::AuthState;
use hs_testkit::Scenario;

fn app() -> axum::Router {
    hs_auth::routes::router().with_state(AuthState::in_memory())
}

#[tokio::test]
async fn register_login_whoami_refresh_logout_round_trip() {
    let mut scenario = Scenario::new(app());

    // Register alice; the router hands back a usable access token immediately (the m.login.dummy
    // stage completes registration in one round).
    let register = scenario
        .register("alice", "alice", "correct horse battery staple")
        .await;
    register.assert_ok();
    assert_eq!(register.str_field("user_id"), "@alice:example.org");
    assert!(!register.str_field("access_token").is_empty());
    let alice_device = register.str_field("device_id").to_string();

    // whoami with the freshly registered token reports the same user and device.
    let whoami = scenario.whoami("alice").await;
    whoami.assert_ok();
    assert_eq!(whoami.str_field("user_id"), "@alice:example.org");
    assert_eq!(whoami.str_field("device_id"), alice_device);
    assert_eq!(whoami.json["is_guest"], false);

    // A second, independent user logs in by password (rather than registering) against the same
    // account created via the store directly is out of scope here; instead prove `login` works
    // end to end by registering bob, then logging bob in again with a *second* session.
    let register_bob = scenario.register("bob", "bob", "hunter2official").await;
    register_bob.assert_ok();

    let bob_login = scenario
        .login("bob_second_device", "bob", "hunter2official")
        .await;
    bob_login.assert_ok();
    assert_eq!(bob_login.str_field("user_id"), "@bob:example.org");
    assert!(
        scenario
            .session("bob_second_device")
            .unwrap()
            .refresh_token
            .is_some()
    );

    // Bob's second session's whoami is independent of his first.
    let bob_whoami = scenario.whoami("bob_second_device").await;
    bob_whoami.assert_ok();
    assert_eq!(bob_whoami.str_field("user_id"), "@bob:example.org");

    // Refresh bob's second session: the access token changes, whoami with the new one still
    // works, and the DSL's stored session was updated in place.
    let old_access_token = scenario
        .session("bob_second_device")
        .unwrap()
        .access_token
        .clone();
    let refreshed = scenario.refresh("bob_second_device").await;
    refreshed.assert_ok();
    let new_access_token = scenario
        .session("bob_second_device")
        .unwrap()
        .access_token
        .clone();
    assert_ne!(old_access_token, new_access_token);

    let whoami_after_refresh = scenario.whoami("bob_second_device").await;
    whoami_after_refresh.assert_ok();
    assert_eq!(
        whoami_after_refresh.str_field("user_id"),
        "@bob:example.org"
    );

    // Logging out invalidates the token; the DSL clears its stored access token on a successful
    // logout, so whoami now fails with M_MISSING_TOKEN (no Authorization header is sent at all).
    let logout = scenario.logout("bob_second_device").await;
    logout.assert_ok();
    let whoami_after_logout = scenario.whoami("bob_second_device").await;
    whoami_after_logout.assert_matrix_error(StatusCode::UNAUTHORIZED, "M_MISSING_TOKEN");

    // Alice's independent session is unaffected by bob's logout.
    let alice_whoami_still_works = scenario.whoami("alice").await;
    alice_whoami_still_works.assert_ok();

    // A well-formed but never-issued token is the distinct M_UNKNOWN_TOKEN case: inject a bogus
    // token into bob's (now credential-less) session and confirm the router tells the two cases
    // apart.
    scenario.set_access_token("bob_second_device", "syt_totally_made_up");
    let unknown_token = scenario.whoami("bob_second_device").await;
    unknown_token.assert_matrix_error(StatusCode::UNAUTHORIZED, "M_UNKNOWN_TOKEN");
}

#[tokio::test]
async fn wrong_password_login_is_a_matrix_shaped_forbidden_error() {
    let mut scenario = Scenario::new(app());
    scenario
        .register("carol", "carol", "the-right-password")
        .await
        .assert_ok();

    let bad_login = scenario
        .login("carol_attacker", "carol", "the-wrong-password")
        .await;
    bad_login.assert_matrix_error(StatusCode::FORBIDDEN, "M_FORBIDDEN");
    assert!(
        scenario.session("carol_attacker").is_none() || {
            // login() only populates the session on success; a failed attempt must leave no token.
            scenario
                .session("carol_attacker")
                .unwrap()
                .access_token
                .is_none()
        }
    );
}

#[tokio::test]
async fn double_registering_the_same_username_is_rejected() {
    let mut scenario = Scenario::new(app());
    scenario
        .register("dave", "dave", "hunter2official")
        .await
        .assert_ok();

    let second = scenario
        .register("dave_again", "dave", "a-different-password")
        .await;
    second.assert_matrix_error(StatusCode::BAD_REQUEST, "M_USER_IN_USE");
}
