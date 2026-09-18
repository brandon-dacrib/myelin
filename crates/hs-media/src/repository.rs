//! [`MediaRepository`]: ties [`crate::store`] (bytes), [`crate::metadata`] (rows),
//! [`crate::policy`] (quotas) and [`crate::thumbnail`] (generation) together into the operations
//! [`crate::routes`] calls. This is the one place that knows how a synchronous upload, an
//! async-upload reservation, a download and a thumbnail request each work end to end; the HTTP
//! layer only translates requests into calls here and results into responses.

use std::sync::Arc;

use bytes::Bytes;
use hs_kv::KvBackend;
use object_store::path::Path as ObjectPath;
use object_store::{ObjectStore, ObjectStoreExt};

use hs_config::MediaConfig;

use crate::error::MediaError;
use crate::id::MediaId;
use crate::metadata::{MediaRecord, MetadataStore, ThumbnailRecord};
use crate::policy::{UploadContext, UploadPolicy};
use crate::security::{ByteRange, RangeOutcome, parse_range};
use crate::sniff::DecodeLimits;
use crate::thumbnail::{self, ThumbnailMethod, ThumbnailPolicy};

/// How long an async-upload reservation (`POST .../create`) stays valid before it expires if the
/// content is never `PUT`. The spec leaves the exact duration to the server; this matches
/// Synapse's observed default (`synapse/config/media.py`'s `unused_expiration_time`, 24 hours).
pub const DEFAULT_RESERVATION_TTL_MS: u64 = 24 * 60 * 60 * 1000;

/// Everything needed to serve a download or thumbnail response body: the record plus the actual
/// bytes (already sliced to a [`ByteRange`] if one was requested).
#[derive(Debug, Clone)]
pub struct ContentBytes {
    /// The full resource length (even for a partial response).
    pub total_len: u64,
    /// The bytes actually being returned (the whole resource, or one range of it).
    pub bytes: Bytes,
    /// `Some` if this is a partial (`206`) response.
    pub range: Option<ByteRange>,
}

/// The media repository.
#[derive(Clone)]
pub struct MediaRepository<B: KvBackend> {
    object_store: Arc<dyn ObjectStore>,
    metadata: MetadataStore<B>,
    config: Arc<MediaConfig>,
    policy: Arc<dyn UploadPolicy>,
    thumbnail_policy: ThumbnailPolicy,
    decode_limits: DecodeLimits,
    /// This homeserver's own name (used as `server_name` for locally uploaded media).
    server_name: String,
    clock: Arc<dyn Fn() -> u64 + Send + Sync>,
}

impl<B: KvBackend> MediaRepository<B> {
    /// Builds a repository from its parts. `now_ms` is injectable so tests control time (the real
    /// server passes `|| system time in ms`).
    #[must_use]
    pub fn new(
        object_store: Arc<dyn ObjectStore>,
        metadata: MetadataStore<B>,
        config: Arc<MediaConfig>,
        policy: Arc<dyn UploadPolicy>,
        thumbnail_policy: ThumbnailPolicy,
        server_name: String,
        now_ms: impl Fn() -> u64 + Send + Sync + 'static,
    ) -> Self {
        Self {
            object_store,
            metadata,
            config,
            policy,
            thumbnail_policy,
            decode_limits: DecodeLimits::default(),
            server_name,
            clock: Arc::new(now_ms),
        }
    }

    fn now_ms(&self) -> u64 {
        (self.clock)()
    }

    /// This homeserver's own server name (the `server_name` recorded for anything uploaded
    /// through [`MediaRepository::upload`] or [`MediaRepository::create_reservation`]).
    #[must_use]
    pub fn server_name(&self) -> &str {
        &self.server_name
    }

    /// The configured maximum upload size, for the `/config` endpoint's `m.upload.size`
    /// capability.
    #[must_use]
    pub fn max_upload_size(&self) -> u64 {
        self.config.max_upload_size.as_u64()
    }

