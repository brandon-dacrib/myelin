//! Server-name resolution: the Matrix server-server API's "Resolving server names" algorithm.
//!
//! Written from `docs/design/06-federation-threat-model.md` section 2.1 and the plan recorded in
//! `docs/status/06-federation.md` (item 2). The resolution *logic* lives behind three small
//! traits ([`WellKnownFetcher`], [`SrvResolver`], [`AddrResolver`]) so it can be unit tested
//! against fakes reproducing the spec's own worked examples, with no real DNS/network involved in
//! tests. [`HttpWellKnownFetcher`] (reqwest-backed) and [`HickorySrvResolver`]/
//! [`HickoryAddrResolver`] (`hickory-resolver`-backed) are the real implementations wired together
//! by [`resolve`] for production use.
//!
//! # The algorithm (spec "Resolving server names", `server-server-api.md`)
//!
//! Given a `server_name` (a hostname, optionally with a port, or an IP literal):
//!
//! 1. If `server_name` is an IP literal (with or without an explicit port), use it directly —
//!    port defaults to 8448 if absent. No well-known fetch, no SRV lookup.
//! 2. If `server_name` is a hostname with an explicit port, resolve the hostname via A/AAAA only
//!    (using the literal port). No well-known fetch, no SRV lookup.
//! 3. If `server_name` is a hostname with no port, fetch
//!    `https://<hostname>/.well-known/matrix/server` (a single GET, no redirects followed — see
//!    the threat model on why). If the body is a valid `{"m.server": "delegated[:port]"}`:
//!    - if the delegated name has an explicit port, resolve it via A/AAAA only (step 2's rule,
//!      applied to the delegated name);
//!    - if the delegated name is an IP literal, use it directly (step 1's rule);
//!    - otherwise, SRV-lookup the delegated hostname (`_matrix-fed._tcp` first, falling back to
//!      the deprecated `_matrix._tcp` if the first returns no records), falling back to A/AAAA on
//!      port 8448 if neither SRV lookup returns records.
//!
//!    If the well-known fetch fails or the body is not validly shaped, fall back to an SRV lookup
//!    on the **original** hostname (same `_matrix-fed._tcp` then `_matrix._tcp` order), falling
//!    back to A/AAAA on port 8448 if neither returns records.
//!
//! Every fetch/lookup step also carries a `Host` header to present on the eventual HTTPS
//! connection (the TLS SNI / certificate-check name), per the spec: it is the *delegated* name
//! for well-known delegation, and the original name otherwise. [`ResolvedServer`] carries this.

use std::net::{IpAddr, SocketAddr};
use std::time::Duration;

use async_trait::async_trait;
use serde::Deserialize;

/// Max bytes read from a `.well-known/matrix/server` response body (threat model section 3: 16
/// KiB). Enforced by the fetcher before any JSON parsing happens.
pub const MAX_WELL_KNOWN_BODY_BYTES: usize = 16 * 1024;

/// Timeout for a single `.well-known`, SRV or A/AAAA lookup (threat model section 3: 10s, tighter
/// than the general federation client timeout since discovery blocks the first real request).
pub const DISCOVERY_TIMEOUT: Duration = Duration::from_secs(10);

/// Minimum well-known cache TTL we will honour from a peer's `Cache-Control: max-age`, even if
/// the peer asks for less. Prevents a hostile/misconfigured peer from forcing us into a refetch
/// storm against ourselves via a tiny `max-age`.
pub const MIN_WELL_KNOWN_CACHE_SECS: u64 = 60; // 1 minute
/// Maximum well-known cache TTL we will honour, even if the peer asks for more (clamps a
/// cache-poisoning attempt to keep a bad delegation alive indefinitely).
pub const MAX_WELL_KNOWN_CACHE_SECS: u64 = 24 * 60 * 60; // 24 hours, matches the spec's own guidance
/// The spec's recommended default when no `Cache-Control` header (or an unparsable one) is
/// present.
pub const DEFAULT_WELL_KNOWN_CACHE_SECS: u64 = 24 * 60 * 60;
/// How long a *failed* well-known fetch is cached negatively, so a hostile or broken peer cannot
/// force a refetch on every single outbound request to it (a self-inflicted DoS lever named in
/// the threat model). Deliberately much shorter than a success TTL.
pub const FAILED_WELL_KNOWN_CACHE_SECS: u64 = 60;

/// The default federation port used whenever discovery falls through to a bare A/AAAA lookup.
pub const DEFAULT_FEDERATION_PORT: u16 = 8448;

/// A server name resolved to a concrete address to connect to, plus the `Host`/SNI name to
/// present on the connection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedServer {
    /// The address (IP or hostname) to open the TCP connection to. A hostname here still needs a
    /// final A/AAAA resolution by the HTTP client; `hs-federation`'s own [`AddrResolver`] step
    /// already validated at least one address exists and is not IP-range-blocked, per the threat
    /// model's SSRF defence — the client re-resolving is expected to land on the same, already
    /// vetted, address set (DNS is assumed stable within one request's lifetime; TOCTOU against a
    /// hostile authoritative resolver is a known, accepted residual risk shared with every other
    /// Matrix implementation).
    pub connect_host: String,
    pub connect_port: u16,
    /// The name to send as the TLS SNI / HTTP `Host` header — the delegated name when well-known
    /// delegation happened, the original server name otherwise.
    pub tls_server_name: String,
    /// How this was resolved, for logging/testing.
    pub via: ResolutionPath,
}

