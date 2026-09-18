//! The verdict cache (`docs/rfcs/0008-content-scanning.md`, section 6): keyed on
//! `(sha256(content), provider_id, engine_version)`, stored through `hs-tables`, with the
//! configured capacity and time to live. A signature update changes `engine_version`, which is
//! part of the key, so prior verdicts are invalidated for free — no explicit purge needed (see
//! `tests::signature_version_change_invalidates_the_cache`).
//!
//! Only terminal verdicts ([`Verdict::Clean`], [`Verdict::Infected`], [`Verdict::Unscannable`])
//! are ever cached; [`Verdict::Pending`] is never stored (a ticket is meaningless once a real
//! verdict lands, and the same content may resolve differently across two separate uploads if a
//! provider's judgment is unstable, so keeping a stale ticket around serves no purpose).
//!
//! # A hit must never count as a scan
//!
//! RFC section 6: "a cache hit must never be reported as a scan in metrics, or the metrics lie
//! about scanner load." This module itself does not touch metrics at all — `crate::scanning::engine`
//! is the only caller, and it is structured so a cache hit short-circuits before any
//! `ScanMetrics::record_scan` call. See `engine`'s tests for the assertion that this holds.

use hs_kv::{KvBackend, RangeSpec, TransactConfig, transact};
use hs_tables::keyspace::TypedKeyspace;
use serde::{Deserialize, Serialize};

use crate::error::MediaError;
use crate::scanning::config::CacheConfig;
use crate::scanning::types::{UnscannableReason, Verdict};

/// The subset of [`Verdict`] this cache stores — [`Verdict::Pending`] is deliberately not
/// representable here (see the module doc).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum CachedVerdict {
    /// See [`Verdict::Clean`].
    Clean,
    /// See [`Verdict::Infected`].
    Infected {
        /// The signature name.
        signature: String,
        /// Additional detail, if any.
        details: Option<String>,
    },
    /// See [`Verdict::Unscannable`].
    Unscannable {
        /// Why.
        reason: StoredUnscannableReason,
    },
}

/// [`UnscannableReason`], made `Serialize`/`Deserialize` for storage (the live type intentionally
/// is not, to keep `crate::scanning::types` a pure interface module with no storage concerns).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum StoredUnscannableReason {
    /// See [`UnscannableReason::Encrypted`].
    Encrypted,
    /// See [`UnscannableReason::TooLarge`].
    TooLarge,
    /// See [`UnscannableReason::TooDeep`].
    TooDeep,
    /// See [`UnscannableReason::UnsupportedFormat`].
    UnsupportedFormat,
    /// See [`UnscannableReason::Other`].
    Other(String),
}

impl From<&UnscannableReason> for StoredUnscannableReason {
    fn from(r: &UnscannableReason) -> Self {
        match r {
            UnscannableReason::Encrypted => StoredUnscannableReason::Encrypted,
            UnscannableReason::TooLarge => StoredUnscannableReason::TooLarge,
            UnscannableReason::TooDeep => StoredUnscannableReason::TooDeep,
            UnscannableReason::UnsupportedFormat => StoredUnscannableReason::UnsupportedFormat,
            UnscannableReason::Other(s) => StoredUnscannableReason::Other(s.clone()),
        }
    }
}

impl From<StoredUnscannableReason> for UnscannableReason {
    fn from(r: StoredUnscannableReason) -> Self {
        match r {
            StoredUnscannableReason::Encrypted => UnscannableReason::Encrypted,
            StoredUnscannableReason::TooLarge => UnscannableReason::TooLarge,
            StoredUnscannableReason::TooDeep => UnscannableReason::TooDeep,
            StoredUnscannableReason::UnsupportedFormat => UnscannableReason::UnsupportedFormat,
            StoredUnscannableReason::Other(s) => UnscannableReason::Other(s),
        }
    }
}

impl CachedVerdict {
    /// Converts a live [`Verdict`] to its cacheable form, or `None` for [`Verdict::Pending`]
    /// (never cached — see the module doc).
    #[must_use]
    pub fn from_verdict(v: &Verdict) -> Option<Self> {
        match v {
            Verdict::Clean => Some(CachedVerdict::Clean),
            Verdict::Infected { signature, details } => Some(CachedVerdict::Infected {
                signature: signature.clone(),
                details: details.clone(),
            }),
            Verdict::Unscannable { reason } => Some(CachedVerdict::Unscannable {
                reason: reason.into(),
            }),
            Verdict::Pending { .. } => None,
        }
    }

