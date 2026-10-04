//! The serializable-transaction retry helper. See the crate-level docs, "Retry rules".

use std::time::Duration;

use crate::error::KvError;
use crate::traits::KvBackend;

/// Tuning for [`transact`]. The defaults are conservative for interactive request paths; a
/// background job doing bulk work may want a larger [`TransactConfig::max_attempts`].
#[derive(Debug, Clone, Copy)]
pub struct TransactConfig {
    /// Maximum number of attempts (the first try plus retries). Must be at least 1.
    pub max_attempts: u32,
    /// Backoff before the second attempt; doubles each subsequent retry (capped at
    /// `max_backoff`) and is jittered by up to 50% to avoid synchronized retry storms between
    /// transactions racing on the same keys.
    pub base_backoff: Duration,
    /// Backoff never grows past this.
    pub max_backoff: Duration,
}

impl Default for TransactConfig {
    fn default() -> Self {
        Self {
            max_attempts: 10,
            base_backoff: Duration::from_millis(2),
            max_backoff: Duration::from_millis(100),
        }
    }
}

/// Runs `f` in a fresh, retried, serializable transaction on `backend`.
///
/// `f` is called again from scratch on every attempt — including the first — and must not assume
/// anything about a previous, conflicted attempt: rebuild every read and write inside `f` itself.
/// If `f` returns `Err`, `transact` stops immediately and returns that error without retrying (see
/// the crate-level contract: only a commit [`crate::Conflict`] is retried). If every attempt's
/// commit conflicts, `transact` returns [`KvError::RetriesExhausted`] after `config.max_attempts`
/// tries.
///
/// # Errors
/// Propagates whatever `f` returns, any backend error from beginning or committing the
/// transaction, or [`KvError::RetriesExhausted`].
pub fn transact<B, T>(
    backend: &B,
    config: TransactConfig,
    mut f: impl FnMut(&mut B::Txn) -> Result<T, KvError>,
) -> Result<T, KvError>
where
    B: KvBackend,
{
    assert!(
        config.max_attempts >= 1,
        "TransactConfig::max_attempts must be at least 1"
    );

    let mut attempt = 0u32;
    let mut jitter = Jitter::new();
    loop {
        attempt += 1;
        let mut txn = backend.begin()?;
        let conflicted = match f(&mut txn) {
            Ok(value) => match backend.commit(txn)? {
                Ok(()) => return Ok(value),
                Err(crate::error::Conflict) => true,
            },
            // Some backends (PostgreSQL's SSI) can detect a conflict on any statement, not only
            // at commit; see `KvError::MidTransactionConflict`. Treat it identically to a
            // commit-time `Conflict`: the transaction (already discarded by the backend) is
            // retried from scratch. Every other error stops immediately, unretried.
            Err(KvError::MidTransactionConflict) => true,
            Err(e) => return Err(e),
        };
        if conflicted {
            if attempt >= config.max_attempts {
                return Err(KvError::RetriesExhausted { attempts: attempt });
            }
            std::thread::sleep(backoff(&config, attempt, &mut jitter));
        }
    }
}

/// The wait before attempt `attempt + 1`: `base_backoff * 2^(attempt - 1)`, capped at
/// `max_backoff`, scaled by a factor in `[0.5, 1.0)` drawn from `jitter`.
///
/// The factor must differ between the transactions that conflicted with each other, or they
/// retry in lockstep and meet again on every attempt. It used to be a function of the attempt
/// number alone, which is the same for every party to a conflict: on PostgreSQL, whose SSI
/// cancels writers of *different* keys of a small table (it tracks reads of a one-page table at
/// page or relation granularity), two requests authenticating at once were each cancelled ten
/// times within a few milliseconds and the request answered 500 (a merge gate, 2026-10-04;
/// `tests/postgres_contention.rs`).
fn backoff(config: &TransactConfig, attempt: u32, jitter: &mut Jitter) -> Duration {
    let scale = 1u32 << attempt.saturating_sub(1).min(20);
    let unjittered = config
        .base_backoff
        .saturating_mul(scale)
        .min(config.max_backoff);
    let jitter_permille = 500 + jitter.next() % 500;
    unjittered.saturating_mul(u32::try_from(jitter_permille).unwrap_or(1000)) / 1000
}

/// A per-[`transact`]-call pseudo-random sequence for [`backoff`]: a SplitMix64 stream seeded
/// from the standard library's randomly keyed hasher, the calling thread and the time, so two
/// transactions retrying at once draw different waits. A wait duration, not cryptography; no
/// dependency needed.
struct Jitter(u64);

impl Jitter {
    fn new() -> Self {
        use std::hash::{BuildHasher, Hash, Hasher};
        // `RandomState::new` is keyed randomly per process and differently for each call.
        let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
        std::thread::current().id().hash(&mut hasher);
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
            .hash(&mut hasher);
        Self(hasher.finish())
    }

    fn next(&mut self) -> u64 {
        // SplitMix64 (Steele, Lea and Flood, 2014; public domain constants).
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_grows_and_caps() {
        let config = TransactConfig::default();
        let mut jitter = Jitter::new();
        let first = backoff(&config, 1, &mut jitter);
        let capped = backoff(&config, 30, &mut jitter);
        assert!(first < config.base_backoff);
        assert!(first >= config.base_backoff / 2);
        assert!(capped < config.max_backoff);
        assert!(capped >= config.max_backoff / 2);
        for attempt in 1..12 {
            let wait = backoff(&config, attempt, &mut jitter);
            let unjittered = config
                .base_backoff
                .saturating_mul(1 << (attempt - 1))
                .min(config.max_backoff);
            assert!(
                wait < unjittered && wait >= unjittered / 2,
                "{attempt}: {wait:?}"
            );
        }
    }

    /// Two transactions that conflicted with each other must not wait the same: the schedule
    /// used to depend on the attempt number alone, so both retried in lockstep.
    #[test]
    fn two_retrying_transactions_draw_different_waits() {
        let config = TransactConfig {
            max_attempts: 10,
            base_backoff: Duration::from_millis(100),
            max_backoff: Duration::from_secs(10),
        };
        let schedule = move || {
            let mut jitter = Jitter::new();
            (1..10)
                .map(|attempt| backoff(&config, attempt, &mut jitter))
                .collect::<Vec<_>>()
        };
        let a = schedule();
        let b = std::thread::spawn(schedule).join().expect("thread");
        let same = a.iter().zip(&b).filter(|(x, y)| x == y).count();
        assert!(
            same < 3,
            "the two schedules matched on {same} of 9 attempts: {a:?} {b:?}"
        );
    }
}
