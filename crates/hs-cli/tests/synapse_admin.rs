//! The `/_synapse/admin` compatibility surface through the real server: what a Synapse-era
//! tool (synapse-admin, Draupnir, an operator's script) finds when it is pointed at this server.
//! Every route forwards into the native `/api/v1` router behind the same tokens and scopes
//! (`hs_compat::admin_proxy`, mounted by `hs_cli::synapse_shims`), so the capability probe, the
//! user and room listings and a deactivation answer in Synapse's shapes, and a caller without an
//! administrator's token is refused as the native API refuses it. The routes an operator sees
//! in `routes.json` under the `synapse-admin-compat` surface are exactly the ones mounted.

use reqwest::{Method, StatusCode};
use serde_json::{Value, json};

fn test_config(data_dir: &std::path::Path) -> hs_config::Config {
    let media_dir = data_dir.join("media");
    let yaml = format!(
        "server:\n  server_name: example.org\n\
         listeners:\n  listeners:\n    - port: 1\n      bind_addresses: [\"127.0.0.1\"]\n      resources: [client, admin, health, metrics]\n\
         storage:\n  backend: embedded\n  data_dir: {:?}\n\
         media:\n  storage:\n    backend: local\n    path: {:?}\n\
         auth:\n  enable_registration: true\n  registration_shared_secret: \"a-shared-secret-for-the-test\"\n",
        data_dir, media_dir
    );
    let mut config = hs_config::Config::from_yaml(&yaml).unwrap();
    config.listeners.listeners[0].port = 0;
    config
}

fn setup_token_of(link: &str) -> &str {
    link.split_once("/admin/setup#token=")
        .unwrap_or_else(|| panic!("not a setup link: {link}"))
        .1
}

/// Calls `base` and answers the status and the JSON body (`Null` for none).
async fn call(
    base: &str,
    method: Method,
    path: &str,
    token: Option<&str>,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let mut request = reqwest::Client::new().request(method, format!("{base}{path}"));
    if let Some(token) = token {
        request = request.bearer_auth(token);
    }
    if let Some(body) = body {
        request = request.json(&body);
    }
    let response = request.send().await.unwrap();
    let status = response.status();
    let text = response.text().await.unwrap();
    (status, serde_json::from_str(&text).unwrap_or(Value::Null))
}

async fn first_admin(handle: &hs_cli::serve::ServeHandle) -> String {
    let token = setup_token_of(handle.setup_link.as_deref().unwrap()).to_owned();
    let (status, admin) = call(
        &handle.base_url(),
        Method::POST,
        "/api/v1/setup",
        None,
        Some(json!({"setup_token": token, "username": "ops", "password": "hunter2-first-admin"})),
    )
    .await;
    assert!(status.is_success(), "{admin}");
    admin["access_token"].as_str().unwrap().to_owned()
}

