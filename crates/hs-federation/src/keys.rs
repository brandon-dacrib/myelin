//! Our own signing keys, the `/_matrix/key/v2/server` response, the notary endpoints, and a
//! cache of other servers' keys fetched (directly or via a notary) over federation.
//!
//! Written from `docs/design/06-federation-threat-model.md` section 2.2 and the plan in
//! `docs/status/06-federation.md` (item 3). The two halves of this module are independent:
//! [`OwnSigningKeys`] is about keys *we* hold and publish; [`RemoteKeyCache`] is about keys
//! *other servers* publish that we have to verify before trusting.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use ed25519_dalek::VerifyingKey;
use hs_model::canonical::CanonicalJsonValue;
use hs_model::signing::{self, ALGORITHM, SigningKeyPair};
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex as AsyncMutex;

use crate::error::FederationError;

/// Max bytes read from a `/_matrix/key/v2/server` (or notary) response body (threat model
/// section 3: 64 KiB).
pub const MAX_KEY_RESPONSE_BODY_BYTES: usize = 64 * 1024;

/// How long our own published key response asserts itself valid for before a fetcher must
/// refetch. Chosen conservatively (24h); operators rotate keys far less often than this.
pub const OWN_KEY_VALID_FOR_SECS: u64 = 24 * 60 * 60;

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

// -------------------------------------------------------------------------------------------
// Our own keys
// -------------------------------------------------------------------------------------------

/// One line of a Synapse-shaped signing-key file: `<algorithm> <version> <base64 seed>`, e.g.
/// `ed25519 a_1 Wq4DFbo3zL5qb...`. Only `ed25519` is understood; other algorithms are skipped
/// (forward compatibility with a future key type this server does not yet support signing with,
/// rather than a hard parse error that would prevent boot). `ed25519_dalek::SigningKey::to_bytes`/
/// `from_bytes` round-trip exactly the 32-byte seed (not the expanded key), which is what makes
/// this format loss-free.
fn parse_signing_key_line(line: &str) -> Option<SigningKeyPair> {
    let line = line.trim();
    if line.is_empty() || line.starts_with('#') {
        return None;
    }
    let mut parts = line.split_whitespace();
    let algorithm = parts.next()?;
    let version = parts.next()?;
    let seed_b64 = parts.next()?;
    if algorithm != ALGORITHM {
        return None;
    }
    use base64::Engine as _;
    let seed_bytes = base64::engine::general_purpose::STANDARD_NO_PAD
        .decode(seed_b64)
        .or_else(|_| base64::engine::general_purpose::STANDARD.decode(seed_b64))
        .ok()?;
    let seed: [u8; 32] = seed_bytes.try_into().ok()?;
    let signing_key = ed25519_dalek::SigningKey::from_bytes(&seed);
    Some(SigningKeyPair::new(version.to_string(), signing_key))
}

/// Formats a freshly generated `(version, seed)` pair into the one-line Synapse-shaped format
/// used to persist it. Only usable at the point of generation, where the seed is still directly
/// in hand — [`SigningKeyPair`] itself does not expose it once constructed (by design, to
/// discourage exporting private key material outside this narrow use).
fn format_signing_key_line(version: &str, seed: &[u8; 32]) -> String {
    use base64::Engine as _;
    let seed_b64 = base64::engine::general_purpose::STANDARD_NO_PAD.encode(seed);
    format!("{ALGORITHM} {version} {seed_b64}")
}

/// This server's own Ed25519 signing keys, loaded from (or generated into) a directory on disk.
///
/// Every key in [`OwnSigningKeys::all`] is published in `verify_keys` and used to sign outbound
/// requests/events/key responses (the spec permits, and this implementation performs, signing
/// with every currently active key — not just one "current" key — so that a request cannot be
/// used to determine which key an observer should treat as primary). During normal operation a
/// deployment has exactly one active key; multiple keys are only expected transiently during a
/// deliberate rotation.
pub struct OwnSigningKeys {
    keys: Vec<SigningKeyPair>,
}

