//! `POST /_matrix/client/v3/rooms/{roomId}/upgrade` against the real `hs` binary, to room
//! version 12 and to an opaque-ID version.
//!
//! A version-12 room's ID is its create event's reference hash (MSC4291), so the replacement
//! room cannot be named before it is created. Before the fix the old room's tombstone named an
//! ID minted ahead, which the version-12 create ignored, so the tombstone pointed at a room that
//! never existed, and the replacement's carried-over power levels named its creator, which
//! version 12's auth rules refuse. Here alice upgrades a version-11 room to 12 through the
//! client API, bob follows the tombstone's `replacement_room` and joins it by that ID, and the
//! replacement names the old room as its predecessor and holds the moved alias. The same is done
//! for an upgrade from 10 to 11, which keeps the old order (ID, tombstone, room). `/metrics`
//! counts both upgrades.

use std::time::{Duration, Instant};

use serde_json::{Value, json};

fn reserve_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// One `hs serve` process, its stdout read line by line.
struct HsProcess {
    child: std::process::Child,
    lines: std::sync::mpsc::Receiver<String>,
    seen: Vec<String>,
}

impl HsProcess {
    fn serve(config_path: &std::path::Path) -> Self {
        use std::io::BufRead;
        let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_hs"))
            .args(["serve", "-c"])
            .arg(config_path)
            .env_remove("RUST_LOG")
            .env_remove("HS_DATA_DIR")
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::inherit())
            .spawn()
            .expect("the hs binary should start");
        let stdout = child.stdout.take().unwrap();
        let (tx, lines) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for line in std::io::BufReader::new(stdout)
                .lines()
                .map_while(Result::ok)
            {
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        Self {
            child,
            lines,
            seen: Vec::new(),
        }
    }

    /// Reads the log until a line contains `needle`. A debug `hs` under load can take a minute
    /// to boot, so the deadline is generous.
    fn wait_for(&mut self, needle: &str) -> String {
        let deadline = Instant::now() + Duration::from_secs(120);
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match self.lines.recv_timeout(left) {
                Ok(line) => {
                    self.seen.push(line.clone());
                    if line.contains(needle) {
                        return line;
                    }
                }
                Err(_) => panic!(
                    "the log never said {needle:?}; it said:\n{}",
                    self.seen.join("\n")
                ),
            }
        }
    }
}

impl Drop for HsProcess {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

const SERVER: &str = "upgrade.example.org";

fn config_yaml(port: u16, data_dir: &std::path::Path) -> String {
    format!(
        "server:\n  server_name: \"{SERVER}\"\n\
         listeners:\n  listeners:\n    - port: {port}\n      bind_addresses: [\"127.0.0.1\"]\n      resources: [client, health, metrics]\n\
         storage:\n  backend: embedded\n  data_dir: {data_dir:?}\n\
         media:\n  storage:\n    backend: local\n    path: {media:?}\n\
         auth:\n  enable_registration: true\n\
         rate_limits:\n  enabled: false\n",
        media = data_dir.join("media"),
    )
}

struct Client {
    http: reqwest::Client,
    base: String,
}

impl Client {
    /// `method path` with `body`, asserting success.
    async fn call(&self, method: reqwest::Method, path: &str, token: &str, body: Value) -> Value {
        let response = self
            .http
            .request(method.clone(), format!("{}{path}", self.base))
            .bearer_auth(token)
            .json(&body)
            .send()
            .await
            .unwrap();
        let status = response.status();
        let value: Value = response.json().await.unwrap_or(Value::Null);
        assert!(status.is_success(), "{method} {path}: {status} {value}");
        value
    }

    async fn get(&self, path: &str, token: &str) -> Value {
        let response = self
            .http
            .get(format!("{}{path}", self.base))
            .bearer_auth(token)
            .send()
            .await
            .unwrap();
        let status = response.status();
        let value: Value = response.json().await.unwrap_or(Value::Null);
        assert!(status.is_success(), "GET {path}: {status} {value}");
        value
    }

    async fn register(&self, username: &str) -> String {
        let first: Value = self
            .http
            .post(format!("{}/_matrix/client/v3/register", self.base))
            .json(&json!({"username": username, "password": "correct horse"}))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let done: Value = self
            .http
            .post(format!("{}/_matrix/client/v3/register", self.base))
            .json(&json!({
                "username": username,
                "password": "correct horse",
                "auth": {"type": "m.login.dummy", "session": first["session"]},
            }))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        done["access_token"]
            .as_str()
            .unwrap_or_else(|| panic!("registration failed: {done}"))
            .to_owned()
    }

