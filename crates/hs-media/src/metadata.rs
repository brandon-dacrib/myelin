//! Media and thumbnail metadata, stored over `hs-tables`/`hs-kv`. Object bytes live in
//! [`crate::store`]; this module is the row that says a given `(server_name, media_id)` exists,
//! who uploaded it, what it claims to be, and whether it has finished uploading.
//!
//! Generic over `B: hs_kv::KvBackend` so the same code runs against
//! `hs_kv::memory::MemoryBackend` in tests (per `docs/workstreams/README.md` rule 2) and against
//! `hs_kv::fjall_backend::FjallBackend` in production, with no different code path.
//!
//! `hs-kv` transactions are synchronous (no `.await` inside a transaction body, by design — see
//! `hs-kv`'s crate docs, "keep transactions short"); this module's methods are therefore
//! themselves synchronous even though [`crate::repository`] calls them from async handlers. For
//! `MemoryBackend` this is effectively free; a production `FjallBackend` caller on the hot HTTP
//! path should wrap calls in `tokio::task::spawn_blocking` if profiling shows contention — not
//! done here, since Phase 0 has only the in-memory backend wired through this crate's tests.

use bytes::Bytes;
use hs_kv::{KvBackend, RangeSpec, TransactConfig, transact};
use hs_tables::TableError;
use hs_tables::keyspace::TypedKeyspace;
use serde::{Deserialize, Serialize};

use crate::error::MediaError;
use crate::thumbnail::ThumbnailMethod;

/// One row of the media table: everything about a piece of content except its bytes.
///
/// Keyed by `(server_name, media_id)`: `server_name` is this server's own name for a local
/// upload, or the origin server's name for a cached copy of remote media (fetched over
/// federation — see `docs/rfcs/0007-federation-media.md`). This matches Synapse's split between
/// `local_media_repository` and `remote_media_cache` while keeping one table, since every lookup
/// this crate does is keyed the same way regardless of origin.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MediaRecord {
    /// The origin server (this server, for a local upload).
    pub server_name: String,
    /// The media ID (see [`crate::id::MediaId`]).
    pub media_id: String,
    /// The `Content-Type` supplied at upload time, served back verbatim (`security` module,
    /// Rule 4 — never re-derived).
    pub content_type: String,
    /// The uploader-supplied filename, if any (`?filename=` or the async-upload `create` body).
    pub upload_name: Option<String>,
    /// Size in bytes. `None` until an async upload's content has actually been `PUT`.
    pub byte_length: Option<u64>,
    /// Milliseconds since the Unix epoch this record was created (the `create` call for an async
    /// upload, or the single synchronous upload call).
    pub created_ms: u64,
    /// The uploading user, for local media only (`None` for a cached remote copy).
    pub uploader: Option<String>,
    /// Set once the content bytes exist in [`crate::store`] and are servable. An async-upload
    /// reservation with `completed: false` and a past [`MediaRecord::expires_at_ms`] is treated
    /// as [`MediaError::UploadExpired`] by [`crate::repository`], not as a live row.
    pub completed: bool,
    /// For an async-upload reservation (`POST .../create`): the deadline by which the content
    /// must be `PUT`, per the spec's async-upload expiry semantics. `None` for anything that was
    /// never a reservation (a synchronous upload, or a completed async one — Synapse keeps
    /// `PUT`-then-expired-later-than-completion irrelevant since a completed upload is permanent).
    pub expires_at_ms: Option<u64>,
    /// Set to the admin's identity when this media has been quarantined. A quarantined item is
    /// reported as plain "not found" to non-admin callers (see [`MediaError::Quarantined`]'s
    /// doc) but its row is kept, not deleted, so an admin can un-quarantine it.
    pub quarantined_by: Option<String>,
    /// If true, this item is exempt from quarantine (an admin marked it trusted). Mirrors
    /// Synapse's `safe_from_quarantine` column.
    pub safe_from_quarantine: bool,
}

impl MediaRecord {
    /// Whether this row currently represents servable content: completed, and not
    /// (un-safe-from-quarantine) quarantined.
    #[must_use]
    pub fn is_servable(&self) -> bool {
        self.completed && (self.quarantined_by.is_none() || self.safe_from_quarantine)
    }
}

