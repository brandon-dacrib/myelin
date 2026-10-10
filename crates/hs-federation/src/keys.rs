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
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use ed25519_dalek::VerifyingKey;
use futures::StreamExt as _;
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

/// How long one fetch of another server's keys may take, connect and answer together, before
/// it is given up ([`RemoteKeyCache::with_fetch_timeout`] changes it). Shorter than the
/// federation client's general request timeout (30 s) on purpose: a key fetch is on the path
/// of verifying every event, and a server that is gone -- most of the servers a large room's
/// state cites -- costs this much once (then [`KEY_FETCH_BACKOFF_MIN`]). Synapse's direct key
/// fetch uses the same 10 s.
pub const DEFAULT_KEY_FETCH_TIMEOUT: Duration = Duration::from_secs(10);

/// How long a server whose key fetch failed is left alone before it is asked again; doubled
/// after each further failure up to [`KEY_FETCH_BACKOFF_MAX`], cleared by a fetch that
/// succeeds. Keys already held are never affected: this only governs fetching.
pub const KEY_FETCH_BACKOFF_MIN: Duration = Duration::from_secs(60);

/// The ceiling of the key-fetch backoff ([`KEY_FETCH_BACKOFF_MIN`]).
pub const KEY_FETCH_BACKOFF_MAX: Duration = Duration::from_secs(60 * 60);

/// `duration` in whole milliseconds, saturating (for the fetch budget's atomic).
fn duration_to_ms(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

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

    /// A second handle to the same keys, for tests that hand one to a fake and keep one.
    #[cfg(test)]
    pub(crate) fn clone_for_test(&self) -> Self {
        Self {
            keys: self.keys.clone(),
        }
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

    /// Asks the notary `notary` with `POST /_matrix/key/v2/query` and `body`
    /// (`{"server_keys": {server: {key_id: {"minimum_valid_until_ts": n}}}}`), answering its
    /// parsed `200` body, not yet verified: [`RemoteKeyCache`] checks that each document it
    /// carries is signed by the notary's configured key and by the server it is about. `None`
    /// when the notary could not be reached or did not answer `200` with JSON. The default asks
    /// nobody, for fetchers (and test fakes) that have no notary to ask.
    async fn query_notary(
        &self,
        notary: &str,
        body: &serde_json::Value,
    ) -> Option<serde_json::Value> {
        let _ = (notary, body);
        None
    }
}

#[async_trait]
impl KeyServerFetcher for Box<dyn KeyServerFetcher> {
    async fn fetch_server_key(&self, server_name: &str) -> Option<serde_json::Value> {
        (**self).fetch_server_key(server_name).await
    }

    async fn query_notary(
        &self,
        notary: &str,
        body: &serde_json::Value,
    ) -> Option<serde_json::Value> {
        (**self).query_notary(notary, body).await
    }
}

/// A [`RemoteKeyCache`] over a boxed, dynamically-dispatched fetcher — the concrete type used
/// wherever the cache is stored alongside other crate-wide state (`crate::xmatrix`,
/// `crate::client`) without infecting every consumer with `KeyServerFetcher`'s type parameter.
pub type DynRemoteKeyCache = RemoteKeyCache<Box<dyn KeyServerFetcher>>;

/// A notary (`federation.trusted_key_servers`; Synapse's "trusted key server" or "perspectives
/// server") this cache asks for a key a server does not publish, with the keys the notary's
/// answers must be signed by. A notary keeps every key response it has ever fetched, so it
/// still has the key a server signed its rooms' creation with years ago and has since rotated
/// out; the server itself usually does not publish it (`old_verify_keys` is rare in practice),
/// and without the notary such a room cannot be joined.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrustedKeyServer {
    server_name: String,
    verify_keys: HashMap<String, VerifyingKey>,
}

impl TrustedKeyServer {
    /// A notary named `server_name` whose answers must be signed by one of `verify_keys`
    /// (`key_id` to base64 public key, as its `/_matrix/key/v2/server` publishes them).
    ///
    /// # Errors
    /// [`FederationError::Key`] when `verify_keys` is empty, or a key is not a base64 Ed25519
    /// public key.
    pub fn new(
        server_name: &str,
        verify_keys: impl IntoIterator<Item = (String, String)>,
    ) -> Result<Self, FederationError> {
        let mut keys = HashMap::new();
        for (key_id, key_b64) in verify_keys {
            let key = signing::verifying_key_from_base64(&key_b64).map_err(|e| {
                FederationError::Key(format!(
                    "notary {server_name}: verify key {key_id} is not an Ed25519 public key: {e}"
                ))
            })?;
            keys.insert(key_id, key);
        }
        if keys.is_empty() {
            return Err(FederationError::Key(format!(
                "notary {server_name}: no verify key configured"
            )));
        }
        Ok(Self {
            server_name: server_name.to_owned(),
            verify_keys: keys,
        })
    }

    /// From one entry of `federation.trusted_key_servers`.
    ///
    /// # Errors
    /// As [`TrustedKeyServer::new`].
    pub fn from_config(
        configured: &hs_config::federation::TrustedKeyServer,
    ) -> Result<Self, FederationError> {
        Self::new(
            &configured.server_name,
            configured
                .verify_keys
                .iter()
                .map(|(id, key)| (id.clone(), key.clone())),
        )
    }

    /// Every entry of `federation.trusted_key_servers` that parses; one that does not is
    /// logged and left out (configuration validation refuses the malformed shapes first, so
    /// this is a key that is well-formed base64 of the wrong length, or the like).
    #[must_use]
    pub fn list_from_config(configured: &[hs_config::federation::TrustedKeyServer]) -> Vec<Self> {
        configured
            .iter()
            .filter_map(|entry| match Self::from_config(entry) {
                Ok(notary) => Some(notary),
                Err(error) => {
                    tracing::warn!(
                        notary = %entry.server_name,
                        %error,
                        "a trusted key server in the configuration is unusable; not asking it"
                    );
                    None
                }
            })
            .collect()
    }

    /// The notary's server name.
    #[must_use]
    pub fn server_name(&self) -> &str {
        &self.server_name
    }
}

/// A key a caller needs: `key_id` of `server_name`, valid when something was signed at
/// `signed_at_ts` (ms since the epoch). [`RemoteKeyCache::ensure_keys`] takes a batch of them.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct WantedKey {
    /// The server whose key it is.
    pub server_name: String,
    /// `ed25519:<version>`.
    pub key_id: String,
    /// When the thing to verify was signed: `origin_server_ts` for an event, now for a request.
    pub signed_at_ts: u64,
}

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
/// (worth a fetch) from "definitely expired" (fetching again will not help). Every variant a
/// fetch pass can end in says what the origin answered and what each notary answered, so the
/// one line logged for a dropped event says where the key was looked for.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum KeyLookupError {
    /// Neither the server (`detail` says whether it answered or could not be reached) nor any
    /// notary has the key.
    #[error("no verify key `{key_id}` known for server `{server_name}`: {detail}")]
    Unknown {
        server_name: String,
        key_id: String,
        /// What the server and each notary answered, for the log.
        detail: String,
    },
    #[error("verify key `{key_id}` for server `{server_name}` was not valid at the required time")]
    Expired { server_name: String, key_id: String },
    /// The server could not be reached (or answered nothing usable), and no notary had the key.
    #[error("could not fetch keys for server `{server_name}`: {detail}")]
    FetchFailed {
        server_name: String,
        /// Why the fetch failed, and what each notary answered.
        detail: String,
    },
    #[error("fetched key response for `{0}` failed self-signature verification")]
    InvalidResponse(String),
    /// Not asked: an earlier fetch failed and the server is being left alone for a while
    /// ([`KEY_FETCH_BACKOFF_MIN`]). Names what failed and when the next attempt may be made,
    /// so every event from a gone server after the first fails at once and says why.
    #[error(
        "not asking `{server_name}` for keys again for {retry_in_secs} s: its last fetch failed \
         ({reason}){notaries}"
    )]
    FetchBackoff {
        server_name: String,
        /// What the failed fetch reported: timed out, unreachable, or an invalid response.
        reason: String,
        /// Seconds until the next fetch may be made (0 when it is due now).
        retry_in_secs: u64,
        /// What each notary answered for the key (`; notary matrix.org had nothing for it`),
        /// or empty when none is configured.
        notaries: String,
    },
}

/// A server whose last key fetch failed, left alone until `retry_at`
/// ([`RemoteKeyCache::fetch_failure`]). Also a notary whose last query failed.
#[derive(Debug, Clone)]
pub struct FetchFailure {
    /// What the fetch reported: timed out, unreachable, or an invalid response.
    pub reason: String,
    /// When it failed.
    pub failed_at: tokio::time::Instant,
    /// When the server may be asked again.
    pub retry_at: tokio::time::Instant,
    /// The wait this failure set, doubled from the one before it.
    pub backoff: Duration,
}

impl FetchFailure {
    /// The error a lookup that is not made because of this failure answers.
    fn error(
        &self,
        server_name: &str,
        now: tokio::time::Instant,
        notaries: String,
    ) -> KeyLookupError {
        KeyLookupError::FetchBackoff {
            server_name: server_name.to_owned(),
            reason: self.reason.clone(),
            retry_in_secs: self.retry_at.saturating_duration_since(now).as_secs(),
            notaries,
        }
    }
}

/// Records a failure of `name` into `failures` at `now`: the wait before the next attempt
/// doubles from the previous failure's ([`KEY_FETCH_BACKOFF_MIN`] for the first, at most
/// [`KEY_FETCH_BACKOFF_MAX`]). Answers the wait set.
fn record_failure(
    failures: &mut HashMap<String, FetchFailure>,
    name: &str,
    reason: &str,
    now: tokio::time::Instant,
) -> Duration {
    let backoff = failures
        .get(name)
        .map_or(KEY_FETCH_BACKOFF_MIN, |previous| {
            previous
                .backoff
                .saturating_mul(2)
                .min(KEY_FETCH_BACKOFF_MAX)
        });
    failures.insert(
        name.to_owned(),
        FetchFailure {
            reason: reason.to_owned(),
            failed_at: now,
            retry_at: now + backoff,
            backoff,
        },
    );
    backoff
}

/// How long after a server answered its keys it is not asked again for a key it did not
/// list: what it publishes is what it published a moment ago. Short, so a server that has
/// just rotated to a new key and signs with it at once is asked again soon; long enough that
/// the events of one `send_join` answer signed with a key the server does not publish cost
/// one fetch, not one each ([`RemoteKeyCache::ensure_keys`] fetches ahead of them).
pub const ORIGIN_RECHECK_INTERVAL: Duration = Duration::from_secs(10);

/// How long a key a notary had nothing for is not asked of that notary again. A notary that
/// does not hold a key now will not hold it a moment later unless the server it belongs to
/// publishes it again, which the server is asked about itself.
pub const NOTARY_MISS_INTERVAL: Duration = Duration::from_secs(60);

