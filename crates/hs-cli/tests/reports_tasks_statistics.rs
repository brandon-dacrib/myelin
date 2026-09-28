//! Reports, tasks and statistics through the real binary: a user reports a message and a
//! person over the client-server API, an administrator sees the report with the message beside
//! it and resolves it, a bridge replay is kept as a task, and the statistics count real accounts,
//! uploads and reports -- and all of it is still there after a restart.

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

/// One run of the real `hs` binary. A restart has to be a new process: an in-process server's
/// background tasks keep the store's lock until the process exits (see `tests/e2e.rs`).
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

struct Client {
    http: reqwest::Client,
    base: String,
}

impl Client {
    async fn call(
        &self,
        method: reqwest::Method,
        path: &str,
        token: &str,
        body: Option<Value>,
    ) -> (u16, Value) {
        let mut request = self
            .http
            .request(method, format!("{}{path}", self.base))
            .bearer_auth(token);
        if let Some(body) = body {
            request = request.json(&body);
        }
        let response = request.send().await.unwrap();
        let status = response.status().as_u16();
        let text = response.text().await.unwrap();
        (status, serde_json::from_str(&text).unwrap_or(Value::Null))
    }

    async fn get(&self, path: &str, token: &str) -> Value {
        let (status, body) = self.call(reqwest::Method::GET, path, token, None).await;
        assert_eq!(status, 200, "GET {path}: {body}");
        body
    }

    async fn post(&self, path: &str, token: &str, body: Value) -> (u16, Value) {
        self.call(reqwest::Method::POST, path, token, Some(body))
            .await
    }

