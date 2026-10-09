//! Binding and unbinding a third-party identifier at an identity server, through the real `hs`
//! binary and a fake identity server served over TLS from this test (the same one the
//! third-party invite tests use, with `/3pid/bind` and `/3pid/unbind` added here). Until this
//! test the identity-server side of `POST /account/3pid/bind`, `/unbind` and an administrator's
//! deactivation was covered by a fake seam inside `hs-auth` only.
//!
//! Alice binds an address: the identity server is asked with her `id_access_token` and the
//! session she names, and the binding is remembered. She unbinds it: the identity server is
//! asked with an `X-Matrix` signature from this server, and `id_server_unbind_result` is
//! `success`; unbinding an address nobody bound is `no-support`; an identity server this server
//! does not use is `M_SERVER_NOT_TRUSTED`; an identity server that fails the unbind is a `502`
//! for her. Then she binds two addresses and an administrator deactivates her: both are
//! unbound at the identity server, and the one it fails is logged and does not stop the
//! deactivation.

use std::sync::{Arc, Mutex};

use reqwest::{Method, StatusCode};
use serde_json::{Value, json};

#[path = "support/fake_identity.rs"]
mod fake_identity;
use fake_identity::{FakeIdentityServer, serve_tls};

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// The real `hs serve`, its log, killed when dropped.
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
            base: self.base.clone(),
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
        expected: StatusCode,
    ) -> Value {
        let (status, body) = self.call(method, path, body).await;
        assert_eq!(status, expected, "{path}: {body}");
        body
    }
}

fn metric(metrics: &str, name: &str) -> f64 {
    metrics
        .lines()
        .find(|l| l.starts_with(name))
        .and_then(|l| l.rsplit(' ').next())
        .and_then(|v| v.parse().ok())
        .unwrap_or(0.0)
}

/// What the fake identity server was asked to bind and unbind.
#[derive(Default)]
struct Recorded {
    /// `(bearer token, body)` of each `/3pid/bind`.
    binds: Mutex<Vec<(String, Value)>>,
    /// `(Authorization header, body)` of each `/3pid/unbind`.
    unbinds: Mutex<Vec<(String, Value)>>,
}

/// The sessions the fake identity server validated: `sid1` is `alice@example.org`, `sid2` is
/// `other@example.org`. Unbinding `other@example.org` fails with a `500`.
fn bind_routes(recorded: Arc<Recorded>) -> axum::Router {
    use axum::http::HeaderMap;
    use axum::routing::post;
    let for_bind = recorded.clone();
    axum::Router::new()
        .route(
            "/_matrix/identity/v2/3pid/bind",
            post(
                move |headers: HeaderMap, axum::Json(body): axum::Json<Value>| {
                    let recorded = for_bind.clone();
                    async move {
                        let bearer = headers
                            .get("authorization")
                            .and_then(|v| v.to_str().ok())
                            .unwrap_or("")
                            .to_owned();
                        recorded.binds.lock().unwrap().push((bearer, body.clone()));
                        let address = match body["sid"].as_str() {
                            Some("sid1") => "alice@example.org",
                            Some("sid2") => "other@example.org",
                            _ => {
                                return (
                                    StatusCode::NOT_FOUND,
                                    axum::Json(json!({"errcode": "M_NOT_FOUND"})),
                                );
                            }
                        };
                        (
                            StatusCode::OK,
                            axum::Json(json!({
                                "medium": "email",
                                "address": address,
                                "mxid": body["mxid"],
                                "not_before": 0,
                                "not_after": 4_102_444_800_000_u64,
                                "ts": 1,
                                "signatures": {"localhost": {"ed25519:0": "fake"}},
                            })),
                        )
                    }
                },
            ),
        )
        .route(
            "/_matrix/identity/v2/3pid/unbind",
            post(
                move |headers: HeaderMap, axum::Json(body): axum::Json<Value>| {
                    let recorded = recorded.clone();
                    async move {
                        let authorization = headers
                            .get("authorization")
                            .and_then(|v| v.to_str().ok())
                            .unwrap_or("")
                            .to_owned();
                        recorded
                            .unbinds
                            .lock()
                            .unwrap()
                            .push((authorization, body.clone()));
                        if body["threepid"]["address"] == "other@example.org" {
                            (StatusCode::INTERNAL_SERVER_ERROR, axum::Json(json!({})))
                        } else {
                            (StatusCode::OK, axum::Json(json!({})))
                        }
                    }
                },
            ),
        )
}