/// One generated thumbnail's metadata.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ThumbnailRecord {
    /// Target width in pixels (the actual encoded image may be smaller for `scale`, which
    /// preserves aspect ratio — see [`crate::thumbnail`]).
    pub width: u32,
    /// Target height in pixels.
    pub height: u32,
    /// Crop or scale.
    pub method: ThumbnailMethod,
    /// The thumbnail's own encoded format (`image/png`, `image/jpeg`, ...).
    pub content_type: String,
    /// Encoded size in bytes.
    pub byte_length: u64,
    /// Creation time, milliseconds since the Unix epoch.
    pub created_ms: u64,
}

impl ThumbnailRecord {
    /// The object-store-key-safe, lookup-key string this thumbnail is stored under:
    /// `<width>x<height>-<method>.<content-type subtype>`. Stable and collision-free for the
    /// finite set of (size, method, format) combinations this crate ever generates.
    #[must_use]
    pub fn variant_key(
        width: u32,
        height: u32,
        method: ThumbnailMethod,
        content_type: &str,
    ) -> String {
        let ext = content_type.rsplit('/').next().unwrap_or("bin");
        format!(
            "{width}x{height}-{}.{ext}",
            crate::thumbnail::method_str(method)
        )
    }
}

type MediaKey = (String, String);
type ThumbKey = (String, String, String);

/// Media and thumbnail metadata over one `hs-kv` backend.
#[derive(Clone)]
pub struct MetadataStore<B: KvBackend> {
    backend: B,
    media: TypedKeyspace<B::Keyspace, MediaKey>,
    thumbnails: TypedKeyspace<B::Keyspace, ThumbKey>,
}

impl<B: KvBackend> MetadataStore<B> {
    /// Opens (creating if necessary) this crate's two keyspaces on `backend`.
    ///
    /// # Errors
    /// Returns [`MediaError::Metadata`] if the backend could not open a keyspace.
    pub fn open(backend: B) -> Result<Self, MediaError> {
        let media = TypedKeyspace::new(
            backend
                .keyspace("hs_media.media")
                .map_err(|e| MediaError::Metadata(e.to_string()))?,
        );
        let thumbnails = TypedKeyspace::new(
            backend
                .keyspace("hs_media.thumbnails")
                .map_err(|e| MediaError::Metadata(e.to_string()))?,
        );
        Ok(Self {
            backend,
            media,
            thumbnails,
        })
    }

    /// Inserts or overwrites a media row.
    ///
    /// # Errors
    /// Returns [`MediaError::Metadata`] on a backend failure.
    pub fn put_media(&self, record: &MediaRecord) -> Result<(), MediaError> {
        let key = (record.server_name.clone(), record.media_id.clone());
        let value = serde_json::to_vec(record)
            .map_err(|e| MediaError::Metadata(format!("encoding media record: {e}")))?;
        transact(&self.backend, TransactConfig::default(), |txn| {
            self.media.put(txn, &key, &value).map_err(to_kv_err)
        })
        .map_err(|e| MediaError::Metadata(e.to_string()))
    }

    /// Reads a media row, if it exists.
    ///
    /// # Errors
    /// Returns [`MediaError::Metadata`] on a backend failure or an undecodable stored row.
    pub fn get_media(
        &self,
        server_name: &str,
        media_id: &str,
    ) -> Result<Option<MediaRecord>, MediaError> {
        let snapshot = self.backend.snapshot();
        let key = (server_name.to_string(), media_id.to_string());
        let bytes = self
            .media
            .get(&snapshot, &key)
            .map_err(|e: TableError| MediaError::Metadata(e.to_string()))?;
        decode_optional(bytes)
    }

