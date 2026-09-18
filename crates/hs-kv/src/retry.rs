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
    loop {
        attempt += 1;
        let mut txn = backend.begin()?;
        let value = f(&mut txn)?;
        match backend.commit(txn)? {
            Ok(()) => return Ok(value),
            Err(crate::error::Conflict) => {
                if attempt >= config.max_attempts {
                    return Err(KvError::RetriesExhausted { attempts: attempt });
                }
                std::thread::sleep(backoff(&config, attempt));
            }
        }
    }
}

fn backoff(config: &TransactConfig, attempt: u32) -> Duration {
    let scale = 1u32 << attempt.saturating_sub(1).min(20);
    let unjittered = config
        .base_backoff
        .saturating_mul(scale)
        .min(config.max_backoff);
    // Jitter in [0.5, 1.0) of the computed backoff, using the attempt number as a cheap,
    // dependency-free pseudo-random source (this is a wait duration, not cryptography).
    let jitter_permille = 500 + (u64::from(attempt).wrapping_mul(2_654_435_761) % 500);
    unjittered.saturating_mul(u32::try_from(jitter_permille).unwrap_or(1000)) / 1000
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_grows_and_caps() {
        let config = TransactConfig::default();
        let first = backoff(&config, 1);
        let later = backoff(&config, 3);
        let capped = backoff(&config, 30);
        assert!(first <= config.base_backoff);
        assert!(later >= first);
        assert!(capped <= config.max_backoff);
    }
}
