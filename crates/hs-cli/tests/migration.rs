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
    dir: PathBuf,
}

impl SynapseDatabase {
    fn create() -> Option<Self> {
        Self::create_from(fixture_dir())
    }

    fn create_from(dir: PathBuf) -> Option<Self> {
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
        // Two tests of this file run at once: the counter keeps their databases apart.
        static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let name = format!("synapse_fixture_{}_{nanos}_{n}", std::process::id());
        admin
            .batch_execute(&format!(
                "CREATE DATABASE {name} ENCODING 'UTF8' LC_COLLATE 'C' LC_CTYPE 'C' TEMPLATE template0"
            ))
            .unwrap();
        let mut db_config = config.clone();
        db_config.dbname(&name);
        let mut client = db_config.connect(postgres::NoTls).unwrap();
        for file in ["schema.sql", "data.sql"] {
            let sql = std::fs::read_to_string(dir.join(file)).unwrap();
            client.batch_execute(&sql).unwrap();
        }
        Some(Self {
            admin_dsn,
            name,
            config,
            dir,
        })
    }

    /// The `migration` section pointing at this database.
    fn source(&self) -> Value {
        let host = match self.config.get_hosts().first() {
            Some(postgres::config::Host::Tcp(host)) => host.clone(),
            _ => "127.0.0.1".to_owned(),
        };
        let mut source = json!({
            "synapse": {
                "database": {
                    "host": host,
                    "port": self.config.get_ports().first().copied().unwrap_or(5432),
                    "database": self.name,
                    "user": self.config.get_user().unwrap_or("postgres"),
                    "password": String::from_utf8_lossy(self.config.get_password().unwrap_or_default()),
                },
                "batch_size": 2,
            }
        });
        if let Ok(media) = self.dir.join("media_store").canonicalize() {
            source["synapse"]["media_store_path"] = json!(media);
        }
        source
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
    // Other servers' media: the one whose file is in the store is copied, the one whose file is
    // gone is left out on purpose.
    assert_eq!(stream("remote_media")["copied_count"], 1, "{ready}");
    assert_eq!(stream("remote_media")["skipped_count"], 1, "{ready}");
    assert_eq!(stream("remote_media")["failed_count"], 0, "{ready}");
    assert_eq!(stream("account_data")["copied_count"], 3);
    for (name, count) in [
        ("e2e_keys", 2),
        ("cross_signing", 2),
        ("key_backups", 2),
        ("push_rules", 1),
        ("pushers", 1),
        ("filters", 2),
        ("receipts", 4),
        ("threepids", 2),
        ("external_ids", 1),
        ("registration_tokens", 2),
    ] {
        assert_eq!(stream(name)["copied_count"], count, "{name}: {ready}");
        assert_eq!(stream(name)["failed_count"], 0, "{name}: {ready}");
    }
    // Refresh tokens: the two unspent ones; bob's exchanged one is left out. To-device
    // messages: the two waiting for alice's phone; the one for a device nobody has is left out.
    for name in ["refresh_tokens", "to_device"] {
        assert_eq!(stream(name)["copied_count"], 2, "{name}: {ready}");
        assert_eq!(stream(name)["skipped_count"], 1, "{name}: {ready}");
        assert_eq!(stream(name)["failed_count"], 0, "{name}: {ready}");
    }

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
    // The cutover's final pass read every room again, and this process measured it.
    let (_, metrics) = nobody.bytes("/metrics").await;
    let metrics = String::from_utf8(metrics).unwrap();
    let counter = |name: &str| {
        metrics
            .lines()
            .find_map(|l| l.strip_prefix(&format!("{name} ")))
            .and_then(|n| n.trim().parse::<f64>().ok())
            .unwrap_or_else(|| panic!("no {name}: {metrics}"))
    };
    assert!(counter("hs_migration_events_read_total") > 30.0);
    assert!(counter("hs_migration_event_bytes_read_total") > 10_000.0);
    assert!(counter("hs_migration_room_seconds_count") >= 2.0);
    assert!(counter("hs_migration_peak_rss_bytes") > 1_000_000.0);

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

    // The picture from another server that Synapse had cached: served from the copy, byte for
    // byte, with the content type Synapse recorded. Nothing resolves `other.test`, so bytes here
    // can only have come from the import. The one whose file was gone is not here, and asking
    // for it is a fetch from its server, which fails as it would have in Synapse.
    let remote = facts["remote_picture"].as_str().unwrap();
    let (origin, remote_id) = remote
        .strip_prefix("mxc://")
        .unwrap()
        .split_once('/')
        .unwrap();
    let (status, bytes) = alice
        .bytes(&format!(
            "/_matrix/client/v1/media/download/{origin}/{remote_id}"
        ))
        .await;
    assert_eq!(status, StatusCode::OK);
    let original = std::fs::read(
        fixture_dir()
            .join("media_store/remote_content")
            .join(origin)
            .join(&remote_id[0..2])
            .join(&remote_id[2..4])
            .join(&remote_id[4..]),
    )
    .unwrap();
    assert_eq!(bytes, original);
    let missing = facts["remote_missing"].as_str().unwrap();
    let (_, missing_id) = missing
        .strip_prefix("mxc://")
        .unwrap()
        .split_once('/')
        .unwrap();
    let (status, _) = alice
        .bytes(&format!(
            "/_matrix/client/v1/media/download/{origin}/{missing_id}"
        ))
        .await;
    assert_ne!(
        status,
        StatusCode::OK,
        "a cache entry without its file was copied"
    );

    // End-to-end keys: alice's phone as her client uploaded it to Synapse, signed by her
    // self-signing key; her cross-signing keys; bob's master key with her signature on it.
    let alice_id = "@alice:fixture.test";
    let bob_id = "@bob:fixture.test";
    let queried = alice
        .expect(
            Method::POST,
            "/_matrix/client/v3/keys/query",
            Some(json!({"device_keys": {alice_id: [], bob_id: []}})),
            StatusCode::OK,
        )
        .await;
    let phone = &queried["device_keys"][alice_id]["ALICEPHONE"];
    assert_eq!(phone["device_id"], "ALICEPHONE", "{queried}");
    let self_signing = format!(
        "ed25519:{}",
        facts["alice_self_signing_key"].as_str().unwrap()
    );
    assert!(
        phone["signatures"][alice_id].get(&self_signing).is_some(),
        "{phone}"
    );
    assert!(
        queried["device_keys"][bob_id].get("BOBLAPTOP").is_some(),
        "{queried}"
    );
    let alice_master = facts["alice_master_key"].as_str().unwrap();
    assert!(
        queried["master_keys"][alice_id]["keys"]
            .get(format!("ed25519:{alice_master}"))
            .is_some(),
        "{queried}"
    );
    assert!(
        queried["self_signing_keys"].get(alice_id).is_some(),
        "{queried}"
    );
    assert!(
        queried["user_signing_keys"].get(alice_id).is_some(),
        "{queried}"
    );
    assert!(
        queried["master_keys"][bob_id]["signatures"]
            .get(alice_id)
            .is_some(),
        "alice's verification of bob was lost: {queried}"
    );
    // One of the one-time keys alice's phone uploaded to Synapse, oldest first, and her
    // fallback key once those run out.
    let claimed = nobody
        .with(facts["bob_token"].as_str().unwrap())
        .expect(
            Method::POST,
            "/_matrix/client/v3/keys/claim",
            Some(json!({"one_time_keys": {alice_id: {"ALICEPHONE": "signed_curve25519"}}})),
            StatusCode::OK,
        )
        .await;
    let one_time = claimed["one_time_keys"][alice_id]["ALICEPHONE"]
        .as_object()
        .unwrap_or_else(|| panic!("no one-time key: {claimed}"));
    assert!(
        one_time.contains_key("signed_curve25519:AAAAA0"),
        "{claimed}"
    );
    // The backup: version 2 (version 1 was deleted in Synapse), holding three room keys.
    let backup = alice.get("/_matrix/client/v3/room_keys/version").await;
    assert_eq!(backup["version"], facts["backup_version"], "{backup}");
    assert_eq!(backup["count"], 3, "{backup}");
    assert!(backup["auth_data"].get("public_key").is_some(), "{backup}");
    let (status, _) = alice
        .call(Method::GET, "/_matrix/client/v3/room_keys/version/1", None)
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let room_keys = alice
        .get("/_matrix/client/v3/room_keys/keys?version=2")
        .await;
    assert_eq!(
        room_keys["rooms"][lobby]["sessions"]
            .as_object()
            .map(serde_json::Map::len),
        Some(2),
        "{room_keys}"
    );

    // Push rules and the pusher.
    let rules = alice.get("/_matrix/client/v3/pushrules/").await;
    let global = &rules["global"];
    let find = |kind: &str, id: &str| -> Value {
        global[kind]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["rule_id"] == id)
            .cloned()
            .unwrap_or_else(|| panic!("no {kind} rule {id}: {rules}"))
    };
    assert_eq!(find("content", "lobbyword")["pattern"], "lobby");
    assert_eq!(find("room", dm)["actions"], json!([]));
    assert_eq!(find("override", "fixture.quiet_bots")["actions"], json!([]));
    assert_eq!(
        find("override", ".m.rule.suppress_notices")["enabled"],
        false
    );
    assert_eq!(
        find("underride", ".m.rule.message")["actions"],
        json!(["notify", {"set_tweak": "sound", "value": "default"}])
    );
    let pushers = alice.get("/_matrix/client/v3/pushers").await;
    assert_eq!(
        pushers["pushers"][0]["pushkey"], "alice-pushkey",
        "{pushers}"
    );
    assert_eq!(
        pushers["pushers"][0]["data"]["url"],
        "https://push.fixture.test/_matrix/push/v1/notify"
    );

