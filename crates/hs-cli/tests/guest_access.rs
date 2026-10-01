//! Guest access through the real `hs` binary: `auth.allow_guest_access` is off by default and
//! applies on the running server when an operator turns it on through the admin API (as the
//! Configuration page does); a guest then gets an account from `POST /register?kind=guest`,
//! may read and talk in rooms that let guests in, may not create rooms or upload, is made to
//! leave when a room withdraws guest access, shows as a guest in the admin API, and becomes a
//! full account by registering with its own token. `/metrics` counts each outcome.
//!
//! Every step but the first refusal failed before guest access had a setting: the guest
//! registration was always refused, and a guest's every request but `/whoami` and `/logout`
//! answered `M_GUEST_ACCESS_FORBIDDEN`.

use reqwest::{Method, StatusCode};
use serde_json::{Value, json};

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// One run of the real `hs` binary (the harness of `config_hot.rs`).
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

    /// Reads the log until a line contains every one of `needles`.
    fn wait_for(&mut self, needles: &[&str]) -> String {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
        loop {
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            match self.lines.recv_timeout(left) {
                Ok(line) => {
                    self.seen.push(line.clone());
                    if needles.iter().all(|needle| line.contains(needle)) {
                        return line;
                    }
                }
                Err(_) => panic!(
                    "the log never said {needles:?}; it said:\n{}",
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

#[derive(Clone)]
struct Caller {
    base: String,
    token: Option<String>,
}

impl Caller {
    fn with(&self, token: &str) -> Self {
        Self {
            token: Some(token.to_owned()),
            ..self.clone()
        }
    }

    async fn call(&self, method: Method, path: &str, body: Option<Value>) -> (StatusCode, Value) {
        let mut request = reqwest::Client::new().request(method, format!("{}{path}", self.base));
        if let Some(token) = &self.token {
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

    async fn expect(
        &self,
        method: Method,
        path: &str,
        body: Option<Value>,
        want: StatusCode,
    ) -> Value {
        let (status, body) = self.call(method, path, body).await;
        assert_eq!(status, want, "{path}: {body}");
        body
    }

    async fn guest_access(&self, room: &str, value: &str) {
        self.expect(
            Method::PUT,
            &format!(
                "/_matrix/client/v3/rooms/{}/state/m.room.guest_access/",
                escape(room)
            ),
            Some(json!({"guest_access": value})),
            StatusCode::OK,
        )
        .await;
    }

    async fn membership(&self, room: &str, user: &str) -> Option<String> {
        let (status, body) = self
            .call(
                Method::GET,
                &format!(
                    "/_matrix/client/v3/rooms/{}/state/m.room.member/{user}",
                    escape(room)
                ),
                None,
            )
            .await;
        (status == StatusCode::OK).then(|| body["membership"].as_str().unwrap().to_owned())
    }
}

fn escape(room: &str) -> String {
    room.replace('!', "%21").replace(':', "%3A")
}

fn metric(metrics: &str, name: &str) -> f64 {
    metrics
        .lines()
        .find(|line| line.starts_with(name))
        .and_then(|line| line.rsplit(' ').next())
        .and_then(|value| value.parse().ok())
        .unwrap_or_else(|| panic!("{name} is not in /metrics:\n{metrics}"))
}

#[tokio::test]
async fn a_guest_reads_joins_talks_is_removed_and_upgrades_once_an_operator_allows_guests() {
    let dir = tempfile::tempdir().unwrap();
    let port = free_port();
    let config_path = dir.path().join("hs.yaml");
    std::fs::write(
        &config_path,
        format!(
            "server:\n  server_name: example.org\n\
             listeners:\n  listeners:\n    - port: {port}\n      bind_addresses: [\"127.0.0.1\"]\n      resources: [client, health, metrics]\n\
             storage:\n  backend: embedded\n  data_dir: {:?}\n\
             media:\n  storage:\n    backend: local\n    path: {:?}\n\
             federation:\n  enabled: false\n\
             rate_limits:\n  enabled: false\n\
             auth:\n  enable_registration: true\n",
            dir.path().join("db"),
            dir.path().join("media"),
        ),
    )
    .unwrap();

    let mut hs = HsProcess::serve(&config_path);
    let setup_line = hs.wait_for(&["setup_link="]);
    let setup_token = setup_line
        .rsplit_once("#token=")
        .unwrap()
        .1
        .trim()
        .to_owned();
    let nobody = Caller {
        base: format!("http://127.0.0.1:{port}"),
        token: None,
    };
    let session = nobody
        .expect(
            Method::POST,
            "/api/v1/setup",
            Some(json!({"setup_token": setup_token, "username": "ops", "password": "hunter2-first-admin"})),
            StatusCode::CREATED,
        )
        .await;
    let ops = nobody.with(session["access_token"].as_str().unwrap());

    // Off by default, and a hot setting.
    let (status, refused) = nobody
        .call(
            Method::POST,
            "/_matrix/client/v3/register?kind=guest",
            Some(json!({})),
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{refused}");
    assert_eq!(refused["errcode"], "M_GUEST_ACCESS_FORBIDDEN");
    let schema = ops
        .expect(Method::GET, "/api/v1/config/schema", None, StatusCode::OK)
        .await;
    let setting = schema["settings"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["pointer"] == "/auth/allow_guest_access")
        .expect("the setting is in the schema the Configuration page reads")
        .clone();
    assert_eq!(setting["applies"], "hot", "{setting}");
    let updated = ops
        .expect(
            Method::PATCH,
            "/api/v1/config/auth",
            Some(json!({"allow_guest_access": true})),
            StatusCode::OK,
        )
        .await;
    assert_eq!(
        updated["applied"]["requires_restart"],
        json!([]),
        "{updated}"
    );
    hs.wait_for(&[
        "the auth settings are now in force",
        "allow_guest_access=true",
    ]);

    // A guest: an account, a device and a token, no password.
    let guest = nobody
        .expect(
            Method::POST,
            "/_matrix/client/v3/register?kind=guest",
            Some(json!({})),
            StatusCode::OK,
        )
        .await;
    let guest_id = guest["user_id"].as_str().unwrap().to_owned();
    assert!(guest_id.ends_with(":example.org"), "{guest}");
    assert!(guest["device_id"].is_string(), "{guest}");
    let guest_caller = nobody.with(guest["access_token"].as_str().unwrap());
    let whoami = guest_caller
        .expect(
            Method::GET,
            "/_matrix/client/v3/account/whoami",
            None,
            StatusCode::OK,
        )
        .await;
    assert_eq!(whoami["is_guest"], true);

    // Guests may not create rooms or upload.
    let (status, body) = guest_caller
        .call(
            Method::POST,
            "/_matrix/client/v3/createRoom",
            Some(json!({})),
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert_eq!(body["errcode"], "M_GUEST_ACCESS_FORBIDDEN");
    let response = reqwest::Client::new()
        .post(format!("{}/_matrix/client/v1/media/upload", nobody.base))
        .bearer_auth(guest["access_token"].as_str().unwrap())
        .header("content-type", "application/octet-stream")
        .body(vec![7u8; 16])
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);

    // Alice's room is public and world-readable: the guest can read it without joining, but
    // may not join until the room lets guests in.
    let alice = nobody
        .expect(
            Method::POST,
            "/_matrix/client/v3/register",
            Some(json!({"username": "alice", "password": "hunter2-alice", "auth": {"type": "m.login.dummy"}})),
            StatusCode::OK,
        )
        .await;
    let alice = nobody.with(alice["access_token"].as_str().unwrap());
    let room = alice
        .expect(
            Method::POST,
            "/_matrix/client/v3/createRoom",
            Some(json!({"preset": "public_chat"})),
            StatusCode::OK,
        )
        .await["room_id"]
        .as_str()
        .unwrap()
        .to_owned();
    alice
        .expect(
            Method::PUT,
            &format!(
                "/_matrix/client/v3/rooms/{}/state/m.room.history_visibility/",
                escape(&room)
            ),
            Some(json!({"history_visibility": "world_readable"})),
            StatusCode::OK,
        )
        .await;
    guest_caller
        .expect(
            Method::GET,
            &format!("/_matrix/client/v3/rooms/{}/messages?dir=b", escape(&room)),
            None,
            StatusCode::OK,
        )
        .await;
    let join_path = format!("/_matrix/client/v3/join/{}", escape(&room));
    let (status, body) = guest_caller
        .call(Method::POST, &join_path, Some(json!({})))
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");

    alice.guest_access(&room, "can_join").await;
    guest_caller
        .expect(Method::POST, &join_path, Some(json!({})), StatusCode::OK)
        .await;
    guest_caller
        .expect(
            Method::PUT,
            &format!(
                "/_matrix/client/v3/rooms/{}/send/m.room.message/g1",
                escape(&room)
            ),
            Some(json!({"msgtype": "m.text", "body": "sup"})),
            StatusCode::OK,
        )
        .await;
    let sync = guest_caller
        .expect(
            Method::GET,
            "/_matrix/client/v3/sync?timeout=0",
            None,
            StatusCode::OK,
        )
        .await;
    assert!(sync["rooms"]["join"][&room].is_object(), "{sync}");
    guest_caller
        .expect(
            Method::PUT,
            &format!("/_matrix/client/v3/profile/{guest_id}/displayname"),
            Some(json!({"displayname": "creeper"})),
            StatusCode::OK,
        )
        .await;

    // The room withdraws guest access: the guest leaves; alice stays.
    alice.guest_access(&room, "forbidden").await;
    assert_eq!(
        alice.membership(&room, &guest_id).await.as_deref(),
        Some("leave")
    );
    assert_eq!(
        alice
            .membership(&room, "@alice:example.org")
            .await
            .as_deref(),
        Some("join")
    );

    // The admin API says who is a guest.
    let user = ops
        .expect(
            Method::GET,
            &format!("/api/v1/users/{guest_id}"),
            None,
            StatusCode::OK,
        )
        .await;
    assert_eq!(user["is_guest"], true, "{user}");
    let guests = ops
        .expect(
            Method::GET,
            "/api/v1/users?guests=true",
            None,
            StatusCode::OK,
        )
        .await;
    let listed: Vec<&str> = guests["items"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|u| u["user_id"].as_str())
        .collect();
    assert_eq!(listed, vec![guest_id.as_str()], "{guests}");

    // The guest becomes a full account under its own name.
    let localpart = guest_id
        .trim_start_matches('@')
        .split(':')
        .next()
        .unwrap()
        .to_owned();
    let upgraded = nobody
        .expect(
            Method::POST,
            "/_matrix/client/v3/register",
            Some(json!({
                "username": localpart,
                "password": "SIR_Arthur_David",
                "guest_access_token": guest["access_token"],
                "auth": {"type": "m.login.dummy"},
            })),
            StatusCode::OK,
        )
        .await;
    assert_eq!(upgraded["user_id"], guest_id.as_str());
    let member = nobody.with(upgraded["access_token"].as_str().unwrap());
    let whoami = member
        .expect(
            Method::GET,
            "/_matrix/client/v3/account/whoami",
            None,
            StatusCode::OK,
        )
        .await;
    assert_eq!(whoami["is_guest"], false);
    member
        .expect(
            Method::POST,
            "/_matrix/client/v3/createRoom",
            Some(json!({})),
            StatusCode::OK,
        )
        .await;

    let metrics = reqwest::get(format!("{}/metrics", nobody.base))
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert_eq!(
        metric(
            &metrics,
            "hs_auth_guest_registrations_total{outcome=\"created\"}"
        ),
        1.0
    );
    assert_eq!(
        metric(
            &metrics,
            "hs_auth_guest_registrations_total{outcome=\"refused\"}"
        ),
        1.0
    );
    assert!(metric(&metrics, "hs_auth_guest_requests_refused_total") >= 2.0);
    assert_eq!(metric(&metrics, "hs_room_guest_joins_refused_total"), 1.0);
    assert_eq!(metric(&metrics, "hs_auth_guest_upgrades_total"), 1.0);
}
