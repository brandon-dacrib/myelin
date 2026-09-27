//! The outbound federation queue survives a restart of the real binary.
//!
//! Two real `hs serve` processes, A and B, federate with each other over TLS. The binary does
//! not terminate TLS itself (`hs serve` serves plaintext and says so when a listener declares
//! TLS), and outbound federation is HTTPS only, so this test terminates it: a private CA and a
//! leaf certificate for `127.0.0.1` from `rcgen`, and a TLS-terminating proxy in this process in
//! front of each server's plaintext listener. Each server's name is `127.0.0.1:{its proxy's
//! port}`, and each trusts the CA through `federation.custom_ca_certificates`, exactly as an
//! operator with a private CA would configure it. The proxies are also how B goes away: closing
//! B's proxy is B's port closing, and A gets `connection refused`.
//!
//! The sequence is the one `docs/next-steps.md` item 4 describes: B is in a room A hosts; B
//! becomes unreachable; alice on A says something (A queues it for B and fails to deliver);
//! A is stopped and started again over the same data directory; B becomes reachable; and what
//! alice said arrives at B. Before and after A's restart its admin API lists B as a failing
//! destination with one PDU pending -- the persisted queue and backoff, read back by the
//! process that did not queue them.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tokio::net::{TcpListener, TcpStream};

// ---------------------------------------------------------------------------------------------
// The real binary.
// ---------------------------------------------------------------------------------------------

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

    /// Reads the log until a line contains `needle`. A condition, not a duration: the timeout
    /// only bounds how long a broken server can hang the suite.
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

    /// Asks the server to stop the way `docker stop` or Kubernetes would, waits for it to, and
    /// returns everything it logged.
    fn stop(mut self) -> String {
        let pid = self.child.id().to_string();
        let _ = std::process::Command::new("kill")
            .args(["-TERM", &pid])
            .status();
        let _ = self.child.wait();
        while let Ok(line) = self.lines.recv() {
            self.seen.push(line);
        }
        self.seen.join("\n")
    }
}