    /// Marks an async-upload reservation as completed: sets `content_type`, `byte_length` and
    /// `completed: true`, clears `expires_at_ms`. Fails silently (returns `Ok(false)`) if no row
    /// existed, so callers can distinguish "no such reservation" from a backend error.
    ///
    /// # Errors
    /// Returns [`MediaError::Metadata`] on a backend failure.
    pub fn complete_upload(
        &self,
        server_name: &str,
        media_id: &str,
        content_type: &str,
        byte_length: u64,
    ) -> Result<bool, MediaError> {
        let key = (server_name.to_string(), media_id.to_string());
        transact(&self.backend, TransactConfig::default(), |txn| {
            let Some(bytes) = self.media.get(txn, &key).map_err(to_kv_err)? else {
                return Ok(false);
            };
            let mut record: MediaRecord = serde_json::from_slice(&bytes)
                .map_err(|e| hs_kv::KvError::backend(DecodeError(e.to_string())))?;
            record.content_type = content_type.to_string();
            record.byte_length = Some(byte_length);
            record.completed = true;
            record.expires_at_ms = None;
            let value = serde_json::to_vec(&record)
                .map_err(|e| hs_kv::KvError::backend(DecodeError(e.to_string())))?;
            self.media.put(txn, &key, &value).map_err(to_kv_err)?;
            Ok(true)
        })
        .map_err(|e| MediaError::Metadata(e.to_string()))
    }

    /// Overwrites a completed row's `content_type`/`byte_length` in place, without touching
    /// anything else (`completed`, `expires_at_ms`, quarantine state). Used when a content-scanning
    /// provider's [`crate::scanning::types::Verdict::Replaced`] is applied *after* the client
    /// already received a `content_uri` — `defer`/`quarantine` mode's background scan
    /// (`crate::repository`) — so the stored bytes and the row describing them stay consistent
    /// even though the replacement was not known at the moment [`MetadataStore::put_media`] first
    /// ran.
    ///
    /// # Errors
    /// Returns [`MediaError::Metadata`] on a backend failure, or `Ok(false)` if no such row.
    pub fn update_content_type_and_length(
        &self,
        server_name: &str,
        media_id: &str,
        content_type: &str,
        byte_length: u64,
    ) -> Result<bool, MediaError> {
        let key = (server_name.to_string(), media_id.to_string());
        transact(&self.backend, TransactConfig::default(), |txn| {
            let Some(bytes) = self.media.get(txn, &key).map_err(to_kv_err)? else {
                return Ok(false);
            };
            let mut record: MediaRecord = serde_json::from_slice(&bytes)
                .map_err(|e| hs_kv::KvError::backend(DecodeError(e.to_string())))?;
            record.content_type = content_type.to_string();
            record.byte_length = Some(byte_length);
            let value = serde_json::to_vec(&record)
                .map_err(|e| hs_kv::KvError::backend(DecodeError(e.to_string())))?;
            self.media.put(txn, &key, &value).map_err(to_kv_err)?;
            Ok(true)
        })
        .map_err(|e| MediaError::Metadata(e.to_string()))
    }

    /// Sets or clears the quarantine flag on a media row.
    ///
    /// # Errors
    /// Returns [`MediaError::Metadata`] on a backend failure, or `Ok(false)` if no such row.
    pub fn set_quarantined(
        &self,
        server_name: &str,
        media_id: &str,
        by: Option<&str>,
    ) -> Result<bool, MediaError> {
        let key = (server_name.to_string(), media_id.to_string());
        transact(&self.backend, TransactConfig::default(), |txn| {
            let Some(bytes) = self.media.get(txn, &key).map_err(to_kv_err)? else {
                return Ok(false);
            };
            let mut record: MediaRecord = serde_json::from_slice(&bytes)
                .map_err(|e| hs_kv::KvError::backend(DecodeError(e.to_string())))?;
            record.quarantined_by = by.map(str::to_string);
            let value = serde_json::to_vec(&record)
                .map_err(|e| hs_kv::KvError::backend(DecodeError(e.to_string())))?;
            self.media.put(txn, &key, &value).map_err(to_kv_err)?;
            Ok(true)
        })
        .map_err(|e| MediaError::Metadata(e.to_string()))
    }

    /// Inserts or overwrites a thumbnail row.
    ///
    /// # Errors
    /// Returns [`MediaError::Metadata`] on a backend failure.
    pub fn put_thumbnail(
        &self,
        server_name: &str,
        media_id: &str,
        record: &ThumbnailRecord,
    ) -> Result<(), MediaError> {
        let variant = ThumbnailRecord::variant_key(
            record.width,
            record.height,
            record.method,
            &record.content_type,
        );
        let key = (server_name.to_string(), media_id.to_string(), variant);
        let value = serde_json::to_vec(record)
            .map_err(|e| MediaError::Metadata(format!("encoding thumbnail record: {e}")))?;
        transact(&self.backend, TransactConfig::default(), |txn| {
            self.thumbnails.put(txn, &key, &value).map_err(to_kv_err)
        })
        .map_err(|e| MediaError::Metadata(e.to_string()))
    }

