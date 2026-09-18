//! The outbound federation HTTP client: per-destination concurrency limits, persisted
//! retry/backoff, and allow/deny-list (domain and IP-range) enforcement in both the discovery and
//! send paths, per `docs/design/06-federation-threat-model.md` section 2.6.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;

use ipnet::IpNet;
use tokio::sync::Semaphore;

use crate::destination_store::DestinationStore;
use crate::discovery::{self, AddrResolver, ResolveOutcome, SrvResolver, WellKnownFetcher};
use crate::xmatrix;
use hs_model::signing::SigningKeyPair;

/// Max bytes read from any single federation HTTP response before giving up (threat model
/// section 3: 50 MiB), enforced independent of any `Content-Length` the peer claims.
pub const MAX_RESPONSE_BODY_BYTES: usize = 50 * 1024 * 1024;

/// Per-destination outbound concurrency (threat model section 3 / Synapse's own default: 1
/// in-flight request per destination).
pub const DEFAULT_PER_DESTINATION_CONCURRENCY: usize = 1;

/// Parsed CIDR allow/deny policy for outbound connection targets, built once from
/// `hs-config::FederationConfig`'s string lists (this crate does not depend on `hs-config`'s
/// struct directly, to keep this module testable without it — see [`IpPolicy::from_cidrs`]).
#[derive(Debug, Clone, Default)]
pub struct IpPolicy {
    blocklist: Vec<IpNet>,
    allowlist: Vec<IpNet>,
}

impl IpPolicy {
    /// Parses CIDR strings (invalid entries are skipped — `hs-config::FederationConfig::validate`
    /// is the place malformed CIDRs are rejected at config-load time; by the time this runs, the
    /// list is assumed already validated, but a defensive skip here is cheap insurance against a
    /// panic on bad input reaching this far).
    #[must_use]
    pub fn from_cidrs(blocklist: &[String], allowlist: &[String]) -> Self {
        Self {
            blocklist: blocklist.iter().filter_map(|s| s.parse().ok()).collect(),
            allowlist: allowlist.iter().filter_map(|s| s.parse().ok()).collect(),
        }
    }

    /// Whether `addr` is allowed to be connected to: not in `blocklist`, or in `allowlist` (an
    /// explicit allowlist entry overrides a blocklist match, matching
    /// `hs-config::FederationConfig`'s documented semantics for a deliberately private
    /// deployment).
    #[must_use]
    pub fn allows(&self, addr: IpAddr) -> bool {
        let blocked = self.blocklist.iter().any(|net| net.contains(&addr));
        if !blocked {
            return true;
        }
        self.allowlist.iter().any(|net| net.contains(&addr))
    }
}

/// The domain allow/deny check (`FederationConfig::domain_allowlist`), applied against the
/// *original* server name we were asked to federate with (threat model 2.6: delegation must not
/// bypass this).
#[derive(Debug, Clone, Default)]
pub struct DomainPolicy {
    allowlist: Option<Vec<String>>,
}

impl DomainPolicy {
    #[must_use]
    pub fn new(allowlist: Option<Vec<String>>) -> Self {
        Self { allowlist }
    }

    #[must_use]
    pub fn allows(&self, server_name: &str) -> bool {
        match &self.allowlist {
            None => true,
            Some(list) => list.iter().any(|s| s == server_name),
        }
    }
}

/// Why an outbound federation call did not happen (or did not succeed).
#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error("federation is disabled")]
    Disabled,
    #[error("destination `{0}` is not in the domain allowlist")]
    DomainDenied(String),
    #[error("destination `{0}` resolved to an address outside the allowed IP ranges")]
    IpDenied(String),
    #[error("destination `{destination}` is backing off, retry after {retry_at_ms}")]
    Backoff {
        destination: String,
        retry_at_ms: u64,
    },
    #[error("could not resolve destination `{0}`: {1}")]
    Discovery(String, String),
    #[error("request to `{0}` failed: {1}")]
    Request(String, String),
    #[error("response from `{0}` exceeded the size limit")]
    ResponseTooLarge(String),
    #[error("response from `{0}` was not valid JSON: {1}")]
    BadResponseJson(String, String),
}

