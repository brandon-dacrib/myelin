//! Self-service email addresses through the real `hs` binary and a real SMTP conversation: a
//! person registers with an email address this server validates (the validation email is read
//! off the wire, its link followed), signs in by that address with the legacy top-level
//! `medium`/`address`, adds a second address after user-interactive auth, deletes it, and
//! deactivates the account, after which the address signs nobody in. The counters say so.
//!
//! The SMTP server is the smallest one `lettre` will talk to, in this test (plain text, no
//! authentication): enough to receive what `hs` sends without Docker.

use std::sync::{Arc, Mutex};

use reqwest::{Method, StatusCode};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

/// Every message received, as the raw `DATA`.
#[derive(Clone, Default)]
struct Inbox(Arc<Mutex<Vec<String>>>);

impl Inbox {
    fn messages(&self) -> Vec<String> {
        self.0.lock().unwrap().clone()
    }
}

/// Accepts SMTP on 127.0.0.1 and records each message. Returns its port.
async fn smtp_server(inbox: Inbox) -> u16 {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let inbox = inbox.clone();
            tokio::spawn(async move {
                let (read, mut write) = stream.into_split();
                let mut lines = BufReader::new(read).lines();
                let _ = write.write_all(b"220 localhost ESMTP test\r\n").await;
                let mut in_data = false;
                let mut data = String::new();
                while let Ok(Some(line)) = lines.next_line().await {
                    if in_data {
                        if line == "." {
                            in_data = false;
                            inbox.0.lock().unwrap().push(std::mem::take(&mut data));
                            let _ = write.write_all(b"250 OK queued\r\n").await;
                        } else {
                            data.push_str(&line);
                            data.push_str("\r\n");
                        }
                        continue;
                    }
                    let verb = line
                        .split_whitespace()
                        .next()
                        .unwrap_or("")
                        .to_ascii_uppercase();
                    let reply: &[u8] = match verb.as_str() {
                        "EHLO" | "HELO" => b"250 localhost\r\n",
                        "DATA" => {
                            in_data = true;
                            b"354 go ahead\r\n"
                        }
                        "QUIT" => {
                            let _ = write.write_all(b"221 bye\r\n").await;
                            break;
                        }
                        _ => b"250 OK\r\n",
                    };
                    let _ = write.write_all(reply).await;
                }
            });
        }
    });
    port
}

