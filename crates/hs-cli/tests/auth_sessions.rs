//! User-interactive auth, registration sessions, the CAPTCHA stage, OpenID tokens and the admin
//! `whois` through the real `hs` binary, with a stand-in CAPTCHA service on 127.0.0.1.
//!
//! What it walks, and the conformance test each step stands for:
//!
//! - `GET /capabilities` without a token is `401` (Sytest's and Complement's "GET
//!   /v3/capabilities is not public"), and works with one.
//! - Registration remembers its parameters across rounds ("registration remembers
//!   parameters"), logs the same account in again when its session finishes twice ("registration
//!   is idempotent"), and challenges a stage sent without the session its username was handed
//!   (Complement's "Registration without a session fails").
//! - `m.login.recaptcha` is checked with the CAPTCHA service named by `auth.recaptcha` ("Register
//!   with a recaptcha"); the challenge carries the site key and `completed`.
//! - Deleting a device with another account's password is `403`, and a session started deleting
//!   one device cannot delete another ("... requires UI auth user to match device owner", "The
//!   operation must be consistent through an interactive authentication session"); a wrong
//!   password at `/account/deactivate` is a `401` carrying `completed` ("Can't deactivate account
//!   with wrong password").
//! - An OpenID token is exchanged at `/_matrix/federation/v1/openid/userinfo` ("Can generate a
//!   openid access_token that can be exchanged for information about a user").
//! - `GET /admin/whois/{self}` lists the device's last connection ("/whois").
//! - The shared-secret registration refuses `us,er` (Complement's "... disallows symbols").
//! - `/metrics` counts the CAPTCHA checks and OpenID tokens.

use reqwest::{Method, StatusCode};
use serde_json::{Value, json};

const PASSWORD: &str = "sUp3rs3kr1t";
const SHARED_SECRET: &str = "a-shared-secret-for-the-test";

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// One run of the real `hs` binary (the harness of `guest_access.rs`).
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

    async fn register(&self, body: Value, want: StatusCode) -> Value {
        self.expect(
            Method::POST,
            "/_matrix/client/v3/register",
            Some(body),
            want,
        )
        .await
    }
}

/// A stand-in CAPTCHA service: `success` when the answer is "right" and the secret is the
/// configured one.
async fn fake_captcha_service() -> String {
    use axum::routing::post;
    async fn siteverify(
        axum::Form(form): axum::Form<std::collections::HashMap<String, String>>,
    ) -> axum::Json<Value> {
        let ok = form.get("secret").map(String::as_str) == Some("captcha-secret")
            && form.get("response").map(String::as_str) == Some("right");
        axum::Json(json!({"success": ok}))
    }
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(
            listener,
            axum::Router::new().route("/siteverify", post(siteverify)),
        )
        .await
        .unwrap();
    });
    format!("http://{addr}/siteverify")
}

fn metric(metrics: &str, name: &str) -> f64 {
    metrics
        .lines()
        .find(|line| line.starts_with(name))
        .and_then(|line| line.rsplit(' ').next())
        .and_then(|value| value.parse().ok())
        .unwrap_or_else(|| panic!("{name} is not in /metrics:\n{metrics}"))
}

fn password_auth(user: &str, session: Option<&str>) -> Value {
    let mut auth = json!({
        "type": "m.login.password",
        "identifier": {"type": "m.id.user", "user": user},
        "password": PASSWORD,
    });
    if let Some(session) = session {
        auth["session"] = json!(session);
    }
    auth
}

