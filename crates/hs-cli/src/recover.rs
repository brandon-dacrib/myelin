//! `hs recover`: asks a running server for a one-time administrator recovery link, proving the
//! right to one by signing the request with the server's own signing key. What the link does,
//! and why the key is the credential, is in `hs_auth::recovery`; this is the client half: find
//! the key, sign the message both sides agree on, `POST /api/v1/recovery/links`, print what
//! comes back.

use std::path::{Path, PathBuf};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD_NO_PAD;
use hs_admin::model::{RecoveryLink, RecoveryLinkRequest};
use hs_model::signing::{SigningKeyPair, sign_bytes};
use rand::Rng as _;
use rand::distr::Alphanumeric;

/// The environment variable a Helm chart in cluster mode, or an operator, pins the key path
/// with; the same one `hs serve` honours.
pub const SIGNING_KEY_PATH_ENV: &str = "HS__SERVER__SIGNING_KEY_PATH";

/// Errors from asking for a link.
#[derive(Debug, thiserror::Error)]
pub enum RecoverError {
    /// The HTTP request itself failed.
    #[error(
        "could not reach {url}: {source}. Is the server running there? --server sets the address"
    )]
    Request {
        /// The URL that failed.
        url: String,
        /// The underlying error.
        #[source]
        source: reqwest::Error,
    },
    /// The server answered, and the answer was no.
    #[error("the server refused ({status}): {detail}")]
    Refused {
        /// The HTTP status.
        status: reqwest::StatusCode,
        /// The problem document's `detail`, or the raw body.
        detail: String,
    },
    /// A success status with a body that is not a link.
    #[error("unexpected response from {url}: {source}")]
    UnexpectedResponse {
        /// The URL whose response could not be parsed.
        url: String,
        /// The underlying JSON error.
        #[source]
        source: reqwest::Error,
    },
}

/// Where to look for the signing key: `--signing-key`, else [`SIGNING_KEY_PATH_ENV`], else
/// `keys/` under `--data-dir` or `HS_DATA_DIR`, else nowhere. `env` is how the environment is
/// read, so a test can supply one.
#[must_use]
pub fn signing_key_path(
    explicit: Option<&Path>,
    data_dir: Option<&Path>,
    env: impl Fn(&str) -> Option<PathBuf>,
) -> Option<PathBuf> {
    explicit
        .map(Path::to_path_buf)
        .or_else(|| env(SIGNING_KEY_PATH_ENV))
        .or_else(|| {
            data_dir
                .map(Path::to_path_buf)
                .or_else(|| env(crate::bootstrap::DATA_DIR_ENV))
                .map(|root| crate::bootstrap::DataLayout::new(&root).signing_keys)
        })
}

/// A request signed at `requested_at_ms` with `key`, over the message layout the server
/// verifies (`hs_auth::recovery::message_to_sign`), with a fresh nonce.
#[must_use]
pub fn signed_request(key: &SigningKeyPair, requested_at_ms: u64) -> RecoveryLinkRequest {
    let nonce: String = rand::rng()
        .sample_iter(Alphanumeric)
        .take(24)
        .map(char::from)
        .collect();
    let signature = sign_bytes(
        &hs_auth::recovery::message_to_sign(requested_at_ms, &nonce),
        key,
    );
    RecoveryLinkRequest {
        key_id: key.key_id(),
        requested_at_ms,
        nonce,
        signature: STANDARD_NO_PAD.encode(signature.to_bytes()),
    }
}

/// `POST {server_url}/api/v1/recovery/links`.
///
/// # Errors
/// See [`RecoverError`].
pub async fn request_link(
    client: &reqwest::Client,
    server_url: &str,
    request: &RecoveryLinkRequest,
) -> Result<RecoveryLink, RecoverError> {
    let url = format!("{}/api/v1/recovery/links", server_url.trim_end_matches('/'));
    let response = client
        .post(&url)
        .json(request)
        .send()
        .await
        .map_err(|source| RecoverError::Request {
            url: url.clone(),
            source,
        })?;
    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        let detail = serde_json::from_str::<serde_json::Value>(&body)
            .ok()
            .and_then(|v| v.get("detail")?.as_str().map(str::to_owned))
            .unwrap_or(body);
        return Err(RecoverError::Refused { status, detail });
    }
    response
        .json()
        .await
        .map_err(|source| RecoverError::UnexpectedResponse { url, source })
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    fn env(vars: &[(&str, &str)]) -> impl Fn(&str) -> Option<PathBuf> {
        let map: HashMap<String, PathBuf> = vars
            .iter()
            .map(|(k, v)| ((*k).to_owned(), PathBuf::from(v)))
            .collect();
        move |name| map.get(name).cloned()
    }

    #[test]
    fn the_key_is_looked_for_where_the_server_would_keep_it() {
        // Nothing to go on.
        assert_eq!(signing_key_path(None, None, env(&[])), None);
        // The data directory, from the flag or the variable the image sets.
        assert_eq!(
            signing_key_path(None, Some(Path::new("/data")), env(&[])),
            Some(PathBuf::from("/data/keys"))
        );
        assert_eq!(
            signing_key_path(None, None, env(&[("HS_DATA_DIR", "/data")])),
            Some(PathBuf::from("/data/keys"))
        );
        // The pinned key path wins over the data directory: a cluster-mode pod has a mounted
        // Secret and no data volume.
        assert_eq!(
            signing_key_path(
                None,
                Some(Path::new("/data")),
                env(&[(
                    "HS__SERVER__SIGNING_KEY_PATH",
                    "/etc/hs/secrets/signing-key"
                )])
            ),
            Some(PathBuf::from("/etc/hs/secrets/signing-key"))
        );
        // And the flag wins over everything.
        assert_eq!(
            signing_key_path(
                Some(Path::new("./signing.key")),
                Some(Path::new("/data")),
                env(&[(
                    "HS__SERVER__SIGNING_KEY_PATH",
                    "/etc/hs/secrets/signing-key"
                )])
            ),
            Some(PathBuf::from("./signing.key"))
        );
    }

    #[test]
    fn a_signed_request_verifies_under_the_key_that_signed_it_and_no_other() {
        use ed25519_dalek::Verifier as _;
        let key = SigningKeyPair::generate("a_test");
        let request = signed_request(&key, 1_700_000_000_000);
        assert_eq!(request.key_id, "ed25519:a_test");
        assert_eq!(request.nonce.len(), 24);
        let raw = STANDARD_NO_PAD.decode(&request.signature).unwrap();
        let signature = ed25519_dalek::Signature::from_bytes(&raw.try_into().unwrap());
        let message = hs_auth::recovery::message_to_sign(request.requested_at_ms, &request.nonce);
        assert!(key.verifying_key().verify(&message, &signature).is_ok());
        assert!(
            SigningKeyPair::generate("b_other")
                .verifying_key()
                .verify(&message, &signature)
                .is_err()
        );
        // Two requests never share a nonce.
        assert_ne!(signed_request(&key, 1).nonce, signed_request(&key, 1).nonce);
    }
}