    /// A synchronous upload (`POST .../upload`): generates a new media ID, checks size and quota,
    /// stores the bytes, and records a completed row.
    ///
    /// # Errors
    /// [`MediaError::TooLarge`] if `bytes` exceeds `max_upload_size`; [`MediaError::QuotaExceeded`]
    /// if `ctx`'s policy check fails; [`MediaError::Store`]/[`MediaError::Metadata`] on backend
    /// failure.
    pub async fn upload(
        &self,
        ctx: &UploadContext,
        content_type: &str,
        filename: Option<String>,
        bytes: Bytes,
    ) -> Result<MediaId, MediaError> {
        self.check_size(bytes.len() as u64)?;
        self.policy.check(ctx, Some(bytes.len() as u64)).await?;

        let media_id = MediaId::generate();
        let key = crate::store::content_key(&self.server_name, &media_id);
        self.object_store
            .put(&key, bytes.clone().into())
            .await
            .map_err(MediaError::from)?;

        let record = MediaRecord {
            server_name: self.server_name.clone(),
            media_id: media_id.as_str().to_string(),
            content_type: content_type.to_string(),
            upload_name: filename,
            byte_length: Some(bytes.len() as u64),
            created_ms: self.now_ms(),
            uploader: Some(ctx.user_id.clone()),
            completed: true,
            expires_at_ms: None,
            quarantined_by: None,
            safe_from_quarantine: false,
        };
        self.metadata.put_media(&record)?;
        self.policy.record(ctx, bytes.len() as u64).await;
        Ok(media_id)
    }

    /// Async upload, step 1 (`POST .../create`): reserves a media ID and returns it, with no
    /// content yet. The reservation expires after [`DEFAULT_RESERVATION_TTL_MS`] unless
    /// [`MediaRepository::complete_reservation`] is called first.
    ///
    /// # Errors
    /// [`MediaError::Metadata`] on backend failure.
    ///
    /// Returns the new media ID and the reservation's expiry (milliseconds since the Unix epoch)
    /// — the spec's `POST .../create` response carries this as `unused_expires_at`.
    pub fn create_reservation(&self, ctx: &UploadContext) -> Result<(MediaId, u64), MediaError> {
        let media_id = MediaId::generate();
        let now = self.now_ms();
        let expires_at = now + DEFAULT_RESERVATION_TTL_MS;
        let record = MediaRecord {
            server_name: self.server_name.clone(),
            media_id: media_id.as_str().to_string(),
            content_type: "application/octet-stream".to_string(),
            upload_name: None,
            byte_length: None,
            created_ms: now,
            uploader: Some(ctx.user_id.clone()),
            completed: false,
            expires_at_ms: Some(expires_at),
            quarantined_by: None,
            safe_from_quarantine: false,
        };
        self.metadata.put_media(&record)?;
        Ok((media_id, expires_at))
    }

    /// Async upload, step 2 (`PUT .../upload/{server}/{mediaId}`): fills a reservation created by
    /// [`MediaRepository::create_reservation`].
    ///
    /// The spec requires this to only ever be called for media on *this* server, by the same user
    /// that reserved it, exactly once; [`crate::routes`] is responsible for checking `server_name
    /// == this server` and `ctx.user_id == record.uploader` before calling this (kept out of this
    /// method so it stays a pure "does the reservation still exist and is it still open" check,
    /// testable without an `hs-auth` `Requester` in hand).
    ///
    /// # Errors
    /// [`MediaError::NotFound`] if no such reservation exists; [`MediaError::UploadExpired`] if it
    /// expired; [`MediaError::TooLarge`]/[`MediaError::QuotaExceeded`] as for
    /// [`MediaRepository::upload`]; a plain `Err` with `completed: true` semantics is represented
    /// as [`MediaError::InvalidInput`] ("already uploaded") since re-`PUT`ing is a caller bug, not
    /// a missing/expired reservation.
    pub async fn complete_reservation(
        &self,
        ctx: &UploadContext,
        media_id: &MediaId,
        content_type: &str,
        bytes: Bytes,
    ) -> Result<(), MediaError> {
        let record = self
            .metadata
            .get_media(&self.server_name, media_id.as_str())?
            .ok_or(MediaError::NotFound)?;
        if record.completed {
            return Err(MediaError::InvalidInput(
                "this media ID has already been uploaded".into(),
            ));
        }
        if let Some(expires_at) = record.expires_at_ms
            && expires_at < self.now_ms()
        {
            return Err(MediaError::UploadExpired);
        }
        self.check_size(bytes.len() as u64)?;
        self.policy.check(ctx, Some(bytes.len() as u64)).await?;

        let key = crate::store::content_key(&self.server_name, media_id);
        self.object_store
            .put(&key, bytes.clone().into())
            .await
            .map_err(MediaError::from)?;
        self.metadata.complete_upload(
            &self.server_name,
            media_id.as_str(),
            content_type,
            bytes.len() as u64,
        )?;
        self.policy.record(ctx, bytes.len() as u64).await;
        Ok(())
    }