    async fn metric(&self, sample: &str) -> u64 {
        let text = self
            .http
            .get(format!("{}/metrics", self.base))
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        text.lines()
            .find_map(|line| line.strip_prefix(sample)?.trim().parse::<f64>().ok())
            .map_or(0, |v| v as u64)
    }
}

/// A room ID or alias as one path segment.
fn segment(id: &str) -> String {
    id.replace('!', "%21")
        .replace('#', "%23")
        .replace(':', "%3A")
}

/// Alice makes a public room at `from` with alias `#{alias}`, bob joins it, alice upgrades it to
/// `to`; then bob joins the room the old room's tombstone names, by that ID, and says something
/// there. Returns `(old room, replacement room)`.
async fn upgrade_and_follow(
    client: &Client,
    alice: &str,
    bob: &str,
    from: &str,
    to: &str,
    alias: &str,
) -> (String, String) {
    let post = reqwest::Method::POST;
    let old = client
        .call(
            post.clone(),
            "/_matrix/client/v3/createRoom",
            alice,
            json!({
                "preset": "public_chat",
                "room_version": from,
                "room_alias_name": alias,
                "topic": format!("a version {from} room"),
            }),
        )
        .await["room_id"]
        .as_str()
        .unwrap()
        .to_owned();
    client
        .call(
            post.clone(),
            &format!("/_matrix/client/v3/join/{}", segment(&old)),
            bob,
            json!({}),
        )
        .await;

    let upgraded = client
        .call(
            post.clone(),
            &format!("/_matrix/client/v3/rooms/{}/upgrade", segment(&old)),
            alice,
            json!({"new_version": to}),
        )
        .await;
    let replacement = upgraded["replacement_room"].as_str().unwrap().to_owned();

    // What a client does: read the tombstone and follow it.
    let tombstone = client
        .get(
            &format!(
                "/_matrix/client/v3/rooms/{}/state/m.room.tombstone/",
                segment(&old)
            ),
            bob,
        )
        .await;
    let named = tombstone["replacement_room"].as_str().unwrap().to_owned();
    assert_eq!(named, replacement, "the tombstone names the replacement");
    let joined = client
        .call(
            post.clone(),
            &format!("/_matrix/client/v3/join/{}", segment(&named)),
            bob,
            json!({}),
        )
        .await;
    assert_eq!(joined["room_id"], named.as_str());
    client
        .call(
            reqwest::Method::PUT,
            &format!(
                "/_matrix/client/v3/rooms/{}/send/m.room.message/t-{to}",
                segment(&named)
            ),
            bob,
            json!({"msgtype": "m.text", "body": "followed the tombstone"}),
        )
        .await;

    let create = client
        .get(
            &format!(
                "/_matrix/client/v3/rooms/{}/state/m.room.create/",
                segment(&named)
            ),
            bob,
        )
        .await;
    assert_eq!(create["room_version"], to);
    assert_eq!(create["predecessor"]["room_id"], old.as_str());
    let topic = client
        .get(
            &format!(
                "/_matrix/client/v3/rooms/{}/state/m.room.topic/",
                segment(&named)
            ),
            bob,
        )
        .await;
    assert_eq!(topic["topic"], format!("a version {from} room"));
    let resolved = client
        .get(
            &format!(
                "/_matrix/client/v3/directory/room/{}",
                segment(&format!("#{alias}:{SERVER}"))
            ),
            bob,
        )
        .await;
    assert_eq!(resolved["room_id"], named.as_str(), "the alias moved");
    (old, named)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_tombstone_names_a_replacement_room_that_can_be_joined_by_that_id() {
    let dir = tempfile::tempdir().unwrap();
    let port = reserve_port();
    let config = dir.path().join("hs.yaml");
    std::fs::write(&config, config_yaml(port, &dir.path().join("data"))).unwrap();
    let client = Client {
        http: reqwest::Client::new(),
        base: format!("http://127.0.0.1:{port}"),
    };
    let mut hs = HsProcess::serve(&config);
    hs.wait_for("listening");

    let alice = client.register("alice").await;
    let bob = client.register("bob").await;

    let (old12, new12) = upgrade_and_follow(&client, &alice, &bob, "11", "12", "to-twelve").await;
    assert!(
        !new12.contains(':'),
        "a version-12 room id carries no server name: {new12}"
    );
    let create = client
        .get(
            &format!(
                "/_matrix/client/v3/rooms/{}/state/m.room.create/",
                segment(&new12)
            ),
            &bob,
        )
        .await;
    assert!(
        create["predecessor"].get("event_id").is_none(),
        "a version-12 predecessor names the room only: {create}"
    );
    let (_, new11) = upgrade_and_follow(&client, &alice, &bob, "10", "11", "to-eleven").await;
    assert!(new11.ends_with(&format!(":{SERVER}")), "{new11}");

    // The old room is locked down after the upgrade: bob may no longer speak in it.
    let refused = client
        .http
        .put(format!(
            "{}/_matrix/client/v3/rooms/{}/send/m.room.message/late",
            client.base,
            segment(&old12)
        ))
        .bearer_auth(&bob)
        .json(&json!({"msgtype": "m.text", "body": "too late"}))
        .send()
        .await
        .unwrap();
    assert_eq!(refused.status(), reqwest::StatusCode::FORBIDDEN);

    assert_eq!(
        client
            .metric("hs_room_upgrades_total{outcome=\"completed\"}")
            .await,
        2
    );
}

/// What an upgrade carries that is not the spec's recommended state: a ban (the banned user is
/// banned in the replacement and cannot follow the tombstone), the room's place in the public
/// directory (the replacement is listed, the old room no longer), and a room closed to
/// federation stays closed. Before 2026-10-02 none of the three came across.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_upgrade_carries_bans_the_directory_entry_and_federation_closure() {
    let dir = tempfile::tempdir().unwrap();
    let port = reserve_port();
    let config = dir.path().join("hs.yaml");
    std::fs::write(&config, config_yaml(port, &dir.path().join("data"))).unwrap();
    let client = Client {
        http: reqwest::Client::new(),
        base: format!("http://127.0.0.1:{port}"),
    };
    let mut hs = HsProcess::serve(&config);
    hs.wait_for("listening");
    let post = reqwest::Method::POST;

    let alice = client.register("alice").await;
    let mallory = client.register("mallory").await;
    let old = client
        .call(
            post.clone(),
            "/_matrix/client/v3/createRoom",
            &alice,
            json!({
                "preset": "public_chat",
                "room_version": "10",
                "visibility": "public",
                "creation_content": {"m.federate": false},
            }),
        )
        .await["room_id"]
        .as_str()
        .unwrap()
        .to_owned();
    client
        .call(
            post.clone(),
            &format!("/_matrix/client/v3/join/{}", segment(&old)),
            &mallory,
            json!({}),
        )
        .await;
    client
        .call(
            post.clone(),
            &format!("/_matrix/client/v3/rooms/{}/ban", segment(&old)),
            &alice,
            json!({"user_id": format!("@mallory:{SERVER}"), "reason": "spam"}),
        )
        .await;

    let replacement = client
        .call(
            post.clone(),
            &format!("/_matrix/client/v3/rooms/{}/upgrade", segment(&old)),
            &alice,
            json!({"new_version": "11"}),
        )
        .await["replacement_room"]
        .as_str()
        .unwrap()
        .to_owned();

    // The ban came across, with its reason, and it holds.
    let ban = client
        .get(
            &format!(
                "/_matrix/client/v3/rooms/{}/state/m.room.member/{}",
                segment(&replacement),
                segment(&format!("@mallory:{SERVER}"))
            ),
            &alice,
        )
        .await;
    assert_eq!(ban["membership"], "ban");
    assert_eq!(ban["reason"], "spam");
    let refused = client
        .http
        .post(format!(
            "{}/_matrix/client/v3/join/{}",
            client.base,
            segment(&replacement)
        ))
        .bearer_auth(&mallory)
        .json(&json!({}))
        .send()
        .await
        .unwrap();
    assert_eq!(
        refused.status(),
        reqwest::StatusCode::FORBIDDEN,
        "mallory cannot follow the tombstone into the replacement"
    );

    // The directory lists the replacement and not the tombstoned room.
    let listed = client.get("/_matrix/client/v3/publicRooms", &alice).await;
    let ids: Vec<&str> = listed["chunk"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|room| room["room_id"].as_str())
        .collect();
    assert!(ids.contains(&replacement.as_str()), "{listed}");
    assert!(!ids.contains(&old.as_str()), "{listed}");
    let visibility = client
        .get(
            &format!(
                "/_matrix/client/v3/directory/list/room/{}",
                segment(&replacement)
            ),
            &alice,
        )
        .await;
    assert_eq!(visibility["visibility"], "public");

    // The replacement is as closed to federation as the old room was.
    let create = client
        .get(
            &format!(
                "/_matrix/client/v3/rooms/{}/state/m.room.create/",
                segment(&replacement)
            ),
            &alice,
        )
        .await;
    assert_eq!(create["m.federate"], false);

    let line = hs.wait_for("upgraded a room");
    assert!(
        line.contains("bans_carried=1") && line.contains("directory_moved=true"),
        "the upgrade's log line counts the ban and the directory move: {line}"
    );
}

/// Sytest's "/upgrade preserves direct room state": the upgrader's own `m.direct` names the
/// replacement once the upgrade is done. The replacement's create burst reached no stream, so
/// the session hub never saw the upgrader join it and never carried the account data over.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_upgraded_direct_chat_is_still_a_direct_chat_for_the_upgrader() {
    let dir = tempfile::tempdir().unwrap();
    let port = reserve_port();
    let config = dir.path().join("hs.yaml");
    std::fs::write(&config, config_yaml(port, &dir.path().join("data"))).unwrap();
    let client = Client {
        http: reqwest::Client::new(),
        base: format!("http://127.0.0.1:{port}"),
    };
    let mut hs = HsProcess::serve(&config);
    hs.wait_for("listening");

