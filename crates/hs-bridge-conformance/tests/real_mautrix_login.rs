//! A person signs in to their own mautrix-whatsapp bridge: the real bridge, the real `hs`
//! binary, and a real encrypting client (`matrix-sdk` with `e2e-encryption`).
//!
//! This is the moment RFC 0017 was built for and the one nothing had exercised: an administrator
//! enables the WhatsApp offering, an instance is made for a person, its personal bot invites them
//! to a chat, and they type `login qr`. The bridge answers with a QR code (an `m.image`, or the
//! code as text when it cannot upload) without any phone being involved, so the whole path from
//! a person's keyboard to the bridge's command handler and back is checked here, with the chat
//! encrypted (the offering's default) and in the clear.
//!
//! Skipped, saying why, when Docker cannot be reached, the image cannot be had, or the `hs`
//! binary is not built (`cargo build -p hs-cli --bin hs`; `HS_BIN` names it explicitly). Docker
//! Hub is not involved: the image is `dock.mau.dev/mautrix/whatsapp:latest`, and a `DOCKER_HOST`
//! or `DOCKER_CONFIG` in the environment is passed through. `HS_BRIDGE_LOGIN_LOG_DIR` names a
//! directory to copy the server's and the bridge's logs into; `HS_BRIDGE_LOGIN_TEXT` replaces
//! what alice types.
//!
//! What it caught (2026-10-02): the bridge decrypted `login qr` and dropped it, because the
//! chat had been started by its bot, which a mautrix bridge never takes as a person's
//! management room; the manager now starts the chat as the person.

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
use serde_json::{Value, json};

const IMAGE: &str = "dock.mau.dev/mautrix/whatsapp:latest";
const SERVER_NAME: &str = "test.local";
/// How long the bridge gets to answer `login qr`. It answers in well under a second when it
/// understands the message.
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
        return Some(format!("could not pull {IMAGE}: {e}"));
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
    fn start(dir: &Path, port: u16) -> Result<Self> {
        let data = dir.join("data");
        let media = dir.join("media");
        let config = format!(
            "server: {{ server_name: {SERVER_NAME}, public_baseurl: \"http://host.docker.internal:{port}\" }}\n\
             listeners: {{ listeners: [ {{ port: {port}, bind_addresses: [\"0.0.0.0\"], resources: [client, admin, health, metrics] }} ] }}\n\
             storage: {{ backend: embedded, data_dir: {data:?} }}\n\
             media: {{ storage: {{ backend: local, path: {media:?} }} }}\n\
             auth: {{ enable_registration: true }}\n\
             rate_limits: {{ enabled: false }}\n"
        );
        let path = dir.join("homeserver.yaml");
        std::fs::write(&path, config)?;
        let mut child = Command::new(hs_binary().context("hs binary")?)
            .args(["serve", "-c"])
            .arg(&path)
            .env(
                "RUST_LOG",
                "info,hs_appservice=debug,hs_e2e=debug,hs_bridges=debug",
            )
            .env_remove("HS_DATA_DIR")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .context("starting hs serve")?;
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

// ---- the admin API -----------------------------------------------------------------------------

#[derive(Clone)]
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

async fn register(base: &str, username: &str) -> Result<Client> {
    let client = Client::builder().homeserver_url(base).build().await?;
    let mut request = RegisterRequest::new();
    request.username = Some(username.to_owned());
    request.password = Some("a-password-for-the-test".to_owned());
    request.initial_device_display_name = Some("hs-bridge-conformance".to_owned());
    request.auth = Some(uiaa::AuthData::Dummy(uiaa::Dummy::new()));
    client.matrix_auth().register(request).await?;
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

/// What the bot said in `room` after `after_event_id`, as it arrives over `/sync`.
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
            // Give a second reply (the image after the notice) a moment to follow.
            tokio::time::sleep(Duration::from_secs(2)).await;
            let response = client
                .sync_once(
                    SyncSettings::default()
                        .token(since.clone())
                        .timeout(Duration::from_secs(1)),
                )
                .await?;
            *since = response.next_batch.clone();
            if let Some(joined) = response.rooms.joined.get(room) {
                for event in &joined.timeline.events {
                    let (how, json) = classify(event);
                    if json["sender"].as_str() == Some(bot) && json["type"] != "m.room.member" {
                        replies.push(Reply { how, event: json });
                    }
                }
            }
            return Ok(replies);
        }
    }
    Ok(replies)
}

