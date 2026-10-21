//! The real matrix-hookshot against the real `hs` binary: an administrator offers Hookshot with
//! a public URL, the server renders its files whole (`config.yml`, `registration.yml` and the
//! passkey it minted), the image runs from exactly those files and comes up `ready`, a person
//! invites the bot to a room and asks for a webhook, the bot answers with a URL under the
//! offering's public URL, a POST to that URL lands in the room, and the admin API lists the
//! connection it read from the room's state. Then the other way round: the administrator makes
//! a webhook through the admin API from the `datadog` source, in a new room the bot creates
//! and invites the person to; the bridge picks the connection up live from the state event;
//! Datadog's sample payload posted to the URL lands as the source's notice; the listing says
//! the source; and removing it through the API makes the URL stop working. Then as a plain
//! user (decision 0057): alice, no administrator, makes a webhook in a room of her own through
//! the user API with her own token; the server invites the bot as her, the bot joins and is
//! raised to the level the state event takes, the bridge picks the connection up live, Linear's
//! sample posted to the URL lands as the source's notice, the listing names her, and removing
//! it stops the URL. The story `docs/bridges/hookshot.md` tells, run.
//!
//! Hookshot is one shared bridge with many connections, so unlike `real_mautrix_login.rs` there
//! is no sign-in and no encryption here: the person is a plain client on the client API.
//!
//! Skipped, saying why, when Docker cannot be reached, the image cannot be had, or the `hs`
//! binary is not built (`cargo build -p hs-cli --bin hs`; `HS_BIN` names it explicitly). The
//! image is the catalogue's pin ([`IMAGE`]); the offering the server makes has to name it, so a
//! pin bumped on one side only fails here. On a Mac whose `docker pull` from ghcr.io needs the
//! keychain the pull fails and the test skips; CI pulls. `HS_HOOKSHOT_LOG_DIR` names a
//! directory to copy the server's and the bridge's logs into.

use std::io::BufRead;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

/// The catalogue's pin for Hookshot (`crates/hs-admin/src/bridge_types.rs`); bumped together.
const IMAGE: &str = "ghcr.io/matrix-org/matrix-hookshot:7.5.0";
const TYPE_ID: &str = "matrix-hookshot";
const SERVER_NAME: &str = "test.local";
/// The user webhooks API's prefix (`hs_bridges::user_webhooks::PREFIX`).
const USER_PREFIX: &str = "/_matrix/client/unstable/io.myelin.webhooks";
/// The appservice port the catalogue renders, and the webhooks port the rendered config binds.
const APPSERVICE_PORT: u16 = 9993;
const WEBHOOKS_PORT: u16 = 9000;
/// How long the bridge gets to answer a command once it is ready.
const REPLY_WAIT: Duration = Duration::from_secs(30);

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

// ---- the admin API and the client API --------------------------------------------------------

/// A caller of this server's admin API (`/api/v1`) or client API (`/_matrix/client/v3`) with
/// one bearer token.
#[derive(Clone)]
struct Caller {
    http: reqwest::Client,
    base: String,
    prefix: &'static str,
    token: String,
}

impl Caller {
    async fn call(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<Value>,
    ) -> Result<(u16, Value)> {
        let mut req = self
            .http
            .request(
                method.clone(),
                format!("{}{}{path}", self.base, self.prefix),
            )
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

async fn claim_server(server: &Server) -> Result<Caller> {
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
    Ok(Caller {
        http,
        base: server.base.clone(),
        prefix: "/api/v1",
        token,
    })
}

/// Registers `username` with a password and dummy auth; a plain client of the client API.
async fn register(base: &str, username: &str) -> Result<Caller> {
    let http = reqwest::Client::new();
    let answer: Value = http
        .post(format!("{base}/_matrix/client/v3/register"))
        .json(&json!({
            "username": username,
            "password": "a-password-for-the-test",
            "auth": {"type": "m.login.dummy"},
            "initial_device_display_name": "hs-bridge-conformance",
        }))
        .send()
        .await?
        .json()
        .await?;
    let token = answer["access_token"]
        .as_str()
        .with_context(|| format!("register answered {answer}"))?
        .to_owned();
    Ok(Caller {
        http,
        base: base.to_owned(),
        prefix: "/_matrix/client/v3",
        token,
    })
}

// ---- the bridge ------------------------------------------------------------------------------

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
        let _ = docker(&["rm", "-f", &self.name]);
    }
}

