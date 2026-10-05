//! Third-party (3PID) invites through the real `hs` binary, against a fake identity server
//! served over TLS from this test: refused `M_THREEPID_DENIED` while `auth.identity_servers` is
//! empty (the default); allowed once an operator names the identity server through the admin API
//! (the setting applies at once); a bound address is an ordinary invite of its owner; an unbound
//! one is stored with the identity server and held in the room as `m.room.third_party_invite`;
//! the identity server's `onbind` turns it into an invite carrying `third_party_invite`, which
//! the invitee joins; `/metrics` counts each outcome. Before this, the request (with no
//! `user_id`) was read as an invite of the inviter and refused "Invite is not a valid transition
//! from Join".

use reqwest::{Method, StatusCode};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};

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
            // The fake identity server's certificate is self-signed, as Sytest's is.
            .env("HS_TEST_INSECURE_IDENTITY_SERVER_TLS", "1")
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
}

fn metric(metrics: &str, name: &str) -> f64 {
    metrics
        .lines()
        .find(|line| line.starts_with(name))
        .and_then(|line| line.rsplit(' ').next())
        .and_then(|value| value.parse().ok())
        .unwrap_or_else(|| panic!("{name} is not in /metrics:\n{metrics}"))
}

fn escape(room: &str) -> String {
    room.replace('!', "%21").replace(':', "%3A")
}

#[path = "support/fake_identity.rs"]
mod fake_identity;
use fake_identity::{FakeIdentityServer, serve_tls};

#[tokio::test]
async fn an_email_invite_reaches_its_owner_through_an_allowed_identity_server_only() {
    let stored = Arc::new(Mutex::new(Vec::new()));
    let key = Arc::new(ed25519_dalek::SigningKey::from_bytes(&[5u8; 32]));
    let ids_cell: Arc<Mutex<Option<FakeIdentityServer>>> = Arc::default();
    let ids_for_router = ids_cell.clone();
    let (key2, stored2) = (key.clone(), stored.clone());
    let base = serve_tls(move |base| {
        let ids = FakeIdentityServer {
            key: key2,
            base,
            stored: stored2,
        };
        *ids_for_router.lock().unwrap() = Some(ids.clone());
        ids.router()
    })
    .await;
    let ids = ids_cell.lock().unwrap().clone().unwrap();
    let id_server = base.trim_start_matches("https://").to_owned();

    let dir = tempfile::tempdir().unwrap();
    let port = free_port();
    let config_path = dir.path().join("hs.yaml");
    std::fs::write(
        &config_path,
        format!(
            "server:\n  server_name: example.org\n\
             listeners:\n  listeners:\n    - port: {port}\n      bind_addresses: [\"127.0.0.1\"]\n      resources: [client, federation, health, metrics]\n\
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
    let mut users = Vec::new();
    for name in ["alice", "bob", "carol"] {
        let body = nobody
            .expect(
                Method::POST,
                "/_matrix/client/v3/register",
                Some(json!({"username": name, "password": format!("hunter2-{name}"), "auth": {"type": "m.login.dummy"}})),
                StatusCode::OK,
            )
            .await;
        users.push(nobody.with(body["access_token"].as_str().unwrap()));
    }
    let (alice, carol) = (&users[0], &users[2]);
    let room = alice
        .expect(
            Method::POST,
            "/_matrix/client/v3/createRoom",
            Some(json!({"preset": "private_chat"})),
            StatusCode::OK,
        )
        .await["room_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let invite_path = format!("/_matrix/client/v3/rooms/{}/invite", escape(&room));
    let by_email = |address: &str| json!({"id_server": id_server, "id_access_token": "t", "medium": "email", "address": address});
    let member = |user: &'static str| {
        let alice = alice.clone();
        let room = room.clone();
        async move {
            let (status, body) = alice
                .call(
                    Method::GET,
                    &format!(
                        "/_matrix/client/v3/rooms/{}/state/m.room.member/{user}",
                        escape(&room)
                    ),
                    None,
                )
                .await;
            (status == StatusCode::OK).then_some(body)
        }
    };

    // No identity server allowed yet: refused, and nobody is asked.
    let (status, body) = alice
        .call(
            Method::POST,
            &invite_path,
            Some(by_email("bob@example.org")),
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert_eq!(body["errcode"], "M_THREEPID_DENIED");

    let updated = ops
        .expect(
            Method::PATCH,
            "/api/v1/config/auth",
            Some(json!({"identity_servers": ["localhost"]})),
            StatusCode::OK,
        )
        .await;
    assert_eq!(
        updated["applied"]["requires_restart"],
        json!([]),
        "{updated}"
    );

    // A bound address: an ordinary invite of its owner.
    alice
        .expect(
            Method::POST,
            &invite_path,
            Some(by_email("bob@example.org")),
            StatusCode::OK,
        )
        .await;
    assert_eq!(
        member("@bob:example.org").await.unwrap()["membership"],
        "invite"
    );

    // An unbound one: stored with the identity server, and the room holds the invitation.
    alice
        .expect(
            Method::POST,
            &invite_path,
            Some(by_email("carol@example.org")),
            StatusCode::OK,
        )
        .await;
    assert_eq!(stored.lock().unwrap().len(), 1);
    assert_eq!(stored.lock().unwrap()[0]["sender"], "@alice:example.org");
    let invitation = alice
        .expect(
            Method::GET,
            &format!(
                "/_matrix/client/v3/rooms/{}/state/m.room.third_party_invite/tok1",
                escape(&room)
            ),
            None,
            StatusCode::OK,
        )
        .await;
    assert_eq!(invitation["display_name"], "c...@e...");
    assert!(member("@carol:example.org").await.is_none());

    // Carol binds the address; the identity server tells this server, and she is invited.
    nobody
        .expect(
            Method::PUT,
            "/_matrix/federation/v1/3pid/onbind",
            Some(json!({
                "medium": "email",
                "address": "carol@example.org",
                "mxid": "@carol:example.org",
                "invites": [{
                    "medium": "email",
                    "address": "carol@example.org",
                    "mxid": "@carol:example.org",
                    "room_id": room,
                    "sender": "@alice:example.org",
                    "signed": ids.sign("@carol:example.org", "tok1"),
                }],
            })),
            StatusCode::OK,
        )
        .await;
    let invited = member("@carol:example.org").await.unwrap();
    assert_eq!(invited["membership"], "invite", "{invited}");
    assert_eq!(invited["third_party_invite"]["display_name"], "c...@e...");
    carol
        .expect(
            Method::POST,
            &format!("/_matrix/client/v3/join/{}", escape(&room)),
            Some(json!({})),
            StatusCode::OK,
        )
        .await;
    assert_eq!(
        member("@carol:example.org").await.unwrap()["membership"],
        "join"
    );

    let metrics = reqwest::get(format!("{}/metrics", nobody.base))
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    for (outcome, want) in [("invited", 1.0), ("stored", 1.0), ("exchanged", 1.0)] {
        assert_eq!(
            metric(
                &metrics,
                &format!("hs_room_third_party_invites_total{{outcome=\"{outcome}\"}}")
            ),
            want
        );
    }
    assert!(
        metric(
            &metrics,
            "hs_room_third_party_invites_total{outcome=\"refused\"}"
        ) >= 1.0
    );
}