impl Drop for HsProcess {
    fn drop(&mut self) {
        // Only reached with the child still running when a test panicked; `stop` has already
        // waited otherwise, and killing an exited child is a harmless error.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// A port for a server that is started as a subprocess from a configuration file, which has to
/// name one. Never the same port twice in this test process; another process on the machine
/// taking it in the gap remains possible, and rare.
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
    // `max_retry_backoff` short so the whole outage-and-recovery fits a test: it caps both the
    // client's connection-level backoff and the sender's per-transaction one.
    format!(
        "server:\n  server_name: \"{server_name}\"\n\
         listeners:\n  listeners:\n    - port: {client_port}\n      bind_addresses: [\"127.0.0.1\"]\n      resources: [client, federation, health]\n\
         storage:\n  backend: embedded\n  data_dir: {data_dir:?}\n\
         media:\n  storage:\n    backend: local\n    path: {media_dir:?}\n\
         auth:\n  enable_registration: true\n\
         federation:\n  ip_range_blocklist: []\n  custom_ca_certificates: [{ca_path:?}]\n  max_retry_backoff: 2s\n"
    )
}

// ---------------------------------------------------------------------------------------------
// TLS: a private CA, a leaf for 127.0.0.1, and a terminating proxy.
// ---------------------------------------------------------------------------------------------

struct Tls {
    ca_pem: String,
    acceptor: tokio_rustls::TlsAcceptor,
}

fn mint_tls() -> Tls {
    // Fails only if something already installed one, which is fine for a test.
    let _ = rustls::crypto::ring::default_provider().install_default();
    let ca_key = rcgen::KeyPair::generate().unwrap();
    let mut ca_params = rcgen::CertificateParams::new(Vec::<String>::new()).unwrap();
    ca_params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    ca_params
        .distinguished_name
        .push(rcgen::DnType::CommonName, "federation_restart test CA");
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

/// A TLS-terminating proxy: accepts on `listener`, forwards the plaintext to `backend_port`.
/// [`Proxy::stop`] closes the port and cuts every connection through it, so a client that had
/// one pooled gets nothing from it either.
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

// ---------------------------------------------------------------------------------------------
// Client-side helpers.
// ---------------------------------------------------------------------------------------------

/// A client per request: a pooled connection to a process that has since been restarted is no
/// use against its successor.
async fn call(
    method: reqwest::Method,
    url: String,
    token: Option<&str>,
    body: Option<Value>,
) -> (reqwest::StatusCode, Value) {
    let mut request = reqwest::Client::new().request(method, url);
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

/// Registers `username` through the real UIA dance; returns `(user_id, access_token)`.
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

async fn send_message(base: &str, token: &str, room_id: &str, body: &str) -> String {
    let txn = format!(
        "txn-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    let (status, response) = call(
        reqwest::Method::PUT,
        format!("{base}/_matrix/client/v3/rooms/{room_id}/send/m.room.message/{txn}"),
        Some(token),
        Some(json!({"msgtype": "m.text", "body": body})),
    )
    .await;
    assert_eq!(status, 200, "send failed: {response}");
    response["event_id"].as_str().unwrap().to_owned()
}

fn timeline_bodies(sync: &Value, room_id: &str) -> Vec<String> {
    sync["rooms"]["join"][room_id]["timeline"]["events"]
        .as_array()
        .map(|events| {
            events
                .iter()
                .filter_map(|e| e["content"]["body"].as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default()
}

/// Initial syncs until `wanted` is true of the response, or `deadline` has passed. A
/// condition, not a duration: delivery runs off background tasks and backoffs.
async fn sync_until(
    base: &str,
    token: &str,
    deadline: Duration,
    wanted: impl Fn(&Value) -> bool,
) -> Value {
    let until = Instant::now() + deadline;
    let mut last = Value::Null;
    while Instant::now() < until {
        let (_, response) = call(
            reqwest::Method::GET,
            format!("{base}/_matrix/client/v3/sync?timeout=500"),
            Some(token),
            None,
        )
        .await;
        if wanted(&response) {
            return response;
        }
        last = response;
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    panic!("the sync never said what was expected; the last one said: {last}");
}

/// `GET /api/v1/federation/destinations` until `wanted` is true of the row for `server_name`
/// (absent rows are `Null`), or `deadline` has passed.
async fn destination_until(
    base: &str,
    admin: &str,
    server_name: &str,
    deadline: Duration,
    wanted: impl Fn(&Value) -> bool,
) -> Value {
    let until = Instant::now() + deadline;
    let mut last = Value::Null;
    while Instant::now() < until {
        let (status, listed) = call(
            reqwest::Method::GET,
            format!("{base}/api/v1/federation/destinations"),
            Some(admin),
            None,
        )
        .await;
        assert_eq!(status, 200, "{listed}");
        let row = listed["items"]
            .as_array()
            .and_then(|items| {
                items
                    .iter()
                    .find(|item| item["server_name"] == server_name)
                    .cloned()
            })
            .unwrap_or(Value::Null);
        if wanted(&row) {
            return row;
        }
        last = row;
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    panic!("the destination never looked as expected; it last looked like: {last}");
}

/// The first administrator, from the setup link `hs serve` logged: the admin API's bearer token
/// is a Matrix access token, so it is still good after the process restarts.
async fn admin_token_of(server: &mut HsProcess, base: &str) -> String {
    let setup_line = server.wait_for("setup_link=");
    let setup_token: String = setup_line
        .split_once("/admin/setup#token=")
        .unwrap_or_else(|| panic!("not a setup link: {setup_line}"))
        .1
        .chars()
        .take_while(char::is_ascii_alphabetic)
        .collect();
    let (status, session) = call(
        reqwest::Method::POST,
        format!("{base}/api/v1/setup"),
        None,
        Some(json!({"setup_token": setup_token, "username": "ops", "password": "hunter2-ops-restart"})),
    )
    .await;
    assert_eq!(status, 201, "{session}");
    session["access_token"].as_str().unwrap().to_owned()
}

/// Waits for the server to be listening, then finds `needle` among what it logged on the way
/// (or waits for it, if it is still to come).
fn wait_for_listening_and(server: &mut HsProcess, needle: &str) -> String {
    server.wait_for("listening");
    match server.seen.iter().find(|line| line.contains(needle)) {
        Some(line) => line.clone(),
        None => server.wait_for(needle),
    }
}

// ---------------------------------------------------------------------------------------------
// The test.
// ---------------------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_event_queued_for_a_server_that_is_down_arrives_after_a_restart_of_the_sender() {
    let tls = mint_tls();
    let dir = tempfile::tempdir().unwrap();
    let ca_path = dir.path().join("ca.pem");
    std::fs::write(&ca_path, &tls.ca_pem).unwrap();

    // Four ports: each server's plaintext listener and the TLS proxy in front of it. The
    // proxy's port is the server's name.
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
    let admin = admin_token_of(&mut a, &a_base).await;

    // Alice on A makes a room; bob on B joins it; a message crosses, so federation between the
    // two works before anything is taken away.
    let (_alice, alice_token) = register(&a_base, "alice").await;
    let (_bob, bob_token) = register(&b_base, "bob").await;
    let (status, created) = call(
        reqwest::Method::POST,
        format!("{a_base}/_matrix/client/v3/createRoom"),
        Some(&alice_token),
        Some(json!({"preset": "public_chat", "name": "restart", "room_version": "11"})),
    )
    .await;
    assert_eq!(status, 200, "{created}");
    let room_id = created["room_id"].as_str().unwrap().to_owned();
    let (status, joined) = call(
        reqwest::Method::POST,
        format!("{b_base}/_matrix/client/v3/join/{room_id}?server_name={a_name}"),
        Some(&bob_token),
        Some(json!({})),
    )
    .await;
    assert_eq!(status, 200, "join failed: {joined}");
    send_message(&a_base, &alice_token, &room_id, "before the outage").await;
    sync_until(&b_base, &bob_token, Duration::from_secs(30), |sync| {
        timeline_bodies(sync, &room_id).contains(&"before the outage".to_owned())
    })
    .await;
    // A has delivered to B, and its admin API says so.
    let row = destination_until(&a_base, &admin, &b_name, Duration::from_secs(30), |row| {
        row["last_successful_at"].is_string()
    })
    .await;
    assert!(row["failing_since"].is_null(), "{row}");

    // B's port closes. Alice says something A now cannot deliver: it is queued for B, the
    // attempt fails, and A's admin API shows a failing destination with one PDU pending.
    b_proxy.stop();
    send_message(&a_base, &alice_token, &room_id, "said while B was down").await;
    let row = destination_until(&a_base, &admin, &b_name, Duration::from_secs(30), |row| {
        row["failing_since"].is_string() && row["pending_pdu_count"] == 1
    })
    .await;
    assert!(row["retry_last_at"].is_string(), "{row}");
    let failing_since = row["failing_since"].as_str().unwrap().to_owned();

    // A stops and comes back over the same data directory. The queue and the backoff are what
    // the previous process left, read back by this one.
    let a_log = a.stop();
    assert!(
        a_log.contains("kept for the next start"),
        "A should have said what it kept queued:\n{a_log}"
    );
    let mut a = HsProcess::serve(&a_config);
    let resumed = wait_for_listening_and(&mut a, "resuming an outbound federation queue");
    assert!(resumed.contains(&b_name), "{resumed}");
    let row = destination_until(&a_base, &admin, &b_name, Duration::from_secs(30), |row| {
        row["pending_pdu_count"] == 1
    })
    .await;
    assert_eq!(
        row["failing_since"].as_str(),
        Some(failing_since.as_str()),
        "the failure run's start survives the restart: {row}"
    );

    // B's port opens again, and what alice said while it was closed arrives -- sent by a
    // process that never saw her send it.
    let b_proxy = Proxy::start(
        TcpListener::bind(("127.0.0.1", b_tls_port)).await.unwrap(),
        tls.acceptor.clone(),
        b_port,
    );
    sync_until(&b_base, &bob_token, Duration::from_secs(60), |sync| {
        timeline_bodies(sync, &room_id).contains(&"said while B was down".to_owned())
    })
    .await;
    let row = destination_until(&a_base, &admin, &b_name, Duration::from_secs(30), |row| {
        row["pending_pdu_count"] == 0 && row["failing_since"].is_null()
    })
    .await;
    assert!(row["last_successful_at"].is_string(), "{row}");

    // And nothing was sent twice: bob's timeline has each message once.
    let (_, sync) = call(
        reqwest::Method::GET,
        format!("{b_base}/_matrix/client/v3/sync?timeout=0"),
        Some(&bob_token),
        None,
    )
    .await;
    let bodies = timeline_bodies(&sync, &room_id);
    assert_eq!(
        bodies
            .iter()
            .filter(|body| *body == "said while B was down")
            .count(),
        1,
        "{bodies:?}"
    );

    b_proxy.stop();
    a_proxy.stop();
    a.stop();
    b.stop();
}