    // Filter 0, under the id Synapse gave it.
    let filter = alice
        .get("/_matrix/client/v3/user/%40alice%3Afixture.test/filter/0")
        .await;
    assert_eq!(filter["room"]["timeline"]["limit"], 20, "{filter}");

    // Receipts: bob's read receipt in the lobby, and alice's private one in the direct chat.
    let receipt_in = |sync: &Value, room: &str| -> Value {
        sync["rooms"]["join"][room]["ephemeral"]["events"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|e| e["type"] == "m.receipt")
            .map(|e| e["content"].clone())
            .unwrap_or(Value::Null)
    };
    let lobby_receipts = receipt_in(&sync, lobby);
    let first = facts["first_message"].as_str().unwrap();
    assert!(
        lobby_receipts[first]["m.read"].get(bob_id).is_some(),
        "{lobby_receipts}"
    );
    // Alice's threaded receipts in the lobby, each in its thread (MSC3771).
    for (event, thread) in [
        (facts["thread_receipt"].as_str().unwrap(), first),
        (facts["main_receipt"].as_str().unwrap(), "main"),
    ] {
        assert_eq!(
            lobby_receipts[event]["m.read"][alice_id]["thread_id"], thread,
            "{lobby_receipts}"
        );
    }
    let dm_receipts = receipt_in(&sync, dm);
    let private = facts["private_receipt"].as_str().unwrap();
    assert!(
        dm_receipts[private]["m.read.private"]
            .get(alice_id)
            .is_some(),
        "{dm_receipts}"
    );

