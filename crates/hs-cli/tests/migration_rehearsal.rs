//! A migration rehearsal against a real Synapse: the runbook's path, end to end, with real
//! client sessions on the other side.
//!
//! A Synapse (`mirror.gcr.io/matrixdotorg/synapse:latest`, 1.162 when this was written) on a
//! PostgreSQL 17, both in Docker, are populated through Synapse's own client and admin APIs the
//! way people and tools populate one: accounts, a phone signed in and a web session signed in
//! with a refresh token, the phone's end-to-end keys (identity, one-time and fallback), a key
//! backup with a room key, a public room with an alias, messages and a read receipt, a room
//! tag, account data, a filter, a push rule, a pusher, an upload and an avatar, an email
//! address and an upstream identity (admin API), a registration token (admin API), an erased
//! account (admin API), and a room-key request waiting for the phone, which does not sync
//! again.
//!
//! Then a fresh `hs` named as the Synapse, holding its signing key, is pointed at the live
//! Synapse's database and media store through the admin API and copies everything while
//! Synapse keeps running. More is written to Synapse after the copy (a message, a sign-up,
//! another to-device message): the delta the cutover's final pass must bring over. Synapse is
//! stopped (the runbook's "stop Synapse": nothing written to it from here on), the cutover
//! runs, and verification passes.
//!
//! On the other side: the phone's Synapse access token works; the web session exchanges its
//! Synapse refresh token for a new pair; the phone's keys are what `/keys/query` and
//! `/keys/claim` answer; the backup and its key are there; the room, its history including the
//! delta message, the alias and the public directory; the receipt; the two to-device messages
//! reach the phone's first sync; the tag, the account data, the filter, the push rule and the
//! pusher; the upload byte for byte and the avatar; the email address (and signing in by it),
//! the upstream identity, the erased account, the registration token (and registering with
//! it, on a closed server), the account that signed up after the copy, and the user directory.
//!
//! Needs Docker with both images present (`docker pull` from a session fails on the keychain;
//! the mirror and ECR images below pull without credentials from an operator's terminal).
//! Skips, saying so, without them. Every container and network it makes is named
//! `hs-rehearsal-<pid>-*` and removed when the test ends, however it ends.

use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

use reqwest::{Method, StatusCode};
use serde_json::{Value, json};

const SYNAPSE_IMAGE: &str = "mirror.gcr.io/matrixdotorg/synapse:latest";
const POSTGRES_IMAGE: &str = "public.ecr.aws/docker/library/postgres:17";
const SERVER_NAME: &str = "rehearsal.test";
const SHARED_SECRET: &str = "rehearsal-shared-secret";
const PG_PASSWORD: &str = "hspg";

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn docker(args: &[&str]) -> Result<String, String> {
    let output = Command::new("docker")
        .args(args)
        .output()
        .map_err(|e| format!("docker {}: {e}", args.join(" ")))?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
    } else {
        Err(format!(
            "docker {} failed: {}{}",
            args.join(" "),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        ))
    }
}

/// Whether Docker runs and both images are here; the reason when not.
fn docker_ready() -> Result<(), String> {
    docker(&["info"]).map_err(|_| "Docker is not available".to_owned())?;
    for image in [SYNAPSE_IMAGE, POSTGRES_IMAGE] {
        docker(&["image", "inspect", image])
            .map_err(|_| format!("the image {image} is not present (docker pull {image})"))?;
    }
    Ok(())
}

/// The containers and network of one run, removed on drop.
struct Containers {
    prefix: String,
}

impl Containers {
    fn name(&self, what: &str) -> String {
        format!("{}-{what}", self.prefix)
    }
}

impl Drop for Containers {
    fn drop(&mut self) {
        for what in ["synapse", "pg"] {
            let _ = docker(&["rm", "-f", "-v", &self.name(what)]);
        }
        let _ = docker(&["network", "rm", &self.name("net")]);
    }
}

/// One run of the real `hs` binary.
struct HsProcess {
    child: std::process::Child,
    lines: std::sync::mpsc::Receiver<String>,
    seen: Vec<String>,
}