    let alice = client.register("alice").await;
    let user_id = format!("@alice:{SERVER}");
    let old = client
        .call(
            reqwest::Method::POST,
            "/_matrix/client/v3/createRoom",
            &alice,
            json!({}),
        )
        .await["room_id"]
        .as_str()
        .unwrap()
        .to_owned();
    client
        .call(
            reqwest::Method::PUT,
            &format!(
                "/_matrix/client/v3/user/{}/account_data/m.direct",
                segment(&user_id)
            ),
            &alice,
            json!({ user_id.clone(): [old.clone()] }),
        )
        .await;
    let new = client
        .call(
            reqwest::Method::POST,
            &format!("/_matrix/client/v3/rooms/{}/upgrade", segment(&old)),
            &alice,
            json!({"new_version": "11"}),
        )
        .await["replacement_room"]
        .as_str()
        .unwrap()
        .to_owned();
    // What Sytest does: wait for the new room in /sync, then read the account data once.
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let sync = client
            .get("/_matrix/client/v3/sync?timeout=0", &alice)
            .await;
        if sync["rooms"]["join"].get(new.as_str()).is_some() {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the replacement never reached /sync"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let direct = client
        .get(
            &format!(
                "/_matrix/client/v3/user/{}/account_data/m.direct",
                segment(&user_id)
            ),
            &alice,
        )
        .await;
    let rooms: Vec<&str> = direct[user_id.as_str()]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(Value::as_str)
        .collect();
    assert!(rooms.contains(&new.as_str()), "{direct}");
    assert!(rooms.contains(&old.as_str()), "{direct}");
}

/// Push rules about the old room follow its members into the replacement (Complement's
/// `TestPushRuleRoomUpgrade`, Synapse's `copy_push_rules_from_room_to_room_for_user`): the
/// upgrader's room rule is there for the new room the moment the upgrade answers (her join is
/// part of it), and a follower's override rule naming the old room in its `rule_id` and its
/// `room_id` condition is copied, renamed and rewritten for the new room, when he joins; a
/// disabled rule stays disabled. The old rules stay as they were.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn push_rules_about_the_old_room_follow_its_members_into_the_replacement() {
    let dir = tempfile::tempdir().unwrap();
    let port = reserve_port();
    let config = dir.path().join("hs.yaml");
    std::fs::write(&config, config_yaml(port, &dir.path().join("data"))).unwrap();
    let client = Client {
        http: reqwest::Client::new(),
        base: format!("http://127.0.0.1:{port}"),
    };
    let mut hs = HsProcess::serve(&config);
    hs.wait_for("listening");