impl OwnSigningKeys {
    /// Loads every `ed25519 <version> <seed>` line from every regular file directly inside `dir`
    /// (order: directory-listing order, which is not guaranteed sorted — callers that care about
    /// a deterministic "primary" key should not rely on ordering; every key is treated as equally
    /// authoritative for verification, see the struct doc). If `dir` does not exist or contains
    /// no parseable key, a fresh key is generated and written to `dir/signing.key` (creating
    /// `dir` if necessary).
    ///
    /// # Errors
    /// Returns [`FederationError::Io`] if `dir` cannot be created/read/written, or
    /// [`FederationError::Key`] if a key file could not be written.
    pub fn load_or_generate(dir: &Path) -> Result<Self, FederationError> {
        let mut keys = Vec::new();
        if dir.is_dir() {
            let mut entries: Vec<_> = std::fs::read_dir(dir)?.filter_map(Result::ok).collect();
            entries.sort_by_key(|e| e.file_name());
            for entry in entries {
                if !entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
                    continue;
                }
                let Ok(contents) = std::fs::read_to_string(entry.path()) else {
                    continue;
                };
                for line in contents.lines() {
                    if let Some(pair) = parse_signing_key_line(line) {
                        keys.push(pair);
                    }
                }
            }
        }

        if keys.is_empty() {
            std::fs::create_dir_all(dir)?;
            let mut seed = [0u8; 32];
            use rand_core::RngCore as _;
            rand_core::OsRng.fill_bytes(&mut seed);
            // The version must identify this key *material*, not merely when it was made: a
            // millisecond timestamp alone collides whenever two keys are generated in the same
            // millisecond, and a key ID shared by two different keys breaks the one thing a key ID
            // is for. A remote then caches whichever key it saw first under that ID and verifies
            // the other server's signatures against it, and `old_verify_keys` expiry stops working
            // entirely, because a lookup finds the still-valid current key under the same ID and
            // never consults the expired entry. Deriving the suffix from the public key makes a
            // collision mean an actual key collision. The timestamp stays, ahead of it, so key IDs
            // still sort into rotation order.
            let signing_key_for_id = ed25519_dalek::SigningKey::from_bytes(&seed);
            use base64::Engine as _;
            let fingerprint = base64::engine::general_purpose::STANDARD_NO_PAD
                .encode(&signing_key_for_id.verifying_key().to_bytes()[..6])
                .replace(['+', '/'], "_");
            let version = format!("a_{}_{fingerprint}", now_ms());
            let signing_key = ed25519_dalek::SigningKey::from_bytes(&seed);
            let pair = SigningKeyPair::new(version.clone(), signing_key);
            let line = format_signing_key_line(&version, &seed);
            std::fs::write(dir.join("signing.key"), format!("{line}\n"))?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = std::fs::set_permissions(
                    dir.join("signing.key"),
                    std::fs::Permissions::from_mode(0o600),
                );
            }
            keys.push(pair);
        }

        Ok(Self { keys })
    }

    /// Wraps already-constructed keys directly (for tests, and for callers that generate keys
    /// through some other path than [`OwnSigningKeys::load_or_generate`]).
    #[must_use]
    pub fn from_keys(keys: Vec<SigningKeyPair>) -> Self {
        Self { keys }
    }

    /// Every active signing key.
    #[must_use]
    pub fn all(&self) -> &[SigningKeyPair] {
        &self.keys
    }

    /// The key used to sign new outbound material when exactly one is needed (the request-
    /// signing path signs with one key, per the spec's `X-Matrix` header carrying a single
    /// `key=`). Picks the lexicographically greatest version string, which is `a_<unix_ms>` for
    /// generated keys and therefore newest-first in practice.
    #[must_use]
    pub fn primary(&self) -> &SigningKeyPair {
        self.keys
            .iter()
            .max_by_key(|k| k.version().to_string())
            .expect("OwnSigningKeys always holds at least one key")
    }

    /// The `verify_keys` object of a `/_matrix/key/v2/server` response: `{key_id: {"key": b64}}`.
    #[must_use]
    pub fn verify_keys_json(&self) -> serde_json::Map<String, serde_json::Value> {
        self.keys
            .iter()
            .map(|k| {
                (
                    k.key_id(),
                    serde_json::json!({ "key": k.verifying_key_base64() }),
                )
            })
            .collect()
    }
}

// -------------------------------------------------------------------------------------------
// The `/_matrix/key/v2/server` response
// -------------------------------------------------------------------------------------------

/// One entry of `old_verify_keys`: a key this server used to sign with but no longer does,
/// retained so signatures made while it was active can still be checked.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OldVerifyKey {
    pub key_id: String,
    /// Base64 (standard, unpadded) public key.
    pub key: String,
    /// When this key was replaced — milliseconds since the Unix epoch. A signature timestamped
    /// at or before this may still be verified against this key; the key must never be treated as
    /// *currently* valid for anything past this point (threat model 2.2's downgrade defence).
    pub expired_ts: u64,
}

