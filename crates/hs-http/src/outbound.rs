//! The outbound address policy: which address families this server connects to other hosts
//! over, and what it does when a name resolves to several addresses.
//!
//! # Why this exists
//!
//! On a Kubernetes cluster whose pods have no IPv6 route, `getaddrinfo` and the federation
//! resolver still return a dual-stack host's AAAA record, and a client that connects to the
//! first address it was given fails with `Network unreachable` without ever trying the A
//! record. That is how remote media from `maunium.net` (delegated to `federation.mau.chat`, A
//! and AAAA) failed three times in three hours on the demo cluster on 2026-10-02. Two things
//! were wrong: the federation client and the URL previewer pinned their connections to the one
//! address they resolved first (`reqwest::ClientBuilder::resolve` with a single `SocketAddr`),
//! so hyper's fall-back across addresses had nothing to fall back to; and nothing let an
//! operator say "this host has no IPv6".
//!
//! # What it does
//!
//! - **The policy** ([`set_ipv4_only`], [`ipv4_only`]) is process-wide, like the network the
//!   process is on. It is `network.outbound.ipv4_only` in the configuration, on by default: a
//!   name's IPv6 addresses are dropped before any connection is made. It is read when a name
//!   is resolved, which is per new connection, so a change applies to the running server at
//!   once; connections already open are kept.
//! - **Every address reaches the connector.** [`Resolver`] is the `reqwest` DNS resolver every
//!   client this crate builds uses ([`crate::client::builder`] and
//!   [`crate::client::pinned_builder`]): it resolves through the operating system
//!   (`tokio::net::lookup_host`, the same `getaddrinfo` reqwest's default resolver uses) or
//!   through the addresses a caller pinned for a host, applies the policy, and hands hyper the
//!   whole list in the order it came. hyper-util's connector then does Happy Eyeballs (RFC
//!   8305): it tries the first address's family in order, and starts the other family after
//!   300 ms or at once when the first family fails -- so a dead IPv6 address costs one failed
//!   connect, never the request. Nothing here reimplements that; the fix is giving it every
//!   address.
//! - **A fall-back is visible.** [`ObserveLayer`] wraps the connector. On every connection it
//!   learns which address connected (hyper-util's `HttpInfo`) and which addresses the resolver
//!   offered, so it can log, at `debug`, each address that was passed over and its family, and
//!   count it in `hs_outbound_connect_failures_total{family}`; a connection that fails
//!   altogether counts every address it tried. `hs_outbound_connections_total{family}` counts
//!   what did connect. An operator reading `/metrics` sees "every failure is IPv6" at a glance.
//!
//! # What it cannot see
//!
//! The connector tries addresses inside hyper-util; this crate sees the outcome, not each
//! attempt. When the first address's family is slower than the 300 ms fall-back delay rather
//! than dead, the other family can win the race and the slow address is counted as passed over
//! all the same. A URL whose host is an IP literal never reaches the resolver (hyper parses
//! literals itself), so the policy does not apply to it: an operator who names an IPv6 literal
//! asked for IPv6.
//!
//! # Tests and the process-wide flag
//!
//! Tests that change the policy share it with every other test in the same binary; such tests
//! run their cases in sequence inside one test function and restore the default.

use std::collections::HashMap;
use std::future::Future;
use std::net::{IpAddr, SocketAddr};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use std::task::{Context, Poll};

use hyper_util::client::legacy::connect::{Connection, HttpInfo};
use prometheus_client::encoding::EncodeLabelSet;
use prometheus_client::metrics::counter::Counter;
use prometheus_client::metrics::family::Family;
use prometheus_client::registry::Registry;
use reqwest::dns::{Addrs, Name, Resolve, Resolving};
use tower::{Layer, Service};

/// `network.outbound.ipv4_only`: on by default, as the configuration's default is.
static IPV4_ONLY: AtomicBool = AtomicBool::new(true);

/// Sets whether outbound connections use IPv4 only (`true`) or every address a name resolves
/// to (`false`). Read per new connection; open connections are kept.
pub fn set_ipv4_only(ipv4_only: bool) {
    IPV4_ONLY.store(ipv4_only, Ordering::Relaxed);
}

