//! An append-only, ordered log of recorded JSON records, backed by
//! [`hs_kv::memory::MemoryBackend`] rather than an ad hoc `Vec<Mutex<...>>`.
//!
//! Every fake sink in this crate (appservice transactions, federation requests, push-gateway
//! notifications, SMTP messages) needs the same shape: record things in arrival order, read them
//! back for assertions, from multiple concurrent callers. [`RecordLog`] is that shape, built on
//! the same ordered transactional KV abstraction the rest of the workspace stores data through
//! (`docs/workstreams/README.md`'s seam table), so this crate does not maintain a second,
//! competing idea of "a fake store."

use hs_kv::memory::MemoryBackend;
use hs_kv::{KvBackend, KvRead, KvWrite, RangeSpec, transact};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;

const COUNTER_KEYSPACE: &str = "testkit_record_log_counter";
const DATA_KEYSPACE: &str = "testkit_record_log_data";

/// An append-only log of JSON-serializable records, readable back in insertion order.
///
/// Each [`RecordLog`] gets its own private [`MemoryBackend`] (backends are cheap: no I/O, just a
/// couple of `BTreeMap`s), so independent fakes in the same test never share state by accident.
pub struct RecordLog {
    backend: MemoryBackend,
    counter_ks: <MemoryBackend as KvBackend>::Keyspace,
    data_ks: <MemoryBackend as KvBackend>::Keyspace,
}

impl Default for RecordLog {
    fn default() -> Self {
        Self::new()
    }
}

impl RecordLog {
    /// A fresh, empty log.
    #[must_use]
    pub fn new() -> Self {
        let backend = MemoryBackend::new();
        let counter_ks = backend
            .keyspace(COUNTER_KEYSPACE)
            .expect("static keyspace name is always valid");
        let data_ks = backend
            .keyspace(DATA_KEYSPACE)
            .expect("static keyspace name is always valid");
        Self {
            backend,
            counter_ks,
            data_ks,
        }
    }

    /// Appends `record`, returning its 1-based sequence number.
    ///
    /// # Panics
    /// Panics if `record` cannot be serialized to JSON, or if the in-memory backend reports a
    /// transaction conflict on every retry attempt (practically unreachable for a private,
    /// process-local backend with no other writers).
    pub fn record(&self, record: &impl Serialize) -> u64 {
        let value = serde_json::to_vec(record).expect("record must serialize to JSON");
        transact(&self.backend, Default::default(), |txn| {
            let seq = txn.atomic_add(&self.counter_ks, b"seq", 1)?;
            #[allow(clippy::cast_sign_loss)]
            let key = (seq as u64).to_be_bytes();
            txn.put(&self.data_ks, &key, &value)?;
            Ok(seq)
        })
        .expect("record log is process-local; retries should never be exhausted") as u64
    }

    /// Every recorded value, oldest first, parsed as untyped JSON.
    ///
    /// # Panics
    /// Panics if a stored record is not valid JSON (unreachable: only [`RecordLog::record`]
    /// writes to this log, and it always writes valid JSON).
    #[must_use]
    pub fn all(&self) -> Vec<Value> {
        self.all_as()
            .expect("records recorded by this type are always valid JSON")
    }

    /// Every recorded value, oldest first, deserialized as `T`.
    ///
    /// # Errors
    /// Returns the first `serde_json` decode error encountered.
    pub fn all_as<T: DeserializeOwned>(&self) -> serde_json::Result<Vec<T>> {
        let snapshot = self.backend.snapshot();
        snapshot
            .range(&self.data_ks, RangeSpec::full())
            .map(|item| {
                let (_, value) = item.expect("in-memory backend range scans do not fail");
                serde_json::from_slice(&value)
            })
            .collect()
    }

    /// How many records have been appended so far.
    #[must_use]
    pub fn len(&self) -> usize {
        self.all().len()
    }

    /// Whether the log is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn records_come_back_in_insertion_order() {
        let log = RecordLog::new();
        log.record(&json!({"n": 1}));
        log.record(&json!({"n": 2}));
        log.record(&json!({"n": 3}));

        let all = log.all();
        assert_eq!(all.len(), 3);
        assert_eq!(all[0]["n"], 1);
        assert_eq!(all[1]["n"], 2);
        assert_eq!(all[2]["n"], 3);
    }

    #[test]
    fn empty_log_reports_empty() {
        let log = RecordLog::new();
        assert!(log.is_empty());
        assert_eq!(log.len(), 0);
    }

    #[test]
    fn concurrent_records_from_multiple_threads_are_all_kept() {
        let log = std::sync::Arc::new(RecordLog::new());
        let handles: Vec<_> = (0..8)
            .map(|i| {
                let log = log.clone();
                std::thread::spawn(move || {
                    log.record(&json!({"thread": i}));
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
        assert_eq!(log.len(), 8);
    }
}