impl HsProcess {
    fn serve(config_path: &Path) -> Self {
        use std::io::BufRead;
        let mut child = Command::new(env!("CARGO_BIN_EXE_hs"))
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

    fn stop(mut self) {
        let pid = self.child.id().to_string();
        let _ = Command::new("kill").args(["-TERM", &pid]).status();
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

    async fn post(&self, path: &str, body: Value) -> Value {
        self.expect(Method::POST, path, Some(body), StatusCode::OK)
            .await
    }

    async fn put(&self, path: &str, body: Value) -> Value {
        self.expect(Method::PUT, path, Some(body), StatusCode::OK)
            .await
    }

    async fn upload(&self, bytes: &[u8], content_type: &str) -> Value {
        let response = reqwest::Client::new()
            .post(format!(
                "{}/_matrix/media/v3/upload?filename=picture.png",
                self.base
            ))
            .bearer_auth(self.token.as_deref().unwrap())
            .header("content-type", content_type)
            .body(bytes.to_vec())
            .send()
            .await
            .unwrap();
        let status = response.status();
        let body: Value = response.json().await.unwrap_or(Value::Null);
        assert_eq!(status, StatusCode::OK, "{body}");
        body
    }

    async fn bytes(&self, path: &str) -> (StatusCode, Vec<u8>) {
        let mut request = reqwest::Client::new().get(format!("{}{path}", self.base));
        if let Some(token) = &self.token {
            request = request.bearer_auth(token);
        }
        let response = request.send().await.unwrap();
        (response.status(), response.bytes().await.unwrap().to_vec())
    }

    async fn migration_reaches(&self, want: &str) -> Value {
        let deadline = Instant::now() + Duration::from_secs(180);
        loop {
            let status = self.get("/api/v1/migration").await;
            if status["status"] == want {
                return status;
            }
            assert!(
                status["status"] != "failed" || want == "failed",
                "the migration failed: {status}"
            );
            assert!(Instant::now() < deadline, "never reached {want}: {status}");
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    }

    async fn task_ends(&self, id: &str) -> Value {
        let deadline = Instant::now() + Duration::from_secs(180);
        loop {
            let task = self.get(&format!("/api/v1/tasks/{id}")).await;
            if matches!(
                task["status"].as_str(),
                Some("succeeded" | "failed" | "cancelled")
            ) {
                return task;
            }
            assert!(Instant::now() < deadline, "the task never ended: {task}");
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    }
}

async fn wait_http(url: &str, what: &str) {
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        if let Ok(response) = reqwest::get(url).await
            && response.status().is_success()
        {
            return;
        }
        assert!(Instant::now() < deadline, "{what} never answered at {url}");
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

fn user(localpart: &str) -> String {
    format!("@{localpart}:{SERVER_NAME}")
}

fn enc(s: &str) -> String {
    s.replace('%', "%25")
        .replace('#', "%23")
        .replace('!', "%21")
        .replace(':', "%3A")
        .replace('@', "%40")
        .replace('$', "%24")
        .replace('/', "%2F")
}

/// Registers `localpart` on Synapse with a named device, answering its access token.
async fn register_on_synapse(synapse: &Caller, localpart: &str, device_id: &str) -> String {
    let body = synapse
        .post(
            "/_matrix/client/v3/register",
            json!({
                "username": localpart,
                "password": format!("{localpart}-password-1"),
                "device_id": device_id,
                "initial_device_display_name": format!("{localpart}'s {device_id}"),
                "auth": {"type": "m.login.dummy"},
            }),
        )
        .await;
    body["access_token"].as_str().unwrap().to_owned()
}

/// Synapse's shared-secret registration of an administrator (what `register_new_matrix_user`
/// does), through the same protocol this server's compat surface serves.
async fn register_admin_on_synapse(synapse: &Caller, localpart: &str) -> String {
    let nonce = synapse.get("/_synapse/admin/v1/register").await;
    let nonce = nonce["nonce"].as_str().unwrap();
    let password = format!("{localpart}-password-1");
    let mac = hs_compat::shared_secret::compute_mac(
        SHARED_SECRET.as_bytes(),
        nonce,
        localpart,
        &password,
        true,
        None,
    );
    let body = synapse
        .post(
            "/_synapse/admin/v1/register",
            json!({
                "nonce": nonce,
                "username": localpart,
                "password": password,
                "admin": true,
                "mac": mac,
            }),
        )
        .await;
    body["access_token"].as_str().unwrap().to_owned()
}

/// A 1x1 PNG.
const PNG: &[u8] = &[
    0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44, 0x52,
    0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1F, 0x15, 0xC4,
    0x89, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9C, 0x63, 0xF8, 0xCF, 0xC0, 0x00,
    0x00, 0x03, 0x01, 0x01, 0x00, 0x18, 0xDD, 0x8D, 0xB0, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4E,
    0x44, 0xAE, 0x42, 0x60, 0x82,
];

#[tokio::test]
async fn a_real_synapse_is_migrated_with_its_sessions_keys_and_a_delta_after_the_copy() {
    if let Err(why) = docker_ready() {
        eprintln!("SKIP: the migration rehearsal needs a real Synapse in Docker: {why}");
        return;
    }
    let containers = Containers {
        prefix: format!("hs-rehearsal-{}", std::process::id()),
    };
    let dir = tempfile::tempdir().unwrap();
    let synapse_dir = dir.path().join("synapse");
    std::fs::create_dir_all(&synapse_dir).unwrap();
    let (uid, gid) = owner_of(&synapse_dir);
    let uid = format!("UID={uid}");
    let gid = format!("GID={gid}");
    let net = containers.name("net");
    docker(&["network", "create", &net]).unwrap();

    // ---- PostgreSQL for Synapse, reachable from Synapse by name and from here by port.
    let pg_port = free_port().to_string();
    docker(&[
        "run",
        "-d",
        "--name",
        &containers.name("pg"),
        "--network",
        &net,
        "-e",
        &format!("POSTGRES_PASSWORD={PG_PASSWORD}"),
        "-e",
        "POSTGRES_INITDB_ARGS=--encoding=UTF8 --locale=C",
        "-p",
        &format!("127.0.0.1:{pg_port}:5432"),
        POSTGRES_IMAGE,
    ])
    .unwrap();
    let deadline = Instant::now() + Duration::from_secs(120);
    // Made in a retry loop rather than after a readiness probe: initdb's temporary server
    // answers `pg_isready` and even `select 1` before the real one is up.
    loop {
        match docker(&[
            "exec",
            &containers.name("pg"),
            "psql",
            "-U",
            "postgres",
            "-c",
            "CREATE DATABASE synapse ENCODING 'UTF8' LC_COLLATE 'C' LC_CTYPE 'C' TEMPLATE template0",
        ]) {
            Ok(_) => break,
            Err(e) if e.contains("already exists") => break,
            Err(e) => {
                assert!(
                    Instant::now() < deadline,
                    "PostgreSQL never became ready: {e}"
                );
                std::thread::sleep(Duration::from_millis(500));
            }
        }
    }

    // ---- Synapse: its own `generate` for the signing key, then a homeserver.yaml of ours.
    let synapse_data = synapse_dir.to_str().unwrap().to_owned();
    docker(&[
        "run",
        "--rm",
        "-e",
        &uid,
        "-e",
        &gid,
        "-v",
        &format!("{synapse_data}:/data"),
        "-e",
        &format!("SYNAPSE_SERVER_NAME={SERVER_NAME}"),
        "-e",
        "SYNAPSE_REPORT_STATS=no",
        SYNAPSE_IMAGE,
        "generate",
    ])
    .unwrap();
    let signing_key = synapse_dir.join(format!("{SERVER_NAME}.signing.key"));
    assert!(signing_key.is_file(), "generate wrote no signing key");
    std::fs::write(
        synapse_dir.join("homeserver.yaml"),
        format!(
            "server_name: \"{SERVER_NAME}\"\n\
             pid_file: /data/homeserver.pid\n\
             report_stats: false\n\
             signing_key_path: /data/{SERVER_NAME}.signing.key\n\
             media_store_path: /data/media\n\
             log_config: /data/log.yaml\n\
             database:\n  name: psycopg2\n  args:\n    user: postgres\n    password: {PG_PASSWORD}\n    database: synapse\n    host: {pg}\n    cp_min: 2\n    cp_max: 4\n\
             listeners:\n  - port: 8008\n    tls: false\n    type: http\n    x_forwarded: false\n    resources: [{{names: [client]}}]\n\
             registration_shared_secret: \"{SHARED_SECRET}\"\n\
             enable_registration: true\n\
             enable_registration_without_verification: true\n\
             refreshable_access_token_lifetime: 1h\n\
             trusted_key_servers: []\n\
             suppress_key_server_warning: true\n\
             room_list_publication_rules: [{{action: allow}}]\n\
             rc_message: {{per_second: 1000, burst_count: 1000}}\n\
             rc_registration: {{per_second: 1000, burst_count: 1000}}\n\
             rc_login: {{address: {{per_second: 1000, burst_count: 1000}}, account: {{per_second: 1000, burst_count: 1000}}, failed_attempts: {{per_second: 1000, burst_count: 1000}}}}\n\
             rc_joins: {{local: {{per_second: 1000, burst_count: 1000}}, remote: {{per_second: 1000, burst_count: 1000}}}}\n\
             rc_invites: {{per_room: {{per_second: 1000, burst_count: 1000}}, per_user: {{per_second: 1000, burst_count: 1000}}}}\n",
            pg = containers.name("pg"),
        ),
    )
    .unwrap();
    std::fs::write(
        synapse_dir.join("log.yaml"),
        "version: 1\nformatters: {precise: {format: '%(asctime)s - %(name)s - %(levelname)s - %(message)s'}}\n\
         handlers: {console: {class: logging.StreamHandler, formatter: precise}}\n\
         root: {level: WARNING, handlers: [console]}\n",
    )
    .unwrap();
    let synapse_port = free_port().to_string();
    docker(&[
        "run",
        "-d",
        "--name",
        &containers.name("synapse"),
        "--network",
        &net,
        "-e",
        &uid,
        "-e",
        &gid,
        "-p",
        &format!("127.0.0.1:{synapse_port}:8008"),
        "-v",
        &format!("{synapse_data}:/data"),
        SYNAPSE_IMAGE,
    ])
    .unwrap();
    let synapse = Caller {
        base: format!("http://127.0.0.1:{synapse_port}"),
        token: None,
    };
    wait_http(
        &format!("{}/_matrix/client/versions", synapse.base),
        "Synapse",
    )
    .await;

    // ---- People and their things, on Synapse.
    let alice_id = user("alice");
    let bob_id = user("bob");
    let ops_token = register_admin_on_synapse(&synapse, "ops").await;
    let ops_syn = synapse.with(&ops_token);
    let alice_phone_token = register_on_synapse(&synapse, "alice", "ALICEPHONE").await;
    let alice = synapse.with(&alice_phone_token);
    let bob_token = register_on_synapse(&synapse, "bob", "BOBLAPTOP").await;
    let bob = synapse.with(&bob_token);
    register_on_synapse(&synapse, "carol", "CAROLDESK").await;

    // A web session signed in with a refresh token (as Element Web and Element X do).
    let web = synapse
        .post(
            "/_matrix/client/v3/login",
            json!({
                "type": "m.login.password",
                "identifier": {"type": "m.id.user", "user": "alice"},
                "password": "alice-password-1",
                "device_id": "ALICEWEB",
                "initial_device_display_name": "alice's browser",
                "refresh_token": true,
            }),
        )
        .await;
    let web_access = web["access_token"].as_str().unwrap().to_owned();
    let web_refresh = web["refresh_token"]
        .as_str()
        .unwrap_or_else(|| panic!("Synapse handed out no refresh token: {web}"))
        .to_owned();

    // The phone's end-to-end keys: identity keys, three one-time keys, a fallback key.
    let device_keys = json!({
        "user_id": alice_id, "device_id": "ALICEPHONE",
        "algorithms": ["m.olm.v1.curve25519-aes-sha2", "m.megolm.v1.aes-sha2"],
        "keys": {
            "curve25519:ALICEPHONE": "alicephonecurve25519publickeyAAAAAAAAAAAAAA",
            "ed25519:ALICEPHONE": "alicephoneed25519publickeyBBBBBBBBBBBBBBBBB",
        },
        "signatures": {&alice_id: {"ed25519:ALICEPHONE": "selfsignature"}},
    });
    let uploaded = alice
        .post(
            "/_matrix/client/v3/keys/upload",
            json!({
                "device_keys": device_keys,
                "one_time_keys": {
                    "signed_curve25519:AAAAAQ": {"key": "otk1", "signatures": {&alice_id: {"ed25519:ALICEPHONE": "s1"}}},
                    "signed_curve25519:AAAAAg": {"key": "otk2", "signatures": {&alice_id: {"ed25519:ALICEPHONE": "s2"}}},
                    "signed_curve25519:AAAAAw": {"key": "otk3", "signatures": {&alice_id: {"ed25519:ALICEPHONE": "s3"}}},
                },
                "fallback_keys": {
                    "signed_curve25519:FALLBACK1": {"key": "fallback1", "fallback": true, "signatures": {&alice_id: {"ed25519:ALICEPHONE": "sf"}}},
                },
            }),
        )
        .await;
    assert_eq!(
        uploaded["one_time_key_counts"]["signed_curve25519"], 3,
        "{uploaded}"
    );

    // A key backup with one room key.
    let backup = alice
        .post(
            "/_matrix/client/v3/room_keys/version",
            json!({"algorithm": "m.megolm_backup.v1.curve25519-aes-sha2", "auth_data": {"public_key": "backuppublickey"}}),
        )
        .await;
    let backup_version = backup["version"].as_str().unwrap().to_owned();

    // A public room with an alias, messages, a receipt, a tag, account data, a filter, a push
    // rule, a pusher, an upload and an avatar.
    let created = alice
        .post(
            "/_matrix/client/v3/createRoom",
            json!({"name": "Rehearsal", "preset": "public_chat", "room_alias_name": "rehearsal", "visibility": "public"}),
        )
        .await;
    let room_id = created["room_id"].as_str().unwrap().to_owned();
    alice
        .put(
            &format!(
                "/_matrix/client/v3/room_keys/keys/{}/rehearsalsession?version={backup_version}",
                enc(&room_id)
            ),
            json!({"first_message_index": 0, "forwarded_count": 0, "is_verified": false,
                   "session_data": {"ciphertext": "c", "ephemeral": "e", "mac": "m"}}),
        )
        .await;
    bob.post(
        &format!(
            "/_matrix/client/v3/join/{}",
            enc(&format!("#rehearsal:{SERVER_NAME}"))
        ),
        json!({}),
    )
    .await;
    let mut event_ids = Vec::new();
    for (i, who) in [&alice, &bob, &alice].iter().enumerate() {
        let sent = who
            .put(
                &format!(
                    "/_matrix/client/v3/rooms/{}/send/m.room.message/t{i}",
                    enc(&room_id)
                ),
                json!({"msgtype": "m.text", "body": format!("message {i} before the copy")}),
            )
            .await;
        event_ids.push(sent["event_id"].as_str().unwrap().to_owned());
    }
    bob.post(
        &format!(
            "/_matrix/client/v3/rooms/{}/receipt/m.read/{}",
            enc(&room_id),
            enc(&event_ids[2])
        ),
        json!({}),
    )
    .await;
    alice
        .put(
            &format!(
                "/_matrix/client/v3/user/{}/rooms/{}/tags/m.favourite",
                enc(&alice_id),
                enc(&room_id)
            ),
            json!({"order": 0.5}),
        )
        .await;
    alice
        .put(
            &format!(
                "/_matrix/client/v3/user/{}/account_data/rehearsal.note",
                enc(&alice_id)
            ),
            json!({"note": "kept across the migration"}),
        )
        .await;
    let filter = alice
        .post(
            &format!("/_matrix/client/v3/user/{}/filter", enc(&alice_id)),
            json!({"room": {"timeline": {"limit": 5}}}),
        )
        .await;
    let filter_id = filter["filter_id"].as_str().unwrap().to_owned();
    alice
        .put(
            "/_matrix/client/v3/pushrules/global/content/rehearsal-keyword",
            json!({"pattern": "rehearsal", "actions": ["notify", {"set_tweak": "highlight"}]}),
        )
        .await;
    alice
        .post(
            "/_matrix/client/v3/pushers/set",
            json!({"kind": "http", "app_id": "test.rehearsal.app", "pushkey": "rehearsal-pushkey",
                   "app_display_name": "Rehearsal", "device_display_name": "alice's phone",
                   "lang": "en", "data": {"url": "http://127.0.0.1:1/_matrix/push/v1/notify"}}),
        )
        .await;
    let upload = alice.upload(PNG, "image/png").await;
    let picture = upload["content_uri"].as_str().unwrap().to_owned();
    alice
        .put(
            &format!("/_matrix/client/v3/profile/{}/avatar_url", enc(&alice_id)),
            json!({"avatar_url": picture}),
        )
        .await;

    // What an administrator's tools leave: an email address and an upstream identity on alice,
    // a registration token, carol erased.
    ops_syn
        .put(
            &format!("/_synapse/admin/v2/users/{}", enc(&alice_id)),
            json!({
                "threepids": [{"medium": "email", "address": format!("alice@{SERVER_NAME}")}],
                "external_ids": [{"auth_provider": "oidc-rehearsal", "external_id": "alice-at-the-provider"}],
            }),
        )
        .await;
    ops_syn
        .post(
            "/_synapse/admin/v1/registration_tokens/new",
            json!({"token": "rehearsal-token", "uses_allowed": 3}),
        )
        .await;
    ops_syn
        .post(
            &format!("/_synapse/admin/v1/deactivate/{}", enc(&user("carol"))),
            json!({"erase": true}),
        )
        .await;

    // A room-key request from bob's laptop waiting for alice's phone, which does not sync again.
    bob.put(
        "/_matrix/client/v3/sendToDevice/m.room_key_request/txn-before",
        json!({"messages": {&alice_id: {"ALICEPHONE": {
            "action": "request", "request_id": "req-before", "requesting_device_id": "BOBLAPTOP",
            "body": {"algorithm": "m.megolm.v1.aes-sha2", "room_id": room_id, "sender_key": "bobkey", "session_id": "rehearsalsession"},
        }}}}),
    )
    .await;

    // ---- This server, named as the Synapse, with its signing key.
    let keys = dir.path().join("keys");
    std::fs::create_dir_all(&keys).unwrap();
    std::fs::copy(
        &signing_key,
        keys.join(format!("{SERVER_NAME}.signing.key")),
    )
    .unwrap();
    let port = free_port();
    let config_path = dir.path().join("hs.yaml");
    std::fs::write(
        &config_path,
        format!(
            "server:\n  server_name: {SERVER_NAME}\n  signing_key_path: {keys:?}\n\
             listeners:\n  listeners:\n    - port: {port}\n      bind_addresses: [\"127.0.0.1\"]\n      resources: [client, federation, media, admin, health, metrics]\n\
             storage:\n  backend: embedded\n  data_dir: {:?}\n\
             media:\n  storage:\n    backend: local\n    path: {:?}\n\
             rate_limits:\n  enabled: false\n",
            dir.path().join("db"),
            dir.path().join("media"),
        ),
    )
    .unwrap();
    let mut hs = HsProcess::serve(&config_path);
    let setup_line = hs.wait_for("setup_link=");
    let setup_token = setup_line
        .rsplit_once("#token=")
        .unwrap()
        .1
        .trim()
        .to_owned();
    let here = Caller {
        base: format!("http://127.0.0.1:{port}"),
        token: None,
    };
    let created = here
        .expect(
            Method::POST,
            "/api/v1/setup",
            Some(json!({"setup_token": setup_token, "username": "myelin-ops", "password": "ops-password-1"})),
            StatusCode::CREATED,
        )
        .await;
    let ops = here.with(created["access_token"].as_str().unwrap());

    // Point at Synapse (its database by the host port, its media store on this disk).
    ops.expect(
        Method::PATCH,
        "/api/v1/config/migration",
        Some(json!({"synapse": {
            "database": {"host": "127.0.0.1", "port": pg_port.parse::<u16>().unwrap(),
                         "database": "synapse", "user": "postgres", "password": PG_PASSWORD},
            "media_store_path": synapse_dir.join("media"),
            "batch_size": 50,
        }})),
        StatusCode::OK,
    )
    .await;
    let started = ops
        .expect(
            Method::POST,
            "/api/v1/migration/start",
            None,
            StatusCode::OK,
        )
        .await;
    assert_eq!(started["status"], "copying", "{started}");
    let ready = ops.migration_reaches("ready_for_cutover").await;
    let log = ops.get("/api/v1/migration/log?limit=500").await;
    let problems: Vec<&str> = log["items"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|e| e["message"].as_str())
        .filter(|m| !m.starts_with("done:") && !m.starts_with("throughput:"))
        .collect();
    let stream = |status: &Value, name: &str| {
        status["streams"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["name"] == name)
            .cloned()
            .unwrap_or_else(|| panic!("no {name} stream in {status}"))
    };
    for name in [
        "users",
        "devices",
        "access_tokens",
        "refresh_tokens",
        "threepids",
        "external_ids",
        "e2e_keys",
        "key_backups",
        "to_device",
        "push_rules",
        "pushers",
        "filters",
        "registration_tokens",
        "rooms",
        "receipts",
        "media",
    ] {
        let s = stream(&ready, name);
        if s["failed_count"] != 0 {
            let dump = docker(&[
                "exec",
                &containers.name("pg"),
                "psql",
                "-U",
                "postgres",
                "-d",
                "synapse",
                "-c",
                "select event_id, type, room_id, outlier, rejection_reason, topological_ordering, stream_ordering, depth from events order by stream_ordering",
            ]);
            let rooms = docker(&[
                "exec",
                &containers.name("pg"),
                "psql",
                "-U",
                "postgres",
                "-d",
                "synapse",
                "-c",
                "select json from event_json where json like '%m.room.create%' limit 1",
            ]);
            panic!("{name}: {s}\n{problems:#?}\n{dump:?}\n{rooms:?}");
        }
        assert!(
            s["copied_count"].as_u64().unwrap() >= 1,
            "{name} copied nothing: {s}"
        );
    }
    assert_eq!(stream(&ready, "rooms")["copied_count"], 1, "{ready}");
    assert_eq!(stream(&ready, "to_device")["copied_count"], 1, "{ready}");

    // ---- The delta: Synapse is still in service, so people keep writing to it.
    let late = bob
        .put(
            &format!(
                "/_matrix/client/v3/rooms/{}/send/m.room.message/late",
                enc(&room_id)
            ),
            json!({"msgtype": "m.text", "body": "after the copy, before the cutover"}),
        )
        .await;
    let late_id = late["event_id"].as_str().unwrap().to_owned();
    register_on_synapse(&synapse, "dave", "DAVEPHONE").await;
    bob.put(
        "/_matrix/client/v3/sendToDevice/m.room_key_request/txn-after",
        json!({"messages": {&alice_id: {"ALICEPHONE": {
            "action": "request_cancellation", "request_id": "req-before", "requesting_device_id": "BOBLAPTOP",
        }}}}),
    )
    .await;

    // ---- Cutover: stop Synapse (the runbook's step 1: nothing written to it from here on),
    // the final pass, verification, completed.
    docker(&["stop", "-t", "30", &containers.name("synapse")]).unwrap();
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
    assert_eq!(done["verification"]["passed"], true, "{done}");
    assert_eq!(stream(&done, "to_device")["copied_count"], 2, "{done}");
    assert_eq!(stream(&done, "users")["copied_count"], 5, "{done}");

    // Pointing clients here is the operator's DNS flip; here they were on 127.0.0.1 all along.
    // A restart in between, as a deployment has one.
    hs.stop();
    let mut hs = HsProcess::serve(&config_path);
    hs.wait_for("listening");

    // ---- The other side.
    let alice = here.with(&alice_phone_token);
    let whoami = alice.get("/_matrix/client/v3/account/whoami").await;
    assert_eq!(whoami["user_id"], alice_id, "{whoami}");
    assert_eq!(whoami["device_id"], "ALICEPHONE", "{whoami}");

    // The web session: its Synapse access token works, and when it expires the Synapse
    // refresh token buys a new pair here.
    let web_here = here.with(&web_access);
    assert_eq!(
        web_here.get("/_matrix/client/v3/account/whoami").await["device_id"],
        "ALICEWEB"
    );
    let refreshed = here
        .post(
            "/_matrix/client/v3/refresh",
            json!({"refresh_token": web_refresh}),
        )
        .await;
    let renewed = here.with(refreshed["access_token"].as_str().unwrap());
    assert_eq!(
        renewed.get("/_matrix/client/v3/account/whoami").await["device_id"],
        "ALICEWEB"
    );
    let (status, _) = web_here
        .call(Method::GET, "/_matrix/client/v3/account/whoami", None)
        .await;
    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "the exchanged access token still works"
    );

    // The phone's keys, as other people's clients will find them.
    let bob_here = here.with(&bob_token);
    let query = bob_here
        .post(
            "/_matrix/client/v3/keys/query",
            json!({"device_keys": {&alice_id: []}}),
        )
        .await;
    let phone = &query["device_keys"][&alice_id]["ALICEPHONE"];
    assert_eq!(
        phone["keys"]["ed25519:ALICEPHONE"], "alicephoneed25519publickeyBBBBBBBBBBBBBBBBB",
        "{query}"
    );
    let claimed = bob_here
        .post(
            "/_matrix/client/v3/keys/claim",
            json!({"one_time_keys": {&alice_id: {"ALICEPHONE": "signed_curve25519"}}}),
        )
        .await;
    assert_eq!(
        claimed["one_time_keys"][&alice_id]["ALICEPHONE"]["signed_curve25519:AAAAAQ"]["key"],
        "otk1",
        "the first one-time key Synapse would have handed out: {claimed}"
    );
    let version = alice.get("/_matrix/client/v3/room_keys/version").await;
    assert_eq!(version["version"], backup_version, "{version}");
    assert_eq!(version["count"], 1, "{version}");
    let room_key = alice
        .get(&format!(
            "/_matrix/client/v3/room_keys/keys/{}/rehearsalsession?version={backup_version}",
            enc(&room_id)
        ))
        .await;
    assert_eq!(room_key["session_data"]["ciphertext"], "c", "{room_key}");

    // The room: its history including the delta message, the alias, the directory, bob's
    // receipt, the tag, the account data, the filter, the push rule, the pusher.
    let sync = alice.get("/_matrix/client/v3/sync").await;
    assert!(sync["rooms"]["join"].get(&room_id).is_some(), "{sync}");
    let messages = alice
        .get(&format!(
            "/_matrix/client/v3/rooms/{}/messages?dir=b&limit=50",
            enc(&room_id)
        ))
        .await;
    let bodies: Vec<&str> = messages["chunk"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|e| e["content"]["body"].as_str())
        .collect();
    assert!(
        bodies.contains(&"message 0 before the copy")
            && bodies.contains(&"after the copy, before the cutover"),
        "{bodies:?}"
    );
    assert!(
        messages["chunk"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["event_id"] == late_id),
        "the delta message is not there by id"
    );
    let resolved = alice
        .get(&format!(
            "/_matrix/client/v3/directory/room/{}",
            enc(&format!("#rehearsal:{SERVER_NAME}"))
        ))
        .await;
    assert_eq!(resolved["room_id"], room_id, "{resolved}");
    let directory = alice.get("/_matrix/client/v3/publicRooms").await;
    assert!(
        directory["chunk"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["room_id"] == room_id),
        "the public room is not in the directory: {directory}"
    );
    let receipts: Vec<Value> = sync["rooms"]["join"][&room_id]["ephemeral"]["events"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    assert!(
        receipts.iter().any(|e| e["type"] == "m.receipt"
            && e["content"][&event_ids[2]]["m.read"].get(&bob_id).is_some()),
        "bob's receipt is missing: {receipts:?}"
    );
    let tags = alice
        .get(&format!(
            "/_matrix/client/v3/user/{}/rooms/{}/tags",
            enc(&alice_id),
            enc(&room_id)
        ))
        .await;
    assert_eq!(tags["tags"]["m.favourite"]["order"], 0.5, "{tags}");
    let note = alice
        .get(&format!(
            "/_matrix/client/v3/user/{}/account_data/rehearsal.note",
            enc(&alice_id)
        ))
        .await;
    assert_eq!(note["note"], "kept across the migration", "{note}");
    let filter_here = alice
        .get(&format!(
            "/_matrix/client/v3/user/{}/filter/{filter_id}",
            enc(&alice_id)
        ))
        .await;
    assert_eq!(filter_here["room"]["timeline"]["limit"], 5, "{filter_here}");
    let rules = alice.get("/_matrix/client/v3/pushrules/").await;
    assert!(
        rules["global"]["content"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["rule_id"] == "rehearsal-keyword" && r["pattern"] == "rehearsal"),
        "{rules}"
    );
    let pushers = alice.get("/_matrix/client/v3/pushers").await;
    assert!(
        pushers["pushers"]
            .as_array()
            .unwrap()
            .iter()
            .any(|p| p["pushkey"] == "rehearsal-pushkey" && p["app_id"] == "test.rehearsal.app"),
        "{pushers}"
    );

    // The two room-key messages that were waiting for the phone (one from before the copy,
    // one from after) are in its first sync here, in order.
    let waiting: Vec<Value> = sync["to_device"]["events"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let actions: Vec<&str> = waiting
        .iter()
        .filter(|e| e["type"] == "m.room_key_request" && e["sender"] == bob_id)
        .filter_map(|e| e["content"]["action"].as_str())
        .collect();
    assert_eq!(actions, ["request", "request_cancellation"], "{waiting:?}");

    // The upload, byte for byte, and the avatar that points at it.
    let media_path = picture.trim_start_matches("mxc://");
    let (status, bytes) = alice
        .bytes(&format!("/_matrix/client/v1/media/download/{media_path}"))
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(bytes, PNG);
    let profile = alice
        .get(&format!("/_matrix/client/v3/profile/{}", enc(&alice_id)))
        .await;
    assert_eq!(profile["avatar_url"], picture, "{profile}");

    // The email address: shown, and good to sign in by. The upstream identity, linked. Carol,
    // erased. The registration token, usable on this closed server. Dave, who signed up after
    // the copy, signs in. Bob is in the user directory.
    let threepids = alice.get("/_matrix/client/v3/account/3pid").await;
    assert!(
        threepids["threepids"]
            .as_array()
            .unwrap()
            .iter()
            .any(|t| t["address"] == format!("alice@{SERVER_NAME}")),
        "{threepids}"
    );
    here.post(
        "/_matrix/client/v3/login",
        json!({
            "type": "m.login.password",
            "identifier": {"type": "m.id.thirdparty", "medium": "email", "address": format!("alice@{SERVER_NAME}")},
            "password": "alice-password-1",
        }),
    )
    .await;
    let found = ops
        .get("/api/v1/users/lookup?provider=oidc-rehearsal&external_id=alice-at-the-provider")
        .await;
    assert_eq!(found["user_id"], alice_id, "{found}");
    let carol = ops
        .get(&format!("/api/v1/users/{}", enc(&user("carol"))))
        .await;
    assert_eq!(carol["erased"], true, "{carol}");
    assert_eq!(carol["deactivated"], true, "{carol}");
    let (status, flows) = here
        .call(
            Method::POST,
            "/_matrix/client/v3/register",
            Some(json!({"username": "erin", "password": "erin-password-1"})),
        )
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{flows}");
    here.post(
        "/_matrix/client/v3/register",
        json!({
            "username": "erin", "password": "erin-password-1",
            "auth": {"type": "m.login.registration_token", "token": "rehearsal-token", "session": flows["session"]},
        }),
    )
    .await;
    let token = ops.get("/api/v1/registration-tokens/rehearsal-token").await;
    assert_eq!(token["completed"], 1, "{token}");
    here.post(
        "/_matrix/client/v3/login",
        json!({
            "type": "m.login.password",
            "identifier": {"type": "m.id.user", "user": "dave"},
            "password": "dave-password-1",
        }),
    )
    .await;
    let search = alice
        .post(
            "/_matrix/client/v3/user_directory/search",
            json!({"search_term": "bob"}),
        )
        .await;
    assert!(
        search["results"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["user_id"] == bob_id),
        "{search}"
    );

    // And through the Synapse admin surface, what synapse-admin's user page shows.
    let record = ops
        .get(&format!("/_synapse/admin/v2/users/{}", enc(&alice_id)))
        .await;
    assert_eq!(record["name"], alice_id, "{record}");
    assert_eq!(record["avatar_url"], picture, "{record}");

    hs.stop();
    drop(containers);
}

/// The owner of `path` (this process's user and group, for a directory it just made): the
/// Synapse image's `UID`/`GID`, so the files Synapse writes -- the signing key, the media store --
/// are ours to read.
fn owner_of(path: &Path) -> (u32, u32) {
    use std::os::unix::fs::MetadataExt;
    let metadata = std::fs::metadata(path).expect("the directory just made has metadata");
    (metadata.uid(), metadata.gid())
}