/// Configuration the client needs from `hs-config::FederationConfig`, copied into this crate's
/// own type (see [`IpPolicy`]'s doc for why) rather than depending on `hs-config` directly.
pub struct ClientConfig {
    pub enabled: bool,
    pub domain_policy: DomainPolicy,
    pub ip_policy: IpPolicy,
    pub verify_certificates: bool,
    pub request_timeout: Duration,
    pub max_retry_backoff: Duration,
    pub per_destination_concurrency: usize,
    /// The URL scheme used for outbound requests. Always `"https"` in production — federation is
    /// specified as HTTPS-only. This exists as a seam so this crate's own tests can point the
    /// client at a plaintext `hs-testkit::FakeFederationPeer` without standing up a real TLS
    /// listener and a certificate trust chain, which would test `reqwest`'s TLS stack (someone
    /// else's already-tested code) rather than this client's own logic (discovery, signing,
    /// pooling, concurrency limits, backoff). Not exposed by any config loader — `hs-config`
    /// deserializes `ClientConfig` fields it knows about and has no path that could set this to
    /// anything but the default.
    pub scheme: &'static str,
}

impl Default for ClientConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            domain_policy: DomainPolicy::default(),
            ip_policy: IpPolicy::default(),
            verify_certificates: true,
            request_timeout: Duration::from_secs(30),
            max_retry_backoff: Duration::from_secs(3600),
            per_destination_concurrency: DEFAULT_PER_DESTINATION_CONCURRENCY,
            scheme: "https",
        }
    }
}

/// A JSON federation response: status code plus parsed body.
#[derive(Debug, Clone)]
pub struct FederationResponse {
    pub status: u16,
    pub body: serde_json::Value,
}

/// The outbound federation HTTP client.
pub struct FederationClient {
    own_server_name: String,
    signing_key: SigningKeyPair,
    config: ClientConfig,
    destinations: Arc<dyn DestinationStore>,
    well_known: Arc<dyn WellKnownFetcher>,
    srv: Arc<dyn SrvResolver>,
    addr: Arc<dyn AddrResolver>,
    semaphores: std::sync::Mutex<HashMap<String, Arc<Semaphore>>>,
    /// One pooled, resolve-pinned `reqwest::Client` per destination's current resolution, so
    /// repeat requests to a still-current destination reuse connections. Rebuilt whenever a fresh
    /// [`ResolveOutcome`] differs from what is cached (no proactive TTL-based invalidation this
    /// pass — see `docs/status/06-federation.md`).
    http_clients: std::sync::Mutex<HashMap<String, (ResolveOutcome, reqwest::Client)>>,
}

impl FederationClient {
    #[must_use]
    pub fn new(
        own_server_name: impl Into<String>,
        signing_key: SigningKeyPair,
        config: ClientConfig,
        destinations: Arc<dyn DestinationStore>,
        well_known: Arc<dyn WellKnownFetcher>,
        srv: Arc<dyn SrvResolver>,
        addr: Arc<dyn AddrResolver>,
    ) -> Self {
        Self {
            own_server_name: own_server_name.into(),
            signing_key,
            config,
            destinations,
            well_known,
            srv,
            addr,
            semaphores: std::sync::Mutex::new(HashMap::new()),
            http_clients: std::sync::Mutex::new(HashMap::new()),
        }
    }

    fn semaphore_for(&self, destination: &str) -> Arc<Semaphore> {
        self.semaphores
            .lock()
            .unwrap()
            .entry(destination.to_string())
            .or_insert_with(|| Arc::new(Semaphore::new(self.config.per_destination_concurrency)))
            .clone()
    }

    /// Sends a signed federation request. `path` is the spec-relative path
    /// (`/_matrix/federation/v1/version`, not just `/version`) — the exact string that becomes
    /// both the HTTP request target and the `uri` field of the signed object.
    ///
    /// # Errors
    /// See [`ClientError`].
    pub async fn send(
        &self,
        destination: &str,
        method: &str,
        path: &str,
        body: Option<&serde_json::Value>,
    ) -> Result<FederationResponse, ClientError> {
        if !self.config.enabled {
            return Err(ClientError::Disabled);
        }
        if !self.config.domain_policy.allows(destination) {
            return Err(ClientError::DomainDenied(destination.to_string()));
        }

        let now = now_ms();
        let state = self.destinations.get(destination).await;
        if !state.is_ready(now) {
            return Err(ClientError::Backoff {
                destination: destination.to_string(),
                retry_at_ms: state.retry_at_ms.unwrap_or(now),
            });
        }

        let permit = self
            .semaphore_for(destination)
            .acquire_owned()
            .await
            .expect("semaphore is never closed");

        let result = self.send_inner(destination, method, path, body).await;
        drop(permit);

        match &result {
            Ok(_) => self.destinations.record_success(destination).await,
            Err(ClientError::Request(..) | ClientError::ResponseTooLarge(..)) => {
                self.destinations
                    .record_failure(
                        destination,
                        self.config.max_retry_backoff.as_millis() as u64,
                    )
                    .await;
            }
            _ => {}
        }

        result
    }

