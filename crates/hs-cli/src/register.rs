//! `hs register`: a thin HTTP client for the shared-secret registration protocol
//! (`docs/compat/cli-shims.md`), `register_new_matrix_user`-compatible. Talks to
//! `GET`/`POST {SERVER_URL}/_synapse/admin/v1/register` using
//! `hs_compat::shared_secret::compute_mac` for the digest, so it works unmodified against either
//! Synapse or `hs serve`.
//!
//! **Known gap** (see `docs/status/12-platform-and-kubernetes.md`): as of this writing, `hs serve`
//! does not yet mount `/_synapse/admin/v1/register` — `hs-compat` ships the
//! `shared_secret` library (nonce issuance, MAC compute/verify) but no HTTP handler wired into
//! any router (`docs/compat/cli-shims.md` describes that handler as "owned jointly by this track
//! and 07... lives in hs-compat's HTTP layer", which does not exist yet). This module is built and
//! tested against the documented protocol regardless (it is what `docs/compat/cli-shims.md`
//! specifies, and the same client works unmodified once the route lands), but running it against
//! a live `hs serve` today returns 404. The end-to-end test in `tests/e2e.rs` registers its test
//! user through `hs-auth`'s own `POST /register` (`m.login.dummy` UIA) instead, which *is* wired
//! up, to prove the rest of the server boots and serves correctly without depending on the
//! missing route.

use hs_compat::shared_secret::compute_mac;
use serde::{Deserialize, Serialize};

/// Errors from the shared-secret registration client.
#[derive(Debug, thiserror::Error)]
pub enum RegisterError {
    /// The HTTP request itself failed (connection refused, timeout, TLS error, ...).
    #[error("request to {url} failed: {source}")]
    Request {
        /// The URL that failed.
        url: String,
        /// The underlying `reqwest` error.
        #[source]
        source: reqwest::Error,
    },
    /// The server returned a non-2xx response.
    #[error("server returned {status}: {body}")]
    ServerError {
        /// The HTTP status code.
        status: reqwest::StatusCode,
        /// The response body (the server's Matrix error JSON, if parseable, else raw text).
        body: String,
    },
    /// The response body was not the JSON shape expected at this step.
    #[error("unexpected response shape from {url}: {source}")]
    UnexpectedResponse {
        /// The URL whose response could not be parsed.
        url: String,
        /// The underlying JSON error.
        #[source]
        source: reqwest::Error,
    },
}

#[derive(Deserialize)]
struct NonceResponse {
    nonce: String,
}

#[derive(Serialize)]
struct RegisterRequestBody<'a> {
    nonce: &'a str,
    username: &'a str,
    password: &'a str,
    admin: bool,
    mac: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    user_type: Option<&'a str>,
}

/// The result of a successful registration, per `docs/compat/cli-shims.md`.
#[derive(Debug, Deserialize)]
pub struct RegisteredUser {
    /// The access token for the newly created account.
    pub access_token: String,
    /// The full Matrix user ID.
    pub user_id: String,
    /// The server name that created the account.
    pub home_server: String,
    /// The device ID minted for this registration, if any.
    pub device_id: Option<String>,
}

/// The parameters for one shared-secret registration, mirroring `hs register`'s CLI flags.
pub struct RegisterRequest<'a> {
    /// The target server's base URL (no trailing slash), e.g. `https://matrix.example.org`.
    pub server_url: &'a str,
    /// `auth.registration_shared_secret`.
    pub shared_secret: &'a str,
    /// Localpart or full Matrix user ID to create.
    pub username: &'a str,
    /// Plaintext password.
    pub password: &'a str,
    /// Whether the account should be a server admin.
    pub admin: bool,
    /// Optional user type (Synapse's `support`/`bot` categories).
    pub user_type: Option<&'a str>,
}

/// Runs the full two-step protocol: `GET` a nonce, compute the MAC, `POST` the registration.
///
/// # Errors
/// See [`RegisterError`].
pub async fn register(
    client: &reqwest::Client,
    req: &RegisterRequest<'_>,
) -> Result<RegisteredUser, RegisterError> {
    let get_url = format!("{}/_synapse/admin/v1/register", req.server_url);
    let nonce_response =
        client
            .get(&get_url)
            .send()
            .await
            .map_err(|source| RegisterError::Request {
                url: get_url.clone(),
                source,
            })?;
    let nonce_response = check_status(nonce_response).await?;
    let NonceResponse { nonce } =
        nonce_response
            .json()
            .await
            .map_err(|source| RegisterError::UnexpectedResponse {
                url: get_url.clone(),
                source,
            })?;

    let mac = compute_mac(
        req.shared_secret.as_bytes(),
        &nonce,
        req.username,
        req.password,
        req.admin,
        req.user_type,
    );

    let post_url = get_url;
    let body = RegisterRequestBody {
        nonce: &nonce,
        username: req.username,
        password: req.password,
        admin: req.admin,
        mac: &mac,
        user_type: req.user_type,
    };
    let response = client
        .post(&post_url)
        .json(&body)
        .send()
        .await
        .map_err(|source| RegisterError::Request {
            url: post_url.clone(),
            source,
        })?;
    let response = check_status(response).await?;
    response
        .json()
        .await
        .map_err(|source| RegisterError::UnexpectedResponse {
            url: post_url,
            source,
        })
}

async fn check_status(response: reqwest::Response) -> Result<reqwest::Response, RegisterError> {
    if response.status().is_success() {
        Ok(response)
    } else {
        let status = response.status();
        let body = response
            .text()
            .await
            .unwrap_or_else(|_| "<no body>".to_owned());
        Err(RegisterError::ServerError { status, body })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn register_request_body_serializes_without_user_type_when_absent() {
        let body = RegisterRequestBody {
            nonce: "n",
            username: "u",
            password: "p",
            admin: false,
            mac: "m",
            user_type: None,
        };
        let json = serde_json::to_value(&body).unwrap();
        assert!(json.get("user_type").is_none());
    }

    #[test]
    fn register_request_body_includes_user_type_when_present() {
        let body = RegisterRequestBody {
            nonce: "n",
            username: "u",
            password: "p",
            admin: false,
            mac: "m",
            user_type: Some("bot"),
        };
        let json = serde_json::to_value(&body).unwrap();
        assert_eq!(json["user_type"], "bot");
    }
}