/// Which branch of the algorithm produced a [`ResolvedServer`], recorded so tests can assert the
/// *path* taken, not just the final address (the spec's worked examples are keyed on this).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolutionPath {
    IpLiteral,
    ExplicitPort,
    WellKnownExplicitPort,
    WellKnownIpLiteral,
    WellKnownSrv,
    WellKnownFallbackDirect,
    Srv,
    FallbackDirect,
}

/// Errors from [`resolve`].
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum DiscoveryError {
    #[error("{0} did not resolve to any address")]
    NoAddress(String),
    #[error("resolution loop or excessive delegation depth for {0}")]
    TooManyRedirects(String),
}

/// A parsed `server_name`: either an IP literal (with optional port) or a hostname (with optional
/// port).
#[derive(Debug, Clone, PartialEq, Eq)]
enum ParsedServerName {
    IpLiteral { ip: IpAddr, port: Option<u16> },
    Hostname { host: String, port: Option<u16> },
}

/// Parses a `server_name` per the spec's grammar: `IPv4address[:port]`,
/// `"[" IPv6address "]"[":" port]`, or `hostname[:port]` (a hostname is anything that's not
/// syntactically an IP literal). This does not validate the hostname is a well-formed DNS name
/// beyond "not an IP literal and not empty" — malformed hostnames simply fail to resolve later.
fn parse_server_name(server_name: &str) -> ParsedServerName {
    // IPv6 literal: `[::1]` or `[::1]:8448`.
    if let Some(rest) = server_name.strip_prefix('[') {
        if let Some((addr_part, after)) = rest.split_once(']')
            && let Ok(ip) = addr_part.parse::<IpAddr>()
        {
            let port = after.strip_prefix(':').and_then(|p| p.parse().ok());
            return ParsedServerName::IpLiteral { ip, port };
        }
        // Malformed bracket syntax: treat as a (doomed) hostname rather than panicking.
        return ParsedServerName::Hostname {
            host: server_name.to_string(),
            port: None,
        };
    }

    // Try splitting off a `:port` suffix and see if what's left is an IP literal (handles IPv4
    // with a port, and bare hostnames with a port). A bare, portless string is checked as-is.
    if let Some((host_part, port_part)) = server_name.rsplit_once(':')
        && let Ok(port) = port_part.parse::<u16>()
    {
        if let Ok(ip) = host_part.parse::<IpAddr>() {
            return ParsedServerName::IpLiteral {
                ip,
                port: Some(port),
            };
        }
        return ParsedServerName::Hostname {
            host: host_part.to_string(),
            port: Some(port),
        };
    }

    if let Ok(ip) = server_name.parse::<IpAddr>() {
        return ParsedServerName::IpLiteral { ip, port: None };
    }

    ParsedServerName::Hostname {
        host: server_name.to_string(),
        port: None,
    }
}

/// The `.well-known/matrix/server` document shape.
#[derive(Debug, Clone, Deserialize)]
struct WellKnownServer {
    #[serde(rename = "m.server")]
    m_server: String,
}

/// Parses and structurally validates a `.well-known/matrix/server` response body the same way
/// [`HttpWellKnownFetcher::fetch`] does: valid JSON, an object, a non-empty `m.server` string.
/// Exposed standalone (not just inline in the fetcher) so it is directly fuzzable without needing
/// a real HTTP response to drive it — see `fuzz/fuzz_targets/well_known_body_parse.rs`.
#[must_use]
pub fn parse_well_known_body(bytes: &[u8]) -> Option<String> {
    if bytes.len() > MAX_WELL_KNOWN_BODY_BYTES {
        return None;
    }
    match serde_json::from_slice::<WellKnownServer>(bytes) {
        Ok(doc) if !doc.m_server.trim().is_empty() => Some(doc.m_server),
        _ => None,
    }
}

/// The outcome of a well-known fetch: either a parsed delegation with a cache TTL, or "no
/// delegation" (absent/malformed/error) with a (shorter) negative-cache TTL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WellKnownOutcome {
    Delegated {
        delegated_to: String,
        cache_for: Duration,
    },
    Absent {
        cache_for: Duration,
    },
}

/// Fetches a `.well-known/matrix/server` document for a hostname. Implementations must: issue at
/// most one GET, never follow redirects, cap the body at [`MAX_WELL_KNOWN_BODY_BYTES`], and apply
/// [`DISCOVERY_TIMEOUT`].
#[async_trait]
pub trait WellKnownFetcher: Send + Sync {
    async fn fetch(&self, hostname: &str) -> WellKnownOutcome;
}

