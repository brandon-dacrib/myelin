//! A person signs in to their own mautrix-whatsapp bridge: the real bridge, the real `hs`
//! binary, and a real encrypting client (`matrix-sdk` with `e2e-encryption`). Since
//! 2026-10-04 also a second real bridge, mautrix-signal (`dock.mau.dev/mautrix/signal:latest`),
//! through the same story: offered, run from its rendered files, `login` answered with a QR
//! code in the encrypted chat.
//!
//! This is the moment RFC 0017 was built for and the one nothing had exercised: an administrator
//! enables the WhatsApp offering, an instance is made for a person, a chat with its personal bot
//! appears, and they type `login qr`. The bridge answers with a QR code (an `m.image`, or the
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
//! management room; the manager now starts the chat as the person. And the same day, on the
//! demo server: the chat of an instance from before that fix was still the bot's, and typing in
//! it still did nothing. The third test here is that chat: started by the bot, repaired by the
//! manager in place (the bot leaves, the person re-invites it), and answered.

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

const IMAGE: &str = "dock.mau.dev/mautrix/whatsapp:latest";

/// Which mautrix bridge a story runs: its catalogue type, its image, the port it listens on
/// (its `DefaultPort`, which the catalogue renders), and what a person types to sign in.
#[derive(Debug, Clone, Copy)]
struct Network {
    type_id: &'static str,
    image: &'static str,
    port: u16,
    /// What is typed to get a QR code: `login qr` for WhatsApp (it has two flows, so the
    /// flow is named); `login` for Signal, whose only flow is linking by QR code.
    login: &'static str,
}

const WHATSAPP: Network = Network {
    type_id: "mautrix-whatsapp",
    image: IMAGE,
    port: 29318,
    login: "login qr",
};

/// The second real mautrix bridge (2026-10-04): mautrix-signal, `bridgev2` like WhatsApp, from
/// mau.dev's registry as well.
const SIGNAL: Network = Network {
    type_id: "mautrix-signal",
    image: "dock.mau.dev/mautrix/signal:latest",
    port: 29328,
    login: "login",
};
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
fn skip_reason(image: &str) -> Option<String> {
    if hs_binary().is_none() {
        return Some(
            "no hs binary under target/{debug,release}; run `cargo build -p hs-cli --bin hs` or set HS_BIN"
                .into(),
        );
    }
    if let Err(e) = docker(&["version", "--format", "{{.Server.Version}}"]) {
        return Some(format!("Docker is not reachable: {e}"));
    }
    if docker(&["image", "inspect", image, "--format", "{{.Id}}"]).is_err()
        && let Err(e) = docker(&["pull", image])
    {
        return Some(format!("could not pull {image}: {e}"));
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

/// Alice, on a client that excludes insecure devices: `matrix-sdk`'s
/// `CollectStrategy::IdentityBasedStrategy`, the rule behind Element's "Exclude insecure devices
/// when sending/receiving messages" and Element X's invisible crypto (MSC4153), which shares a
/// room's keys only with devices cross-signed by their owner and with no device of a user who
/// has no identity. The owner's Element did this to the demo bridge on 2026-10-03 and the bot
/// answered "⚠️ Your message was not bridged: your client refused to share decryption keys with
/// the bridge"; with the bot's device unsigned, every encrypted story here would end the same
/// way.
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
    // That strategy refuses to send at all until the sender's own cross-signing exists
    // ("Encryption failed because cross-signing is not set up on your account"), as the
    // owner's Element has it; a first upload needs no re-authentication on this server.
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

/// The bodies of what the bot says in `room` from now on, until one line per needle has been
/// seen (each needle in some message, decrypted or plain), or `wait` is up. Returns every body
/// seen, in order.
async fn wait_for_bot_lines(
    client: &Client,
    since: &mut String,
    room: &RoomId,
    bot: &str,
    needles: &[&str],
    wait: Duration,
) -> Result<Vec<String>> {
    let deadline = Instant::now() + wait;
    let mut bodies: Vec<String> = Vec::new();
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
                let (how, json) = classify(event);
                if json["sender"].as_str() == Some(bot) && json["type"] == "m.room.message" {
                    bodies.push(format!(
                        "[{how}] {}",
                        json["content"]["body"].as_str().unwrap_or_default()
                    ));
                }
            }
        }
        if needles
            .iter()
            .all(|needle| bodies.iter().any(|b| b.contains(needle)))
        {
            return Ok(bodies);
        }
    }
    Ok(bodies)
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

    /// A file under the bridge's `/data`, read from inside the container. The bridge rewrites
    /// `config.yaml` as its own user (uid 1337) with a mode nobody else can read, so the host
    /// copy in the test's temp dir is unreadable on Linux (GitHub's runners, run 37096260816:
    /// "Permission denied (os error 13)") and readable on a Mac only because Docker Desktop
    /// maps the uid. `docker exec cat` reads it the way `docker logs` reads the log.
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