    let alice = client.register("alice").await;
    let bob = client.register("bob").await;
    let post = reqwest::Method::POST;
    let put = reqwest::Method::PUT;

    let old = client
        .call(
            post.clone(),
            "/_matrix/client/v3/createRoom",
            &alice,
            json!({"preset": "public_chat", "room_version": "10"}),
        )
        .await["room_id"]
        .as_str()
        .unwrap()
        .to_owned();
    client
        .call(
            post.clone(),
            &format!("/_matrix/client/v3/join/{}", segment(&old)),
            &bob,
            json!({}),
        )
        .await;

    // Alice mutes the room; bob has a (disabled) override rule about it, named after it.
    client
        .call(
            put.clone(),
            &format!("/_matrix/client/v3/pushrules/global/room/{}", segment(&old)),
            &alice,
            json!({"actions": ["dont_notify"]}),
        )
        .await;
    let bob_rule = format!("loud-{old}");
    client
        .call(
            put.clone(),
            &format!(
                "/_matrix/client/v3/pushrules/global/override/{}",
                segment(&bob_rule)
            ),
            &bob,
            json!({
                "conditions": [{"kind": "event_match", "key": "room_id", "pattern": old}],
                "actions": ["notify", {"set_tweak": "sound", "value": "loud"}],
            }),
        )
        .await;
    client
        .call(
            put.clone(),
            &format!(
                "/_matrix/client/v3/pushrules/global/override/{}/enabled",
                segment(&bob_rule)
            ),
            &bob,
            json!({"enabled": false}),
        )
        .await;