/// Caches other servers' verify keys, fetched (and self-signature-checked) on demand, with
/// per-origin in-flight de-duplication (threat model 2.3's confused-deputy defence: N concurrent
/// callers asking about the same unknown origin trigger exactly one fetch, and the ones that
/// waited share its outcome, success or failure, instead of fetching again in turn).
///
/// A fetch has a budget ([`DEFAULT_KEY_FETCH_TIMEOUT`]), and a server whose fetch failed is
/// not asked again until a backoff ends ([`KEY_FETCH_BACKOFF_MIN`], doubling to
/// [`KEY_FETCH_BACKOFF_MAX`]; a success clears it). Without both, joining a large room through
/// another server took hours: its state cites thousands of servers, many gone, and every event
/// from a gone server cost the client's full 30 s request timeout, one after another (the demo
/// server joining `#matrix:matrix.org` on 2026-10-10). Keys already held are never affected by
/// the backoff: it only decides whether a fetch is made.
///
/// A key the server does not publish, or that cannot be fetched because the server is gone or
/// backed off from, is asked of the notaries ([`TrustedKeyServer`]; none by default here,
/// matrix.org by default in the configuration), concurrently with the server itself and under
/// the same budget. A notary's answer is accepted only when it is signed by the notary's
/// configured key *and* by the server it is about, and a key it answers is cached like one
/// fetched from the server, with the validity the server gave it: an event signed while the
/// key was valid verifies against it however long ago that was. When the server and a notary
/// both answer, the server's answer wins (it is the fresher document). A key no notary had is
/// not asked of them again for [`NOTARY_MISS_INTERVAL`]. The demo could not join
/// `#selfhosted:selfhosted.chat` on 2026-10-10: its `m.room.create` is signed with
/// `ed25519:a_VOzg`, which selfhosted.chat no longer publishes; matrix.org's notary has it.
pub struct RemoteKeyCache<F: KeyServerFetcher> {
    fetcher: F,
    /// How long one fetch may take, in milliseconds (`federation.key_fetch_timeout`; read per
    /// fetch, so [`RemoteKeyCache::set_fetch_timeout`] applies to the next one).
    fetch_timeout_ms: std::sync::atomic::AtomicU64,
    /// The notaries asked for a key a server does not publish, in the order asked
    /// (concurrently). Replaced whole by [`RemoteKeyCache::set_trusted_key_servers`].
    notaries: std::sync::RwLock<Arc<Vec<TrustedKeyServer>>>,
    /// The servers whose last fetch failed, with when they may be asked again.
    failures: std::sync::Mutex<HashMap<String, FetchFailure>>,
    /// When each server's own key response was last accepted ([`ORIGIN_RECHECK_INTERVAL`]).
    origin_fetched: std::sync::Mutex<HashMap<String, tokio::time::Instant>>,
    /// The notaries whose last query failed, with when they may be asked again.
    notary_failures: std::sync::Mutex<HashMap<String, FetchFailure>>,
    /// `(notary, server, key_id)` the notary was asked about and had nothing for, with until
    /// when it is not asked again ([`NOTARY_MISS_INTERVAL`]).
    notary_misses: std::sync::Mutex<HashMap<(String, String, String), tokio::time::Instant>>,
    current: std::sync::Mutex<HashMap<(String, String), CachedCurrent>>,
    old: std::sync::Mutex<HashMap<(String, String), CachedOld>>,
    in_flight: std::sync::Mutex<HashMap<String, Arc<AsyncMutex<()>>>>,
    /// When a response was last accepted for each server (ms since the epoch), for the admin
    /// API's view of the cache.
    fetched_at: std::sync::Mutex<HashMap<String, u64>>,
    /// The last self-signed response accepted that listed each `(server, key_id)` in its
    /// `verify_keys`, exactly as the server published it, kept for the notary endpoints
    /// ([`RemoteKeyCache::notary_responses`]): a notary co-signs the origin's own document, so
    /// the parsed keys above are not enough. Expired responses are kept too -- the spec's
    /// notary answers with the last keys it holds for a server that cannot be reached.
    responses: std::sync::Mutex<HashMap<(String, String), StoredResponse>>,
    /// Where accepted responses are also written, so a restart starts with them
    /// ([`RemoteKeyCache::with_store`]); `None` keeps them in memory only.
    store: Option<Arc<dyn crate::key_store::HeldKeyStore>>,
    /// This server's own verify keys, by `(own server_name, key_id)`, seeded by
    /// [`RemoteKeyCache::seed_own_keys`]. Consulted before any cache lookup or fetch, so an event
    /// this server signed itself -- its own user's membership echoed back in a `send_join`,
    /// `invite` or `make_join` response -- verifies against the keys already in memory rather
    /// than being fetched over federation from this server (which cannot answer a request to
    /// itself: the fetch would always fail, as it did against a real Synapse on 2026-10-09). An
    /// own key is trusted at any timestamp: whatever this server signed, it signed.
    own: std::sync::Mutex<HashMap<(String, String), VerifyingKey>>,
}

/// One response held for the notary endpoints: the document and its `valid_until_ts`.
#[derive(Debug, Clone)]
struct StoredResponse {
    valid_until_ts: u64,
    doc: Arc<serde_json::Value>,
}

/// The most servers one notary query (`POST /_matrix/key/v2/query`) may ask about. Each server
/// not held fresh enough costs this server one outbound fetch, so a request naming thousands
/// would turn the notary into an amplifier; Synapse's clients ask about one or a few at a time.
/// This server's own queries of a notary name at most this many servers each too
/// ([`RemoteKeyCache::ensure_keys`]).
pub const MAX_NOTARY_SERVERS_PER_QUERY: usize = 100;

/// One key [`RemoteKeyCache`] holds for a server, as the admin API shows it
/// (`federation.keys.get`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CachedKeyView {
    /// `ed25519:<version>`.
    pub key_id: String,
    /// The public key, base64 (standard alphabet, unpadded), as the server published it.
    pub public_key: String,
    /// Until when (ms since the epoch) it may be used: the response's `valid_until_ts` for a
    /// current key, its `expired_ts` for an old one.
    pub valid_until_ts: u64,
    /// Whether it came from `old_verify_keys` (usable only for what was signed before then).
    pub old: bool,
}

/// Everything [`RemoteKeyCache`] holds for one server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CachedServerKeys {
    pub server_name: String,
    /// Sorted by key id, current keys before old ones.
    pub keys: Vec<CachedKeyView>,
    /// When a response from (or about) the server was last accepted, ms since the epoch.
    pub fetched_at_ms: Option<u64>,
}

/// Where a key response came from, for the log and the `source` of
/// `hs_federation_key_fetch_total`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Source<'a> {
    /// The server's own `/_matrix/key/v2/server`.
    Origin,
    /// A notary's `/_matrix/key/v2/query`.
    Notary(&'a str),
}

/// Why one notary query produced nothing for a key.
#[derive(Debug, Clone, PartialEq, Eq)]
enum NotaryOutcome {
    /// The notary answered, and had nothing covering the key.
    Nothing,
    /// The query failed (timed out, unreachable, or every document it carried was refused).
    Failed(String),
}

impl<F: KeyServerFetcher> RemoteKeyCache<F> {
    #[must_use]
    pub fn new(fetcher: F) -> Self {
        Self {
            fetcher,
            fetch_timeout_ms: std::sync::atomic::AtomicU64::new(duration_to_ms(
                DEFAULT_KEY_FETCH_TIMEOUT,
            )),
            notaries: std::sync::RwLock::new(Arc::new(Vec::new())),
            failures: std::sync::Mutex::new(HashMap::new()),
            origin_fetched: std::sync::Mutex::new(HashMap::new()),
            notary_failures: std::sync::Mutex::new(HashMap::new()),
            notary_misses: std::sync::Mutex::new(HashMap::new()),
            current: std::sync::Mutex::new(HashMap::new()),
            old: std::sync::Mutex::new(HashMap::new()),
            in_flight: std::sync::Mutex::new(HashMap::new()),
            fetched_at: std::sync::Mutex::new(HashMap::new()),
            responses: std::sync::Mutex::new(HashMap::new()),
            store: None,
            own: std::sync::Mutex::new(HashMap::new()),
        }
    }

    /// Changes how long one fetch of a server's keys may take, connect and answer together
    /// (the default is [`DEFAULT_KEY_FETCH_TIMEOUT`]; `federation.key_fetch_timeout`). A fetch
    /// over the budget counts as a failure for the backoff. A notary query has the same budget,
    /// concurrently.
    #[must_use]
    pub fn with_fetch_timeout(self, timeout: Duration) -> Self {
        self.fetch_timeout_ms.store(
            duration_to_ms(timeout),
            std::sync::atomic::Ordering::Relaxed,
        );
        self
    }

    /// [`RemoteKeyCache::with_fetch_timeout`] on a running cache (a change to
    /// `federation.key_fetch_timeout`): the next fetch, and every one after it, has the new
    /// budget; one under way keeps the budget it started with.
    pub fn set_fetch_timeout(&self, timeout: Duration) {
        let previous = self.fetch_timeout();
        self.fetch_timeout_ms.store(
            duration_to_ms(timeout),
            std::sync::atomic::Ordering::Relaxed,
        );
        if previous != timeout {
            tracing::info!(
                key_fetch_timeout_ms = duration_to_ms(timeout),
                "the key fetch budget is now in force"
            );
        }
    }

    /// How long one fetch of a server's keys (or one notary query) may take.
    #[must_use]
    pub fn fetch_timeout(&self) -> Duration {
        Duration::from_millis(
            self.fetch_timeout_ms
                .load(std::sync::atomic::Ordering::Relaxed),
        )
    }

    /// The notaries to ask for a key a server does not publish (`federation.trusted_key_servers`).
    #[must_use]
    pub fn with_trusted_key_servers(self, notaries: Vec<TrustedKeyServer>) -> Self {
        self.set_trusted_key_servers(notaries);
        self
    }

    /// Replaces the notaries ([`RemoteKeyCache::with_trusted_key_servers`]) on a running cache:
    /// the next key a server does not hold is asked of the new list. Cheap: the list is read
    /// once per fetch pass, and nothing cached depends on who answered it.
    pub fn set_trusted_key_servers(&self, notaries: Vec<TrustedKeyServer>) {
        if *self.notaries_snapshot() == notaries {
            return;
        }
        tracing::info!(
            notaries = ?notaries.iter().map(TrustedKeyServer::server_name).collect::<Vec<_>>(),
            "trusted key servers (notaries) in force"
        );
        *self
            .notaries
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Arc::new(notaries);
    }

    /// The names of the notaries asked ([`RemoteKeyCache::with_trusted_key_servers`]).
    #[must_use]
    pub fn trusted_key_servers(&self) -> Vec<String> {
        self.notaries_snapshot()
            .iter()
            .map(|notary| notary.server_name.clone())
            .collect()
    }