    /// Converts back to a live [`Verdict`].
    #[must_use]
    pub fn into_verdict(self) -> Verdict {
        match self {
            CachedVerdict::Clean => Verdict::Clean,
            CachedVerdict::Infected { signature, details } => {
                Verdict::Infected { signature, details }
            }
            CachedVerdict::Unscannable { reason } => Verdict::Unscannable {
                reason: reason.into(),
            },
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Entry {
    verdict: CachedVerdict,
    cached_at_ms: u64,
    ttl_ms: u64,
    seq: u64,
}

type EntryKey = (String, String, String); // (sha256_hex, provider_id, version_key)
type OrderKey = (u64, u64); // (cached_at_ms, seq) -- eviction order, oldest first

/// The verdict cache over one `hs-kv` backend.
#[derive(Clone)]
pub struct VerdictCache<B: KvBackend> {
    backend: B,
    entries: TypedKeyspace<B::Keyspace, EntryKey>,
    order: TypedKeyspace<B::Keyspace, OrderKey>,
    /// A separate, untyped keyspace for the monotonic insertion-sequence counter
    /// ([`SEQ_KEY`]) that [`VerdictCache::put`] uses to break ties in the eviction order.
    /// Deliberately not the same keyspace as `order` or `entries`: those hold only
    /// [`hs_tables`] tuple-encoded rows, and mixing a raw scalar counter key into either would
    /// make every "is this a tuple-encoded key" assumption in this module (and any future range
    /// scan over it) an exception rather than a rule.
    meta: B::Keyspace,
    capacity: usize,
    ttl_ms: u64,
    unversioned_ttl_ms: u64,
}

/// The literal `version_key` used when a provider cannot report an `engine_version`
/// ([`crate::scanning::types::ContentScanner::engine_version`] returned `None`). A real
/// `engine_version` string equal to this exact sentinel would collide; every provider in this
/// crate returns either `None` or a version derived from the scanner itself (a signature date, an
/// `ISTag`, ...), never this literal string, so this is treated as safe in practice rather than
/// unreachable in principle.
const UNVERSIONED: &str = "\u{0}unversioned\u{0}";

/// The `hs-kv` key at which this cache's monotonic insertion sequence counter lives (used only
/// for eviction ordering, never exposed).
const SEQ_KEY: &[u8] = b"seq";

impl<B: KvBackend> VerdictCache<B> {
    /// Opens (creating if necessary) this cache's keyspaces on `backend`, from [`CacheConfig`].
    ///
    /// # Errors
    /// Returns [`MediaError::Metadata`] if the backend could not open a keyspace.
    pub fn open(backend: B, config: &CacheConfig) -> Result<Self, MediaError> {
        let entries = TypedKeyspace::new(
            backend
                .keyspace("hs_media.scan_cache.entries")
                .map_err(|e| MediaError::Metadata(e.to_string()))?,
        );
        let order = TypedKeyspace::new(
            backend
                .keyspace("hs_media.scan_cache.order")
                .map_err(|e| MediaError::Metadata(e.to_string()))?,
        );
        let meta = backend
            .keyspace("hs_media.scan_cache.meta")
            .map_err(|e| MediaError::Metadata(e.to_string()))?;
        Ok(Self {
            backend,
            entries,
            order,
            meta,
            capacity: config.capacity.max(1),
            ttl_ms: config.ttl.as_millis(),
            unversioned_ttl_ms: config.unversioned_ttl.as_millis(),
        })
    }

    fn version_key(engine_version: Option<&str>) -> String {
        match engine_version {
            Some(v) => v.to_string(),
            None => UNVERSIONED.to_string(),
        }
    }

    /// Looks up a cached verdict for `(sha256_hex, provider_id, engine_version)`, if present and
    /// not expired as of `now_ms`.
    ///
    /// # Errors
    /// Returns [`MediaError::Metadata`] on a backend failure.
    pub fn get(
        &self,
        now_ms: u64,
        sha256_hex: &str,
        provider_id: &str,
        engine_version: Option<&str>,
    ) -> Result<Option<Verdict>, MediaError> {
        let key = (
            sha256_hex.to_string(),
            provider_id.to_string(),
            Self::version_key(engine_version),
        );
        let snapshot = self.backend.snapshot();
        let Some(bytes) = self
            .entries
            .get(&snapshot, &key)
            .map_err(|e: hs_tables::TableError| MediaError::Metadata(e.to_string()))?
        else {
            return Ok(None);
        };
        let entry: Entry = serde_json::from_slice(&bytes)
            .map_err(|e| MediaError::Metadata(format!("decoding cache entry: {e}")))?;
        if entry.cached_at_ms.saturating_add(entry.ttl_ms) <= now_ms {
            return Ok(None);
        }
        Ok(Some(entry.verdict.into_verdict()))
    }

    /// Stores a verdict for `(sha256_hex, provider_id, engine_version)`, evicting the
    /// oldest-inserted entries if this insert pushes the cache over capacity.
    ///
    /// Silently does nothing for [`Verdict::Pending`] (never cached — see the module doc).
    ///
    /// # Errors
    /// Returns [`MediaError::Metadata`] on a backend failure.
    pub fn put(
        &self,
        now_ms: u64,
        sha256_hex: &str,
        provider_id: &str,
        engine_version: Option<&str>,
        verdict: &Verdict,
    ) -> Result<(), MediaError> {
        let Some(cached) = CachedVerdict::from_verdict(verdict) else {
            return Ok(());
        };
        let ttl_ms = if engine_version.is_some() {
            self.ttl_ms
        } else {
            self.unversioned_ttl_ms
        };
        let key: EntryKey = (
            sha256_hex.to_string(),
            provider_id.to_string(),
            Self::version_key(engine_version),
        );
        let capacity = self.capacity;

        transact(&self.backend, TransactConfig::default(), |txn| {
            #[allow(
                clippy::cast_sign_loss,
                reason = "atomic_add's counter is only ever incremented by 1 by this module, so \
                          it can never go negative"
            )]
            let seq = hs_kv::KvWrite::atomic_add(txn, &self.meta, SEQ_KEY, 1)? as u64;

            let entry = Entry {
                verdict: cached.clone(),
                cached_at_ms: now_ms,
                ttl_ms,
                seq,
            };
            let value = serde_json::to_vec(&entry)
                .map_err(|e| hs_kv::KvError::backend(EncodeError(e.to_string())))?;
            self.entries.put(txn, &key, &value).map_err(to_kv_err)?;

            let order_key: OrderKey = (now_ms, seq);
            let order_value = serde_json::to_vec(&key)
                .map_err(|e| hs_kv::KvError::backend(EncodeError(e.to_string())))?;
            self.order
                .put(txn, &order_key, &order_value)
                .map_err(to_kv_err)?;

            // Evict oldest-first until we are back at or under capacity. A full scan is avoided:
            // the order keyspace's key encoding sorts by (cached_at_ms, seq), so the oldest
            // `overflow` entries are exactly the first `overflow` items of a forward range scan,
            // read with a `limit` rather than by scanning the whole keyspace.
            let count = self.order_count(txn)?;
            if count > capacity {
                let overflow = count - capacity;
                let spec = RangeSpec::full().limit(overflow);
                let victims: Vec<(OrderKey, bytes::Bytes)> = self
                    .order
                    .range(txn, spec)
                    .collect::<Result<Vec<_>, hs_tables::TableError>>()
                    .map_err(to_kv_err)?;
                for (order_key, value) in victims {
                    let entry_key: EntryKey = serde_json::from_slice(&value)
                        .map_err(|e| hs_kv::KvError::backend(EncodeError(e.to_string())))?;
                    self.entries.delete(txn, &entry_key).map_err(to_kv_err)?;
                    self.order.delete(txn, &order_key).map_err(to_kv_err)?;
                }
            }
            Ok(())
        })
        .map_err(|e| MediaError::Metadata(e.to_string()))
    }

    /// Counts entries by scanning the order keyspace with no limit. Only ever called from inside
    /// [`VerdictCache::put`]'s transaction, right after inserting one row, so in practice this
    /// scans at most `capacity + 1` entries — bounded by the same capacity this method helps
    /// enforce, not proportional to total cache traffic.
    fn order_count<R: hs_kv::KvRead<Keyspace = B::Keyspace>>(
        &self,
        txn: &R,
    ) -> Result<usize, hs_kv::KvError> {
        let mut n = 0usize;
        for item in self.order.range(txn, RangeSpec::full()) {
            item.map_err(to_kv_err)?;
            n += 1;
        }
        Ok(n)
    }
}

fn to_kv_err(e: hs_tables::TableError) -> hs_kv::KvError {
    match e {
        hs_tables::TableError::Kv(kv) => kv,
        other => hs_kv::KvError::backend(EncodeError(other.to_string())),
    }
}

#[derive(Debug, thiserror::Error)]
#[error("{0}")]
struct EncodeError(String);

#[cfg(test)]
mod tests {
    use super::*;
    use hs_config::Duration;
    use hs_kv::memory::MemoryBackend;

