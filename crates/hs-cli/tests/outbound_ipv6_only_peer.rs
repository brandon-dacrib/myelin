//! A peer that listens on IPv6 alone, reached under the default outbound policy
//! (`network.outbound.ipv4_only: true`): the failure mode of the Sytest run of 2026-10-04,
//! where haproxy bound `::1` only, `localhost` resolved to `::1` and `127.0.0.1`, the policy
//! dropped `::1`, and every federation request was "connection refused" on `127.0.0.1` with
//! nothing in the log saying why.
//!
//! Two `hs serve` instances in one process (the process-wide policy is why this is one test):
//! the first is named `v6-peer.test:{port}` and listens on `[::1]` only; the second resolves
//! that name -- through `ServeOptions::federation_resolvers`, the one seam a test may use -- to
//! `::1` and `127.0.0.1`, the way `localhost` resolves on most hosts. The second server asks for
//! a profile on the first over federation and must log exactly one warning that names the host,
//! the IPv6 address it did not try and the setting, however many times it is asked within the
//! warning interval. A third server, booted with `ipv4_only: false`, then reaches the peer: the
//! warning's advice works. Skipped (printing `SKIP`) on a host with no IPv6 loopback.

use std::net::{IpAddr, Ipv6Addr, SocketAddr};
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};

const PEER_HOST: &str = "v6-peer.test";

/// A port free on `[::1]` and on `127.0.0.1` (so the IPv4 connect is refused, not answered by
/// something else), or `None` when this host has no IPv6 loopback.
fn reserve_v6_port() -> Option<u16> {
    for _ in 0..20 {
        let v6 = std::net::TcpListener::bind((Ipv6Addr::LOCALHOST, 0)).ok()?;
        let port = v6.local_addr().ok()?.port();
        if std::net::TcpListener::bind(("127.0.0.1", port)).is_ok() {
            return Some(port);
        }
    }
    None
}

fn reserve_v4_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn config(
    server_name: &str,
    bind: &str,
    port: u16,
    data_dir: &std::path::Path,
    extra: &str,
) -> hs_config::Config {
    let media_dir = data_dir.join("media");
    let yaml = format!(
        "server:\n  server_name: \"{server_name}\"\n\
         listeners:\n  listeners:\n    - port: {port}\n      bind_addresses: [\"{bind}\"]\n      resources: [client, federation, health, metrics]\n\
         storage:\n  backend: embedded\n  data_dir: {data_dir:?}\n\
         media:\n  storage:\n    backend: local\n    path: {media_dir:?}\n\
         auth:\n  enable_registration: true\n\
         federation:\n  ip_range_blocklist: []\n\
         rate_limits:\n  enabled: false\n\
         {extra}"
    );
    hs_config::Config::from_yaml(&yaml).expect("the test configuration parses")
}

/// Resolves `v6-peer.test` to `::1` then `127.0.0.1`, as `localhost` resolves; no SRV records.
struct LikeLocalhost;

#[async_trait::async_trait]
impl hs_federation::discovery::AddrResolver for LikeLocalhost {
    async fn resolve_addr(&self, hostname: &str) -> Vec<IpAddr> {
        if hostname.eq_ignore_ascii_case(PEER_HOST) {
            vec![
                IpAddr::V6(Ipv6Addr::LOCALHOST),
                IpAddr::V4([127, 0, 0, 1].into()),
            ]
        } else {
            Vec::new()
        }
    }
}

#[async_trait::async_trait]
impl hs_federation::discovery::SrvResolver for LikeLocalhost {
    async fn lookup_srv(&self, _service: &str, _hostname: &str) -> Vec<(String, u16)> {
        Vec::new()
    }
}

struct Server {
    handle: hs_cli::serve::ServeHandle,
    base: String,
    _dir: tempfile::TempDir,
}

async fn start(server_name: &str, bind: &str, port: u16, extra: &str) -> Server {
    let dir = tempfile::tempdir().unwrap();
    let resolver = Arc::new(LikeLocalhost);
    let handle = hs_cli::serve::spawn_serve(
        config(server_name, bind, port, dir.path(), extra),
        hs_cli::serve::ServeOptions {
            federation_scheme: Some("http"),
            federation_resolvers: Some((resolver.clone(), resolver)),
            ..Default::default()
        },
    )
    .await
    .expect("the server boots");
    Server {
        base: handle.base_url(),
        handle,
        _dir: dir,
    }
}

