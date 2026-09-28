//! `hs-admin`'s [`MediaSource`] over this crate's [`MediaRepository`]: what the admin API's
//! `media.*` operations and the interface's Media page read and change.
//!
//! One row of the media table is one item. It is `local` when its server name is this
//! server's, `remote` otherwise (a cached copy of another server's media). Async-upload
//! reservations whose content never arrived are not listed: there is nothing to look at or
//! delete yet, and they expire on their own.
//!
//! An item is reported `quarantined` when it is actually withheld: quarantined and not
//! protected. That includes an upload still waiting on a `defer`-mode content scan, which is
//! withheld until the scan decides.

use std::sync::Arc;

use async_trait::async_trait;
use hs_admin::media::{AdminMediaItem, MediaSource};
use hs_admin::sources::SourceError;
use hs_kv::KvBackend;

use crate::error::MediaError;
use crate::metadata::MediaRecord;
use crate::repository::MediaRepository;

/// See the module docs.
pub struct RepositoryMediaSource<B: KvBackend> {
    repository: Arc<MediaRepository<B>>,
}

impl<B: KvBackend> RepositoryMediaSource<B> {
    /// A source over `repository`, which is shared with the media routes.
    #[must_use]
    pub fn new(repository: Arc<MediaRepository<B>>) -> Self {
        Self { repository }
    }

    fn view(&self, record: &MediaRecord) -> AdminMediaItem {
        let origin = if record.server_name == self.repository.server_name() {
            "local"
        } else {
            "remote"
        };
        AdminMediaItem {
            server_name: record.server_name.clone(),
            media_id: record.media_id.clone(),
            origin: origin.to_owned(),
            uploader: record.uploader.clone(),
            upload_name: record.upload_name.clone(),
            content_type: Some(record.content_type.clone()).filter(|t| !t.is_empty()),
            size_bytes: record.byte_length.unwrap_or(0),
            created_at: rfc3339(record.created_ms),
            last_accessed_at: record.last_accessed_ms.map(rfc3339),
            quarantined: record.quarantined_by.is_some() && !record.safe_from_quarantine,
            protected: record.safe_from_quarantine,
        }
    }
}

fn rfc3339(ms: u64) -> String {
    hs_http::time::rfc3339_from_millis(i64::try_from(ms).unwrap_or(i64::MAX))
}

fn source_error(error: MediaError) -> SourceError {
    match error {
        MediaError::NotFound => SourceError::NotFound,
        other => SourceError::Unavailable(other.to_string()),
    }
}

