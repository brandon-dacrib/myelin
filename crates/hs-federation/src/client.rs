//! The outbound federation HTTP client: per-destination concurrency limits, persisted
//! retry/backoff, and allow/deny-list (domain and IP-range) enforcement in both the discovery and
//! send paths, per `docs/design/06-federation-threat-model.md` section 2.6.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;

use ipnet::IpNet;
use serde_json::Value;
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
///
/// A shared handle: clones see the same lists, and [`IpPolicy::set_cidrs`] replaces them for
/// every one of them, so a running server takes an operator's change on the next request.
#[derive(Debug, Clone, Default)]
pub struct IpPolicy {
    lists: Arc<std::sync::RwLock<IpLists>>,
}

#[derive(Debug, Default)]
struct IpLists {
    blocklist: Vec<IpNet>,
    allowlist: Vec<IpNet>,
}

impl IpLists {
    fn parse(blocklist: &[String], allowlist: &[String]) -> Self {
        Self {
            blocklist: blocklist.iter().filter_map(|s| s.parse().ok()).collect(),
            allowlist: allowlist.iter().filter_map(|s| s.parse().ok()).collect(),
        }
    }
}

impl IpPolicy {
    /// Parses CIDR strings (invalid entries are skipped — `hs-config::FederationConfig::validate`
    /// is the place malformed CIDRs are rejected at config-load time; by the time this runs, the
    /// list is assumed already validated, but a defensive skip here is cheap insurance against a
    /// panic on bad input reaching this far).
    #[must_use]
    pub fn from_cidrs(blocklist: &[String], allowlist: &[String]) -> Self {
        Self {
            lists: Arc::new(std::sync::RwLock::new(IpLists::parse(blocklist, allowlist))),
        }
    }

    /// Replaces both lists, for this handle and every clone of it, parsed as
    /// [`IpPolicy::from_cidrs`] parses them.
    pub fn set_cidrs(&self, blocklist: &[String], allowlist: &[String]) {
        *self
            .lists
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) =
            IpLists::parse(blocklist, allowlist);
    }

    /// Whether `addr` is allowed to be connected to: not in `blocklist`, or in `allowlist` (an
    /// explicit allowlist entry overrides a blocklist match, matching
    /// `hs-config::FederationConfig`'s documented semantics for a deliberately private
    /// deployment).
    #[must_use]
    pub fn allows(&self, addr: IpAddr) -> bool {
        let lists = self
            .lists
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let blocked = lists.blocklist.iter().any(|net| net.contains(&addr));
        if !blocked {
            return true;
        }
        lists.allowlist.iter().any(|net| net.contains(&addr))
    }
}

/// The domain allow/deny check (`FederationConfig::domain_allowlist`), applied against the
/// *original* server name we were asked to federate with (threat model 2.6: delegation must not
/// bypass this).
///
/// A shared handle, like [`IpPolicy`]: [`DomainPolicy::set`] replaces the list for every clone.
#[derive(Debug, Clone, Default)]
pub struct DomainPolicy {
    allowlist: Arc<std::sync::RwLock<Option<Vec<String>>>>,
}

impl DomainPolicy {
    /// A policy allowing exactly `allowlist`, or every server when `None`.
    #[must_use]
    pub fn new(allowlist: Option<Vec<String>>) -> Self {
        Self {
            allowlist: Arc::new(std::sync::RwLock::new(allowlist)),
        }
    }

    /// Replaces the allowlist, for this handle and every clone of it.
    pub fn set(&self, allowlist: Option<Vec<String>>) {
        *self
            .allowlist
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = allowlist;
    }