    let new = client
        .call(
            post.clone(),
            &format!("/_matrix/client/v3/rooms/{}/upgrade", segment(&old)),
            &alice,
            json!({"new_version": "11"}),
        )
        .await["replacement_room"]
        .as_str()
        .unwrap()
        .to_owned();

    // The copy is made by the session hub as it processes the join, just after the join
    // itself: wait for its log line for each user before reading.
    let copied_for = |hs: &mut HsProcess, user: &str| {
        let deadline = Instant::now() + Duration::from_secs(120);
        loop {
            let line = hs.wait_for("their push rules for the old room followed");
            if line.contains(user) {
                return;
            }
            assert!(Instant::now() < deadline, "no copy logged for {user}");
        }
    };
    copied_for(&mut hs, "@alice:");
    let copied = client
        .get(
            &format!("/_matrix/client/v3/pushrules/global/room/{}", segment(&new)),
            &alice,
        )
        .await;
    assert_eq!(copied["actions"], json!(["dont_notify"]), "{copied}");
    assert_eq!(copied["rule_id"], new.as_str());
    let kept = client
        .get(
            &format!("/_matrix/client/v3/pushrules/global/room/{}", segment(&old)),
            &alice,
        )
        .await;
    assert_eq!(
        kept["actions"],
        json!(["dont_notify"]),
        "the old rule stays"
    );

    client
        .call(
            post.clone(),
            &format!("/_matrix/client/v3/join/{}", segment(&new)),
            &bob,
            json!({}),
        )
        .await;
    copied_for(&mut hs, "@bob:");
    let bob_copy = format!("loud-{new}");
    let copied = client
        .get(
            &format!(
                "/_matrix/client/v3/pushrules/global/override/{}",
                segment(&bob_copy)
            ),
            &bob,
        )
        .await;
    assert_eq!(copied["rule_id"], bob_copy.as_str(), "{copied}");
    assert_eq!(
        copied["enabled"], false,
        "a disabled rule is copied disabled"
    );
    assert_eq!(copied["conditions"][0]["pattern"], new.as_str());
    assert_eq!(copied["actions"][1]["value"], "loud");

    // The copy is account data the next /sync carries, as any rule change is.
    let sync = client.get("/_matrix/client/v3/sync?timeout=0", &bob).await;
    let rules = sync["account_data"]["events"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["type"] == "m.push_rules")
        .cloned()
        .unwrap_or(Value::Null);
    let ids: Vec<&str> = rules["content"]["global"]["override"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|r| r["rule_id"].as_str())
        .collect();
    assert!(ids.contains(&bob_copy.as_str()), "{ids:?}");
    assert!(ids.contains(&bob_rule.as_str()), "{ids:?}");
}