    fn check_size(&self, len: u64) -> Result<(), MediaError> {
        let limit = self.config.max_upload_size.as_u64();
        if len > limit {
            return Err(MediaError::TooLarge { limit });
        }
        Ok(())
    }

    /// Looks up a media row for download/thumbnail purposes, applying the "quarantined looks like
    /// not-found" and "not-yet-uploaded"/"expired" rules uniformly.
    ///
    /// # Errors
    /// [`MediaError::NotFound`], [`MediaError::NotYetUploaded`], [`MediaError::UploadExpired`], or
    /// a backend [`MediaError::Metadata`].
    pub fn get_record(&self, server_name: &str, media_id: &str) -> Result<MediaRecord, MediaError> {
        let record = self
            .metadata
            .get_media(server_name, media_id)?
            .ok_or(MediaError::NotFound)?;
        if record.quarantined_by.is_some() && !record.safe_from_quarantine {
            return Err(MediaError::Quarantined);
        }
        if !record.completed {
            if let Some(expires_at) = record.expires_at_ms
                && expires_at < self.now_ms()
            {
                return Err(MediaError::UploadExpired);
            }
            return Err(MediaError::NotYetUploaded);
        }
        Ok(record)
    }

    /// Fetches content bytes for a download, honoring an HTTP `Range` header.
    ///
    /// # Errors
    /// [`MediaError::RangeNotSatisfiable`] if `range_header` names an unsatisfiable range;
    /// otherwise as [`MediaRepository::get_record`] plus [`MediaError::Store`] on object-store
    /// failure.
    pub async fn get_content(
        &self,
        record: &MediaRecord,
        range_header: Option<&str>,
    ) -> Result<ContentBytes, MediaError> {
        let key =
            crate::store::content_key(&record.server_name, &parse_media_id(&record.media_id)?);
        let total = record.byte_length.unwrap_or(0);
        match parse_range(range_header, total) {
            RangeOutcome::Full => {
                let bytes = self
                    .object_store
                    .get(&key)
                    .await
                    .map_err(MediaError::from)?
                    .bytes()
                    .await
                    .map_err(MediaError::from)?;
                Ok(ContentBytes {
                    total_len: total,
                    bytes,
                    range: None,
                })
            }
            RangeOutcome::Partial(r) => {
                let bytes = self
                    .object_store
                    .get_range(&key, r.start..(r.end + 1))
                    .await
                    .map_err(MediaError::from)?;
                Ok(ContentBytes {
                    total_len: total,
                    bytes,
                    range: Some(r),
                })
            }
            RangeOutcome::Unsatisfiable => Err(MediaError::RangeNotSatisfiable),
        }
    }

