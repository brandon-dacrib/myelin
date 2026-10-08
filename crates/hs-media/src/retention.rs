//! `media.remote_media_retention`: cached copies of other servers' media that nobody has asked
//! for in that long are deleted, once an hour, as Synapse's `media_retention.remote_media_lifetime`
//! does.
//!
//! What a pass selects is what the admin API's `media.purge_remote_cache` selects for the same
//! `before`: completed copies of *another* server's media whose last use
//! ([`MediaRecord::last_used_ms`]: last served, or fetched if never served) is older than the
//! retention, except protected copies (`safe_from_quarantine`) and quarantined ones (deleting a
//! quarantined copy would let the next request fetch it again, undoing the quarantine). A deleted
//! copy is fetched afresh from its server the next time someone asks for it.
//!
//! The retention is read from the repository's live configuration on every pass, so a change
//! applies at the next pass and unsetting it stops the deletions. In a cluster every replica
//! runs the sweeper over the one shared store; a copy another replica deleted first is skipped
//! (`NotFound`), so the passes cost a list each and delete each copy once.
//!
//! Observable: every pass that deletes anything logs one `info` line (target
//! `hs_media::retention`) with the counts, a copy that cannot be deleted logs a `warn`, and
//! `hs_media_remote_retention_deleted_total` / `_bytes_total` / `_failed_total` count them.

use std::sync::Arc;
use std::time::Duration;

use hs_kv::KvBackend;
use prometheus_client::metrics::counter::Counter;
use prometheus_client::registry::Registry;

use crate::error::MediaError;
use crate::metadata::MediaRecord;
use crate::repository::MediaRepository;

/// How often the sweeper looks, once the first pass (a minute after start) is done.
pub const SWEEP_INTERVAL: Duration = Duration::from_secs(3600);

/// The first pass waits this long after start, so a restart does not add a scan of the whole
/// media table to everything else a starting server does.
pub const FIRST_SWEEP_AFTER: Duration = Duration::from_secs(60);

/// What one pass did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RetentionPass {
    /// Remote copies older than the retention, and so selected.
    pub selected: u64,
    /// Of those, deleted now.
    pub deleted: u64,
    /// The bytes those held.
    pub bytes: u64,
    /// Selected but already gone (another replica's pass got there first).
    pub already_gone: u64,
    /// Selected but not deleted: the object store or the database refused.
    pub failed: u64,
    /// Remote copies kept although older, because they are protected or quarantined.
    pub kept: u64,
}

/// The sweeper's counters, registered on `hs serve`'s `/metrics`.
#[derive(Clone, Default)]
pub struct RetentionMetrics {
    deleted: Counter,
    bytes: Counter,
    failed: Counter,
}

impl RetentionMetrics {
    /// Registers the counters into `metrics`'s shared registry.
    #[must_use]
    pub fn register(metrics: &hs_telemetry::metrics::Metrics) -> Self {
        let this = Self::default();
        metrics.with_registry(|registry: &mut Registry| {
            // No `_total` suffix here: the text encoder appends it to a counter.
            registry.register(
                "hs_media_remote_retention_deleted",
                "Cached copies of other servers' media deleted because nobody asked for them within media.remote_media_retention.",
                this.deleted.clone(),
            );
            registry.register(
                "hs_media_remote_retention_deleted_bytes",
                "Bytes those cached copies held.",
                this.bytes.clone(),
            );
            registry.register(
                "hs_media_remote_retention_failed",
                "Cached copies of other servers' media the retention sweeper selected and could not delete.",
                this.failed.clone(),
            );
        });
        this
    }

    fn record(&self, pass: &RetentionPass) {
        self.deleted.inc_by(pass.deleted);
        self.bytes.inc_by(pass.bytes);
        self.failed.inc_by(pass.failed);
    }
}

/// Whether `record` is a copy of another server's media this pass may delete, given the
/// cut-off: `Some(true)` delete, `Some(false)` old enough but kept, `None` not a candidate.
fn verdict(record: &MediaRecord, own_server: &str, cutoff_ms: u64) -> Option<bool> {
    if record.server_name == own_server || !record.completed || record.last_used_ms() >= cutoff_ms {
        return None;
    }
    Some(!(record.safe_from_quarantine || record.quarantined_by.is_some()))
}

/// One pass: deletes the remote copies unused for longer than `retention`, as of the
/// repository's clock.
///
/// # Errors
/// [`MediaError`] when the media table cannot be listed; a copy that cannot be deleted is
/// counted in [`RetentionPass::failed`] and the pass goes on.
pub async fn sweep_once<B: KvBackend + 'static>(
    repository: &Arc<MediaRepository<B>>,
    retention: Duration,
) -> Result<RetentionPass, MediaError> {
    let retention_ms = u64::try_from(retention.as_millis()).unwrap_or(u64::MAX);
    let cutoff_ms = repository.now_ms().saturating_sub(retention_ms);
    let lister = repository.clone();
    let records = tokio::task::spawn_blocking(move || lister.list_media())
        .await
        .map_err(|e| MediaError::Metadata(e.to_string()))??;
    let own = repository.server_name().to_owned();
    let mut pass = RetentionPass::default();
    for record in records {
        match verdict(&record, &own, cutoff_ms) {
            None => {}
            Some(false) => pass.kept += 1,
            Some(true) => {
                pass.selected += 1;
                match repository
                    .delete_media(&record.server_name, &record.media_id)
                    .await
                {
                    Ok(deleted) => {
                        pass.deleted += 1;
                        pass.bytes += deleted.byte_length.unwrap_or(0);
                    }
                    Err(MediaError::NotFound) => pass.already_gone += 1,
                    Err(error) => {
                        pass.failed += 1;
                        tracing::warn!(
                            target: "hs_media::retention",
                            server_name = %record.server_name,
                            media_id = %record.media_id,
                            %error,
                            "could not delete a cached remote copy past media.remote_media_retention; the next pass tries again"
                        );
                    }
                }
            }
        }
    }
    Ok(pass)
}