/// Builds and self-signs a `/_matrix/key/v2/server` response body.
///
/// # Errors
/// Returns [`FederationError::Key`] if signing fails (canonicalization of a well-formed object
/// built entirely by this function should never fail in practice).
pub fn build_server_key_response(
    server_name: &str,
    own_keys: &OwnSigningKeys,
    old_keys: &[OldVerifyKey],
    valid_for_secs: u64,
) -> Result<serde_json::Value, FederationError> {
    let old_verify_keys: serde_json::Map<String, serde_json::Value> = old_keys
        .iter()
        .map(|k| {
            (
                k.key_id.clone(),
                serde_json::json!({ "key": k.key, "expired_ts": k.expired_ts }),
            )
        })
        .collect();

    let body = serde_json::json!({
        "server_name": server_name,
        "verify_keys": own_keys.verify_keys_json(),
        "old_verify_keys": old_verify_keys,
        "valid_until_ts": now_ms() + valid_for_secs * 1000,
    });

    let mut object =
        signing::to_signable_object(&body).map_err(|e| FederationError::Key(e.to_string()))?;
    let server_name_ruma = ruma::ServerName::parse(server_name)
        .map_err(|e| FederationError::Key(format!("invalid own server_name: {e}")))?;
    for key in own_keys.all() {
        signing::sign_object(&mut object, server_name_ruma.as_ref(), key)
            .map_err(|e| FederationError::Key(e.to_string()))?;
    }
    let bytes = CanonicalJsonValue::Object(object).to_canonical_bytes();
    Ok(serde_json::from_slice(&bytes)?)
}

/// The notary role: adds our own signature to an already-self-signed key response fetched (or
/// held) for some other server, without touching its content. Per the spec, a notary vouches for
/// a response by co-signing it, not by re-minting it — the origin server's own self-signature
/// (already present in `doc`) is left exactly as-is.
///
/// # Errors
/// Returns [`FederationError::Key`] if `doc` is not a JSON object or cannot be canonicalized.
pub fn wrap_for_notary(
    doc: &serde_json::Value,
    own_server_name: &str,
    own_keys: &OwnSigningKeys,
) -> Result<serde_json::Value, FederationError> {
    let mut object =
        signing::to_signable_object(doc).map_err(|e| FederationError::Key(e.to_string()))?;
    let server_name_ruma = ruma::ServerName::parse(own_server_name)
        .map_err(|e| FederationError::Key(format!("invalid own server_name: {e}")))?;
    signing::sign_object(&mut object, server_name_ruma.as_ref(), own_keys.primary())
        .map_err(|e| FederationError::Key(e.to_string()))?;
    let bytes = CanonicalJsonValue::Object(object).to_canonical_bytes();
    Ok(serde_json::from_slice(&bytes)?)
}

// -------------------------------------------------------------------------------------------
// Fetching and caching other servers' keys
// -------------------------------------------------------------------------------------------

/// Fetches a server's own `/_matrix/key/v2/server` response, as raw parsed JSON (not yet
/// verified — [`RemoteKeyCache`] does that). `None` means the fetch failed (network error,
/// non-2xx, oversized/malformed body); the cache treats that as "could not refresh", not as "this
/// server has no keys".
#[async_trait]
pub trait KeyServerFetcher: Send + Sync {
    async fn fetch_server_key(&self, server_name: &str) -> Option<serde_json::Value>;
}

#[async_trait]
impl KeyServerFetcher for Box<dyn KeyServerFetcher> {
    async fn fetch_server_key(&self, server_name: &str) -> Option<serde_json::Value> {
        (**self).fetch_server_key(server_name).await
    }
}

/// A [`RemoteKeyCache`] over a boxed, dynamically-dispatched fetcher — the concrete type used
/// wherever the cache is stored alongside other crate-wide state (`crate::xmatrix`,
/// `crate::client`) without infecting every consumer with `KeyServerFetcher`'s type parameter.
pub type DynRemoteKeyCache = RemoteKeyCache<Box<dyn KeyServerFetcher>>;

/// A cached, currently-trusted verify key, with the response's own claimed validity window.
#[derive(Debug, Clone)]
struct CachedCurrent {
    verifying_key: VerifyingKey,
    valid_until_ts: u64,
}