    fn notaries_snapshot(&self) -> Arc<Vec<TrustedKeyServer>> {
        self.notaries
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// The failure `server_name` is being backed off from, if its last key fetch failed and the
    /// backoff has not ended (for the operator's view and for tests).
    #[must_use]
    pub fn fetch_failure(&self, server_name: &str) -> Option<FetchFailure> {
        self.fetch_failure_at(server_name, tokio::time::Instant::now())
    }

    fn fetch_failure_at(
        &self,
        server_name: &str,
        now: tokio::time::Instant,
    ) -> Option<FetchFailure> {
        self.failures
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(server_name)
            .filter(|failure| now < failure.retry_at)
            .cloned()
    }

    /// The failure `notary` is being backed off from, if its last query failed and the backoff
    /// has not ended.
    #[must_use]
    pub fn notary_failure(&self, notary: &str) -> Option<FetchFailure> {
        let now = tokio::time::Instant::now();
        self.notary_failures
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(notary)
            .filter(|failure| now < failure.retry_at)
            .cloned()
    }

    /// Seeds this server's own verify keys under `own_server_name`, so an event this server
    /// signed itself verifies against the keys in memory instead of being fetched over
    /// federation from this server (a fetch that can never succeed, since this server does not
    /// federate with itself). Every key in `own_keys` -- current and rotated-out -- is seeded,
    /// and each is trusted for a signature made at any time: whatever this server signed, it
    /// signed. Call once at startup, after the signing keys are loaded.
    pub fn seed_own_keys(&self, own_server_name: &str, own_keys: &OwnSigningKeys) {
        let mut own = self
            .own
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for key in own_keys.all() {
            own.insert(
                (own_server_name.to_owned(), key.key_id()),
                key.verifying_key(),
            );
        }
    }

    /// A cache that also keeps every response it accepts in `store`, and starts with what
    /// `store` holds (`crate::key_store`): each held response is verified again, oldest first,
    /// and one that expired more than [`crate::key_store::MAX_HELD_AGE_MS`] ago is forgotten.
    /// What is restored answers the notary and verifies requests and events exactly as a
    /// response fetched now would, without the fetch.
    #[must_use]
    pub fn with_store(fetcher: F, store: Arc<dyn crate::key_store::HeldKeyStore>) -> Self {
        let mut cache = Self::new(fetcher);
        let mut held = store.load();
        held.sort_by_key(|response| response.valid_until_ts);
        let cutoff = now_ms().saturating_sub(crate::key_store::MAX_HELD_AGE_MS);
        let (mut restored, mut forgotten) = (0usize, 0usize);
        for response in held {
            if response.valid_until_ts < cutoff {
                store.forget(&response.server_name, &response.key_id);
                forgotten += 1;
                continue;
            }
            match cache.ingest(&response.server_name, &response.doc, Ingest::Restored) {
                Ok(()) => restored += 1,
                Err(error) => {
                    tracing::warn!(
                        server = %response.server_name,
                        key_id = %response.key_id,
                        %error,
                        "a held key response no longer verifies; forgetting it"
                    );
                    store.forget(&response.server_name, &response.key_id);
                    forgotten += 1;
                }
            }
        }
        if restored + forgotten > 0 {
            tracing::info!(
                restored,
                forgotten,
                "restored the key responses held for other servers"
            );
        }
        cache.store = Some(store);
        cache
    }

    /// The notary half of `/_matrix/key/v2/query`: the self-signed key responses this cache
    /// holds for `server_name`, as the server published them (not yet co-signed: the transport
    /// adds this server's signature, [`wrap_for_notary`]).
    ///
    /// `key_ids` names the keys asked about (empty: all of them). A response is fresh enough
    /// when its `valid_until_ts` is at least `minimum_valid_until_ts`; when some key asked about
    /// has no fresh-enough response held (or, for an empty `key_ids`, none at all is), the server
    /// is asked again first (the server only: a notary does not consult other notaries).
    /// Whatever the fetch's outcome, the answer is every response then held for the keys asked
    /// about -- expired ones included, since the spec's notary returns the last keys it has for
    /// a server that is offline -- and a response that lists another key does not displace one
    /// held for a key it no longer lists (Synapse issue 5305, Sytest's "must not overwrite a
    /// valid key with a spurious result from the origin server").
    pub async fn notary_responses(
        &self,
        server_name: &str,
        key_ids: &[String],
        minimum_valid_until_ts: u64,
    ) -> Vec<serde_json::Value> {
        if !self.held_fresh_enough(server_name, key_ids, minimum_valid_until_ts)
            && let Err(error) = self.refresh_origin(server_name, Refresh::Fresher).await
        {
            tracing::debug!(
                server = server_name,
                %error,
                "notary: could not refresh a server's keys; answering with what is held"
            );
        }
        let held = self
            .responses
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut docs: Vec<Arc<serde_json::Value>> = Vec::new();
        let mut push = |doc: &Arc<serde_json::Value>| {
            if !docs.iter().any(|seen| Arc::ptr_eq(seen, doc)) {
                docs.push(doc.clone());
            }
        };
        if key_ids.is_empty() {
            let mut all: Vec<(&String, &StoredResponse)> = held
                .iter()
                .filter(|((server, _), _)| server == server_name)
                .map(|((_, key_id), stored)| (key_id, stored))
                .collect();
            all.sort_by(|a, b| a.0.cmp(b.0));
            for (_, stored) in all {
                push(&stored.doc);
            }
        } else {
            for key_id in key_ids {
                if let Some(stored) = held.get(&(server_name.to_owned(), key_id.clone())) {
                    push(&stored.doc);
                }
            }
        }
        docs.into_iter().map(|doc| (*doc).clone()).collect()
    }

    /// Whether every key `key_ids` names (or, when it names none, some key of the server) has a
    /// held response valid until at least `minimum_valid_until_ts`.
    fn held_fresh_enough(
        &self,
        server_name: &str,
        key_ids: &[String],
        minimum_valid_until_ts: u64,
    ) -> bool {
        let held = self
            .responses
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let fresh = |stored: &StoredResponse| stored.valid_until_ts >= minimum_valid_until_ts;
        if key_ids.is_empty() {
            held.iter()
                .any(|((server, _), stored)| server == server_name && fresh(stored))
        } else {
            key_ids.iter().all(|key_id| {
                held.get(&(server_name.to_owned(), key_id.clone()))
                    .is_some_and(fresh)
            })
        }
    }

    /// What the cache holds for `server_name` (expired entries included: they are what an
    /// operator looking at a verification failure needs to see), or `None` when it holds
    /// nothing for it.
    #[must_use]
    pub fn cached_keys(&self, server_name: &str) -> Option<CachedServerKeys> {
        use base64::Engine as _;
        let encode = |vk: &VerifyingKey| {
            base64::engine::general_purpose::STANDARD_NO_PAD.encode(vk.to_bytes())
        };
        let mut keys: Vec<CachedKeyView> = self
            .current
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .filter(|((server, _), _)| server == server_name)
            .map(|((_, key_id), cached)| CachedKeyView {
                key_id: key_id.clone(),
                public_key: encode(&cached.verifying_key),
                valid_until_ts: cached.valid_until_ts,
                old: false,
            })
            .collect();
        keys.extend(
            self.old
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .iter()
                .filter(|((server, _), _)| server == server_name)
                .map(|((_, key_id), cached)| CachedKeyView {
                    key_id: key_id.clone(),
                    public_key: encode(&cached.verifying_key),
                    valid_until_ts: cached.expired_ts,
                    old: true,
                }),
        );
        let fetched_at_ms = self
            .fetched_at
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(server_name)
            .copied();
        if keys.is_empty() && fetched_at_ms.is_none() {
            return None;
        }
        keys.sort_by(|a, b| a.old.cmp(&b.old).then_with(|| a.key_id.cmp(&b.key_id)));
        Some(CachedServerKeys {
            server_name: server_name.to_owned(),
            keys,
            fetched_at_ms,
        })
    }

    /// Drops everything the cache holds for `server_name`: its current and old keys, the
    /// responses kept for the notary endpoints, when they were fetched, and any fetch backoff;
    /// and forgets them from the held-key store, so a restart does not bring them back. What
    /// `federation.destinations.forget` does with a forgotten destination's keys (decision
    /// 0042). This server's own keys are never dropped. Returns how many keys were dropped.
    /// A signature of the server seen later fetches its keys afresh.
    pub fn forget_server(&self, server_name: &str) -> usize {
        let mut dropped = 0usize;
        let mut key_ids: Vec<String> = Vec::new();
        {
            let mut current = self
                .current
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            current.retain(|(server, key_id), _| {
                if server == server_name {
                    key_ids.push(key_id.clone());
                    false
                } else {
                    true
                }
            });
        }
        dropped += key_ids.len();
        {
            let mut old = self
                .old
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let before = old.len();
            old.retain(|(server, key_id), _| {
                if server == server_name {
                    key_ids.push(key_id.clone());
                    false
                } else {
                    true
                }
            });
            dropped += before - old.len();
        }
        self.responses
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .retain(|(server, key_id), _| {
                if server == server_name {
                    key_ids.push(key_id.clone());
                    false
                } else {
                    true
                }
            });
        self.fetched_at
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(server_name);
        self.failures
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(server_name);
        // Nor is it remembered as asked a moment ago, or as a key the notaries lacked: the next
        // signature of it asks the server and the notaries afresh.
        self.origin_fetched
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(server_name);
        self.notary_misses
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .retain(|(_, server, _), _| server != server_name);
        if let Some(store) = &self.store {
            key_ids.sort();
            key_ids.dedup();
            for key_id in &key_ids {
                store.forget(server_name, key_id);
            }
        }
        dropped
    }

    /// Fetches `server_name`'s keys again now, whatever is cached and whatever backoff an
    /// earlier failure set (an administrator's `federation.keys.refresh`), and answers what the
    /// cache then holds for it. The server itself is asked, not the notaries.
    ///
    /// # Errors
    /// [`KeyLookupError::FetchFailed`] when the server could not be reached, and
    /// [`KeyLookupError::InvalidResponse`] when what it answered was not a validly self-signed
    /// key response for it; the cache is unchanged either way.
    pub async fn refetch(&self, server_name: &str) -> Result<CachedServerKeys, KeyLookupError> {
        self.refresh_origin(server_name, Refresh::Force).await?;
        self.cached_keys(server_name)
            .ok_or_else(|| KeyLookupError::InvalidResponse(server_name.to_owned()))
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
            crate::metrics::record_key_fetch("cache");
            return Ok(key.verifying_key);
        }
        let wanted = WantedKey {
            server_name: server_name.to_owned(),
            key_id: key_id.to_owned(),
            signed_at_ts: now_ms(),
        };
        self.refresh_for(&wanted).await?;
        self.cached_current(server_name, key_id)
            .map(|k| k.verifying_key)
            .ok_or_else(|| self.unknown(server_name, key_id))
    }

