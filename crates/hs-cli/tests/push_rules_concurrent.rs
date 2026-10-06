//! Push-rule changes one user makes at the same time all land, and `/sync` carries the result,
//! through a real `hs serve` (Complement's `TestPushRuleRoomUpgrade`, whose parallel subtests
//! add one user's room rules for two rooms at once and then sync until each rule is there).
//!
//! Every change is a read, an edit and a write of the user's whole ruleset; two at once both
//! read the same ruleset and the second write dropped the first one's rule, which the user's
//! `/sync` then never showed (`hs_push::rulesets::CachedRulesetStore::update_ruleset`).

use serde_json::{Value, json};

fn reserve_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn config(port: u16, data_dir: &std::path::Path) -> hs_config::Config {
    let media_dir = data_dir.join("media");
    let yaml = format!(
        "server:\n  server_name: \"127.0.0.1:{port}\"\n\
         listeners:\n  listeners:\n    - port: {port}\n      bind_addresses: [\"127.0.0.1\"]\n      resources: [client, health]\n\
         storage:\n  backend: embedded\n  data_dir: {data_dir:?}\n\
         media:\n  storage:\n    backend: local\n    path: {media_dir:?}\n\
         auth:\n  enable_registration: true\n\
         rate_limits:\n  enabled: false\n"
    );
    hs_config::Config::from_yaml(&yaml).expect("the test configuration parses")
}

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
        done["user_id"]
            .as_str()
            .unwrap_or_else(|| panic!("registration failed: {done}"))
            .to_owned(),
        done["access_token"].as_str().unwrap().to_owned(),
    )
}

async fn sync(client: &reqwest::Client, base: &str, token: &str, since: Option<&str>) -> Value {
    let mut query = vec![("timeout", "0".to_owned())];
    if let Some(since) = since {
        query.push(("since", since.to_owned()));
    }
    client
        .get(format!("{base}/_matrix/client/v3/sync"))
        .query(&query)
        .bearer_auth(token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap()
}

/// The `rule_id`s of the room rules in a `{"global": ...}` ruleset.
fn room_rules(global: &Value) -> Vec<String> {
    global["room"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|rule| rule["rule_id"].as_str().map(str::to_owned))
        .collect()
}

#[tokio::test]
async fn room_rules_added_at_once_are_all_kept_and_synced() {
    let port = reserve_port();
    let dir = tempfile::tempdir().unwrap();
    let handle = hs_cli::serve::spawn_serve(config(port, dir.path()), Default::default())
        .await
        .expect("the server boots");
    let base = handle.base_url();
    let client = reqwest::Client::new();
    let (_, token) = register(&client, &base, "bob").await;
    let since = sync(&client, &base, &token, None).await["next_batch"]
        .as_str()
        .unwrap()
        .to_owned();

    // Five rounds of eight rules added at once.
    let mut wanted = Vec::new();
    for round in 0..5 {
        let rooms: Vec<String> = (0..8)
            .map(|n| format!("!r{round}x{n}:127.0.0.1:{port}"))
            .collect();
        let mut puts = tokio::task::JoinSet::new();
        for room in &rooms {
            let client = client.clone();
            let url = format!("{base}/_matrix/client/v3/pushrules/global/room/{room}");
            let token = token.clone();
            puts.spawn(async move {
                let response = client
                    .put(url)
                    .bearer_auth(token)
                    .json(&json!({"actions": ["dont_notify"]}))
                    .send()
                    .await
                    .unwrap();
                assert!(response.status().is_success(), "{}", response.status());
            });
        }
        while let Some(put) = puts.join_next().await {
            put.unwrap();
        }
        wanted.extend(rooms);
    }

    let all: Value = client
        .get(format!("{base}/_matrix/client/v3/pushrules/"))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let held = room_rules(&all["global"]);
    let lost: Vec<&String> = wanted.iter().filter(|r| !held.contains(r)).collect();
    assert!(
        lost.is_empty(),
        "rules lost to a change made at the same time: {lost:?}"
    );

    let response = sync(&client, &base, &token, Some(&since)).await;
    let synced = response["account_data"]["events"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|e| e["type"] == "m.push_rules")
        .map(|e| room_rules(&e["content"]["global"]))
        .unwrap_or_default();
    let missing: Vec<&String> = wanted.iter().filter(|r| !synced.contains(r)).collect();
    assert!(
        missing.is_empty(),
        "the incremental /sync's m.push_rules lacks {missing:?}: {response}"
    );
}