#[tokio::test]
async fn an_address_is_bound_and_unbound_at_the_identity_server_by_its_owner_and_at_deactivation() {
    let recorded = Arc::new(Recorded::default());
    let key = Arc::new(ed25519_dalek::SigningKey::from_bytes(&[7u8; 32]));
    let routes = recorded.clone();
    let base = serve_tls(move |base| {
        let ids = FakeIdentityServer {
            key,
            base,
            stored: Arc::default(),
        };
        ids.router().merge(bind_routes(routes))
    })
    .await;
    let id_server = base.trim_start_matches("https://").to_owned();

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
             auth:\n  enable_registration: true\n  identity_servers: [localhost]\n",
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
    let registered = nobody
        .expect(
            Method::POST,
            "/_matrix/client/v3/register",
            Some(json!({"username": "alice", "password": "hunter2-alice", "auth": {"type": "m.login.dummy"}})),
            StatusCode::OK,
        )
        .await;
    let alice = nobody.with(registered["access_token"].as_str().unwrap());
    let bind = |sid: &str, id_server: &str| json!({"id_server": id_server, "id_access_token": "ist-token", "sid": sid, "client_secret": "s3cret"});

    // An identity server this server does not use is refused before anybody is asked.
    let (status, body) = alice
        .call(
            Method::POST,
            "/_matrix/client/v3/account/3pid/bind",
            Some(bind("sid1", "evil.example")),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["errcode"], "M_SERVER_NOT_TRUSTED", "{body}");
    assert!(recorded.binds.lock().unwrap().is_empty());

    // Bound: the identity server is asked with her token and the session she named.
    alice
        .expect(
            Method::POST,
            "/_matrix/client/v3/account/3pid/bind",
            Some(bind("sid1", &id_server)),
            StatusCode::OK,
        )
        .await;
    hs.wait_for(&["bound a third-party identifier at an identity server"]);
    {
        let binds = recorded.binds.lock().unwrap();
        assert_eq!(binds.len(), 1);
        assert_eq!(binds[0].0, "Bearer ist-token");
        assert_eq!(binds[0].1["sid"], "sid1");
        assert_eq!(binds[0].1["client_secret"], "s3cret");
        assert_eq!(binds[0].1["mxid"], "@alice:example.org");
    }

    // Unbound by its owner, naming no identity server: the one this server bound it at is
    // asked, with this server's signature.
    let answer = alice
        .expect(
            Method::POST,
            "/_matrix/client/v3/account/3pid/unbind",
            Some(json!({"medium": "email", "address": "alice@example.org"})),
            StatusCode::OK,
        )
        .await;
    assert_eq!(answer["id_server_unbind_result"], "success", "{answer}");
    {
        let unbinds = recorded.unbinds.lock().unwrap();
        assert_eq!(unbinds.len(), 1);
        assert!(
            unbinds[0].0.starts_with("X-Matrix origin=\"example.org\","),
            "signed by this server: {}",
            unbinds[0].0
        );
        assert_eq!(unbinds[0].1["mxid"], "@alice:example.org");
        assert_eq!(unbinds[0].1["threepid"]["medium"], "email");
        assert_eq!(unbinds[0].1["threepid"]["address"], "alice@example.org");
    }
    // Nothing is bound any more: nowhere to try.
    let answer = alice
        .expect(
            Method::POST,
            "/_matrix/client/v3/account/3pid/unbind",
            Some(json!({"medium": "email", "address": "alice@example.org"})),
            StatusCode::OK,
        )
        .await;
    assert_eq!(answer["id_server_unbind_result"], "no-support", "{answer}");
    assert_eq!(recorded.unbinds.lock().unwrap().len(), 1);

    // Two bindings, one the identity server will fail to unbind: her own unbind of that one is
    // a 502 and the binding is kept.
    for sid in ["sid1", "sid2"] {
        alice
            .expect(
                Method::POST,
                "/_matrix/client/v3/account/3pid/bind",
                Some(bind(sid, &id_server)),
                StatusCode::OK,
            )
            .await;
    }
    let (status, body) = alice
        .call(
            Method::POST,
            "/_matrix/client/v3/account/3pid/unbind",
            Some(json!({"medium": "email", "address": "other@example.org"})),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_GATEWAY, "{body}");
    assert_eq!(recorded.unbinds.lock().unwrap().len(), 2);

    // An administrator deactivates her: both bindings are tried; the failure is logged and
    // kept, and she is deactivated either way.
    let deactivated = ops
        .expect(
            Method::POST,
            "/api/v1/users/%40alice%3Aexample.org/deactivate",
            Some(json!({})),
            StatusCode::OK,
        )
        .await;
    assert_eq!(deactivated["deactivated"], true, "{deactivated}");
    hs.wait_for(&["could not unbind a deactivated account's third-party identifier"]);
    {
        let unbinds = recorded.unbinds.lock().unwrap();
        let addresses: Vec<&str> = unbinds[2..]
            .iter()
            .map(|(_, body)| body["threepid"]["address"].as_str().unwrap())
            .collect();
        assert_eq!(unbinds.len(), 4, "{addresses:?}");
        assert!(addresses.contains(&"alice@example.org"));
        assert!(addresses.contains(&"other@example.org"));
    }
    let (status, body) = alice
        .call(Method::GET, "/_matrix/client/v3/account/whoami", None)
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");

    let metrics = reqwest::get(format!("http://127.0.0.1:{port}/metrics"))
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert_eq!(
        metric(&metrics, "hs_auth_threepid_changes_total{action=\"bound\"}"),
        3.0
    );
    assert_eq!(
        metric(
            &metrics,
            "hs_auth_threepid_changes_total{action=\"unbound\"}"
        ),
        2.0
    );
}