    async fn send_inner(
        &self,
        destination: &str,
        method: &str,
        path: &str,
        body: Option<&serde_json::Value>,
    ) -> Result<FederationResponse, ClientError> {
        let outcome = discovery::resolve(
            destination,
            self.well_known.as_ref(),
            self.srv.as_ref(),
            self.addr.as_ref(),
        )
        .await
        .map_err(|e| ClientError::Discovery(destination.to_string(), e.to_string()))?;

        // IP-range check: against the resolved addresses if any were returned, otherwise against
        // the connect_host itself (already-an-IP-literal case) — either way, this is the
        // *resolved connection target*, checked independent of the domain allowlist above (threat
        // model 2.6: delegation must not bypass either check).
        let candidates: Vec<IpAddr> = if outcome.addresses.is_empty() {
            outcome
                .server
                .connect_host
                .parse::<IpAddr>()
                .into_iter()
                .collect()
        } else {
            outcome.addresses.clone()
        };
        if !candidates.is_empty()
            && !candidates
                .iter()
                .any(|ip| self.config.ip_policy.allows(*ip))
        {
            return Err(ClientError::IpDenied(destination.to_string()));
        }

        let client = self.client_for(destination, &outcome);

        let url = format!(
            "{}://{}:{}{}",
            self.config.scheme, outcome.server.tls_server_name, outcome.server.connect_port, path
        );

        let content = body.cloned();
        let auth_header = xmatrix::sign_request(
            method,
            path,
            &self.own_server_name,
            destination,
            content.as_ref(),
            &self.signing_key,
        )
        .map_err(|e| ClientError::Request(destination.to_string(), e.to_string()))?;

        let mut request = client.request(
            method
                .parse()
                .map_err(|_| ClientError::Request(destination.to_string(), "bad method".into()))?,
            &url,
        );
        request = request.header(reqwest::header::AUTHORIZATION, auth_header);
        if let Some(b) = body {
            request = request.json(b);
        }

        let response = request
            .send()
            .await
            .map_err(|e| ClientError::Request(destination.to_string(), e.to_string()))?;
        let status = response.status().as_u16();

        let bytes = read_capped(response, MAX_RESPONSE_BODY_BYTES)
            .await
            .ok_or_else(|| ClientError::ResponseTooLarge(destination.to_string()))?;

        let parsed = if bytes.is_empty() {
            serde_json::Value::Null
        } else {
            serde_json::from_slice(&bytes)
                .map_err(|e| ClientError::BadResponseJson(destination.to_string(), e.to_string()))?
        };

        Ok(FederationResponse {
            status,
            body: parsed,
        })
    }

    /// Returns a pooled `reqwest::Client` pinned (via `.resolve()`) so that connecting to
    /// `outcome.server.tls_server_name` actually opens a TCP connection to
    /// `outcome.server.connect_host`'s resolved address, while TLS SNI / the HTTP `Host` header
    /// still present `tls_server_name` — the separation the spec's delegation model requires.
    /// Rebuilds the pinned client if `outcome` differs from what is cached for this destination.
    fn client_for(&self, destination: &str, outcome: &ResolveOutcome) -> reqwest::Client {
        let mut clients = self.http_clients.lock().unwrap();
        if let Some((cached_outcome, client)) = clients.get(destination)
            && cached_outcome.server == outcome.server
        {
            return client.clone();
        }

        let connect_addr: Option<IpAddr> = outcome
            .addresses
            .first()
            .copied()
            .or_else(|| outcome.server.connect_host.parse().ok());

        let mut builder = reqwest::Client::builder()
            .timeout(self.config.request_timeout)
            .danger_accept_invalid_certs(!self.config.verify_certificates)
            .http1_only(); // HTTP/1.1-only to peers, per the recorded decision.

        if let Some(ip) = connect_addr {
            builder = builder.resolve(
                &outcome.server.tls_server_name,
                std::net::SocketAddr::new(ip, outcome.server.connect_port),
            );
        }

        let client = builder
            .build()
            .expect("reqwest client with only timeout/resolve overrides always builds");
        clients.insert(
            destination.to_string(),
            (clone_outcome(outcome), client.clone()),
        );
        client
    }
}

