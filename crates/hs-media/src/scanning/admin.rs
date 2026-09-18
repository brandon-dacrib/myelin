//! The admin surface RFC section 8 asks for: "list recent verdicts, rescan a media item, rescan
//! everything matching a filter after a signature update, and show current provider health."
//!
//! Per this track's instructions, the admin OpenAPI document
//! (`crates/hs-admin/openapi/openapi.yaml`) belongs to track 15 and is not edited here. This
//! module is this crate's side of that seam: a trait track 15's HTTP handlers can call, plus the
//! plain data types those handlers would serialize. The wire-shape proposal for track 15's
//! endpoints is `docs/rfcs/0011-admin-scanning-endpoints.md`.
//!
//! **Not implemented in this session**: a concrete `impl ScanAdmin`. `crate::repository::
//! MediaRepository<B>` now holds a `crate::scanning::engine::ScanEngine<B>` (see
//! `MediaRepository::with_scanning`, wired into the upload path this session), which gets a
//! `ScanAdmin` implementation most of the way there for `rescan`/`rescan_many` (the repository
//! already has object-store access to re-read a media item's bytes), but `recent_verdicts` still
//! needs a concrete `Arc<InMemoryAuditSink>` handle threaded alongside the engine, since
//! `ScanEngine` only holds `Arc<dyn AuditSink>` (no `recent()` method on the trait object) — see
//! RFC 0011 section 7 for the exact list of what is still missing and why. See
//! `docs/status/09-media.md` for the exact next steps.

use async_trait::async_trait;

use crate::error::MediaError;
use crate::scanning::audit::AuditEntry;
use crate::scanning::types::Verdict;

/// A provider's current reachability, as best this crate can determine it. `ContentScanner` (RFC
/// section 2) has no dedicated health-check method — adding one would change the trait shape the
/// RFC specifies exactly — so this is necessarily approximate: `reachable: true` means the most
/// recent `engine_version()`/`scan()` call succeeded, not that a fresh probe was just made.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderHealth {
    /// The provider id.
    pub provider_id: String,
    /// Best-effort reachability.
    pub reachable: bool,
    /// The last known engine/signature version, if any.
    pub engine_version: Option<String>,
    /// When this was last checked, milliseconds since the Unix epoch.
    pub checked_at_ms: u64,
}

/// The admin operations RFC section 8 lists, behind a trait so track 15's HTTP layer can depend
/// on this crate without this crate depending on track 15's admin framework.
#[async_trait]
pub trait ScanAdmin: Send + Sync {
    /// The most recent audit entries (infected, error, replacement-applied, appservice-bypass),
    /// newest first.
    async fn recent_verdicts(&self, limit: usize) -> Vec<AuditEntry>;

    /// Re-scans one media item by its stored bytes, returning the fresh verdict. Does not itself
    /// change the item's servability/quarantine state — that is the caller's decision, informed
    /// by the returned verdict (mirroring how a normal scan's `EngineDecision` is applied at an
    /// upload scan point).
    ///
    /// # Errors
    /// [`MediaError::NotFound`] if no such item exists; otherwise as scanning can fail.
    async fn rescan(&self, server_name: &str, media_id: &str) -> Result<Verdict, MediaError>;

    /// Re-scans every item in `items` (typically "everything matching a filter" that the admin
    /// HTTP layer resolved via `crate::metadata::MetadataStore` before calling this), returning
    /// one result per input item in order.
    async fn rescan_many(
        &self,
        items: &[(String, String)],
    ) -> Vec<(String, String, Result<Verdict, MediaError>)> {
        let mut out = Vec::with_capacity(items.len());
        for (server_name, media_id) in items {
            let result = self.rescan(server_name, media_id).await;
            out.push((server_name.clone(), media_id.clone(), result));
        }
        out
    }

    /// The configured provider's current health.
    async fn provider_health(&self) -> ProviderHealth;
}
