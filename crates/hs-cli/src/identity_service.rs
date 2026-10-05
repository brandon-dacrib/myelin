//! [`hs_room::third_party_invite::IdentityService`] over HTTPS: the identity-server calls a
//! third-party (3PID) invite makes -- `hash_details` and `lookup` (v2, sha256 or plain), `store-
//! invite`, and a key's `isvalid` -- and the `PUT /_matrix/federation/v1/3pid/onbind` route an
//! identity server calls when an invited address is bound.
//!
//! Only the identity servers `auth.identity_servers` names are contacted
//! ([`HttpIdentityService::allows`]); the list is swapped while the server runs
//! ([`HttpIdentityService::set_allowed`], from the live configuration), so the setting applies at
//! once. `key_validity_url`s come from a room's `m.room.third_party_invite` event, which a room
//! member wrote, so they are only followed to an allowed identity server too.
//!
//! Certificates are verified. `HS_TEST_INSECURE_IDENTITY_SERVER_TLS=1` in the environment turns
//! that off for test harnesses whose fake identity server has a self-signed certificate (Sytest's);
//! it is not a configuration setting and is logged loudly when set.

use std::sync::RwLock;
use std::time::Duration;

use async_trait::async_trait;
use base64::Engine;
use hs_room::RoomError;
use hs_room::third_party_invite::{IdentityService, StoredInvite};
use ruma::OwnedUserId;
use serde_json::{Value, json};
use sha2::Digest;

/// The environment variable that turns certificate verification off for identity servers. For
/// test harnesses only; see the module docs.
pub const INSECURE_TLS_ENV: &str = "HS_TEST_INSECURE_IDENTITY_SERVER_TLS";

/// How long one identity-server request may take.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);

/// The `hs serve` [`IdentityService`], and [`hs_auth::threepid::IdentityServerClient`] for
/// binding and unbinding a user's third-party identifiers. See the module docs.
pub struct HttpIdentityService {
    client: reqwest::Client,
    allowed: RwLock<Vec<String>>,
    /// This server's name and signing key, which unbinding signs its request with
    /// ([`Self::set_signer`]). Unset, an unbind goes unsigned.
    signer: RwLock<Option<(String, std::sync::Arc<hs_model::signing::SigningKeyPair>)>>,
}

impl HttpIdentityService {
    /// A client allowed to contact exactly `allowed` (`auth.identity_servers`).
    ///
    /// # Errors
    /// If the HTTP client cannot be built.
    pub fn new(allowed: Vec<String>) -> Result<Self, reqwest::Error> {
        let insecure = std::env::var(INSECURE_TLS_ENV).is_ok_and(|v| v == "1");
        if insecure {
            tracing::warn!(
                "{INSECURE_TLS_ENV}=1: identity servers' certificates are not verified; this is \
                 for test harnesses only"
            );
        }
        // The shared builder: the operating system's roots loaded once, and the outbound
        // address policy (`network.outbound.ipv4_only`, fall-back across addresses).
        let client = hs_http::client::builder()
            .timeout(REQUEST_TIMEOUT)
            .danger_accept_invalid_certs(insecure)
            .build()?;
        Ok(Self {
            client,
            allowed: RwLock::new(normalise(allowed)),
            signer: RwLock::new(None),
        })
    }

    /// Sets the name and key an unbind request is signed with (`Authorization: X-Matrix`), as
    /// Synapse signs `POST /_matrix/identity/v2/3pid/unbind` so the identity server can tell
    /// the request comes from the user's own homeserver.
    pub fn set_signer(
        &self,
        server_name: String,
        key: std::sync::Arc<hs_model::signing::SigningKeyPair>,
    ) {
        *self
            .signer
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some((server_name, key));
    }

    /// Replaces the allowed identity servers (a change to `auth.identity_servers`).
    pub fn set_allowed(&self, allowed: Vec<String>) {
        let allowed = normalise(allowed);
        tracing::info!(identity_servers = ?allowed, "the identity servers third-party invites may use are now in force");
        *self
            .allowed
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = allowed;
    }

    fn base(id_server: &str) -> String {
        format!("https://{}", id_server.trim_end_matches('/'))
    }