async fn register(base: &str, username: &str) -> String {
    let (status, body) = call(
        base,
        Method::POST,
        "/_matrix/client/v3/register",
        None,
        Some(json!({
            "username": username,
            "password": format!("hunter2-{username}"),
            "auth": {"type": "m.login.dummy"},
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    body["access_token"].as_str().unwrap().to_owned()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn synapse_era_tooling_finds_the_admin_operations_it_expects() {
    let dir = tempfile::tempdir().unwrap();
    let manifest_path = dir.path().join("routes.json");
    let handle = hs_cli::serve::spawn_serve(
        test_config(dir.path()),
        hs_cli::serve::ServeOptions {
            routes_manifest_path: Some(manifest_path.clone()),
            ..Default::default()
        },
    )
    .await
    .expect("server should boot");
    let base = handle.base_url();
    let admin = first_admin(&handle).await;
    let alice = register(&base, "alice").await;

    // The capability probe every Synapse tool starts with, and the compat surface's own name
    // for this server.
    let (status, version) = call(
        &base,
        Method::GET,
        "/_synapse/admin/v1/server_version",
        Some(&admin),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{version}");
    assert!(version["server_version"].is_string(), "{version}");

    // Without a token, and with a plain user's token: refused with the native API's own status
    // (`401`: a client's access token is not an administrator's token there), in the flat
    // `errcode` shape Synapse's clients read.
    let (status, refused) = call(&base, Method::GET, "/_synapse/admin/v2/users", None, None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{refused}");
    assert_eq!(refused["errcode"], "M_FORBIDDEN", "{refused}");
    let (status, refused) = call(
        &base,
        Method::GET,
        "/_synapse/admin/v2/users",
        Some(&alice),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{refused}");
    assert_eq!(refused["errcode"], "M_FORBIDDEN", "{refused}");

    // The user listing, in Synapse's shape: `users[]` with `name`, `admin`, `deactivated`, and
    // a `total`.
    let (status, users) = call(
        &base,
        Method::GET,
        "/_synapse/admin/v2/users?limit=10",
        Some(&admin),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{users}");
    let names: Vec<&str> = users["users"]
        .as_array()
        .unwrap()
        .iter()
        .map(|u| u["name"].as_str().unwrap())
        .collect();
    assert!(names.contains(&"@ops:example.org"), "{users}");
    assert!(names.contains(&"@alice:example.org"), "{users}");
    assert_eq!(users["total"], 2, "{users}");
    let ops = users["users"]
        .as_array()
        .unwrap()
        .iter()
        .find(|u| u["name"] == "@ops:example.org")
        .unwrap();
    assert_eq!(ops["admin"], true, "{users}");
    assert_eq!(ops["deactivated"], false, "{users}");

    // One user, and a user who does not exist.
    let (status, alice_record) = call(
        &base,
        Method::GET,
        "/_synapse/admin/v2/users/@alice:example.org",
        Some(&admin),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{alice_record}");
    assert_eq!(alice_record["name"], "@alice:example.org");
    assert_eq!(alice_record["admin"], false);
    let (status, nobody) = call(
        &base,
        Method::GET,
        "/_synapse/admin/v2/users/@nobody:example.org",
        Some(&admin),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{nobody}");
    assert_eq!(nobody["errcode"], "M_NOT_FOUND", "{nobody}");

    // A room alice makes is in the room listing and answers by id.
    let (status, created) = call(
        &base,
        Method::POST,
        "/_matrix/client/v3/createRoom",
        Some(&alice),
        Some(json!({"name": "Ops", "preset": "public_chat"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{created}");
    let room_id = created["room_id"].as_str().unwrap().to_owned();
    let (status, rooms) = call(
        &base,
        Method::GET,
        "/_synapse/admin/v1/rooms",
        Some(&admin),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{rooms}");
    assert_eq!(rooms["total_rooms"], 1, "{rooms}");
    assert_eq!(rooms["rooms"][0]["room_id"], room_id, "{rooms}");
    assert_eq!(rooms["rooms"][0]["name"], "Ops", "{rooms}");
    let (status, room) = call(
        &base,
        Method::GET,
        &format!("/_synapse/admin/v1/rooms/{room_id}"),
        Some(&admin),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{room}");
    assert_eq!(room["room_id"], room_id, "{room}");
    assert_eq!(room["joined_members"], 1, "{room}");
    assert_eq!(room["creator"], "@alice:example.org", "{room}");

    // synapse-admin's user page: create a user through Synapse's create-or-modify route (with
    // an email address and an upstream identity), read the record back with them, change the
    // display name and the administrator flag, list devices, joined rooms and pushers, reset
    // the password, act as the user, whois, and check a username.
    let (status, bob) = call(
        &base,
        Method::PUT,
        "/_synapse/admin/v2/users/@bob:example.org",
        Some(&admin),
        Some(json!({
            "password": "bob-password-1",
            "displayname": "Bob",
            "threepids": [{"medium": "email", "address": "bob@example.org"}],
            "external_ids": [{"auth_provider": "oidc-example", "external_id": "bob-at-idp"}],
            "admin": false,
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{bob}");
    assert_eq!(bob["name"], "@bob:example.org", "{bob}");
    assert_eq!(bob["displayname"], "Bob", "{bob}");
    assert_eq!(bob["threepids"][0]["address"], "bob@example.org", "{bob}");
    assert_eq!(
        bob["external_ids"][0]["auth_provider"], "oidc-example",
        "{bob}"
    );
    let (status, bob) = call(
        &base,
        Method::PUT,
        "/_synapse/admin/v2/users/@bob:example.org",
        Some(&admin),
        Some(json!({"displayname": "Robert", "admin": true, "threepids": []})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{bob}");
    assert_eq!(bob["displayname"], "Robert", "{bob}");
    assert_eq!(bob["admin"], true, "{bob}");
    assert_eq!(bob["threepids"], json!([]), "{bob}");
    assert_eq!(bob["external_ids"][0]["external_id"], "bob-at-idp", "{bob}");
    let (_, is_admin) = call(
        &base,
        Method::GET,
        "/_synapse/admin/v1/users/@bob:example.org/admin",
        Some(&admin),
        None,
    )
    .await;
    assert_eq!(is_admin, json!({"admin": true}));
    call(
        &base,
        Method::PUT,
        "/_synapse/admin/v1/users/@bob:example.org/admin",
        Some(&admin),
        Some(json!({"admin": false})),
    )
    .await;
    let (_, found) = call(
        &base,
        Method::GET,
        "/_synapse/admin/v1/auth_providers/oidc-example/users/bob-at-idp",
        Some(&admin),
        None,
    )
    .await;
    assert_eq!(found, json!({"user_id": "@bob:example.org"}));
    let (status, reset) = call(
        &base,
        Method::POST,
        "/_synapse/admin/v1/reset_password/@bob:example.org",
        Some(&admin),
        Some(json!({"new_password": "bob-password-2", "logout_devices": true})),
    )
    .await;
    assert_eq!((status, reset), (StatusCode::OK, json!({})));
    let (status, login) = call(
        &base,
        Method::POST,
        "/_matrix/client/v3/login",
        None,
        Some(json!({
            "type": "m.login.password",
            "identifier": {"type": "m.id.user", "user": "bob"},
            "password": "bob-password-2",
            "device_id": "BOBPHONE",
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{login}");
    let bob_token = login["access_token"].as_str().unwrap().to_owned();
    let (_, devices) = call(
        &base,
        Method::GET,
        "/_synapse/admin/v2/users/@bob:example.org/devices",
        Some(&admin),
        None,
    )
    .await;
    assert_eq!(devices["total"], 1, "{devices}");
    assert_eq!(devices["devices"][0]["device_id"], "BOBPHONE", "{devices}");
    call(
        &base,
        Method::PUT,
        "/_synapse/admin/v2/users/@bob:example.org/devices/BOBPHONE",
        Some(&admin),
        Some(json!({"display_name": "Bob's phone"})),
    )
    .await;
    let (_, device) = call(
        &base,
        Method::GET,
        "/_synapse/admin/v2/users/@bob:example.org/devices/BOBPHONE",
        Some(&admin),
        None,
    )
    .await;
    assert_eq!(device["display_name"], "Bob's phone", "{device}");
    call(
        &base,
        Method::POST,
        &format!("/_matrix/client/v3/join/{}", urlencode(&room_id)),
        Some(&bob_token),
        Some(json!({})),
    )
    .await;
    let (_, joined) = call(
        &base,
        Method::GET,
        "/_synapse/admin/v1/users/@bob:example.org/joined_rooms",
        Some(&admin),
        None,
    )
    .await;
    assert_eq!(joined["joined_rooms"], json!([room_id]), "{joined}");
    let (_, pushers) = call(
        &base,
        Method::GET,
        "/_synapse/admin/v1/users/@bob:example.org/pushers",
        Some(&admin),
        None,
    )
    .await;
    assert_eq!(pushers["total"], 0, "{pushers}");
    let (status, acting) = call(
        &base,
        Method::POST,
        "/_synapse/admin/v1/users/@bob:example.org/login",
        Some(&admin),
        Some(json!({})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{acting}");
    let (_, whoami) = call(
        &base,
        Method::GET,
        "/_matrix/client/v3/account/whoami",
        acting["access_token"].as_str(),
        None,
    )
    .await;
    assert_eq!(whoami["user_id"], "@bob:example.org", "{whoami}");
    let (_, whois) = call(
        &base,
        Method::GET,
        "/_synapse/admin/v1/whois/@bob:example.org",
        Some(&admin),
        None,
    )
    .await;
    assert_eq!(whois["user_id"], "@bob:example.org", "{whois}");
    assert!(whois["devices"]["BOBPHONE"].is_object(), "{whois}");
    let (status, _) = call(
        &base,
        Method::GET,
        "/_synapse/admin/v1/username_available?username=carol",
        Some(&admin),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, taken) = call(
        &base,
        Method::GET,
        "/_synapse/admin/v1/username_available?username=bob",
        Some(&admin),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{taken}");
    assert_eq!(taken["errcode"], "M_USER_IN_USE", "{taken}");

    // The room page: members, state, block, make admin; the registration tokens page; the
    // reports page; the federation page; the media statistics page.
    let (_, members) = call(
        &base,
        Method::GET,
        &format!("/_synapse/admin/v1/rooms/{room_id}/members"),
        Some(&admin),
        None,
    )
    .await;
    assert_eq!(members["total"], 2, "{members}");
    let (_, state) = call(
        &base,
        Method::GET,
        &format!("/_synapse/admin/v1/rooms/{room_id}/state"),
        Some(&admin),
        None,
    )
    .await;
    assert!(
        state["state"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["type"] == "m.room.create"),
        "{state}"
    );
    let (status, blocked) = call(
        &base,
        Method::PUT,
        &format!("/_synapse/admin/v1/rooms/{room_id}/block"),
        Some(&admin),
        Some(json!({"block": true})),
    )
    .await;
    assert_eq!((status, blocked), (StatusCode::OK, json!({"block": true})));
    let (_, block_status) = call(
        &base,
        Method::GET,
        &format!("/_synapse/admin/v1/rooms/{room_id}/block"),
        Some(&admin),
        None,
    )
    .await;
    assert_eq!(block_status["block"], true, "{block_status}");
    call(
        &base,
        Method::PUT,
        &format!("/_synapse/admin/v1/rooms/{room_id}/block"),
        Some(&admin),
        Some(json!({"block": false})),
    )
    .await;
    let (_, token) = call(
        &base,
        Method::POST,
        "/_synapse/admin/v1/registration_tokens/new",
        Some(&admin),
        Some(json!({"token": "compat-token", "uses_allowed": 2})),
    )
    .await;
    assert_eq!(token["token"], "compat-token", "{token}");
    assert_eq!(token["uses_allowed"], 2, "{token}");
    let (_, tokens) = call(
        &base,
        Method::GET,
        "/_synapse/admin/v1/registration_tokens",
        Some(&admin),
        None,
    )
    .await;
    assert!(
        tokens["registration_tokens"]
            .as_array()
            .unwrap()
            .iter()
            .any(|t| t["token"] == "compat-token"),
        "{tokens}"
    );
    let (status, _) = call(
        &base,
        Method::DELETE,
        "/_synapse/admin/v1/registration_tokens/compat-token",
        Some(&admin),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, reports) = call(
        &base,
        Method::GET,
        "/_synapse/admin/v1/event_reports",
        Some(&admin),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{reports}");
    assert_eq!(reports["event_reports"], json!([]), "{reports}");
    let (status, destinations) = call(
        &base,
        Method::GET,
        "/_synapse/admin/v1/federation/destinations",
        Some(&admin),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{destinations}");
    assert!(destinations["destinations"].is_array(), "{destinations}");
    let (status, statistics) = call(
        &base,
        Method::GET,
        "/_synapse/admin/v1/statistics/users/media",
        Some(&admin),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{statistics}");
    assert!(statistics["users"].is_array(), "{statistics}");

    // Deactivation through the Synapse route is the native deactivation: alice's token stops
    // working and the listing says so.
    let (status, deactivated) = call(
        &base,
        Method::POST,
        "/_synapse/admin/v1/deactivate/@alice:example.org",
        Some(&admin),
        Some(json!({"erase": false})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{deactivated}");
    assert_eq!(deactivated["id_server_unbind_result"], "no-support");
    let (status, whoami) = call(
        &base,
        Method::GET,
        "/_matrix/client/v3/account/whoami",
        Some(&alice),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{whoami}");
    let (_, alice_record) = call(
        &base,
        Method::GET,
        "/_synapse/admin/v2/users/@alice:example.org",
        Some(&admin),
        None,
    )
    .await;
    assert_eq!(alice_record["deactivated"], true, "{alice_record}");

    // The shared-secret registration protocol's first step answers too (the config above sets
    // the secret; without one both register routes are "unrecognized", by design).
    let (status, nonce) = call(
        &base,
        Method::GET,
        "/_synapse/admin/v1/register",
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{nonce}");
    assert!(nonce["nonce"].is_string(), "{nonce}");

    // `routes.json` lists every mounted compat route, and only mounted ones: each answers
    // something other than "unrecognized" to a request with the admin's token.
    let manifest: Value =
        serde_json::from_str(&std::fs::read_to_string(&manifest_path).unwrap()).unwrap();
    let compat: Vec<(String, String)> = manifest["routes"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|r| r["surface"] == "synapse-admin-compat")
        .map(|r| {
            (
                r["method"].as_str().unwrap().to_owned(),
                r["path"].as_str().unwrap().to_owned(),
            )
        })
        .collect();
    assert!(
        compat.len() >= 60,
        "the compat surface has shrunk: {compat:?}"
    );
    for (method, path) in &compat {
        // Alice, not the administrator: the loop reaches the deactivation route too.
        let path = path
            .replace("{user_id}", "@alice:example.org")
            .replace("{room_id}", &room_id)
            .replace("{txn_id}", "t1")
            .replace("{device_id}", "NOSUCHDEVICE")
            .replace("{event_id}", "$nosuchevent")
            .replace("{delete_id}", "nosuchtask")
            .replace("{redact_id}", "nosuchtask")
            .replace("{token}", "nosuchtoken")
            .replace("{report_id}", "nosuchreport")
            .replace("{server_name}", "example.org")
            .replace("{media_id}", "nosuchmedia")
            .replace("{destination}", "other.example")
            .replace("{provider}", "oidc-example")
            .replace("{external_id}", "nobody")
            .replace("{medium}", "email")
            .replace("{address}", "nobody@example.org");
        let method = Method::from_bytes(method.as_bytes()).unwrap();
        let (status, body) =
            call(&base, method.clone(), &path, Some(&admin), Some(json!({}))).await;
        assert_ne!(
            body["errcode"], "M_UNRECOGNIZED",
            "{method} {path} is in routes.json but not mounted: {status} {body}"
        );
    }

    handle.shutdown().await;
}

fn urlencode(s: &str) -> String {
    s.replace('!', "%21")
        .replace(':', "%3A")
        .replace('#', "%23")
}