    /// Returns a verifying key usable to verify something signed *at* `signed_at_ts`
    /// (milliseconds since the epoch) — accepts a key that was current at that time even if it
    /// has since rotated out (checked against `old_verify_keys`' `expired_ts`, or the
    /// `valid_until_ts` of the last response that listed it), and rejects a key that had
    /// already expired by then. Fetches (with de-duplication) if nothing cached covers the
    /// requested timestamp: the server, and the notaries with `minimum_valid_until_ts` set to
    /// `signed_at_ts`.
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
            crate::metrics::record_key_fetch("cache");
            return Ok(key);
        }
        let wanted = WantedKey {
            server_name: server_name.to_owned(),
            key_id: key_id.to_owned(),
            signed_at_ts,
        };
        self.refresh_for(&wanted).await?;
        self.cached_valid_at(server_name, key_id, signed_at_ts)
            .ok_or_else(|| {
                // Distinguish "we have never heard of this key" from "we have heard of it and it
                // does not cover this timestamp" for a clearer error, matching the plan's
                // "expired key rejected" test intent.
                let key = (server_name.to_string(), key_id.to_string());
                let known_but_wrong_time = self
                    .current
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .contains_key(&key)
                    || self
                        .old
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .contains_key(&key);
                if known_but_wrong_time {
                    KeyLookupError::Expired {
                        server_name: server_name.to_string(),
                        key_id: key_id.to_string(),
                    }
                } else {
                    self.unknown(server_name, key_id)
                }
            })
    }

    /// Fetches ahead of a batch of lookups: every key of `wanted` not already held is asked of
    /// its server (the servers [`JOIN_PREFETCH_CONCURRENCY`] at a time, each with the usual
    /// budget, de-duplication and backoff) and then, for the keys the servers did not provide,
    /// of each notary in one query naming them all (chunked by
    /// [`MAX_NOTARY_SERVERS_PER_QUERY`] servers). The
    /// lookups that follow ([`RemoteKeyCache::get_valid_at`]) then find their key held, or are
    /// refused at once: a server that did not answer is backed off from, one that answered
    /// without the key is not asked again for [`ORIGIN_RECHECK_INTERVAL`], and a key no notary
    /// had is not asked of them again for [`NOTARY_MISS_INTERVAL`]. A `send_join` answer's
    /// events (`crate::outbound_join`) cost one notary query, not one per event.
    pub async fn ensure_keys(&self, wanted: &[WantedKey]) {
        let mut missing: Vec<WantedKey> = Vec::new();
        for key in wanted {
            if self
                .cached_valid_at(&key.server_name, &key.key_id, key.signed_at_ts)
                .is_none()
                && !missing.contains(key)
            {
                missing.push(key.clone());
            }
        }
        if missing.is_empty() {
            return;
        }
        // Owned names: a closure over `&str` here is not general enough in its lifetime for
        // the task a join runs in (`tokio::spawn` needs the whole future to be `Send` for any
        // lifetime), and the compiler says so in `hs-cli`, not here.
        let mut servers: Vec<String> = Vec::new();
        for key in &missing {
            if !servers.contains(&key.server_name) {
                servers.push(key.server_name.clone());
            }
        }
        futures::stream::iter(servers.into_iter().map(|server| async move {
            if let Err(error) = self.refresh_origin(&server, Refresh::Lookup).await {
                tracing::debug!(server, %error, "prefetch: a server's keys were not fetched");
            }
        }))
        .buffer_unordered(JOIN_PREFETCH_CONCURRENCY)
        .collect::<Vec<()>>()
        .await;
        // The notaries, for what the servers did not provide: the keys of servers that are
        // gone or backed off from, and keys a server has rotated out. One query per notary
        // names them all; a join whose servers all answer costs no notary query.
        let still_missing: Vec<WantedKey> = missing
            .into_iter()
            .filter(|key| {
                self.cached_valid_at(&key.server_name, &key.key_id, key.signed_at_ts)
                    .is_none()
            })
            .collect();
        if still_missing.is_empty() {
            return;
        }
        let notaries = self.notaries_snapshot();
        let notaries: Vec<&TrustedKeyServer> = notaries
            .iter()
            .filter(|notary| self.notary_failure(&notary.server_name).is_none())
            .collect();
        if !notaries.is_empty() {
            tracing::info!(
                keys = still_missing.len(),
                servers = still_missing
                    .iter()
                    .map(|key| key.server_name.as_str())
                    .collect::<std::collections::BTreeSet<_>>()
                    .len(),
                notaries = ?notaries.iter().map(|n| n.server_name.as_str()).collect::<Vec<_>>(),
                "asking the notaries for the keys their servers did not provide"
            );
        }
        self.query_notaries(&notaries, &still_missing).await;
    }

    fn cached_current(&self, server_name: &str, key_id: &str) -> Option<CachedCurrent> {
        if let Some(verifying_key) = self
            .own
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&(server_name.to_string(), key_id.to_string()))
        {
            return Some(CachedCurrent {
                verifying_key: *verifying_key,
                valid_until_ts: u64::MAX,
            });
        }
        let now = now_ms();
        let map = self
            .current
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        map.get(&(server_name.to_string(), key_id.to_string()))
            .filter(|c| now < c.valid_until_ts)
            .cloned()
    }

    fn cached_valid_at(&self, server_name: &str, key_id: &str, ts: u64) -> Option<VerifyingKey> {
        let key = (server_name.to_string(), key_id.to_string());
        if let Some(verifying_key) = self
            .own
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&key)
        {
            return Some(*verifying_key);
        }
        if let Some(c) = self
            .current
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&key)
            && ts <= c.valid_until_ts
        {
            return Some(c.verifying_key);
        }
        if let Some(c) = self
            .old
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&key)
            && ts <= c.expired_ts
        {
            return Some(c.verifying_key);
        }
        None
    }

    /// The in-flight lock of `server_name`: one fetch of a server's keys at a time.
    fn in_flight_lock(&self, server_name: &str) -> Arc<AsyncMutex<()>> {
        self.in_flight
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .entry(server_name.to_string())
            .or_insert_with(|| Arc::new(AsyncMutex::new(())))
            .clone()
    }

    /// Whether `server_name`'s own keys were accepted within [`ORIGIN_RECHECK_INTERVAL`] of
    /// `now`: what it publishes is known, and a key not among them is not there.
    fn origin_fetched_recently(&self, server_name: &str, now: tokio::time::Instant) -> bool {
        self.origin_fetched
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(server_name)
            .is_some_and(|at| now.saturating_duration_since(*at) < ORIGIN_RECHECK_INTERVAL)
    }

    /// The notaries to ask about `server_name`'s keys: every configured one but the server
    /// itself (its notary would answer its own current document, which is what asking it
    /// directly gets).
    fn notaries_for(&self, server_name: &str) -> Vec<TrustedKeyServer> {
        self.notaries_snapshot()
            .iter()
            .filter(|notary| notary.server_name != server_name)
            .cloned()
            .collect()
    }

    /// Whether `notary` was asked about `key` within [`NOTARY_MISS_INTERVAL`] and had nothing.
    fn notary_missed(&self, notary: &str, key: &WantedKey, now: tokio::time::Instant) -> bool {
        self.notary_misses
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&(
                notary.to_owned(),
                key.server_name.clone(),
                key.key_id.clone(),
            ))
            .is_some_and(|until| now < *until)
    }

    /// Fetches what is needed to answer one lookup of `wanted`, de-duplicated per server: the
    /// server's own keys (unless fetched within [`ORIGIN_RECHECK_INTERVAL`], or backed off
    /// from) and, concurrently, the notaries' answer for the key (unless each already had
    /// nothing for it, or is backed off from). `Ok` when the key may now be held -- the caller
    /// looks again -- and `Err` when the server could not be asked and no notary had it.
    async fn refresh_for(&self, wanted: &WantedKey) -> Result<(), KeyLookupError> {
        let server_name = wanted.server_name.as_str();
        let lock = self.in_flight_lock(server_name);
        let _guard = lock.lock().await;
        // A fetch for this server finished while this caller waited for the lock, and brought
        // the key: its outcome is this caller's too.
        if self
            .cached_valid_at(server_name, &wanted.key_id, wanted.signed_at_ts)
            .is_some()
        {
            return Ok(());
        }
        let now = tokio::time::Instant::now();
        let backed_off = self.fetch_failure_at(server_name, now);
        let ask_origin = backed_off.is_none() && !self.origin_fetched_recently(server_name, now);
        let notaries = self.notaries_for(server_name);
        let notaries: Vec<&TrustedKeyServer> = notaries
            .iter()
            .filter(|notary| {
                !self.notary_missed(&notary.server_name, wanted, now)
                    && self.notary_failure(&notary.server_name).is_none()
            })
            .collect();
        if !ask_origin && notaries.is_empty() {
            return match backed_off {
                Some(failure) => {
                    crate::metrics::record_key_fetch_failure("backoff");
                    Err(failure.error(server_name, now, self.notary_summary(wanted)))
                }
                // Fetched a moment ago, and the key was not among them: the caller answers
                // `Unknown` with the detail.
                None => Ok(()),
            };
        }
        // The server and the notaries are asked concurrently, but the server's answer is
        // waited for first: when it carries the key, the notaries' answer is not needed (the
        // query is dropped, and with it the request), so a lookup the server answers in a
        // moment does not wait on matrix.org as well. Only a server that is gone, backed off
        // from, or does not publish the key waits for the notaries.
        let origin = async {
            if ask_origin {
                Some(self.fetch_origin(server_name).await)
            } else {
                None
            }
        };
        let notary_query = self.query_notaries(&notaries, std::slice::from_ref(wanted));
        let mut origin = std::pin::pin!(origin);
        let mut notary_query = std::pin::pin!(notary_query);
        let mut notaries_done = notaries.is_empty();
        let origin = loop {
            tokio::select! {
                // The notaries first, so their query is on its way before the server's answer
                // can end the wait.
                biased;
                () = &mut notary_query, if !notaries_done => notaries_done = true,
                outcome = &mut origin => break outcome,
            }
        };
        if self
            .cached_valid_at(server_name, &wanted.key_id, wanted.signed_at_ts)
            .is_some()
        {
            return Ok(());
        }
        if !notaries_done {
            notary_query.await;
        }
        match origin {
            Some(Err(error))
                if self
                    .cached_valid_at(server_name, &wanted.key_id, wanted.signed_at_ts)
                    .is_none() =>
            {
                Err(match error {
                    KeyLookupError::FetchFailed {
                        server_name,
                        detail,
                    } => KeyLookupError::FetchFailed {
                        server_name,
                        detail: format!("{detail}{}", self.notary_summary(wanted)),
                    },
                    other => other,
                })
            }
            _ => Ok(()),
        }
    }

    /// Fetches `server_name`'s own keys under its in-flight lock, unless `how` says the server
    /// is not to be asked now: backed off from (the backoff's error), or fetched within
    /// [`ORIGIN_RECHECK_INTERVAL`] (`Ok`, nothing fetched). The notaries are not asked.
    async fn refresh_origin(&self, server_name: &str, how: Refresh) -> Result<(), KeyLookupError> {
        let lock = self.in_flight_lock(server_name);
        let _guard = lock.lock().await;
        if how != Refresh::Force {
            let now = tokio::time::Instant::now();
            if let Some(failure) = self.fetch_failure_at(server_name, now) {
                crate::metrics::record_key_fetch_failure("backoff");
                return Err(failure.error(server_name, now, String::new()));
            }
            if how == Refresh::Lookup && self.origin_fetched_recently(server_name, now) {
                return Ok(());
            }
        }
        self.fetch_origin(server_name).await
    }

    /// One fetch of `server_name`'s own `/_matrix/key/v2/server`, within the budget, ingested;
    /// a failure is recorded for the backoff. The caller holds the server's in-flight lock.
    async fn fetch_origin(&self, server_name: &str) -> Result<(), KeyLookupError> {
        let started = tokio::time::Instant::now();
        let fetch_timeout = self.fetch_timeout();
        let outcome =
            tokio::time::timeout(fetch_timeout, self.fetcher.fetch_server_key(server_name)).await;
        let result: Result<(), (&'static str, String)> = match outcome {
            Err(_elapsed) => Err((
                "timeout",
                format!("timed out after {} s", fetch_timeout.as_secs()),
            )),
            Ok(None) => Err((
                "unreachable",
                "the key server could not be reached, or did not answer 200 with a key \
                 response"
                    .to_owned(),
            )),
            Ok(Some(doc)) => self
                .ingest(server_name, &doc, Ingest::Fetched(Source::Origin))
                .map_err(|error| ("invalid_response", error.to_string())),
        };
        match result {
            Ok(()) => {
                self.failures
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .remove(server_name);
                self.origin_fetched
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .insert(server_name.to_owned(), tokio::time::Instant::now());
                Ok(())
            }
            Err((label, reason)) => {
                crate::metrics::record_key_fetch_failure(label);
                let backoff = record_failure(
                    &mut self
                        .failures
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner),
                    server_name,
                    &reason,
                    started,
                );
                tracing::info!(
                    server = server_name,
                    reason = %reason,
                    took_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
                    backoff_secs = backoff.as_secs(),
                    "could not fetch a server's keys; not asking it again until the backoff ends"
                );
                Err(match label {
                    "invalid_response" => KeyLookupError::InvalidResponse(server_name.to_owned()),
                    _ => KeyLookupError::FetchFailed {
                        server_name: server_name.to_owned(),
                        detail: reason,
                    },
                })
            }
        }
    }

    /// Asks each of `notaries`, concurrently and each within the budget, for every key of
    /// `wanted` (not those of the notary itself), in queries of at most
    /// [`MAX_NOTARY_SERVERS_PER_QUERY`] servers; ingests what verifies; records a miss for
    /// every key a notary answered nothing covering, and a failure for a notary that could not
    /// be asked.
    async fn query_notaries(&self, notaries: &[&TrustedKeyServer], wanted: &[WantedKey]) {
        if notaries.is_empty() || wanted.is_empty() {
            return;
        }
        let queries = notaries.iter().map(|notary| async move {
            let keys: Vec<&WantedKey> = wanted
                .iter()
                .filter(|key| key.server_name != notary.server_name)
                .collect();
            let mut servers: Vec<&str> = Vec::new();
            for key in &keys {
                if !servers.contains(&key.server_name.as_str()) {
                    servers.push(&key.server_name);
                }
            }
            for chunk in servers.chunks(MAX_NOTARY_SERVERS_PER_QUERY) {
                let batch: Vec<&WantedKey> = keys
                    .iter()
                    .copied()
                    .filter(|key| chunk.contains(&key.server_name.as_str()))
                    .collect();
                let outcome = self.query_notary(notary, &batch).await;
                if let Some(NotaryOutcome::Failed(reason)) = outcome {
                    let now = tokio::time::Instant::now();
                    let backoff = record_failure(
                        &mut self
                            .notary_failures
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner),
                        &notary.server_name,
                        &reason,
                        now,
                    );
                    tracing::warn!(
                        notary = %notary.server_name,
                        reason = %reason,
                        backoff_secs = backoff.as_secs(),
                        "could not ask a notary for keys; not asking it again until the \
                         backoff ends"
                    );
                    break;
                }
            }
        });
        futures::future::join_all(queries).await;
    }

    /// One `POST /_matrix/key/v2/query` to `notary` for `batch`: ingests the documents that
    /// verify, records a miss for each key still not held. `None` when there was nothing to
    /// ask; `Nothing` when the notary answered; `Failed` when it did not, or nothing it
    /// answered verified.
    async fn query_notary(
        &self,
        notary: &TrustedKeyServer,
        batch: &[&WantedKey],
    ) -> Option<NotaryOutcome> {
        if batch.is_empty() {
            return None;
        }
        let mut server_keys = serde_json::Map::new();
        for key in batch {
            server_keys
                .entry(key.server_name.clone())
                .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()))
                .as_object_mut()
                .map(|criteria| {
                    criteria.insert(
                        key.key_id.clone(),
                        serde_json::json!({ "minimum_valid_until_ts": key.signed_at_ts }),
                    )
                });
        }
        let body = serde_json::json!({ "server_keys": server_keys });
        let started = tokio::time::Instant::now();
        let fetch_timeout = self.fetch_timeout();
        let answer = tokio::time::timeout(
            fetch_timeout,
            self.fetcher.query_notary(&notary.server_name, &body),
        )
        .await;
        let outcome = match answer {
            Err(_elapsed) => {
                NotaryOutcome::Failed(format!("timed out after {} s", fetch_timeout.as_secs()))
            }
            Ok(None) => NotaryOutcome::Failed(
                "the notary could not be reached, or did not answer 200 with JSON".to_owned(),
            ),
            Ok(Some(answer)) => self.ingest_notary_answer(notary, &answer, batch),
        };
        match &outcome {
            NotaryOutcome::Failed(_) => {
                crate::metrics::record_key_fetch_failure("notary");
            }
            NotaryOutcome::Nothing => {
                let until = tokio::time::Instant::now() + NOTARY_MISS_INTERVAL;
                let mut misses = self
                    .notary_misses
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                let mut missed = 0usize;
                for key in batch {
                    if self
                        .cached_valid_at(&key.server_name, &key.key_id, key.signed_at_ts)
                        .is_none()
                    {
                        misses.insert(
                            (
                                notary.server_name.clone(),
                                key.server_name.clone(),
                                key.key_id.clone(),
                            ),
                            until,
                        );
                        missed += 1;
                    }
                }
                tracing::debug!(
                    notary = %notary.server_name,
                    asked = batch.len(),
                    missed,
                    took_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
                    "asked a notary for keys"
                );
            }
        }
        Some(outcome)
    }

    /// Takes a notary's answer: each document of `server_keys` that is about a server in
    /// `batch`, is signed by one of the notary's configured keys, and (in [`Self::ingest`]) is
    /// self-signed by that server, is cached as if fetched from the server. `Nothing` when the
    /// answer parsed (whether or not anything in it was accepted; the caller checks what is
    /// held), `Failed` when it did not, or every document in it was refused.
    fn ingest_notary_answer(
        &self,
        notary: &TrustedKeyServer,
        answer: &serde_json::Value,
        batch: &[&WantedKey],
    ) -> NotaryOutcome {
        let Some(docs) = answer
            .get("server_keys")
            .and_then(serde_json::Value::as_array)
        else {
            return NotaryOutcome::Failed("the answer has no `server_keys` array".to_owned());
        };
        let (mut accepted, mut refused) = (0usize, 0usize);
        for doc in docs {
            let Some(server_name) = doc.get("server_name").and_then(serde_json::Value::as_str)
            else {
                refused += 1;
                continue;
            };
            if !batch.iter().any(|key| key.server_name == server_name) {
                // Not asked about: not taken, however well signed.
                refused += 1;
                continue;
            }
            let Ok(object) = signing::to_signable_object(doc) else {
                refused += 1;
                continue;
            };
            let signed_by_notary = object
                .get("signatures")
                .and_then(CanonicalJsonValue::as_object)
                .and_then(|signatures| signatures.get(&notary.server_name))
                .and_then(CanonicalJsonValue::as_object)
                .is_some_and(|by_notary| {
                    by_notary.keys().any(|key_id| {
                        notary.verify_keys.get(key_id).is_some_and(|vk| {
                            signing::verify_object(&object, &notary.server_name, key_id, vk).is_ok()
                        })
                    })
                });
            if !signed_by_notary {
                tracing::warn!(
                    notary = %notary.server_name,
                    server = server_name,
                    configured_key_ids = ?notary.verify_keys.keys().collect::<Vec<_>>(),
                    "a notary's answer is not signed by any of its configured keys; refusing it \
                     (if the notary rotated its key, update federation.trusted_key_servers)"
                );
                refused += 1;
                continue;
            }
            match self.ingest(
                server_name,
                doc,
                Ingest::Fetched(Source::Notary(&notary.server_name)),
            ) {
                Ok(()) => accepted += 1,
                Err(error) => {
                    tracing::warn!(
                        notary = %notary.server_name,
                        server = server_name,
                        %error,
                        "a document in a notary's answer is not signed by the server it is \
                         about; refusing it"
                    );
                    refused += 1;
                }
            }
        }
        if accepted == 0 && refused > 0 {
            return NotaryOutcome::Failed(format!(
                "every document in the notary's answer was refused ({refused})"
            ));
        }
        NotaryOutcome::Nothing
    }

    /// What each notary answered for `wanted`, for an error message: `; notary matrix.org had
    /// nothing for it`, or empty when no notary applies.
    fn notary_summary(&self, wanted: &WantedKey) -> String {
        let notaries = self.notaries_for(&wanted.server_name);
        if notaries.is_empty() {
            return if self.notaries_snapshot().is_empty() {
                "; no notary is configured (federation.trusted_key_servers)".to_owned()
            } else {
                String::new()
            };
        }
        let now = tokio::time::Instant::now();
        let parts: Vec<String> = notaries
            .iter()
            .map(|notary| {
                if let Some(failure) = self.notary_failure(&notary.server_name) {
                    format!(
                        "notary {} could not be asked ({})",
                        notary.server_name, failure.reason
                    )
                } else if self.notary_missed(&notary.server_name, wanted, now) {
                    format!("notary {} had nothing for it", notary.server_name)
                } else {
                    format!("notary {} was not asked", notary.server_name)
                }
            })
            .collect();
        format!("; {}", parts.join("; "))
    }

    /// The error for a key that is not held after a fetch pass: what the server answered, and
    /// what each notary answered.
    fn unknown(&self, server_name: &str, key_id: &str) -> KeyLookupError {
        let now = tokio::time::Instant::now();
        let origin = if let Some(failure) = self.fetch_failure_at(server_name, now) {
            format!("its last key fetch failed ({})", failure.reason)
        } else if self.origin_fetched_recently(server_name, now)
            || self
                .fetched_at
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .contains_key(server_name)
        {
            "the server does not publish it".to_owned()
        } else {
            "the server was not asked".to_owned()
        };
        let wanted = WantedKey {
            server_name: server_name.to_owned(),
            key_id: key_id.to_owned(),
            signed_at_ts: 0,
        };
        KeyLookupError::Unknown {
            server_name: server_name.to_owned(),
            key_id: key_id.to_owned(),
            detail: format!("{origin}{}", self.notary_summary(&wanted)),
        }
    }

    /// Verifies a fetched key response is validly self-signed by a key it itself claims, then
    /// caches every key it lists — scoped strictly to `expected_server_name`, so a response
    /// cannot inject keys under any other server's name (the "no key substitution" defence).
    ///
    /// `pub` (not just called internally from the fetch paths) so it is directly fuzzable
    /// against arbitrary, hostile JSON without needing a live fetcher to drive it — see
    /// `fuzz/fuzz_targets/key_server_response_parse.rs`.
    ///
    /// # Errors
    /// See [`KeyLookupError`].
    pub fn ingest_response(
        &self,
        expected_server_name: &str,
        doc: &serde_json::Value,
    ) -> Result<(), KeyLookupError> {
        self.ingest(expected_server_name, doc, Ingest::Fetched(Source::Origin))
    }

    /// [`RemoteKeyCache::ingest_response`], for a response just fetched from the server or a
    /// notary (kept in the store, its fetch time recorded) or one restored from the store
    /// (neither).
    ///
    /// What the response says never shortens what is held: a key already held with a later
    /// `valid_until_ts` (or `expired_ts`) keeps it, so a notary's older copy of a document
    /// cannot undo what the server itself answered a moment ago (the server's answer is
    /// preferred when both arrive, whichever is ingested first), and a document whose
    /// `valid_until_ts` has passed still records its keys as valid until then: an event
    /// signed while the key was valid verifies against it (the point of asking a notary for
    /// a key the server rotated out years ago).
    fn ingest(
        &self,
        expected_server_name: &str,
        doc: &serde_json::Value,
        how: Ingest<'_>,
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

        // A key is cached as "current" only from a response that has not expired, and only
        // when nothing held says it is valid for longer; an expired response's keys go to the
        // retrospective (`old`) cache, valid until the response's `valid_until_ts`, so a
        // notary's copy of a document the server published years ago still verifies what was
        // signed while it was current.
        let now = now_ms();
        {
            let mut current = self
                .current
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let mut old = self
                .old
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            for (key_id, vk) in &candidates {
                let key = (expected_server_name.to_string(), key_id.clone());
                if now < valid_until_ts {
                    if current
                        .get(&key)
                        .is_none_or(|held| held.valid_until_ts <= valid_until_ts)
                    {
                        current.insert(
                            key,
                            CachedCurrent {
                                verifying_key: *vk,
                                valid_until_ts,
                            },
                        );
                    }
                } else if old
                    .get(&key)
                    .is_none_or(|held| held.expired_ts <= valid_until_ts)
                {
                    old.insert(
                        key,
                        CachedOld {
                            verifying_key: *vk,
                            expired_ts: valid_until_ts,
                        },
                    );
                }
            }
        }

        if let Some(old_verify_keys) = object
            .get("old_verify_keys")
            .and_then(CanonicalJsonValue::as_object)
        {
            let mut old = self
                .old
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
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
                    let key = (expected_server_name.to_string(), key_id.clone());
                    if old
                        .get(&key)
                        .is_none_or(|held| held.expired_ts <= expired_ts)
                    {
                        old.insert(
                            key,
                            CachedOld {
                                verifying_key: vk,
                                expired_ts,
                            },
                        );
                    }
                }
            }
        }

        // The document itself, for the notary endpoints, under every key it vouches for --
        // unless a later document is already held for that key.
        let stored = StoredResponse {
            valid_until_ts,
            doc: Arc::new(doc.clone()),
        };
        let mut newly_held: Vec<String> = Vec::new();
        {
            let mut responses = self
                .responses
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            for key_id in candidates.keys() {
                let key = (expected_server_name.to_string(), key_id.clone());
                if responses
                    .get(&key)
                    .is_none_or(|held| held.valid_until_ts <= valid_until_ts)
                {
                    responses.insert(key, stored.clone());
                    newly_held.push(key_id.clone());
                }
            }
        }
        let Ingest::Fetched(source) = how else {
            return Ok(());
        };
        if let Some(store) = &self.store {
            for key_id in &newly_held {
                store.hold(&crate::key_store::HeldKeyResponse {
                    server_name: expected_server_name.to_owned(),
                    key_id: key_id.clone(),
                    valid_until_ts,
                    doc: doc.clone(),
                });
            }
        }

        self.fetched_at
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(expected_server_name.to_string(), now);
        let mut key_ids: Vec<&String> = candidates.keys().collect();
        key_ids.sort();
        match source {
            Source::Origin => {
                crate::metrics::record_key_fetch("origin");
                tracing::debug!(
                    server = expected_server_name,
                    source = "origin",
                    ?key_ids,
                    valid_until_ts,
                    expired = now >= valid_until_ts,
                    "accepted a server's key response"
                );
            }
            Source::Notary(notary) => {
                crate::metrics::record_key_fetch("notary");
                tracing::info!(
                    server = expected_server_name,
                    source = "notary",
                    notary,
                    ?key_ids,
                    valid_until_ts,
                    expired = now >= valid_until_ts,
                    "accepted a server's key response from a notary"
                );
            }
        }
        Ok(())
    }
}