/// Runs [`sweep_once`] a minute after start and then every [`SWEEP_INTERVAL`], with the
/// retention read from the repository's live configuration each time; nothing is deleted
/// while it is unset.
pub fn spawn_sweeper<B: KvBackend + 'static>(
    repository: Arc<MediaRepository<B>>,
    metrics: RetentionMetrics,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        tokio::time::sleep(FIRST_SWEEP_AFTER).await;
        let mut ticks = tokio::time::interval(SWEEP_INTERVAL);
        ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            ticks.tick().await;
            let Some(retention) = repository.config().remote_media_retention else {
                continue;
            };
            match sweep_once(&repository, retention.into()).await {
                Ok(pass) => {
                    metrics.record(&pass);
                    if pass.selected > 0 || pass.kept > 0 {
                        tracing::info!(
                            target: "hs_media::retention",
                            retention = %retention,
                            deleted = pass.deleted,
                            bytes = pass.bytes,
                            already_gone = pass.already_gone,
                            failed = pass.failed,
                            kept_protected_or_quarantined = pass.kept,
                            "deleted cached copies of other servers' media nobody asked for within media.remote_media_retention"
                        );
                    }
                }
                Err(error) => tracing::warn!(
                    target: "hs_media::retention",
                    %error,
                    "could not list media for the remote-media retention pass; the next pass tries again"
                ),
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metadata::MetadataStore;
    use crate::policy::{InMemoryQuotaPolicy, UploadContext};
    use crate::thumbnail::ThumbnailPolicy;
    use hs_config::MediaConfig;
    use hs_kv::memory::MemoryBackend;
    use object_store::ObjectStore;
    use object_store::memory::InMemory;
    use std::sync::atomic::{AtomicU64, Ordering};

    const HOUR: u64 = 3_600_000;

    fn repo(clock: Arc<AtomicU64>) -> Arc<MediaRepository<MemoryBackend>> {
        let object_store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        Arc::new(MediaRepository::new(
            object_store,
            MetadataStore::open(MemoryBackend::new()).unwrap(),
            Arc::new(MediaConfig::default()),
            Arc::new(InMemoryQuotaPolicy::unlimited()),
            ThumbnailPolicy::default(),
            "example.org".to_owned(),
            move || clock.load(Ordering::SeqCst),
        ))
    }

    /// An upload made at the clock's time, then turned into another server's copy (or left
    /// local), last used at `used_ms`.
    async fn item(
        repo: &MediaRepository<MemoryBackend>,
        server: &str,
        used_ms: Option<u64>,
        edit: impl FnOnce(&mut MediaRecord),
    ) -> String {
        let alice = UploadContext {
            user_id: "@alice:example.org".into(),
            server_name: "example.org".into(),
            appservice_id: None,
        };
        let id = repo
            .upload(
                &alice,
                "image/png",
                None,
                crate::test_fixtures::valid_png().into(),
            )
            .await
            .unwrap();
        let mut record = repo
            .metadata()
            .get_media("example.org", id.as_str())
            .unwrap()
            .unwrap();
        record.server_name = server.into();
        record.last_accessed_ms = used_ms;
        edit(&mut record);
        repo.metadata().put_media(&record).unwrap();
        id.as_str().to_owned()
    }

    #[tokio::test]
    async fn a_pass_deletes_old_remote_copies_and_keeps_the_rest() {
        let clock = Arc::new(AtomicU64::new(HOUR));
        let repo = repo(clock.clone());
        // Fetched at hour 1, never served since: old by hour 50.
        let stale = item(&repo, "matrix.org", None, |_| {}).await;
        // Served at hour 45: recent.
        let used = item(&repo, "matrix.org", Some(45 * HOUR), |_| {}).await;
        // Old, but protected or quarantined.
        let protected = item(&repo, "matrix.org", None, |r| r.safe_from_quarantine = true).await;
        let quarantined = item(&repo, "kde.org", None, |r| {
            r.quarantined_by = Some("@ops:example.org".into());
        })
        .await;
        // Old, but this server's own upload.
        let local = item(&repo, "example.org", None, |_| {}).await;

        clock.store(50 * HOUR, Ordering::SeqCst);
        let pass = sweep_once(&repo, Duration::from_secs(24 * 3600))
            .await
            .unwrap();
        assert_eq!(pass.selected, 1, "{pass:?}");
        assert_eq!(pass.deleted, 1);
        assert!(pass.bytes > 0);
        assert_eq!(pass.kept, 2);
        let exists =
            |server: &str, id: &str| repo.metadata().get_media(server, id).unwrap().is_some();
        assert!(!exists("matrix.org", &stale));
        assert!(exists("matrix.org", &used));
        assert!(exists("matrix.org", &protected));
        assert!(exists("kde.org", &quarantined));
        assert!(exists("example.org", &local));

        // A second pass finds nothing more to do.
        let again = sweep_once(&repo, Duration::from_secs(24 * 3600))
            .await
            .unwrap();
        assert_eq!(again.deleted, 0);
    }
}