    // The two to-device messages that were waiting for alice's phone in Synapse are in its
    // first sync here, from bob, as they were.
    let fresh = alice.get("/_matrix/client/v3/sync").await;
    let waiting = fresh["to_device"]["events"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let types: Vec<&str> = waiting.iter().filter_map(|e| e["type"].as_str()).collect();
    assert!(
        types.contains(&"m.room_key_request") && types.contains(&"m.room.encrypted"),
        "{fresh}"
    );
    assert!(
        waiting.iter().all(|e| e["sender"] == "@bob:fixture.test"),
        "{waiting:?}"
    );
    assert!(
        waiting.iter().any(|e| e["content"]["request_id"] == "req1"),
        "{waiting:?}"
    );

    // Her email address came with her: she sees it, and signs in by it.
    let threepids = alice.get("/_matrix/client/v3/account/3pid").await;
    assert!(
        threepids["threepids"]
            .as_array()
            .unwrap()
            .iter()
            .any(|t| t["medium"] == "email" && t["address"] == "alice@fixture.test"),
        "{threepids}"
    );
    nobody
        .expect(
            Method::POST,
            "/_matrix/client/v3/login",
            Some(json!({
                "type": "m.login.password",
                "identifier": {"type": "m.id.thirdparty", "medium": "email", "address": "alice@fixture.test"},
                "password": "alice-password-1",
            })),
            StatusCode::OK,
        )
        .await;
    // Her identity at the upstream provider is linked to her account, so a sign-in through
    // that provider lands in it; and dave's erasure came over with him.
    let found = ops
        .get("/api/v1/users/lookup?provider=oidc-fixture&external_id=alice-at-the-provider")
        .await;
    assert_eq!(found["user_id"], "@alice:fixture.test", "{found}");
    let dave = ops.get("/api/v1/users/@dave:fixture.test").await;
    assert_eq!(dave["erased"], true, "{dave}");
    assert_eq!(dave["deactivated"], true, "{dave}");

    // Synapse's registration tokens open this (closed) server: one still has uses left.
    let tokens = ops.get("/api/v1/registration-tokens").await;
    let token_one = tokens["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["token"] == "fixture-token-one")
        .cloned()
        .unwrap_or_else(|| panic!("{tokens}"));
    assert_eq!(token_one["uses_allowed"], 5, "{token_one}");
    assert_eq!(token_one["completed"], 2, "{token_one}");
    let (status, flows) = nobody
        .call(
            Method::POST,
            "/_matrix/client/v3/register",
            Some(json!({"username": "erin", "password": "erin-password-1"})),
        )
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{flows}");
    assert!(
        flows["flows"]
            .as_array()
            .unwrap()
            .iter()
            .any(|f| f["stages"]
                .as_array()
                .unwrap()
                .contains(&json!("m.login.registration_token"))),
        "{flows}"
    );
    nobody
        .expect(
            Method::POST,
            "/_matrix/client/v3/register",
            Some(json!({
                "username": "erin",
                "password": "erin-password-1",
                "auth": {
                    "type": "m.login.registration_token",
                    "token": "fixture-token-two",
                    "session": flows["session"],
                },
            })),
            StatusCode::OK,
        )
        .await;

    // Refresh tokens: alice's phone exchanges its Synapse refresh token here for a new pair,
    // and the access token it was minted with stops working, as a refresh does; bob's
    // exchanged one is unknown here, as it is in Synapse.
    let refreshed = nobody
        .expect(
            Method::POST,
            "/_matrix/client/v3/refresh",
            Some(json!({"refresh_token": facts["alice_refresh_token"]})),
            StatusCode::OK,
        )
        .await;
    assert!(refreshed["access_token"].is_string(), "{refreshed}");
    assert!(refreshed["refresh_token"].is_string(), "{refreshed}");
    let renewed = nobody.with(refreshed["access_token"].as_str().unwrap());
    let whoami = renewed.get("/_matrix/client/v3/account/whoami").await;
    assert_eq!(whoami["device_id"], "ALICEPHONE", "{whoami}");
    let (status, _) = alice
        .call(Method::GET, "/_matrix/client/v3/account/whoami", None)
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let (status, refused) = nobody
        .call(
            Method::POST,
            "/_matrix/client/v3/refresh",
            Some(json!({"refresh_token": facts["bob_spent_refresh_token"]})),
        )
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{refused}");
    assert_eq!(refused["errcode"], "M_UNKNOWN_TOKEN", "{refused}");
    nobody
        .expect(
            Method::POST,
            "/_matrix/client/v3/refresh",
            Some(json!({"refresh_token": facts["bob_refresh_token"]})),
            StatusCode::OK,
        )
        .await;

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
    assert!(
        metrics.contains("hs_migration_rows_copied{stream=\"e2e_keys\"} 2"),
        "{metrics}"
    );
    assert!(
        log["items"].as_array().unwrap().iter().any(|e| e["message"]
            .as_str()
            .is_some_and(|m| m.starts_with("throughput: 2 rooms"))),
        "{log}"
    );
    let _ = hs.log();
    hs.stop();
}

fn urlencode(s: &str) -> String {
    s.replace('!', "%21")
        .replace(':', "%3A")
        .replace('#', "%23")
        .replace('@', "%40")
}

/// `crates/hs-compat/tests/fixtures/synapse-federated`: a real Synapse 1.161 (`127.0.0.1:18301`)
/// whose two accounts joined two rooms of another Synapse (`127.0.0.1:18302`) over federation.
/// "Elsewhere" Synapse backfilled whole; "Faraway" it holds only from hana's join. Both are
/// migrated into the real binary and served: their history, their state, the receipt, and a new
/// message goes into the one held from the join.
#[tokio::test]
async fn rooms_joined_over_federation_are_migrated_and_served() {
    let dir =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../hs-compat/tests/fixtures/synapse-federated");
    let fixture = dir.clone();
    let Some(db) = tokio::task::spawn_blocking(move || SynapseDatabase::create_from(fixture))
        .await
        .unwrap()
    else {
        return;
    };
    let facts: Value =
        serde_json::from_str(&std::fs::read_to_string(dir.join("facts.json")).unwrap()).unwrap();
    let work = tempfile::tempdir().unwrap();
    let keys = work.path().join("keys");
    std::fs::create_dir_all(&keys).unwrap();
    std::fs::copy(
        dir.join("signing.key"),
        keys.join("127.0.0.1:18301.signing.key"),
    )
    .unwrap();
    let port = free_port();
    std::fs::write(
        work.path().join("hs.yaml"),
        format!(
            "server:\n  server_name: \"127.0.0.1:18301\"\n  signing_key_path: {keys:?}\n\
             listeners:\n  listeners:\n    - port: {port}\n      bind_addresses: [\"127.0.0.1\"]\n      resources: [client, federation, media, admin, health, metrics]\n\
             storage:\n  backend: embedded\n  data_dir: {:?}\n\
             media:\n  storage:\n    backend: local\n    path: {:?}\n\
             rate_limits:\n  enabled: false\n",
            work.path().join("db"),
            work.path().join("media"),
        ),
    )
    .unwrap();
    let mut hs = HsProcess::serve(&work.path().join("hs.yaml"));
    let setup_line = hs.wait_for("setup_link=");
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
    let created = nobody
        .expect(
            Method::POST,
            "/api/v1/setup",
            Some(json!({"setup_token": setup_token, "username": "ops", "password": "ops-password-1"})),
            StatusCode::CREATED,
        )
        .await;
    let ops = nobody.with(created["access_token"].as_str().unwrap());
    ops.expect(
        Method::PATCH,
        "/api/v1/config/migration",
        Some(db.source()),
        StatusCode::OK,
    )
    .await;
    ops.expect(
        Method::POST,
        "/api/v1/migration/start",
        None,
        StatusCode::OK,
    )
    .await;
    let ready = ops.migration_reaches("ready_for_cutover").await;
    let rooms = ready["streams"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["name"] == "rooms")
        .cloned()
        .unwrap();
    let log = ops.get("/api/v1/migration/log?limit=500").await;
    assert_eq!(rooms["copied_count"], 2, "{ready}\n{log}");
    assert_eq!(rooms["skipped_count"], 0, "{ready}\n{log}");
    assert_eq!(rooms["failed_count"], 0, "{ready}\n{log}");
    let faraway = facts["faraway"].as_str().unwrap();
    let elsewhere = facts["elsewhere"].as_str().unwrap();
    assert!(
        log["items"].as_array().unwrap().iter().any(|e| e["message"]
            .as_str()
            .is_some_and(|m| m.starts_with(&format!("{faraway}: joined over federation")))),
        "{log}"
    );

    // Verification: every room's history from where it starts here, and its current state,
    // against Synapse's.
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
    assert_eq!(verified["verification"]["passed"], true, "{verified}");

    // hana's Synapse session: both rooms in her `/sync`, with her receipt in Faraway.
    let hana = nobody.with(facts["hana_token"].as_str().unwrap());
    let sync = hana.get("/_matrix/client/v3/sync").await;
    let joined = &sync["rooms"]["join"];
    assert!(
        joined.get(faraway).is_some() && joined.get(elsewhere).is_some(),
        "{sync}"
    );
    let last = facts["faraway_last"].as_str().unwrap();
    let receipts = joined[faraway]["ephemeral"]["events"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|e| e["type"] == "m.receipt")
        .cloned()
        .unwrap_or(Value::Null);
    assert!(
        receipts["content"][last]["m.read"]
            .get("@hana:127.0.0.1:18301")
            .is_some(),
        "{receipts}"
    );

    // Faraway: its history since hana joined, its state as the other server left it.
    let messages = hana
        .get(&format!(
            "/_matrix/client/v3/rooms/{}/messages?dir=b&limit=6",
            urlencode(faraway)
        ))
        .await;
    let bodies: Vec<&str> = messages["chunk"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|e| e["content"]["body"].as_str())
        .collect();
    for body in [
        "faraway: welcome, hana",
        "faraway: hello from home",
        "faraway: hugo here too",
        "faraway: the last word, from elsewhere",
    ] {
        assert!(bodies.contains(&body), "{body} is missing: {bodies:?}");
    }
    let topic = hana
        .get(&format!(
            "/_matrix/client/v3/rooms/{}/state/m.room.topic",
            urlencode(faraway)
        ))
        .await;
    assert_eq!(topic["topic"], "Faraway, on another server, with guests");
    let members = hana
        .get(&format!(
            "/_matrix/client/v3/rooms/{}/joined_members",
            urlencode(faraway)
        ))
        .await;
    for member in [
        "@rita:127.0.0.1:18302",
        "@hana:127.0.0.1:18301",
        "@hugo:127.0.0.1:18301",
    ] {
        assert!(members["joined"].get(member).is_some(), "{members}");
    }
    // It is a room here, not a copy of one: hana can talk in it.
    hana.expect(
        Method::PUT,
        &format!(
            "/_matrix/client/v3/rooms/{}/send/m.room.message/after-migration",
            urlencode(faraway)
        ),
        Some(json!({"msgtype": "m.text", "body": "Hello from Myelin"})),
        StatusCode::OK,
    )
    .await;

    // Elsewhere, which Synapse held whole: its history from the beginning.
    let messages = hana
        .get(&format!(
            "/_matrix/client/v3/rooms/{}/messages?dir=b&limit=100",
            urlencode(elsewhere)
        ))
        .await;
    let bodies: Vec<&str> = messages["chunk"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|e| e["content"]["body"].as_str())
        .collect();
    assert!(
        bodies.contains(&"elsewhere: before anyone from home joined, 1")
            && bodies.contains(&"elsewhere: the last word, from elsewhere"),
        "{bodies:?}"
    );
    let _ = hs.log();
    hs.stop();
}