    /// Whether this server may federate with `server_name`.
    #[must_use]
    pub fn allows(&self, server_name: &str) -> bool {
        match &*self
            .allowlist
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
        {
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
    /// The destination answered, with a status that is not success. For the calls that read a
    /// list out of the body ([`FederationClient::backfill`],
    /// [`FederationClient::get_missing_events`]) this is what a `403` or `404` becomes, so that
    /// a refusal is logged as one and never read as "the remote has nothing".
    #[error("`{destination}` answered HTTP {status}: {body}")]
    Rejected {
        destination: String,
        status: u16,
        body: String,
    },
}

/// The body of a non-success response, cut to what a log line can carry.
fn rejection_body(body: &serde_json::Value) -> String {
    let text = body.to_string();
    if text.len() > 200 {
        format!("{}...", &text[..text.floor_char_boundary(200)])
    } else {
        text
    }
}

/// Configuration the client needs from `hs-config::FederationConfig`, copied into this crate's
/// own type (see [`IpPolicy`]'s doc for why) rather than depending on `hs-config` directly.
pub struct ClientConfig {
    pub enabled: bool,
    pub domain_policy: DomainPolicy,
    pub ip_policy: IpPolicy,
    /// Whether outbound federation TLS validates the peer's certificate at all. See
    /// [`FederationClient::new`]'s doc for the loud warning this crate emits when this is `false`
    /// — every real deployment must leave this `true`; it exists for test harnesses (Complement,
    /// ...) that terminate TLS with a certificate this server has no other way to trust yet.
    pub verify_certificates: bool,
    /// Additional CA certificates trusted for outbound federation TLS, as raw PEM bytes (each
    /// entry may itself be a bundle of more than one certificate — see
    /// [`reqwest::Certificate::from_pem_bundle`]). This is the config surface's answer to "how do
    /// I federate with a server whose certificate chains to a CA that is not one of the ~140
    /// public roots this server trusts by default": name the CA explicitly here (matching
    /// Synapse's `federation_custom_ca_list`), rather than reaching for
    /// [`Self::verify_certificates`], which trusts *any* certificate at all. Added on top of —
    /// never in place of — the built-in public root bundle, so ordinary public federation is
    /// unaffected. `hs-config::FederationConfig::custom_ca_certificates` is the schema field this
    /// is built from (file paths); reading the files is left to the wiring site that already
    /// does I/O for config loading, so this crate's own tests can supply certificate bytes
    /// directly (e.g. from `rcgen`) without touching a filesystem.
    pub custom_root_certificates: Vec<Vec<u8>>,
    /// Whether outbound federation TLS also trusts whatever CA store the operating system
    /// trusts. See `hs-config::FederationConfig::trust_os_root_store`'s doc comment for the
    /// default (`false`) and the reasoning; this field exists purely so this crate's own tests
    /// can exercise the toggle without a real `hs-config` value.
    pub trust_os_root_store: bool,
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
            custom_root_certificates: Vec::new(),
            trust_os_root_store: false,
            request_timeout: Duration::from_secs(30),
            max_retry_backoff: Duration::from_secs(3600),
            per_destination_concurrency: DEFAULT_PER_DESTINATION_CONCURRENCY,
            scheme: "https",
        }
    }
}

/// `error`'s message followed by every cause underneath it. `reqwest::Error`'s own `Display`
/// says only "error sending request for url (...)"; the reason -- a certificate rejected for its
/// name, a refused connection, a resolver with no answer -- is in its source chain, and a
/// destination that cannot be reached is not worth much in a log without it. Found the hard way
/// twice: a doubled port in the URL (seventh session), and a federation certificate with no
/// subject alternative name (eighth), each of which this crate reported as the bare sentence.
fn error_chain(error: &dyn std::error::Error) -> String {
    let mut message = error.to_string();
    let mut cause = error.source();
    while let Some(source) = cause {
        message.push_str(": ");
        message.push_str(&source.to_string());
        cause = source.source();
    }
    message
}

/// A JSON federation response: status code plus parsed body.
#[derive(Debug, Clone)]
pub struct FederationResponse {
    pub status: u16,
    pub body: serde_json::Value,
}

/// A raw media response ([`FederationClient::get_media`]): the status, the headers a media
/// answer is read by, and the body as bytes.
#[derive(Debug, Clone)]
pub struct MediaResponse {
    /// The HTTP status.
    pub status: u16,
    /// `Content-Type`: `multipart/mixed; boundary=...` from the federation media API, the
    /// media's own type from the legacy path.
    pub content_type: Option<String>,
    /// `Content-Disposition`, from the legacy path (the federation API carries it inside the
    /// multipart body instead).
    pub content_disposition: Option<String>,
    /// `Location`, on a redirect.
    pub location: Option<String>,
    /// The body, capped at the `max_bytes` the caller gave.
    pub body: bytes::Bytes,
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
    /// `config.custom_root_certificates`, parsed once at construction rather than on every
    /// `client_for` rebuild. An entry that fails to parse is dropped with a loud `tracing::error!`
    /// (not a panic and not a silent skip) — a malformed CA file should be visibly wrong at
    /// startup, not a mysterious TLS failure the first time this server tries to reach the peer
    /// it was meant to trust.
    custom_roots: Vec<reqwest::Certificate>,
}

impl FederationClient {
    /// The domain allowlist this client checks every destination against, as a shared handle:
    /// [`DomainPolicy::set`] on it changes what the next request is allowed to reach.
    #[must_use]
    pub fn domain_policy(&self) -> &DomainPolicy {
        &self.config.domain_policy
    }

