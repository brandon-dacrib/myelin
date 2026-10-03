//! The outbound address policy (`network.outbound.ipv4_only`) on the real server: two `hs
//! serve` instances in one process, the first named `dual-stack.test:{port}`, which the second
//! resolves -- through `ServeOptions::federation_resolvers`, the one seam a test may use -- to
//! `2001:db8::1` (the documentation prefix, routed nowhere) and `127.0.0.1`, in that order, the
//! way a dual-stack server's AAAA record came first for `federation.mau.chat` on the demo
//! cluster. Under the default policy the second server never touches IPv6; with IPv6 on, it
//! tries the dead address, falls back, and the profile query still succeeds -- and `/metrics`
//! shows the fall-back. The policy is process-wide, so both cases run in one test.

use std::net::{IpAddr, Ipv6Addr};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

const DUAL_STACK_HOST: &str = "dual-stack.test";

fn reserve_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn config(
    server_name: &str,
    port: u16,
    data_dir: &std::path::Path,
    extra: &str,
) -> hs_config::Config {
    let media_dir = data_dir.join("media");
    let yaml = format!(
        "server:\n  server_name: \"{server_name}\"\n\
         listeners:\n  listeners:\n    - port: {port}\n      bind_addresses: [\"127.0.0.1\"]\n      resources: [client, federation, health, metrics]\n\
         storage:\n  backend: embedded\n  data_dir: {data_dir:?}\n\
         media:\n  storage:\n    backend: local\n    path: {media_dir:?}\n\
         auth:\n  enable_registration: true\n\
         federation:\n  ip_range_blocklist: []\n\
         rate_limits:\n  enabled: false\n\
         {extra}"
    );
    hs_config::Config::from_yaml(&yaml).expect("the test configuration parses")
}

/// Resolves `dual-stack.test` to an unreachable IPv6 address and then this machine; no SRV
/// records anywhere.
struct DualStack;

#[async_trait::async_trait]
impl hs_federation::discovery::AddrResolver for DualStack {
    async fn resolve_addr(&self, hostname: &str) -> Vec<IpAddr> {
        if hostname.eq_ignore_ascii_case(DUAL_STACK_HOST) {
            vec![
                IpAddr::V6(Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 1)),
                IpAddr::V4([127, 0, 0, 1].into()),
            ]
        } else {
            Vec::new()
        }
    }
}

#[async_trait::async_trait]
impl hs_federation::discovery::SrvResolver for DualStack {
    async fn lookup_srv(&self, _service: &str, _hostname: &str) -> Vec<(String, u16)> {
        Vec::new()
    }
}

struct Server {
    handle: hs_cli::serve::ServeHandle,
    base: String,
    _dir: tempfile::TempDir,
}

async fn start(server_name: &str, port: u16, extra: &str) -> Server {
    let dir = tempfile::tempdir().unwrap();
    let resolver = Arc::new(DualStack);
    let handle = hs_cli::serve::spawn_serve(
        config(server_name, port, dir.path(), extra),
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

/// The value of the `/metrics` sample `name{labels}`, or 0 when the series is not there yet.
async fn metric(base: &str, sample: &str) -> f64 {
    let text = reqwest::get(format!("{base}/metrics"))
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    text.lines()
        .find_map(|line| {
            line.strip_prefix(sample)
                .and_then(|rest| rest.trim().parse::<f64>().ok())
        })
        .unwrap_or(0.0)
}

/// Alice's profile on the dual-stack server, asked through `asker`'s client API, which asks
/// over federation: the status and how long it took.
async fn ask_profile(client: &reqwest::Client, asker: &Server, alice: &str) -> (Value, Duration) {
    let started = Instant::now();
    let profile: Value = client
        .get(format!("{}/_matrix/client/v3/profile/{alice}", asker.base))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    (profile, started.elapsed())
}

#[tokio::test]
async fn a_dual_stack_server_with_a_dead_ipv6_address_is_reached_under_either_policy() {
    let port_a = reserve_port();
    let name_a = format!("{DUAL_STACK_HOST}:{port_a}");
    let a = start(&name_a, port_a, "").await;
    let client = reqwest::Client::new();
    let (alice, alice_token) = register(&client, &a.base, "alice").await;
    let put = client
        .put(format!(
            "{}/_matrix/client/v3/profile/{alice}/displayname",
            a.base
        ))
        .bearer_auth(&alice_token)
        .json(&json!({"displayname": "Reached over IPv4"}))
        .send()
        .await
        .unwrap();
    assert_eq!(put.status(), 200);

    // 1. The default: IPv4 only. B never tries the IPv6 address.
    let port_b = reserve_port();
    let b = start(&format!("127.0.0.1:{port_b}"), port_b, "").await;
    assert!(hs_http::outbound::ipv4_only(), "the default is IPv4 only");
    let v6_failures_before = metric(
        &b.base,
        "hs_outbound_connect_failures_total{family=\"ipv6\"}",
    )
    .await;
    let (profile, took) = ask_profile(&client, &b, &alice).await;
    assert_eq!(profile, json!({"displayname": "Reached over IPv4"}));
    assert!(took < Duration::from_secs(10), "took {took:?}");
    assert_eq!(
        metric(
            &b.base,
            "hs_outbound_connect_failures_total{family=\"ipv6\"}"
        )
        .await,
        v6_failures_before,
        "IPv4 only: no IPv6 attempt was made"
    );
    assert!(
        metric(&b.base, "hs_outbound_connections_total{family=\"ipv4\"}").await >= 1.0,
        "the connection to the dual-stack server was made over IPv4"
    );

    // 2. IPv6 on (`network.outbound.ipv4_only: false`): the dead IPv6 address is tried first,
    //    the connector falls back to IPv4, and the query still succeeds.
    let port_c = reserve_port();
    let c = start(
        &format!("127.0.0.1:{port_c}"),
        port_c,
        "network:\n  outbound:\n    ipv4_only: false\n",
    )
    .await;
    assert!(
        !hs_http::outbound::ipv4_only(),
        "the configuration turned IPv6 on at boot"
    );
    let v6_failures_before = metric(
        &c.base,
        "hs_outbound_connect_failures_total{family=\"ipv6\"}",
    )
    .await;
    let v4_connections_before =
        metric(&c.base, "hs_outbound_connections_total{family=\"ipv4\"}").await;
    let (profile, took) = ask_profile(&client, &c, &alice).await;
    assert_eq!(profile, json!({"displayname": "Reached over IPv4"}));
    // Instant on a host with no IPv6 route; hyper-util's 300 ms fall-back delay on one whose
    // route swallows the packets. Either way, well within one request.
    assert!(
        took < Duration::from_secs(10),
        "the fall-back took {took:?}"
    );
    assert!(
        metric(
            &c.base,
            "hs_outbound_connect_failures_total{family=\"ipv6\"}"
        )
        .await
            >= v6_failures_before + 1.0,
        "the IPv6 address that was passed over is counted"
    );
    assert!(
        metric(&c.base, "hs_outbound_connections_total{family=\"ipv4\"}").await
            >= v4_connections_before + 1.0,
        "the fall-back connected over IPv4"
    );

    hs_http::outbound::set_ipv4_only(true);
    c.handle.shutdown().await;
    b.handle.shutdown().await;
    a.handle.shutdown().await;
}
