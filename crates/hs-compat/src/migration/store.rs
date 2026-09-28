//! Where a migration's record and log are kept: [`MigrationStore`]. `hs-cli` keeps them in the
//! server's own database (so they survive a restart and are the same on every replica);
//! [`InMemoryMigrationStore`] is for tests.

use std::sync::Mutex;

use async_trait::async_trait;

use super::MigrationError;
use super::model::{LogEntry, MigrationRecord};

/// The most log entries kept; older ones are dropped first.
pub const LOG_LIMIT: usize = 10_000;

/// Keeps the one migration record and its log.
#[async_trait]
pub trait MigrationStore: Send + Sync + 'static {
    /// The record, or the default (idle) record if none was ever saved.
    async fn load(&self) -> Result<MigrationRecord, MigrationError>;
    /// Replaces the record.
    async fn save(&self, record: &MigrationRecord) -> Result<(), MigrationError>;
    /// Appends to the log, dropping the oldest entries beyond [`LOG_LIMIT`].
    async fn append_log(&self, entries: &[LogEntry]) -> Result<(), MigrationError>;
    /// The whole log, oldest first.
    async fn log(&self) -> Result<Vec<LogEntry>, MigrationError>;
}

/// A [`MigrationStore`] in memory: not durable.
#[derive(Default)]
pub struct InMemoryMigrationStore {
    record: Mutex<Option<MigrationRecord>>,
    log: Mutex<Vec<LogEntry>>,
}

impl InMemoryMigrationStore {
    /// An empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl MigrationStore for InMemoryMigrationStore {
    async fn load(&self) -> Result<MigrationRecord, MigrationError> {
        Ok(self
            .record
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
            .unwrap_or_default())
    }

    async fn save(&self, record: &MigrationRecord) -> Result<(), MigrationError> {
        *self
            .record
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(record.clone());
        Ok(())
    }

    async fn append_log(&self, entries: &[LogEntry]) -> Result<(), MigrationError> {
        let mut log = self
            .log
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        log.extend_from_slice(entries);
        let excess = log.len().saturating_sub(LOG_LIMIT);
        log.drain(..excess);
        Ok(())
    }

    async fn log(&self) -> Result<Vec<LogEntry>, MigrationError> {
        Ok(self
            .log
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone())
    }
}