    /// Fetches (generating and caching if necessary) one thumbnail variant.
    ///
    /// # Errors
    /// [`MediaError::UnsupportedThumbnail`] if `(width, height, method)` is not allowed by this
    /// repository's [`ThumbnailPolicy`]; otherwise as [`MediaRepository::get_record`] plus
    /// [`MediaError::DecodeFailed`]/[`MediaError::Store`].
    pub async fn get_thumbnail(
        &self,
        record: &MediaRecord,
        width: u32,
        height: u32,
        method: ThumbnailMethod,
    ) -> Result<(ThumbnailRecord, Bytes), MediaError> {
        if !self.thumbnail_policy.allows(width, height, method) {
            return Err(MediaError::UnsupportedThumbnail);
        }
        let media_id = parse_media_id(&record.media_id)?;

        // Try each format already cached for this exact (width, height, method): thumbnails are
        // always PNG or JPEG (`crate::thumbnail::output_format_for`), so two lookups cover it.
        for content_type in ["image/png", "image/jpeg"] {
            if let Some(cached) = self.metadata.get_thumbnail(
                &record.server_name,
                &record.media_id,
                width,
                height,
                method,
                content_type,
            )? {
                let variant = ThumbnailRecord::variant_key(width, height, method, content_type);
                let key = crate::store::thumbnail_key(&record.server_name, &media_id, &variant);
                let bytes = self
                    .object_store
                    .get(&key)
                    .await
                    .map_err(MediaError::from)?
                    .bytes()
                    .await
                    .map_err(MediaError::from)?;
                return Ok((cached, bytes));
            }
        }

        // Not cached: fetch the source, generate, store, record.
        let source_key = crate::store::content_key(&record.server_name, &media_id);
        let source_bytes = self
            .object_store
            .get(&source_key)
            .await
            .map_err(MediaError::from)?
            .bytes()
            .await
            .map_err(MediaError::from)?;
        let (thumb_bytes, mime) =
            thumbnail::generate(&source_bytes, width, height, method, self.decode_limits)?;

        let variant = ThumbnailRecord::variant_key(width, height, method, mime);
        let key = crate::store::thumbnail_key(&record.server_name, &media_id, &variant);
        self.object_store
            .put(&key, Bytes::from(thumb_bytes.clone()).into())
            .await
            .map_err(MediaError::from)?;

        let thumb_record = ThumbnailRecord {
            width,
            height,
            method,
            content_type: mime.to_string(),
            byte_length: thumb_bytes.len() as u64,
            created_ms: self.now_ms(),
        };
        self.metadata
            .put_thumbnail(&record.server_name, &record.media_id, &thumb_record)?;

        Ok((thumb_record, Bytes::from(thumb_bytes)))
    }

    /// Sets or clears quarantine on a media item (admin operation; `crate::routes` is responsible
    /// for checking `Requester::is_admin` before calling this).
    ///
    /// # Errors
    /// [`MediaError::NotFound`] if no such item exists.
    pub fn set_quarantined(
        &self,
        server_name: &str,
        media_id: &str,
        by: Option<&str>,
    ) -> Result<(), MediaError> {
        if self.metadata.set_quarantined(server_name, media_id, by)? {
            Ok(())
        } else {
            Err(MediaError::NotFound)
        }
    }

    /// Direct access to the object store, for callers (the Synapse importer, admin tooling) that
    /// need to write content this repository did not itself receive through
    /// [`MediaRepository::upload`] (an already-known key and bytes, imported verbatim).
    #[must_use]
    pub fn object_store(&self) -> &Arc<dyn ObjectStore> {
        &self.object_store
    }

    /// Direct access to the metadata store, for the same reason.
    #[must_use]
    pub fn metadata(&self) -> &MetadataStore<B> {
        &self.metadata
    }
}

fn parse_media_id(s: &str) -> Result<MediaId, MediaError> {
    MediaId::parse(s).map_err(|e| MediaError::Metadata(format!("stored an invalid media id: {e}")))
}