    async fn send(
        &self,
        request: reqwest::RequestBuilder,
        id_access_token: Option<&str>,
    ) -> Result<Value, RoomError> {
        let request = match id_access_token {
            Some(token) => request.bearer_auth(token),
            None => request,
        };
        let response = request
            .send()
            .await
            .map_err(|e| unreachable_identity_server(&e.to_string()))?;
        let status = response.status();
        let body: Value = response.json().await.unwrap_or(Value::Null);
        if !status.is_success() {
            return Err(RoomError::Forbidden(format!(
                "the identity server refused the request ({status}): {body}"
            )));
        }
        Ok(body)
    }
}

fn normalise(allowed: Vec<String>) -> Vec<String> {
    allowed
        .into_iter()
        .map(|s| s.trim().to_ascii_lowercase())
        .filter(|s| !s.is_empty())
        .collect()
}

fn unreachable_identity_server(error: &str) -> RoomError {
    RoomError::Forbidden(format!("could not reach the identity server: {error}"))
}

/// The `host[:port]` a URL names, lower-cased, if it is an `https` URL.
fn host_of(url: &str) -> Option<String> {
    let rest = url.strip_prefix("https://")?;
    let host = rest.split(['/', '?', '#']).next()?;
    (!host.is_empty()).then(|| host.to_ascii_lowercase())
}

/// Whether `id_server` (`host[:port]`) matches an allowed entry: the same `host:port`, or, for an
/// entry without a port, the same host on any port.
fn matches(allowed: &[String], id_server: &str) -> bool {
    let id_server = id_server.trim().to_ascii_lowercase();
    let host = id_server
        .rsplit_once(':')
        .filter(|(_, port)| port.chars().all(|c| c.is_ascii_digit()))
        .map_or(id_server.as_str(), |(host, _)| host);
    allowed
        .iter()
        .any(|entry| *entry == id_server || (!entry.contains(':') && entry == host))
}

/// The spec's lookup hash: unpadded URL-safe base64 of SHA-256 over `"{address} {medium}
/// {pepper}"`.
fn lookup_hash(address: &str, medium: &str, pepper: &str) -> String {
    let digest = sha2::Sha256::digest(format!("{address} {medium} {pepper}").as_bytes());
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(digest)
}

#[async_trait]
impl IdentityService for HttpIdentityService {
    fn allows(&self, id_server: &str) -> bool {
        let allowed = self
            .allowed
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        matches(&allowed, id_server)
    }