// ---- the run -----------------------------------------------------------------------------------

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
}

impl Drop for Bridge {
    fn drop(&mut self) {
        let _ = docker(&["rm", "-f", "-v", &self.name]);
    }
}

fn reserve_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .and_then(|l| l.local_addr())
        .map(|a| a.port())
        .unwrap_or(0)
}

fn keep_logs(label: &str, server: &Server, bridge: &Bridge) {
    if let Ok(dir) = std::env::var("HS_BRIDGE_LOGIN_LOG_DIR") {
        let dir = Path::new(&dir);
        let _ = std::fs::create_dir_all(dir);
        let _ = std::fs::write(dir.join(format!("{label}-hs.log")), server.log());
        let _ = std::fs::write(dir.join(format!("{label}-bridge.log")), bridge.log());
    }
}

/// The whole story for one chat, encrypted or not. Returns the bot's replies to `login qr`.
async fn login_qr(encrypted: bool) -> Result<()> {
    let label = if encrypted { "encrypted" } else { "plain" };
    let dir = tempfile::tempdir()?;
    let port = reserve_port();
    let server = Server::start(dir.path(), port)?;
    let admin = claim_server(&server).await?;
    let alice = register(&server.base, "alice").await?;
    let alice_id = alice.user_id().context("alice's id")?.to_owned();
    let mut since = alice.sync_once(SyncSettings::default()).await?.next_batch;

    // The offering, as an administrator sets it up; the instance for alice; its files.
    admin
        .ok(
            reqwest::Method::PUT,
            "/bridge-offerings/mautrix-whatsapp",
            Some(json!({"runtime": "elsewhere", "options": {"encryption": encrypted}})),
        )
        .await?;
    let instance_path = format!(
        "/bridge-offerings/mautrix-whatsapp/instances/{}",
        alice_id.as_str().replace('@', "%40").replace(':', "%3A")
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
    let bridge_dir = dir.path().join("bridge");
    std::fs::create_dir_all(&bridge_dir)?;
    std::fs::write(
        bridge_dir.join("config.yaml"),
        files["config_yaml"].as_str().unwrap_or_default(),
    )?;
    std::fs::write(
        bridge_dir.join("registration.yaml"),
        files["registration_yaml"].as_str().unwrap_or_default(),
    )?;
    // The container runs as its own user and writes its database and the completed config here.
    let _ = Command::new("chmod").arg("777").arg(&bridge_dir).status();

    // The bridge, and where the server reaches it.
    let bridge_port = reserve_port();
    let name = format!("hs-bridge-login-{label}-{}", std::process::id());
    // `host.docker.internal` is how the bridge's config names this server (the rendered
    // `homeserver.address` comes from `public_baseurl` above). Docker on a Mac resolves it by
    // itself; Docker on Linux, GitHub's runners included, only with this flag, and without it
    // the bridge answered the server's pings (that direction uses the published port) but could
    // never reach the server to accept the invite or send, and CI failed on 2026-10-02 with
    // "said nothing within 30s" on both architectures.
    docker(&[
        "run",
        "-d",
        "--name",
        &name,
        "--add-host",
        "host.docker.internal:host-gateway",
        "-p",
        &format!("127.0.0.1:{bridge_port}:29318"),
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
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        let instance = admin.ok(reqwest::Method::GET, &instance_path, None).await?;
        if instance["state"] == "ready" {
            break;
        }
        if instance["state"] == "failed" || Instant::now() > deadline {
            keep_logs(label, &server, &bridge);
            bail!(
                "the instance never became ready: {instance}\n--- bridge log ---\n{}",
                bridge.log()
            );
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }

    // Alice's chat with her bot appears: started as her, with the bot invited (the manager
    // acting through the instance's claim on her), or, where it cannot act as her, an
    // invitation from the bot that she accepts. Then she types `login qr`.
    let deadline = Instant::now() + Duration::from_secs(60);
    let room_id: OwnedRoomId = loop {
        let response = alice
            .sync_once(
                SyncSettings::default()
                    .token(since.clone())
                    .timeout(Duration::from_secs(2)),
            )
            .await?;
        since = response.next_batch.clone();
        if let Some(room_id) = response.rooms.invited.keys().next() {
            alice.join_room_by_id(room_id).await?;
            break room_id.clone();
        }
        if let Some((room_id, _)) = response.rooms.joined.iter().find(|(_, joined)| {
            joined.timeline.events.iter().any(|event| {
                let (_, json) = classify(event);
                json["sender"].as_str() == Some(&bot) && json["type"] == "m.room.message"
            })
        }) {
            break room_id.clone();
        }
        if Instant::now() > deadline {
            keep_logs(label, &server, &bridge);
            bail!("no chat with {bot} ever reached alice");
        }
    };
    let room = alice.get_room(&room_id).context("the joined room")?;
    // Let the join and the bot's welcome settle, and the client see the room's members.
    since = alice
        .sync_once(
            SyncSettings::default()
                .token(since.clone())
                .timeout(Duration::from_secs(2)),
        )
        .await?
        .next_batch;
    let is_encrypted = room.latest_encryption_state().await?.is_encrypted();
    assert_eq!(
        is_encrypted, encrypted,
        "the chat's encryption should follow the offering's option"
    );
    // `HS_BRIDGE_LOGIN_TEXT` types something else, to see how the bridge takes it (for
    // example `!wa login qr`, the prefixed form a mautrix bridge takes in any room).
    let text = std::env::var("HS_BRIDGE_LOGIN_TEXT").unwrap_or_else(|_| "login qr".into());
    let sent = room
        .send(RoomMessageEventContent::text_plain(&text))
        .await
        .with_context(|| format!("sending `{text}`"))?;
    let sent_id = sent.response.event_id.to_string();
    eprintln!(
        "[{label}] alice sent `{text}` as {sent_id} in {room_id} (encrypted: {is_encrypted})"
    );

    let replies = wait_for_reply(&alice, &mut since, &room_id, &bot, &sent_id, REPLY_WAIT).await?;
    keep_logs(label, &server, &bridge);
    for r in &replies {
        eprintln!(
            "[{label}] {bot} answered ({}): type={} msgtype={} body={}",
            r.how, r.event["type"], r.event["content"]["msgtype"], r.event["content"]["body"]
        );
    }
    let health = admin
        .ok(
            reqwest::Method::GET,
            &format!("/appservices/{appservice_id}/health"),
            None,
        )
        .await?;
    eprintln!("[{label}] health: {health}");
    if replies.is_empty() {
        bail!(
            "{bot} said nothing within {REPLY_WAIT:?} of `login qr` ({label} chat). health: {health}\n--- bridge log (tail) ---\n{}\n--- server log (tail) ---\n{}",
            tail(&bridge.log(), 60),
            tail(&server.log(), 60)
        );
    }
    let qr = replies.iter().any(|r| {
        r.how != "utd"
            && (r.event["content"]["msgtype"] == "m.image"
                || r.event["content"]["body"]
                    .as_str()
                    .is_some_and(|b| b.to_ascii_lowercase().contains("qr") || b.contains("scan")))
    });
    if !qr {
        bail!(
            "{bot} answered, but not with a QR code: {:?}\n--- bridge log (tail) ---\n{}",
            replies
                .iter()
                .map(|r| (r.how, r.event["content"].clone()))
                .collect::<Vec<_>>(),
            tail(&bridge.log(), 60)
        );
    }
    Ok(())
}

fn tail(s: &str, n: usize) -> String {
    let lines: Vec<&str> = s.lines().collect();
    lines[lines.len().saturating_sub(n)..].join("\n")
}

fn run(encrypted: bool) {
    let _guard = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(reason) = skip_reason() {
        eprintln!("SKIP: {reason}");
        return;
    }
    let runtime = tokio::runtime::Runtime::new().expect("a tokio runtime");
    if let Err(e) = runtime.block_on(login_qr(encrypted)) {
        panic!("{e:#}");
    }
}

/// The offering's default: an end-to-end encrypted chat with the personal bot.
#[test]
fn a_person_types_login_qr_in_the_encrypted_chat_and_gets_a_qr_code() {
    run(true);
}

/// The same with `options.encryption: false`.
#[test]
fn a_person_types_login_qr_in_a_plain_chat_and_gets_a_qr_code() {
    run(false);
}
