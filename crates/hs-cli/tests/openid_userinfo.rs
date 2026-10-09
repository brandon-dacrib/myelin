//! `GET /_matrix/federation/v1/openid/userinfo` is served whenever the client listener runs,
//! with federation on and with `federation.enabled: false`, on a real `hs serve`.
//!
//! An integration manager or a widget's backend checks the OpenID token a user handed it
//! (`POST /user/{userId}/openid/request_token`) there, whether or not the user's server
//! federates: Synapse serves it from its `openid` listener resource, which a deployment may
//! enable without `federation`. After `/openid/userinfo` moved beside the `X-Matrix` layer inside
//! the federation router (2026-10-05), a server with federation off stopped answering it
//! (`404`); it is its own router now (`hs_federation::transport::openid`). With federation off,
//! every signed federation route is still not mounted.

use serde_json::{Value, json};

fn reserve_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn config(port: u16, data_dir: &std::path::Path, federation: bool) -> hs_config::Config {
    let yaml = format!(
        "server:\n  server_name: \"127.0.0.1:{port}\"\n\
         listeners:\n  listeners:\n    - port: {port}\n      bind_addresses: [\"127.0.0.1\"]\n      resources: [client, federation, health, metrics]\n\
         storage:\n  backend: embedded\n  data_dir: {data:?}\n\
         media:\n  storage:\n    backend: local\n    path: {media:?}\n\
         auth:\n  enable_registration: true\n\
         federation:\n  enabled: {federation}\n  ip_range_blocklist: []\n",
        data = data_dir.join("data"),
        media = data_dir.join("media"),
    );
    hs_config::Config::from_yaml(&yaml).expect("the test configuration parses")
}

/// Registers `username` through the UIA dance and returns `(user_id, access_token)`.
async fn register(client: &reqwest::Client, base: &str, username: &str) -> (String, String) {
    let first: Value = client
        .post(format!("{base}/_matrix/client/v3/register"))
        .json(&json!({"username": username, "password": "correct horse"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let session = first["session"].as_str().unwrap().to_owned();
    let done: Value = client
        .post(format!("{base}/_matrix/client/v3/register"))
        .json(&json!({
            "username": username,
            "password": "correct horse",
            "auth": {"type": "m.login.dummy", "session": session},
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    (
        done["user_id"].as_str().unwrap().to_owned(),
        done["access_token"].as_str().unwrap().to_owned(),
    )
}

/// A token from `request_token` is exchanged for its user; a made-up one is `401
/// M_UNKNOWN_TOKEN`, no token `401 M_MISSING_TOKEN`. Then: is a signed federation route served?
async fn openid_round_trip(federation: bool) {
    let port = reserve_port();
    let dir = tempfile::tempdir().unwrap();
    let server = hs_cli::serve::spawn_serve(
        config(port, dir.path(), federation),
        hs_cli::serve::ServeOptions {
            federation_scheme: Some("http"),
            ..Default::default()
        },
    )
    .await
    .expect("the server boots");
    let base = server.base_url();
    let client = reqwest::Client::new();
    let (alice, token) = register(&client, &base, "alice").await;

    let openid: Value = client
        .post(format!(
            "{base}/_matrix/client/v3/user/{alice}/openid/request_token"
        ))
        .bearer_auth(&token)
        .json(&json!({}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let openid_token = openid["access_token"]
        .as_str()
        .unwrap_or_else(|| panic!("no OpenID token: {openid}"));

    let userinfo = |query: String| {
        let client = client.clone();
        let base = base.clone();
        async move {
            let response = client
                .get(format!(
                    "{base}/_matrix/federation/v1/openid/userinfo{query}"
                ))
                .send()
                .await
                .unwrap();
            let status = response.status().as_u16();
            (
                status,
                response.json::<Value>().await.unwrap_or(Value::Null),
            )
        }
    };
    let (status, body) = userinfo(format!("?access_token={openid_token}")).await;
    assert_eq!(
        (status, body.clone()),
        (200, json!({"sub": alice})),
        "federation.enabled: {federation}: {body}"
    );
    let (status, body) = userinfo("?access_token=made-up".to_owned()).await;
    assert_eq!((status, &body["errcode"]), (401, &json!("M_UNKNOWN_TOKEN")));
    let (status, body) = userinfo(String::new()).await;
    assert_eq!((status, &body["errcode"]), (401, &json!("M_MISSING_TOKEN")));

    // The signed federation routes: mounted only with federation on. Unsigned, `/publicRooms`
    // is refused by the `X-Matrix` layer when it is there, and not found when it is not.
    // (`/version` is unsigned by the spec: `federation_version.rs`.)
    let version = client
        .get(format!("{base}/_matrix/federation/v1/publicRooms"))
        .send()
        .await
        .unwrap()
        .status()
        .as_u16();
    if federation {
        assert_eq!(version, 401, "an unsigned federation request is refused");
    } else {
        assert_eq!(version, 404, "no federation route is mounted");
    }

    server.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn openid_userinfo_is_served_with_federation_on() {
    openid_round_trip(true).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn openid_userinfo_is_served_with_federation_off() {
    openid_round_trip(false).await;
}