/// Whether outbound connections use IPv4 only. `true` until [`set_ipv4_only`] says otherwise.
#[must_use]
pub fn ipv4_only() -> bool {
    IPV4_ONLY.load(Ordering::Relaxed)
}

/// The policy in the words the startup log uses: `IPv4 only` or `IPv4 and IPv6`.
#[must_use]
pub fn describe() -> &'static str {
    if ipv4_only() {
        "IPv4 only"
    } else {
        "IPv4 and IPv6"
    }
}

/// The address family label of `addr`: `ipv4` or `ipv6`.
#[must_use]
pub fn family(addr: IpAddr) -> &'static str {
    match addr {
        IpAddr::V4(_) => "ipv4",
        IpAddr::V6(_) => "ipv6",
    }
}

/// Why a name yielded no address to connect to.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ResolveError {
    /// The name has addresses, all IPv6, and the policy is IPv4 only.
    #[error(
        "`{name}` resolves only to IPv6 addresses ({count}) and this server connects over IPv4 \
         only (`network.outbound.ipv4_only`); turn that off on a host with working IPv6"
    )]
    OnlyIpv6 {
        /// The name resolved.
        name: String,
        /// How many IPv6 addresses were dropped.
        count: usize,
    },
    /// The name resolved to nothing at all.
    #[error("`{name}` resolves to no address")]
    NoAddress {
        /// The name resolved.
        name: String,
    },
}

/// Applies the policy to the addresses `name` resolved to: drops duplicates, drops IPv6 when
/// the policy is IPv4 only, and keeps the resolver's order otherwise (hyper prefers the first
/// address's family and falls back to the other).
///
/// # Errors
/// [`ResolveError::OnlyIpv6`] when the policy left nothing of a name that had only IPv6
/// addresses; [`ResolveError::NoAddress`] when there was nothing to begin with.
pub fn select(
    name: &str,
    addrs: impl IntoIterator<Item = SocketAddr>,
) -> Result<Vec<SocketAddr>, ResolveError> {
    let mut all: Vec<SocketAddr> = Vec::new();
    for addr in addrs {
        if !all.contains(&addr) {
            all.push(addr);
        }
    }
    if all.is_empty() {
        return Err(ResolveError::NoAddress {
            name: name.to_owned(),
        });
    }
    if !ipv4_only() {
        return Ok(all);
    }
    let kept: Vec<SocketAddr> = all.iter().copied().filter(SocketAddr::is_ipv4).collect();
    if kept.is_empty() {
        return Err(ResolveError::OnlyIpv6 {
            name: name.to_owned(),
            count: all.len(),
        });
    }
    Ok(kept)
}

/// The `reqwest` DNS resolver of every outbound client: the operating system's resolver, or a
/// pinned address list per host, with the policy applied. See the module doc.
#[derive(Clone, Debug, Default)]
pub struct Resolver {
    /// Hosts whose addresses a caller resolved and checked itself (the federation client after
    /// server discovery, the URL previewer after its blocklist check), lower-cased. A pinned
    /// host never goes to the operating system's resolver.
    pins: Arc<HashMap<String, Vec<SocketAddr>>>,
}

impl Resolver {
    /// A resolver that pins `host` to `addrs` (every one of them, in this order) and resolves
    /// every other host through the operating system.
    #[must_use]
    pub fn pinned(host: &str, addrs: &[SocketAddr]) -> Self {
        let mut pins = HashMap::new();
        pins.insert(host.to_ascii_lowercase(), addrs.to_vec());
        Self {
            pins: Arc::new(pins),
        }
    }

    /// The addresses `host` connects to under the policy, from the pins or the operating
    /// system, with port `0` standing for "the URL's port" on a system-resolved address.
    ///
    /// # Errors
    /// The operating system's lookup error, or a [`ResolveError`].
    pub async fn candidates(
        &self,
        host: &str,
    ) -> Result<Vec<SocketAddr>, Box<dyn std::error::Error + Send + Sync>> {
        let raw: Vec<SocketAddr> = match self.pins.get(&host.to_ascii_lowercase()) {
            Some(pinned) => pinned.clone(),
            None => tokio::net::lookup_host((host, 0)).await?.collect(),
        };
        Ok(select(host, raw)?)
    }
}