    fn cache_with_capacity(capacity: usize) -> VerdictCache<MemoryBackend> {
        VerdictCache::open(
            MemoryBackend::new(),
            &CacheConfig {
                ttl: Duration::from_secs(3600),
                unversioned_ttl: Duration::from_secs(60),
                capacity,
            },
        )
        .unwrap()
    }

    #[test]
    fn round_trips_a_clean_verdict() {
        let cache = cache_with_capacity(10);
        cache
            .put(1000, "abc", "clamav", Some("v1"), &Verdict::Clean)
            .unwrap();
        let got = cache.get(1500, "abc", "clamav", Some("v1")).unwrap();
        assert_eq!(got, Some(Verdict::Clean));
    }

    #[test]
    fn miss_for_unknown_key() {
        let cache = cache_with_capacity(10);
        assert_eq!(cache.get(0, "nope", "clamav", Some("v1")).unwrap(), None);
    }

    #[test]
    fn pending_is_never_cached() {
        let cache = cache_with_capacity(10);
        let verdict = Verdict::Pending {
            ticket: crate::scanning::types::ScanTicket("t1".into()),
            retry_after: std::time::Duration::from_secs(1),
        };
        cache.put(0, "abc", "http", Some("v1"), &verdict).unwrap();
        assert_eq!(cache.get(0, "abc", "http", Some("v1")).unwrap(), None);
    }

