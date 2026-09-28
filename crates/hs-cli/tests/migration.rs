//! Migrating a real Synapse database into the real `hs` binary, through the admin API only:
//! `crates/hs-compat/tests/fixtures/synapse-small` (a Synapse 1.161 on PostgreSQL that
//! `populate.py` filled: four accounts, one deactivated; six devices and their tokens; a public
//! room with an alias, an edit, a redaction, an image and a member who left; a direct chat;
//! account data and a room tag; two uploads) is loaded into a fresh PostgreSQL database, and a
//! fresh server named `fixture.test` is pointed at it.
//!
//! The operator's whole path is followed: set the source (`config.update` of the `migration`
//! section, the password never answered back), start, wait for `ready_for_cutover`, restart the
//! server (the migration is still where it was), verify (a task; every count matches and every
//! sample agrees), cut over (a task; `completed`), restart again. Then the people on the other
//! side: alice signs in with her Synapse password, and her Synapse access token still works
//! without signing in again; her `/sync` has both rooms with their history, the redacted message
//! redacted; `#lobby:fixture.test` resolves (and the lobby stays out of the room directory, as
//! Synapse kept it); her account
//! data and room tag are there; the image she was sent downloads byte for byte; the deactivated
//! account stays deactivated. Every step is in the audit log, and the metrics say how many rows
//! each stream copied.
//!
//! Needs PostgreSQL: `HS_MIGRATION_TEST_POSTGRES_DSN`, or the local one at
//! `postgres://postgres:hspg@127.0.0.1:5439/postgres`; skips, saying so, without one.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use reqwest::{Method, StatusCode};
use serde_json::{Value, json};

fn fixture_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../hs-compat/tests/fixtures/synapse-small")
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// A fresh database holding the fixture, dropped afterwards.
struct SynapseDatabase {
    admin_dsn: String,
    name: String,
    config: postgres::Config,
}

impl SynapseDatabase {
    fn create() -> Option<Self> {
        let admin_dsn = std::env::var("HS_MIGRATION_TEST_POSTGRES_DSN")
            .unwrap_or_else(|_| "postgres://postgres:hspg@127.0.0.1:5439/postgres".to_owned());
        let config: postgres::Config = admin_dsn.parse().ok()?;
        let mut admin = match config.connect(postgres::NoTls) {
            Ok(client) => client,
            Err(e) => {
                eprintln!(
                    "SKIP: the migration test needs PostgreSQL at {admin_dsn:?}: {e}. Start one \
                     with: docker run --rm -d -e POSTGRES_PASSWORD=hspg -p 127.0.0.1:5439:5432 \
                     postgres:17"
                );
                return None;
            }
        };
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let name = format!("synapse_fixture_{}_{nanos}", std::process::id());
        admin
            .batch_execute(&format!(
                "CREATE DATABASE {name} ENCODING 'UTF8' LC_COLLATE 'C' LC_CTYPE 'C' TEMPLATE template0"
            ))
            .unwrap();
        let mut db_config = config.clone();
        db_config.dbname(&name);
        let mut client = db_config.connect(postgres::NoTls).unwrap();
        for file in ["schema.sql", "data.sql"] {
            let sql = std::fs::read_to_string(fixture_dir().join(file)).unwrap();
            client.batch_execute(&sql).unwrap();
        }
        Some(Self {
            admin_dsn,
            name,
            config,
        })
    }

    /// The `migration` section pointing at this database.
    fn source(&self) -> Value {
        let host = match self.config.get_hosts().first() {
            Some(postgres::config::Host::Tcp(host)) => host.clone(),
            _ => "127.0.0.1".to_owned(),
        };
        json!({
            "synapse": {
                "database": {
                    "host": host,
                    "port": self.config.get_ports().first().copied().unwrap_or(5432),
                    "database": self.name,
                    "user": self.config.get_user().unwrap_or("postgres"),
                    "password": String::from_utf8_lossy(self.config.get_password().unwrap_or_default()),
                },
                "media_store_path": fixture_dir().join("media_store").canonicalize().unwrap(),
                "batch_size": 2,
            }
        })
    }
}

impl Drop for SynapseDatabase {
    fn drop(&mut self) {
        let dsn = self.admin_dsn.clone();
        let name = self.name.clone();
        // Dropped inside the test's runtime; the synchronous client needs a thread of its own.
        let _ = std::thread::spawn(move || {
            if let Ok(mut client) = dsn
                .parse::<postgres::Config>()
                .and_then(|c| c.connect(postgres::NoTls))
            {
                let _ =
                    client.batch_execute(&format!("DROP DATABASE IF EXISTS {name} WITH (FORCE)"));
            }
        })
        .join();
    }
}