/// The object-store key for arbitrary content under `server_name`/`media_id` — re-exported at
/// this module's surface for callers that already have a [`MediaId`] and just need the same
/// sharding scheme [`MediaRepository`] itself uses (the Synapse importer, most notably).
#[must_use]
pub fn content_object_key(server_name: &str, media_id: &MediaId) -> ObjectPath {
    crate::store::content_key(server_name, media_id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use hs_kv::memory::MemoryBackend;
    use object_store::memory::InMemory;

    fn repo() -> MediaRepository<MemoryBackend> {
        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let metadata = MetadataStore::open(MemoryBackend::new()).unwrap();
        MediaRepository::new(
            store,
            metadata,
            Arc::new(MediaConfig::default()),
            Arc::new(crate::policy::InMemoryQuotaPolicy::unlimited()),
            ThumbnailPolicy::default(),
            "example.org".to_string(),
            || 1_000_000,
        )
    }

    fn ctx() -> UploadContext {
        UploadContext {
            user_id: "@alice:example.org".into(),
            server_name: "example.org".into(),
        }
    }

    #[tokio::test]
    async fn synchronous_upload_round_trips() {
        let repo = repo();
        let bytes = Bytes::from(crate::test_fixtures::valid_png());
        let id = repo
            .upload(&ctx(), "image/png", Some("cat.png".into()), bytes.clone())
            .await
            .unwrap();

        let record = repo.get_record("example.org", id.as_str()).unwrap();
        assert!(record.is_servable());
        assert_eq!(record.content_type, "image/png");

        let content = repo.get_content(&record, None).await.unwrap();
        assert_eq!(content.bytes, bytes);
        assert_eq!(content.total_len, bytes.len() as u64);
        assert!(content.range.is_none());
    }

    #[tokio::test]
    async fn upload_over_the_size_limit_is_rejected() {
        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let metadata = MetadataStore::open(MemoryBackend::new()).unwrap();
        let config = MediaConfig {
            max_upload_size: hs_config::ByteSize::bytes(0),
            ..MediaConfig::default()
        };
        let repo = MediaRepository::new(
            store,
            metadata,
            Arc::new(config),
            Arc::new(crate::policy::InMemoryQuotaPolicy::unlimited()),
            ThumbnailPolicy::default(),
            "example.org".to_string(),
            || 0,
        );
        let err = repo
            .upload(&ctx(), "image/png", None, Bytes::from_static(b"abc"))
            .await
            .unwrap_err();
        assert!(matches!(err, MediaError::TooLarge { .. }));
    }

    #[tokio::test]
    async fn range_request_returns_a_slice() {
        let repo = repo();
        let bytes = Bytes::from_static(b"0123456789");
        let id = repo
            .upload(&ctx(), "text/plain", None, bytes)
            .await
            .unwrap();
        let record = repo.get_record("example.org", id.as_str()).unwrap();

        let partial = repo.get_content(&record, Some("bytes=2-4")).await.unwrap();
        assert_eq!(partial.bytes, Bytes::from_static(b"234"));
        assert_eq!(partial.range.unwrap().start, 2);
    }

    #[tokio::test]
    async fn async_upload_lifecycle() {
        let repo = repo();
        let (id, _expires_at) = repo.create_reservation(&ctx()).unwrap();

        // Not yet uploaded.
        let err = repo.get_record("example.org", id.as_str()).unwrap_err();
        assert!(matches!(err, MediaError::NotYetUploaded));

        repo.complete_reservation(
            &ctx(),
            &id,
            "image/png",
            Bytes::from(crate::test_fixtures::valid_png()),
        )
        .await
        .unwrap();

        let record = repo.get_record("example.org", id.as_str()).unwrap();
        assert!(record.is_servable());
        assert_eq!(record.content_type, "image/png");
    }

    #[tokio::test]
    async fn completing_an_unknown_reservation_is_not_found() {
        let repo = repo();
        let fake = MediaId::generate();
        let err = repo
            .complete_reservation(&ctx(), &fake, "image/png", Bytes::from_static(b"x"))
            .await
            .unwrap_err();
        assert!(matches!(err, MediaError::NotFound));
    }

    #[tokio::test]
    async fn completing_twice_is_rejected() {
        let repo = repo();
        let (id, _expires_at) = repo.create_reservation(&ctx()).unwrap();
        repo.complete_reservation(&ctx(), &id, "image/png", Bytes::from_static(b"x"))
            .await
            .unwrap();
        let err = repo
            .complete_reservation(&ctx(), &id, "image/png", Bytes::from_static(b"y"))
            .await
            .unwrap_err();
        assert!(matches!(err, MediaError::InvalidInput(_)));
    }

    #[tokio::test]
    async fn expired_reservation_cannot_be_completed() {
        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let metadata = MetadataStore::open(MemoryBackend::new()).unwrap();
        // A clock that starts at 0 (so the reservation's expiry is computed relative to 0) and
        // is advanced past `DEFAULT_RESERVATION_TTL_MS` before the completion attempt.
        let now = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let now_for_clock = now.clone();
        let repo = MediaRepository::new(
            store,
            metadata,
            Arc::new(MediaConfig::default()),
            Arc::new(crate::policy::InMemoryQuotaPolicy::unlimited()),
            ThumbnailPolicy::default(),
            "example.org".to_string(),
            move || now_for_clock.load(std::sync::atomic::Ordering::SeqCst),
        );
        let (id, expires_at) = repo.create_reservation(&ctx()).unwrap();
        assert_eq!(expires_at, DEFAULT_RESERVATION_TTL_MS);

        now.store(
            DEFAULT_RESERVATION_TTL_MS + 1,
            std::sync::atomic::Ordering::SeqCst,
        );
        let err = repo
            .complete_reservation(&ctx(), &id, "image/png", Bytes::from_static(b"x"))
            .await
            .unwrap_err();
        assert!(matches!(err, MediaError::UploadExpired));
    }

    #[tokio::test]
    async fn thumbnail_is_generated_then_cached() {
        let repo = repo();
        let png = crate::test_fixtures::valid_png();
        let id = repo
            .upload(&ctx(), "image/png", None, Bytes::from(png))
            .await
            .unwrap();
        let record = repo.get_record("example.org", id.as_str()).unwrap();

        let (thumb1, bytes1) = repo
            .get_thumbnail(&record, 32, 32, ThumbnailMethod::Crop)
            .await
            .unwrap();
        assert_eq!(thumb1.width, 32);
        assert!(!bytes1.is_empty());

        let (thumb2, bytes2) = repo
            .get_thumbnail(&record, 32, 32, ThumbnailMethod::Crop)
            .await
            .unwrap();
        assert_eq!(thumb1, thumb2);
        assert_eq!(bytes1, bytes2);
    }

    #[tokio::test]
    async fn unconfigured_thumbnail_size_is_rejected_by_default() {
        let repo = repo();
        let png = crate::test_fixtures::valid_png();
        let id = repo
            .upload(&ctx(), "image/png", None, Bytes::from(png))
            .await
            .unwrap();
        let record = repo.get_record("example.org", id.as_str()).unwrap();
        let err = repo
            .get_thumbnail(&record, 123, 123, ThumbnailMethod::Crop)
            .await
            .unwrap_err();
        assert!(matches!(err, MediaError::UnsupportedThumbnail));
    }

    #[tokio::test]
    async fn quarantine_hides_content_but_admin_can_still_flag_it() {
        let repo = repo();
        let png = crate::test_fixtures::valid_png();
        let id = repo
            .upload(&ctx(), "image/png", None, Bytes::from(png))
            .await
            .unwrap();
        repo.set_quarantined("example.org", id.as_str(), Some("@admin:example.org"))
            .unwrap();
        let err = repo.get_record("example.org", id.as_str()).unwrap_err();
        assert!(matches!(err, MediaError::Quarantined));
    }

    #[tokio::test]
    async fn not_found_for_unknown_media() {
        let repo = repo();
        let err = repo.get_record("example.org", "doesnotexist").unwrap_err();
        assert!(matches!(err, MediaError::NotFound));
    }
}