    #[test]
    fn expired_entry_is_a_miss() {
        let cache = cache_with_capacity(10);
        cache
            .put(1000, "abc", "clamav", Some("v1"), &Verdict::Clean)
            .unwrap();
        // ttl is 3600s = 3_600_000ms; ask at exactly the expiry instant, and past it.
        assert_eq!(
            cache.get(1000 + 3_600_000, "abc", "clamav", Some("v1")).unwrap(),
            None
        );
        assert!(
            cache
                .get(1000 + 3_600_000 - 1, "abc", "clamav", Some("v1"))
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn unversioned_verdicts_use_the_shorter_ttl() {
        let cache = cache_with_capacity(10);
        cache
            .put(1000, "abc", "http", None, &Verdict::Clean)
            .unwrap();
        // unversioned_ttl is 60s = 60_000ms.
        assert!(cache.get(1000 + 59_999, "abc", "http", None).unwrap().is_some());
        assert_eq!(cache.get(1000 + 60_000, "abc", "http", None).unwrap(), None);
    }

    #[test]
    fn signature_version_change_invalidates_the_cache() {
        let cache = cache_with_capacity(10);
        cache
            .put(
                0,
                "abc",
                "clamav",
                Some("sigs-2026-09-01"),
                &Verdict::Infected {
                    signature: "Eicar-Test-Signature".into(),
                    details: None,
                },
            )
            .unwrap();
        // Same content, same provider, but the engine (signature) version has moved on: a fresh
        // scan must not see the stale verdict.
        assert_eq!(
            cache
                .get(1, "abc", "clamav", Some("sigs-2026-09-02"))
                .unwrap(),
            None
        );
        // The old version is, naturally, still cached under its own key.
        assert!(
            cache
                .get(1, "abc", "clamav", Some("sigs-2026-09-01"))
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn different_providers_do_not_share_a_cache_entry() {
        let cache = cache_with_capacity(10);
        cache
            .put(0, "abc", "clamav", Some("v1"), &Verdict::Clean)
            .unwrap();
        assert_eq!(cache.get(0, "abc", "icap", Some("v1")).unwrap(), None);
    }

    #[test]
    fn capacity_evicts_oldest_first() {
        let cache = cache_with_capacity(2);
        cache
            .put(100, "a", "clamav", Some("v1"), &Verdict::Clean)
            .unwrap();
        cache
            .put(200, "b", "clamav", Some("v1"), &Verdict::Clean)
            .unwrap();
        // Third insert pushes the cache to 3 entries, over capacity 2: "a" (oldest) is evicted.
        cache
            .put(300, "c", "clamav", Some("v1"), &Verdict::Clean)
            .unwrap();

        assert_eq!(cache.get(300, "a", "clamav", Some("v1")).unwrap(), None);
        assert!(cache.get(300, "b", "clamav", Some("v1")).unwrap().is_some());
        assert!(cache.get(300, "c", "clamav", Some("v1")).unwrap().is_some());
    }

    #[test]
    fn infected_and_unscannable_round_trip() {
        let cache = cache_with_capacity(10);
        cache
            .put(
                0,
                "bad",
                "clamav",
                Some("v1"),
                &Verdict::Infected {
                    signature: "Eicar-Test-Signature".into(),
                    details: Some("test file".into()),
                },
            )
            .unwrap();
        let got = cache.get(0, "bad", "clamav", Some("v1")).unwrap().unwrap();
        assert_eq!(
            got,
            Verdict::Infected {
                signature: "Eicar-Test-Signature".into(),
                details: Some("test file".into()),
            }
        );

        cache
            .put(
                0,
                "enc",
                "clamav",
                Some("v1"),
                &Verdict::Unscannable {
                    reason: UnscannableReason::Encrypted,
                },
            )
            .unwrap();
        let got = cache.get(0, "enc", "clamav", Some("v1")).unwrap().unwrap();
        assert_eq!(
            got,
            Verdict::Unscannable {
                reason: UnscannableReason::Encrypted
            }
        );
    }
}
