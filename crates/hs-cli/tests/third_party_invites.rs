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

const PEPPER: &str = "pepper";

fn lookup_hash(address: &str) -> String {
    use base64::Engine;
    use sha2::Digest;
    let digest = sha2::Sha256::digest(format!("{address} email {PEPPER}").as_bytes());
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(digest)
}

fn b64(bytes: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD_NO_PAD.encode(bytes)
}

/// A fake identity server: `bob@example.org` is bound to bob, anything else is not, and every
/// invitation it stores gets token `tok1` and this key.
#[derive(Clone)]
struct FakeIdentityServer {
    key: Arc<ed25519_dalek::SigningKey>,
    base: String,
    stored: Arc<Mutex<Vec<Value>>>,
}

impl FakeIdentityServer {
    fn public_key(&self) -> String {
        b64(&self.key.verifying_key().to_bytes())
    }

    fn sign(&self, mxid: &str, token: &str) -> Value {
        let signed = json!({"mxid": mxid, "token": token});
        let canonical = hs_model::canonical::to_canonical_value(&signed, true).unwrap();
        let signature = ed25519_dalek::Signer::sign(&*self.key, &canonical.to_canonical_bytes());
        let mut signed = signed;
        signed["signatures"] = json!({"localhost": {"ed25519:0": b64(&signature.to_bytes())}});
        signed
    }

    fn router(&self) -> axum::Router {
        use axum::routing::{get, post};
        let lookup_server = self.clone();
        let store_server = self.clone();
        let valid_server = self.clone();
        axum::Router::new()
            .route(
                "/_matrix/identity/v2/hash_details",
                get(|| async { axum::Json(json!({"lookup_pepper": PEPPER, "algorithms": ["sha256"]})) }),
            )
            .route(
                "/_matrix/identity/v2/lookup",
                post(move |axum::Json(body): axum::Json<Value>| {
                    let _ = &lookup_server;
                    async move {
                        let mut mappings = serde_json::Map::new();
                        let bob = lookup_hash("bob@example.org");
                        for address in body["addresses"].as_array().unwrap() {
                            if address == &Value::String(bob.clone()) {
                                mappings.insert(bob.clone(), json!("@bob:example.org"));
                            }
                        }
                        axum::Json(json!({"mappings": mappings}))
                    }
                }),
            )
            .route(
                "/_matrix/identity/v2/store-invite",
                post(move |axum::Json(body): axum::Json<Value>| {
                    let server = store_server.clone();
                    async move {
                        server.stored.lock().unwrap().push(body);
                        let key = server.public_key();
                        axum::Json(json!({
                            "token": "tok1",
                            "display_name": "c...@e...",
                            "public_key": key,
                            "public_keys": [{
                                "public_key": key,
                                "key_validity_url": format!("{}/_matrix/identity/v2/pubkey/isvalid", server.base),
                            }],
                        }))
                    }
                }),
            )
            .route(
                "/_matrix/identity/v2/pubkey/isvalid",
                get(
                    move |axum::extract::Query(query): axum::extract::Query<
                        std::collections::HashMap<String, String>,
                    >| {
                        let server = valid_server.clone();
                        async move {
                            axum::Json(json!({"valid": query.get("public_key") == Some(&server.public_key())}))
                        }
                    },
                ),
            )
    }
}

/// Serves `router` over TLS on a free port, with a certificate for `localhost` the server under
/// test is told not to verify (as Sytest's own identity server needs).
async fn serve_tls(make: impl FnOnce(String) -> axum::Router) -> String {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let key = rcgen::KeyPair::generate().unwrap();
    let cert = rcgen::CertificateParams::new(vec!["localhost".to_string()])
        .unwrap()
        .self_signed(&key)
        .unwrap();
    let key_der = rustls_pki_types::PrivateKeyDer::Pkcs8(
        rustls_pki_types::PrivatePkcs8KeyDer::from(key.serialize_der()),
    );
    let config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(vec![cert.der().clone()], key_der)
        .unwrap();
    let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(config));
    // The identity server itself speaks plain HTTP on one port; a TLS-terminating proxy in
    // front of it is what the server under test talks to.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let base = format!("https://localhost:{port}");
    let plain = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let plain_port = plain.local_addr().unwrap().port();
    let router = make(base.clone());
    tokio::spawn(async move {
        let _ = axum::serve(plain, router).await;
    });
    tokio::spawn(async move {
        loop {
            let Ok((tcp, _)) = listener.accept().await else {
                return;
            };
            let acceptor = acceptor.clone();
            tokio::spawn(async move {
                let Ok(mut tls) = acceptor.accept(tcp).await else {
                    return;
                };
                let Ok(mut backend) =
                    tokio::net::TcpStream::connect(("127.0.0.1", plain_port)).await
                else {
                    return;
                };
                let _ = tokio::io::copy_bidirectional(&mut tls, &mut backend).await;
            });
        }
    });
    base
}

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