    /// The IP-range policy this client checks every resolved address against, as a shared
    /// handle: [`IpPolicy::set_cidrs`] on it changes what the next request may connect to.
    #[must_use]
    pub fn ip_policy(&self) -> &IpPolicy {
        &self.config.ip_policy
    }

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
        if !config.verify_certificates {
            tracing::warn!(
                "federation.verify_certificates is FALSE: outbound federation TLS will accept \
                 ANY certificate, valid or not, from ANY peer. Every event this server receives \
                 over federation is only as trustworthy as its Ed25519 signature at that point \
                 -- an attacker who can intercept outbound federation traffic can impersonate any \
                 remote server. This is a test-harness escape hatch (e.g. Complement, which \
                 terminates TLS with a certificate this server has no other way to trust yet) and \
                 must never be set on a production deployment. Prefer \
                 `federation.custom_ca_certificates` to trust a specific, known CA instead."
            );
        }

        let mut custom_roots = Vec::with_capacity(config.custom_root_certificates.len());
        for (i, pem) in config.custom_root_certificates.iter().enumerate() {
            match reqwest::Certificate::from_pem_bundle(pem) {
                Ok(certs) => custom_roots.extend(certs),
                Err(e) => tracing::error!(
                    "federation.custom_ca_certificates[{i}] could not be parsed as a PEM \
                     certificate (or bundle) and will NOT be trusted: {e}"
                ),
            }
        }

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
            custom_roots,
        }
    }

    /// The ceiling on this client's per-destination backoff (`ClientConfig::max_retry_backoff`),
    /// so a caller layering its own retry policy on top (`crate::sender`) can share it rather
    /// than invent a second one.
    #[must_use]
    pub fn max_retry_backoff(&self) -> Duration {
        self.config.max_retry_backoff
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

    /// Resolves `destination`, applies the IP policy, and builds the request for `method` and
    /// `path` against it: `X-Matrix` signed when `signed` is true (every federation call), bare
    /// when it is not (the legacy media fallback, [`FederationClient::get_media`]).
    async fn build_request(
        &self,
        destination: &str,
        method: &str,
        path: &str,
        body: Option<&serde_json::Value>,
        signed: bool,
    ) -> Result<reqwest::RequestBuilder, ClientError> {
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

        let mut request = client.request(
            method
                .parse()
                .map_err(|_| ClientError::Request(destination.to_string(), "bad method".into()))?,
            &url,
        );
        if signed {
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
            request = request.header(reqwest::header::AUTHORIZATION, auth_header);
        }
        if let Some(b) = body {
            request = request.json(b);
        }
        Ok(request)
    }

    async fn send_inner(
        &self,
        destination: &str,
        method: &str,
        path: &str,
        body: Option<&serde_json::Value>,
    ) -> Result<FederationResponse, ClientError> {
        let request = self
            .build_request(destination, method, path, body, true)
            .await?;

        let response = request
            .send()
            .await
            .map_err(|e| ClientError::Request(destination.to_string(), error_chain(&e)))?;
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

    /// The outbound half of `/backfill`: fetches up to `limit` PDUs walking backwards from
    /// `from_event_ids` from `destination`, reusing [`FederationClient::send`] for the signed
    /// request rather than a second X-Matrix client. The server side of this same endpoint lives
    /// in `crate::transport::read_routes`; `crate::backfill::resolve_missing_ancestors` is this
    /// method's real caller.
    ///
    /// The returned events are **not verified** -- callers must run each one through
    /// `crate::inbound::verify_pdu` before trusting anything about it, exactly as for any other
    /// inbound PDU.
    ///
    /// # Errors
    /// See [`ClientError`].
    pub async fn backfill(
        &self,
        destination: &str,
        room_id: &str,
        from_event_ids: &[String],
        limit: usize,
    ) -> Result<Vec<serde_json::Value>, ClientError> {
        let mut path = format!("/_matrix/federation/v1/backfill/{room_id}?limit={limit}");
        for id in from_event_ids {
            path.push_str("&v=");
            path.push_str(id);
        }
        let response = self.send(destination, "GET", &path, None).await?;
        if response.status / 100 != 2 {
            return Err(ClientError::Rejected {
                destination: destination.to_owned(),
                status: response.status,
                body: rejection_body(&response.body),
            });
        }
        Ok(response
            .body
            .get("pdus")
            .and_then(serde_json::Value::as_array)
            .cloned()
            .unwrap_or_default())
    }

    /// A signed `GET` of `path` on `destination` whose answer must be a `2xx`: the body, or
    /// [`ClientError::Rejected`] with what the server said.
    async fn get_ok(&self, destination: &str, path: &str) -> Result<Value, ClientError> {
        let response = self.send(destination, "GET", path, None).await?;
        if response.status / 100 != 2 {
            return Err(ClientError::Rejected {
                destination: destination.to_owned(),
                status: response.status,
                body: rejection_body(&response.body),
            });
        }
        Ok(response.body)
    }

    /// The outbound half of `GET /state_ids/{roomId}?event_id=`: the IDs of the room's state
    /// *before* `event_id` (the spec's "state at the event", which a spec-conforming server --
    /// Synapse's `get_state_ids_for_pdu`, and this one's `crate::transport::read_routes` -- does
    /// not include the event itself in), and of that state's auth chain, as
    /// `(pdu_ids, auth_chain_ids)`. Nothing is fetched beyond the IDs: what the caller does not
    /// hold it asks for with [`FederationClient::event`], or all at once with
    /// [`FederationClient::room_state`].
    ///
    /// `hs_cli::backfill` is the caller: the state at the oldest event of a backfilled batch,
    /// from which the state at every other event of the batch is derived.
    ///
    /// # Errors
    /// See [`ClientError`]; a non-`2xx` (an event the server does not know, or a room it will not
    /// show this server) is [`ClientError::Rejected`], and an answer without the two lists is
    /// [`ClientError::BadResponseJson`].
    pub async fn state_ids(
        &self,
        destination: &str,
        room_id: &str,
        event_id: &str,
    ) -> Result<(Vec<String>, Vec<String>), ClientError> {
        let path = format!("/_matrix/federation/v1/state_ids/{room_id}?event_id={event_id}");
        let body = self.get_ok(destination, &path).await?;
        let ids = |field: &str| -> Result<Vec<String>, ClientError> {
            body.get(field)
                .and_then(Value::as_array)
                .map(|ids| {
                    ids.iter()
                        .filter_map(Value::as_str)
                        .map(str::to_owned)
                        .collect()
                })
                .ok_or_else(|| {
                    ClientError::BadResponseJson(
                        destination.to_owned(),
                        format!("/state_ids answered without `{field}`"),
                    )
                })
        };
        Ok((ids("pdu_ids")?, ids("auth_chain_ids")?))
    }

    /// The outbound half of `GET /state/{roomId}?event_id=`: [`FederationClient::state_ids`]
    /// with the events themselves, as `(pdus, auth_chain)`. Heavier -- every state event of the
    /// room, whether the caller holds it or not -- so it is the fallback for when `/state_ids`
    /// fails, or when so much of the state is missing that one request beats one
    /// [`FederationClient::event`] per event.
    ///
    /// The returned events are **not verified**; the caller runs each through
    /// `crate::inbound::verify_pdu` before trusting it.
    ///
    /// # Errors
    /// See [`ClientError`] and [`FederationClient::state_ids`].
    pub async fn room_state(
        &self,
        destination: &str,
        room_id: &str,
        event_id: &str,
    ) -> Result<(Vec<Value>, Vec<Value>), ClientError> {
        let path = format!("/_matrix/federation/v1/state/{room_id}?event_id={event_id}");
        let body = self.get_ok(destination, &path).await?;
        let events = |field: &str| -> Result<Vec<Value>, ClientError> {
            body.get(field)
                .and_then(Value::as_array)
                .cloned()
                .ok_or_else(|| {
                    ClientError::BadResponseJson(
                        destination.to_owned(),
                        format!("/state answered without `{field}`"),
                    )
                })
        };
        Ok((events("pdus")?, events("auth_chain")?))
    }

    /// The outbound half of `GET /event/{eventId}`: one PDU by ID (the first of the answer's
    /// `pdus`). **Not verified**; the caller runs it through `crate::inbound::verify_pdu`, and
    /// checks it is the event it asked for.
    ///
    /// # Errors
    /// See [`ClientError`]; an answer with no PDU is [`ClientError::BadResponseJson`].
    pub async fn event(&self, destination: &str, event_id: &str) -> Result<Value, ClientError> {
        let path = format!("/_matrix/federation/v1/event/{event_id}");
        let body = self.get_ok(destination, &path).await?;
        body.get("pdus")
            .and_then(Value::as_array)
            .and_then(|pdus| pdus.first())
            .cloned()
            .ok_or_else(|| {
                ClientError::BadResponseJson(
                    destination.to_owned(),
                    "/event answered without a PDU".to_owned(),
                )
            })
    }

    /// The outbound half of `GET /hierarchy/{roomId}` (MSC2946): asks `destination` to describe
    /// `room_id` and the children it holds, for the client-server `/hierarchy` walk of a space
    /// with rooms this server does not hold. The body is handed back as it came (`room`,
    /// `children`, `inaccessible_children`); `hs_room::hierarchy::RemoteHierarchyPage` reads it.
    /// Nothing in it is trusted beyond what the summary says about itself: it decides what the
    /// requesting user is shown of a room this server cannot see, never anything in a room.
    ///
    /// # Errors
    /// See [`ClientError`]; a `404` (the room is unknown to that server, or it will not show it
    /// to this one) is [`ClientError::Rejected`].
    pub async fn room_hierarchy(
        &self,
        destination: &str,
        room_id: &str,
        suggested_only: bool,
    ) -> Result<serde_json::Value, ClientError> {
        let path =
            format!("/_matrix/federation/v1/hierarchy/{room_id}?suggested_only={suggested_only}");
        let response = self.send(destination, "GET", &path, None).await?;
        if response.status / 100 != 2 {
            return Err(ClientError::Rejected {
                destination: destination.to_owned(),
                status: response.status,
                body: rejection_body(&response.body),
            });
        }
        Ok(response.body)
    }

    /// The outbound half of `/get_missing_events`: asks `destination` for up to `limit` PDUs on
    /// the paths from `latest_events` back to, not including, `earliest_events`, none below
    /// `min_depth`. Oldest first, per the spec. Like [`FederationClient::backfill`], the returned
    /// events are **not verified**; `crate::backfill::resolve_missing_ancestors` is the caller
    /// and verifies each one.
    ///
    /// # Errors
    /// See [`ClientError`].
    pub async fn get_missing_events(
        &self,
        destination: &str,
        room_id: &str,
        earliest_events: &[String],
        latest_events: &[String],
        limit: usize,
        min_depth: i64,
    ) -> Result<Vec<serde_json::Value>, ClientError> {
        let path = format!("/_matrix/federation/v1/get_missing_events/{room_id}");
        let body = serde_json::json!({
            "earliest_events": earliest_events,
            "latest_events": latest_events,
            "limit": limit,
            "min_depth": min_depth,
        });
        let response = self.send(destination, "POST", &path, Some(&body)).await?;
        if response.status / 100 != 2 {
            return Err(ClientError::Rejected {
                destination: destination.to_owned(),
                status: response.status,
                body: rejection_body(&response.body),
            });
        }
        Ok(response
            .body
            .get("events")
            .and_then(serde_json::Value::as_array)
            .cloned()
            .unwrap_or_default())
    }

    /// A raw `GET` for media: `X-Matrix` signed for the federation media API
    /// (`/_matrix/federation/v1/media/{download,thumbnail}/...`, `signed: true`), or bare for the
    /// legacy `/_matrix/media/v3/download/...` fallback a server older than spec v1.11 answers
    /// (`signed: false`). The body is handed back as bytes with the headers `hs-media` needs to
    /// read a `multipart/mixed` answer or follow a redirect -- it is never parsed as JSON here.
    ///
    /// The same enabled, domain, backoff and IP-range checks as [`FederationClient::send`] run
    /// first, and a transport failure counts towards the destination's backoff. Unlike `send`,
    /// this does not take the per-destination concurrency permit: a download of a large file
    /// must not hold up the destination's transactions, and `hs-media` already makes one fetch
    /// per item at a time. Redirects are not followed (no federation request follows one); a
    /// `3xx` comes back as it is, `Location` included, for the caller to follow through its own
    /// SSRF guard.
    ///
    /// # Errors
    /// See [`ClientError`]; [`ClientError::ResponseTooLarge`] when the body exceeds `max_bytes`.
    pub async fn get_media(
        &self,
        destination: &str,
        path: &str,
        signed: bool,
        max_bytes: usize,
    ) -> Result<MediaResponse, ClientError> {
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

        let result = self
            .get_media_inner(destination, path, signed, max_bytes)
            .await;
        match &result {
            Ok(_) => self.destinations.record_success(destination).await,
            Err(ClientError::Request(..)) => {
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

    async fn get_media_inner(
        &self,
        destination: &str,
        path: &str,
        signed: bool,
        max_bytes: usize,
    ) -> Result<MediaResponse, ClientError> {
        let request = self
            .build_request(destination, "GET", path, None, signed)
            .await?;
        let mut response = request
            .send()
            .await
            .map_err(|e| ClientError::Request(destination.to_string(), error_chain(&e)))?;
        let status = response.status().as_u16();
        let header = |name: reqwest::header::HeaderName| {
            response
                .headers()
                .get(name)
                .and_then(|v| v.to_str().ok())
                .map(str::to_owned)
        };
        let content_type = header(reqwest::header::CONTENT_TYPE);
        let content_disposition = header(reqwest::header::CONTENT_DISPOSITION);
        let location = header(reqwest::header::LOCATION);
        if response
            .content_length()
            .is_some_and(|len| len > max_bytes as u64)
        {
            return Err(ClientError::ResponseTooLarge(destination.to_string()));
        }
        let mut body = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|e| ClientError::Request(destination.to_string(), error_chain(&e)))?
        {
            if body.len() + chunk.len() > max_bytes {
                return Err(ClientError::ResponseTooLarge(destination.to_string()));
            }
            body.extend_from_slice(&chunk);
        }
        Ok(MediaResponse {
            status,
            content_type,
            content_disposition,
            location,
            body: bytes::Bytes::from(body),
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
            // The ~140 public webpki roots are always trusted (this call never disables them);
            // whether the OS trust store is *also* trusted is the operator's explicit choice --
            // see `hs-config::FederationConfig::trust_os_root_store`'s doc comment for why the
            // default is `false`.
            .tls_built_in_native_certs(self.config.trust_os_root_store)
            // No federation request is answered by a redirect except a media download, and that
            // one must be followed through `hs-media`'s SSRF guard, not here: following it here
            // would connect wherever a peer pointed, past the IP-range policy above.
            .redirect(reqwest::redirect::Policy::none())
            .http1_only(); // HTTP/1.1-only to peers, per the recorded decision.

        for cert in &self.custom_roots {
            builder = builder.add_root_certificate(cert.clone());
        }

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

    #[tokio::test]
    async fn backfill_sends_a_signed_get_and_parses_the_pdus() {
        let peer = FakeFederationPeer::new("peer.example.org");
        peer.queue_response(hs_testkit::fake_federation::CannedResponse::ok(
            serde_json::json!({ "pdus": [{"type": "m.room.message"}] }),
        ));
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = peer.router();
        let handle = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let config = ClientConfig {
            scheme: "http",
            ..ClientConfig::default()
        };
        let client = client_for_port(addr.port(), config);

        let pdus = client
            .backfill(
                &format!("localhost:{}", addr.port()),
                "!r:example.org",
                &["$missing".to_string()],
                50,
            )
            .await
            .unwrap();
        assert_eq!(pdus, vec![serde_json::json!({"type": "m.room.message"})]);

        // The request that reached the fake peer is a real, signed GET at the exact path
        // `crate::transport::read_routes::backfill` parses (`v=`/`limit=` query params).
        let requests = peer.requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].method, "GET");
        assert!(
            requests[0]
                .path
                .starts_with("/_matrix/federation/v1/backfill/!r:example.org")
        );
        assert!(requests[0].path.contains("v=$missing"));
        assert!(requests[0].path.contains("limit=50"));
        handle.abort();
    }

    /// Terminates real TLS (`rustls` via `tokio-rustls`, a real handshake over a real loopback
    /// socket) with a certificate self-signed by a CA no one but this test knows about --
    /// deliberately not derived from any of the ~140 public roots `rustls-tls-webpki-roots`
    /// bundles. Returns `(port, cert_pem)`; the server answers exactly one plain `200 {}` per
    /// connection, then stops accepting once `stop` is dropped.
    /// Installs `ring` as the process-wide rustls provider, once.
    ///
    /// Necessary because of a difference between a scoped build and a workspace build that is
    /// easy to be caught by: `cargo test -p hs-federation` enables only the workspace's own
    /// `ring` backend and rustls picks it automatically, while `cargo test --workspace` unifies
    /// features across every crate — `hs-loadgen` pulls `matrix-sdk` with `rustls-aws-lc-rs` —
    /// so two backends are enabled at once, the automatic choice becomes ambiguous, and rustls
    /// panics. Naming the provider explicitly is what `hs_cluster::mesh::tls` already does for
    /// the same reason.
    fn install_ring_provider() {
        use std::sync::Once;
        static ONCE: Once = Once::new();
        ONCE.call_once(|| {
            // Fails only if something already installed one, which is fine for a test.
            let _ = rustls::crypto::ring::default_provider().install_default();
        });
    }

    async fn spawn_self_signed_tls_peer() -> (u16, Vec<u8>, tokio::task::JoinHandle<()>) {
        install_ring_provider();
        let rcgen::CertifiedKey { cert, signing_key } =
            rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).unwrap();
        let cert_pem = cert.pem().into_bytes();
        let cert_der = cert.der().clone();
        let key_der = rustls_pki_types::PrivateKeyDer::Pkcs8(
            rustls_pki_types::PrivatePkcs8KeyDer::from(signing_key.serialize_der()),
        );

        let server_config = rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(vec![cert_der], key_der)
            .unwrap();
        let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(server_config));

        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();

        let handle = tokio::spawn(async move {
            loop {
                let Ok((tcp, _)) = listener.accept().await else {
                    return;
                };
                let acceptor = acceptor.clone();
                tokio::spawn(async move {
                    // A handshake failure here (the exact thing the "without the CA" half of this
                    // test expects the *client* to hit) is not a server-side bug -- just drop the
                    // connection like a real TLS server would.
                    let Ok(tls) = acceptor.accept(tcp).await else {
                        return;
                    };
                    let io = hyper_util::rt::TokioIo::new(tls);
                    let service = hyper::service::service_fn(
                        |_req: hyper::Request<hyper::body::Incoming>| async {
                            Ok::<_, std::convert::Infallible>(hyper::Response::new(
                                http_body_util::Full::new(hyper::body::Bytes::from_static(b"{}")),
                            ))
                        },
                    );
                    let _ = hyper::server::conn::http1::Builder::new()
                        .serve_connection(io, service)
                        .await;
                });
            }
        });

        (port, cert_pem, handle)
    }

    /// The real proof this session's TLS/CA fix is real, per `docs/status/06-federation.md`: a
    /// self-signed certificate chaining to no public root, verified two ways against the exact
    /// same peer. Needs no Docker and no network beyond loopback.
    #[tokio::test]
    async fn outbound_tls_rejects_an_unconfigured_ca_but_trusts_a_configured_one() {
        let (port, cert_pem, server) = spawn_self_signed_tls_peer().await;
        let destination = format!("localhost:{port}");

        // Without the CA configured, this is exactly Complement's pre-fix symptom: a real, correct
        // TLS server the client has no reason to trust yet.
        let without_ca = client_for_port(port, ClientConfig::default());
        let err = without_ca
            .send(&destination, "GET", "/_matrix/federation/v1/version", None)
            .await
            .unwrap_err();
        assert!(
            matches!(err, ClientError::Request(..)),
            "expected the unconfigured client to fail the TLS handshake, got {err:?}"
        );

        // The exact same peer, the exact same certificate -- now named via
        // `custom_root_certificates` (what `hs-config`'s `custom_ca_certificates` feeds into) --
        // and the handshake succeeds.
        let with_ca = client_for_port(
            port,
            ClientConfig {
                custom_root_certificates: vec![cert_pem],
                ..ClientConfig::default()
            },
        );
        let response = with_ca
            .send(&destination, "GET", "/_matrix/federation/v1/version", None)
            .await
            .unwrap();
        assert_eq!(response.status, 200);

        server.abort();
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

    #[test]
    fn a_policy_replaced_through_one_handle_is_replaced_for_every_clone() {
        let domains = DomainPolicy::new(Some(vec!["a.example.org".to_string()]));
        let in_client = domains.clone();
        domains.set(Some(vec!["b.example.org".to_string()]));
        assert!(!in_client.allows("a.example.org"));
        assert!(in_client.allows("b.example.org"));
        domains.set(None);
        assert!(in_client.allows("anything.example.org"));

        let ips = IpPolicy::from_cidrs(&["10.0.0.0/8".to_string()], &[]);
        let in_client = ips.clone();
        let private: IpAddr = "10.1.2.3".parse().unwrap();
        assert!(!in_client.allows(private));
        ips.set_cidrs(&["10.0.0.0/8".to_string()], &["10.1.0.0/16".to_string()]);
        assert!(in_client.allows(private));
        ips.set_cidrs(&[], &[]);
        assert!(in_client.allows(private));
    }

    /// A peer with the two media paths: the federation one answers only a signed request, the
    /// legacy one answers anyone, and a third path redirects.
    async fn spawn_media_peer() -> u16 {
        use axum::http::{HeaderMap, StatusCode, header};
        async fn federation(headers: HeaderMap) -> (StatusCode, HeaderMap, &'static str) {
            let mut out = HeaderMap::new();
            if !headers
                .get(header::AUTHORIZATION)
                .and_then(|v| v.to_str().ok())
                .is_some_and(|v| v.starts_with("X-Matrix "))
            {
                return (StatusCode::UNAUTHORIZED, out, "unsigned");
            }
            out.insert(
                header::CONTENT_TYPE,
                "multipart/mixed; boundary=b".parse().unwrap(),
            );
            (
                StatusCode::OK,
                out,
                "--b\r\n\r\n{}\r\n--b\r\n\r\nhi\r\n--b--",
            )
        }
        async fn legacy() -> ([(header::HeaderName, &'static str); 2], &'static str) {
            (
                [
                    (header::CONTENT_TYPE, "text/plain"),
                    (header::CONTENT_DISPOSITION, "inline; filename=a.txt"),
                ],
                "legacy bytes",
            )
        }
        async fn moved() -> (StatusCode, [(header::HeaderName, &'static str); 1]) {
            (
                StatusCode::TEMPORARY_REDIRECT,
                [(header::LOCATION, "http://169.254.169.254/latest")],
            )
        }
        let app = axum::Router::new()
            .route(
                "/_matrix/federation/v1/media/download/abc",
                axum::routing::get(federation),
            )
            .route(
                "/_matrix/media/v3/download/{server}/abc",
                axum::routing::get(legacy),
            )
            .route("/moved", axum::routing::get(moved));
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        port
    }

    fn media_client(port: u16) -> FederationClient {
        client_for_port(
            port,
            ClientConfig {
                scheme: "http",
                ..ClientConfig::default()
            },
        )
    }

    #[tokio::test]
    async fn a_media_get_is_signed_for_the_federation_api_and_bare_for_the_legacy_one() {
        let port = spawn_media_peer().await;
        let client = media_client(port);
        let destination = format!("localhost:{port}");

        let signed = client
            .get_media(
                &destination,
                "/_matrix/federation/v1/media/download/abc",
                true,
                1024,
            )
            .await
            .unwrap();
        assert_eq!(signed.status, 200);
        assert_eq!(
            signed.content_type.as_deref(),
            Some("multipart/mixed; boundary=b")
        );
        assert!(signed.body.starts_with(b"--b"));

        let unsigned = client
            .get_media(
                &destination,
                "/_matrix/federation/v1/media/download/abc",
                false,
                1024,
            )
            .await
            .unwrap();
        assert_eq!(
            unsigned.status, 401,
            "the fake only answers a signed request"
        );

        let legacy = client
            .get_media(
                &destination,
                &format!("/_matrix/media/v3/download/{destination}/abc"),
                false,
                1024,
            )
            .await
            .unwrap();
        assert_eq!(legacy.status, 200);
        assert_eq!(&legacy.body[..], b"legacy bytes");
        assert_eq!(legacy.content_type.as_deref(), Some("text/plain"));
        assert_eq!(
            legacy.content_disposition.as_deref(),
            Some("inline; filename=a.txt")
        );
    }

    #[tokio::test]
    async fn a_media_get_caps_the_body_and_never_follows_a_redirect() {
        let port = spawn_media_peer().await;
        let client = media_client(port);
        let destination = format!("localhost:{port}");

        let err = client
            .get_media(
                &destination,
                &format!("/_matrix/media/v3/download/{destination}/abc"),
                false,
                4,
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ClientError::ResponseTooLarge(_)), "{err}");

        // Following this would connect to a link-local address the IP policy never saw.
        let moved = client
            .get_media(&destination, "/moved", true, 1024)
            .await
            .unwrap();
        assert_eq!(moved.status, 307);
        assert_eq!(
            moved.location.as_deref(),
            Some("http://169.254.169.254/latest")
        );
    }

    /// `state_ids`, `room_state` and `event` against a stand-in that answers each the way the
    /// spec shapes it, a `404` for an unknown event, and `/state_ids` without its lists.
    #[tokio::test]
    async fn state_ids_state_and_event_read_their_answers() {
        use axum::extract::{Path, Query};
        use axum::routing::get;
        let app = axum::Router::new()
            .route(
                "/_matrix/federation/v1/state_ids/{room}",
                get(
                    |Path(room): Path<String>,
                     Query(q): Query<HashMap<String, String>>| async move {
                        if room.starts_with("!broken") {
                            return axum::Json(serde_json::json!({"pdu_ids": []}));
                        }
                        let at = q.get("event_id").cloned().unwrap_or_default();
                        axum::Json(serde_json::json!({
                            "pdu_ids": ["$create", format!("{at}-before")],
                            "auth_chain_ids": ["$create"],
                        }))
                    },
                ),
            )
            .route(
                "/_matrix/federation/v1/state/{room}",
                get(|| async {
                    axum::Json(serde_json::json!({
                        "pdus": [{"type": "m.room.create"}],
                        "auth_chain": [],
                    }))
                }),
            )
            .route(
                "/_matrix/federation/v1/event/{event}",
                get(|Path(event): Path<String>| async move {
                    if event == "$unknown" {
                        return (
                            axum::http::StatusCode::NOT_FOUND,
                            axum::Json(serde_json::json!({"errcode": "M_NOT_FOUND"})),
                        );
                    }
                    (
                        axum::http::StatusCode::OK,
                        axum::Json(serde_json::json!({
                            "origin": "peer",
                            "origin_server_ts": 1,
                            "pdus": [{"type": "m.room.topic", "asked": event}],
                        })),
                    )
                }),
            );
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let client = client_for_port(
            port,
            ClientConfig {
                ip_policy: IpPolicy::default(),
                scheme: "http",
                ..ClientConfig::default()
            },
        );
        let peer = format!("localhost:{port}");

        let (state, chain) = client.state_ids(&peer, "!room:peer", "$at").await.unwrap();
        assert_eq!(state, vec!["$create".to_owned(), "$at-before".to_owned()]);
        assert_eq!(chain, vec!["$create".to_owned()]);
        assert!(matches!(
            client.state_ids(&peer, "!broken:peer", "$at").await,
            Err(ClientError::BadResponseJson(..))
        ));

        let (pdus, chain) = client.room_state(&peer, "!room:peer", "$at").await.unwrap();
        assert_eq!(pdus.len(), 1);
        assert!(chain.is_empty());

        let pdu = client.event(&peer, "$topic").await.unwrap();
        assert_eq!(pdu["asked"], "$topic");
        assert!(matches!(
            client.event(&peer, "$unknown").await,
            Err(ClientError::Rejected { status: 404, .. })
        ));
    }

    // Suppress "unused" on the unused helper import when compiled without networking pieces used
    // by every test above (kept explicit rather than silently allowed).
    #[allow(dead_code)]
    fn _touch(_a: StdSocketAddr) {}
}