    async fn lookup(
        &self,
        id_server: &str,
        id_access_token: Option<&str>,
        medium: &str,
        address: &str,
    ) -> Result<Option<OwnedUserId>, RoomError> {
        let base = Self::base(id_server);
        let details = self
            .send(
                self.client
                    .get(format!("{base}/_matrix/identity/v2/hash_details")),
                id_access_token,
            )
            .await?;
        let pepper = details["lookup_pepper"].as_str().unwrap_or_default();
        let algorithms: Vec<&str> = details["algorithms"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .collect();
        let (algorithm, key) = if algorithms.contains(&"sha256") {
            ("sha256", lookup_hash(address, medium, pepper))
        } else if algorithms.contains(&"none") {
            ("none", format!("{address} {medium}"))
        } else {
            return Err(RoomError::Forbidden(
                "the identity server offers no lookup algorithm this server knows".into(),
            ));
        };
        let found = self
            .send(
                self.client
                    .post(format!("{base}/_matrix/identity/v2/lookup"))
                    .json(&json!({"addresses": [key], "algorithm": algorithm, "pepper": pepper})),
                id_access_token,
            )
            .await?;
        Ok(found["mappings"][&key]
            .as_str()
            .and_then(|mxid| ruma::UserId::parse(mxid).ok()))
    }

    async fn store_invite(
        &self,
        id_server: &str,
        id_access_token: Option<&str>,
        request: Value,
    ) -> Result<StoredInvite, RoomError> {
        let base = Self::base(id_server);
        let body = self
            .send(
                self.client
                    .post(format!("{base}/_matrix/identity/v2/store-invite"))
                    .json(&request),
                id_access_token,
            )
            .await?;
        let token = body["token"]
            .as_str()
            .ok_or_else(|| RoomError::Forbidden("the identity server gave no token".into()))?;
        Ok(StoredInvite {
            token: token.to_owned(),
            display_name: body["display_name"].as_str().unwrap_or_default().to_owned(),
            public_key: body["public_key"].as_str().unwrap_or_default().to_owned(),
            public_keys: body["public_keys"].as_array().cloned().unwrap_or_default(),
        })
    }

    async fn key_is_valid(
        &self,
        key_validity_url: &str,
        public_key: &str,
    ) -> Result<bool, RoomError> {
        let Some(host) = host_of(key_validity_url) else {
            return Ok(false);
        };
        if !self.allows(&host) {
            return Ok(false);
        }
        let body = self
            .send(
                self.client
                    .get(key_validity_url)
                    .query(&[("public_key", public_key)]),
                None,
            )
            .await?;
        Ok(body["valid"].as_bool().unwrap_or(false))
    }
}

/// What a call to an identity server came back with, for [`hs_auth::threepid`]'s calls.
async fn identity_server_answer(
    request: reqwest::RequestBuilder,
) -> Result<(u16, Value), hs_auth::threepid::IdentityServerError> {
    let response = request
        .send()
        .await
        .map_err(|e| hs_auth::threepid::IdentityServerError::Unreachable(e.to_string()))?;
    let status = response.status().as_u16();
    let body: Value = response.json().await.unwrap_or(Value::Null);
    Ok((status, body))
}

#[async_trait]
impl hs_auth::threepid::IdentityServerClient for HttpIdentityService {
    fn allows(&self, id_server: &str) -> bool {
        IdentityService::allows(self, id_server)
    }

    async fn bind(
        &self,
        id_server: &str,
        id_access_token: &str,
        sid: &str,
        client_secret: &str,
        mxid: &ruma::UserId,
    ) -> Result<Value, hs_auth::threepid::IdentityServerError> {
        let url = format!("{}/_matrix/identity/v2/3pid/bind", Self::base(id_server));
        let (status, body) = identity_server_answer(
            self.client
                .post(url)
                .bearer_auth(id_access_token)
                .json(&json!({"sid": sid, "client_secret": client_secret, "mxid": mxid})),
        )
        .await?;
        if (200..300).contains(&status) {
            Ok(body)
        } else {
            Err(hs_auth::threepid::IdentityServerError::Refused { status, body })
        }
    }

    async fn unbind(
        &self,
        id_server: &str,
        mxid: &ruma::UserId,
        medium: &str,
        address: &str,
    ) -> Result<hs_auth::threepid::UnbindOutcome, hs_auth::threepid::IdentityServerError> {
        const PATH: &str = "/_matrix/identity/v2/3pid/unbind";
        let content = json!({"mxid": mxid, "threepid": {"medium": medium, "address": address}});
        let mut request = self
            .client
            .post(format!("{}{PATH}", Self::base(id_server)))
            .json(&content);
        let signer = self
            .signer
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        if let Some((origin, key)) = signer {
            match hs_federation::xmatrix::sign_request(
                "POST",
                PATH,
                &origin,
                id_server,
                Some(&content),
                &key,
            ) {
                Ok(header) => request = request.header(reqwest::header::AUTHORIZATION, header),
                Err(error) => tracing::warn!(%error, "could not sign an identity server unbind"),
            }
        }
        let (status, body) = identity_server_answer(request).await?;
        match status {
            200..=299 => Ok(hs_auth::threepid::UnbindOutcome::Unbound),
            // An identity server that does not support unbinding, or does not have the binding,
            // as Synapse's `_try_unbind_threepid_with_id_server` reads these.
            400 | 404 | 501 => Ok(hs_auth::threepid::UnbindOutcome::NotSupported),
            _ => Err(hs_auth::threepid::IdentityServerError::Refused { status, body }),
        }
    }
}

/// `PUT` (the spec) or `POST` (what Sytest's and older identity servers send)
/// `/_matrix/federation/v1/3pid/onbind`: an identity server says an address with pending
/// third-party invitations has been bound. Unauthenticated, as the spec has it: each invitation
/// carries the identity server's signature, which the room's auth rules check against the keys
/// the room stored, and the keys are re-checked with the identity server before anything is sent
/// (`hs_room::third_party_invite::exchange`). `{}` when every invitation became an invite; the
/// last refusal otherwise, as Synapse answers.
pub async fn on_bind<B: hs_kv::KvBackend + 'static>(
    axum::extract::State(state): axum::extract::State<hs_room::state::RoomState<B>>,
    axum::Json(body): axum::Json<Value>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    let mxid = body["mxid"].as_str().unwrap_or_default().to_owned();
    match hs_room::third_party_invite::on_bind(&state, &body).await {
        Ok(exchanged) => {
            tracing::info!(%mxid, exchanged, "an identity server reported a bound address");
            axum::Json(json!({})).into_response()
        }
        Err(error) => {
            tracing::info!(%mxid, %error, "an identity server reported a bound address whose invitation was refused");
            error.into_response()
        }
    }
}

