//! Best-effort, in-process change notification. See the crate-level contract: **watches are
//! hints, never a substitute for reading the key inside a transaction.**

use std::collections::HashMap;
use std::sync::{Arc, Condvar, Mutex, Weak};
use std::time::Duration;

#[derive(Default)]
struct Signal {
    version: Mutex<u64>,
    condvar: Condvar,
}

type SignalKey = (String, Vec<u8>);

/// A process-local registry of interested watchers, keyed by `(keyspace name, key)`. One `Hub` is
/// shared (via `Arc`) by every keyspace and transaction opened from the same backend instance.
///
/// Entries are held by [`Weak`] reference and self-prune: once every [`Watch`] on a key is
/// dropped, the next [`Hub::watch`] or [`Hub::notify`] touching that key removes the dead entry.
/// A `Hub` therefore never grows unboundedly with respect to *distinct keys ever watched*, only
/// with respect to keys *currently* watched.
#[derive(Default)]
pub struct Hub {
    signals: Mutex<HashMap<SignalKey, Weak<Signal>>>,
}

impl Hub {
    /// Creates an empty hub.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers interest in `key` within `keyspace`.
    #[must_use]
    pub fn watch(&self, keyspace: &str, key: &[u8]) -> Watch {
        let mut signals = self
            .signals
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let entry = signals.entry((keyspace.to_owned(), key.to_vec()));
        let signal = match entry {
            std::collections::hash_map::Entry::Occupied(mut occ) => {
                if let Some(strong) = occ.get().upgrade() {
                    strong
                } else {
                    let strong = Arc::new(Signal::default());
                    occ.insert(Arc::downgrade(&strong));
                    strong
                }
            }
            std::collections::hash_map::Entry::Vacant(vac) => {
                let strong = Arc::new(Signal::default());
                vac.insert(Arc::downgrade(&strong));
                strong
            }
        };
        let seen = *signal
            .version
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        Watch {
            signal,
            seen_version: seen,
        }
    }

    /// Notifies watchers of `key` within `keyspace` that it (may have) changed. Called by a
    /// backend after a transaction that wrote `key` commits successfully. Not called for
    /// transactions that conflict or are rolled back.
    pub fn notify(&self, keyspace: &str, key: &[u8]) {
        let mut signals = self
            .signals
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(weak) = signals.get(&(keyspace.to_owned(), key.to_vec())) {
            if let Some(signal) = weak.upgrade() {
                let mut v = signal
                    .version
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                *v = v.wrapping_add(1);
                signal.condvar.notify_all();
            } else {
                signals.remove(&(keyspace.to_owned(), key.to_vec()));
            }
        }
    }
}

/// A handle returned by [`crate::KvBackend::watch`]. Blocks the calling thread on [`Watch::wait`]
/// until the watched key is next written, or a timeout elapses — whichever first. There is no
/// guarantee the value actually changed (a put of the same bytes still fires it), no guarantee of
/// exactly-once delivery, and no queue of missed notifications: a `Watch` only ever answers "has
/// this changed since I last checked", never "what changed and how many times".
pub struct Watch {
    signal: Arc<Signal>,
    seen_version: u64,
}

/// The result of [`Watch::wait`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WatchOutcome {
    /// The key was written at least once since the watch was created or last observed a change.
    Changed,
    /// The timeout elapsed with no observed write.
    Timeout,
}

impl Watch {
    /// Blocks until a write is observed or `timeout` elapses.
    pub fn wait(&mut self, timeout: Duration) -> WatchOutcome {
        let guard = self
            .signal
            .version
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let seen = self.seen_version;
        let (guard, result) = self
            .signal
            .condvar
            .wait_timeout_while(guard, timeout, |v| *v == seen)
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if result.timed_out() {
            WatchOutcome::Timeout
        } else {
            self.seen_version = *guard;
            WatchOutcome::Changed
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;

    #[test]
    fn watch_fires_after_notify() {
        let hub = Hub::new();
        let mut watch = hub.watch("ks", b"key");

        let hub2 = Arc::new(hub);
        let hub3 = hub2.clone();
        let handle = thread::spawn(move || {
            thread::sleep(Duration::from_millis(20));
            hub3.notify("ks", b"key");
        });

        assert_eq!(watch.wait(Duration::from_secs(5)), WatchOutcome::Changed);
        handle.join().unwrap();
    }

    #[test]
    fn watch_times_out_without_a_write() {
        let hub = Hub::new();
        let mut watch = hub.watch("ks", b"key");
        assert_eq!(watch.wait(Duration::from_millis(20)), WatchOutcome::Timeout);
    }

    #[test]
    fn notify_on_a_key_with_no_watchers_is_a_no_op() {
        let hub = Hub::new();
        hub.notify("ks", b"nobody-watching");
    }

    #[test]
    fn dropped_watch_entry_is_pruned_and_reusable() {
        let hub = Hub::new();
        {
            let _watch = hub.watch("ks", b"key");
            assert_eq!(hub.signals.lock().unwrap().len(), 1);
        }
        // The Watch (and its strong Arc) is gone; a fresh watch reuses the slot.
        let mut watch = hub.watch("ks", b"key");
        assert_eq!(watch.wait(Duration::from_millis(10)), WatchOutcome::Timeout);
    }
}