#[async_trait]
impl<B: KvBackend + 'static> MediaSource for RepositoryMediaSource<B> {
    async fn list(&self) -> Result<Vec<AdminMediaItem>, SourceError> {
        // A scan of the whole table: off the async workers, as `hs-kv` reads are synchronous.
        let repository = self.repository.clone();
        let records = tokio::task::spawn_blocking(move || repository.list_media())
            .await
            .map_err(|e| SourceError::Unavailable(e.to_string()))?
            .map_err(source_error)?;
        Ok(records
            .iter()
            .filter(|r| r.completed)
            .map(|r| self.view(r))
            .collect())
    }

    async fn get(
        &self,
        server_name: &str,
        media_id: &str,
    ) -> Result<Option<AdminMediaItem>, SourceError> {
        Ok(self
            .repository
            .metadata()
            .get_media(server_name, media_id)
            .map_err(source_error)?
            .filter(|r| r.completed)
            .map(|r| self.view(&r)))
    }

    async fn delete(
        &self,
        server_name: &str,
        media_id: &str,
    ) -> Result<AdminMediaItem, SourceError> {
        let record = self
            .repository
            .delete_media(server_name, media_id)
            .await
            .map_err(source_error)?;
        Ok(self.view(&record))
    }

    async fn set_quarantined(
        &self,
        server_name: &str,
        media_id: &str,
        by: Option<&str>,
    ) -> Result<AdminMediaItem, SourceError> {
        let record = self
            .repository
            .quarantine(server_name, media_id, by)
            .map_err(source_error)?;
        Ok(self.view(&record))
    }

    async fn set_protected(
        &self,
        server_name: &str,
        media_id: &str,
        protected: bool,
    ) -> Result<AdminMediaItem, SourceError> {
        let record = self
            .repository
            .set_protected(server_name, media_id, protected)
            .map_err(source_error)?;
        Ok(self.view(&record))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metadata::MetadataStore;
    use crate::policy::{InMemoryQuotaPolicy, UploadContext};
    use crate::thumbnail::ThumbnailPolicy;
    use hs_config::MediaConfig;
    use hs_kv::memory::MemoryBackend;
    use object_store::memory::InMemory;
    use object_store::{ObjectStore, ObjectStoreExt};
    use std::sync::atomic::{AtomicU64, Ordering};

    fn repo_with_clock(clock: Arc<AtomicU64>) -> MediaRepository<MemoryBackend> {
        let object_store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        MediaRepository::new(
            object_store,
            MetadataStore::open(MemoryBackend::new()).unwrap(),
            Arc::new(MediaConfig::default()),
            Arc::new(InMemoryQuotaPolicy::unlimited()),
            ThumbnailPolicy::default(),
            "example.org".to_owned(),
            move || clock.load(Ordering::SeqCst),
        )
    }

    fn alice() -> UploadContext {
        UploadContext {
            user_id: "@alice:example.org".into(),
            server_name: "example.org".into(),
            appservice_id: None,
        }
    }

    async fn upload(repo: &MediaRepository<MemoryBackend>, name: &str) -> String {
        let png = crate::test_fixtures::valid_png();
        let id = repo
            .upload(&alice(), "image/png", Some(name.to_owned()), png.into())
            .await
            .unwrap();
        id.as_str().to_owned()
    }

    #[tokio::test]
    async fn lists_local_and_remote_and_hides_unfinished_reservations() {
        let clock = Arc::new(AtomicU64::new(1_000));
        let repo = Arc::new(repo_with_clock(clock.clone()));
        let id = upload(&repo, "cat.png").await;
        let mut remote = repo
            .metadata()
            .get_media("example.org", &id)
            .unwrap()
            .unwrap();
        remote.server_name = "matrix.org".into();
        remote.uploader = None;
        repo.metadata().put_media(&remote).unwrap();
        repo.create_reservation(&alice()).unwrap();

        let source = RepositoryMediaSource::new(repo.clone());
        let mut items = source.list().await.unwrap();
        items.sort_by(|a, b| a.server_name.cmp(&b.server_name));
        assert_eq!(items.len(), 2, "{items:?}");
        assert_eq!(items[0].server_name, "example.org");
        assert_eq!(items[0].origin, "local");
        assert_eq!(items[0].uploader.as_deref(), Some("@alice:example.org"));
        assert_eq!(items[0].upload_name.as_deref(), Some("cat.png"));
        assert_eq!(items[0].content_type.as_deref(), Some("image/png"));
        assert!(items[0].size_bytes > 0);
        assert_eq!(items[0].created_at, "1970-01-01T00:00:01.000Z");
        assert_eq!(items[0].last_accessed_at, None);
        assert_eq!(items[1].origin, "remote");
        assert_eq!(items[1].uploader, None);
    }

    #[tokio::test]
    async fn a_download_records_its_access_at_an_hours_resolution() {
        let clock = Arc::new(AtomicU64::new(1_000));
        let repo = Arc::new(repo_with_clock(clock.clone()));
        let id = upload(&repo, "cat.png").await;
        let source = RepositoryMediaSource::new(repo.clone());

        let record = repo.get_record("example.org", &id).unwrap();
        repo.get_content(&record, None).await.unwrap();
        let item = source.get("example.org", &id).await.unwrap().unwrap();
        assert_eq!(
            item.last_accessed_at.as_deref(),
            Some("1970-01-01T00:00:01.000Z")
        );

        // Within the hour: not rewritten.
        clock.store(1_000 + 60_000, Ordering::SeqCst);
        let record = repo.get_record("example.org", &id).unwrap();
        repo.get_content(&record, None).await.unwrap();
        assert_eq!(
            repo.metadata()
                .get_media("example.org", &id)
                .unwrap()
                .unwrap()
                .last_accessed_ms,
            Some(1_000)
        );

        // Past it: rewritten, by a thumbnail as much as a download.
        let later = 1_000 + crate::repository::ACCESS_RESOLUTION_MS;
        clock.store(later, Ordering::SeqCst);
        let record = repo.get_record("example.org", &id).unwrap();
        repo.get_thumbnail(&record, 32, 32, crate::thumbnail::ThumbnailMethod::Crop)
            .await
            .unwrap();
        assert_eq!(
            repo.metadata()
                .get_media("example.org", &id)
                .unwrap()
                .unwrap()
                .last_accessed_ms,
            Some(later)
        );
    }

    #[tokio::test]
    async fn quarantine_withholds_and_protection_is_reported() {
        let repo = Arc::new(repo_with_clock(Arc::new(AtomicU64::new(1_000))));
        let id = upload(&repo, "cat.png").await;
        let source = RepositoryMediaSource::new(repo.clone());

        let item = source
            .set_quarantined("example.org", &id, Some("@ops:example.org"))
            .await
            .unwrap();
        assert!(item.quarantined);
        assert!(matches!(
            repo.get_record("example.org", &id),
            Err(MediaError::Quarantined)
        ));
        let item = source
            .set_quarantined("example.org", &id, None)
            .await
            .unwrap();
        assert!(!item.quarantined);
        assert!(repo.get_record("example.org", &id).is_ok());

        let item = source
            .set_protected("example.org", &id, true)
            .await
            .unwrap();
        assert!(item.protected);
        assert!(
            repo.metadata()
                .get_media("example.org", &id)
                .unwrap()
                .unwrap()
                .safe_from_quarantine
        );
        assert!(matches!(
            source.set_protected("example.org", "nope", true).await,
            Err(SourceError::NotFound)
        ));
    }

    #[tokio::test]
    async fn delete_removes_the_bytes_the_thumbnails_and_the_row() {
        let repo = Arc::new(repo_with_clock(Arc::new(AtomicU64::new(1_000))));
        let id = upload(&repo, "cat.png").await;
        let record = repo.get_record("example.org", &id).unwrap();
        repo.get_thumbnail(&record, 32, 32, crate::thumbnail::ThumbnailMethod::Crop)
            .await
            .unwrap();
        let media_id = crate::id::MediaId::parse(&id).unwrap();
        let content_key = crate::store::content_key("example.org", &media_id);
        assert!(repo.object_store().head(&content_key).await.is_ok());
        let thumbs = repo.metadata().list_thumbnails("example.org", &id).unwrap();
        assert_eq!(thumbs.len(), 1);
        let thumb_key = crate::store::thumbnail_key(
            "example.org",
            &media_id,
            &crate::metadata::ThumbnailRecord::variant_key(
                thumbs[0].width,
                thumbs[0].height,
                thumbs[0].method,
                &thumbs[0].content_type,
            ),
        );
        assert!(repo.object_store().head(&thumb_key).await.is_ok());

        let source = RepositoryMediaSource::new(repo.clone());
        let gone = source.delete("example.org", &id).await.unwrap();
        assert_eq!(gone.media_id, id);
        assert!(source.get("example.org", &id).await.unwrap().is_none());
        assert!(repo.object_store().head(&content_key).await.is_err());
        assert!(
            repo.metadata()
                .list_thumbnails("example.org", &id)
                .unwrap()
                .is_empty()
        );
        assert!(repo.object_store().head(&thumb_key).await.is_err());
        assert!(matches!(
            source.delete("example.org", &id).await,
            Err(SourceError::NotFound)
        ));
    }
}
