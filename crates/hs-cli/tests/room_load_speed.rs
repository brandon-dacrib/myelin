//! How long a room joined over federation and read back takes to load after a restart, on two
//! real `hs` binaries (2026-10-15, `docs/status/04-room-and-events.md`): the shape of the rooms
//! the demo spent minutes loading.
//!
//! A hosts a room: `HS_ROOM_LOAD_MEMBERS` users of A (default 200) join it one by one, each
//! saying something, with a topic change every 25 joins, then `HS_ROOM_LOAD_MESSAGES` more
//! messages (default 1,000). Bob on B joins and reads the whole history back (`/messages`
//! backward to the room's creation), which makes B backfill it from A with the state at each
//! batch, so B holds every member event A's join response gave it as an outlier, placed in the
//! timeline as its history arrives. B restarts; bob reads the room once more, which loads it.
//! The test prints the first read's time and B's own figures from `/metrics`
//! (`hs_room_load_seconds`, and on a build that has them `hs_room_load_phase_seconds`).
//!
//! Ignored: a measurement, minutes in a debug build. Run it alone:
//!
//! ```sh
//! cargo test -p hs-cli --test room_load_speed -- --ignored --nocapture
//! ```
//!
//! The servers federate over TLS through a terminating proxy, as `tests/federation_catch_up.rs`
//! does: the binary federates over HTTPS only.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tokio::net::{TcpListener, TcpStream};

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

    fn wait_for(&mut self, needle: &str) -> String {
        let deadline = Instant::now() + Duration::from_secs(180);
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

    /// Every line logged so far that contains `needle`.
    fn logged(&mut self, needle: &str) -> Vec<String> {
        while let Ok(line) = self.lines.try_recv() {
            self.seen.push(line);
        }
        self.seen
            .iter()
            .filter(|line| line.contains(needle))
            .cloned()
            .collect()
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

fn reserve_port() -> u16 {
    static HANDED_OUT: Mutex<Vec<u16>> = Mutex::new(Vec::new());
    loop {
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let mut handed_out = HANDED_OUT.lock().unwrap();
        if !handed_out.contains(&port) {
            handed_out.push(port);
            return port;
        }
    }
}

fn config_yaml(
    client_port: u16,
    server_name: &str,
    data_dir: &std::path::Path,
    ca_path: &std::path::Path,
) -> String {
    let media_dir = data_dir.join("media");
    format!(
        "server:\n  server_name: \"{server_name}\"\n\
         listeners:\n  listeners:\n    - port: {client_port}\n      bind_addresses: [\"127.0.0.1\"]\n      resources: [client, federation, health, metrics]\n\
         storage:\n  backend: embedded\n  data_dir: {data_dir:?}\n\
         media:\n  storage:\n    backend: local\n    path: {media_dir:?}\n\
         auth:\n  enable_registration: true\n\
         federation:\n  ip_range_blocklist: []\n  custom_ca_certificates: [{ca_path:?}]\n  max_retry_backoff: 2s\n\
         rate_limits:\n  enabled: false\n"
    )
}

struct Tls {
    ca_pem: String,
    acceptor: tokio_rustls::TlsAcceptor,
}

fn mint_tls() -> Tls {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let ca_key = rcgen::KeyPair::generate().unwrap();
    let mut ca_params = rcgen::CertificateParams::new(Vec::<String>::new()).unwrap();
    ca_params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    ca_params
        .distinguished_name
        .push(rcgen::DnType::CommonName, "room_load_speed test CA");
    let ca = rcgen::CertifiedIssuer::self_signed(ca_params, ca_key).unwrap();
    let ca_pem = ca.pem();
    let leaf_key = rcgen::KeyPair::generate().unwrap();
    let leaf_params = rcgen::CertificateParams::new(vec!["127.0.0.1".to_string()]).unwrap();
    let leaf = leaf_params.signed_by(&leaf_key, &ca).unwrap();
    let key_der = rustls_pki_types::PrivateKeyDer::Pkcs8(
        rustls_pki_types::PrivatePkcs8KeyDer::from(leaf_key.serialize_der()),
    );
    let server_config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(vec![leaf.der().clone()], key_der)
        .unwrap();
    Tls {
        ca_pem,
        acceptor: tokio_rustls::TlsAcceptor::from(Arc::new(server_config)),
    }
}

struct Proxy {
    accept: tokio::task::AbortHandle,
    connections: Arc<Mutex<Vec<tokio::task::AbortHandle>>>,
}

impl Proxy {
    fn start(
        listener: TcpListener,
        acceptor: tokio_rustls::TlsAcceptor,
        backend_port: u16,
    ) -> Self {
        let connections: Arc<Mutex<Vec<tokio::task::AbortHandle>>> = Arc::default();
        let tracked = connections.clone();
        let accept = tokio::spawn(async move {
            loop {
                let Ok((tcp, _)) = listener.accept().await else {
                    return;
                };
                let acceptor = acceptor.clone();
                let connection = tokio::spawn(async move {
                    let Ok(mut tls) = acceptor.accept(tcp).await else {
                        return;
                    };
                    let Ok(mut backend) = TcpStream::connect(("127.0.0.1", backend_port)).await
                    else {
                        return;
                    };
                    let _ = tokio::io::copy_bidirectional(&mut tls, &mut backend).await;
                });
                tracked.lock().unwrap().push(connection.abort_handle());
            }
        })
        .abort_handle();
        Self {
            accept,
            connections,
        }
    }

    fn stop(self) {
        self.accept.abort();
        for connection in self.connections.lock().unwrap().drain(..) {
            connection.abort();
        }
    }
}

async fn call(
    method: reqwest::Method,
    url: String,
    token: Option<&str>,
    body: Option<Value>,
) -> (reqwest::StatusCode, Value) {
    let mut request = reqwest::Client::new()
        .request(method, url)
        .timeout(Duration::from_secs(600));
    if let Some(token) = token {
        request = request.bearer_auth(token);
    }
    if let Some(body) = body {
        request = request.json(&body);
    }
    let response = request.send().await.unwrap();
    let status = response.status();
    let body: Value = response.json().await.unwrap_or(Value::Null);
    (status, body)
}

async fn register(base: &str, username: &str) -> (String, String) {
    let (_, first) = call(
        reqwest::Method::POST,
        format!("{base}/_matrix/client/v3/register"),
        None,
        Some(json!({"username": username, "password": "correct horse"})),
    )
    .await;
    let session = first["session"]
        .as_str()
        .unwrap_or_else(|| panic!("registration did not offer a UIA session: {first}"))
        .to_owned();
    let (status, done) = call(
        reqwest::Method::POST,
        format!("{base}/_matrix/client/v3/register"),
        None,
        Some(json!({
            "username": username,
            "password": "correct horse",
            "auth": {"type": "m.login.dummy", "session": session},
        })),
    )
    .await;
    assert_eq!(status, 200, "registration failed: {done}");
    (
        done["user_id"].as_str().unwrap().to_owned(),
        done["access_token"].as_str().unwrap().to_owned(),
    )
}

async fn send(base: &str, token: &str, room_id: &str, event_type: &str, content: Value) {
    static TXN: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let txn = TXN.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let (status, response) = call(
        reqwest::Method::PUT,
        format!("{base}/_matrix/client/v3/rooms/{room_id}/send/{event_type}/t{txn}"),
        Some(token),
        Some(content),
    )
    .await;
    assert_eq!(status, 200, "send failed: {response}");
}

/// Reads the room's history backward to its start as `token`, returning how many events the
/// pages held and how many pages it took.
async fn read_back_to_the_start(base: &str, token: &str, room_id: &str) -> (usize, usize) {
    let mut from: Option<String> = None;
    let mut events = 0usize;
    let mut pages = 0usize;
    loop {
        let url = match &from {
            Some(from) => format!(
                "{base}/_matrix/client/v3/rooms/{room_id}/messages?dir=b&limit=100&from={from}"
            ),
            None => format!("{base}/_matrix/client/v3/rooms/{room_id}/messages?dir=b&limit=100"),
        };
        let (status, page) = call(reqwest::Method::GET, url, Some(token), None).await;
        assert_eq!(status, 200, "{page}");
        pages += 1;
        let chunk = page["chunk"].as_array().map_or(0, Vec::len);
        events += chunk;
        let creates = page["chunk"]
            .as_array()
            .is_some_and(|c| c.iter().any(|e| e["type"] == "m.room.create"));
        match page["end"].as_str() {
            Some(end) if !creates && pages < 1000 => from = Some(end.to_owned()),
            _ => return (events, pages),
        }
    }
}

/// The lines of B's `/metrics` that start with one of `prefixes`.
async fn metrics(base: &str, prefixes: &[&str]) -> Vec<String> {
    let text = reqwest::get(format!("{base}/metrics"))
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    text.lines()
        .filter(|line| prefixes.iter().any(|p| line.starts_with(p)))
        .map(str::to_owned)
        .collect()
}

fn env_usize(name: &str, default: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "a measurement: two real servers, a room of hundreds of members, minutes in a debug build"]
async fn measure_the_load_of_a_backfilled_federated_room_after_a_restart() {
    let members = env_usize("HS_ROOM_LOAD_MEMBERS", 200);
    let messages = env_usize("HS_ROOM_LOAD_MESSAGES", 1_000);
    let tls = mint_tls();
    let dir = tempfile::tempdir().unwrap();
    let ca_path = dir.path().join("ca.pem");
    std::fs::write(&ca_path, &tls.ca_pem).unwrap();

    let a_port = reserve_port();
    let a_tls_port = reserve_port();
    let b_port = reserve_port();
    let b_tls_port = reserve_port();
    let a_name = format!("127.0.0.1:{a_tls_port}");
    let b_name = format!("127.0.0.1:{b_tls_port}");
    let a_base = format!("http://127.0.0.1:{a_port}");
    let b_base = format!("http://127.0.0.1:{b_port}");
    let a_config = dir.path().join("a.yaml");
    std::fs::write(
        &a_config,
        config_yaml(a_port, &a_name, &dir.path().join("a"), &ca_path),
    )
    .unwrap();
    let b_config = dir.path().join("b.yaml");
    std::fs::write(
        &b_config,
        config_yaml(b_port, &b_name, &dir.path().join("b"), &ca_path),
    )
    .unwrap();

    let a_proxy = Proxy::start(
        TcpListener::bind(("127.0.0.1", a_tls_port)).await.unwrap(),
        tls.acceptor.clone(),
        a_port,
    );
    let b_proxy = Proxy::start(
        TcpListener::bind(("127.0.0.1", b_tls_port)).await.unwrap(),
        tls.acceptor.clone(),
        b_port,
    );
    let mut a = HsProcess::serve(&a_config);
    let mut b = HsProcess::serve(&b_config);
    a.wait_for("listening");
    b.wait_for("listening");

    let started = Instant::now();
    let (_alice, alice) = register(&a_base, "alice").await;
    let (status, created) = call(
        reqwest::Method::POST,
        format!("{a_base}/_matrix/client/v3/createRoom"),
        Some(&alice),
        Some(json!({"preset": "public_chat", "name": "load speed", "room_version": "11"})),
    )
    .await;
    assert_eq!(status, 200, "{created}");
    let room_id = created["room_id"].as_str().unwrap().to_owned();
    for n in 0..members {
        let (_, token) = register(&a_base, &format!("member{n}")).await;
        let (status, joined) = call(
            reqwest::Method::POST,
            format!("{a_base}/_matrix/client/v3/join/{room_id}"),
            Some(&token),
            Some(json!({})),
        )
        .await;
        assert_eq!(status, 200, "{joined}");
        send(
            &a_base,
            &token,
            &room_id,
            "m.room.message",
            json!({"msgtype": "m.text", "body": format!("hello from member {n}")}),
        )
        .await;
        if n % 25 == 0 {
            send(
                &a_base,
                &alice,
                &room_id,
                "m.room.topic",
                json!({"topic": format!("topic {n}")}),
            )
            .await;
        }
    }
    for n in 0..messages {
        send(
            &a_base,
            &alice,
            &room_id,
            "m.room.message",
            json!({"msgtype": "m.text", "body": format!("message {n}, about the length of a line of chat")}),
        )
        .await;
    }
    println!(
        "A's room: {members} members, {messages} more messages, built in {:.0}s",
        started.elapsed().as_secs_f64()
    );

    let (_bob, bob) = register(&b_base, "bob").await;
    let started = Instant::now();
    let (status, joined) = call(
        reqwest::Method::POST,
        format!("{b_base}/_matrix/client/v3/join/{room_id}?server_name={a_name}"),
        Some(&bob),
        Some(json!({})),
    )
    .await;
    assert_eq!(status, 200, "join failed: {joined}");
    println!("bob joined in {:.1}s", started.elapsed().as_secs_f64());
    let started = Instant::now();
    let (events, pages) = read_back_to_the_start(&b_base, &bob, &room_id).await;
    println!(
        "bob read {events} events back in {pages} pages in {:.0}s (B backfilled them from A)",
        started.elapsed().as_secs_f64()
    );

    // B restarts; the room is no longer resident.
    b_proxy.stop();
    b.stop();
    let started = Instant::now();
    let mut b = HsProcess::serve(&b_config);
    b.wait_for("listening");
    println!("B restarted in {:.1}s", started.elapsed().as_secs_f64());
    let _b_proxy = Proxy::start(
        TcpListener::bind(("127.0.0.1", b_tls_port)).await.unwrap(),
        tls.acceptor.clone(),
        b_port,
    );
    let before = metrics(&b_base, &["hs_room_load_seconds_count"]).await;
    let started = Instant::now();
    let (status, page) = call(
        reqwest::Method::GET,
        format!("{b_base}/_matrix/client/v3/rooms/{room_id}/messages?dir=b&limit=20"),
        Some(&bob),
        None,
    )
    .await;
    assert_eq!(status, 200, "{page}");
    println!(
        "bob's first read of the room after the restart: {:.3}s (loads before it: {before:?})",
        started.elapsed().as_secs_f64()
    );
    let started = Instant::now();
    let (events_again, _) = read_back_to_the_start(&b_base, &bob, &room_id).await;
    println!(
        "bob read {events_again} events back again in {:.1}s",
        started.elapsed().as_secs_f64()
    );
    for line in metrics(
        &b_base,
        &[
            "hs_room_load_seconds_sum",
            "hs_room_load_seconds_count",
            "hs_room_load_phase_seconds_sum",
            "hs_room_slow_loads",
            "hs_room_resident_rooms",
        ],
    )
    .await
    {
        println!("B /metrics: {line}");
    }
    for line in b.logged("a room load took longer") {
        println!("B logged: {line}");
    }
    assert!(events_again >= events, "{events_again} < {events}");

    a_proxy.stop();
    a.stop();
    b.stop();
}
