//! Single sign-on through CAS against the real `hs` binary, with a CAS server played by this
//! test on 127.0.0.1: `GET /login` lists `m.login.sso` and `m.login.cas`; the redirect sends the
//! browser to the CAS login page with the `r0` ticket endpoint as the service; the ticket is
//! checked at `/proxyValidate` with that same service; a new CAS user gets an account and a
//! page carrying a login token, which `POST /login` (`m.login.token`) trades for a session; and
//! user-interactive auth's `m.login.sso` stage, passed through the same ticket endpoint with a
//! `session`, deletes a device as the account CAS vouched for and refuses it as anybody else.
//! `/metrics` counts each outcome. The same steps as Sytest's `12login/02cas.pl` and the SSO
//! tests of `10apidoc/13ui-auth.pl`, every one of which failed before: `GET /login` offered no
//! SSO and the routes did not exist.

use std::sync::{Arc, Mutex};

use reqwest::{Method, StatusCode};
use serde_json::{Value, json};

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// The real `hs serve`, killed when dropped.
struct HsProcess(std::process::Child);

impl Drop for HsProcess {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// What the fake CAS server answers and what it was asked.
#[derive(Default)]
struct FakeCas {
    user: Mutex<String>,
    validations: Mutex<Vec<(String, String)>>,
}

async fn start_fake_cas(fake: Arc<FakeCas>) -> u16 {
    use axum::extract::{Query, State};
    use std::collections::HashMap;
    let app = axum::Router::new()
        .route("/cas/login", axum::routing::get(|| async { "CAS login page" }))
        .route(
            "/cas/proxyValidate",
            axum::routing::get(
                |State(fake): State<Arc<FakeCas>>, Query(q): Query<HashMap<String, String>>| async move {
                    fake.validations.lock().unwrap().push((
                        q.get("ticket").cloned().unwrap_or_default(),
                        q.get("service").cloned().unwrap_or_default(),
                    ));
                    let user = fake.user.lock().unwrap().clone();
                    format!(
                        "<cas:serviceResponse xmlns:cas='http://www.yale.edu/tp/cas'>\n\
                         <cas:authenticationSuccess><cas:user>{user}</cas:user>\
                         <cas:attributes><cas:displayName>Casey User</cas:displayName></cas:attributes>\
                         </cas:authenticationSuccess></cas:serviceResponse>"
                    )
                },
            ),
        )
        .with_state(fake);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    port
}

struct Client {
    base: String,
    http: reqwest::Client,
}

impl Client {
    async fn json(
        &self,
        method: Method,
        path: &str,
        token: Option<&str>,
        body: Option<Value>,
    ) -> (StatusCode, Value) {
        let mut request = self.http.request(method, format!("{}{path}", self.base));
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

    async fn raw(&self, url: &str) -> reqwest::Response {
        self.http.get(url).send().await.unwrap()
    }

    /// Logs in through CAS as whoever the fake server says, on `device_id`.
    async fn cas_login(&self, fake: &FakeCas, client_url: &str, device_id: &str) -> Value {
        let redirect = self
            .raw(&format!(
                "{}/_matrix/client/v3/login/sso/redirect?redirectUrl={}",
                self.base,
                urlencode(client_url)
            ))
            .await;
        assert_eq!(redirect.status(), StatusCode::FOUND);
        let location = redirect
            .headers()
            .get("location")
            .unwrap()
            .to_str()
            .unwrap()
            .to_owned();
        let location = reqwest::Url::parse(&location).unwrap();
        assert_eq!(location.path(), "/cas/login");
        let service = location
            .query_pairs()
            .find(|(k, _)| k == "service")
            .unwrap()
            .1
            .into_owned();
        assert_eq!(
            service,
            format!(
                "{}/_matrix/client/r0/login/cas/ticket?redirectUrl={}",
                self.base,
                urlencode(client_url)
            )
        );

        // CAS sends the browser back to the service with a ticket.
        let page = self.raw(&format!("{service}&ticket=goldenticket")).await;
        assert_eq!(page.status(), StatusCode::OK);
        assert!(
            page.headers()["content-type"]
                .to_str()
                .unwrap()
                .starts_with("text/html")
        );
        let html = page.text().await.unwrap();
        assert_eq!(
            fake.validations.lock().unwrap().last().unwrap(),
            &("goldenticket".to_owned(), service.clone()),
            "the ticket is checked with the service exactly as it was sent"
        );
        let token = html
            .split("loginToken=")
            .nth(1)
            .and_then(|rest| rest.split(['"', '&']).next())
            .unwrap_or_else(|| panic!("no login token in the page:\n{html}"))
            .to_owned();

        let (status, login) = self
            .json(
                Method::POST,
                "/_matrix/client/v3/login",
                None,
                Some(json!({"type": "m.login.token", "token": token, "device_id": device_id})),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{login}");
        login
    }
}

fn urlencode(raw: &str) -> String {
    raw.bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
                char::from(b).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}

#[tokio::test]
async fn cas_signs_people_in_creates_accounts_and_confirms_user_interactive_auth() {
    let fake = Arc::new(FakeCas::default());
    let cas_port = start_fake_cas(fake.clone()).await;
    let dir = tempfile::tempdir().unwrap();
    let port = free_port();
    let config_path = dir.path().join("hs.yaml");
    std::fs::write(
        &config_path,
        format!(
            "server:\n  server_name: example.org\n  public_baseurl: http://127.0.0.1:{port}/\n\
             listeners:\n  listeners:\n    - port: {port}\n      bind_addresses: [\"127.0.0.1\"]\n      resources: [client, health, metrics]\n\
             storage:\n  backend: embedded\n  data_dir: {:?}\n\
             media:\n  storage:\n    backend: local\n    path: {:?}\n\
             federation:\n  enabled: false\n\
             rate_limits:\n  enabled: false\n\
             auth:\n  enable_registration: true\n  cas:\n    server_url: http://127.0.0.1:{cas_port}/cas\n    displayname_attribute: displayName\n",
            dir.path().join("db"),
            dir.path().join("media"),
        ),
    )
    .unwrap();
    let _hs = HsProcess(
        std::process::Command::new(env!("CARGO_BIN_EXE_hs"))
            .args(["serve", "-c"])
            .arg(&config_path)
            .env_remove("RUST_LOG")
            .env_remove("HS_DATA_DIR")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::inherit())
            .spawn()
            .expect("the hs binary should start"),
    );
    let client = Client {
        base: format!("http://127.0.0.1:{port}"),
        http: reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap(),
    };
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
    loop {
        if let Ok(r) = client
            .http
            .get(format!("{}/_matrix/client/versions", client.base))
            .send()
            .await
            && r.status() == StatusCode::OK
        {
            break;
        }
        assert!(std::time::Instant::now() < deadline, "hs never started");
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    }

    // `GET /login` offers single sign-on and the old CAS type.
    let (_, flows) = client
        .json(Method::GET, "/_matrix/client/v3/login", None, None)
        .await;
    let flows = flows["flows"].as_array().unwrap().clone();
    let sso = flows
        .iter()
        .find(|f| f["type"] == "m.login.sso")
        .expect("m.login.sso is offered");
    assert_eq!(sso["identity_providers"][0]["id"], "cas");
    assert!(flows.iter().any(|f| f["type"] == "m.login.cas"));

    // A new CAS user: an account named after them, with their CAS display name.
    *fake.user.lock().unwrap() = "cas_user!".to_owned();
    let login = client
        .cas_login(&fake, "https://client?p=http%3A%2F%2Fserver", "CASDEV")
        .await;
    assert_eq!(login["user_id"], "@cas_user=21:example.org");
    let (_, name) = client
        .json(
            Method::GET,
            "/_matrix/client/v3/profile/@cas_user=21:example.org/displayname",
            None,
            None,
        )
        .await;
    assert_eq!(name["displayname"], "Casey User");

    // A password account whose localpart CAS also knows signs in as itself through CAS, and
    // can confirm a device deletion through CAS -- as itself, not as anybody else.
    let (status, alice) = client
        .json(
            Method::POST,
            "/_matrix/client/v3/register",
            None,
            Some(json!({"username": "alice", "password": "a-long-password", "auth": {"type": "m.login.dummy"}})),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{alice}");
    let alice_token = alice["access_token"].as_str().unwrap().to_owned();
    *fake.user.lock().unwrap() = "alice".to_owned();
    let second = client.cas_login(&fake, "https://client", "ALICE2").await;
    assert_eq!(second["user_id"], "@alice:example.org");

    for (cas_user, want) in [("bob", StatusCode::FORBIDDEN), ("alice", StatusCode::OK)] {
        let (status, challenge) = client
            .json(
                Method::DELETE,
                "/_matrix/client/v3/devices/ALICE2",
                Some(&alice_token),
                Some(json!({})),
            )
            .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{challenge}");
        assert!(
            challenge["flows"]
                .as_array()
                .unwrap()
                .iter()
                .any(|f| f["stages"][0] == "m.login.sso"),
            "{challenge}"
        );
        let session = challenge["session"].as_str().unwrap().to_owned();
        *fake.user.lock().unwrap() = cas_user.to_owned();
        let page = client
            .raw(&format!(
                "{}/_matrix/client/r0/login/cas/ticket?session={session}&ticket=t",
                client.base
            ))
            .await;
        assert_eq!(page.status(), StatusCode::OK);
        let (status, body) = client
            .json(
                Method::DELETE,
                "/_matrix/client/v3/devices/ALICE2",
                Some(&alice_token),
                Some(json!({"auth": {"session": session}})),
            )
            .await;
        assert_eq!(status, want, "confirmed through CAS as {cas_user}: {body}");
    }
    let (status, _) = client
        .json(
            Method::GET,
            "/_matrix/client/v3/devices/ALICE2",
            Some(&alice_token),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let metrics = client
        .http
        .get(format!("{}/metrics", client.base))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    for (outcome, count) in [
        ("registered", "1"),
        ("login", "1"),
        ("ui_auth", "1"),
        ("ui_auth_mismatch", "1"),
    ] {
        let line =
            format!("hs_auth_sso_logins_total{{provider=\"cas\",outcome=\"{outcome}\"}} {count}");
        assert!(metrics.contains(&line), "{line} not in /metrics");
    }
}