/// `PUT /_matrix/federation/v1/exchange_third_party_invite/{roomId}`: another server -- the
/// invitee's, which is not in the room -- hands over a bound third-party invitation one of this
/// server's users made (`hs_room::third_party_invite::on_exchange`). Behind the `X-Matrix`
/// layer (`crate::serve`); the origin is read from the verified header. `{}` once the invite is
/// made and sent to the invitee's server; the refusal otherwise (`403` from the auth rules or a
/// key the identity server no longer vouches for, `400` for a body that is not such an invite).
pub async fn on_exchange<B: hs_kv::KvBackend + 'static>(
    axum::extract::State(state): axum::extract::State<hs_room::state::RoomState<B>>,
    axum::extract::Path(room_id): axum::extract::Path<String>,
    headers: axum::http::HeaderMap,
    axum::Json(event): axum::Json<Value>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    let origin = hs_federation::xmatrix::parse_x_matrix_header(&headers)
        .map(|auth| auth.origin)
        .unwrap_or_default();
    let Ok(room_id) = ruma::RoomId::parse(room_id.as_str()) else {
        return hs_room::RoomError::BadRequest("not a room ID".into()).into_response();
    };
    match hs_room::third_party_invite::on_exchange(&state, &origin, &room_id, &event).await {
        Ok(invitee) => {
            tracing::info!(%room_id, origin, %invitee, "made the invite of a third-party invitation another server handed over");
            axum::Json(json!({})).into_response()
        }
        Err(error) => {
            tracing::info!(%room_id, origin, %error, "refused a third-party invitation another server handed over");
            error.into_response()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_entry_without_a_port_allows_any_port_of_its_host() {
        let allowed = normalise(vec!["Localhost".into(), "id.example:8090".into()]);
        assert!(matches(&allowed, "localhost:41234"));
        assert!(matches(&allowed, "localhost"));
        assert!(matches(&allowed, "id.example:8090"));
        assert!(!matches(&allowed, "id.example:443"));
        assert!(!matches(&allowed, "evil.example"));
        assert!(!matches(&[], "localhost"));
    }

    #[test]
    fn the_lookup_hash_is_the_specs() {
        // The spec's example: "alice@example.com email matrixrocks".
        assert_eq!(
            lookup_hash("alice@example.com", "email", "matrixrocks"),
            "4kenr7N9drpCJ4AfalmlGQVsOn3o2RHjkADUpXJWZUc"
        );
    }

    #[test]
    fn a_validity_url_names_its_host() {
        assert_eq!(
            host_of("https://ID.example:8090/_matrix/identity/v2/pubkey/isvalid").as_deref(),
            Some("id.example:8090")
        );
        assert_eq!(host_of("http://id.example/x"), None);
    }

    #[tokio::test]
    async fn the_allowed_list_is_swapped_while_running() {
        let service = HttpIdentityService::new(Vec::new()).unwrap();
        assert!(!service.allows("vector.im"));
        service.set_allowed(vec!["vector.im".into()]);
        assert!(service.allows("vector.im"));
        assert!(
            !service
                .key_is_valid("https://other.example/isvalid", "k")
                .await
                .unwrap()
        );
    }
}
