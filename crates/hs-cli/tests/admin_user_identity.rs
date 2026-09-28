//! The devices-and-identity half of a user's page, through the real `hs` binary: each admin
//! operation is checked by what it does to the person on the other side, not only by what the
//! admin API answers.
//!
//! - A device renamed by an administrator is renamed for its owner; devices signed out in bulk
//!   stop authenticating at once, their keys are no longer served by `/keys/query`, and a bulk
//!   request naming a device the user does not have changes nothing.
//! - A 3PID bound by an administrator signs its owner in (`m.id.thirdparty`), is listed by
//!   `GET /account/3pid`, finds the account through `users.lookup`, cannot be bound to a second
//!   account, and signs nobody in once removed.
//! - An external id finds its account through `users.lookup` and cannot be linked twice.
//! - Experimental features are validated, merged and reported.
//! - Account data and pushers a client stored are what the admin API lists.
//! - Every write is on the audit record, and 3PIDs, external ids and experimental features
//!   outlive a restart.

use reqwest::{Method, StatusCode};
use serde_json::{Value, json};

fn config_yaml(port: u16, data_dir: &std::path::Path) -> String {
    format!(
        "server:\n  server_name: example.org\n\
         listeners:\n  listeners:\n    - port: {port}\n      bind_addresses: [\"127.0.0.1\"]\n      resources: [client, health, metrics]\n\
         storage:\n  backend: embedded\n  data_dir: {:?}\n\
         media:\n  storage:\n    backend: local\n    path: {:?}\n\
         auth:\n  enable_registration: true\n",
        data_dir,
        data_dir.join("media"),
    )
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// One run of the real `hs` binary (the same harness as `reports_tasks_statistics.rs`). A
/// restart has to be a new process: an in-process server's background tasks keep the store's
/// lock until the process exits.
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

    /// Reads the log until a line contains `needle`.
    fn wait_for(&mut self, needle: &str) -> String {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
        loop {
            let left = deadline.saturating_duration_since(std::time::Instant::now());
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

    /// Everything the server logged so far, and whatever it logs in the next moment.
    fn log(&mut self) -> String {
        while let Ok(line) = self
            .lines
            .recv_timeout(std::time::Duration::from_millis(200))
        {
            self.seen.push(line);
        }
        self.seen.join("\n")
    }

    fn stop(mut self) {
        let pid = self.child.id().to_string();
        let _ = std::process::Command::new("kill")
            .args(["-TERM", &pid])
            .status();
        let _ = self.child.wait();
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

    async fn get(&self, path: &str) -> Value {
        self.expect(Method::GET, path, None, StatusCode::OK).await
    }
}

const ALICE: &str = "@alice:example.org";
const ALICE_PATH: &str = "%40alice%3Aexample.org";
const PASSWORD: &str = "hunter2-alice";

/// Signs alice in on `device_id`; the new session.
async fn sign_in(nobody: &Caller, device_id: &str, display_name: &str) -> Caller {
    let body = nobody
        .expect(
            Method::POST,
            "/_matrix/client/v3/login",
            Some(json!({
                "type": "m.login.password",
                "identifier": {"type": "m.id.user", "user": "alice"},
                "password": PASSWORD,
                "device_id": device_id,
                "initial_device_display_name": display_name,
            })),
            StatusCode::OK,
        )
        .await;
    Caller {
        base: nobody.base.clone(),
        token: Some(body["access_token"].as_str().unwrap().to_owned()),
    }
}

async fn upload_keys(session: &Caller, device_id: &str) {
    session
        .expect(
            Method::POST,
            "/_matrix/client/v3/keys/upload",
            Some(json!({
                "device_keys": {
                    "user_id": ALICE,
                    "device_id": device_id,
                    "algorithms": ["m.olm.v1.curve25519-aes-sha2", "m.megolm.v1.aes-sha2"],
                    "keys": {
                        format!("curve25519:{device_id}"): format!("curve-{device_id}"),
                        format!("ed25519:{device_id}"): format!("ed-{device_id}"),
                    },
                    "signatures": {ALICE: {format!("ed25519:{device_id}"): "sig"}},
                }
            })),
            StatusCode::OK,
        )
        .await;
}

async fn keyed_devices(session: &Caller) -> Vec<String> {
    let body = session
        .expect(
            Method::POST,
            "/_matrix/client/v3/keys/query",
            Some(json!({"device_keys": {ALICE: []}})),
            StatusCode::OK,
        )
        .await;
    let mut devices: Vec<String> = body["device_keys"][ALICE]
        .as_object()
        .map(|m| m.keys().cloned().collect())
        .unwrap_or_default();
    devices.sort();
    devices
}

async fn audited(admin: &Caller, action: &str) -> Value {
    let audit = admin
        .get(&format!("/api/v1/audit-log?action={action}"))
        .await;
    let entry = audit["items"][0].clone();
    assert_eq!(
        entry["actor"]["id"], "@ops:example.org",
        "{action}: {audit}"
    );
    assert_eq!(entry["target"]["id"], ALICE, "{action}: {audit}");
    entry
}

async fn email_login(nobody: &Caller, address: &str) -> StatusCode {
    nobody
        .call(
            Method::POST,
            "/_matrix/client/v3/login",
            Some(json!({
                "type": "m.login.password",
                "identifier": {"type": "m.id.thirdparty", "medium": "email", "address": address},
                "password": PASSWORD,
            })),
        )
        .await
        .0
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_users_devices_and_identity_through_the_real_server() {
    let dir = tempfile::tempdir().unwrap();
    let port = free_port();
    let config_path = dir.path().join("homeserver.yaml");
    std::fs::write(&config_path, config_yaml(port, &dir.path().join("data"))).unwrap();
    let mut server = HsProcess::serve(&config_path);
    let line = server.wait_for("setup_link=");
    let setup_token: String = line
        .split_once("/admin/setup#token=")
        .unwrap()
        .1
        .chars()
        .take_while(char::is_ascii_alphabetic)
        .collect();
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
    let admin = Caller {
        base: nobody.base.clone(),
        token: Some(session["access_token"].as_str().unwrap().to_owned()),
    };
    for name in ["alice", "bob"] {
        nobody
            .expect(
                Method::POST,
                "/_matrix/client/v3/register",
                Some(json!({"username": name, "password": format!("hunter2-{name}"), "auth": {"type": "m.login.dummy"}})),
                StatusCode::OK,
            )
            .await;
    }

    // Two devices, each with keys.
    let phone = sign_in(&nobody, "PHONE", "Alice's phone").await;
    let laptop = sign_in(&nobody, "LAPTOP", "Alice's laptop").await;
    upload_keys(&phone, "PHONE").await;
    upload_keys(&laptop, "LAPTOP").await;
    assert_eq!(keyed_devices(&phone).await, ["LAPTOP", "PHONE"]);

    // ---- users.devices.get / update ----
    let device = admin
        .get(&format!("/api/v1/users/{ALICE_PATH}/devices/LAPTOP"))
        .await;
    assert_eq!(device["display_name"], "Alice's laptop", "{device}");
    admin
        .expect(
            Method::GET,
            &format!("/api/v1/users/{ALICE_PATH}/devices/NOPE"),
            None,
            StatusCode::NOT_FOUND,
        )
        .await;
    let renamed = admin
        .expect(
            Method::PATCH,
            &format!("/api/v1/users/{ALICE_PATH}/devices/LAPTOP"),
            Some(json!({"display_name": "Lost laptop"})),
            StatusCode::OK,
        )
        .await;
    assert_eq!(renamed["display_name"], "Lost laptop");
    // Renamed for its owner too.
    let own = phone.get("/_matrix/client/v3/devices/LAPTOP").await;
    assert_eq!(own["display_name"], "Lost laptop", "{own}");
    let entry = audited(&admin, "users.devices.update").await;
    assert_eq!(entry["changes"][0]["from"], "Alice's laptop", "{entry}");
    assert_eq!(entry["changes"][0]["to"], "Lost laptop", "{entry}");

    // ---- users.devices.bulk_delete ----
    // Naming a device she does not have changes nothing.
    admin
        .expect(
            Method::POST,
            &format!("/api/v1/users/{ALICE_PATH}/devices/bulk-delete"),
            Some(json!({"device_ids": ["LAPTOP", "NOPE"]})),
            StatusCode::NOT_FOUND,
        )
        .await;
    laptop.get("/_matrix/client/v3/account/whoami").await;
    admin
        .expect(
            Method::POST,
            &format!("/api/v1/users/{ALICE_PATH}/devices/bulk-delete"),
            Some(json!({"device_ids": []})),
            StatusCode::BAD_REQUEST,
        )
        .await;
    admin
        .expect(
            Method::POST,
            &format!("/api/v1/users/{ALICE_PATH}/devices/bulk-delete"),
            Some(json!({"device_ids": ["LAPTOP"]})),
            StatusCode::NO_CONTENT,
        )
        .await;
    // The laptop can no longer authenticate; the phone still can.
    let (status, body) = laptop
        .call(Method::GET, "/_matrix/client/v3/account/whoami", None)
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");
    assert_eq!(body["errcode"], "M_UNKNOWN_TOKEN");
    phone.get("/_matrix/client/v3/account/whoami").await;
    // And nobody is handed its keys again.
    assert_eq!(keyed_devices(&phone).await, ["PHONE"]);
    let devices = admin
        .get(&format!("/api/v1/users/{ALICE_PATH}/devices"))
        .await;
    let remaining: Vec<&str> = devices["items"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|d| d["device_id"].as_str())
        .collect();
    assert!(remaining.contains(&"PHONE"), "{devices}");
    assert!(!remaining.contains(&"LAPTOP"), "{devices}");
    let entry = audited(&admin, "users.devices.bulk_delete").await;
    assert_eq!(entry["outcome"]["status"], 200, "{entry}");

    // ---- users.threepids.* ----
    assert_eq!(
        email_login(&nobody, "alice@example.org").await,
        StatusCode::FORBIDDEN
    );
    let added = admin
        .expect(
            Method::POST,
            &format!("/api/v1/users/{ALICE_PATH}/threepids"),
            Some(json!({"medium": "email", "address": " Alice@Example.org "})),
            StatusCode::CREATED,
        )
        .await;
    assert_eq!(added["address"], "alice@example.org", "{added}");
    assert!(added["added_at"].is_string(), "{added}");
    admin
        .expect(
            Method::POST,
            &format!("/api/v1/users/{ALICE_PATH}/threepids"),
            Some(json!({"medium": "email", "address": "not an address"})),
            StatusCode::BAD_REQUEST,
        )
        .await;
    // Nobody else may have it.
    admin
        .expect(
            Method::POST,
            "/api/v1/users/%40bob%3Aexample.org/threepids",
            Some(json!({"medium": "email", "address": "alice@example.org"})),
            StatusCode::CONFLICT,
        )
        .await;
    let listed = admin
        .get(&format!("/api/v1/users/{ALICE_PATH}/threepids"))
        .await;
    assert_eq!(listed.as_array().unwrap().len(), 1, "{listed}");
    // It signs her in, her client lists it, and it finds her.
    assert_eq!(
        email_login(&nobody, "ALICE@example.org").await,
        StatusCode::OK
    );
    let own = phone.get("/_matrix/client/v3/account/3pid").await;
    assert_eq!(own["threepids"][0]["address"], "alice@example.org", "{own}");
    assert!(own["threepids"][0]["validated_at"].is_u64(), "{own}");
    let found = admin
        .get("/api/v1/users/lookup?medium=email&address=alice%40example.org")
        .await;
    assert_eq!(found["user_id"], ALICE, "{found}");
    audited(&admin, "users.threepids.add").await;

    // ---- users.external_ids.* ----
    let linked = admin
        .expect(
            Method::POST,
            &format!("/api/v1/users/{ALICE_PATH}/external-ids"),
            Some(json!({"provider": "oidc-corp", "external_id": "248289761001"})),
            StatusCode::CREATED,
        )
        .await;
    assert_eq!(linked["external_id"], "248289761001");
    admin
        .expect(
            Method::POST,
            "/api/v1/users/%40bob%3Aexample.org/external-ids",
            Some(json!({"provider": "oidc-corp", "external_id": "248289761001"})),
            StatusCode::CONFLICT,
        )
        .await;
    let found = admin
        .get("/api/v1/users/lookup?provider=oidc-corp&external_id=248289761001")
        .await;
    assert_eq!(found["user_id"], ALICE, "{found}");
    audited(&admin, "users.external_ids.add").await;

    // ---- users.experimental_features.* ----
    let features = admin
        .get(&format!("/api/v1/users/{ALICE_PATH}/experimental-features"))
        .await;
    assert_eq!(features["msc3881"], false, "{features}");
    admin
        .expect(
            Method::PUT,
            &format!("/api/v1/users/{ALICE_PATH}/experimental-features"),
            Some(json!({"msc9999": true})),
            StatusCode::BAD_REQUEST,
        )
        .await;
    let features = admin
        .expect(
            Method::PUT,
            &format!("/api/v1/users/{ALICE_PATH}/experimental-features"),
            Some(json!({"msc3881": true})),
            StatusCode::OK,
        )
        .await;
    assert_eq!(features["msc3881"], true, "{features}");
    assert_eq!(features["msc4222"], false, "{features}");
    let entry = audited(&admin, "users.experimental_features.put").await;
    assert_eq!(entry["changes"][0]["pointer"], "/msc3881", "{entry}");

    // ---- users.account_data.list / users.pushers.list ----
    phone
        .expect(
            Method::PUT,
            &format!("/_matrix/client/v3/user/{ALICE_PATH}/account_data/org.example.theme"),
            Some(json!({"dark": true})),
            StatusCode::OK,
        )
        .await;
    let data = admin
        .get(&format!("/api/v1/users/{ALICE_PATH}/account-data"))
        .await;
    assert_eq!(data["org.example.theme"]["dark"], true, "{data}");
    phone
        .expect(
            Method::POST,
            "/_matrix/client/v3/pushers/set",
            Some(json!({
                "pushkey": "phone-push-key",
                "kind": "http",
                "app_id": "im.example.app",
                "app_display_name": "Example",
                "device_display_name": "Alice's phone",
                "lang": "en",
                "data": {"url": "https://push.example.org/_matrix/push/v1/notify"},
            })),
            StatusCode::OK,
        )
        .await;
    let pushers = admin
        .get(&format!("/api/v1/users/{ALICE_PATH}/pushers"))
        .await;
    assert_eq!(
        pushers["items"][0]["pushkey"], "phone-push-key",
        "{pushers}"
    );
    admin
        .expect(
            Method::GET,
            "/api/v1/users/%40nobody%3Aexample.org/pushers",
            None,
            StatusCode::NOT_FOUND,
        )
        .await;

    // ---- a restart keeps what was set ----
    // Every write was logged, with who did it.
    let log = server.log();
    for said in [
        "an administrator renamed a device",
        "an administrator signed out devices",
        "an administrator bound a 3PID",
        "an administrator linked an external id",
        "an administrator set experimental features",
    ] {
        assert!(log.contains(said), "the log never said {said:?}");
    }
    server.stop();
    let mut server = HsProcess::serve(&config_path);
    server.wait_for("listening");
    let listed = admin
        .get(&format!("/api/v1/users/{ALICE_PATH}/threepids"))
        .await;
    assert_eq!(listed[0]["address"], "alice@example.org", "{listed}");
    let linked = admin
        .get(&format!("/api/v1/users/{ALICE_PATH}/external-ids"))
        .await;
    assert_eq!(linked[0]["provider"], "oidc-corp", "{linked}");
    let features = admin
        .get(&format!("/api/v1/users/{ALICE_PATH}/experimental-features"))
        .await;
    assert_eq!(features["msc3881"], true, "{features}");

    // ---- removing them ----
    admin
        .expect(
            Method::DELETE,
            &format!("/api/v1/users/{ALICE_PATH}/threepids/email/alice%40example.org"),
            None,
            StatusCode::NO_CONTENT,
        )
        .await;
    assert_eq!(
        email_login(&nobody, "alice@example.org").await,
        StatusCode::FORBIDDEN
    );
    admin
        .expect(
            Method::DELETE,
            &format!("/api/v1/users/{ALICE_PATH}/threepids/email/alice%40example.org"),
            None,
            StatusCode::NOT_FOUND,
        )
        .await;
    audited(&admin, "users.threepids.remove").await;
    admin
        .expect(
            Method::DELETE,
            &format!("/api/v1/users/{ALICE_PATH}/external-ids/oidc-corp/248289761001"),
            None,
            StatusCode::NO_CONTENT,
        )
        .await;
    admin
        .expect(
            Method::GET,
            "/api/v1/users/lookup?provider=oidc-corp&external_id=248289761001",
            None,
            StatusCode::NOT_FOUND,
        )
        .await;
    audited(&admin, "users.external_ids.remove").await;
    // Freed, so bob may have them now.
    admin
        .expect(
            Method::POST,
            "/api/v1/users/%40bob%3Aexample.org/external-ids",
            Some(json!({"provider": "oidc-corp", "external_id": "248289761001"})),
            StatusCode::CREATED,
        )
        .await;
    server.stop();
}