    /// Reads one thumbnail's row, if it has already been generated.
    ///
    /// # Errors
    /// Returns [`MediaError::Metadata`] on a backend failure or an undecodable stored row.
    pub fn get_thumbnail(
        &self,
        server_name: &str,
        media_id: &str,
        width: u32,
        height: u32,
        method: ThumbnailMethod,
        content_type: &str,
    ) -> Result<Option<ThumbnailRecord>, MediaError> {
        let variant = ThumbnailRecord::variant_key(width, height, method, content_type);
        let key = (server_name.to_string(), media_id.to_string(), variant);
        let snapshot = self.backend.snapshot();
        let bytes = self
            .thumbnails
            .get(&snapshot, &key)
            .map_err(|e: TableError| MediaError::Metadata(e.to_string()))?;
        decode_optional(bytes)
    }

    /// Every thumbnail generated so far for one piece of media (for `/thumbnail` listing and for
    /// cleanup when the parent media is deleted/quarantined).
    ///
    /// # Errors
    /// Returns [`MediaError::Metadata`] on a backend failure.
    pub fn list_thumbnails(
        &self,
        server_name: &str,
        media_id: &str,
    ) -> Result<Vec<ThumbnailRecord>, MediaError> {
        let snapshot = self.backend.snapshot();
        let prefix = (server_name.to_string(), media_id.to_string());
        let spec: RangeSpec = TypedKeyspace::<B::Keyspace, ThumbKey>::prefix(&prefix);
        let mut out = Vec::new();
        for item in self.thumbnails.range(&snapshot, spec) {
            let (_key, value) = item.map_err(|e| MediaError::Metadata(e.to_string()))?;
            out.push(decode_value(&value)?);
        }
        Ok(out)
    }
}

fn to_kv_err(e: TableError) -> hs_kv::KvError {
    match e {
        TableError::Kv(kv) => kv,
        other => hs_kv::KvError::backend(DecodeError(other.to_string())),
    }
}

fn decode_optional<T: serde::de::DeserializeOwned>(
    bytes: Option<Bytes>,
) -> Result<Option<T>, MediaError> {
    match bytes {
        None => Ok(None),
        Some(b) => Ok(Some(decode_value(&b)?)),
    }
}

fn decode_value<T: serde::de::DeserializeOwned>(bytes: &[u8]) -> Result<T, MediaError> {
    serde_json::from_slice(bytes).map_err(|e| MediaError::Metadata(format!("decoding row: {e}")))
}

#[derive(Debug, thiserror::Error)]
#[error("{0}")]
struct DecodeError(String);

#[cfg(test)]
mod tests {
    use super::*;
    use hs_kv::memory::MemoryBackend;

    fn store() -> MetadataStore<MemoryBackend> {
        MetadataStore::open(MemoryBackend::new()).unwrap()
    }

    fn sample(media_id: &str) -> MediaRecord {
        MediaRecord {
            server_name: "example.org".into(),
            media_id: media_id.into(),
            content_type: "image/png".into(),
            upload_name: Some("cat.png".into()),
            byte_length: Some(1234),
            created_ms: 1000,
            uploader: Some("@alice:example.org".into()),
            completed: true,
            expires_at_ms: None,
            quarantined_by: None,
            safe_from_quarantine: false,
        }
    }

    #[test]
    fn round_trips_a_media_record() {
        let store = store();
        let rec = sample("abc123");
        store.put_media(&rec).unwrap();
        let got = store.get_media("example.org", "abc123").unwrap().unwrap();
        assert_eq!(got, rec);
    }

    #[test]
    fn missing_media_is_none() {
        let store = store();
        assert!(store.get_media("example.org", "nope").unwrap().is_none());
    }