/// A free loopback port for Docker to publish on: the number alone, as the mautrix story takes
/// it (Docker binds it itself, so the listener cannot be handed over).
fn reserve_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .and_then(|l| l.local_addr())
        .map(|a| a.port())
        .unwrap_or(0)
}

/// The messages of `room_id` so far (`GET /messages` from the start), newest last.
async fn messages(client: &Caller, room_id: &str) -> Result<Vec<Value>> {
    let room = room_id.replace('!', "%21").replace(':', "%3A");
    let page = client
        .ok(
            reqwest::Method::GET,
            &format!("/rooms/{room}/messages?dir=f&limit=100"),
            None,
        )
        .await?;
    Ok(page["chunk"].as_array().cloned().unwrap_or_default())
}

/// Waits until a message in `room_id` from `sender` (or anyone, for `None`) satisfies `pick`,
/// and returns it.
async fn wait_for_message(
    client: &Caller,
    room_id: &str,
    sender: Option<&str>,
    what: &str,
    pick: impl Fn(&str) -> bool,
) -> Result<Value> {
    let deadline = Instant::now() + REPLY_WAIT;
    loop {
        for event in messages(client, room_id).await? {
            let from = event["sender"].as_str().unwrap_or_default();
            let body = event["content"]["body"].as_str().unwrap_or_default();
            if event["type"] == "m.room.message" && sender.is_none_or(|s| s == from) && pick(body) {
                return Ok(event);
            }
        }
        if Instant::now() > deadline {
            let seen: Vec<String> = messages(client, room_id)
                .await?
                .iter()
                .map(|e| {
                    format!(
                        "{} {}: {}",
                        e["type"].as_str().unwrap_or_default(),
                        e["sender"].as_str().unwrap_or_default(),
                        e["content"]["body"].as_str().unwrap_or_default()
                    )
                })
                .collect();
            bail!(
                "no {what} within {REPLY_WAIT:?}; the room so far:\n{}",
                seen.join("\n")
            );
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

fn keep_logs(label: &str, server: &Server, bridge: &Bridge) {
    if let Ok(dir) = std::env::var("HS_HOOKSHOT_LOG_DIR") {
        let dir = Path::new(&dir);
        let _ = std::fs::create_dir_all(dir);
        let _ = std::fs::write(dir.join(format!("{label}-hs.log")), server.log());
        let _ = std::fs::write(dir.join(format!("{label}-hookshot.log")), bridge.log());
    }
}

// ---- the story -------------------------------------------------------------------------------

async fn webhook_story() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let server = Server::start(dir.path(), hs_testkit::ports::reserve_wildcard())?;
    let admin = claim_server(&server).await?;
    let alice = register(&server.base, "alice").await?;

    // The offering: a public URL this test can post to (the webhooks port, published on the
    // loopback), so that the URL the bot hands out is the one the test calls.
    let appservice_port = reserve_port();
    let webhooks_port = reserve_port();
    let public_url = format!("http://127.0.0.1:{webhooks_port}");
    let offering = admin
        .ok(
            reqwest::Method::PUT,
            &format!("/bridge-offerings/{TYPE_ID}"),
            Some(json!({"runtime": "elsewhere", "options": {"public_url": public_url}})),
        )
        .await?;
    if offering["image"].as_str() != Some(IMAGE) {
        bail!(
            "the offering names {} but this test runs {IMAGE}: bump the catalogue's pin and this test together",
            offering["image"]
        );
    }
    if offering["mode"] != "shared" {
        bail!(
            "Hookshot is one shared bridge, but the offering says {}",
            offering["mode"]
        );
    }
    let instance_path = format!("/bridge-offerings/{TYPE_ID}/instances/_");
    let deadline = Instant::now() + Duration::from_secs(60);
    let instance = loop {
        let instance = admin.ok(reqwest::Method::GET, &instance_path, None).await?;
        if instance["appservice_id"].is_string() && instance["state"] != "requested" {
            break instance;
        }
        if Instant::now() > deadline {
            bail!("the shared instance never registered: {instance}");
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    };
    let appservice_id = instance["appservice_id"]
        .as_str()
        .unwrap_or_default()
        .to_owned();
    let bot = instance["bot"].as_str().unwrap_or_default().to_owned();
    if bot != format!("@hookshot:{SERVER_NAME}") {
        bail!("the shared instance's bot is {bot}");
    }

    // Its files, every one, exactly as the server renders them: nothing edited by hand.
    let files = admin
        .ok(
            reqwest::Method::POST,
            &format!("{instance_path}/files"),
            None,
        )
        .await?;
    let map = files["files"]
        .as_object()
        .context("the files answer has a `files` map")?;
    for name in ["config.yml", "registration.yml", "passkey.pem"] {
        if !map.contains_key(name) {
            bail!(
                "the files lack {name}: {:?}",
                map.keys().collect::<Vec<_>>()
            );
        }
    }
    let bridge_dir = dir.path().join("bridge");
    std::fs::create_dir_all(&bridge_dir)?;
    for (name, contents) in map {
        std::fs::write(bridge_dir.join(name), contents.as_str().unwrap_or_default())?;
    }
    // The container runs as its own user and writes its database here.
    let _ = Command::new("chmod").arg("777").arg(&bridge_dir).status();
    let _ = Command::new("chmod")
        .arg("644")
        .arg(bridge_dir.join("passkey.pem"))
        .status();

    // The bridge: the image's own command reads /data/config.yml and /data/registration.yml.
    let name = format!("hs-hookshot-{}", std::process::id());
    docker(&[
        "run",
        "-d",
        "--name",
        &name,
        "--add-host",
        "host.docker.internal:host-gateway",
        "-p",
        &format!("127.0.0.1:{appservice_port}:{APPSERVICE_PORT}"),
        "-p",
        &format!("127.0.0.1:{webhooks_port}:{WEBHOOKS_PORT}"),
        "-v",
        &format!("{}:/data", bridge_dir.display()),
        IMAGE,
    ])?;
    let bridge = Bridge { name };
    admin
        .ok(
            reqwest::Method::PATCH,
            &format!("/appservices/{appservice_id}"),
            Some(json!({"url": format!("http://127.0.0.1:{appservice_port}")})),
        )
        .await?;
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        let instance = admin.ok(reqwest::Method::GET, &instance_path, None).await?;
        if instance["state"] == "ready" {
            break;
        }
        if instance["state"] == "failed" || Instant::now() > deadline {
            keep_logs("webhook", &server, &bridge);
            bail!(
                "the instance never became ready: {instance}\n--- bridge log ---\n{}",
                bridge.log()
            );
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }

    // A room with the bot in it, allowed to send state events (each connection is one).
    let room = alice
        .ok(
            reqwest::Method::POST,
            "/createRoom",
            Some(json!({
                "name": "Deploys",
                "invite": [bot],
                "power_level_content_override": {"users": {alice_id(): 100, bot.clone(): 50}},
            })),
        )
        .await?;
    let room_id = room["room_id"].as_str().context("room id")?.to_owned();
    let escaped = room_id.replace('!', "%21").replace(':', "%3A");
    let deadline = Instant::now() + REPLY_WAIT;
    loop {
        let members = alice
            .ok(
                reqwest::Method::GET,
                &format!("/rooms/{escaped}/joined_members"),
                None,
            )
            .await?;
        if members["joined"].get(&bot).is_some() {
            break;
        }
        if Instant::now() > deadline {
            keep_logs("webhook", &server, &bridge);
            bail!(
                "the bot never joined the room\n--- bridge log ---\n{}",
                bridge.log()
            );
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }

    // `!hookshot webhook test`: the bot answers with a URL under the offering's public URL.
    let txn = format!(
        "t{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or_default()
    );
    alice
        .ok(
            reqwest::Method::PUT,
            &format!("/rooms/{escaped}/send/m.room.message/{txn}"),
            Some(json!({"msgtype": "m.text", "body": "!hookshot webhook test"})),
        )
        .await?;
    // The bot confirms in the room ("Room configured to bridge webhooks. See admin room for
    // secret url.") and says the URL in its admin room: a chat it opens with the person. So
    // alice accepts that invitation and reads the URL there.
    let prefix = format!("{public_url}/webhook/");
    if let Err(e) = wait_for_message(&alice, &room_id, Some(&bot), "confirmation", |b| {
        b.contains("webhooks")
    })
    .await
    {
        keep_logs("webhook", &server, &bridge);
        bail!("{e}\n--- bridge log ---\n{}", bridge.log());
    }
    let admin_room = match invite_from(&alice, &bot).await {
        Ok(room) => room,
        Err(e) => {
            keep_logs("webhook", &server, &bridge);
            bail!("{e}\n--- bridge log ---\n{}", bridge.log());
        }
    };
    alice
        .ok(
            reqwest::Method::POST,
            &format!(
                "/join/{}",
                admin_room.replace('!', "%21").replace(':', "%3A")
            ),
            Some(json!({})),
        )
        .await?;
    let reply = match wait_for_message(
        &alice,
        &admin_room,
        Some(&bot),
        "webhook URL from the bot in its admin room",
        |b| b.contains(&prefix),
    )
    .await
    {
        Ok(reply) => reply,
        Err(e) => {
            keep_logs("webhook", &server, &bridge);
            bail!("{e}\n--- bridge log ---\n{}", bridge.log());
        }
    };
    let body = reply["content"]["body"].as_str().unwrap_or_default();
    let url: String = body[body.find(&prefix).unwrap_or_default()..]
        .chars()
        .take_while(|c| !c.is_whitespace() && *c != '`' && *c != ')')
        .collect();

    // Something posts to it, and the room gets the message.
    let posted = reqwest::Client::new()
        .post(&url)
        .json(&json!({"text": "hello from the real-bridge test", "username": "ci"}))
        .send()
        .await
        .with_context(|| format!("POST {url}"))?;
    if !posted.status().is_success() {
        bail!("POST {url} answered {}", posted.status());
    }
    if let Err(e) = wait_for_message(&alice, &room_id, None, "webhook message", |b| {
        b.contains("hello from the real-bridge test")
    })
    .await
    {
        keep_logs("webhook", &server, &bridge);
        bail!("{e}\n--- bridge log ---\n{}", bridge.log());
    }

    // The admin API lists the connection, read from the room's state, with its room's name.
    let connections = admin
        .ok(
            reqwest::Method::GET,
            &format!("/bridge-offerings/{TYPE_ID}/connections"),
            None,
        )
        .await?;
    let items = connections["items"].as_array().cloned().unwrap_or_default();
    let found = items.iter().find(|c| {
        c["service"] == "webhook" && c["name"] == "test" && c["room_id"] == room_id.as_str()
    });
    match found {
        Some(c) if c["state"] == "active" && c["room_name"] == "Deploys" => {}
        other => bail!("the webhook connection is not listed as expected: {other:?} in {items:?}"),
    }
    // A connection made in the room has no source; the room is among the bot's rooms.
    if found.is_some_and(|c| c.get("source").is_some()) {
        bail!("a webhook made in the room says a source: {found:?}");
    }
    if !connections["rooms"]
        .as_array()
        .is_some_and(|rooms| rooms.iter().any(|r| r["room_id"] == room_id.as_str()))
    {
        bail!("the bot's rooms lack {room_id}: {}", connections["rooms"]);
    }

    if let Err(e) = provisioned_story(&admin, &alice, &bot, &public_url).await {
        keep_logs("webhook", &server, &bridge);
        bail!("{e:#}\n--- bridge log ---\n{}", bridge.log());
    }
    if let Err(e) = user_story(&admin, &alice, &bot, &public_url).await {
        keep_logs("webhook", &server, &bridge);
        bail!("{e:#}\n--- bridge log ---\n{}", bridge.log());
    }
    keep_logs("webhook", &server, &bridge);
    Ok(())
}

/// Linear's sample payload, the one the source's transform is tested on.
const LINEAR_SAMPLE: &str =
    include_str!("../../hs-bridges/hookshot-sources/linear/sample-issue-update.json");

/// The third part: a plain user makes a webhook in a room of her own through the user API.
async fn user_story(admin: &Caller, alice: &Caller, bot: &str, public_url: &str) -> Result<()> {
    let user = Caller {
        prefix: USER_PREFIX,
        ..alice.clone()
    };
    // Her own room, made in an ordinary client: she is its admin, the bot is not in it.
    let room = alice
        .ok(
            reqwest::Method::POST,
            "/createRoom",
            Some(json!({"name": "Mine", "preset": "private_chat"})),
        )
        .await?;
    let room_id = room["room_id"].as_str().context("room id")?.to_owned();
    let rooms = user.ok(reqwest::Method::GET, "/rooms", None).await?;
    let mine = rooms["rooms"]
        .as_array()
        .and_then(|rooms| rooms.iter().find(|r| r["room_id"] == room_id.as_str()))
        .cloned()
        .with_context(|| format!("Mine is not among alice's rooms: {rooms}"))?;
    if mine["may_add"] != true || mine["bot_joined"] != false || mine["name"] != "Mine" {
        bail!("the room is not offered as expected: {mine}");
    }

    // Made with her token: the bot is invited as her, joins, and is raised to 50.
    let (status, made) = user
        .call(
            reqwest::Method::POST,
            "/webhooks",
            Some(json!({"source": "linear", "name": "linear-issues", "room": {"id": room_id}})),
        )
        .await?;
    if status != 201 {
        bail!("making the user's webhook answered {status}: {made}");
    }
    let url = made["url"].as_str().unwrap_or_default().to_owned();
    let prefix = format!("{public_url}/webhook/");
    if !url.starts_with(&prefix) || url.len() != prefix.len() + 36 {
        bail!("the URL is not public URL + /webhook/ + a UUID: {url}");
    }
    let members = alice
        .ok(
            reqwest::Method::GET,
            &format!("/rooms/{}/joined_members", escape(&room_id)),
            None,
        )
        .await?;
    if members["joined"].get(bot).is_none() {
        bail!("the bot did not join alice's room: {members}");
    }
    let levels = alice
        .ok(
            reqwest::Method::GET,
            &format!("/rooms/{}/state/m.room.power_levels/", escape(&room_id)),
            None,
        )
        .await?;
    if levels["users"][bot] != 50 {
        bail!("the bot was not raised to 50: {levels}");
    }
    let event = alice
        .ok(
            reqwest::Method::GET,
            &format!(
                "/rooms/{}/state/uk.half-shot.matrix-hookshot.generic.hook/linear-issues",
                escape(&room_id)
            ),
            None,
        )
        .await?;
    if event["io.myelin.created_by"] != alice_id() || event["io.myelin.source"] != "linear" {
        bail!("the state event does not name alice and the source: {event}");
    }

    // The bridge picked it up live: Linear's sample lands as the source's notice.
    let sample: Value = serde_json::from_str(LINEAR_SAMPLE)?;
    let posted = reqwest::Client::new()
        .post(&url)
        .json(&sample)
        .send()
        .await
        .with_context(|| format!("POST {url}"))?;
    if !posted.status().is_success() {
        bail!(
            "POST {url} answered {}: {}",
            posted.status(),
            posted.text().await.unwrap_or_default()
        );
    }
    let landed = wait_for_message(alice, &room_id, None, "the Linear notice", |b| {
        b.contains("ENG-42")
    })
    .await?;
    let body = landed["content"]["body"].as_str().unwrap_or_default();
    if !body.contains("**Ada Lovelace** moved to **In Progress**") {
        bail!("the notice is not the source's rendering: {body:?}");
    }
    if landed["content"]["formatted_body"]
        .as_str()
        .is_none_or(|h| !h.contains("<strong>Ada Lovelace</strong>"))
    {
        bail!("the bridge did not render the Markdown: {landed}");
    }

    // Listed as hers, without the secret; audited with her as actor.
    let listed = user.ok(reqwest::Method::GET, "/webhooks", None).await?;
    let item = listed["items"]
        .as_array()
        .and_then(|items| items.iter().find(|w| w["name"] == "linear-issues"))
        .cloned()
        .with_context(|| format!("linear-issues is not listed: {listed}"))?;
    if item["created_by"] != alice_id()
        || item["source"] != "linear"
        || item["room_name"] != "Mine"
        || item["created_at"].as_u64().is_none_or(|t| t == 0)
    {
        bail!("the listing does not say what it should: {item}");
    }
    let id = url.trim_start_matches(&prefix);
    if listed.to_string().contains(id) {
        bail!("the user listing carries the secret id");
    }
    let log = admin
        .ok(
            reqwest::Method::GET,
            "/audit-log?action=bridge_connection.created",
            None,
        )
        .await?;
    if !log["items"]
        .as_array()
        .is_some_and(|items| items.iter().any(|e| e["actor"]["id"] == alice_id()))
    {
        bail!("the audit log has no entry with alice as actor: {log}");
    }

    // Removed with her token: the URL stops working.
    let (status, body) = user
        .call(
            reqwest::Method::DELETE,
            &format!("/webhooks/{}/linear-issues", escape(&room_id)),
            None,
        )
        .await?;
    if status != 200 {
        bail!("removing the user's webhook answered {status}: {body}");
    }
    let deadline = Instant::now() + REPLY_WAIT;
    loop {
        let again = reqwest::Client::new()
            .post(&url)
            .json(&sample)
            .send()
            .await
            .with_context(|| format!("POST {url} after removal"))?;
        if !again.status().is_success() {
            break;
        }
        if Instant::now() > deadline {
            bail!("the URL still works {REPLY_WAIT:?} after the user removed the webhook");
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    let listed = user.ok(reqwest::Method::GET, "/webhooks", None).await?;
    if listed["items"]
        .as_array()
        .is_some_and(|items| items.iter().any(|w| w["name"] == "linear-issues"))
    {
        bail!("the removed webhook is still listed: {listed}");
    }
    Ok(())
}

/// Datadog's sample payload, the one the source's transform is tested on.
const DATADOG_SAMPLE: &str =
    include_str!("../../hs-bridges/hookshot-sources/datadog/sample-alert.json");

/// The second half: a webhook made through the admin API, as the bot, from a source.
async fn provisioned_story(
    admin: &Caller,
    alice: &Caller,
    bot: &str,
    public_url: &str,
) -> Result<()> {
    // The sources are listed, with the one this uses.
    let sources = admin
        .ok(
            reqwest::Method::GET,
            &format!("/bridge-offerings/{TYPE_ID}/webhook-sources"),
            None,
        )
        .await?;
    let datadog = sources["items"]
        .as_array()
        .and_then(|items| items.iter().find(|s| s["id"] == "datadog"))
        .cloned()
        .with_context(|| format!("no datadog source in {sources}"))?;
    if datadog["transforms"] != true {
        bail!("the datadog source does not transform: {datadog}");
    }

    // Made in a new room the bot creates, with alice invited; the answer carries the URL once.
    let (status, made) = admin
        .call(
            reqwest::Method::POST,
            &format!("/bridge-offerings/{TYPE_ID}/connections"),
            Some(json!({
                "source": "datadog",
                "name": "datadog-alerts",
                "room": {"create": {"name": "Alerts", "topic": "Datadog monitors", "invite": [alice_id()]}},
            })),
        )
        .await?;
    if status != 201 {
        bail!("making the connection answered {status}: {made}");
    }
    let url = made["url"].as_str().unwrap_or_default().to_owned();
    let prefix = format!("{public_url}/webhook/");
    if !url.starts_with(&prefix) || url.len() != prefix.len() + 36 {
        bail!("the URL is not public URL + /webhook/ + a UUID: {url}");
    }
    if made["source"] != "datadog" || made["name"] != "datadog-alerts" {
        bail!("the answer is not the connection asked for: {made}");
    }
    if !made["setup"].as_str().unwrap_or_default().contains(&url) {
        bail!("the setup text lacks the URL: {made}");
    }
    let alerts_room = made["room_id"]
        .as_str()
        .context("room_id in the answer")?
        .to_owned();

    // alice was invited by the bot and joins; the room is named as asked.
    let invited = invite_from(alice, bot).await?;
    if invited != alerts_room {
        bail!("alice was invited to {invited}, not the Alerts room {alerts_room}");
    }
    alice
        .ok(
            reqwest::Method::POST,
            &format!("/join/{}", escape(&alerts_room)),
            Some(json!({})),
        )
        .await?;
    let name = alice
        .ok(
            reqwest::Method::GET,
            &format!("/rooms/{}/state/m.room.name/", escape(&alerts_room)),
            None,
        )
        .await?;
    if name["name"] != "Alerts" {
        bail!("the room is named {name}, not Alerts");
    }
    // The bot said so in the room, without the URL.
    let notice = wait_for_message(alice, &alerts_room, Some(bot), "the bot's notice", |b| {
        b.contains("datadog-alerts")
    })
    .await?;
    if notice["content"]["body"]
        .as_str()
        .unwrap_or_default()
        .contains("/webhook/")
    {
        bail!("the bot said the URL in the room: {notice}");
    }

    // Datadog's sample posted to the URL: the bridge picked the connection up from the state
    // event, ran the source's transformation, and the room has the notice.
    let sample: Value = serde_json::from_str(DATADOG_SAMPLE)?;
    let posted = reqwest::Client::new()
        .post(&url)
        .json(&sample)
        .send()
        .await
        .with_context(|| format!("POST {url}"))?;
    if !posted.status().is_success() {
        bail!(
            "POST {url} answered {}: {}",
            posted.status(),
            posted.text().await.unwrap_or_default()
        );
    }
    let landed = wait_for_message(alice, &alerts_room, None, "the Datadog notice", |b| {
        b.contains("CPU is high on web-1")
    })
    .await?;
    let body = landed["content"]["body"].as_str().unwrap_or_default();
    if !body.contains("**Alert** P2") || !body.contains("env:prod") {
        bail!("the notice is not the source's rendering: {body:?}");
    }
    if landed["content"]["msgtype"] != "m.notice" {
        bail!("the notice is not an m.notice: {landed}");
    }
    if landed["content"]["formatted_body"]
        .as_str()
        .is_none_or(|h| !h.contains("<strong>Alert</strong>"))
    {
        bail!("the bridge did not render the Markdown: {landed}");
    }

    // Listed with its source and its room; the secret id is nowhere in the listing.
    let connections = admin
        .ok(
            reqwest::Method::GET,
            &format!("/bridge-offerings/{TYPE_ID}/connections"),
            None,
        )
        .await?;
    let items = connections["items"].as_array().cloned().unwrap_or_default();
    let listed = items
        .iter()
        .find(|c| c["name"] == "datadog-alerts")
        .with_context(|| format!("datadog-alerts is not listed: {items:?}"))?;
    if listed["source"] != "datadog"
        || listed["state"] != "active"
        || listed["room_name"] != "Alerts"
        || listed["room_id"] != alerts_room.as_str()
    {
        bail!("the connection is not listed as expected: {listed}");
    }
    let id = url.trim_start_matches(&prefix);
    if connections.to_string().contains(id) {
        bail!("the listing carries the secret id");
    }

    // Removed through the API: the bridge purges the connection when it sees the emptied
    // state event, so the URL stops working; the listing says removed.
    let (status, _) = admin
        .call(
            reqwest::Method::DELETE,
            &format!(
                "/bridge-offerings/{TYPE_ID}/connections/{}/datadog-alerts",
                escape(&alerts_room)
            ),
            None,
        )
        .await?;
    if status != 204 {
        bail!("removing the connection answered {status}");
    }
    let deadline = Instant::now() + REPLY_WAIT;
    loop {
        let again = reqwest::Client::new()
            .post(&url)
            .json(&sample)
            .send()
            .await
            .with_context(|| format!("POST {url} after removal"))?;
        if !again.status().is_success() {
            break;
        }
        if Instant::now() > deadline {
            bail!("the URL still works {REPLY_WAIT:?} after the connection was removed");
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    let connections = admin
        .ok(
            reqwest::Method::GET,
            &format!("/bridge-offerings/{TYPE_ID}/connections"),
            None,
        )
        .await?;
    let items = connections["items"].as_array().cloned().unwrap_or_default();
    let listed = items
        .iter()
        .find(|c| c["state_key"] == "datadog-alerts")
        .with_context(|| format!("datadog-alerts is not listed after removal: {items:?}"))?;
    if listed["state"] != "removed" || listed.get("source").is_some() {
        bail!("the removed connection is not listed as removed: {listed}");
    }
    // Gone: a second removal is not found.
    let (status, _) = admin
        .call(
            reqwest::Method::DELETE,
            &format!(
                "/bridge-offerings/{TYPE_ID}/connections/{}/datadog-alerts",
                escape(&alerts_room)
            ),
            None,
        )
        .await?;
    if status != 404 {
        bail!("removing a removed connection answered {status}");
    }
    Ok(())
}

fn escape(room_id: &str) -> String {
    room_id.replace('!', "%21").replace(':', "%3A")
}

fn alice_id() -> String {
    format!("@alice:{SERVER_NAME}")
}

/// The room `bot` has invited `client` to (the bot's admin room with them), from `/sync`.
async fn invite_from(client: &Caller, bot: &str) -> Result<String> {
    let deadline = Instant::now() + REPLY_WAIT;
    loop {
        let sync = client
            .ok(reqwest::Method::GET, "/sync?timeout=0", None)
            .await?;
        if let Some(invites) = sync["rooms"]["invite"].as_object() {
            for (room_id, room) in invites {
                let from_bot = room["invite_state"]["events"]
                    .as_array()
                    .is_some_and(|events| {
                        events.iter().any(|e| {
                            e["type"] == "m.room.member"
                                && e["sender"].as_str() == Some(bot)
                                && e["content"]["membership"] == "invite"
                        })
                    });
                if from_bot {
                    return Ok(room_id.clone());
                }
            }
        }
        if Instant::now() > deadline {
            bail!("the bot never invited alice to its admin room within {REPLY_WAIT:?}");
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

fn run<F>(story: F)
where
    F: std::future::Future<Output = Result<()>>,
{
    if let Some(reason) = skip_reason() {
        eprintln!("SKIP: {reason}");
        return;
    }
    let rt = tokio::runtime::Runtime::new().expect("a runtime");
    if let Err(e) = rt.block_on(story) {
        panic!("{e:#}");
    }
}

#[test]
fn hookshot_runs_from_the_rendered_files_and_bridges_a_webhook_into_a_room() {
    run(webhook_story());
}