fn reserve_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .and_then(|l| l.local_addr())
        .map(|a| a.port())
        .unwrap_or(0)
}

/// The server, the administrator, alice, her instance and its bridge, up and ready.
struct Scene {
    label: &'static str,
    network: Network,
    _dir: tempfile::TempDir,
    server: Server,
    admin: Admin,
    alice: Client,
    alice_id: String,
    since: String,
    instance_path: String,
    appservice_id: String,
    bot: String,
    bridge: Bridge,
}

impl Scene {
    fn keep_logs(&self) {
        if let Ok(dir) = std::env::var("HS_BRIDGE_LOGIN_LOG_DIR") {
            let dir = Path::new(&dir);
            let _ = std::fs::create_dir_all(dir);
            let _ = std::fs::write(
                dir.join(format!("{}-hs.log", self.label)),
                self.server.log(),
            );
            let _ = std::fs::write(
                dir.join(format!("{}-bridge.log", self.label)),
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

/// Everything up to "ready": the offering with `options`, alice's instance, its files, the
/// container, the registration pointed at it.
async fn set_up(label: &'static str, options: Value) -> Result<Scene> {
    set_up_for(WHATSAPP, label, options).await
}

/// [`set_up`] for `network`'s bridge.
async fn set_up_for(network: Network, label: &'static str, options: Value) -> Result<Scene> {
    let dir = tempfile::tempdir()?;
    let port = reserve_port();
    let server = Server::start(dir.path(), port)?;
    let admin = claim_server(&server).await?;
    let alice = register(&server.base, "alice").await?;
    let alice_id = alice.user_id().context("alice's id")?.to_string();
    let since = alice.sync_once(SyncSettings::default()).await?.next_batch;

    // The offering, as an administrator sets it up; the instance for alice; its files.
    admin
        .ok(
            reqwest::Method::PUT,
            &format!("/bridge-offerings/{}", network.type_id),
            Some(json!({"runtime": "elsewhere", "options": options})),
        )
        .await?;
    let instance_path = format!(
        "/bridge-offerings/{}/instances/{}",
        network.type_id,
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
        &format!("127.0.0.1:{bridge_port}:{}", network.port),
        "-v",
        &format!("{}:/data", bridge_dir.display()),
        network.image,
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
        network,
        _dir: dir,
        server,
        admin,
        alice,
        alice_id,
        since,
        instance_path,
        appservice_id,
        bot,
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

/// Alice's chat with her bot appears: started as her, with the bot invited (the manager acting
/// through the instance's claim on her), or, where it cannot act as her, an invitation from
/// the bot that she accepts. Returns the room, after the join and the bot's first words have
/// settled.
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
    // Let the join and the bot's welcome settle, and the client see the room's members.
    scene.sync(Duration::from_secs(2)).await?;
    Ok(room_id)
}

/// Alice types `login qr` (or `HS_BRIDGE_LOGIN_TEXT`) in `room_id` and the bot answers with a
/// QR code.
async fn login_qr_in(scene: &mut Scene, room_id: &RoomId) -> Result<()> {
    let label = scene.label;
    let bot = scene.bot.clone();
    // One sync first, as a client that is running would have done: the manager may have
    // signed the bot's device a moment ago, and alice's client learns that from the
    // device-list change in a sync. Sending before it did withheld the room's key from the
    // bot as `m.unverified` (seen once on 2026-10-04 in the repaired story, under load).
    scene.sync(Duration::from_secs(1)).await?;
    let room = scene.alice.get_room(room_id).context("the joined room")?;
    let is_encrypted = room.latest_encryption_state().await?.is_encrypted();
    // `HS_BRIDGE_LOGIN_TEXT` types something else, to see how the bridge takes it (for
    // example `!wa login qr`, the prefixed form a mautrix bridge takes in any room).
    let text = std::env::var("HS_BRIDGE_LOGIN_TEXT").unwrap_or_else(|_| scene.network.login.into());
    let sent = room
        .send(RoomMessageEventContent::text_plain(&text))
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
    let health = scene
        .admin
        .ok(
            reqwest::Method::GET,
            &format!("/appservices/{}/health", scene.appservice_id),
            None,
        )
        .await?;
    eprintln!("[{label}] health: {health}");
    if replies.is_empty() {
        bail!(
            "{bot} said nothing within {REPLY_WAIT:?} of `{text}` ({label} chat). health: {health}\n--- bridge log (tail) ---\n{}\n--- server log (tail) ---\n{}",
            tail(&scene.bridge.log(), 60),
            tail(&scene.server.log(), 60)
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
        let refused = replies.iter().any(|r| {
            r.event["content"]["body"]
                .as_str()
                .is_some_and(|b| b.contains("refused to share decryption keys"))
        });
        if refused {
            bail!(
                "{bot} said alice's client refused to share the room's keys with it: the bot's device is not cross-signed, or alice's client has not seen that it is\n--- bridge log (tail) ---\n{}",
                tail(&scene.bridge.log(), 60)
            );
        }
        bail!(
            "{bot} answered, but not with a QR code: {:?}\n--- bridge log (tail) ---\n{}",
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
async fn login_qr(encrypted: bool) -> Result<()> {
    let label = if encrypted { "encrypted" } else { "plain" };
    let mut scene = set_up(label, json!({"encryption": encrypted})).await?;
    let room_id = join_chat(&mut scene).await?;
    let room = scene.alice.get_room(&room_id).context("the joined room")?;
    let is_encrypted = room.latest_encryption_state().await?.is_encrypted();
    assert_eq!(
        is_encrypted, encrypted,
        "the chat's encryption should follow the offering's option"
    );
    let instance = scene
        .admin
        .ok(reqwest::Method::GET, &scene.instance_path, None)
        .await?;
    assert_eq!(
        instance["chat_started_by"], "owner",
        "the manager started the chat as alice: {instance}"
    );
    assert_eq!(instance["chat_room"], room_id.as_str(), "{instance}");
    device_name_reached_the_bridge(&scene, &instance)?;
    // A bridge with encryption off makes no device keys, so there is nothing to sign there.
    let signed_device = if encrypted {
        bot_is_cross_signed(&scene).await?
    } else {
        String::new()
    };
    login_qr_in(&mut scene, &room_id).await?;
    // Alice's client excluded insecure devices, and the bridge decrypted `login qr` all the
    // same: it shared the room's keys with the bot's signed device and nothing was withheld.
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
        let instance = scene
            .admin
            .ok(reqwest::Method::GET, &scene.instance_path, None)
            .await?;
        assert_eq!(instance["signed_bot_device"], signed_device, "{instance}");
        assert!(instance["last_key_withheld"].is_null(), "{instance}");
    }
    Ok(())
}

/// The name the instance's config gives the bridge on WhatsApp's side (`network.os_name`, with
/// `browser_name: DESKTOP` so that the phone shows it) survived the bridge's own config
/// upgrader: the bridge completed and rewrote `config.yaml` on its first start, and the
/// rewritten file still says it. The admin API says the same name, for the interface. Nothing
/// here links a phone; what WhatsApp then shows is documented in `docs/bridges/mautrix.md`.
fn device_name_reached_the_bridge(scene: &Scene, instance: &Value) -> Result<()> {
    let expected = format!("Myelin WhatsApp bridge for alice ({SERVER_NAME})");
    assert_eq!(
        instance["device_name"], expected,
        "the admin API names the device: {instance}"
    );
    let config = scene.bridge.read_file("/data/config.yaml")?;
    assert!(
        !config.contains("pickle_key: generate")
            && config
                .lines()
                .any(|l| l.trim_start().starts_with("pickle_key:")),
        "the bridge kept the rendered pickle key rather than generating one; config:\n{config}"
    );
    assert!(
        config.contains(&expected),
        "the bridge's rewritten config names the device; config:\n{config}"
    );
    let platform = config
        .lines()
        .find(|l| l.trim_start().starts_with("browser_name:"));
    assert!(
        platform.is_some_and(|l| l.contains("DESKTOP")),
        "browser_name is DESKTOP: {platform:?}\n{config}"
    );
    Ok(())
}

/// The bot has a cross-signing identity and its device is signed by it, the way a client that
/// excludes insecure devices needs it (`register` above): the manager minted the keys,
/// published them as the appservice and signed the bridge's device
/// (`hs_bridges::cross_signing`). Waits for the manager's step (the instance's
/// `signed_bot_device`), then reads `/keys/query` as alice: the bot's master key, its
/// self-signing key signed by the master key, and the signed device carrying the self-signing
/// key's signature beside its own. Returns the device ID.
async fn bot_is_cross_signed(scene: &Scene) -> Result<String> {
    let label = scene.label;
    let bot = scene.bot.clone();
    let deadline = Instant::now() + Duration::from_secs(60);
    let signed_device = loop {
        let instance = scene
            .admin
            .ok(reqwest::Method::GET, &scene.instance_path, None)
            .await?;
        if let Some(device) = instance["signed_bot_device"].as_str() {
            break device.to_owned();
        }
        if Instant::now() > deadline {
            bail!(
                "the manager never signed the bot's device: {instance}\n--- server log (tail) ---\n{}",
                tail(&scene.server.log(), 40)
            );
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    };
    let token = scene.alice.access_token().context("alice's access token")?;
    let keys: Value = reqwest::Client::new()
        .post(format!(
            "{}/_matrix/client/v3/keys/query",
            scene.server.base
        ))
        .bearer_auth(token)
        .json(&json!({"device_keys": {bot.clone(): []}}))
        .send()
        .await?
        .json()
        .await?;
    let master = &keys["master_keys"][&bot];
    let master_key_id = master["keys"]
        .as_object()
        .and_then(|k| k.keys().next())
        .cloned()
        .with_context(|| format!("the bot has a master key: {keys}"))?;
    assert_eq!(master["usage"], json!(["master"]), "{master}");
    let ssk = &keys["self_signing_keys"][&bot];
    let ssk_key_id = ssk["keys"]
        .as_object()
        .and_then(|k| k.keys().next())
        .cloned()
        .with_context(|| format!("the bot has a self-signing key: {keys}"))?;
    assert!(
        ssk["signatures"][&bot][&master_key_id].is_string(),
        "the self-signing key is signed by the master key: {ssk}"
    );
    let device = &keys["device_keys"][&bot][&signed_device];
    assert!(
        device["signatures"][&bot][&ssk_key_id].is_string(),
        "the bot's device {signed_device} is signed by the self-signing key: {device}"
    );
    assert!(
        device["signatures"][&bot][format!("ed25519:{signed_device}")].is_string(),
        "the device's own signature is kept: {device}"
    );
    eprintln!(
        "[{label}] {bot}'s device {signed_device} is cross-signed (master {master_key_id}, self-signing {ssk_key_id})"
    );
    Ok(signed_device)
}

/// A chat the bot started, repaired in place. Without double puppeting the manager cannot act
/// as alice, so the bot starts the chat and invites her: the shape of every chat from before
/// 2026-10-02, and the bridge drops what she types there. Then the instance is allowed to act
/// as her (the registration's claim on her, and the offering's option), and the manager's next
/// step has the bot leave and alice re-invite it; the bridge accepts, marks the room as her
/// management room and says so; the bot says why it had been silent; and `login qr` is answered.
async fn repaired_chat() -> Result<()> {
    let mut scene = set_up(
        "repaired",
        json!({"encryption": true, "double_puppeting": false}),
    )
    .await?;
    let room_id = join_chat(&mut scene).await?;
    let instance = scene
        .admin
        .ok(reqwest::Method::GET, &scene.instance_path, None)
        .await?;
    assert_eq!(
        instance["chat_started_by"], "bot",
        "without double puppeting the bot starts the chat: {instance}"
    );
    let bot = scene.bot.clone();
    let alice_id = scene.alice_id.clone();
    let appservice_id = scene.appservice_id.clone();

    // What the registration would carry had double puppeting been on from the start: the
    // non-exclusive claim on alice (`bridge_types::render_instance`). Arrays replace under a
    // merge patch, so the whole list is sent.
    let escaped = SERVER_NAME.replace('.', "\\.");
    scene
        .admin
        .ok(
            reqwest::Method::PATCH,
            &format!("/appservices/{appservice_id}"),
            Some(json!({"namespaces": {"users": [
                {"regex": format!("@whatsapp_alice_.*:{escaped}"), "exclusive": true},
                {"regex": format!("@whatsappbot_alice:{escaped}"), "exclusive": true},
                {"regex": format!("@alice:{escaped}"), "exclusive": false},
            ]}})),
        )
        .await?;
    scene
        .admin
        .ok(
            reqwest::Method::PUT,
            "/bridge-offerings/mautrix-whatsapp",
            Some(json!({"options": {"double_puppeting": true}})),
        )
        .await?;

    // The bridge: "This room has been marked as your management room" (encrypted, since the
    // room is). The bot, through the manager: why it had been silent (plain).
    let lines = wait_for_bot_lines(
        &scene.alice,
        &mut scene.since,
        &room_id,
        &bot,
        &["management room", "come back on your invitation"],
        Duration::from_secs(60),
    )
    .await?;
    scene.keep_logs();
    for line in &lines {
        eprintln!("[repaired] {bot}: {line}");
    }
    if !lines.iter().any(|l| l.contains("management room")) {
        bail!(
            "the bridge never marked the chat as alice's management room; it said {lines:?}\n--- bridge log (tail) ---\n{}\n--- server log (tail) ---\n{}",
            tail(&scene.bridge.log(), 80),
            tail(&scene.server.log(), 40)
        );
    }
    if !lines
        .iter()
        .any(|l| l.contains("come back on your invitation"))
    {
        bail!("the bot never said why it had been silent; it said {lines:?}");
    }
    let instance = scene
        .admin
        .ok(reqwest::Method::GET, &scene.instance_path, None)
        .await?;
    assert_eq!(instance["chat_started_by"], "owner", "{instance}");
    assert_eq!(instance["chat_room"], room_id.as_str(), "{instance}");
    let server_log = scene.server.log();
    assert!(
        server_log.contains("repaired the owner's chat with their bridge's bot"),
        "the server says what it did"
    );
    let bridge_log = scene.bridge.log();
    assert!(
        bridge_log.contains("Accepted invite to room as bot"),
        "the bridge accepted alice's invitation; its log (tail):\n{}",
        tail(&bridge_log, 60)
    );
    // The room's state agrees: the bot's membership is on alice's invitation now.
    let room = scene.alice.get_room(&room_id).context("the room")?;
    let members = room.members(matrix_sdk::RoomMemberships::JOIN).await?;
    assert_eq!(members.len(), 2, "alice and the bot");
    let _ = alice_id;

    bot_is_cross_signed(&scene).await?;
    login_qr_in(&mut scene, &room_id).await
}

fn tail(s: &str, n: usize) -> String {
    let lines: Vec<&str> = s.lines().collect();
    lines[lines.len().saturating_sub(n)..].join("\n")
}

fn run<F>(story: F)
where
    F: std::future::Future<Output = Result<()>>,
{
    run_with(IMAGE, story);
}

/// [`run`] for a story whose bridge is `image`.
fn run_with<F>(image: &str, story: F)
where
    F: std::future::Future<Output = Result<()>>,
{
    let _guard = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(reason) = skip_reason(image) {
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
fn a_person_types_login_qr_in_the_encrypted_chat_and_gets_a_qr_code() {
    run(login_qr(true));
}

/// The same with `options.encryption: false`.
#[test]
fn a_person_types_login_qr_in_a_plain_chat_and_gets_a_qr_code() {
    run(login_qr(false));
}

/// The second real bridge: mautrix-signal, offered and run from the files this server
/// rendered, in the encrypted chat with its personal bot. `login` (Signal's one flow, linking
/// as a secondary device) is answered with a QR code; the bot's device is cross-signed by the
/// manager; nothing is withheld; and the rewritten config still names the device after alice
/// and this server (`network.device_name`).
async fn signal_login() -> Result<()> {
    let mut scene = set_up_for(SIGNAL, "signal", json!({"encryption": true})).await?;
    let room_id = join_chat(&mut scene).await?;
    let instance = scene
        .admin
        .ok(reqwest::Method::GET, &scene.instance_path, None)
        .await?;
    assert_eq!(instance["chat_started_by"], "owner", "{instance}");
    let expected = format!("Myelin Signal bridge for alice ({SERVER_NAME})");
    assert_eq!(instance["device_name"], expected, "{instance}");
    let config = scene.bridge.read_file("/data/config.yaml")?;
    assert!(
        config.contains(&expected),
        "the bridge's rewritten config names the device; config:\n{config}"
    );
    let signal = scene
        .admin
        .ok(reqwest::Method::GET, "/bridge-types/mautrix-signal", None)
        .await?;
    assert_eq!(signal["command_prefix"], "!signal", "{signal}");
    assert!(
        config.contains("command_prefix: '!signal'")
            || config.contains("command_prefix: \"!signal\"")
            || config.contains("command_prefix: !signal"),
        "the bridge's own default prefix is the one the catalogue claims; config:\n{config}"
    );
    let signed_device = bot_is_cross_signed(&scene).await?;
    login_qr_in(&mut scene, &room_id).await?;
    let instance = scene
        .admin
        .ok(reqwest::Method::GET, &scene.instance_path, None)
        .await?;
    assert_eq!(instance["signed_bot_device"], signed_device, "{instance}");
    assert!(instance["last_key_withheld"].is_null(), "{instance}");
    Ok(())
}

/// A chat the bot started (every chat from before 2026-10-02) is repaired in place and answers.
#[test]
fn a_chat_the_bot_started_is_repaired_in_place_and_login_qr_gets_a_qr_code() {
    run(repaired_chat());
}

/// The second real bridge, Signal, signed in to the same way.
#[test]
fn a_person_types_login_to_their_signal_bridge_and_gets_a_qr_code() {
    run_with(SIGNAL.image, signal_login());
}