    #[test]
    fn update_content_type_and_length_overwrites_only_those_fields() {
        let store = store();
        let rec = sample("replaced1");
        store.put_media(&rec).unwrap();

        let updated = store
            .update_content_type_and_length("example.org", "replaced1", "image/png", 42)
            .unwrap();
        assert!(updated);

        let got = store
            .get_media("example.org", "replaced1")
            .unwrap()
            .unwrap();
        assert_eq!(got.content_type, "image/png");
        assert_eq!(got.byte_length, Some(42));
        // Everything else (notably `completed` and `quarantined_by`) is untouched.
        assert_eq!(got.completed, rec.completed);
        assert_eq!(got.quarantined_by, rec.quarantined_by);
        assert_eq!(got.upload_name, rec.upload_name);
    }

    #[test]
    fn update_content_type_and_length_on_missing_row_returns_false() {
        let store = store();
        let updated = store
            .update_content_type_and_length("example.org", "nope", "image/png", 1)
            .unwrap();
        assert!(!updated);
    }

    #[test]
    fn complete_upload_updates_the_row_and_clears_expiry() {
        let store = store();
        let mut rec = sample("async1");
        rec.completed = false;
        rec.byte_length = None;
        rec.content_type = "application/octet-stream".into();
        rec.expires_at_ms = Some(999_999);
        store.put_media(&rec).unwrap();

        let updated = store
            .complete_upload("example.org", "async1", "image/jpeg", 55)
            .unwrap();
        assert!(updated);

        let got = store.get_media("example.org", "async1").unwrap().unwrap();
        assert!(got.completed);
        assert_eq!(got.byte_length, Some(55));
        assert_eq!(got.content_type, "image/jpeg");
        assert_eq!(got.expires_at_ms, None);
    }

    #[test]
    fn complete_upload_on_missing_row_returns_false() {
        let store = store();
        let updated = store
            .complete_upload("example.org", "nope", "image/png", 1)
            .unwrap();
        assert!(!updated);
    }

    #[test]
    fn quarantine_round_trips() {
        let store = store();
        store.put_media(&sample("q1")).unwrap();
        assert!(
            store
                .set_quarantined("example.org", "q1", Some("@admin:example.org"))
                .unwrap()
        );
        let got = store.get_media("example.org", "q1").unwrap().unwrap();
        assert_eq!(got.quarantined_by.as_deref(), Some("@admin:example.org"));
        assert!(!got.is_servable());
    }

    #[test]
    fn safe_from_quarantine_stays_servable() {
        let store = store();
        let mut rec = sample("q2");
        rec.safe_from_quarantine = true;
        store.put_media(&rec).unwrap();
        store
            .set_quarantined("example.org", "q2", Some("@admin:example.org"))
            .unwrap();
        let got = store.get_media("example.org", "q2").unwrap().unwrap();
        assert!(got.is_servable());
    }

    #[test]
    fn thumbnail_round_trips_and_lists_by_media() {
        let store = store();
        let rec = ThumbnailRecord {
            width: 96,
            height: 96,
            method: ThumbnailMethod::Crop,
            content_type: "image/png".into(),
            byte_length: 512,
            created_ms: 42,
        };
        store.put_thumbnail("example.org", "m1", &rec).unwrap();
        let got = store
            .get_thumbnail(
                "example.org",
                "m1",
                96,
                96,
                ThumbnailMethod::Crop,
                "image/png",
            )
            .unwrap()
            .unwrap();
        assert_eq!(got, rec);

        let listed = store.list_thumbnails("example.org", "m1").unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0], rec);
    }

    #[test]
    fn list_thumbnails_does_not_cross_media_ids() {
        let store = store();
        let rec = ThumbnailRecord {
            width: 32,
            height: 32,
            method: ThumbnailMethod::Crop,
            content_type: "image/png".into(),
            byte_length: 10,
            created_ms: 1,
        };
        store.put_thumbnail("example.org", "m1", &rec).unwrap();
        store.put_thumbnail("example.org", "m2", &rec).unwrap();
        assert_eq!(store.list_thumbnails("example.org", "m1").unwrap().len(), 1);
        assert_eq!(store.list_thumbnails("example.org", "m2").unwrap().len(), 1);
    }
}
