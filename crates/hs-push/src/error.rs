//! Shared error types for `hs-push`'s stores. Mirrors `hs_auth::store::StoreError`'s shape
//! (`crates/hs-auth/src/store/mod.rs`) rather than inventing a new one, since callers already
//! know how to handle that shape from wiring `hs-auth` in.

/// An error from one of this crate's `hs-kv`/`hs-tables`-backed stores.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    /// The requested row does not exist.
    #[error("not found: {0}")]
    NotFound(String),
    /// The backend (`hs-kv`, `hs-tables`, or this store's own (de)serialization) failed.
    #[error("push store backend error: {0}")]
    Backend(String),
}

impl From<hs_kv::KvError> for StoreError {
    fn from(e: hs_kv::KvError) -> Self {
        Self::Backend(e.to_string())
    }
}

impl From<hs_tables::keyspace::TableError> for StoreError {
    fn from(e: hs_tables::keyspace::TableError) -> Self {
        Self::Backend(e.to_string())
    }
}
