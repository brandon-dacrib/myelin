//! Who has uploaded what: this server's own uploads, read for the admin API's statistics
//! (per-user media usage, and uploads over time).
//!
//! A read of [`crate::metadata::MetadataStore`]'s media table, kept in this crate because the
//! table's layout is this crate's: `(server_name, media_id)` keys under `hs_media.media`, so
//! this server's own uploads are one prefix scan.

use hs_kv::KvBackend;
use hs_tables::keyspace::TypedKeyspace;

use crate::error::MediaError;
use crate::metadata::MediaRecord;

/// One local upload, as the statistics need it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Upload {
    /// Who uploaded it.
    pub uploader: String,
    /// Its size in bytes.
    pub byte_length: u64,
    /// When it was uploaded, milliseconds since the Unix epoch.
    pub created_ms: u64,
}

/// Every completed upload to this server (`server_name`) that has an uploader, in no particular
/// order. Remote media cached here, and async-upload reservations never filled, are left out.
///
/// # Errors
/// [`MediaError::Metadata`] on a backend failure or an undecodable row.
pub fn local_uploads<B: KvBackend>(
    backend: &B,
    server_name: &str,
) -> Result<Vec<Upload>, MediaError> {
    let media: TypedKeyspace<B::Keyspace, (String, String)> = TypedKeyspace::new(
        backend
            .keyspace("hs_media.media")
            .map_err(|e| MediaError::Metadata(e.to_string()))?,
    );
    let snapshot = backend.snapshot();
    let spec = TypedKeyspace::<B::Keyspace, (String, String)>::prefix(&(server_name.to_owned(),));
    let mut out = Vec::new();
    for item in media.range(&snapshot, spec) {
        let (_key, value) = item.map_err(|e| MediaError::Metadata(e.to_string()))?;
        let record: MediaRecord = serde_json::from_slice(&value)
            .map_err(|e| MediaError::Metadata(format!("decoding a media record: {e}")))?;
        if !record.completed {
            continue;
        }
        let Some(uploader) = record.uploader else {
            continue;
        };
        out.push(Upload {
            uploader,
            byte_length: record.byte_length.unwrap_or(0),
            created_ms: record.created_ms,
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use hs_kv::memory::MemoryBackend;

    use super::*;
    use crate::metadata::MetadataStore;

    fn record(server: &str, id: &str, uploader: Option<&str>, completed: bool) -> MediaRecord {
        MediaRecord {
            server_name: server.to_owned(),
            media_id: id.to_owned(),
            content_type: "image/png".to_owned(),
            upload_name: None,
            byte_length: Some(10),
            created_ms: 1_000,
            uploader: uploader.map(str::to_owned),
            completed,
            expires_at_ms: None,
            quarantined_by: None,
            safe_from_quarantine: false,
        }
    }

    #[test]
    fn only_this_servers_completed_uploads_are_counted() {
        let backend = MemoryBackend::new();
        let store = MetadataStore::open(backend.clone()).unwrap();
        store
            .put_media(&record(
                "example.org",
                "a",
                Some("@alice:example.org"),
                true,
            ))
            .unwrap();
        store
            .put_media(&record(
                "example.org",
                "b",
                Some("@alice:example.org"),
                false,
            ))
            .unwrap();
        store
            .put_media(&record(
                "example.org.evil",
                "c",
                Some("@x:example.org.evil"),
                true,
            ))
            .unwrap();
        store
            .put_media(&record("remote.example", "d", None, true))
            .unwrap();
        let uploads = local_uploads(&backend, "example.org").unwrap();
        assert_eq!(
            uploads,
            [Upload {
                uploader: "@alice:example.org".to_owned(),
                byte_length: 10,
                created_ms: 1_000
            }]
        );
    }
}