/// Looks up SRV records for `_service._tcp.<hostname>`, returning `(target_host, port)` pairs in
/// priority/weight order (weighting not modeled; callers may treat the first entry as sufficient
/// since we only need one working destination). Empty vec means "no records" (NXDOMAIN or empty
/// answer both fold to this — callers fall through the same way either way).
#[async_trait]
pub trait SrvResolver: Send + Sync {
    async fn lookup_srv(&self, service: &str, hostname: &str) -> Vec<(String, u16)>;
}

/// Resolves a hostname to at least one usable address (A/AAAA). Implementations should apply
/// [`DISCOVERY_TIMEOUT`]. Returning an empty vec means "does not resolve".
#[async_trait]
pub trait AddrResolver: Send + Sync {
    async fn resolve_addr(&self, hostname: &str) -> Vec<IpAddr>;
}

/// Runs the full resolution algorithm. `hostname` is used as the connect target string
/// (`ResolvedServer::connect_host`) rather than a pre-resolved IP, because the underlying HTTP
/// client (reqwest) does its own connection-level DNS resolution and connection pooling; the
/// caller (`crate::client`) is responsible for applying the IP-range allow/deny check (threat
/// model 2.1/2.6) to the addresses `addr_resolver` returns here *before* trusting this result for
/// SSRF purposes — [`resolve`] itself does call `addr_resolver` (so callers can inspect
/// [`ResolveOutcome::addresses`]) but does not itself enforce any allow/deny policy, which is
/// `hs-config::FederationConfig`-driven and therefore a client-layer concern, not a discovery-layer
/// one.
#[derive(Debug)]
pub struct ResolveOutcome {
    pub server: ResolvedServer,
    /// The concrete addresses found for `server.connect_host`, for the caller's own IP-range
    /// check. Empty only if `server.connect_host` is itself already an IP literal (in which case
    /// the caller should check `connect_host` directly).
    pub addresses: Vec<IpAddr>,
}

pub async fn resolve(
    server_name: &str,
    well_known: &dyn WellKnownFetcher,
    srv: &dyn SrvResolver,
    addr: &dyn AddrResolver,
) -> Result<ResolveOutcome, DiscoveryError> {
    match parse_server_name(server_name) {
        // Step 1: IP literal.
        ParsedServerName::IpLiteral { ip, port } => Ok(ResolveOutcome {
            server: ResolvedServer {
                connect_host: ip.to_string(),
                connect_port: port.unwrap_or(DEFAULT_FEDERATION_PORT),
                tls_server_name: server_name.to_string(),
                via: ResolutionPath::IpLiteral,
            },
            addresses: vec![ip],
        }),

        // Step 2: hostname with explicit port.
        ParsedServerName::Hostname {
            host,
            port: Some(port),
        } => {
            let addrs = addr.resolve_addr(&host).await;
            if addrs.is_empty() {
                return Err(DiscoveryError::NoAddress(host));
            }
            Ok(ResolveOutcome {
                server: ResolvedServer {
                    connect_host: host.clone(),
                    connect_port: port,
                    tls_server_name: host,
                    via: ResolutionPath::ExplicitPort,
                },
                addresses: addrs,
            })
        }

        // Step 3: hostname, no port — well-known, then SRV, then direct A/AAAA.
        ParsedServerName::Hostname { host, port: None } => {
            resolve_via_well_known_then_srv(&host, well_known, srv, addr).await
        }
    }
}

async fn resolve_via_well_known_then_srv(
    original_host: &str,
    well_known: &dyn WellKnownFetcher,
    srv: &dyn SrvResolver,
    addr: &dyn AddrResolver,
) -> Result<ResolveOutcome, DiscoveryError> {
    match well_known.fetch(original_host).await {
        WellKnownOutcome::Delegated { delegated_to, .. } => {
            // A well-known fetch only ever recurses one level (no re-fetching well-known for the
            // delegated name — the spec's algorithm is a single fetch); apply steps 1/2/SRV to
            // `delegated_to` directly.
            match parse_server_name(&delegated_to) {
                ParsedServerName::IpLiteral { ip, port } => Ok(ResolveOutcome {
                    server: ResolvedServer {
                        connect_host: ip.to_string(),
                        connect_port: port.unwrap_or(DEFAULT_FEDERATION_PORT),
                        tls_server_name: delegated_to,
                        via: ResolutionPath::WellKnownIpLiteral,
                    },
                    addresses: vec![ip],
                }),
                ParsedServerName::Hostname {
                    host,
                    port: Some(port),
                } => {
                    let addrs = addr.resolve_addr(&host).await;
                    if addrs.is_empty() {
                        return Err(DiscoveryError::NoAddress(host));
                    }
                    Ok(ResolveOutcome {
                        server: ResolvedServer {
                            connect_host: host.clone(),
                            connect_port: port,
                            tls_server_name: host,
                            via: ResolutionPath::WellKnownExplicitPort,
                        },
                        addresses: addrs,
                    })
                }
                ParsedServerName::Hostname { host, port: None } => {
                    srv_then_direct(&host, srv, addr, ResolutionPath::WellKnownSrv, host.clone())
                        .await
                }
            }
        }
        WellKnownOutcome::Absent { .. } => {
            srv_then_direct(
                original_host,
                srv,
                addr,
                ResolutionPath::Srv,
                original_host.to_string(),
            )
            .await
        }
    }
}

