//! The outbound address policy against a real connector: a client pinned to a dual-stack host
//! whose IPv6 address is `2001:db8::1` (the documentation prefix, routed nowhere) and whose
//! IPv4 address is a listener on this machine. Under the default policy the request never
//! touches IPv6; with IPv6 on, the connector falls back and the request still succeeds, and the
//! fall-back is counted. The policy is process-wide, so the cases run in sequence in one test.

use std::net::{IpAddr, Ipv6Addr, SocketAddr};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use tokio::io::{AsyncReadExt, AsyncWriteExt};

const HOST: &str = "dual-stack.test";

/// A listener on 127.0.0.1 that answers every HTTP request with `ok` and counts the connections
/// it accepted.
async fn ipv4_server() -> (SocketAddr, Arc<AtomicUsize>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let accepted = Arc::new(AtomicUsize::new(0));
    let counter = accepted.clone();
    tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                return;
            };
            counter.fetch_add(1, Ordering::SeqCst);
            tokio::spawn(async move {
                let mut buf = [0u8; 1024];
                let _ = stream.read(&mut buf).await;
                let _ = stream
                    .write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok",
                    )
                    .await;
            });
        }
    });
    (addr, accepted)
}

/// Every message in `error`'s source chain, joined: what an operator reads in the log.
fn chain(error: &dyn std::error::Error) -> String {
    let mut messages = vec![error.to_string()];
    let mut current = error.source();
    while let Some(source) = current {
        messages.push(source.to_string());
        current = source.source();
    }
    messages.join(": ")
}

fn dead_v6(port: u16) -> SocketAddr {
    SocketAddr::new(
        IpAddr::V6(Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 1)),
        port,
    )
}

#[tokio::test(flavor = "multi_thread")]
async fn a_dual_stack_host_with_a_dead_ipv6_address_is_reached_under_either_policy() {
    let (v4, accepted) = ipv4_server().await;
    let v6 = dead_v6(v4.port());
    let url = format!("http://{HOST}:{}/", v4.port());

    // 1. The default: IPv4 only. The IPv6 address is never tried.
    hs_http::outbound::set_ipv4_only(true);
    let v6_failures_before = hs_http::outbound::connect_failures("ipv6");
    let v4_connections_before = hs_http::outbound::connections("ipv4");
    let client = hs_http::client::pinned_builder(HOST, &[v6, v4])
        .timeout(Duration::from_secs(10))
        .build()
        .unwrap();
    let started = Instant::now();
    let response = client.get(&url).send().await.unwrap();
    assert_eq!(response.remote_addr(), Some(v4));
    assert_eq!(response.text().await.unwrap(), "ok");
    assert!(started.elapsed() < Duration::from_secs(5));
    assert_eq!(accepted.load(Ordering::SeqCst), 1);
    assert_eq!(
        hs_http::outbound::connect_failures("ipv6"),
        v6_failures_before,
        "IPv4 only: no IPv6 attempt, so nothing to fail"
    );
    assert_eq!(
        hs_http::outbound::connections("ipv4"),
        v4_connections_before + 1
    );

    // 2. IPv6 on: the IPv6 address is tried first and does not connect; the connector falls
    //    back to IPv4 within one attempt's time, and the fall-back is counted.
    hs_http::outbound::set_ipv4_only(false);
    let v6_failures_before = hs_http::outbound::connect_failures("ipv6");
    let v4_connections_before = hs_http::outbound::connections("ipv4");
    let client = hs_http::client::pinned_builder(HOST, &[v6, v4])
        .timeout(Duration::from_secs(10))
        .build()
        .unwrap();
    let started = Instant::now();
    let response = client.get(&url).send().await.unwrap();
    assert_eq!(response.remote_addr(), Some(v4));
    assert_eq!(response.text().await.unwrap(), "ok");
    // Instant on a host with no IPv6 route (`Network unreachable`); hyper-util's 300 ms
    // fall-back delay on a host whose IPv6 route swallows the packets.
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "the fall-back took {:?}",
        started.elapsed()
    );
    assert_eq!(accepted.load(Ordering::SeqCst), 2);
    assert_eq!(
        hs_http::outbound::connect_failures("ipv6"),
        v6_failures_before + 1,
        "the IPv6 address that was passed over is counted"
    );
    assert_eq!(
        hs_http::outbound::connections("ipv4"),
        v4_connections_before + 1
    );

    // 3. IPv4 only, and the host has only an IPv6 address: the error names the setting.
    hs_http::outbound::set_ipv4_only(true);
    let client = hs_http::client::pinned_builder(HOST, &[v6])
        .timeout(Duration::from_secs(10))
        .build()
        .unwrap();
    let error = client.get(&url).send().await.unwrap_err();
    let message = chain(&error);
    assert!(
        message.contains("network.outbound.ipv4_only"),
        "the error should name the setting: {message}"
    );
    assert_eq!(accepted.load(Ordering::SeqCst), 2);

    // 4. IPv6 on, every address dead: the request fails and every address is counted.
    hs_http::outbound::set_ipv4_only(false);
    let closed = {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.local_addr().unwrap()
    };
    let v6_failures_before = hs_http::outbound::connect_failures("ipv6");
    let v4_failures_before = hs_http::outbound::connect_failures("ipv4");
    let client = hs_http::client::pinned_builder(HOST, &[v6, closed])
        .connect_timeout(Duration::from_secs(2))
        .timeout(Duration::from_secs(10))
        .build()
        .unwrap();
    let error = client
        .get(format!("http://{HOST}:{}/", closed.port()))
        .send()
        .await
        .unwrap_err();
    assert!(error.is_connect() || error.is_timeout(), "{error:?}");
    assert_eq!(
        hs_http::outbound::connect_failures("ipv6"),
        v6_failures_before + 1
    );
    assert_eq!(
        hs_http::outbound::connect_failures("ipv4"),
        v4_failures_before + 1
    );

    hs_http::outbound::set_ipv4_only(true);
}
