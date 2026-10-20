//! The real matrimail (the Matrix <-> email bridge, on mautrix-go's `bridgev2`) against the
//! real `hs` binary, from the image this repository builds for it (decision 0055): an
//! administrator offers it, an instance is made for alice, the server renders her bridge's
//! files (`config.yaml`, `registration.yaml` and the credentials `passphrase` it minted), the
//! image runs from exactly those files and comes up `ready`, her chat with her bot appears, she
//! types `help` and the bot answers with its commands (`login` among them), and the admin API's
//! `logins` route asks the bridge's provisioning API as it asks a mautrix bridge's. Encrypted
//! chat and plain chat, as `real_mautrix_login.rs` runs them.
//!
//! No mailbox is signed in to: that needs an IMAP server with an account, and the story stops
//! where the catalogue's sign-in steps start. What it proves is the part Myelin is responsible
//! for: the image starts, reads the rendered config as it is (the `bridgev2` layout, port
//! 29339, the passphrase file under `/data`), registers, and answers.
//!
//! Skipped, saying why, when Docker cannot be reached, the image cannot be had, or the `hs`
//! binary is not built (`cargo build -p hs-cli --bin hs`; `HS_BIN` names it explicitly). The
//! image is the catalogue's pin ([`IMAGE`]); on a desktop it is built locally first
//! (`docker build -f deploy/bridges/matrimail/Dockerfile -t <IMAGE> deploy/bridges/matrimail`),
//! on CI it is pulled from ghcr.io once `.github/workflows/matrimail-image.yml` has pushed it.
//! `HS_BRIDGE_LOGIN_LOG_DIR` names a directory to copy the server's and the bridge's logs into.

use std::io::BufRead;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use matrix_sdk::Client;
use matrix_sdk::config::SyncSettings;
use matrix_sdk::deserialized_responses::{TimelineEvent, TimelineEventKind};
use matrix_sdk::ruma::api::client::account::register::v3::Request as RegisterRequest;
use matrix_sdk::ruma::api::client::uiaa;
use matrix_sdk::ruma::events::room::message::RoomMessageEventContent;
use matrix_sdk::ruma::{OwnedRoomId, RoomId};
use matrix_sdk_crypto::CollectStrategy;
use serde_json::{Value, json};

/// The catalogue's pin for matrimail (`crates/hs-admin/src/bridge_types.rs`, `MATRIMAIL_IMAGE`);
/// the two are bumped together, and the story fails when they differ.
const IMAGE: &str = "ghcr.io/brandon-dacrib/matrimail:24cd0f9";
const TYPE_ID: &str = "matrimail";
/// The port the rendered config says (`MATRIMAIL_PORT`), not the connector's own default.
const PORT: u16 = 29339;
const SERVER_NAME: &str = "test.local";
/// How long the bridge gets to answer `help`.
const REPLY_WAIT: Duration = Duration::from_secs(30);

/// One run at a time: each boots a server and a container, and the machine is shared.
static ONE_AT_A_TIME: Mutex<()> = Mutex::new(());

// ---- the preconditions ---------------------------------------------------------------------

fn hs_binary() -> Option<PathBuf> {
    if let Ok(path) = std::env::var("HS_BIN") {
        let path = PathBuf::from(path);
        return path.is_file().then_some(path);
    }
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent()?.parent()?;
    ["debug", "release"]
        .iter()
        .map(|profile| root.join("target").join(profile).join("hs"))
        .find(|p| p.is_file())
}