/// SRV lookup (`_matrix-fed._tcp` then the deprecated `_matrix._tcp`) falling back to a direct
/// A/AAAA lookup on [`DEFAULT_FEDERATION_PORT`] if neither returns records.
async fn srv_then_direct(
    host: &str,
    srv: &dyn SrvResolver,
    addr: &dyn AddrResolver,
    srv_path: ResolutionPath,
    tls_server_name: String,
) -> Result<ResolveOutcome, DiscoveryError> {
    let mut records = srv.lookup_srv("_matrix-fed._tcp", host).await;
    if records.is_empty() {
        records = srv.lookup_srv("_matrix._tcp", host).await;
    }
    if let Some((target, port)) = records.into_iter().next() {
        let addrs = addr.resolve_addr(&target).await;
        if addrs.is_empty() {
            return Err(DiscoveryError::NoAddress(target));
        }
        return Ok(ResolveOutcome {
            server: ResolvedServer {
                connect_host: target,
                connect_port: port,
                tls_server_name,
                via: srv_path,
            },
            addresses: addrs,
        });
    }

    let addrs = addr.resolve_addr(host).await;
    if addrs.is_empty() {
        return Err(DiscoveryError::NoAddress(host.to_string()));
    }
    let via = match srv_path {
        ResolutionPath::WellKnownSrv => ResolutionPath::WellKnownFallbackDirect,
        _ => ResolutionPath::FallbackDirect,
    };
    Ok(ResolveOutcome {
        server: ResolvedServer {
            connect_host: host.to_string(),
            connect_port: DEFAULT_FEDERATION_PORT,
            tls_server_name,
            via,
        },
        addresses: addrs,
    })
}

/// Clamps a `Cache-Control: max-age=N` value (or the default, if absent/unparsable) into
/// `[MIN_WELL_KNOWN_CACHE_SECS, MAX_WELL_KNOWN_CACHE_SECS]`. This is the cache-poisoning defence
/// named in the threat model: a hostile `.well-known` host cannot force an arbitrarily long or
/// short cache lifetime.
#[must_use]
pub fn clamp_cache_control(max_age_secs: Option<u64>) -> Duration {
    let secs = max_age_secs.unwrap_or(DEFAULT_WELL_KNOWN_CACHE_SECS);
    Duration::from_secs(secs.clamp(MIN_WELL_KNOWN_CACHE_SECS, MAX_WELL_KNOWN_CACHE_SECS))
}

/// Parses the `max-age` directive out of a `Cache-Control` header value, if present and
/// well-formed.
#[must_use]
pub fn parse_max_age(cache_control: &str) -> Option<u64> {
    cache_control.split(',').find_map(|directive| {
        let directive = directive.trim();
        let rest = directive.strip_prefix("max-age=")?;
        rest.parse::<u64>().ok()
    })
}

/// Wraps a [`WellKnownFetcher`] with an in-memory cache honouring the outcome's own `cache_for`
/// (already clamped by [`clamp_cache_control`] for a real fetcher), so repeated resolutions of
/// the same hostname within its cache lifetime do not re-fetch. Per the threat model: a negative
/// (failed/absent) result is cached too, for [`FAILED_WELL_KNOWN_CACHE_SECS`] — this is what
/// prevents a hostile or broken `.well-known` host from forcing a refetch on every single
/// outbound request to it.
pub struct CachingWellKnownFetcher<F: WellKnownFetcher> {
    inner: F,
    cache:
        std::sync::Mutex<std::collections::HashMap<String, (WellKnownOutcome, std::time::Instant)>>,
}

impl<F: WellKnownFetcher> CachingWellKnownFetcher<F> {
    #[must_use]
    pub fn new(inner: F) -> Self {
        Self {
            inner,
            cache: std::sync::Mutex::new(std::collections::HashMap::new()),
        }
    }

    fn cache_for(outcome: &WellKnownOutcome) -> Duration {
        match outcome {
            WellKnownOutcome::Delegated { cache_for, .. }
            | WellKnownOutcome::Absent { cache_for } => *cache_for,
        }
    }
}

#[async_trait]
impl<F: WellKnownFetcher> WellKnownFetcher for CachingWellKnownFetcher<F> {
    async fn fetch(&self, hostname: &str) -> WellKnownOutcome {
        if let Some((outcome, fetched_at)) = self.cache.lock().unwrap().get(hostname).cloned() {
            let ttl = Self::cache_for(&outcome);
            if fetched_at.elapsed() < ttl {
                return outcome;
            }
        }
        let outcome = self.inner.fetch(hostname).await;
        self.cache.lock().unwrap().insert(
            hostname.to_string(),
            (outcome.clone(), std::time::Instant::now()),
        );
        outcome
    }
}

// ---------------------------------------------------------------------------------------------
// Real implementations
// ---------------------------------------------------------------------------------------------

/// A `reqwest`-backed [`WellKnownFetcher`]. Never follows redirects (threat model 2.1), caps the
/// response body, applies [`DISCOVERY_TIMEOUT`], and clamps the cache TTL via
/// [`clamp_cache_control`].
pub struct HttpWellKnownFetcher {
    client: reqwest::Client,
}