/// A cached "used to be valid until" key, kept only for retrospective verification (threat model
/// 2.2's explicit old-verify-keys rule: never usable to assert currency).
#[derive(Debug, Clone)]
struct CachedOld {
    verifying_key: VerifyingKey,
    expired_ts: u64,
}

/// Why a verification lookup against the cache failed, distinguishing "never heard of this key"
/// (worth a fetch) from "definitely expired" (fetching again will not help).
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum KeyLookupError {
    #[error("no verify key `{key_id}` known for server `{server_name}`")]
    Unknown { server_name: String, key_id: String },
    #[error("verify key `{key_id}` for server `{server_name}` was not valid at the required time")]
    Expired { server_name: String, key_id: String },
    #[error("could not fetch keys for server `{0}`")]
    FetchFailed(String),
    #[error("fetched key response for `{0}` failed self-signature verification")]
    InvalidResponse(String),
}

/// Caches other servers' verify keys, fetched (and self-signature-checked) on demand, with
/// per-origin in-flight de-duplication (threat model 2.3's confused-deputy defence: N concurrent
/// callers asking about the same unknown origin trigger exactly one fetch).
pub struct RemoteKeyCache<F: KeyServerFetcher> {
    fetcher: F,
    current: std::sync::Mutex<HashMap<(String, String), CachedCurrent>>,
    old: std::sync::Mutex<HashMap<(String, String), CachedOld>>,
    in_flight: std::sync::Mutex<HashMap<String, Arc<AsyncMutex<()>>>>,
}

impl<F: KeyServerFetcher> RemoteKeyCache<F> {
    #[must_use]
    pub fn new(fetcher: F) -> Self {
        Self {
            fetcher,
            current: std::sync::Mutex::new(HashMap::new()),
            old: std::sync::Mutex::new(HashMap::new()),
            in_flight: std::sync::Mutex::new(HashMap::new()),
        }
    }

    /// Returns a verifying key usable *right now* for `server_name`/`key_id`, fetching (with
    /// in-flight de-duplication) if not already cached and unexpired.
    ///
    /// # Errors
    /// See [`KeyLookupError`].
    pub async fn get_current(
        &self,
        server_name: &str,
        key_id: &str,
    ) -> Result<VerifyingKey, KeyLookupError> {
        if let Some(key) = self.cached_current(server_name, key_id) {
            return Ok(key.verifying_key);
        }
        self.refresh(server_name).await?;
        self.cached_current(server_name, key_id)
            .map(|k| k.verifying_key)
            .ok_or_else(|| KeyLookupError::Unknown {
                server_name: server_name.to_string(),
                key_id: key_id.to_string(),
            })
    }

    /// Returns a verifying key usable to verify something signed *at* `signed_at_ts`
    /// (milliseconds since the epoch) — accepts a key that was current at that time even if it
    /// has since rotated out (checked against `old_verify_keys`' `expired_ts`), and rejects a key
    /// that had already expired by then. Fetches (with de-duplication) if nothing cached covers
    /// the requested timestamp.
    ///
    /// # Errors
    /// See [`KeyLookupError`].
    pub async fn get_valid_at(
        &self,
        server_name: &str,
        key_id: &str,
        signed_at_ts: u64,
    ) -> Result<VerifyingKey, KeyLookupError> {
        if let Some(key) = self.cached_valid_at(server_name, key_id, signed_at_ts) {
            return Ok(key);
        }
        self.refresh(server_name).await?;
        self.cached_valid_at(server_name, key_id, signed_at_ts)
            .ok_or_else(|| {
                // Distinguish "we have never heard of this key" from "we have heard of it and it
                // does not cover this timestamp" for a clearer error, matching the plan's
                // "expired key rejected" test intent.
                let known_but_wrong_time = self
                    .current
                    .lock()
                    .unwrap()
                    .contains_key(&(server_name.to_string(), key_id.to_string()))
                    || self
                        .old
                        .lock()
                        .unwrap()
                        .contains_key(&(server_name.to_string(), key_id.to_string()));
                if known_but_wrong_time {
                    KeyLookupError::Expired {
                        server_name: server_name.to_string(),
                        key_id: key_id.to_string(),
                    }
                } else {
                    KeyLookupError::Unknown {
                        server_name: server_name.to_string(),
                        key_id: key_id.to_string(),
                    }
                }
            })
    }