impl Resolve for Resolver {
    fn resolve(&self, name: Name) -> Resolving {
        let resolver = self.clone();
        Box::pin(async move {
            let chosen = resolver.candidates(name.as_str()).await?;
            Attempt::record(&chosen);
            Ok(Box::new(chosen.into_iter()) as Addrs)
        })
    }
}

tokio::task_local! {
    /// The addresses the resolver offered for the connection being made, written by
    /// [`Resolver::resolve`] and read by [`Observe`] once the connector is done. Set only while
    /// an [`Observe`] call runs; a resolver used elsewhere records nothing.
    static ATTEMPT: Arc<Mutex<Vec<SocketAddr>>>;
}

/// The record of one connection's candidates, kept in [`ATTEMPT`].
struct Attempt;

impl Attempt {
    fn record(candidates: &[SocketAddr]) {
        let _ = ATTEMPT.try_with(|slot| {
            if let Ok(mut guard) = slot.lock() {
                *guard = candidates.to_vec();
            }
        });
    }
}

/// Labels of the outbound connection counters.
#[derive(Debug, Clone, PartialEq, Eq, Hash, EncodeLabelSet)]
struct FamilyLabels {
    family: &'static str,
}

/// `hs_outbound_connections_total{family}`: connections made, by the family that connected.
static CONNECTIONS: LazyLock<Family<FamilyLabels, Counter>> = LazyLock::new(Family::default);

/// `hs_outbound_connect_failures_total{family}`: addresses that did not connect, by family.
static FAILURES: LazyLock<Family<FamilyLabels, Counter>> = LazyLock::new(Family::default);

/// Registers the outbound connection counters into `registry` (process-wide counters, like
/// the configuration reload counters: a connection is made far from any registry).
pub fn register_metrics(registry: &mut Registry) {
    // Registered without `_total`: the text encoder appends it.
    registry.register(
        "hs_outbound_connections",
        "Outbound connections this server made, by the address family that connected",
        CONNECTIONS.clone(),
    );
    registry.register(
        "hs_outbound_connect_failures",
        "Addresses of other hosts that did not connect, by family: passed over for another \
         address that did (a fall-back), or failed with every other address of the host",
        FAILURES.clone(),
    );
}

/// The number of connections made over `family` (`ipv4` or `ipv6`) so far in this process.
#[must_use]
pub fn connections(family: &'static str) -> u64 {
    CONNECTIONS.get_or_create(&FamilyLabels { family }).get()
}

/// The number of addresses of `family` (`ipv4` or `ipv6`) that did not connect so far in this
/// process.
#[must_use]
pub fn connect_failures(family: &'static str) -> u64 {
    FAILURES.get_or_create(&FamilyLabels { family }).get()
}

/// A `tower::Layer` for `reqwest::ClientBuilder::connector_layer` that observes each
/// connection the connector makes: see the module doc.
#[derive(Clone, Copy, Debug, Default)]
pub struct ObserveLayer;

impl<S> Layer<S> for ObserveLayer {
    type Service = Observe<S>;

    fn layer(&self, inner: S) -> Self::Service {
        Observe { inner }
    }
}

/// The connector service [`ObserveLayer`] wraps around reqwest's.
#[derive(Clone, Debug)]
pub struct Observe<S> {
    inner: S,
}

type BoxFuture<T> = Pin<Box<dyn Future<Output = T> + Send>>;