impl HttpWellKnownFetcher {
    /// Builds a fetcher with a dedicated `reqwest::Client` configured to never follow redirects.
    ///
    /// # Panics
    /// Panics if the underlying `reqwest::Client` cannot be built (only possible from a
    /// misconfigured TLS backend, which would already be a fatal startup error elsewhere).
    #[must_use]
    pub fn new() -> Self {
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(DISCOVERY_TIMEOUT)
            .build()
            .expect("reqwest client with no custom TLS config always builds");
        Self { client }
    }
}

impl Default for HttpWellKnownFetcher {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl WellKnownFetcher for HttpWellKnownFetcher {
    async fn fetch(&self, hostname: &str) -> WellKnownOutcome {
        let url = format!("https://{hostname}/.well-known/matrix/server");
        let response = match self.client.get(&url).send().await {
            Ok(r) if r.status().is_success() => r,
            _ => {
                return WellKnownOutcome::Absent {
                    cache_for: Duration::from_secs(FAILED_WELL_KNOWN_CACHE_SECS),
                };
            }
        };
        let max_age = response
            .headers()
            .get(reqwest::header::CACHE_CONTROL)
            .and_then(|v| v.to_str().ok())
            .and_then(parse_max_age);

        // Enforce the body size cap without buffering past it: read chunks and bail as soon as
        // the running total would exceed the limit, rather than trusting `Content-Length`.
        let mut response = response;
        let mut body = Vec::new();
        loop {
            match response.chunk().await {
                Ok(Some(chunk)) => {
                    if body.len() + chunk.len() > MAX_WELL_KNOWN_BODY_BYTES {
                        return WellKnownOutcome::Absent {
                            cache_for: Duration::from_secs(FAILED_WELL_KNOWN_CACHE_SECS),
                        };
                    }
                    body.extend_from_slice(&chunk);
                }
                Ok(None) => break,
                Err(_) => {
                    return WellKnownOutcome::Absent {
                        cache_for: Duration::from_secs(FAILED_WELL_KNOWN_CACHE_SECS),
                    };
                }
            }
        }

        match parse_well_known_body(&body) {
            Some(delegated_to) => WellKnownOutcome::Delegated {
                delegated_to,
                cache_for: clamp_cache_control(max_age),
            },
            None => WellKnownOutcome::Absent {
                cache_for: Duration::from_secs(FAILED_WELL_KNOWN_CACHE_SECS),
            },
        }
    }
}

/// A `hickory-resolver`-backed [`SrvResolver`]/[`AddrResolver`], using the system resolver
/// configuration.
pub struct HickoryResolver {
    inner: hickory_resolver::TokioResolver,
}

impl HickoryResolver {
    /// Builds a resolver from the system's own DNS configuration (`/etc/resolv.conf` and
    /// friends).
    ///
    /// # Errors
    /// Returns an error if the system resolver configuration cannot be read or the resolver
    /// cannot be built from it.
    pub fn from_system_conf() -> Result<Self, hickory_resolver::net::NetError> {
        let builder = hickory_resolver::TokioResolver::builder_tokio()?;
        Ok(Self {
            inner: builder.build()?,
        })
    }
}

#[async_trait]
impl SrvResolver for HickoryResolver {
    async fn lookup_srv(&self, service: &str, hostname: &str) -> Vec<(String, u16)> {
        let query = format!("{service}.{hostname}");
        match tokio::time::timeout(DISCOVERY_TIMEOUT, self.inner.srv_lookup(query)).await {
            Ok(Ok(lookup)) => {
                let mut records: Vec<hickory_resolver::proto::rr::rdata::SRV> = lookup
                    .answers()
                    .iter()
                    .filter_map(|record| match record.data {
                        hickory_resolver::proto::rr::RData::SRV(ref srv) => Some(srv.clone()),
                        _ => None,
                    })
                    .collect();
                // Lowest priority number first, matching the RFC 2782 selection rule (weight
                // ordering within a priority tier is not modeled — see the trait doc).
                records.sort_by_key(|srv| srv.priority);
                records
                    .into_iter()
                    .map(|srv| {
                        (
                            srv.target.to_utf8().trim_end_matches('.').to_string(),
                            srv.port,
                        )
                    })
                    .collect()
            }
            _ => Vec::new(),
        }
    }
}

#[async_trait]
impl AddrResolver for HickoryResolver {
    async fn resolve_addr(&self, hostname: &str) -> Vec<IpAddr> {
        match tokio::time::timeout(DISCOVERY_TIMEOUT, self.inner.lookup_ip(hostname)).await {
            Ok(Ok(lookup)) => lookup.iter().collect(),
            _ => Vec::new(),
        }
    }
}

/// Convenience: a [`ResolvedServer`] as a plain `SocketAddr` when `connect_host` is already an IP
/// literal (the common case after full resolution succeeds with an IPv4/IPv6 target).
#[must_use]
pub fn as_socket_addr(server: &ResolvedServer) -> Option<SocketAddr> {
    server
        .connect_host
        .parse::<IpAddr>()
        .ok()
        .map(|ip| SocketAddr::new(ip, server.connect_port))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Mutex;

    /// A fake well-known fetcher keyed by hostname, for reproducing the spec's worked examples
    /// without any network access.
    #[derive(Default)]
    struct FakeWellKnown {
        responses: HashMap<String, WellKnownOutcome>,
        calls: Mutex<Vec<String>>,
    }

    impl FakeWellKnown {
        fn delegating_to(mut self, host: &str, target: &str) -> Self {
            self.responses.insert(
                host.to_string(),
                WellKnownOutcome::Delegated {
                    delegated_to: target.to_string(),
                    cache_for: Duration::from_secs(3600),
                },
            );
            self
        }

        fn absent(mut self, host: &str) -> Self {
            self.responses.insert(
                host.to_string(),
                WellKnownOutcome::Absent {
                    cache_for: Duration::from_secs(60),
                },
            );
            self
        }
    }

    #[async_trait]
    impl WellKnownFetcher for FakeWellKnown {
        async fn fetch(&self, hostname: &str) -> WellKnownOutcome {
            self.calls.lock().unwrap().push(hostname.to_string());
            self.responses
                .get(hostname)
                .cloned()
                .unwrap_or(WellKnownOutcome::Absent {
                    cache_for: Duration::from_secs(60),
                })
        }
    }

    #[derive(Default)]
    struct FakeSrv {
        records: HashMap<(String, String), Vec<(String, u16)>>,
    }

    impl FakeSrv {
        fn with(mut self, service: &str, hostname: &str, target: &str, port: u16) -> Self {
            self.records
                .entry((service.to_string(), hostname.to_string()))
                .or_default()
                .push((target.to_string(), port));
            self
        }
    }

    #[async_trait]
    impl SrvResolver for FakeSrv {
        async fn lookup_srv(&self, service: &str, hostname: &str) -> Vec<(String, u16)> {
            self.records
                .get(&(service.to_string(), hostname.to_string()))
                .cloned()
                .unwrap_or_default()
        }
    }

    #[derive(Default)]
    struct FakeAddr {
        addrs: HashMap<String, Vec<IpAddr>>,
    }

    impl FakeAddr {
        fn with(mut self, hostname: &str, ip: &str) -> Self {
            self.addrs
                .entry(hostname.to_string())
                .or_default()
                .push(ip.parse().unwrap());
            self
        }
    }

    #[async_trait]
    impl AddrResolver for FakeAddr {
        async fn resolve_addr(&self, hostname: &str) -> Vec<IpAddr> {
            self.addrs.get(hostname).cloned().unwrap_or_default()
        }
    }

    fn no_well_known() -> FakeWellKnown {
        FakeWellKnown::default()
    }
    fn no_srv() -> FakeSrv {
        FakeSrv::default()
    }

    // --- Step 1: IP literal ------------------------------------------------------------------

    #[tokio::test]
    async fn ipv4_literal_no_port_uses_default_port_directly() {
        let outcome = resolve(
            "192.0.2.1",
            &no_well_known(),
            &no_srv(),
            &FakeAddr::default(),
        )
        .await
        .unwrap();
        assert_eq!(outcome.server.connect_host, "192.0.2.1");
        assert_eq!(outcome.server.connect_port, DEFAULT_FEDERATION_PORT);
        assert_eq!(outcome.server.via, ResolutionPath::IpLiteral);
        assert_eq!(
            outcome.addresses,
            vec!["192.0.2.1".parse::<IpAddr>().unwrap()]
        );
    }

    #[tokio::test]
    async fn ipv4_literal_with_port_bypasses_discovery_entirely() {
        let outcome = resolve(
            "192.0.2.1:8449",
            &no_well_known(),
            &no_srv(),
            &FakeAddr::default(),
        )
        .await
        .unwrap();
        assert_eq!(outcome.server.connect_port, 8449);
        assert_eq!(outcome.server.via, ResolutionPath::IpLiteral);
    }

    #[tokio::test]
    async fn ipv6_literal_in_brackets_with_port() {
        let outcome = resolve(
            "[2001:db8::1]:8449",
            &no_well_known(),
            &no_srv(),
            &FakeAddr::default(),
        )
        .await
        .unwrap();
        assert_eq!(outcome.server.connect_host, "2001:db8::1");
        assert_eq!(outcome.server.connect_port, 8449);
        assert_eq!(outcome.server.via, ResolutionPath::IpLiteral);
    }

    // --- Step 2: hostname with explicit port bypasses well-known/SRV -------------------------

    #[tokio::test]
    async fn explicit_port_bypasses_well_known_and_srv() {
        let well_known =
            FakeWellKnown::default().delegating_to("example.org", "should-not-be-used.org");
        let srv = FakeSrv::default().with(
            "_matrix-fed._tcp",
            "example.org",
            "also-should-not-be-used.org",
            1234,
        );
        let addr = FakeAddr::default().with("example.org", "203.0.113.5");

        let outcome = resolve("example.org:8449", &well_known, &srv, &addr)
            .await
            .unwrap();

        assert_eq!(outcome.server.connect_host, "example.org");
        assert_eq!(outcome.server.connect_port, 8449);
        assert_eq!(outcome.server.tls_server_name, "example.org");
        assert_eq!(outcome.server.via, ResolutionPath::ExplicitPort);
        assert!(well_known.calls.lock().unwrap().is_empty());
    }

    // --- Step 3a: well-known delegates to a hostname with explicit port ----------------------

    #[tokio::test]
    async fn well_known_delegates_to_explicit_port() {
        let well_known =
            FakeWellKnown::default().delegating_to("example.org", "delegated.example.org:8449");
        let addr = FakeAddr::default().with("delegated.example.org", "203.0.113.10");

        let outcome = resolve("example.org", &well_known, &no_srv(), &addr)
            .await
            .unwrap();

        assert_eq!(outcome.server.connect_host, "delegated.example.org");
        assert_eq!(outcome.server.connect_port, 8449);
        assert_eq!(outcome.server.tls_server_name, "delegated.example.org");
        assert_eq!(outcome.server.via, ResolutionPath::WellKnownExplicitPort);
    }

    // --- Step 3b: well-known delegates to an IP literal ---------------------------------------

    #[tokio::test]
    async fn well_known_delegates_to_ip_literal() {
        let well_known = FakeWellKnown::default().delegating_to("example.org", "203.0.113.20:8449");
        let outcome = resolve("example.org", &well_known, &no_srv(), &FakeAddr::default())
            .await
            .unwrap();
        assert_eq!(outcome.server.connect_host, "203.0.113.20");
        assert_eq!(outcome.server.connect_port, 8449);
        assert_eq!(outcome.server.via, ResolutionPath::WellKnownIpLiteral);
    }

    // --- Step 3c: well-known delegates to bare hostname -> SRV --------------------------------

    #[tokio::test]
    async fn well_known_delegated_hostname_does_srv_lookup() {
        let well_known =
            FakeWellKnown::default().delegating_to("example.org", "delegated.example.org");
        let srv = FakeSrv::default().with(
            "_matrix-fed._tcp",
            "delegated.example.org",
            "srv-target.example.org",
            8449,
        );
        let addr = FakeAddr::default().with("srv-target.example.org", "203.0.113.30");

        let outcome = resolve("example.org", &well_known, &srv, &addr)
            .await
            .unwrap();

        assert_eq!(outcome.server.connect_host, "srv-target.example.org");
        assert_eq!(outcome.server.connect_port, 8449);
        // TLS SNI/Host is the delegated name, not the SRV target.
        assert_eq!(outcome.server.tls_server_name, "delegated.example.org");
        assert_eq!(outcome.server.via, ResolutionPath::WellKnownSrv);
    }

    #[tokio::test]
    async fn well_known_delegated_hostname_srv_falls_back_to_deprecated_service() {
        let well_known =
            FakeWellKnown::default().delegating_to("example.org", "delegated.example.org");
        let srv = FakeSrv::default().with(
            "_matrix._tcp",
            "delegated.example.org",
            "old-srv-target.example.org",
            8448,
        );
        let addr = FakeAddr::default().with("old-srv-target.example.org", "203.0.113.31");

        let outcome = resolve("example.org", &well_known, &srv, &addr)
            .await
            .unwrap();
        assert_eq!(outcome.server.connect_host, "old-srv-target.example.org");
        assert_eq!(outcome.server.via, ResolutionPath::WellKnownSrv);
    }

    #[tokio::test]
    async fn well_known_delegated_hostname_no_srv_falls_back_to_direct_a_lookup() {
        let well_known =
            FakeWellKnown::default().delegating_to("example.org", "delegated.example.org");
        let addr = FakeAddr::default().with("delegated.example.org", "203.0.113.40");

        let outcome = resolve("example.org", &well_known, &no_srv(), &addr)
            .await
            .unwrap();

        assert_eq!(outcome.server.connect_host, "delegated.example.org");
        assert_eq!(outcome.server.connect_port, DEFAULT_FEDERATION_PORT);
        assert_eq!(outcome.server.via, ResolutionPath::WellKnownFallbackDirect);
    }

    // --- Step 3d: no well-known -> SRV on original hostname ------------------------------------

    #[tokio::test]
    async fn no_well_known_does_srv_lookup_on_original_host() {
        let well_known = FakeWellKnown::default().absent("example.org");
        let srv = FakeSrv::default().with(
            "_matrix-fed._tcp",
            "example.org",
            "srv-target.example.org",
            8449,
        );
        let addr = FakeAddr::default().with("srv-target.example.org", "203.0.113.50");

        let outcome = resolve("example.org", &well_known, &srv, &addr)
            .await
            .unwrap();

        assert_eq!(outcome.server.connect_host, "srv-target.example.org");
        assert_eq!(outcome.server.connect_port, 8449);
        assert_eq!(outcome.server.tls_server_name, "example.org");
        assert_eq!(outcome.server.via, ResolutionPath::Srv);
    }

    #[tokio::test]
    async fn well_known_fetch_failure_falls_back_to_srv_on_original_host() {
        // No entry at all in the fake => Absent by default, simulating a fetch failure/timeout.
        let well_known = no_well_known();
        let srv = FakeSrv::default().with(
            "_matrix-fed._tcp",
            "example.org",
            "srv-target.example.org",
            8449,
        );
        let addr = FakeAddr::default().with("srv-target.example.org", "203.0.113.51");

        let outcome = resolve("example.org", &well_known, &srv, &addr)
            .await
            .unwrap();
        assert_eq!(outcome.server.via, ResolutionPath::Srv);
    }

    // --- Step 3e: no well-known, no SRV -> direct A/AAAA on 8448 -------------------------------

    #[tokio::test]
    async fn no_well_known_no_srv_falls_back_to_direct_a_lookup_on_default_port() {
        let well_known = FakeWellKnown::default().absent("example.org");
        let addr = FakeAddr::default().with("example.org", "203.0.113.60");

        let outcome = resolve("example.org", &well_known, &no_srv(), &addr)
            .await
            .unwrap();

        assert_eq!(outcome.server.connect_host, "example.org");
        assert_eq!(outcome.server.connect_port, DEFAULT_FEDERATION_PORT);
        assert_eq!(outcome.server.via, ResolutionPath::FallbackDirect);
    }

    // --- Hostile responses -----------------------------------------------------------------

    #[tokio::test]
    async fn malformed_well_known_body_is_treated_as_absent() {
        // A delegated target of only whitespace should not be treated as a valid delegation by a
        // real fetcher (see HttpWellKnownFetcher::fetch); exercise the resolution-algorithm side
        // by simulating what a real fetcher would report for a malformed body: `Absent`.
        let well_known = FakeWellKnown::default().absent("example.org");
        let addr = FakeAddr::default().with("example.org", "203.0.113.70");
        let outcome = resolve("example.org", &well_known, &no_srv(), &addr)
            .await
            .unwrap();
        assert_eq!(outcome.server.via, ResolutionPath::FallbackDirect);
    }

    #[tokio::test]
    async fn no_address_anywhere_is_an_error() {
        let well_known = FakeWellKnown::default().absent("example.org");
        let err = resolve("example.org", &well_known, &no_srv(), &FakeAddr::default())
            .await
            .unwrap_err();
        assert_eq!(err, DiscoveryError::NoAddress("example.org".to_string()));
    }

    // --- Cache clamping ------------------------------------------------------------------------

    #[test]
    fn cache_control_clamps_tiny_max_age_up_to_the_minimum() {
        assert_eq!(
            clamp_cache_control(Some(1)),
            Duration::from_secs(MIN_WELL_KNOWN_CACHE_SECS)
        );
    }

    #[test]
    fn cache_control_clamps_huge_max_age_down_to_the_maximum() {
        assert_eq!(
            clamp_cache_control(Some(u64::MAX)),
            Duration::from_secs(MAX_WELL_KNOWN_CACHE_SECS)
        );
    }

    #[test]
    fn cache_control_uses_default_when_absent() {
        assert_eq!(
            clamp_cache_control(None),
            Duration::from_secs(DEFAULT_WELL_KNOWN_CACHE_SECS)
        );
    }

    // --- CachingWellKnownFetcher -------------------------------------------------------------

    struct CountingFetcher {
        outcome: WellKnownOutcome,
        calls: std::sync::atomic::AtomicUsize,
    }

    #[async_trait]
    impl WellKnownFetcher for CountingFetcher {
        async fn fetch(&self, _hostname: &str) -> WellKnownOutcome {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            self.outcome.clone()
        }
    }

    #[tokio::test]
    async fn caching_fetcher_avoids_refetch_within_the_ttl() {
        let inner = CountingFetcher {
            outcome: WellKnownOutcome::Delegated {
                delegated_to: "delegated.example.org".to_string(),
                cache_for: Duration::from_secs(3600),
            },
            calls: std::sync::atomic::AtomicUsize::new(0),
        };
        let cached = CachingWellKnownFetcher::new(inner);
        cached.fetch("example.org").await;
        cached.fetch("example.org").await;
        cached.fetch("example.org").await;
        assert_eq!(
            cached.inner.calls.load(std::sync::atomic::Ordering::SeqCst),
            1
        );
    }

    #[tokio::test]
    async fn caching_fetcher_refetches_after_ttl_expires() {
        let inner = CountingFetcher {
            outcome: WellKnownOutcome::Absent {
                cache_for: Duration::from_millis(1),
            },
            calls: std::sync::atomic::AtomicUsize::new(0),
        };
        let cached = CachingWellKnownFetcher::new(inner);
        cached.fetch("example.org").await;
        tokio::time::sleep(Duration::from_millis(20)).await;
        cached.fetch("example.org").await;
        assert_eq!(
            cached.inner.calls.load(std::sync::atomic::Ordering::SeqCst),
            2
        );
    }

    #[test]
    fn parses_max_age_from_cache_control_header() {
        assert_eq!(parse_max_age("max-age=3600, public"), Some(3600));
        assert_eq!(parse_max_age("no-cache"), None);
        assert_eq!(parse_max_age(""), None);
    }
}
