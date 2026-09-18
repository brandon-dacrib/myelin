//! Shared size-limit checks, used identically by every backend so the contract in the crate docs
//! holds regardless of which one is running.

use crate::error::KvError;
use crate::{MAX_KEY_BYTES, MAX_TXN_BYTES, MAX_TXN_MUTATIONS, MAX_VALUE_BYTES};

/// Checks a key against [`MAX_KEY_BYTES`].
pub(crate) fn check_key(key: &[u8]) -> Result<(), KvError> {
    if key.is_empty() {
        return Err(KvError::backend(EmptyKeyError));
    }
    if key.len() > MAX_KEY_BYTES {
        return Err(KvError::KeyTooLarge {
            len: key.len(),
            limit: MAX_KEY_BYTES,
        });
    }
    Ok(())
}

/// Checks a value against [`MAX_VALUE_BYTES`].
pub(crate) fn check_value(value: &[u8]) -> Result<(), KvError> {
    if value.len() > MAX_VALUE_BYTES {
        return Err(KvError::ValueTooLarge {
            len: value.len(),
            limit: MAX_VALUE_BYTES,
        });
    }
    Ok(())
}

/// Tracks a transaction's running mutation count and byte budget, erroring once either limit is
/// exceeded. Embedded by each backend's transaction type.
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct TxnBudget {
    mutations: usize,
    bytes: usize,
}

impl TxnBudget {
    pub(crate) fn record(&mut self, mutation_bytes: usize) -> Result<(), KvError> {
        self.mutations += 1;
        self.bytes += mutation_bytes;
        if self.mutations > MAX_TXN_MUTATIONS {
            return Err(KvError::TransactionTooLarge {
                reason: format!(
                    "{} mutations exceeds the {MAX_TXN_MUTATIONS}-mutation limit; split the work across multiple transactions",
                    self.mutations
                ),
            });
        }
        if self.bytes > MAX_TXN_BYTES {
            return Err(KvError::TransactionTooLarge {
                reason: format!(
                    "{} mutation bytes exceeds the {MAX_TXN_BYTES}-byte limit; split the work across multiple transactions",
                    self.bytes
                ),
            });
        }
        Ok(())
    }
}

#[derive(Debug, thiserror::Error)]
#[error("keys must not be empty")]
pub(crate) struct EmptyKeyError;