fn clone_outcome(outcome: &ResolveOutcome) -> ResolveOutcome {
    ResolveOutcome {
        server: outcome.server.clone(),
        addresses: outcome.addresses.clone(),
    }
}

async fn read_capped(mut response: reqwest::Response, cap: usize) -> Option<bytes::Bytes> {
    let mut buf = Vec::new();
    loop {
        match response.chunk().await {
            Ok(Some(chunk)) => {
                if buf.len() + chunk.len() > cap {
                    return None;
                }
                buf.extend_from_slice(&chunk);
            }
            Ok(None) => break,
            Err(_) => return None,
        }
    }
    Some(bytes::Bytes::from(buf))
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::destination_store::InMemoryDestinationStore;
    use crate::discovery::WellKnownOutcome;
    use async_trait::async_trait;
    use hs_testkit::fake_federation::FakeFederationPeer;
    use std::net::{Ipv4Addr, SocketAddr as StdSocketAddr};
    use tokio::net::TcpListener;

    struct NoWellKnown;
    #[async_trait]
    impl WellKnownFetcher for NoWellKnown {
        async fn fetch(&self, _hostname: &str) -> WellKnownOutcome {
            WellKnownOutcome::Absent {
                cache_for: Duration::from_secs(60),
            }
        }
    }
    struct NoSrv;
    #[async_trait]
    impl SrvResolver for NoSrv {
        async fn lookup_srv(&self, _service: &str, _hostname: &str) -> Vec<(String, u16)> {
            Vec::new()
        }
    }
    struct FixedAddr(IpAddr);
    #[async_trait]
    impl AddrResolver for FixedAddr {
        async fn resolve_addr(&self, _hostname: &str) -> Vec<IpAddr> {
            vec![self.0]
        }
    }

    async fn spawn_peer() -> (String, u16, tokio::task::JoinHandle<()>) {
        let peer = FakeFederationPeer::new("peer.example.org");
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = peer.router();
        let handle = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (peer.server_name().to_string(), addr.port(), handle)
    }

    fn client_for_port(_port: u16, config: ClientConfig) -> FederationClient {
        let dir = tempfile::tempdir().unwrap();
        let keys = crate::keys::OwnSigningKeys::load_or_generate(dir.path()).unwrap();
        FederationClient::new(
            "us.example.org",
            keys.primary().clone(),
            config,
            Arc::new(InMemoryDestinationStore::new()),
            Arc::new(NoWellKnown),
            Arc::new(NoSrv),
            Arc::new(FixedAddr(IpAddr::V4(Ipv4Addr::LOCALHOST))),
        )
    }

    #[tokio::test]
    async fn sends_a_signed_request_and_parses_the_response() {
        let (_name, port, _handle) = spawn_peer().await;
        // Allow loopback for this test (default policy blocks it, correctly, in production); use
        // plaintext HTTP against the fake peer (see `ClientConfig::scheme`'s doc for why).
        let config = ClientConfig {
            ip_policy: IpPolicy::default(),
            scheme: "http",
            ..ClientConfig::default()
        };
        let client = client_for_port(port, config);

        let response = client
            .send(
                &format!("localhost:{port}"),
                "GET",
                "/_matrix/federation/v1/version",
                None,
            )
            .await
            .unwrap();
        assert_eq!(response.status, 200);
    }

    #[tokio::test]
    async fn domain_denylist_blocks_before_any_network_call() {
        let config = ClientConfig {
            domain_policy: DomainPolicy::new(Some(vec!["allowed.example.org".to_string()])),
            ..ClientConfig::default()
        };
        let client = client_for_port(0, config);

        let err = client
            .send(
                "denied.example.org",
                "GET",
                "/_matrix/federation/v1/version",
                None,
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ClientError::DomainDenied(_)));
    }

    #[tokio::test]
    async fn ip_range_blocklist_blocks_the_resolved_address() {
        // Block all of loopback explicitly.
        let config = ClientConfig {
            ip_policy: IpPolicy::from_cidrs(&["127.0.0.0/8".to_string()], &[]),
            ..ClientConfig::default()
        };
        let client = client_for_port(9999, config);

        let err = client
            .send(
                "blocked.example.org",
                "GET",
                "/_matrix/federation/v1/version",
                None,
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ClientError::IpDenied(_)));
    }

    #[tokio::test]
    async fn ip_allowlist_overrides_blocklist() {
        let (_name, port, _handle) = spawn_peer().await;
        let config = ClientConfig {
            ip_policy: IpPolicy::from_cidrs(
                &["127.0.0.0/8".to_string()],
                &["127.0.0.1/32".to_string()],
            ),
            scheme: "http",
            ..ClientConfig::default()
        };
        let client = client_for_port(port, config);

        let response = client
            .send(
                &format!("localhost:{port}"),
                "GET",
                "/_matrix/federation/v1/version",
                None,
            )
            .await
            .unwrap();
        assert_eq!(response.status, 200);
    }

    #[tokio::test]
    async fn a_backing_off_destination_is_not_retried_early() {
        let dir = tempfile::tempdir().unwrap();
        let keys = crate::keys::OwnSigningKeys::load_or_generate(dir.path()).unwrap();
        let destinations = Arc::new(InMemoryDestinationStore::new());
        destinations
            .record_failure("flaky.example.org", 3_600_000)
            .await;

        let client = FederationClient::new(
            "us.example.org",
            keys.primary().clone(),
            ClientConfig::default(),
            destinations,
            Arc::new(NoWellKnown),
            Arc::new(NoSrv),
            Arc::new(FixedAddr(IpAddr::V4(Ipv4Addr::LOCALHOST))),
        );

        let err = client
            .send(
                "flaky.example.org",
                "GET",
                "/_matrix/federation/v1/version",
                None,
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ClientError::Backoff { .. }));
    }

    #[tokio::test]
    async fn per_destination_concurrency_limit_serializes_requests() {
        // A concurrency-1 semaphore around two simultaneous sends to the same destination should
        // not deadlock or drop either request — both eventually complete against the fake peer.
        let (_name, port, _handle) = spawn_peer().await;
        let config = ClientConfig {
            per_destination_concurrency: 1,
            scheme: "http",
            ..ClientConfig::default()
        };
        let client = Arc::new(client_for_port(port, config));

        let dest = format!("localhost:{port}");
        let c1 = client.clone();
        let d1 = dest.clone();
        let c2 = client.clone();
        let d2 = dest.clone();
        let (r1, r2) = tokio::join!(
            c1.send(&d1, "GET", "/_matrix/federation/v1/version", None),
            c2.send(&d2, "GET", "/_matrix/federation/v1/version", None)
        );
        assert_eq!(r1.unwrap().status, 200);
        assert_eq!(r2.unwrap().status, 200);
    }

    #[test]
    fn ip_policy_allows_by_default_with_empty_lists() {
        let policy = IpPolicy::default();
        assert!(policy.allows(IpAddr::V4(Ipv4Addr::new(203, 0, 113, 1))));
    }

    #[test]
    fn ip_policy_blocks_listed_ranges() {
        let policy = IpPolicy::from_cidrs(&["10.0.0.0/8".to_string()], &[]);
        assert!(!policy.allows(IpAddr::V4(Ipv4Addr::new(10, 1, 2, 3))));
        assert!(policy.allows(IpAddr::V4(Ipv4Addr::new(203, 0, 113, 1))));
    }

    #[test]
    fn domain_policy_none_allows_everything() {
        assert!(DomainPolicy::new(None).allows("anything.example.org"));
    }

    #[test]
    fn domain_policy_some_restricts() {
        let p = DomainPolicy::new(Some(vec!["a.example.org".to_string()]));
        assert!(p.allows("a.example.org"));
        assert!(!p.allows("b.example.org"));
    }

    // Suppress "unused" on the unused helper import when compiled without networking pieces used
    // by every test above (kept explicit rather than silently allowed).
    #[allow(dead_code)]
    fn _touch(_a: StdSocketAddr) {}
}