impl<S, Req> Service<Req> for Observe<S>
where
    S: Service<Req>,
    S::Future: Send + 'static,
    S::Response: Connection + Send + 'static,
    S::Error: AsRef<dyn std::error::Error + Send + Sync + 'static> + Send + 'static,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = BoxFuture<Result<S::Response, S::Error>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: Req) -> Self::Future {
        let candidates = Arc::new(Mutex::new(Vec::new()));
        let connecting = ATTEMPT.scope(candidates.clone(), self.inner.call(req));
        Box::pin(async move {
            let result = connecting.await;
            let candidates = candidates
                .lock()
                .map(|guard| guard.clone())
                .unwrap_or_default();
            match &result {
                Ok(conn) => note_connected(remote_addr(conn), &candidates),
                Err(error) => note_failed(error.as_ref(), &candidates),
            }
            result
        })
    }
}

/// The address a connection was made to, from hyper-util's `HttpInfo` extra, which reqwest's
/// connection carries through TLS (it is what `reqwest::Response::remote_addr` reads).
fn remote_addr(conn: &impl Connection) -> Option<SocketAddr> {
    let mut extensions = http::Extensions::new();
    conn.connected().get_extras(&mut extensions);
    extensions.get::<HttpInfo>().map(HttpInfo::remote_addr)
}

/// The order hyper-util tries `candidates` in: the first address's family first, in order,
/// then the other family, in order (its Happy Eyeballs split).
fn attempt_order(candidates: &[SocketAddr]) -> Vec<SocketAddr> {
    let Some(first) = candidates.first() else {
        return Vec::new();
    };
    let preferred_v6 = first.is_ipv6();
    let (preferred, fallback): (Vec<SocketAddr>, Vec<SocketAddr>) = candidates
        .iter()
        .copied()
        .partition(|addr| addr.is_ipv6() == preferred_v6);
    preferred.into_iter().chain(fallback).collect()
}

fn note_connected(remote: Option<SocketAddr>, candidates: &[SocketAddr]) {
    let Some(remote) = remote else {
        return;
    };
    CONNECTIONS
        .get_or_create(&FamilyLabels {
            family: family(remote.ip()),
        })
        .inc();
    let order = attempt_order(candidates);
    let Some(position) = order.iter().position(|addr| addr.ip() == remote.ip()) else {
        return;
    };
    for passed_over in &order[..position] {
        FAILURES
            .get_or_create(&FamilyLabels {
                family: family(passed_over.ip()),
            })
            .inc();
        tracing::debug!(
            address = %passed_over,
            family = family(passed_over.ip()),
            connected = %remote,
            connected_family = family(remote.ip()),
            "an outbound address did not connect; the next one did"
        );
    }
}