    fn cached_current(&self, server_name: &str, key_id: &str) -> Option<CachedCurrent> {
        let now = now_ms();
        let map = self.current.lock().unwrap();
        map.get(&(server_name.to_string(), key_id.to_string()))
            .filter(|c| now < c.valid_until_ts)
            .cloned()
    }

    fn cached_valid_at(&self, server_name: &str, key_id: &str, ts: u64) -> Option<VerifyingKey> {
        let key = (server_name.to_string(), key_id.to_string());
        if let Some(c) = self.current.lock().unwrap().get(&key)
            && ts <= c.valid_until_ts
        {
            return Some(c.verifying_key);
        }
        if let Some(c) = self.old.lock().unwrap().get(&key)
            && ts <= c.expired_ts
        {
            return Some(c.verifying_key);
        }
        None
    }

    /// Fetches (de-duplicated per `server_name`) and ingests a fresh key response.
    async fn refresh(&self, server_name: &str) -> Result<(), KeyLookupError> {
        let lock = {
            let mut in_flight = self.in_flight.lock().unwrap();
            in_flight
                .entry(server_name.to_string())
                .or_insert_with(|| Arc::new(AsyncMutex::new(())))
                .clone()
        };
        let _guard = lock.lock().await;
        // Another caller may have already populated the cache while we waited for the lock; the
        // public getters re-check the cache after calling `refresh`, so an early return here is
        // safe and just avoids one redundant fetch.
        let doc = self
            .fetcher
            .fetch_server_key(server_name)
            .await
            .ok_or_else(|| KeyLookupError::FetchFailed(server_name.to_string()))?;
        self.ingest_response(server_name, &doc)
    }