/// Registers `username` through the real UIA dance and returns `(user_id, access_token)`.
async fn register(client: &reqwest::Client, base: &str, username: &str) -> (String, String) {
    let first: Value = client
        .post(format!("{base}/_matrix/client/v3/register"))
        .json(&json!({"username": username, "password": "correct horse"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let session = first["session"]
        .as_str()
        .unwrap_or_else(|| panic!("registration did not offer a UIA session: {first}"));
    let done: Value = client
        .post(format!("{base}/_matrix/client/v3/register"))
        .json(&json!({
            "username": username,
            "password": "correct horse",
            "auth": {"type": "m.login.dummy", "session": session},
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    (
        done["user_id"]
            .as_str()
            .unwrap_or_else(|| panic!("registration failed: {done}"))
            .to_owned(),
        done["access_token"].as_str().unwrap().to_owned(),
    )
}

/// Alice's profile asked through `asker`'s client API, which asks over federation.
async fn ask_profile(client: &reqwest::Client, asker: &Server, alice: &str) -> (u16, Value) {
    let response = client
        .get(format!("{}/_matrix/client/v3/profile/{alice}", asker.base))
        .send()
        .await
        .unwrap();
    let status = response.status().as_u16();
    (status, response.json().await.unwrap_or(Value::Null))
}

/// Every `WARN` line logged so far, without ANSI colours.
#[derive(Clone, Default)]
struct Captured(Arc<Mutex<Vec<u8>>>);

impl Captured {
    fn warnings(&self) -> Vec<String> {
        let bytes = self.0.lock().unwrap().clone();
        String::from_utf8_lossy(&bytes)
            .lines()
            .filter(|line| line.contains("WARN"))
            .map(str::to_owned)
            .collect()
    }
}

impl std::io::Write for Captured {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        // Echo, so a failing run shows what the server said.
        std::io::stderr().write_all(buf)?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_peer_on_ipv6_alone_under_the_ipv4_only_policy_is_one_clear_warning() {
    let Some(port_a) = reserve_v6_port() else {
        eprintln!("SKIP: this host has no IPv6 loopback ([::1] cannot be bound)");
        return;
    };
    let captured = Captured::default();
    let writer = captured.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::WARN)
        .with_ansi(false)
        .with_writer(move || writer.clone())
        .finish();
    tracing::subscriber::set_global_default(subscriber).expect("the only subscriber");

    // The peer: listens on [::1] only, named by a host that resolves to both families.
    let a = start(&format!("{PEER_HOST}:{port_a}"), "::1", port_a, "").await;
    assert!(
        a.base.contains("[::1]"),
        "the peer listens on IPv6: {}",
        a.base
    );
    let client = reqwest::Client::new();
    let (alice, alice_token) = register(&client, &a.base, "alice").await;
    let put = client
        .put(format!(
            "{}/_matrix/client/v3/profile/{alice}/displayname",
            a.base
        ))
        .bearer_auth(&alice_token)
        .json(&json!({"displayname": "Only on IPv6"}))
        .send()
        .await
        .unwrap();
    assert_eq!(put.status(), 200);

    // 1. The default policy, IPv4 only: the asker connects to 127.0.0.1, which is refused.
    let port_b = reserve_v4_port();
    let b = start(&format!("127.0.0.1:{port_b}"), "127.0.0.1", port_b, "").await;
    assert!(hs_http::outbound::ipv4_only(), "the default is IPv4 only");
    let (status, body) = ask_profile(&client, &b, &alice).await;
    assert_ne!(
        status, 200,
        "IPv4 only cannot reach a peer on IPv6 alone: {body}"
    );
    // Asked again inside the warning interval: still one warning.
    let _ = ask_profile(&client, &b, &alice).await;

    let about_ipv6: Vec<String> = captured
        .warnings()
        .into_iter()
        .filter(|line| line.contains("were not tried"))
        .collect();
    assert_eq!(
        about_ipv6.len(),
        1,
        "exactly one warning about the IPv6 address not tried: {about_ipv6:#?}"
    );
    let warning = &about_ipv6[0];
    let v6_addr = SocketAddr::new(IpAddr::V6(Ipv6Addr::LOCALHOST), port_a).to_string();
    let v4_addr = format!("127.0.0.1:{port_a}");
    for needle in [
        PEER_HOST,
        "IPv4",
        "IPv6",
        "network.outbound.ipv4_only",
        v6_addr.as_str(),
        v4_addr.as_str(),
    ] {
        assert!(
            warning.contains(needle),
            "the warning names {needle:?}: {warning}"
        );
    }

    // 2. What the warning says to do: a server with IPv6 on reaches the peer.
    let port_c = reserve_v4_port();
    let c = start(
        &format!("127.0.0.1:{port_c}"),
        "127.0.0.1",
        port_c,
        "network:\n  outbound:\n    ipv4_only: false\n",
    )
    .await;
    assert!(!hs_http::outbound::ipv4_only(), "IPv6 on at boot");
    let (status, profile) = ask_profile(&client, &c, &alice).await;
    assert_eq!(status, 200, "IPv6 on reaches the peer: {profile}");
    assert_eq!(profile, json!({"displayname": "Only on IPv6"}));

    hs_http::outbound::set_ipv4_only(true);
    c.handle.shutdown().await;
    b.handle.shutdown().await;
    a.handle.shutdown().await;
}