/// One run of the real `hs` binary (the harness of `admin_user_identity.rs`).
struct HsProcess {
    child: std::process::Child,
    lines: std::sync::mpsc::Receiver<String>,
    seen: Vec<String>,
}

impl HsProcess {
    fn serve(config_path: &Path) -> Self {
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

    fn log(&mut self) -> String {
        while let Ok(line) = self.lines.recv_timeout(Duration::from_millis(200)) {
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
    fn with(&self, token: &str) -> Self {
        Self {
            base: self.base.clone(),
            token: Some(token.to_owned()),
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

    async fn get(&self, path: &str) -> Value {
        self.expect(Method::GET, path, None, StatusCode::OK).await
    }

    async fn bytes(&self, path: &str) -> (StatusCode, Vec<u8>) {
        let mut request = reqwest::Client::new().get(format!("{}{path}", self.base));
        if let Some(token) = &self.token {
            request = request.bearer_auth(token);
        }
        let response = request.send().await.unwrap();
        (response.status(), response.bytes().await.unwrap().to_vec())
    }

    /// Polls `GET /api/v1/migration` until its status is `want`.
    async fn migration_reaches(&self, want: &str) -> Value {
        let deadline = Instant::now() + Duration::from_secs(120);
        loop {
            let status = self.get("/api/v1/migration").await;
            if status["status"] == want {
                return status;
            }
            assert!(
                !matches!(status["status"].as_str(), Some("failed")) || want == "failed",
                "the migration failed: {status}"
            );
            assert!(Instant::now() < deadline, "never reached {want}: {status}");
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }

    /// Polls a task until it ends; the task.
    async fn task_ends(&self, id: &str) -> Value {
        let deadline = Instant::now() + Duration::from_secs(120);
        loop {
            let task = self.get(&format!("/api/v1/tasks/{id}")).await;
            if matches!(
                task["status"].as_str(),
                Some("succeeded" | "failed" | "cancelled")
            ) {
                return task;
            }
            assert!(Instant::now() < deadline, "the task never ended: {task}");
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }
}

#[tokio::test]
async fn a_synapse_database_is_migrated_verified_and_cut_over_through_the_admin_api() {
    // The synchronous `postgres` client runs its own runtime: off this one.
    let Some(db) = tokio::task::spawn_blocking(SynapseDatabase::create)
        .await
        .unwrap()
    else {
        return;
    };
    let facts: Value =
        serde_json::from_str(&std::fs::read_to_string(fixture_dir().join("facts.json")).unwrap())
            .unwrap();
    let dir = tempfile::tempdir().unwrap();
    // Synapse's own signing key, so that this server signs as the one other servers know.
    let keys = dir.path().join("keys");
    std::fs::create_dir_all(&keys).unwrap();
    std::fs::copy(
        fixture_dir().join("signing.key"),
        keys.join("fixture.test.signing.key"),
    )
    .unwrap();
    let port = free_port();
    std::fs::write(
        dir.path().join("hs.yaml"),
        format!(
            "server:\n  server_name: fixture.test\n  signing_key_path: {keys:?}\n\
             listeners:\n  listeners:\n    - port: {port}\n      bind_addresses: [\"127.0.0.1\"]\n      resources: [client, federation, media, admin, health, metrics]\n\
             storage:\n  backend: embedded\n  data_dir: {:?}\n\
             media:\n  storage:\n    backend: local\n    path: {:?}\n\
             rate_limits:\n  enabled: false\n",
            dir.path().join("db"),
            dir.path().join("media"),
        ),
    )
    .unwrap();

    let config_path = dir.path().join("hs.yaml");
    let mut hs = HsProcess::serve(&config_path);
    let setup_line = hs.wait_for("setup_link=");
    let setup_token = setup_line
        .rsplit_once("#token=")
        .unwrap()
        .1
        .trim()
        .to_owned();
    let base = format!("http://127.0.0.1:{port}");
    let nobody = Caller {
        base: base.clone(),
        token: None,
    };
    let created = nobody
        .expect(
            Method::POST,
            "/api/v1/setup",
            Some(json!({"setup_token": setup_token, "username": "ops", "password": "ops-password-1"})),
            StatusCode::CREATED,
        )
        .await;
    let ops = nobody.with(created["access_token"].as_str().unwrap());

    // Nothing configured: an honest refusal, and nothing recorded.
    let refused = ops
        .expect(
            Method::POST,
            "/api/v1/migration/start",
            None,
            StatusCode::BAD_REQUEST,
        )
        .await;
    assert!(
        refused["detail"]
            .as_str()
            .unwrap()
            .contains("/migration/synapse"),
        "{refused}"
    );

    // The source, set the way the Migration page sets it. The password is never answered back.
    ops.expect(
        Method::PATCH,
        "/api/v1/config/migration",
        Some(db.source()),
        StatusCode::OK,
    )
    .await;
    let section = ops.get("/api/v1/config/migration").await;
    assert_eq!(
        section["values"]["synapse"]["database"]["password"],
        json!({"$secret": true})
    );

    let started = ops
        .expect(
            Method::POST,
            "/api/v1/migration/start",
            None,
            StatusCode::OK,
        )
        .await;
    assert_eq!(started["status"], "copying", "{started}");
    assert!(
        started["source"]
            .as_str()
            .unwrap()
            .starts_with("postgresql://")
    );
    assert!(
        !started.to_string().contains("hspg"),
        "the password leaked: {started}"
    );

    let ready = ops.migration_reaches("ready_for_cutover").await;
    let stream = |name: &str| {
        ready["streams"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["name"] == name)
            .cloned()
            .unwrap_or_else(|| panic!("no {name} stream in {ready}"))
    };
    assert_eq!(stream("users")["copied_count"], 4, "{ready}");
    assert_eq!(stream("devices")["copied_count"], 6);
    assert_eq!(stream("access_tokens")["copied_count"], 6);
    assert_eq!(stream("rooms")["copied_count"], 2);
    assert_eq!(stream("rooms")["failed_count"], 0, "{ready}");
    assert_eq!(stream("media")["copied_count"], 2);
    assert_eq!(stream("account_data")["copied_count"], 3);

    // A restart: the migration is where it was.
    hs.stop();
    let mut hs = HsProcess::serve(&config_path);
    hs.wait_for("listening");
    let after_restart = ops.get("/api/v1/migration").await;
    assert_eq!(
        after_restart["status"], "ready_for_cutover",
        "{after_restart}"
    );

    // Verification: a task, then every count and sample agrees.
    let task = ops
        .expect(
            Method::POST,
            "/api/v1/migration/verify",
            None,
            StatusCode::ACCEPTED,
        )
        .await;
    let ended = ops.task_ends(task["id"].as_str().unwrap()).await;
    assert_eq!(ended["status"], "succeeded", "{ended}");
    let verified = ops.get("/api/v1/migration").await;
    assert_eq!(verified["status"], "ready_for_cutover");
    assert_eq!(verified["verification"]["passed"], true, "{verified}");

    // Cutover: the final pass, verification again, and completed.
    let task = ops
        .expect(
            Method::POST,
            "/api/v1/migration/cutover",
            None,
            StatusCode::ACCEPTED,
        )
        .await;
    let ended = ops.task_ends(task["id"].as_str().unwrap()).await;
    assert_eq!(ended["status"], "succeeded", "{ended}");
    let done = ops.migration_reaches("completed").await;
    assert_eq!(done["cutover_by"], "@ops:fixture.test");
    // Nothing can be started or aborted over a finished migration.
    ops.expect(
        Method::POST,
        "/api/v1/migration/abort",
        None,
        StatusCode::CONFLICT,
    )
    .await;

    hs.stop();
    let mut hs = HsProcess::serve(&config_path);
    hs.wait_for("listening");
    assert_eq!(ops.get("/api/v1/migration").await["status"], "completed");

    // alice's Synapse session goes on working, and so does her Synapse password.
    let alice = nobody.with(facts["alice_token"].as_str().unwrap());
    let whoami = alice.get("/_matrix/client/v3/account/whoami").await;
    assert_eq!(whoami["user_id"], "@alice:fixture.test");
    assert_eq!(whoami["device_id"], "ALICEPHONE");
    nobody
        .expect(
            Method::POST,
            "/_matrix/client/v3/login",
            Some(json!({
                "type": "m.login.password",
                "identifier": {"type": "m.id.user", "user": "alice"},
                "password": "alice-password-1",
            })),
            StatusCode::OK,
        )
        .await;
    // dave was deactivated in Synapse, and is here.
    let (status, _) = nobody
        .call(
            Method::POST,
            "/_matrix/client/v3/login",
            Some(json!({
                "type": "m.login.password",
                "identifier": {"type": "m.id.user", "user": "dave"},
                "password": "dave-password-1",
            })),
        )
        .await;
    assert_ne!(status, StatusCode::OK);

    // Her rooms, with their history.
    let lobby = facts["lobby"].as_str().unwrap();
    let dm = facts["dm"].as_str().unwrap();
    let sync = alice.get("/_matrix/client/v3/sync").await;
    let joined = &sync["rooms"]["join"];
    assert!(
        joined.get(lobby).is_some() && joined.get(dm).is_some(),
        "{sync}"
    );
    let messages = alice
        .get(&format!(
            "/_matrix/client/v3/rooms/{}/messages?dir=b&limit=100",
            urlencode(lobby)
        ))
        .await;
    let bodies: Vec<&str> = messages["chunk"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|e| e["content"]["body"].as_str())
        .collect();
    assert!(bodies.contains(&"Welcome to the lobby"), "{bodies:?}");
    assert!(bodies.contains(&"Message number 12"), "{bodies:?}");
    assert!(
        !bodies.contains(&"this one gets redacted"),
        "the redaction was not applied: {bodies:?}"
    );
    let profile = alice
        .get("/_matrix/client/v3/profile/%40alice%3Afixture.test")
        .await;
    assert_eq!(profile["displayname"], "Alice Liddell");

    // The alias, and the directory.
    let resolved = alice
        .get("/_matrix/client/v3/directory/room/%23lobby%3Afixture.test")
        .await;
    assert_eq!(resolved["room_id"], lobby);
    // Synapse's default `room_list_publication_rules` refused to publish it (`rooms.is_public`
    // is false in the fixture), and the directory here says the same.
    let public = alice.get("/_matrix/client/v3/publicRooms").await;
    assert!(
        !public["chunk"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["room_id"] == lobby),
        "{public}"
    );

    // Account data and the room tag.
    let note = alice
        .get("/_matrix/client/v3/user/%40alice%3Afixture.test/account_data/fixture.test.note")
        .await;
    assert_eq!(note["note"], "kept across the migration");
    let tags = alice
        .get(&format!(
            "/_matrix/client/v3/user/%40alice%3Afixture.test/rooms/{}/account_data/m.tag",
            urlencode(lobby)
        ))
        .await;
    assert!(tags["tags"].get("m.favourite").is_some(), "{tags}");

    // The picture bob sent, byte for byte.
    let picture = facts["picture"].as_str().unwrap();
    let media_id = picture.rsplit('/').next().unwrap();
    let (status, bytes) = alice
        .bytes(&format!(
            "/_matrix/client/v1/media/download/fixture.test/{media_id}"
        ))
        .await;
    assert_eq!(status, StatusCode::OK);
    let original = std::fs::read(
        fixture_dir()
            .join("media_store/local_content")
            .join(&media_id[0..2])
            .join(&media_id[2..4])
            .join(&media_id[4..]),
    )
    .unwrap();
    assert_eq!(bytes, original);

    // The record: each step audited, the log kept, the metrics.
    let audit = ops.get("/api/v1/audit-log?limit=200").await;
    let actions: Vec<&str> = audit["items"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|e| e["action"].as_str())
        .collect();
    for action in ["migration.start", "migration.verify", "migration.cutover"] {
        assert!(
            actions.contains(&action),
            "{action} is not audited: {actions:?}"
        );
    }
    let log = ops.get("/api/v1/migration/log?limit=500").await;
    assert!(
        log["items"].as_array().unwrap().iter().any(|e| e["message"]
            .as_str()
            .is_some_and(|m| m.contains("cut over by"))),
        "{log}"
    );
    let (_, metrics) = nobody.bytes("/metrics").await;
    let metrics = String::from_utf8(metrics).unwrap();
    assert!(
        metrics.contains("hs_migration_rows_copied{stream=\"users\"} 4"),
        "{}",
        metrics
            .lines()
            .filter(|l| l.contains("migration"))
            .collect::<Vec<_>>()
            .join("\n")
    );
    assert!(metrics.contains("hs_migration_status{status=\"completed\"} 1"));
    let _ = hs.log();
    hs.stop();
}

fn urlencode(s: &str) -> String {
    s.replace('!', "%21")
        .replace(':', "%3A")
        .replace('#', "%23")
        .replace('@', "%40")
}