    /// Verifies a fetched key response is validly self-signed by a key it itself claims, then
    /// caches every key it lists — scoped strictly to `expected_server_name`, so a response
    /// cannot inject keys under any other server's name (the "no key substitution" defence).
    ///
    /// `pub` (not just called internally from [`RemoteKeyCache::refresh`]) so it is directly
    /// fuzzable against arbitrary, hostile JSON without needing a live fetcher to drive it — see
    /// `fuzz/fuzz_targets/key_server_response_parse.rs`.
    ///
    /// # Errors
    /// See [`KeyLookupError`].
    pub fn ingest_response(
        &self,
        expected_server_name: &str,
        doc: &serde_json::Value,
    ) -> Result<(), KeyLookupError> {
        let object = signing::to_signable_object(doc)
            .map_err(|_| KeyLookupError::InvalidResponse(expected_server_name.to_string()))?;

        let claimed_server_name = object
            .get("server_name")
            .and_then(CanonicalJsonValue::as_str);
        if claimed_server_name != Some(expected_server_name) {
            return Err(KeyLookupError::InvalidResponse(
                expected_server_name.to_string(),
            ));
        }

        let valid_until_ts = object
            .get("valid_until_ts")
            .and_then(|v| match v {
                CanonicalJsonValue::Integer(i) => u64::try_from(*i).ok(),
                _ => None,
            })
            .ok_or_else(|| KeyLookupError::InvalidResponse(expected_server_name.to_string()))?;

        let verify_keys = object
            .get("verify_keys")
            .and_then(CanonicalJsonValue::as_object)
            .ok_or_else(|| KeyLookupError::InvalidResponse(expected_server_name.to_string()))?;

        // Build the candidate verifying keys from the document's own claims, then require at
        // least one signature (claimed under `expected_server_name`) to verify against one of
        // them — self-signed-by-a-key-it-lists, per the threat model.
        let mut candidates: HashMap<String, VerifyingKey> = HashMap::new();
        for (key_id, value) in verify_keys {
            let Some(key_b64) = value
                .as_object()
                .and_then(|o| o.get("key"))
                .and_then(CanonicalJsonValue::as_str)
            else {
                continue;
            };
            if let Ok(vk) = signing::verifying_key_from_base64(key_b64) {
                candidates.insert(key_id.clone(), vk);
            }
        }

        let mut any_verified = false;
        for (key_id, vk) in &candidates {
            if signing::verify_object(&object, expected_server_name, key_id, vk).is_ok() {
                any_verified = true;
                break;
            }
        }
        if !any_verified {
            return Err(KeyLookupError::InvalidResponse(
                expected_server_name.to_string(),
            ));
        }

        // Only cache as "current" if the response has not already expired; an expired response
        // is not evidence of anything current, but see below — its keys are still recorded into
        // the retrospective (`old`) cache so already-known validity windows are not lost.
        let now = now_ms();
        {
            let mut current = self.current.lock().unwrap();
            for (key_id, vk) in &candidates {
                if now < valid_until_ts {
                    current.insert(
                        (expected_server_name.to_string(), key_id.clone()),
                        CachedCurrent {
                            verifying_key: *vk,
                            valid_until_ts,
                        },
                    );
                }
            }
        }

        if let Some(old_verify_keys) = object
            .get("old_verify_keys")
            .and_then(CanonicalJsonValue::as_object)
        {
            let mut old = self.old.lock().unwrap();
            for (key_id, value) in old_verify_keys {
                let Some(obj) = value.as_object() else {
                    continue;
                };
                let Some(key_b64) = obj.get("key").and_then(CanonicalJsonValue::as_str) else {
                    continue;
                };
                let Some(expired_ts) = obj.get("expired_ts").and_then(|v| match v {
                    CanonicalJsonValue::Integer(i) => u64::try_from(*i).ok(),
                    _ => None,
                }) else {
                    continue;
                };
                if let Ok(vk) = signing::verifying_key_from_base64(key_b64) {
                    old.insert(
                        (expected_server_name.to_string(), key_id.clone()),
                        CachedOld {
                            verifying_key: vk,
                            expired_ts,
                        },
                    );
                }
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex as StdMutex;

    fn tempdir() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    #[test]
    fn generates_a_key_on_first_boot_into_an_empty_directory() {
        let dir = tempdir();
        let keys = OwnSigningKeys::load_or_generate(dir.path()).unwrap();
        assert_eq!(keys.all().len(), 1);
        assert!(dir.path().join("signing.key").exists());
    }

    #[test]
    fn reloads_the_same_key_on_a_second_boot() {
        let dir = tempdir();
        let first = OwnSigningKeys::load_or_generate(dir.path()).unwrap();
        let first_id = first.primary().key_id();
        let first_pub = first.primary().verifying_key_base64();

        let second = OwnSigningKeys::load_or_generate(dir.path()).unwrap();
        assert_eq!(second.all().len(), 1);
        assert_eq!(second.primary().key_id(), first_id);
        assert_eq!(second.primary().verifying_key_base64(), first_pub);
    }

    #[test]
    fn loads_multiple_keys_from_separate_files_for_rotation() {
        let dir = tempdir();
        // Deliberately malformed line (not a valid base64-encoded 32-byte seed): must be skipped,
        // not fatal.
        std::fs::write(
            dir.path().join("bad.key"),
            "ed25519 a_1 not-valid-base64!!\n",
        )
        .unwrap();

        let sk = ed25519_dalek::SigningKey::from_bytes(&[7u8; 32]);
        let line = format_signing_key_line("a_2", &sk.to_bytes());
        std::fs::write(dir.path().join("current.key"), format!("{line}\n")).unwrap();

        let keys = OwnSigningKeys::load_or_generate(dir.path()).unwrap();
        // Only the well-formed key loads; the malformed line is skipped, not fatal.
        assert_eq!(keys.all().len(), 1);
        assert!(keys.all().iter().any(|k| k.key_id() == "ed25519:a_2"));
    }

    #[test]
    fn own_key_response_is_self_signed_and_verifies() {
        let dir = tempdir();
        let keys = OwnSigningKeys::load_or_generate(dir.path()).unwrap();
        let response =
            build_server_key_response("example.org", &keys, &[], OWN_KEY_VALID_FOR_SECS).unwrap();

        let object = signing::to_signable_object(&response).unwrap();
        signing::verify_object(
            &object,
            "example.org",
            &keys.primary().key_id(),
            &keys.primary().verifying_key(),
        )
        .unwrap();
    }

    #[test]
    fn own_key_response_carries_old_verify_keys() {
        let dir = tempdir();
        let keys = OwnSigningKeys::load_or_generate(dir.path()).unwrap();
        let old = OldVerifyKey {
            key_id: "ed25519:old1".into(),
            key: "aGVsbG8td29ybGQtcGxhY2Vob2xkZXItMzJieXRlcyE".into(),
            expired_ts: 12345,
        };
        let response =
            build_server_key_response("example.org", &keys, &[old], OWN_KEY_VALID_FOR_SECS)
                .unwrap();
        assert!(response["old_verify_keys"]["ed25519:old1"]["expired_ts"] == 12345);
    }

    #[test]
    fn notary_wrapping_adds_a_second_signature_without_touching_the_first() {
        let origin_dir = tempdir();
        let origin_keys = OwnSigningKeys::load_or_generate(origin_dir.path()).unwrap();
        let origin_response =
            build_server_key_response("origin.example.org", &origin_keys, &[], 3600).unwrap();

        let notary_dir = tempdir();
        let notary_keys = OwnSigningKeys::load_or_generate(notary_dir.path()).unwrap();
        let wrapped =
            wrap_for_notary(&origin_response, "notary.example.org", &notary_keys).unwrap();

        let object = signing::to_signable_object(&wrapped).unwrap();
        // Origin's own signature is still present and still verifies.
        signing::verify_object(
            &object,
            "origin.example.org",
            &origin_keys.primary().key_id(),
            &origin_keys.primary().verifying_key(),
        )
        .unwrap();
        // Notary's signature is present too.
        signing::verify_object(
            &object,
            "notary.example.org",
            &notary_keys.primary().key_id(),
            &notary_keys.primary().verifying_key(),
        )
        .unwrap();
    }

    // --- RemoteKeyCache -------------------------------------------------------------------

    struct FixedFetcher {
        responses: StdMutex<HashMap<String, serde_json::Value>>,
        fetch_count: StdMutex<HashMap<String, usize>>,
    }

    impl FixedFetcher {
        fn new() -> Self {
            Self {
                responses: StdMutex::new(HashMap::new()),
                fetch_count: StdMutex::new(HashMap::new()),
            }
        }
        fn set(&self, server: &str, doc: serde_json::Value) {
            self.responses
                .lock()
                .unwrap()
                .insert(server.to_string(), doc);
        }
        fn count_for(&self, server: &str) -> usize {
            *self.fetch_count.lock().unwrap().get(server).unwrap_or(&0)
        }
    }

    #[async_trait]
    impl KeyServerFetcher for FixedFetcher {
        async fn fetch_server_key(&self, server_name: &str) -> Option<serde_json::Value> {
            *self
                .fetch_count
                .lock()
                .unwrap()
                .entry(server_name.to_string())
                .or_insert(0) += 1;
            self.responses.lock().unwrap().get(server_name).cloned()
        }
    }

    fn signed_response(
        server_name: &str,
        valid_for_secs: u64,
    ) -> (serde_json::Value, OwnSigningKeys) {
        let dir = tempfile::tempdir().unwrap();
        let keys = OwnSigningKeys::load_or_generate(dir.path()).unwrap();
        let response = build_server_key_response(server_name, &keys, &[], valid_for_secs).unwrap();
        (response, keys)
    }

    #[tokio::test]
    async fn fetches_and_caches_a_valid_current_key() {
        let fetcher = FixedFetcher::new();
        let (doc, keys) = signed_response("remote.example.org", 3600);
        fetcher.set("remote.example.org", doc);
        let cache = RemoteKeyCache::new(fetcher);

        let key = cache
            .get_current("remote.example.org", &keys.primary().key_id())
            .await
            .unwrap();
        assert_eq!(key, keys.primary().verifying_key());
    }

    #[tokio::test]
    async fn a_key_valid_when_an_event_was_signed_is_still_accepted_for_that_event() {
        let fetcher = FixedFetcher::new();
        // Build a response whose validity already ended in the past, but whose key we still want
        // to accept for something signed while it was live: model this via old_verify_keys,
        // which is exactly the mechanism the spec provides for this case.
        let dir = tempfile::tempdir().unwrap();
        let keys = OwnSigningKeys::load_or_generate(dir.path()).unwrap();
        let old = OldVerifyKey {
            key_id: keys.primary().key_id(),
            key: keys.primary().verifying_key_base64(),
            expired_ts: 1_000_000,
        };
        // The "current" response now uses a *different* key (simulating rotation); old_verify_keys
        // carries the previous one.
        let new_dir = tempfile::tempdir().unwrap();
        let new_keys = OwnSigningKeys::load_or_generate(new_dir.path()).unwrap();
        let doc = build_server_key_response("remote.example.org", &new_keys, &[old], 3_600_000_000)
            .unwrap();
        fetcher.set("remote.example.org", doc);
        let cache = RemoteKeyCache::new(fetcher);

        // Signed at ts before expiry: accepted.
        let ok = cache
            .get_valid_at("remote.example.org", &keys.primary().key_id(), 999_999)
            .await;
        assert!(ok.is_ok(), "{ok:?}");

        // Signed at ts after expiry: rejected.
        let err = cache
            .get_valid_at("remote.example.org", &keys.primary().key_id(), 1_000_001)
            .await
            .unwrap_err();
        assert_eq!(
            err,
            KeyLookupError::Expired {
                server_name: "remote.example.org".to_string(),
                key_id: keys.primary().key_id(),
            }
        );
    }

    #[tokio::test]
    async fn unknown_key_id_is_rejected() {
        let fetcher = FixedFetcher::new();
        let (doc, _keys) = signed_response("remote.example.org", 3600);
        fetcher.set("remote.example.org", doc);
        let cache = RemoteKeyCache::new(fetcher);

        let err = cache
            .get_current("remote.example.org", "ed25519:nope")
            .await
            .unwrap_err();
        assert!(matches!(err, KeyLookupError::Unknown { .. }));
    }

    #[tokio::test]
    async fn a_server_cannot_substitute_keys_for_another_server() {
        let fetcher = FixedFetcher::new();
        let (doc_a, keys_a) = signed_response("a.example.org", 3600);
        fetcher.set("a.example.org", doc_a);
        let cache = RemoteKeyCache::new(fetcher);

        // Prime the cache for server A.
        cache
            .get_current("a.example.org", &keys_a.primary().key_id())
            .await
            .unwrap();

        // The same key_id under a *different* claimed server name must not be found — the cache
        // is keyed by (server_name, key_id), and nothing populated b.example.org's entry.
        let err = cache
            .get_current("b.example.org", &keys_a.primary().key_id())
            .await
            .unwrap_err();
        assert!(matches!(
            err,
            KeyLookupError::FetchFailed(_) | KeyLookupError::Unknown { .. }
        ));
    }

    #[tokio::test]
    async fn a_response_claiming_the_wrong_server_name_is_rejected() {
        let fetcher = FixedFetcher::new();
        // Signed as "a.example.org" but registered under "b.example.org" — the fetcher lies about
        // which server it belongs to (simulating a compromised/malicious intermediary).
        let (doc_a, _keys_a) = signed_response("a.example.org", 3600);
        fetcher.set("b.example.org", doc_a);
        let cache = RemoteKeyCache::new(fetcher);

        let err = cache
            .get_current("b.example.org", "ed25519:whatever")
            .await
            .unwrap_err();
        assert!(matches!(err, KeyLookupError::InvalidResponse(_)));
    }

    #[tokio::test]
    async fn tampered_response_fails_self_signature_check() {
        let fetcher = FixedFetcher::new();
        let (mut doc, _keys) = signed_response("remote.example.org", 3600);
        // Tamper with a verify key's claimed public key after signing.
        doc["verify_keys"]
            .as_object_mut()
            .unwrap()
            .values_mut()
            .next()
            .unwrap()["key"] =
            serde_json::Value::String("AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA".to_string());
        fetcher.set("remote.example.org", doc);
        let cache = RemoteKeyCache::new(fetcher);

        let err = cache
            .get_current("remote.example.org", "ed25519:a_1")
            .await
            .unwrap_err();
        assert!(matches!(err, KeyLookupError::InvalidResponse(_)));
    }

    #[tokio::test]
    async fn concurrent_lookups_for_an_unknown_origin_trigger_one_fetch() {
        let fetcher = FixedFetcher::new();
        let (doc, keys) = signed_response("remote.example.org", 3600);
        fetcher.set("remote.example.org", doc);
        let cache = Arc::new(RemoteKeyCache::new(fetcher));

        let key_id = keys.primary().key_id();
        let mut handles = Vec::new();
        for _ in 0..8 {
            let cache = cache.clone();
            let key_id = key_id.clone();
            handles.push(tokio::spawn(async move {
                cache.get_current("remote.example.org", &key_id).await
            }));
        }
        for h in handles {
            h.await.unwrap().unwrap();
        }
        assert_eq!(cache.fetcher.count_for("remote.example.org"), 1);
    }

    #[tokio::test]
    async fn fetch_failure_is_reported_and_not_cached() {
        let fetcher = FixedFetcher::new();
        let cache = RemoteKeyCache::new(fetcher);
        let err = cache
            .get_current("nowhere.example.org", "ed25519:a_1")
            .await
            .unwrap_err();
        assert!(matches!(err, KeyLookupError::FetchFailed(_)));
    }
}