fn docker(args: &[&str]) -> Result<String> {
    let out = Command::new("docker")
        .args(args)
        .stdin(Stdio::null())
        .output()
        .with_context(|| format!("running docker {}", args.join(" ")))?;
    if !out.status.success() {
        bail!(
            "docker {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

/// `None` when the test can run; otherwise why it cannot.
fn skip_reason() -> Option<String> {
    if hs_binary().is_none() {
        return Some(
            "no hs binary under target/{debug,release}; run `cargo build -p hs-cli --bin hs` or set HS_BIN"
                .into(),
        );
    }
    if let Err(e) = docker(&["version", "--format", "{{.Server.Version}}"]) {
        return Some(format!("Docker is not reachable: {e}"));
    }
    if docker(&["image", "inspect", IMAGE, "--format", "{{.Id}}"]).is_err()
        && let Err(e) = docker(&["pull", IMAGE])
    {
        return Some(format!(
            "could not pull {IMAGE} (build it: docker build -f deploy/bridges/matrimail/Dockerfile -t {IMAGE} deploy/bridges/matrimail): {e}"
        ));
    }
    None
}

// ---- the server ------------------------------------------------------------------------------

struct Server {
    child: std::process::Child,
    lines: Arc<Mutex<Vec<String>>>,
    base: String,
}

impl Server {
    /// Starts `hs serve` on `listener`'s port, handing it the listener still bound. The public
    /// base URL is how the bridge container reaches the server (`host.docker.internal`).
    fn start(dir: &Path, listener: hs_testkit::ports::ReservedPort) -> Result<Self> {
        let port = listener.port();
        let data = dir.join("data");
        let media = dir.join("media");
        let config = format!(
            "server: {{ server_name: {SERVER_NAME}, public_baseurl: \"http://host.docker.internal:{port}\" }}\n\
             listeners: {{ listeners: [ {{ port: {port}, bind_addresses: [\"0.0.0.0\"], resources: [client, media, admin, health, metrics] }} ] }}\n\
             storage: {{ backend: embedded, data_dir: {data:?} }}\n\
             media: {{ storage: {{ backend: local, path: {media:?} }} }}\n\
             auth: {{ enable_registration: true }}\n\
             rate_limits: {{ enabled: false }}\n"
        );
        let path = dir.join("homeserver.yaml");
        std::fs::write(&path, config)?;
        let mut command = Command::new(hs_binary().context("hs binary")?);
        command
            .args(["serve", "-c"])
            .arg(&path)
            .env("RUST_LOG", "info,hs_appservice=debug,hs_bridges=debug")
            .env_remove("HS_DATA_DIR")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        hs_testkit::ports::pass_to(&mut command, [listener]);
        let mut child = command.spawn().context("starting hs serve")?;
        drop(command);
        let lines = Arc::new(Mutex::new(Vec::new()));
        for reader in [
            Box::new(child.stdout.take().context("stdout")?) as Box<dyn std::io::Read + Send>,
            Box::new(child.stderr.take().context("stderr")?),
        ] {
            let lines = lines.clone();
            std::thread::spawn(move || {
                for line in std::io::BufReader::new(reader)
                    .lines()
                    .map_while(Result::ok)
                {
                    lines.lock().unwrap().push(line);
                }
            });
        }
        Ok(Self {
            child,
            lines,
            base: format!("http://127.0.0.1:{port}"),
        })
    }

    fn log(&self) -> String {
        self.lines.lock().unwrap().join("\n")
    }

    fn wait_for_line(&self, needle: &str, timeout: Duration) -> Result<String> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(line) = self
                .lines
                .lock()
                .unwrap()
                .iter()
                .find(|l| l.contains(needle))
            {
                return Ok(line.clone());
            }
            if Instant::now() > deadline {
                bail!(
                    "the server never logged {needle:?}; its log:\n{}",
                    self.log()
                );
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

// ---- the administrator -------------------------------------------------------------------------

struct Admin {
    http: reqwest::Client,
    base: String,
    token: String,
}

impl Admin {
    async fn call(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<Value>,
    ) -> Result<(u16, Value)> {
        let mut req = self
            .http
            .request(method.clone(), format!("{}/api/v1{path}", self.base))
            .bearer_auth(&self.token)
            .header("accept", "application/json");
        if let Some(body) = body {
            req = req.json(&body);
        }
        let res = req
            .send()
            .await
            .with_context(|| format!("{method} {path}"))?;
        let status = res.status().as_u16();
        let text = res.text().await?;
        Ok((status, serde_json::from_str(&text).unwrap_or(Value::Null)))
    }

    async fn ok(&self, method: reqwest::Method, path: &str, body: Option<Value>) -> Result<Value> {
        let (status, json) = self.call(method.clone(), path, body).await?;
        if !(200..300).contains(&status) {
            bail!("{method} {path} answered {status}: {json}");
        }
        Ok(json)
    }
}

async fn claim_server(server: &Server) -> Result<Admin> {
    let line = server.wait_for_line("setup_link=", Duration::from_secs(120))?;
    let setup_token: String = line
        .split_once("#token=")
        .context("setup link")?
        .1
        .chars()
        .take_while(char::is_ascii_alphanumeric)
        .collect();
    let http = reqwest::Client::new();
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        if let Ok(r) = http
            .get(format!("{}/health/live", server.base))
            .send()
            .await
            && r.status().is_success()
        {
            break;
        }
        if Instant::now() > deadline {
            bail!("the server never became healthy");
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let session: Value = http
        .post(format!("{}/api/v1/setup", server.base))
        .json(&json!({"setup_token": setup_token, "username": "ops", "password": "ops-password-12345"}))
        .send()
        .await?
        .json()
        .await?;
    let token = session["access_token"]
        .as_str()
        .with_context(|| format!("setup answered {session}"))?
        .to_owned();
    Ok(Admin {
        http,
        base: server.base.clone(),
        token,
    })
}

// ---- the person --------------------------------------------------------------------------------

/// Alice, on a client that excludes insecure devices (`real_mautrix_login.rs` says why): the
/// bot's device has to be cross-signed by the manager for her encrypted `help` to reach it.
async fn register(base: &str, username: &str) -> Result<Client> {
    let client = Client::builder()
        .homeserver_url(base)
        .with_room_key_recipient_strategy(CollectStrategy::IdentityBasedStrategy)
        .build()
        .await?;
    let mut request = RegisterRequest::new();
    request.username = Some(username.to_owned());
    request.password = Some("a-password-for-the-test".to_owned());
    request.initial_device_display_name = Some("hs-bridge-conformance".to_owned());
    request.auth = Some(uiaa::AuthData::Dummy(uiaa::Dummy::new()));
    client.matrix_auth().register(request).await?;
    client
        .encryption()
        .bootstrap_cross_signing(None)
        .await
        .context("alice's own cross-signing")?;
    Ok(client)
}

/// `(how, json)`: `decrypted`, `utd` or `plaintext`, with the content to read.
fn classify(event: &TimelineEvent) -> (&'static str, Value) {
    match &event.kind {
        TimelineEventKind::Decrypted(d) => (
            "decrypted",
            serde_json::from_str(d.event.json().get()).unwrap_or(Value::Null),
        ),
        TimelineEventKind::UnableToDecrypt { event, utd_info } => {
            let mut v: Value = serde_json::from_str(event.json().get()).unwrap_or(Value::Null);
            v["_utd_reason"] = Value::String(format!("{:?}", utd_info.reason));
            ("utd", v)
        }
        TimelineEventKind::PlainText { event } => (
            "plaintext",
            serde_json::from_str(event.json().get()).unwrap_or(Value::Null),
        ),
    }
}

struct Reply {
    how: &'static str,
    event: Value,
}

/// What the bot says in `room` after alice's own event `after_event_id`, within `wait`.
async fn wait_for_reply(
    client: &Client,
    since: &mut String,
    room: &RoomId,
    bot: &str,
    after_event_id: &str,
    wait: Duration,
) -> Result<Vec<Reply>> {
    let deadline = Instant::now() + wait;
    let mut seen_ours = false;
    let mut replies = Vec::new();
    while Instant::now() < deadline {
        let response = client
            .sync_once(
                SyncSettings::default()
                    .token(since.clone())
                    .timeout(Duration::from_secs(3)),
            )
            .await?;
        *since = response.next_batch.clone();
        if let Some(joined) = response.rooms.joined.get(room) {
            for event in &joined.timeline.events {
                let id = event.event_id().map(|e| e.to_string()).unwrap_or_default();
                if id == after_event_id {
                    seen_ours = true;
                    continue;
                }
                let (how, json) = classify(event);
                if json["sender"].as_str() == Some(bot) && json["type"] != "m.room.member" {
                    replies.push(Reply { how, event: json });
                }
            }
        }
        if !replies.is_empty() && seen_ours {
            return Ok(replies);
        }
    }
    Ok(replies)
}

// ---- the scene -----------------------------------------------------------------------------------

struct Bridge {
    name: String,
}

impl Bridge {
    fn log(&self) -> String {
        Command::new("docker")
            .args(["logs", &self.name])
            .output()
            .map(|o| {
                format!(
                    "{}{}",
                    String::from_utf8_lossy(&o.stdout),
                    String::from_utf8_lossy(&o.stderr)
                )
            })
            .unwrap_or_default()
    }

    /// A file under the bridge's `/data`, read from inside the container (the bridge's user
    /// owns it there; the host copy may be unreadable on Linux).
    fn read_file(&self, path: &str) -> Result<String> {
        docker(&["exec", &self.name, "cat", path])
            .with_context(|| format!("reading {path} from the bridge container"))
    }
}

impl Drop for Bridge {
    fn drop(&mut self) {
        let _ = docker(&["rm", "-f", "-v", &self.name]);
    }
}

/// A port for the bridge container's published port. Docker binds it on the host, and cannot be
/// handed a bound socket, so this one is still picked by binding port 0 and releasing it.
fn reserve_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .and_then(|l| l.local_addr())
        .map(|a| a.port())
        .unwrap_or(0)
}

/// The server, the administrator, alice, her instance and its bridge, up and ready.
struct Scene {
    label: &'static str,
    _dir: tempfile::TempDir,
    server: Server,
    admin: Admin,
    alice: Client,
    alice_id: String,
    since: String,
    instance_path: String,
    appservice_id: String,
    bot: String,
    /// The passphrase the server minted for alice's bridge, as its files carried it.
    passphrase: String,
    bridge: Bridge,
}

impl Scene {
    fn keep_logs(&self) {
        if let Ok(dir) = std::env::var("HS_BRIDGE_LOGIN_LOG_DIR") {
            let dir = Path::new(&dir);
            let _ = std::fs::create_dir_all(dir);
            let _ = std::fs::write(
                dir.join(format!("matrimail-{}-hs.log", self.label)),
                self.server.log(),
            );
            let _ = std::fs::write(
                dir.join(format!("matrimail-{}-bridge.log", self.label)),
                self.bridge.log(),
            );
        }
    }

    async fn sync(&mut self, timeout: Duration) -> Result<matrix_sdk::sync::SyncResponse> {
        let response = self
            .alice
            .sync_once(
                SyncSettings::default()
                    .token(self.since.clone())
                    .timeout(timeout),
            )
            .await?;
        self.since = response.next_batch.clone();
        Ok(response)
    }
}

/// Everything up to "ready": the offering with `options`, alice's instance, its files (every
/// one the `files` map carries: the config, the registration and the passphrase), the
/// container from the image the offering names, the registration pointed at it.
async fn set_up(label: &'static str, options: Value) -> Result<Scene> {
    let dir = tempfile::tempdir()?;
    let server = Server::start(dir.path(), hs_testkit::ports::reserve_wildcard())?;
    let admin = claim_server(&server).await?;
    let alice = register(&server.base, "alice").await?;
    let alice_id = alice.user_id().context("alice's id")?.to_string();
    let since = alice.sync_once(SyncSettings::default()).await?.next_batch;

    let offering = admin
        .ok(
            reqwest::Method::PUT,
            &format!("/bridge-offerings/{TYPE_ID}"),
            Some(json!({"runtime": "elsewhere", "options": options})),
        )
        .await?;
    if offering["image"].as_str() != Some(IMAGE) {
        bail!(
            "the offering names {} but this test runs {IMAGE}: bump the catalogue's pin and this test together",
            offering["image"]
        );
    }
    let instance_path = format!(
        "/bridge-offerings/{TYPE_ID}/instances/{}",
        alice_id.replace('@', "%40").replace(':', "%3A")
    );
    admin.ok(reqwest::Method::PUT, &instance_path, None).await?;
    let deadline = Instant::now() + Duration::from_secs(60);
    let instance = loop {
        let instance = admin.ok(reqwest::Method::GET, &instance_path, None).await?;
        if instance["appservice_id"].is_string() && instance["state"] != "requested" {
            break instance;
        }
        if Instant::now() > deadline {
            bail!("the instance never registered: {instance}");
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    };
    let appservice_id = instance["appservice_id"]
        .as_str()
        .unwrap_or_default()
        .to_owned();
    let bot = instance["bot"].as_str().unwrap_or_default().to_owned();
    let files = admin
        .ok(
            reqwest::Method::POST,
            &format!("{instance_path}/files"),
            None,
        )
        .await?;
    let map = files["files"]
        .as_object()
        .with_context(|| format!("the files answer has a `files` map: {files}"))?;
    let mut names: Vec<&String> = map.keys().collect();
    names.sort();
    if names != ["config.yaml", "passphrase", "registration.yaml"] {
        bail!("the files of a matrimail instance: {names:?}");
    }
    let passphrase = map["passphrase"].as_str().unwrap_or_default().to_owned();
    if passphrase.len() != 64 || !passphrase.chars().all(|c| c.is_ascii_hexdigit()) {
        bail!("the passphrase is 64 hex characters, not {passphrase:?}");
    }
    let bridge_dir = dir.path().join("bridge");
    std::fs::create_dir_all(&bridge_dir)?;
    for (name, contents) in map {
        std::fs::write(bridge_dir.join(name), contents.as_str().unwrap_or_default())?;
    }
    // The container chowns /data to its own user and writes its database there.
    let _ = Command::new("chmod").arg("777").arg(&bridge_dir).status();

    let bridge_port = reserve_port();
    let name = format!("hs-bridge-matrimail-{label}-{}", std::process::id());
    // `host.docker.internal` is how the bridge's config names this server; Docker on Linux
    // resolves it only with this flag (`real_mautrix_login.rs`).
    docker(&[
        "run",
        "-d",
        "--name",
        &name,
        "--add-host",
        "host.docker.internal:host-gateway",
        "-p",
        &format!("127.0.0.1:{bridge_port}:{PORT}"),
        "-v",
        &format!("{}:/data", bridge_dir.display()),
        IMAGE,
    ])?;
    let bridge = Bridge { name };
    admin
        .ok(
            reqwest::Method::PATCH,
            &format!("/appservices/{appservice_id}"),
            Some(json!({"url": format!("http://127.0.0.1:{bridge_port}")})),
        )
        .await?;
    let scene = Scene {
        label,
        _dir: dir,
        server,
        admin,
        alice,
        alice_id,
        since,
        instance_path,
        appservice_id,
        bot,
        passphrase,
        bridge,
    };
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        let instance = scene
            .admin
            .ok(reqwest::Method::GET, &scene.instance_path, None)
            .await?;
        if instance["state"] == "ready" {
            break;
        }
        if instance["state"] == "failed" || Instant::now() > deadline {
            scene.keep_logs();
            bail!(
                "the instance never became ready: {instance}\n--- bridge log ---\n{}",
                scene.bridge.log()
            );
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    Ok(scene)
}

/// Alice's chat with her bot appears (started as her by the manager, or an invitation from
/// the bot that she accepts). Returns the room once the join and the bot's first words settled.
async fn join_chat(scene: &mut Scene) -> Result<OwnedRoomId> {
    let deadline = Instant::now() + Duration::from_secs(60);
    let room_id: OwnedRoomId = loop {
        let response = scene.sync(Duration::from_secs(2)).await?;
        if let Some(room_id) = response.rooms.invited.keys().next() {
            scene.alice.join_room_by_id(room_id).await?;
            break room_id.clone();
        }
        let bot = scene.bot.clone();
        if let Some((room_id, _)) = response.rooms.joined.iter().find(|(_, joined)| {
            joined.timeline.events.iter().any(|event| {
                let (_, json) = classify(event);
                json["sender"].as_str() == Some(&bot) && json["type"] == "m.room.message"
            })
        }) {
            break room_id.clone();
        }
        if Instant::now() > deadline {
            scene.keep_logs();
            bail!("no chat with {} ever reached alice", scene.bot);
        }
    };
    scene.sync(Duration::from_secs(2)).await?;
    Ok(room_id)
}

/// Alice types `help` in `room_id` and the bot answers with its commands, `login` among them.
async fn help_in(scene: &mut Scene, room_id: &RoomId) -> Result<()> {
    let label = scene.label;
    let bot = scene.bot.clone();
    // One sync first, so that alice's client has seen the bot's device signed (the key would
    // otherwise be withheld as `m.unverified`; `real_mautrix_login.rs`).
    scene.sync(Duration::from_secs(1)).await?;
    let room = scene.alice.get_room(room_id).context("the joined room")?;
    let is_encrypted = room.latest_encryption_state().await?.is_encrypted();
    let text = "help";
    let sent = room
        .send(RoomMessageEventContent::text_plain(text))
        .await
        .with_context(|| format!("sending `{text}`"))?;
    let sent_id = sent.response.event_id.to_string();
    eprintln!(
        "[{label}] alice sent `{text}` as {sent_id} in {room_id} (encrypted: {is_encrypted})"
    );
    let replies = wait_for_reply(
        &scene.alice,
        &mut scene.since,
        room_id,
        &bot,
        &sent_id,
        REPLY_WAIT,
    )
    .await?;
    scene.keep_logs();
    for r in &replies {
        eprintln!(
            "[{label}] {bot} answered ({}): type={} msgtype={} body={}",
            r.how, r.event["type"], r.event["content"]["msgtype"], r.event["content"]["body"]
        );
    }
    if replies.is_empty() {
        bail!(
            "{bot} said nothing within {REPLY_WAIT:?} of `{text}` ({label} chat)\n--- bridge log (tail) ---\n{}\n--- server log (tail) ---\n{}",
            tail(&scene.bridge.log(), 60),
            tail(&scene.server.log(), 60)
        );
    }
    let help = replies.iter().any(|r| {
        r.how != "utd"
            && r.event["content"]["body"]
                .as_str()
                .is_some_and(|b| b.contains("login") && b.contains("logout"))
    });
    if !help {
        bail!(
            "{bot} answered, but not with its commands: {:?}\n--- bridge log (tail) ---\n{}",
            replies
                .iter()
                .map(|r| (r.how, r.event["content"].clone()))
                .collect::<Vec<_>>(),
            tail(&scene.bridge.log(), 60)
        );
    }
    Ok(())
}

/// The whole story for one chat, encrypted or not.
async fn help(encrypted: bool) -> Result<()> {
    let label = if encrypted { "encrypted" } else { "plain" };
    let mut scene = set_up(label, json!({"encryption": encrypted})).await?;
    let room_id = join_chat(&mut scene).await?;
    let room = scene.alice.get_room(&room_id).context("the joined room")?;
    assert_eq!(
        room.latest_encryption_state().await?.is_encrypted(),
        encrypted,
        "the chat's encryption follows the offering's option"
    );
    let instance = scene
        .admin
        .ok(reqwest::Method::GET, &scene.instance_path, None)
        .await?;
    assert_eq!(instance["chat_started_by"], "owner", "{instance}");
    assert!(instance["reason"].is_null(), "{instance}");

    // The bridge read the rendered config as it was: its own rewrite of it keeps the port the
    // catalogue chose, the bot and ghost names, the kept pickle key and the provisioning secret.
    let config = scene.bridge.read_file("/data/config.yaml")?;
    let bot_localpart = scene
        .bot
        .split(':')
        .next()
        .unwrap_or_default()
        .trim_start_matches('@')
        .to_owned();
    let mut needles = vec![
        format!("port: {PORT}"),
        format!("username: {bot_localpart}"),
        // The bridge's upgrader quotes the template when it rewrites the file.
        "username_template: \"email_alice_{{.}}\"".to_owned(),
    ];
    if encrypted {
        needles.push("msc4190: true".to_owned());
    }
    for needle in &needles {
        assert!(
            config.contains(needle),
            "the rewritten config lacks {needle:?}:\n{config}"
        );
    }
    // The passphrase file is the one the server minted, and the bridge did not write another.
    assert_eq!(
        scene.bridge.read_file("/data/passphrase")?.trim(),
        scene.passphrase,
        "the bridge kept the passphrase it was given"
    );
    let log = scene.bridge.log();
    assert!(
        !log.contains("Auto-generated secure passphrase"),
        "the bridge generated a passphrase of its own:\n{}",
        tail(&log, 40)
    );

    help_in(&mut scene, &room_id).await?;

    // The admin API asks the bridge's provisioning API with the secret the server wrote
    // (`GET /appservices/{id}/logins`): a `bridgev2` bridge answers, and alice is not signed
    // in to any mailbox.
    let logins = scene
        .admin
        .ok(
            reqwest::Method::GET,
            &format!(
                "/appservices/{}/logins?user_id={}",
                scene.appservice_id,
                scene.alice_id.replace('@', "%40").replace(':', "%3A")
            ),
            None,
        )
        .await?;
    eprintln!("[{label}] logins: {logins}");
    assert_eq!(logins["provisioning_api"], "mautrix_v3", "{logins}");
    assert_eq!(logins["supported"], true, "{logins}");
    assert!(
        logins["error"].is_null(),
        "the bridge answered whoami: {logins}"
    );
    assert_eq!(logins["signed_in"], false, "{logins}");
    assert_eq!(logins["logins"], json!([]), "{logins}");
    if encrypted {
        let health = scene
            .admin
            .ok(
                reqwest::Method::GET,
                &format!("/appservices/{}/health", scene.appservice_id),
                None,
            )
            .await?;
        assert!(
            health["last_key_withheld"].is_null(),
            "nothing was withheld from the bridge: {health}"
        );
    }
    Ok(())
}

fn tail(s: &str, n: usize) -> String {
    let lines: Vec<&str> = s.lines().collect();
    lines[lines.len().saturating_sub(n)..].join("\n")
}

fn run<F>(story: F)
where
    F: std::future::Future<Output = Result<()>>,
{
    let _guard = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(reason) = skip_reason() {
        eprintln!("SKIP: {reason}");
        return;
    }
    let runtime = tokio::runtime::Runtime::new().expect("a tokio runtime");
    if let Err(e) = runtime.block_on(story) {
        panic!("{e:#}");
    }
}

/// The offering's default: an end-to-end encrypted chat with the personal bot.
#[test]
fn a_person_types_help_in_the_encrypted_chat_and_the_bot_lists_its_commands() {
    run(help(true));
}

/// The same with `options.encryption: false`.
#[test]
fn a_person_types_help_in_a_plain_chat_and_the_bot_lists_its_commands() {
    run(help(false));
}