#[tokio::test]
async fn sessions_bind_their_user_and_operation_and_registration_remembers_what_it_was_sent() {
    let siteverify = fake_captcha_service().await;
    let dir = tempfile::tempdir().unwrap();
    let port = free_port();
    let config_path = dir.path().join("hs.yaml");
    std::fs::write(
        &config_path,
        format!(
            "server:\n  server_name: example.org\n  signing_key_path: {:?}\n\
             listeners:\n  listeners:\n    - port: {port}\n      bind_addresses: [\"127.0.0.1\"]\n      resources: [client, federation, health, metrics]\n\
             storage:\n  backend: embedded\n  data_dir: {:?}\n\
             media:\n  storage:\n    backend: local\n    path: {:?}\n\
             rate_limits:\n  enabled: false\n\
             auth:\n  enable_registration: true\n  registration_shared_secret: {SHARED_SECRET:?}\n\
             \x20 recaptcha:\n    public_key: captcha-site\n    private_key: captcha-secret\n    siteverify_api: {siteverify:?}\n",
            dir.path().join("keys"),
            dir.path().join("db"),
            dir.path().join("media"),
        ),
    )
    .unwrap();
    let mut hs = HsProcess::serve(&config_path);
    hs.wait_for(&["setup_link="]);
    let nobody = Caller {
        base: format!("http://127.0.0.1:{port}"),
        token: None,
    };

    // Capabilities are not public.
    nobody
        .expect(
            Method::GET,
            "/_matrix/client/v3/capabilities",
            None,
            StatusCode::UNAUTHORIZED,
        )
        .await;

    // Registration remembers its parameters and its password across rounds.
    let challenge = nobody
        .register(
            json!({
                "username": "alice",
                "password": PASSWORD,
                "device_id": "xyzzy",
                "initial_device_display_name": "display_name",
            }),
            StatusCode::UNAUTHORIZED,
        )
        .await;
    assert_eq!(challenge["completed"], json!([]), "{challenge}");
    assert_eq!(
        challenge["params"]["m.login.recaptcha"]["public_key"],
        "captcha-site"
    );
    let session = challenge["session"].as_str().unwrap().to_owned();
    let finish = json!({"auth": {"type": "m.login.dummy", "session": session}});
    let first = nobody.register(finish.clone(), StatusCode::OK).await;
    assert_eq!(first["user_id"], "@alice:example.org");
    assert_eq!(first["device_id"], "xyzzy");
    let alice = nobody.with(first["access_token"].as_str().unwrap());
    let device = alice
        .expect(
            Method::GET,
            "/_matrix/client/v3/devices/xyzzy",
            None,
            StatusCode::OK,
        )
        .await;
    assert_eq!(device["display_name"], "display_name");
    // ... and the same session finishing again logs the same account in again.
    let again = nobody.register(finish, StatusCode::OK).await;
    assert_eq!(again["user_id"], "@alice:example.org");
    assert_ne!(again["access_token"], first["access_token"]);

    let capabilities = alice
        .expect(
            Method::GET,
            "/_matrix/client/v3/capabilities",
            None,
            StatusCode::OK,
        )
        .await;
    assert_eq!(
        capabilities["capabilities"]["m.change_password"]["enabled"],
        true
    );

    // A stage without the session the username was handed is challenged again.
    let carol = nobody
        .register(
            json!({"username": "carol", "password": PASSWORD}),
            StatusCode::UNAUTHORIZED,
        )
        .await;
    let again = nobody
        .register(
            json!({"username": "carol", "password": PASSWORD, "auth": {"type": "m.login.dummy"}}),
            StatusCode::UNAUTHORIZED,
        )
        .await;
    assert_eq!(again["session"], carol["session"]);

    // The CAPTCHA, checked with the service: completed when right, refused when wrong.
    let right = nobody
        .register(
            json!({"username": "dave", "password": PASSWORD, "auth": {"type": "m.login.recaptcha", "response": "right"}}),
            StatusCode::UNAUTHORIZED,
        )
        .await;
    assert_eq!(right["completed"], json!(["m.login.recaptcha"]), "{right}");
    let wrong = nobody
        .register(
            json!({"username": "erin", "password": PASSWORD, "auth": {"type": "m.login.recaptcha", "response": "wrong"}}),
            StatusCode::UNAUTHORIZED,
        )
        .await;
    assert_eq!(wrong["errcode"], "M_FORBIDDEN", "{wrong}");

    // Bob has the same password as alice, as every Sytest account does.
    nobody
        .register(
            json!({"username": "bob", "password": PASSWORD, "auth": {"type": "m.login.dummy"}}),
            StatusCode::OK,
        )
        .await;
    let login = nobody
        .expect(
            Method::POST,
            "/_matrix/client/v3/login",
            Some(json!({
                "type": "m.login.password",
                "identifier": {"type": "m.id.user", "user": "alice"},
                "password": PASSWORD,
                "device_id": "SECOND",
            })),
            StatusCode::OK,
        )
        .await;
    assert_eq!(login["device_id"], "SECOND");
    // Alice's token with bob's password does not delete alice's device.
    let refused = alice
        .expect(
            Method::DELETE,
            "/_matrix/client/v3/devices/SECOND",
            Some(json!({"auth": password_auth("@bob:example.org", None)})),
            StatusCode::FORBIDDEN,
        )
        .await;
    assert_eq!(refused["errcode"], "M_FORBIDDEN");
    // A session started deleting SECOND does not delete xyzzy.
    let started = alice
        .expect(
            Method::DELETE,
            "/_matrix/client/v3/devices/SECOND",
            Some(json!({})),
            StatusCode::UNAUTHORIZED,
        )
        .await;
    let session = started["session"].as_str().unwrap();
    alice
        .expect(
            Method::DELETE,
            "/_matrix/client/v3/devices/xyzzy",
            Some(json!({"auth": password_auth("@alice:example.org", Some(session))})),
            StatusCode::FORBIDDEN,
        )
        .await;
    alice
        .expect(
            Method::GET,
            "/_matrix/client/v3/devices/xyzzy",
            None,
            StatusCode::OK,
        )
        .await;
    alice
        .expect(
            Method::DELETE,
            "/_matrix/client/v3/devices/SECOND",
            Some(json!({"auth": password_auth("alice", Some(session))})),
            StatusCode::OK,
        )
        .await;
    alice
        .expect(
            Method::GET,
            "/_matrix/client/v3/devices/SECOND",
            None,
            StatusCode::NOT_FOUND,
        )
        .await;

    // A wrong password at deactivation is a challenge with `completed`.
    let mut wrong_password = password_auth("@alice:example.org", None);
    wrong_password["password"] = json!("wrong password");
    let challenge = alice
        .expect(
            Method::POST,
            "/_matrix/client/v3/account/deactivate",
            Some(json!({"auth": wrong_password})),
            StatusCode::UNAUTHORIZED,
        )
        .await;
    for key in ["error", "errcode", "params", "completed", "flows"] {
        assert!(challenge.get(key).is_some(), "no {key}: {challenge}");
    }

    // An OpenID token, exchanged over the federation API.
    let openid = alice
        .expect(
            Method::POST,
            "/_matrix/client/v3/user/@alice:example.org/openid/request_token",
            Some(json!({})),
            StatusCode::OK,
        )
        .await;
    assert_eq!(openid["matrix_server_name"], "example.org");
    assert_eq!(openid["expires_in"], 3600);
    let token = openid["access_token"].as_str().unwrap();
    let info = nobody
        .expect(
            Method::GET,
            &format!("/_matrix/federation/v1/openid/userinfo?access_token={token}"),
            None,
            StatusCode::OK,
        )
        .await;
    assert_eq!(info["sub"], "@alice:example.org");
    nobody
        .expect(
            Method::GET,
            "/_matrix/federation/v1/openid/userinfo?access_token=an%2Finvalid%2Ftoken",
            None,
            StatusCode::UNAUTHORIZED,
        )
        .await;
    alice
        .expect(
            Method::POST,
            "/_matrix/client/v3/user/@bob:example.org/openid/request_token",
            Some(json!({})),
            StatusCode::FORBIDDEN,
        )
        .await;

    // Whois, about oneself: the device alice is using, last seen from here.
    let whois = alice
        .expect(
            Method::GET,
            "/_matrix/client/v3/admin/whois/@alice:example.org",
            None,
            StatusCode::OK,
        )
        .await;
    let connection = &whois["devices"]["xyzzy"]["sessions"][0]["connections"][0];
    assert_eq!(connection["ip"], "127.0.0.1", "{whois}");
    assert!(connection["last_seen"].is_u64(), "{whois}");
    assert!(connection.get("user_agent").is_some(), "{whois}");
    alice
        .expect(
            Method::GET,
            "/_matrix/client/v3/admin/whois/@bob:example.org",
            None,
            StatusCode::FORBIDDEN,
        )
        .await;

    // The shared-secret registration keeps to the user ID grammar.
    let nonce = nobody
        .expect(
            Method::GET,
            "/_synapse/admin/v1/register",
            None,
            StatusCode::OK,
        )
        .await;
    let nonce = nonce["nonce"].as_str().unwrap();
    let mac = hs_compat::shared_secret::compute_mac(
        SHARED_SECRET.as_bytes(),
        nonce,
        "us,er",
        PASSWORD,
        false,
        None,
    );
    let refused = nobody
        .expect(
            Method::POST,
            "/_synapse/admin/v1/register",
            Some(json!({"nonce": nonce, "username": "us,er", "password": PASSWORD, "admin": false, "mac": mac})),
            StatusCode::BAD_REQUEST,
        )
        .await;
    assert_eq!(refused["errcode"], "M_INVALID_USERNAME");

    let metrics = reqwest::get(format!("{}/metrics", nobody.base))
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert_eq!(
        metric(
            &metrics,
            "hs_auth_recaptcha_checks_total{outcome=\"passed\"}"
        ),
        1.0
    );
    assert_eq!(
        metric(
            &metrics,
            "hs_auth_recaptcha_checks_total{outcome=\"failed\"}"
        ),
        1.0
    );
    assert_eq!(metric(&metrics, "hs_auth_openid_tokens_issued_total"), 1.0);
    assert_eq!(metric(&metrics, "hs_auth_openid_tokens_checked_total"), 1.0);
}
