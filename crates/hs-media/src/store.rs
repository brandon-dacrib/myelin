//! The object-store backend: turns `hs_config::MediaStorageBackend` into an
//! `Arc<dyn object_store::ObjectStore>`, and the key layout media bytes are stored under.
//!
//! `object_store` already abstracts local filesystem and S3-compatible storage behind one trait
//! ([`object_store::ObjectStore`]), which is the whole point of `PLAN.md` D6 ("media lives on
//! object storage first"): [`crate::repository`] never branches on which backend is configured,
//! it just calls `put`/`get`/`get_range`/`delete` on whatever this module built. GCS and Azure
//! are declared in [`hs_config::MediaStorageBackend`] and gated behind this crate's `gcp`/`azure`
//! Cargo features (kept off by default to keep the dependency tree lean, per
//! `docs/workstreams/09-media.md`'s day-one scope); [`build`] returns
//! [`MediaError::Store`] for them today rather than silently misconfiguring something.

use std::sync::Arc;

use object_store::ObjectStore;
use object_store::local::LocalFileSystem;
use object_store::path::Path as ObjectPath;

use hs_config::media::MediaStorageBackend;

use crate::error::MediaError;
use crate::id::MediaId;

/// Builds the configured object store.
///
/// # Errors
/// Returns [`MediaError::Store`] if the backend's directory could not be created (`Local`), its
/// credentials/bucket configuration is invalid (`S3`), or the backend is not yet implemented
/// (`Gcs`, `Azure` — see the module docs).
pub fn build(config: &MediaStorageBackend) -> Result<Arc<dyn ObjectStore>, MediaError> {
    match config {
        MediaStorageBackend::Local { path } => {
            std::fs::create_dir_all(path)
                .map_err(|e| MediaError::Store(format!("creating {}: {e}", path.display())))?;
            let fs = LocalFileSystem::new_with_prefix(path)
                .map_err(|e| MediaError::Store(e.to_string()))?;
            Ok(Arc::new(fs))
        }
        MediaStorageBackend::S3 {
            bucket,
            region,
            endpoint,
            access_key_id,
            secret_access_key,
            ..
        } => {
            let mut builder = object_store::aws::AmazonS3Builder::new().with_bucket_name(bucket);
            if let Some(region) = region {
                builder = builder.with_region(region);
            }
            if let Some(endpoint) = endpoint {
                builder = builder.with_endpoint(endpoint).with_allow_http(true);
            }
            if let Some(key) = access_key_id {
                builder = builder.with_access_key_id(key);
            }
            if let Some(secret) = secret_access_key.as_str() {
                builder = builder.with_secret_access_key(secret);
            }
            let s3 = builder
                .build()
                .map_err(|e| MediaError::Store(e.to_string()))?;
            Ok(Arc::new(s3))
        }
        MediaStorageBackend::Gcs { .. } => Err(MediaError::Store(
            "GCS backend is not yet implemented (crate feature `gcp` is a placeholder)".into(),
        )),
        MediaStorageBackend::Azure { .. } => Err(MediaError::Store(
            "Azure backend is not yet implemented (crate feature `azure` is a placeholder)".into(),
        )),
    }
}

/// The object-store key for a piece of local or cached-remote media's original bytes:
/// `media/<server_name>/<b0>/<b1>/<rest>`, sharded on the media ID's first two characters like
/// Synapse's `filepath.py` (`local_content`/`remote_content`; behavior only, no code copied) so
/// no single directory ends up with millions of entries.
#[must_use]
pub fn content_key(server_name: &str, media_id: &MediaId) -> ObjectPath {
    let (b0, b1, rest) = shard(media_id.as_str());
    ObjectPath::from(format!("media/{server_name}/{b0}/{b1}/{rest}"))
}

/// The object-store key for one generated thumbnail variant.
#[must_use]
pub fn thumbnail_key(server_name: &str, media_id: &MediaId, variant: &str) -> ObjectPath {
    let (b0, b1, rest) = shard(media_id.as_str());
    ObjectPath::from(format!(
        "thumbnails/{server_name}/{b0}/{b1}/{rest}/{variant}"
    ))
}

/// Splits a media ID into `(first 2 chars, next 2 chars, remainder)`, falling back gracefully
/// for IDs shorter than 4 characters (never produced by [`MediaId::generate`], but a remote
/// server's imported ID is out of this server's control).
fn shard(media_id: &str) -> (String, String, String) {
    let chars: Vec<char> = media_id.chars().collect();
    let b0: String = chars.iter().take(2).collect();
    let b1: String = chars.iter().skip(2).take(2).collect();
    let rest: String = chars.iter().skip(4).collect();
    let b0 = if b0.is_empty() { "_".to_string() } else { b0 };
    let b1 = if b1.is_empty() { "_".to_string() } else { b1 };
    let rest = if rest.is_empty() {
        "_".to_string()
    } else {
        rest
    };
    (b0, b1, rest)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_backend_creates_its_directory() {
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("media-store");
        let cfg = MediaStorageBackend::Local { path: sub.clone() };
        let _store = build(&cfg).unwrap();
        assert!(sub.exists());
    }

    #[test]
    fn s3_backend_requires_no_network_to_construct() {
        // Building the client is purely local config validation; no network call happens until a
        // request is actually made, so this must succeed (or fail on bad config) without
        // credentials or connectivity.
        let cfg = MediaStorageBackend::S3 {
            bucket: "test-bucket".into(),
            region: Some("us-east-1".into()),
            endpoint: Some("http://localhost:9000".into()),
            access_key_id: Some("minioadmin".into()),
            secret_access_key: "minioadmin".to_string().into(),
            secret_access_key_file: None,
        };
        assert!(build(&cfg).is_ok());
    }

    #[test]
    fn gcs_backend_is_a_clean_not_implemented_error() {
        let cfg = MediaStorageBackend::Gcs {
            bucket: "b".into(),
            service_account_key_file: None,
        };
        assert!(build(&cfg).is_err());
    }

    #[test]
    fn content_key_shards_on_first_four_characters() {
        let id = MediaId::parse("AbCdEfGhIjKlMnOpQrStUvWx").unwrap();
        let key = content_key("example.org", &id);
        assert_eq!(
            key.to_string(),
            "media/example.org/Ab/Cd/EfGhIjKlMnOpQrStUvWx"
        );
    }

    #[test]
    fn content_key_differs_by_server_name() {
        let id = MediaId::generate();
        let a = content_key("a.example.org", &id);
        let b = content_key("b.example.org", &id);
        assert_ne!(a, b);
    }

    #[test]
    fn thumbnail_key_includes_the_variant() {
        let id = MediaId::parse("AbCdEfGhIjKlMnOpQrStUvWx").unwrap();
        let key = thumbnail_key("example.org", &id, "96x96-crop.png");
        assert!(key.to_string().ends_with("96x96-crop.png"));
    }

    #[test]
    fn shard_handles_short_ids_without_panicking() {
        let (b0, b1, rest) = shard("a");
        assert_eq!(b0, "a");
        assert_eq!(b1, "_");
        assert_eq!(rest, "_");
    }
}