fn note_failed(error: &(dyn std::error::Error + Send + Sync + 'static), candidates: &[SocketAddr]) {
    if candidates.is_empty() || !is_connect_failure(error) {
        return;
    }
    for addr in attempt_order(candidates) {
        FAILURES
            .get_or_create(&FamilyLabels {
                family: family(addr.ip()),
            })
            .inc();
        tracing::debug!(
            address = %addr,
            family = family(addr.ip()),
            %error,
            "an outbound address did not connect, and no other address of the host did"
        );
    }
}

/// Whether `error` is the connector failing to reach any address (hyper-util's `tcp connect
/// error`, which it returns once every address has failed, or reqwest's connect timeout),
/// rather than something after the connection was made, such as a TLS failure. hyper-util's
/// error type is private, so this reads the chain's messages.
fn is_connect_failure(error: &(dyn std::error::Error + 'static)) -> bool {
    let mut current: Option<&(dyn std::error::Error + 'static)> = Some(error);
    while let Some(err) = current {
        let message = err.to_string();
        if message.starts_with("tcp connect error") || message == "operation timed out" {
            return true;
        }
        current = err.source();
    }
    false
}

/// Installs the policy on `builder`: the [`Resolver`] and the [`ObserveLayer`]. Every outbound
/// client goes through here (`crate::client::builder` does, and the federation client, which
/// builds its own TLS trust, calls it directly).
pub fn configure(builder: reqwest::ClientBuilder, resolver: Resolver) -> reqwest::ClientBuilder {
    builder
        .dns_resolver(Arc::new(resolver))
        .connector_layer(ObserveLayer)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v4(last: u8) -> SocketAddr {
        SocketAddr::from(([10, 0, 0, last], 443))
    }

    fn v6(last: u16) -> SocketAddr {
        SocketAddr::new(
            IpAddr::V6(std::net::Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, last)),
            443,
        )
    }

    /// The policy is process-wide: a test that changes it holds this, runs its cases in
    /// sequence, and restores the default.
    static POLICY: Mutex<()> = Mutex::new(());

    #[test]
    fn select_applies_the_policy() {
        let _serial = POLICY
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let before = ipv4_only();

        set_ipv4_only(true);
        assert_eq!(
            select("h", [v6(1), v4(1), v6(2), v4(2)]),
            Ok(vec![v4(1), v4(2)])
        );
        assert_eq!(
            select("h", [v6(1), v6(2)]),
            Err(ResolveError::OnlyIpv6 {
                name: "h".into(),
                count: 2
            })
        );
        assert_eq!(
            select("h", []),
            Err(ResolveError::NoAddress { name: "h".into() })
        );

        set_ipv4_only(false);
        assert_eq!(
            select("h", [v6(1), v4(1), v6(1), v4(2)]),
            Ok(vec![v6(1), v4(1), v4(2)]),
            "every address, in the resolver's order, without duplicates"
        );

        set_ipv4_only(before);
    }

    #[test]
    fn the_only_ipv6_error_names_the_setting() {
        let error = ResolveError::OnlyIpv6 {
            name: "federation.mau.chat".into(),
            count: 1,
        };
        assert!(error.to_string().contains("network.outbound.ipv4_only"));
    }

    #[test]
    fn attempt_order_is_the_first_family_then_the_other() {
        assert_eq!(
            attempt_order(&[v6(1), v4(1), v6(2), v4(2)]),
            vec![v6(1), v6(2), v4(1), v4(2)]
        );
        assert_eq!(
            attempt_order(&[v4(1), v6(1), v4(2)]),
            vec![v4(1), v4(2), v6(1)]
        );
        assert!(attempt_order(&[]).is_empty());
    }

    #[test]
    fn a_pinned_host_resolves_to_its_pins_and_nothing_else() {
        let _serial = POLICY
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let before = ipv4_only();
        set_ipv4_only(false);
        let resolver = Resolver::pinned("Dual.Example", &[v6(1), v4(1)]);
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let got = runtime
            .block_on(resolver.candidates("dual.example"))
            .unwrap();
        assert_eq!(got, vec![v6(1), v4(1)]);
        set_ipv4_only(true);
        let got = runtime
            .block_on(resolver.candidates("DUAL.EXAMPLE"))
            .unwrap();
        assert_eq!(got, vec![v4(1)], "the pins go through the policy too");
        set_ipv4_only(before);
    }

    #[test]
    fn the_system_resolver_answers_localhost() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let got = runtime
            .block_on(Resolver::default().candidates("localhost"))
            .unwrap();
        assert!(!got.is_empty());
        assert!(
            got.iter().all(|addr| addr.port() == 0),
            "port 0 stands for the URL's port"
        );
    }

    #[test]
    fn connect_failures_are_told_from_other_errors() {
        let io = std::io::Error::new(std::io::ErrorKind::ConnectionRefused, "refused");
        let connect: Box<dyn std::error::Error + Send + Sync> =
            Box::new(Wrapped("tcp connect error", Some(io)));
        assert!(is_connect_failure(connect.as_ref()));
        let timeout: Box<dyn std::error::Error + Send + Sync> =
            Box::new(Wrapped("operation timed out", None));
        assert!(is_connect_failure(timeout.as_ref()));
        let tls: Box<dyn std::error::Error + Send + Sync> =
            Box::new(Wrapped("invalid peer certificate: UnknownIssuer", None));
        assert!(!is_connect_failure(tls.as_ref()));
    }

    #[derive(Debug)]
    struct Wrapped(&'static str, Option<std::io::Error>);

    impl std::fmt::Display for Wrapped {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str(self.0)
        }
    }

    impl std::error::Error for Wrapped {
        fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
            self.1
                .as_ref()
                .map(|e| e as &(dyn std::error::Error + 'static))
        }
    }
}