/// When [`RemoteKeyCache::refresh_origin`] asks the server.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Refresh {
    /// For a key not held: not while backed off from, and not again within
    /// [`ORIGIN_RECHECK_INTERVAL`] of its last answer, which did not list the key.
    Lookup,
    /// For a document fresher than the one held (the notary role, asked for a later
    /// `minimum_valid_until_ts`): not while backed off from; the recheck interval does not
    /// apply, since the last answer is exactly what is not fresh enough.
    Fresher,
    /// An administrator's refetch: now, whatever the backoff.
    Force,
}

/// Where a response [`RemoteKeyCache::ingest`] takes came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Ingest<'a> {
    /// Fetched (or handed in) now, from the server or a notary: kept in the store, its fetch
    /// time recorded.
    Fetched(Source<'a>),
    /// Read back from the store at boot: already kept, and not fetched now.
    Restored,
}

/// How many servers [`RemoteKeyCache::ensure_keys`] fetches from at once: the same as
/// `crate::outbound_join`'s verification concurrency, whose batch it fetches ahead of.
pub const JOIN_PREFETCH_CONCURRENCY: usize = 64;

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
    async fn the_cache_shows_what_it_holds_and_refetches_on_demand() {
        let fetcher = Arc::new(FixedFetcher::new());
        let (doc, keys) = signed_response("remote.example.org", 3600);
        fetcher.set("remote.example.org", doc);
        struct Shared(Arc<FixedFetcher>);
        #[async_trait]
        impl KeyServerFetcher for Shared {
            async fn fetch_server_key(&self, server_name: &str) -> Option<serde_json::Value> {
                self.0.fetch_server_key(server_name).await
            }
        }
        let cache = RemoteKeyCache::new(Shared(fetcher.clone()));
        assert_eq!(cache.cached_keys("remote.example.org"), None);

        let first = cache.refetch("remote.example.org").await.unwrap();
        assert_eq!(first.keys.len(), 1);
        assert_eq!(first.keys[0].key_id, keys.primary().key_id());
        assert_eq!(
            first.keys[0].public_key,
            keys.primary().verifying_key_base64()
        );
        assert!(!first.keys[0].old);
        assert!(first.fetched_at_ms.is_some());
        assert_eq!(cache.cached_keys("remote.example.org"), Some(first));

        // A refetch asks again even though the cached key is still good.
        cache.refetch("remote.example.org").await.unwrap();
        assert_eq!(fetcher.count_for("remote.example.org"), 2);
        // A server that cannot be reached is an error, and caches nothing.
        assert!(matches!(
            cache.refetch("gone.example.org").await.unwrap_err(),
            KeyLookupError::FetchFailed { ref server_name, .. } if server_name == "gone.example.org"
        ));
        assert_eq!(cache.cached_keys("gone.example.org"), None);
    }

    #[tokio::test]
    async fn own_keys_verify_without_a_fetch() {
        // An event this server signed itself -- echoed back in a `send_join`/`invite` response --
        // must verify against the seeded own key, never a fetch from this server (which, against a
        // real Synapse on 2026-10-09, failed with "could not fetch keys for server `<self>`").
        let dir = tempfile::tempdir().unwrap();
        let own = OwnSigningKeys::load_or_generate(dir.path()).unwrap();
        let fetcher = Arc::new(FixedFetcher::new());
        struct Shared(Arc<FixedFetcher>);
        #[async_trait]
        impl KeyServerFetcher for Shared {
            async fn fetch_server_key(&self, server_name: &str) -> Option<serde_json::Value> {
                self.0.fetch_server_key(server_name).await
            }
        }
        let cache = RemoteKeyCache::new(Shared(fetcher.clone()));
        cache.seed_own_keys("me.example.org", &own);
        let key_id = own.primary().key_id();

        // Current and at-a-past-timestamp both resolve to the seeded key.
        assert_eq!(
            cache.get_current("me.example.org", &key_id).await.unwrap(),
            own.primary().verifying_key()
        );
        assert_eq!(
            cache
                .get_valid_at("me.example.org", &key_id, 1)
                .await
                .unwrap(),
            own.primary().verifying_key()
        );
        // Nothing was fetched: the fetcher never saw our own name.
        assert_eq!(fetcher.count_for("me.example.org"), 0);
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
            KeyLookupError::FetchFailed { .. } | KeyLookupError::Unknown { .. }
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
        assert!(matches!(err, KeyLookupError::FetchFailed { .. }));
    }

    /// A fetcher that never answers for some servers (a server that is gone: the connection
    /// hangs until a timeout), and answers at once for the rest.
    struct StallingFetcher {
        inner: FixedFetcher,
        stalls: Vec<String>,
    }

    #[async_trait]
    impl KeyServerFetcher for StallingFetcher {
        async fn fetch_server_key(&self, server_name: &str) -> Option<serde_json::Value> {
            if self.stalls.iter().any(|s| s == server_name) {
                *self
                    .inner
                    .fetch_count
                    .lock()
                    .unwrap()
                    .entry(server_name.to_string())
                    .or_insert(0) += 1;
                std::future::pending::<()>().await;
            }
            self.inner.fetch_server_key(server_name).await
        }
    }

    /// A server whose fetch failed is not asked again until a backoff ends: 60 s after the
    /// first failure, doubling after each further one, and a success clears it. Until
    /// 2026-10-10 every lookup asked again, and a join of a large room asked each gone server
    /// once per event it had sent.
    #[tokio::test(start_paused = true)]
    async fn a_failed_fetch_is_backed_off_and_the_backoff_doubles_until_a_success_clears_it() {
        let fetcher = FixedFetcher::new();
        let cache = RemoteKeyCache::new(fetcher);
        let gone = "gone.example.org";
        async fn lookup(cache: &RemoteKeyCache<FixedFetcher>) -> KeyLookupError {
            cache
                .get_current("gone.example.org", "ed25519:a_1")
                .await
                .unwrap_err()
        }

        assert!(matches!(
            lookup(&cache).await,
            KeyLookupError::FetchFailed { .. }
        ));
        assert_eq!(cache.fetcher.count_for(gone), 1);
        let failure = cache
            .fetch_failure(gone)
            .expect("the failure is remembered");
        assert_eq!(failure.backoff, KEY_FETCH_BACKOFF_MIN);
        assert!(failure.reason.contains("could not be reached"));

        // Asked again at once: refused without a fetch, naming the failure and the wait.
        let err = lookup(&cache).await;
        match &err {
            KeyLookupError::FetchBackoff {
                server_name,
                reason,
                retry_in_secs,
                notaries,
            } => {
                assert_eq!(server_name, gone);
                assert!(reason.contains("could not be reached"));
                assert_eq!(*retry_in_secs, 60);
                assert!(notaries.contains("no notary is configured"), "{notaries}");
            }
            other => panic!("expected a backoff, got {other:?}"),
        }
        assert!(err.to_string().contains("not asking `gone.example.org`"));
        assert_eq!(cache.fetcher.count_for(gone), 1);

        // Just short of the backoff: still not asked. At it: asked, and the wait doubles.
        tokio::time::advance(Duration::from_secs(59)).await;
        assert!(matches!(
            lookup(&cache).await,
            KeyLookupError::FetchBackoff { .. }
        ));
        assert_eq!(cache.fetcher.count_for(gone), 1);
        tokio::time::advance(Duration::from_secs(1)).await;
        assert!(matches!(
            lookup(&cache).await,
            KeyLookupError::FetchFailed { .. }
        ));
        assert_eq!(cache.fetcher.count_for(gone), 2);
        assert_eq!(
            cache.fetch_failure(gone).unwrap().backoff,
            KEY_FETCH_BACKOFF_MIN * 2
        );
        tokio::time::advance(Duration::from_secs(60)).await;
        assert!(matches!(
            lookup(&cache).await,
            KeyLookupError::FetchBackoff { .. }
        ));
        assert_eq!(cache.fetcher.count_for(gone), 2);

        // The server comes back: the next attempt succeeds and the backoff is gone.
        let (doc, keys) = signed_response(gone, 3600);
        cache.fetcher.set(gone, doc);
        tokio::time::advance(Duration::from_secs(60)).await;
        cache
            .get_current(gone, &keys.primary().key_id())
            .await
            .unwrap();
        assert_eq!(cache.fetcher.count_for(gone), 3);
        assert!(cache.fetch_failure(gone).is_none());
    }

    /// The backoff never climbs past an hour.
    #[tokio::test(start_paused = true)]
    async fn the_fetch_backoff_is_capped_at_an_hour() {
        let cache = RemoteKeyCache::new(FixedFetcher::new());
        let gone = "gone.example.org";
        let mut backoff = Duration::ZERO;
        for _ in 0..10 {
            tokio::time::advance(backoff).await;
            let _ = cache.get_current(gone, "ed25519:a_1").await;
            backoff = cache.fetch_failure(gone).unwrap().backoff;
        }
        assert_eq!(backoff, KEY_FETCH_BACKOFF_MAX);
    }

    /// A fetch that takes longer than the budget is given up and counts as a failure (with the
    /// timeout as its reason), so a gone server costs the budget, not the client's 30 s request
    /// timeout, and only once.
    #[tokio::test(start_paused = true)]
    async fn a_fetch_over_the_budget_is_given_up_and_backed_off() {
        let gone = "gone.example.org";
        let fetcher = StallingFetcher {
            inner: FixedFetcher::new(),
            stalls: vec![gone.to_owned()],
        };
        let cache = RemoteKeyCache::new(fetcher);
        let before = tokio::time::Instant::now();
        let err = cache.get_current(gone, "ed25519:a_1").await.unwrap_err();
        assert!(matches!(err, KeyLookupError::FetchFailed { .. }), "{err}");
        assert_eq!(before.elapsed(), DEFAULT_KEY_FETCH_TIMEOUT);
        let failure = cache.fetch_failure(gone).unwrap();
        assert!(
            failure.reason.contains("timed out after 10 s"),
            "{}",
            failure.reason
        );

        let before = tokio::time::Instant::now();
        let err = cache.get_current(gone, "ed25519:a_1").await.unwrap_err();
        assert!(matches!(err, KeyLookupError::FetchBackoff { .. }), "{err}");
        assert_eq!(before.elapsed(), Duration::ZERO, "refused without a fetch");
        assert_eq!(cache.fetcher.inner.count_for(gone), 1);

        let quick = RemoteKeyCache::new(StallingFetcher {
            inner: FixedFetcher::new(),
            stalls: vec![gone.to_owned()],
        })
        .with_fetch_timeout(Duration::from_secs(2));
        let before = tokio::time::Instant::now();
        let _ = quick.get_current(gone, "ed25519:a_1").await;
        assert_eq!(before.elapsed(), Duration::from_secs(2));
    }

    /// Lookups that wait for one in-flight fetch share its outcome: when it fails, every
    /// waiter is answered with the backoff at once, instead of each fetching again in turn.
    /// Eight events from a gone server cost one budget, not eight.
    #[tokio::test(start_paused = true)]
    async fn waiters_on_one_in_flight_fetch_share_its_failure() {
        let gone = "gone.example.org";
        let cache = Arc::new(RemoteKeyCache::new(StallingFetcher {
            inner: FixedFetcher::new(),
            stalls: vec![gone.to_owned()],
        }));
        let before = tokio::time::Instant::now();
        let mut handles = Vec::new();
        for _ in 0..8 {
            let cache = cache.clone();
            handles.push(tokio::spawn(async move {
                cache.get_current(gone, "ed25519:a_1").await.unwrap_err()
            }));
        }
        let mut failed = 0;
        let mut backed_off = 0;
        for handle in handles {
            match handle.await.unwrap() {
                KeyLookupError::FetchFailed { .. } => failed += 1,
                KeyLookupError::FetchBackoff { .. } => backed_off += 1,
                other => panic!("unexpected {other:?}"),
            }
        }
        assert_eq!((failed, backed_off), (1, 7));
        assert_eq!(cache.fetcher.inner.count_for(gone), 1);
        assert_eq!(before.elapsed(), DEFAULT_KEY_FETCH_TIMEOUT);
    }

    /// Keys already held are served through a backoff: a failure to fetch a server's keys
    /// again (for a key it has never published) does not touch what is cached for it, and an
    /// administrator's refetch asks the server regardless of the backoff.
    #[tokio::test(start_paused = true)]
    async fn held_keys_are_served_through_a_backoff_and_refetch_ignores_it() {
        let server = "flaky.example.org";
        let fetcher = FixedFetcher::new();
        let (doc, keys) = signed_response(server, 3600);
        fetcher.set(server, doc);
        let cache = RemoteKeyCache::new(fetcher);
        let known = keys.primary().key_id();
        cache.get_current(server, &known).await.unwrap();
        assert_eq!(cache.fetcher.count_for(server), 1);

        // The server goes away, and an unknown key id forces a fetch that fails (once the
        // last answer is older than the recheck interval; until then what it published is
        // known not to include the key, without a fetch).
        cache.fetcher.responses.lock().unwrap().remove(server);
        let err = cache
            .get_current(server, "ed25519:never_published")
            .await
            .unwrap_err();
        assert!(matches!(err, KeyLookupError::Unknown { .. }), "{err}");
        assert!(err.to_string().contains("does not publish it"), "{err}");
        assert_eq!(cache.fetcher.count_for(server), 1);
        tokio::time::advance(ORIGIN_RECHECK_INTERVAL).await;
        let err = cache
            .get_current(server, "ed25519:never_published")
            .await
            .unwrap_err();
        assert!(matches!(err, KeyLookupError::FetchFailed { .. }), "{err}");
        assert!(cache.fetch_failure(server).is_some());

        // The held key still verifies, with no fetch.
        cache.get_current(server, &known).await.unwrap();
        assert_eq!(cache.fetcher.count_for(server), 2);
        assert!(
            cache
                .cached_keys(server)
                .is_some_and(|held| held.keys.iter().any(|k| k.key_id == known))
        );

        // The administrator's refetch goes through the backoff, and clears it on success.
        let err = cache.refetch(server).await.unwrap_err();
        assert!(matches!(err, KeyLookupError::FetchFailed { .. }), "{err}");
        assert_eq!(cache.fetcher.count_for(server), 3);
        let (doc, _) = signed_response(server, 3600);
        cache.fetcher.set(server, doc);
        cache.refetch(server).await.unwrap();
        assert!(cache.fetch_failure(server).is_none());
    }

    /// The key responses a cache accepts are kept in its store, and a cache built over the same
    /// store after a restart starts with them: the notary answers for a server that cannot be
    /// reached any more, and a key verifies without a fetch. A response that expired more than
    /// Forgetting a server drops its keys from the cache and from the held-key store, so a
    /// restart does not bring them back; a later signature of it fetches afresh; other servers'
    /// keys and this server's own are untouched.
    #[tokio::test]
    async fn a_forgotten_servers_keys_are_gone_from_the_cache_and_the_store() {
        let backend = hs_kv::memory::MemoryBackend::new();
        let store: Arc<dyn crate::key_store::HeldKeyStore> =
            Arc::new(crate::key_store::KvHeldKeyStore::open(backend.clone()).unwrap());
        let fetcher = FixedFetcher::new();
        let (doc, keys) = signed_response("gone.example.org", 3600);
        fetcher.set("gone.example.org", doc);
        let (kept_doc, kept_keys) = signed_response("kept.example.org", 3600);
        fetcher.set("kept.example.org", kept_doc);
        let cache = RemoteKeyCache::with_store(fetcher, store.clone());
        cache
            .get_current("gone.example.org", &keys.primary().key_id())
            .await
            .unwrap();
        cache
            .get_current("kept.example.org", &kept_keys.primary().key_id())
            .await
            .unwrap();
        assert_eq!(store.load().len(), 2);

        assert_eq!(cache.forget_server("gone.example.org"), 1);
        assert_eq!(cache.cached_keys("gone.example.org"), None);
        assert!(cache.cached_keys("kept.example.org").is_some());
        assert_eq!(
            store.load().len(),
            1,
            "only the kept server's response is held"
        );
        assert_eq!(store.load()[0].server_name, "kept.example.org");
        assert_eq!(
            cache.forget_server("gone.example.org"),
            0,
            "nothing left to drop"
        );

        // The next signature of it fetches again.
        cache
            .get_current("gone.example.org", &keys.primary().key_id())
            .await
            .unwrap();
        assert_eq!(cache.fetcher.count_for("gone.example.org"), 2);
    }

    /// a year ago is forgotten at boot. Until 2026-10-01 all of it was in memory only, and a
    /// restarted notary answered nothing for a server that was down.
    #[tokio::test]
    async fn held_key_responses_survive_a_restart_and_long_expired_ones_are_forgotten() {
        let backend = hs_kv::memory::MemoryBackend::new();
        let store: Arc<dyn crate::key_store::HeldKeyStore> =
            Arc::new(crate::key_store::KvHeldKeyStore::open(backend.clone()).unwrap());
        let fetcher = FixedFetcher::new();
        let (doc, keys) = signed_response("remote.example.org", 3600);
        fetcher.set("remote.example.org", doc.clone());
        let first = RemoteKeyCache::with_store(fetcher, store.clone());
        first
            .get_current("remote.example.org", &keys.primary().key_id())
            .await
            .unwrap();
        assert_eq!(store.load().len(), 1, "the accepted response is kept");
        // A response long expired, as a server gone for over a year left it.
        store.hold(&crate::key_store::HeldKeyResponse {
            server_name: "gone.example.org".to_owned(),
            key_id: "ed25519:old".to_owned(),
            valid_until_ts: 1,
            doc: serde_json::json!({"server_name": "gone.example.org"}),
        });
        drop(first);

        // The restart: the same store, and the server can no longer be reached.
        let store: Arc<dyn crate::key_store::HeldKeyStore> =
            Arc::new(crate::key_store::KvHeldKeyStore::open(backend).unwrap());
        let restarted = RemoteKeyCache::with_store(FixedFetcher::new(), store.clone());
        let answered = restarted
            .notary_responses("remote.example.org", &[], 0)
            .await;
        assert_eq!(answered, vec![doc]);
        let key = restarted
            .get_current("remote.example.org", &keys.primary().key_id())
            .await
            .expect("verifies from what was kept, with no fetch");
        assert_eq!(key, keys.primary().verifying_key());
        assert_eq!(restarted.fetcher.count_for("remote.example.org"), 0);
        assert!(
            store
                .load()
                .iter()
                .all(|held| held.server_name != "gone.example.org"),
            "a response expired over a year ago is forgotten"
        );
    }

    // --- Notaries ------------------------------------------------------------------------

    /// A fetcher that plays the origin servers (`FixedFetcher`) and one or more notaries: each
    /// notary answers every query with the documents scripted for it, co-signed with its key
    /// ([`wrap_for_notary`]), or with nothing when it is scripted as down. Every query body is
    /// kept, so a test can see what was asked.
    struct NotaryFetcher {
        origins: FixedFetcher,
        /// Notary name to (its keys, the origin documents it holds); `None` keys: down.
        notaries: StdMutex<HashMap<String, FakeNotary>>,
        queries: StdMutex<Vec<(String, serde_json::Value)>>,
        /// An answer that replaces the scripted one once (a tampered document, say).
        canned: StdMutex<Option<serde_json::Value>>,
    }

    /// A fake notary's keys (`None`: the notary is down) and the origin documents it holds.
    type FakeNotary = (Option<OwnSigningKeys>, Vec<serde_json::Value>);

    impl NotaryFetcher {
        fn new() -> Self {
            Self {
                origins: FixedFetcher::new(),
                notaries: StdMutex::new(HashMap::new()),
                queries: StdMutex::new(Vec::new()),
                canned: StdMutex::new(None),
            }
        }
        fn notary_holds(&self, notary: &str, keys: OwnSigningKeys, docs: Vec<serde_json::Value>) {
            self.notaries
                .lock()
                .unwrap()
                .insert(notary.to_owned(), (Some(keys), docs));
        }
        fn notary_down(&self, notary: &str) {
            self.notaries
                .lock()
                .unwrap()
                .insert(notary.to_owned(), (None, Vec::new()));
        }
        fn queries(&self) -> Vec<(String, serde_json::Value)> {
            self.queries.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl KeyServerFetcher for NotaryFetcher {
        async fn fetch_server_key(&self, server_name: &str) -> Option<serde_json::Value> {
            self.origins.fetch_server_key(server_name).await
        }
        async fn query_notary(
            &self,
            notary: &str,
            body: &serde_json::Value,
        ) -> Option<serde_json::Value> {
            self.queries
                .lock()
                .unwrap()
                .push((notary.to_owned(), body.clone()));
            if let Some(canned) = self.canned.lock().unwrap().take() {
                return Some(canned);
            }
            let notaries = self.notaries.lock().unwrap();
            let (keys, docs) = notaries.get(notary)?;
            let keys = keys.as_ref()?;
            let asked = body["server_keys"].as_object()?;
            let server_keys: Vec<serde_json::Value> = docs
                .iter()
                .filter(|doc| asked.contains_key(doc["server_name"].as_str().unwrap_or("")))
                .map(|doc| wrap_for_notary(doc, notary, keys).unwrap())
                .collect();
            Some(serde_json::json!({ "server_keys": server_keys }))
        }
    }

    fn new_keys() -> OwnSigningKeys {
        let dir = tempfile::tempdir().unwrap();
        OwnSigningKeys::load_or_generate(dir.path()).unwrap()
    }

    /// `notary`'s configuration entry, naming its primary key.
    fn trusted(notary: &str, keys: &OwnSigningKeys) -> TrustedKeyServer {
        TrustedKeyServer::new(
            notary,
            [(
                keys.primary().key_id(),
                keys.primary().verifying_key_base64(),
            )],
        )
        .unwrap()
    }

    /// A key response for `server` signed by `keys`, valid until `valid_until_ts` (a timestamp,
    /// not a duration: the response may already be expired).
    fn response_valid_until(
        server: &str,
        keys: &OwnSigningKeys,
        valid_until_ts: u64,
    ) -> serde_json::Value {
        let body = serde_json::json!({
            "server_name": server,
            "verify_keys": keys.verify_keys_json(),
            "old_verify_keys": {},
            "valid_until_ts": valid_until_ts,
        });
        let mut object = signing::to_signable_object(&body).unwrap();
        let name = ruma::ServerName::parse(server).unwrap();
        for key in keys.all() {
            signing::sign_object(&mut object, name.as_ref(), key).unwrap();
        }
        serde_json::from_slice(&CanonicalJsonValue::Object(object).to_canonical_bytes()).unwrap()
    }

    /// The scenario of 2026-10-10: `#selfhosted:selfhosted.chat`'s create event is signed with
    /// a key the server rotated out and no longer publishes; matrix.org's notary still has the
    /// document that listed it. The origin's `/key/v2/server` carries only the new key, and the
    /// old one is found at the notary, co-signed by the notary and self-signed by the origin;
    /// an event signed while it was valid verifies against it. Counted as a notary fetch.
    #[tokio::test]
    async fn a_rotated_out_key_found_only_at_the_notary_verifies_an_old_event() {
        let fetcher = NotaryFetcher::new();
        let origin = "selfhosted.example.org";
        let old_keys = new_keys();
        let current_keys = new_keys();
        let old_doc = response_valid_until(origin, &old_keys, 2_000_000);
        fetcher.origins.set(
            origin,
            build_server_key_response(origin, &current_keys, &[], 3600).unwrap(),
        );
        let notary_keys = new_keys();
        fetcher.notary_holds(
            "notary.example.org",
            notary_keys.clone_for_test(),
            vec![old_doc],
        );
        let cache = RemoteKeyCache::new(fetcher)
            .with_trusted_key_servers(vec![trusted("notary.example.org", &notary_keys)]);

        let before = crate::metrics::key_fetches("notary");
        let key = cache
            .get_valid_at(origin, &old_keys.primary().key_id(), 1_500_000)
            .await
            .expect("the notary had the rotated-out key");
        assert_eq!(key, old_keys.primary().verifying_key());
        assert!(
            crate::metrics::key_fetches("notary") > before,
            "counted (the counter is process-wide)"
        );
        // The origin was asked too (concurrently), and its current key is held as well.
        assert_eq!(cache.fetcher.origins.count_for(origin), 1);
        assert_eq!(
            cache.fetcher.queries().len(),
            1,
            "{:?}",
            cache.fetcher.queries()
        );
        cache
            .get_current(origin, &current_keys.primary().key_id())
            .await
            .expect("the origin's current key, from the same pass");
        // The rotated-out key is held as an old one: not usable for anything current.
        let held = cache.cached_keys(origin).unwrap();
        let old = held
            .keys
            .iter()
            .find(|k| k.key_id == old_keys.primary().key_id())
            .unwrap();
        assert!(old.old);
        assert_eq!(old.valid_until_ts, 2_000_000);
    }

    /// An answer the notary did not sign with a key this server was configured with is worth
    /// nothing, however well the origin signed what is inside it: a notary is trusted by its
    /// key, not by its name. The error says the notary had nothing usable.
    #[tokio::test]
    async fn a_notary_answer_with_a_bad_notary_signature_is_rejected() {
        let fetcher = NotaryFetcher::new();
        let origin = "origin.example.org";
        let old_keys = new_keys();
        let doc = response_valid_until(origin, &old_keys, 2_000_000);
        let signing_with = new_keys();
        fetcher.notary_holds("notary.example.org", signing_with, vec![doc]);
        let configured = new_keys();
        let cache = RemoteKeyCache::new(fetcher)
            .with_trusted_key_servers(vec![trusted("notary.example.org", &configured)]);

        let err = cache
            .get_valid_at(origin, &old_keys.primary().key_id(), 1_500_000)
            .await
            .unwrap_err();
        assert!(
            matches!(err, KeyLookupError::FetchFailed { .. }),
            "the origin is gone too: {err}"
        );
        let message = err.to_string();
        assert!(
            message.contains("notary notary.example.org could not be asked")
                && message.contains("every document in the notary's answer was refused"),
            "{message}"
        );
        assert_eq!(cache.cached_keys(origin), None);
        assert!(cache.notary_failure("notary.example.org").is_some());
    }

    /// A document inside a well-signed notary answer that is not signed by the server it is
    /// about (tampered after the origin signed it, or forged) is refused: the spec's rule is
    /// both signatures, and a notary cannot mint keys for another server.
    #[tokio::test]
    async fn a_bad_origin_signature_inside_a_notary_answer_is_rejected() {
        let fetcher = NotaryFetcher::new();
        let origin = "origin.example.org";
        let old_keys = new_keys();
        let mut doc = response_valid_until(origin, &old_keys, 2_000_000);
        // The notary (or someone between) swaps the key material after the origin signed.
        let forged = new_keys();
        doc["verify_keys"][old_keys.primary().key_id()]["key"] =
            serde_json::Value::String(forged.primary().verifying_key_base64());
        let notary_keys = new_keys();
        fetcher.notary_holds(
            "notary.example.org",
            notary_keys.clone_for_test(),
            vec![doc],
        );
        let cache = RemoteKeyCache::new(fetcher)
            .with_trusted_key_servers(vec![trusted("notary.example.org", &notary_keys)]);

        let err = cache
            .get_valid_at(origin, &old_keys.primary().key_id(), 1_500_000)
            .await
            .unwrap_err();
        assert!(
            err.to_string()
                .contains("every document in the notary's answer was refused"),
            "{err}"
        );
        assert_eq!(cache.cached_keys(origin), None);
    }

    /// A key from an expired document (the notary's copy of what the server published years
    /// ago) verifies an event from before the document's `valid_until_ts`, and not one after:
    /// that is what asking a notary is for, and the limit on it.
    #[tokio::test]
    async fn an_expired_key_from_a_notary_verifies_before_its_expiry_and_not_after() {
        let fetcher = NotaryFetcher::new();
        let origin = "origin.example.org";
        let old_keys = new_keys();
        let expired_ts = 2_000_000;
        let doc = response_valid_until(origin, &old_keys, expired_ts);
        let notary_keys = new_keys();
        fetcher.notary_holds(
            "notary.example.org",
            notary_keys.clone_for_test(),
            vec![doc],
        );
        let cache = RemoteKeyCache::new(fetcher)
            .with_trusted_key_servers(vec![trusted("notary.example.org", &notary_keys)]);
        let key_id = old_keys.primary().key_id();

        cache
            .get_valid_at(origin, &key_id, expired_ts - 1)
            .await
            .expect("signed while the key was valid");
        cache
            .get_valid_at(origin, &key_id, expired_ts)
            .await
            .expect("signed at the last valid moment");
        let err = cache
            .get_valid_at(origin, &key_id, expired_ts + 1)
            .await
            .unwrap_err();
        assert!(matches!(err, KeyLookupError::Expired { .. }), "{err}");
        // Not current: the origin (gone) is backed off from, and the notary had nothing fresher.
        let err = cache.get_current(origin, &key_id).await.unwrap_err();
        assert!(
            matches!(err, KeyLookupError::FetchBackoff { .. }),
            "not current: {err}"
        );
        assert!(
            err.to_string()
                .contains("notary notary.example.org had nothing for it"),
            "{err}"
        );
    }

    /// `ensure_keys` asks each notary once for every key still missing, not once per key:
    /// one query naming every server and key, each with the timestamp it must cover. Keys
    /// already held are not asked about, and the notary is not asked about its own keys.
    #[tokio::test]
    async fn the_batch_query_carries_every_miss_in_one_request_per_notary() {
        let fetcher = NotaryFetcher::new();
        let held_keys = new_keys();
        fetcher.origins.set(
            "held.example.org",
            build_server_key_response("held.example.org", &held_keys, &[], 3600).unwrap(),
        );
        let notary_keys = new_keys();
        fetcher.notary_holds("notary.example.org", notary_keys.clone_for_test(), vec![]);
        let cache = RemoteKeyCache::new(fetcher)
            .with_trusted_key_servers(vec![trusted("notary.example.org", &notary_keys)]);
        cache
            .get_current("held.example.org", &held_keys.primary().key_id())
            .await
            .unwrap();
        assert_eq!(
            cache.fetcher.queries().len(),
            1,
            "one query for the held key's pass"
        );

        let wanted = vec![
            WantedKey {
                server_name: "held.example.org".to_owned(),
                key_id: held_keys.primary().key_id(),
                signed_at_ts: 5,
            },
            WantedKey {
                server_name: "a.example.org".to_owned(),
                key_id: "ed25519:a1".to_owned(),
                signed_at_ts: 10,
            },
            WantedKey {
                server_name: "a.example.org".to_owned(),
                key_id: "ed25519:a2".to_owned(),
                signed_at_ts: 20,
            },
            WantedKey {
                server_name: "b.example.org".to_owned(),
                key_id: "ed25519:b1".to_owned(),
                signed_at_ts: 30,
            },
            WantedKey {
                server_name: "notary.example.org".to_owned(),
                key_id: "ed25519:n1".to_owned(),
                signed_at_ts: 40,
            },
        ];
        cache.ensure_keys(&wanted).await;
        let queries = cache.fetcher.queries();
        assert_eq!(queries.len(), 2, "{queries:?}");
        let (notary, body) = &queries[1];
        assert_eq!(notary, "notary.example.org");
        assert_eq!(
            body,
            &serde_json::json!({"server_keys": {
                "a.example.org": {
                    "ed25519:a1": {"minimum_valid_until_ts": 10},
                    "ed25519:a2": {"minimum_valid_until_ts": 20},
                },
                "b.example.org": {"ed25519:b1": {"minimum_valid_until_ts": 30}},
            }})
        );
        // Each origin was asked too, once.
        assert_eq!(cache.fetcher.origins.count_for("a.example.org"), 1);
        assert_eq!(cache.fetcher.origins.count_for("b.example.org"), 1);
        assert_eq!(cache.fetcher.origins.count_for("held.example.org"), 1);

        // The lookups that follow are refused at once, naming what was asked, with no further
        // query: the origins are backed off from and the notary had nothing.
        let err = cache
            .get_valid_at("a.example.org", "ed25519:a1", 10)
            .await
            .unwrap_err();
        assert!(matches!(err, KeyLookupError::FetchBackoff { .. }), "{err}");
        assert!(
            err.to_string()
                .contains("notary notary.example.org had nothing for it"),
            "{err}"
        );
        assert_eq!(cache.fetcher.queries().len(), 2);
        assert_eq!(cache.fetcher.origins.count_for("a.example.org"), 1);
    }

    /// When the origin and the notary both answer, what the origin said wins: its document is
    /// the fresher one, and a notary's older copy does not shorten the validity held.
    #[tokio::test]
    async fn the_origin_is_preferred_when_both_answer() {
        let fetcher = NotaryFetcher::new();
        let origin = "origin.example.org";
        let keys = new_keys();
        let fresh = now_ms() + 3_600_000;
        fetcher
            .origins
            .set(origin, response_valid_until(origin, &keys, fresh));
        let notary_keys = new_keys();
        fetcher.notary_holds(
            "notary.example.org",
            notary_keys.clone_for_test(),
            vec![response_valid_until(origin, &keys, fresh - 1_000_000)],
        );
        let cache = RemoteKeyCache::new(fetcher)
            .with_trusted_key_servers(vec![trusted("notary.example.org", &notary_keys)]);
        let before = crate::metrics::key_fetches("origin");
        cache
            .get_current(origin, &keys.primary().key_id())
            .await
            .unwrap();
        assert_eq!(cache.fetcher.origins.count_for(origin), 1);
        assert_eq!(cache.fetcher.queries().len(), 1, "both were asked");
        assert!(
            crate::metrics::key_fetches("origin") > before,
            "counted (the counter is process-wide)"
        );
        let held = cache.cached_keys(origin).unwrap();
        assert_eq!(held.keys.len(), 1);
        assert_eq!(held.keys[0].valid_until_ts, fresh, "the origin's validity");
        assert_eq!(
            cache.notary_responses(origin, &[], 0).await[0]["valid_until_ts"],
            fresh,
            "and the origin's document is what this server's notary passes on"
        );
    }

    /// A notary that cannot be reached is backed off from like a server, counted under the
    /// `notary` failure reason, and the error for a key says so; the origin's answer is
    /// unaffected.
    #[tokio::test(start_paused = true)]
    async fn a_notary_that_does_not_answer_is_backed_off_and_named() {
        let fetcher = NotaryFetcher::new();
        fetcher.notary_down("notary.example.org");
        let notary_keys = new_keys();
        let cache = RemoteKeyCache::new(fetcher)
            .with_trusted_key_servers(vec![trusted("notary.example.org", &notary_keys)]);
        let before = crate::metrics::key_fetch_failures("notary");
        let err = cache
            .get_valid_at("gone.example.org", "ed25519:x", 1)
            .await
            .unwrap_err();
        assert!(matches!(err, KeyLookupError::FetchFailed { .. }), "{err}");
        assert!(
            err.to_string().contains(
                "notary notary.example.org could not be asked (the notary could not be reached"
            ),
            "{err}"
        );
        assert!(
            crate::metrics::key_fetch_failures("notary") > before,
            "counted (the counter is process-wide)"
        );
        assert_eq!(
            cache.notary_failure("notary.example.org").unwrap().backoff,
            KEY_FETCH_BACKOFF_MIN
        );
        // Not asked again while backed off: the next lookup makes no query.
        let _ = cache
            .get_valid_at("other.example.org", "ed25519:y", 1)
            .await;
        assert_eq!(cache.fetcher.queries().len(), 1);
        tokio::time::advance(KEY_FETCH_BACKOFF_MIN).await;
        let _ = cache
            .get_valid_at("third.example.org", "ed25519:z", 1)
            .await;
        assert_eq!(cache.fetcher.queries().len(), 2);
    }

    /// Replacing the notaries on a running cache takes effect on the next pass: an empty list
    /// turns notary lookups off, and the error says no notary is configured.
    #[tokio::test]
    async fn the_notaries_can_be_replaced_on_a_running_cache() {
        let fetcher = NotaryFetcher::new();
        let notary_keys = new_keys();
        fetcher.notary_holds("notary.example.org", notary_keys.clone_for_test(), vec![]);
        let cache = RemoteKeyCache::new(fetcher)
            .with_trusted_key_servers(vec![trusted("notary.example.org", &notary_keys)]);
        assert_eq!(cache.trusted_key_servers(), vec!["notary.example.org"]);
        let _ = cache.get_valid_at("a.example.org", "ed25519:a", 1).await;
        assert_eq!(cache.fetcher.queries().len(), 1);

        cache.set_trusted_key_servers(Vec::new());
        assert!(cache.trusted_key_servers().is_empty());
        let err = cache
            .get_valid_at("b.example.org", "ed25519:b", 1)
            .await
            .unwrap_err();
        assert_eq!(cache.fetcher.queries().len(), 1, "nobody to ask");
        assert!(err.to_string().contains("no notary is configured"), "{err}");
    }

    /// The join of 2026-10-10: a server that is gone (its key fetch fails) signed events the
    /// notary still has the key for. The notary's answer is taken, the event verifies, and the
    /// gone server is backed off from: the next key of the same server is not asked of it again
    /// but is asked of the notary, which keeps a join's events from a gone server verifiable
    /// instead of dropped.
    #[tokio::test]
    async fn a_gone_servers_key_is_found_at_the_notary_and_the_server_is_backed_off() {
        let fetcher = NotaryFetcher::new();
        let gone = "gone.example.org";
        let gone_keys = new_keys();
        let other_keys = new_keys();
        let notary_keys = new_keys();
        fetcher.notary_holds(
            "notary.example.org",
            notary_keys.clone_for_test(),
            vec![
                response_valid_until(gone, &gone_keys, 2_000_000),
                response_valid_until(gone, &other_keys, 3_000_000),
            ],
        );
        let cache = RemoteKeyCache::new(fetcher)
            .with_trusted_key_servers(vec![trusted("notary.example.org", &notary_keys)]);

        let key = cache
            .get_valid_at(gone, &gone_keys.primary().key_id(), 1_500_000)
            .await
            .expect("the notary had the gone server's key");
        assert_eq!(key, gone_keys.primary().verifying_key());
        assert_eq!(
            cache.fetcher.origins.count_for(gone),
            1,
            "the server was asked once"
        );
        assert!(
            cache.fetch_failure(gone).is_some(),
            "and is backed off from, having not answered"
        );

        // The negative cache is consulted before anything is fetched: the server is not asked
        // again. The notary's one answer carried every document it held for the server, so the
        // other key is held already and costs no query either.
        cache
            .get_valid_at(gone, &other_keys.primary().key_id(), 2_500_000)
            .await
            .expect("the notary's answer carried this key too");
        assert_eq!(cache.fetcher.origins.count_for(gone), 1, "not asked again");
        assert_eq!(cache.fetcher.queries().len(), 1);

        // A key nobody has: the notary is asked once (the server is not), and the error names
        // the backoff and the notary's answer; the lookup after that is refused at once, since
        // the notary's miss is remembered too.
        let err = cache
            .get_valid_at(gone, "ed25519:nobody", 1)
            .await
            .unwrap_err();
        assert!(matches!(err, KeyLookupError::Unknown { .. }), "{err}");
        let message = err.to_string();
        assert!(
            message.contains("its last key fetch failed")
                && message.contains("notary notary.example.org had nothing for it"),
            "{message}"
        );
        assert_eq!(cache.fetcher.origins.count_for(gone), 1);
        assert_eq!(cache.fetcher.queries().len(), 2);
        let err = cache
            .get_valid_at(gone, "ed25519:nobody", 1)
            .await
            .unwrap_err();
        assert!(matches!(err, KeyLookupError::FetchBackoff { .. }), "{err}");
        assert_eq!(cache.fetcher.queries().len(), 2, "nobody asked again");
    }

    /// With two notaries configured, one that is down does not cost the key: the other's
    /// answer is taken. The one that is down is backed off from and named in the log and the
    /// error; the next pass asks only the one that answers.
    #[tokio::test]
    async fn a_notary_that_is_down_falls_back_to_the_next() {
        let fetcher = NotaryFetcher::new();
        let origin = "origin.example.org";
        let old_keys = new_keys();
        fetcher.notary_down("down.example.org");
        let up_keys = new_keys();
        fetcher.notary_holds(
            "up.example.org",
            up_keys.clone_for_test(),
            vec![response_valid_until(origin, &old_keys, 2_000_000)],
        );
        let down_keys = new_keys();
        let cache = RemoteKeyCache::new(fetcher).with_trusted_key_servers(vec![
            trusted("down.example.org", &down_keys),
            trusted("up.example.org", &up_keys),
        ]);

        let key = cache
            .get_valid_at(origin, &old_keys.primary().key_id(), 1_500_000)
            .await
            .expect("the second notary had it");
        assert_eq!(key, old_keys.primary().verifying_key());
        let asked: Vec<String> = cache
            .fetcher
            .queries()
            .into_iter()
            .map(|(notary, _)| notary)
            .collect();
        assert_eq!(asked.len(), 2, "both were asked, concurrently: {asked:?}");
        assert!(asked.contains(&"down.example.org".to_owned()));
        assert!(asked.contains(&"up.example.org".to_owned()));
        assert!(cache.notary_failure("down.example.org").is_some());
        assert!(cache.notary_failure("up.example.org").is_none());

        // The next key: only the notary that answers is asked.
        let _ = cache
            .get_valid_at("other.example.org", "ed25519:o", 1)
            .await;
        let asked: Vec<String> = cache
            .fetcher
            .queries()
            .into_iter()
            .skip(2)
            .map(|(notary, _)| notary)
            .collect();
        assert_eq!(asked, vec!["up.example.org".to_owned()]);
    }

    /// `federation.key_fetch_timeout` changed on the running server: the next fetch has the new
    /// budget.
    #[tokio::test(start_paused = true)]
    async fn the_fetch_budget_can_be_changed_on_a_running_cache() {
        let cache = RemoteKeyCache::new(StallingFetcher {
            inner: FixedFetcher::new(),
            stalls: vec!["slow.example.org".to_owned()],
        });
        assert_eq!(cache.fetch_timeout(), DEFAULT_KEY_FETCH_TIMEOUT);
        cache.set_fetch_timeout(Duration::from_secs(2));
        assert_eq!(cache.fetch_timeout(), Duration::from_secs(2));
        let started = tokio::time::Instant::now();
        let err = cache
            .get_current("slow.example.org", "ed25519:x")
            .await
            .unwrap_err();
        assert_eq!(started.elapsed(), Duration::from_secs(2));
        assert!(err.to_string().contains("timed out after 2 s"), "{err}");
    }
}