/// The first `http` link in a quoted-printable message.
fn link_in(message: &str) -> String {
    let unfolded = message.replace("=\r\n", "");
    let start = unfolded.find("http").expect("a link in the email");
    unfolded[start..]
        .split_whitespace()
        .next()
        .unwrap()
        .replace("=3D", "=")
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

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

    /// Reads the log until a line contains `needle`; 120 s bounds a slow boot under load.
    fn wait_for(&mut self, needle: &str) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
        loop {
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            match self.lines.recv_timeout(left) {
                Ok(line) => {
                    let found = line.contains(needle);
                    self.seen.push(line);
                    if found {
                        return;
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

struct Caller {
    base: String,
    token: Option<String>,
}

impl Caller {
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

    async fn expect(&self, method: Method, path: &str, body: Option<Value>) -> Value {
        let (status, body) = self.call(method, path, body).await;
        assert!(status.is_success(), "{path}: {status} {body}");
        body
    }
}

/// Asks for a validation email for `address` at `path`, follows its link, and returns the sid.
async fn validate(
    nobody: &Caller,
    inbox: &Inbox,
    path: &str,
    address: &str,
    purpose: &str,
) -> String {
    let before = inbox.messages().len();
    let sid = nobody
        .expect(
            Method::POST,
            path,
            Some(
                json!({"client_secret": "clientSECRET.=_-", "email": address,
                        "send_attempt": 1, "id_server": "id.example"}),
            ),
        )
        .await["sid"]
        .as_str()
        .unwrap()
        .to_owned();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    let message = loop {
        if let Some(m) = inbox.messages().get(before) {
            break m.clone();
        }
        assert!(
            std::time::Instant::now() < deadline,
            "no validation email arrived"
        );
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    };
    assert!(message.contains(address), "sent to {address}:\n{message}");
    let link = link_in(&message);
    assert!(
        link.starts_with(&format!(
            "{}/_matrix/client/unstable/{purpose}/email/submit_token?",
            nobody.base
        )),
        "{link}"
    );
    let page = reqwest::get(&link).await.unwrap();
    assert_eq!(page.status(), StatusCode::OK, "following {link}");
    assert!(page.text().await.unwrap().contains("validated"));
    sid
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_email_address_is_validated_registered_with_signed_in_by_added_and_removed() {
    let inbox = Inbox::default();
    let smtp_port = smtp_server(inbox.clone()).await;
    let dir = tempfile::tempdir().unwrap();
    let port = free_port();
    let base = format!("http://127.0.0.1:{port}");
    let config_path = dir.path().join("homeserver.yaml");
    let data = dir.path().join("data");
    std::fs::write(
        &config_path,
        format!(
            "server:\n  server_name: example.org\n  public_baseurl: {base}/\n\
             listeners:\n  listeners:\n    - port: {port}\n      bind_addresses: [\"127.0.0.1\"]\n      resources: [client, health, metrics]\n\
             storage:\n  backend: embedded\n  data_dir: {data:?}\n\
             media:\n  storage:\n    backend: local\n    path: {:?}\n\
             auth:\n  enable_registration: true\n\
             email:\n  smtp:\n    host: 127.0.0.1\n    port: {smtp_port}\n    security: none\n\
             \x20 from: \"Myelin <noreply@example.org>\"\n  app_name: Myelin\n",
            data.join("media"),
        ),
    )
    .unwrap();
    let mut server = HsProcess::serve(&config_path);
    server.wait_for("setup_link=");
    let nobody = Caller {
        base: base.clone(),
        token: None,
    };

    // Registration offers the email flow, and completes with a validated address.
    let (status, challenge) = nobody
        .call(
            Method::POST,
            "/_matrix/client/v3/register",
            Some(json!({"username": "alice", "password": "alice-password-1"})),
        )
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert!(
        challenge["flows"]
            .as_array()
            .unwrap()
            .iter()
            .any(|f| f["stages"] == json!(["m.login.email.identity"])),
        "{challenge}"
    );
    let session = challenge["session"].as_str().unwrap();
    let sid = validate(
        &nobody,
        &inbox,
        "/_matrix/client/v3/register/email/requestToken",
        "alice@example.com",
        "registration",
    )
    .await;
    let registered = nobody
        .expect(
            Method::POST,
            "/_matrix/client/v3/register",
            Some(json!({
                "username": "alice", "password": "alice-password-1",
                "auth": {"type": "m.login.email.identity", "session": session,
                         "threepid_creds": {"sid": sid, "client_secret": "clientSECRET.=_-"}},
            })),
        )
        .await;
    let alice = Caller {
        base: base.clone(),
        token: Some(registered["access_token"].as_str().unwrap().to_owned()),
    };
    let listed = alice
        .expect(Method::GET, "/_matrix/client/v3/account/3pid", None)
        .await;
    assert_eq!(listed["threepids"][0]["address"], "alice@example.com");

    // The address signs in, the legacy way Sytest uses.
    let login = nobody
        .expect(
            Method::POST,
            "/_matrix/client/v3/login",
            Some(json!({"type": "m.login.password", "medium": "email",
                        "address": "Alice@Example.com", "password": "alice-password-1"})),
        )
        .await;
    assert_eq!(login["user_id"], "@alice:example.org");

    // A second address, added after user-interactive auth, then deleted.
    let sid = validate(
        &nobody,
        &inbox,
        "/_matrix/client/v3/account/3pid/email/requestToken",
        "alice2@example.com",
        "add_threepid",
    )
    .await;
    let add = json!({"sid": sid, "client_secret": "clientSECRET.=_-"});
    let (status, _) = alice
        .call(
            Method::POST,
            "/_matrix/client/v3/account/3pid/add",
            Some(add.clone()),
        )
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let mut with_auth = add;
    with_auth["auth"] = json!({"type": "m.login.password", "password": "alice-password-1",
                               "identifier": {"type": "m.id.user", "user": "alice"}});
    alice
        .expect(
            Method::POST,
            "/_matrix/client/v3/account/3pid/add",
            Some(with_auth),
        )
        .await;
    let listed = alice
        .expect(Method::GET, "/_matrix/client/v3/account/3pid", None)
        .await;
    assert_eq!(listed["threepids"].as_array().unwrap().len(), 2);
    let deleted = alice
        .expect(
            Method::POST,
            "/_matrix/client/unstable/account/3pid/delete",
            Some(json!({"medium": "email", "address": "alice2@example.com"})),
        )
        .await;
    assert_eq!(deleted["id_server_unbind_result"], "no-support");

    // Phone numbers are not validated by this server.
    let (status, refused) = nobody
        .call(
            Method::POST,
            "/_matrix/client/v3/register/msisdn/requestToken",
            Some(
                json!({"client_secret": "s", "country": "GB", "phone_number": "1",
                        "send_attempt": 1}),
            ),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(refused["errcode"], "M_THREEPID_MEDIUM_NOT_SUPPORTED");

    // Deactivated, the address signs nobody in.
    let deactivated = alice
        .expect(
            Method::POST,
            "/_matrix/client/v3/account/deactivate",
            Some(
                json!({"auth": {"type": "m.login.password", "password": "alice-password-1",
                                 "identifier": {"type": "m.id.user", "user": "alice"}}}),
            ),
        )
        .await;
    assert_eq!(deactivated["id_server_unbind_result"], "success");
    let (status, _) = nobody
        .call(
            Method::POST,
            "/_matrix/client/v3/login",
            Some(json!({"type": "m.login.password", "medium": "email",
                        "address": "alice@example.com", "password": "alice-password-1"})),
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    let metrics = reqwest::get(format!("{base}/metrics"))
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(
        metrics.contains(r#"hs_auth_threepid_validations_total{medium="email",outcome="sent"} 2"#),
        "{metrics}"
    );
    assert!(
        metrics.contains(
            r#"hs_auth_threepid_validations_total{medium="email",outcome="validated"} 2"#
        ),
        "validated counter"
    );
    assert!(
        metrics.contains(r#"hs_auth_threepid_changes_total{action="added"} 2"#),
        "added counter"
    );
}

/// Element's "Forgot password?" against the real binary: signed out, `POST /account/password`
/// asks for `m.login.email.identity`; the reset email's link first shows a confirmation page
/// (a mail scanner fetching it validates nothing), its button sends the browser on to the
/// client's `next_link`; the reset then sets the new password and signs every session out.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_forgotten_password_is_reset_by_email() {
    let inbox = Inbox::default();
    let smtp_port = smtp_server(inbox.clone()).await;
    let dir = tempfile::tempdir().unwrap();
    let port = free_port();
    let base = format!("http://127.0.0.1:{port}");
    let config_path = dir.path().join("homeserver.yaml");
    let data = dir.path().join("data");
    std::fs::write(
        &config_path,
        format!(
            "server:\n  server_name: example.org\n  public_baseurl: {base}/\n\
             listeners:\n  listeners:\n    - port: {port}\n      bind_addresses: [\"127.0.0.1\"]\n      resources: [client, health, metrics]\n\
             storage:\n  backend: embedded\n  data_dir: {data:?}\n\
             media:\n  storage:\n    backend: local\n    path: {:?}\n\
             auth:\n  enable_registration: true\n  next_link_domain_whitelist: [app.example]\n\
             email:\n  smtp:\n    host: 127.0.0.1\n    port: {smtp_port}\n    security: none\n\
             \x20 from: \"Myelin <noreply@example.org>\"\n  app_name: Myelin\n",
            data.join("media"),
        ),
    )
    .unwrap();
    let mut server = HsProcess::serve(&config_path);
    server.wait_for("setup_link=");
    let nobody = Caller {
        base: base.clone(),
        token: None,
    };

    // Carol registers with her address.
    let (_, challenge) = nobody
        .call(
            Method::POST,
            "/_matrix/client/v3/register",
            Some(json!({"username": "carol", "password": "old-password-1"})),
        )
        .await;
    let session = challenge["session"].as_str().unwrap().to_owned();
    let sid = validate(
        &nobody,
        &inbox,
        "/_matrix/client/v3/register/email/requestToken",
        "carol@example.com",
        "registration",
    )
    .await;
    let registered = nobody
        .expect(
            Method::POST,
            "/_matrix/client/v3/register",
            Some(json!({
                "username": "carol", "password": "old-password-1",
                "auth": {"type": "m.login.email.identity", "session": session,
                         "threepid_creds": {"sid": sid, "client_secret": "clientSECRET.=_-"}},
            })),
        )
        .await;
    let carol = Caller {
        base: base.clone(),
        token: Some(registered["access_token"].as_str().unwrap().to_owned()),
    };

    // Signed out, the challenge is the email flow.
    let (status, challenge) = nobody
        .call(
            Method::POST,
            "/_matrix/client/v3/account/password",
            Some(json!({"new_password": "new-password-2"})),
        )
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{challenge}");
    assert_eq!(
        challenge["flows"],
        json!([{"stages": ["m.login.email.identity"]}])
    );
    let session = challenge["session"].as_str().unwrap().to_owned();

    // A next_link on a host the operator did not list is refused before any email goes.
    let ask = |next_link: &str| {
        json!({"client_secret": "resetSECRET", "email": "carol@example.com",
               "send_attempt": 1, "next_link": next_link})
    };
    let (status, refused) = nobody
        .call(
            Method::POST,
            "/_matrix/client/v3/account/password/email/requestToken",
            Some(ask("https://elsewhere.example/")),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(refused["errcode"], "M_INVALID_PARAM");
    let before = inbox.messages().len();
    let sid = nobody
        .expect(
            Method::POST,
            "/_matrix/client/v3/account/password/email/requestToken",
            Some(ask("https://app.example/reset-done")),
        )
        .await["sid"]
        .as_str()
        .unwrap()
        .to_owned();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    let message = loop {
        if let Some(m) = inbox.messages().get(before) {
            break m.clone();
        }
        assert!(
            std::time::Instant::now() < deadline,
            "no reset email arrived"
        );
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    };
    let link = link_in(&message);
    assert!(
        link.starts_with(&format!(
            "{base}/_matrix/client/unstable/password_reset/email/submit_token?"
        )),
        "{link}"
    );
    let reset = json!({
        "auth": {"type": "m.login.email.identity", "session": session,
                 "threepid_creds": {"sid": sid, "client_secret": "resetSECRET"}},
    });

    // Fetching the link only shows the confirmation page.
    let page = reqwest::get(&link).await.unwrap();
    assert_eq!(page.status(), StatusCode::OK);
    assert!(page.text().await.unwrap().contains("method=\"post\""));
    let (status, _) = nobody
        .call(
            Method::POST,
            "/_matrix/client/v3/account/password",
            Some(reset.clone()),
        )
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "not validated yet");

    // Confirmed: the browser goes on to the client's page.
    let confirmed = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap()
        .post(&link)
        .send()
        .await
        .unwrap();
    assert_eq!(confirmed.status(), StatusCode::FOUND);
    assert_eq!(
        confirmed.headers()["location"],
        "https://app.example/reset-done"
    );

    // The reset completes with the password the first round sent, and signs Carol out.
    nobody
        .expect(
            Method::POST,
            "/_matrix/client/v3/account/password",
            Some(reset),
        )
        .await;
    let (status, _) = carol
        .call(Method::GET, "/_matrix/client/v3/account/whoami", None)
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "the old session is gone");
    let (status, _) = nobody
        .call(
            Method::POST,
            "/_matrix/client/v3/login",
            Some(json!({"type": "m.login.password", "identifier": {"type": "m.id.user", "user": "carol"},
                        "password": "old-password-1"})),
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "the old password is refused");
    let login = nobody
        .expect(
            Method::POST,
            "/_matrix/client/v3/login",
            Some(json!({"type": "m.login.password", "identifier": {"type": "m.id.user", "user": "carol"},
                        "password": "new-password-2"})),
        )
        .await;
    assert_eq!(login["user_id"], "@carol:example.org");

    let metrics = reqwest::get(format!("{base}/metrics"))
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(
        metrics.contains(r#"hs_auth_password_resets_total{outcome="reset"} 1"#),
        "{metrics}"
    );
    server.wait_for("a password was reset by email");
}