    async fn register(&self, name: &str) -> String {
        let response: Value = self
            .http
            .post(format!("{}/_matrix/client/v3/register", self.base))
            .json(&json!({"username": name, "password": format!("{name}-password-1"), "auth": {"type": "m.login.dummy"}}))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        response["access_token"].as_str().unwrap().to_owned()
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_report_reaches_the_inbox_is_resolved_and_everything_survives_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    let port = free_port();
    let config_path = dir.path().join("homeserver.yaml");
    std::fs::write(&config_path, config_yaml(port, &dir.path().join("data"))).unwrap();
    let client = Client {
        http: reqwest::Client::new(),
        base: format!("http://127.0.0.1:{port}"),
    };

    let mut server = HsProcess::serve(&config_path);
    let line = server.wait_for("setup_link=");
    let setup_token: String = line
        .split_once("/admin/setup#token=")
        .unwrap()
        .1
        .chars()
        .take_while(char::is_ascii_alphabetic)
        .collect();
    let admin: Value = client
        .http
        .post(format!("{}/api/v1/setup", client.base))
        .json(&json!({"setup_token": setup_token, "username": "ops", "password": "hunter2-first-admin"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let ops = admin["access_token"].as_str().unwrap().to_owned();
    let alice = client.register("alice").await;
    let mallory = client.register("mallory").await;

    // Nothing reported yet, and no tasks: empty pages, not errors.
    assert_eq!(
        client.get("/api/v1/reports", &ops).await["items"],
        json!([])
    );
    assert_eq!(client.get("/api/v1/tasks", &ops).await["items"], json!([]));

    let (_, created) = client
        .post(
            "/_matrix/client/v3/createRoom",
            &alice,
            json!({"preset": "public_chat"}),
        )
        .await;
    let room_id = created["room_id"].as_str().unwrap().to_owned();
    let (status, _) = client
        .post(
            &format!("/_matrix/client/v3/rooms/{room_id}/join"),
            &mallory,
            json!({}),
        )
        .await;
    assert_eq!(status, 200);
    let (status, sent) = client
        .call(
            reqwest::Method::PUT,
            &format!("/_matrix/client/v3/rooms/{room_id}/send/m.room.message/1"),
            &mallory,
            Some(json!({"msgtype": "m.text", "body": "click my totally safe link"})),
        )
        .await;
    assert_eq!(status, 200, "{sent}");
    let spam = sent["event_id"].as_str().unwrap().to_owned();

    let (status, body) = client
        .post(
            &format!("/_matrix/client/v3/rooms/{room_id}/report/{spam}"),
            &alice,
            json!({"reason": "phishing", "score": -80}),
        )
        .await;
    assert_eq!((status, &body), (200, &json!({})));
    let (status, _) = client
        .post(
            "/_matrix/client/v3/users/@mallory:example.org/report",
            &alice,
            json!({"reason": "sent me the same link privately"}),
        )
        .await;
    assert_eq!(status, 200);

    // Alice uploads something, for the media statistics.
    let upload = client
        .http
        .post(format!(
            "{}/_matrix/client/v1/media/upload?filename=a.txt",
            client.base
        ))
        .bearer_auth(&alice)
        .header("content-type", "text/plain")
        .body("twelve bytes")
        .send()
        .await
        .unwrap();
    assert_eq!(upload.status(), 200);

    // The inbox.
    let inbox = client.get("/api/v1/reports?status=open", &ops).await;
    let items = inbox["items"].as_array().unwrap();
    assert_eq!(items.len(), 2, "{inbox}");
    let event_report = items.iter().find(|r| r["kind"] == "event").unwrap();
    assert_eq!(event_report["reporter_id"], "@alice:example.org");
    assert_eq!(event_report["reported_user_id"], "@mallory:example.org");
    assert_eq!(event_report["reason"], "phishing");
    assert_eq!(event_report["score"], -80);
    let id = event_report["id"].as_str().unwrap().to_owned();
    let detail = client.get(&format!("/api/v1/reports/{id}"), &ops).await;
    assert_eq!(
        detail["event"]["content"]["body"], "click my totally safe link",
        "{detail}"
    );

    // The Overview counts what awaits action.
    let overview = client.get("/api/v1/statistics/overview", &ops).await;
    assert_eq!(overview["pending_reports_count"], 2, "{overview}");

    // Ordinary users cannot read the inbox.
    let (status, _) = client
        .call(reqwest::Method::GET, "/api/v1/reports", &alice, None)
        .await;
    assert_eq!(status, 401);

    let (status, resolved) = client
        .post(
            &format!("/api/v1/reports/{id}/resolve"),
            &ops,
            json!({"resolution": "redacted", "note": "phishing link removed"}),
        )
        .await;
    assert_eq!(status, 200, "{resolved}");
    assert_eq!(resolved["status"], "resolved");
    assert_eq!(resolved["resolved_by"], "@ops:example.org");
    let audit = client
        .get("/api/v1/audit-log?action=reports.resolve", &ops)
        .await;
    assert_eq!(audit["items"][0]["target"]["id"], id.as_str(), "{audit}");

    // A bridge replay is kept as a task.
    let (status, bridge) = client
        .post(
            "/api/v1/appservices",
            &ops,
            json!({"registration_yaml": "id: tasks-test\nurl: null\nas_token: as-token-tasks-test\nhs_token: hs-token-tasks-test\nsender_localpart: tasksbot\nnamespaces:\n  users: []\n  aliases: []\n  rooms: []\n"}),
        )
        .await;
    assert!(status == 201 || status == 200, "{status}: {bridge}");
    let (status, task) = client
        .post("/api/v1/appservices/tasks-test/replay", &ops, json!({}))
        .await;
    assert_eq!(status, 202, "{task}");
    let task_id = task["id"].as_str().unwrap().to_owned();
    let kept = client.get(&format!("/api/v1/tasks/{task_id}"), &ops).await;
    assert_eq!(kept["status"], "succeeded");
    assert_eq!(kept["action"], "appservices.replay");

    // The statistics, from the real records.
    let media = client.get("/api/v1/statistics/users/media", &ops).await;
    assert_eq!(
        media["items"][0]["user_id"], "@alice:example.org",
        "{media}"
    );
    assert_eq!(media["items"][0]["media_count"], 1);
    assert_eq!(media["items"][0]["media_bytes"], 12);
    let rooms = client.get("/api/v1/statistics/rooms", &ops).await;
    assert_eq!(rooms["items"][0]["room_id"], room_id.as_str());
    assert_eq!(rooms["items"][0]["joined_members_count"], 2);
    let registered = client
        .get(
            "/api/v1/statistics/timeseries?metric=users.registered&step=1d",
            &ops,
        )
        .await;
    let total: f64 = registered["points"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["value"].as_f64().unwrap())
        .sum();
    assert_eq!(total, 3.0, "ops, alice and mallory: {registered}");
    let reports_received = client
        .get(
            "/api/v1/statistics/timeseries?metric=reports.received&step=1h",
            &ops,
        )
        .await;
    let last = reports_received["points"]
        .as_array()
        .unwrap()
        .last()
        .unwrap()
        .clone();
    assert_eq!(last["value"], 2.0, "{reports_received}");
    // The sampler took its first sample at startup.
    let mut sampled = Value::Null;
    for _ in 0..50 {
        sampled = client
            .get(
                "/api/v1/statistics/timeseries?metric=users_count&step=1h",
                &ops,
            )
            .await;
        if !sampled["points"].as_array().unwrap().is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert!(
        !sampled["points"].as_array().unwrap().is_empty(),
        "{sampled}"
    );

    server.stop();

    // After a restart: the reports, their decisions and the task are all still there.
    let mut server = HsProcess::serve(&config_path);
    server.wait_for("listening");
    let login: Value = client
        .http
        .post(format!("{}/_matrix/client/v3/login", client.base))
        .json(&json!({"type": "m.login.password", "identifier": {"type": "m.id.user", "user": "ops"}, "password": "hunter2-first-admin"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let ops = login["access_token"].as_str().unwrap().to_owned();
    let all = client.get("/api/v1/reports?include_total=true", &ops).await;
    assert_eq!(all["total"], 2, "{all}");
    let again = client.get(&format!("/api/v1/reports/{id}"), &ops).await;
    assert_eq!(again["status"], "resolved");
    assert_eq!(again["resolution_note"], "phishing link removed");
    let kept = client.get(&format!("/api/v1/tasks/{task_id}"), &ops).await;
    assert_eq!(kept["status"], "succeeded");
    let (status, _) = client
        .call(
            reqwest::Method::DELETE,
            &format!("/api/v1/reports/{id}"),
            &ops,
            None,
        )
        .await;
    assert_eq!(status, 204);
    server.stop();
}
