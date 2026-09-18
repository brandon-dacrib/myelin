//! A read-only adapter over Synapse's on-disk media-directory layout, for track 13's importer.
//!
//! Behavior reference: `refs/synapse/synapse/media/filepath.py` (`MediaFilePaths`, AGPL-3.0,
//! read for behavior only — the logic below is a small, independent Rust reimplementation of the
//! same path scheme, not a translation of Synapse's source). Six subtrees, each media ID sharded
//! by its first two, then next two, characters (so no directory holds more than a few thousand
//! entries):
//!
//! | Subtree | Path shape |
//! |---|---|
//! | `local_content` | `local_content/<b0>/<b1>/<rest>` (file: `rest` is the content bytes) |
//! | `remote_content` | `remote_content/<server>/<b0>/<b1>/<rest>` |
//! | `local_thumbnails` | `local_thumbnails/<b0>/<b1>/<rest>/<w>-<h>-<type>-<subtype>-<method>` (`rest` is a directory of thumbnail files) |
//! | `remote_thumbnail` | `remote_thumbnail/<server>/<b0>/<b1>/<rest>/<w>-<h>-<type>-<subtype>[-<method>]` (the trailing `-<method>` is absent in files written by Synapse's legacy naming, still read here) |
//! | `url_cache` | `url_cache/<b0>/<b1>/<rest>` (legacy) or `url_cache/<YYYY-MM-DD>/<rest>` (Synapse's "new format" media IDs, which embed their creation date) |
//! | `url_cache_thumbnails` | same split as `url_cache`, with a trailing thumbnail filename component |
//!
//! This module only *reads*: [`SynapseMediaStore::iter_entries`] walks the tree and classifies
//! each file it finds via [`classify_relative_path`] (itself pure and filesystem-free, so it is
//! unit-tested directly against synthetic paths without needing real files on disk), and
//! [`SynapseMediaStore::read_content`] opens one for reading. Nothing here creates, deletes or
//! modifies a file — this crate is not the writer of a Synapse-shaped tree, only a source the
//! importer reads from. The policy for what the importer does with what this module finds
//! (conflict handling, ID remapping, what counts as already-imported) is track 13's
//! `docs/compat/synapse-importer-mapping.md`, not this module's concern.

use std::path::{Path, PathBuf};

use crate::error::MediaError;

/// Which of the six subtrees an entry came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SynapseMediaKind {
    /// `local_content/`
    LocalContent,
    /// `remote_content/<server>/`
    RemoteContent,
    /// `local_thumbnails/`
    LocalThumbnail,
    /// `remote_thumbnail/<server>/`
    RemoteThumbnail,
    /// `url_cache/`
    UrlCache,
    /// `url_cache_thumbnails/`
    UrlCacheThumbnail,
}

/// A thumbnail file's parsed name: `<width>-<height>-<type>/<subtype>[-<method>]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThumbnailInfo {
    /// Target width, pixels.
    pub width: u32,
    /// Target height, pixels.
    pub height: u32,
    /// The thumbnail's own `Content-Type` (`<type>/<subtype>` reassembled from the two filename
    /// components Synapse splits it into).
    pub content_type: String,
    /// `crop` or `scale`, if present. Synapse's legacy remote-thumbnail naming
    /// (`remote_media_thumbnail_rel_legacy`) omits this; such files are still read, with `method`
    /// `None`, since a real Synapse deployment may have a mix of both on disk.
    pub method: Option<String>,
}

/// One classified file found under a Synapse media store root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SynapseMediaEntry {
    /// Which subtree this came from.
    pub kind: SynapseMediaKind,
    /// The origin server, for `RemoteContent`/`RemoteThumbnail` entries.
    pub server_name: Option<String>,
    /// The reconstructed media ID (the sharding directories rejoined with the leaf component).
    pub media_id: String,
    /// Present for `LocalThumbnail`/`RemoteThumbnail`/`UrlCacheThumbnail` entries.
    pub thumbnail: Option<ThumbnailInfo>,
    /// Path to the actual file, relative to the media store root.
    pub relative_path: PathBuf,
}

/// Parses a thumbnail filename (`<width>-<height>-<type>-<subtype>[-<method>]`) as Synapse writes
/// it (`local_media_thumbnail_rel`/`remote_media_thumbnail_rel`; the trailing `-<method>` is
/// absent for `remote_media_thumbnail_rel_legacy` files).
///
/// # Errors
/// Returns [`MediaError::InvalidInput`] if `name` does not have at least the four required
/// hyphen-separated components, or the first two are not valid `u32`s.
fn parse_thumbnail_filename(name: &str) -> Result<ThumbnailInfo, MediaError> {
    let parts: Vec<&str> = name.splitn(5, '-').collect();
    if parts.len() < 4 {
        return Err(MediaError::InvalidInput(format!(
            "not a recognized thumbnail filename: {name:?}"
        )));
    }
    let width: u32 = parts[0]
        .parse()
        .map_err(|_| MediaError::InvalidInput(format!("bad width in {name:?}")))?;
    let height: u32 = parts[1]
        .parse()
        .map_err(|_| MediaError::InvalidInput(format!("bad height in {name:?}")))?;
    let content_type = format!("{}/{}", parts[2], parts[3]);
    let method = parts.get(4).map(|s| (*s).to_string());
    Ok(ThumbnailInfo {
        width,
        height,
        content_type,
        method,
    })
}

/// Whether a Synapse media ID is the "new format" (`<YYYY-MM-DD>-<random>`, used only for URL
/// preview cache entries, which embed their creation date so a whole day's cache can be pruned by
/// deleting one directory).
fn is_dated_media_id(media_id: &str) -> bool {
    let bytes = media_id.as_bytes();
    bytes.len() >= 10
        && bytes[0..4].iter().all(u8::is_ascii_digit)
        && bytes[4] == b'-'
        && bytes[5..7].iter().all(u8::is_ascii_digit)
        && bytes[7] == b'-'
        && bytes[8..10].iter().all(u8::is_ascii_digit)
}

/// Classifies one file's path, relative to a Synapse media store root, into a
/// [`SynapseMediaEntry`]. Pure and filesystem-free.
///
/// # Errors
/// Returns [`MediaError::InvalidInput`] if the path does not match any of the six known shapes,
/// or a thumbnail filename within it could not be parsed.
pub fn classify_relative_path(rel_path: &Path) -> Result<SynapseMediaEntry, MediaError> {
    let components: Vec<String> = rel_path
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect();
    let unrecognized = || {
        MediaError::InvalidInput(format!(
            "path does not match a known Synapse media layout shape: {}",
            rel_path.display()
        ))
    };
    let Some(root) = components.first() else {
        return Err(unrecognized());
    };

    match root.as_str() {
        "local_content" if components.len() == 4 => Ok(SynapseMediaEntry {
            kind: SynapseMediaKind::LocalContent,
            server_name: None,
            media_id: format!("{}{}{}", components[1], components[2], components[3]),
            thumbnail: None,
            relative_path: rel_path.to_path_buf(),
        }),
        "remote_content" if components.len() == 5 => Ok(SynapseMediaEntry {
            kind: SynapseMediaKind::RemoteContent,
            server_name: Some(components[1].clone()),
            media_id: format!("{}{}{}", components[2], components[3], components[4]),
            thumbnail: None,
            relative_path: rel_path.to_path_buf(),
        }),
        "local_thumbnails" if components.len() == 5 => Ok(SynapseMediaEntry {
            kind: SynapseMediaKind::LocalThumbnail,
            server_name: None,
            media_id: format!("{}{}{}", components[1], components[2], components[3]),
            thumbnail: Some(parse_thumbnail_filename(&components[4])?),
            relative_path: rel_path.to_path_buf(),
        }),
        "remote_thumbnail" if components.len() == 6 => Ok(SynapseMediaEntry {
            kind: SynapseMediaKind::RemoteThumbnail,
            server_name: Some(components[1].clone()),
            media_id: format!("{}{}{}", components[2], components[3], components[4]),
            thumbnail: Some(parse_thumbnail_filename(&components[5])?),
            relative_path: rel_path.to_path_buf(),
        }),
        "url_cache" if components.len() == 3 && is_dated_media_id_prefix(&components[1]) => {
            Ok(SynapseMediaEntry {
                kind: SynapseMediaKind::UrlCache,
                server_name: None,
                media_id: format!("{}-{}", components[1], components[2]),
                thumbnail: None,
                relative_path: rel_path.to_path_buf(),
            })
        }
        "url_cache" if components.len() == 4 => Ok(SynapseMediaEntry {
            kind: SynapseMediaKind::UrlCache,
            server_name: None,
            media_id: format!("{}{}{}", components[1], components[2], components[3]),
            thumbnail: None,
            relative_path: rel_path.to_path_buf(),
        }),
        "url_cache_thumbnails"
            if components.len() == 4 && is_dated_media_id_prefix(&components[1]) =>
        {
            Ok(SynapseMediaEntry {
                kind: SynapseMediaKind::UrlCacheThumbnail,
                server_name: None,
                media_id: format!("{}-{}", components[1], components[2]),
                thumbnail: Some(parse_thumbnail_filename(&components[3])?),
                relative_path: rel_path.to_path_buf(),
            })
        }
        "url_cache_thumbnails" if components.len() == 5 => Ok(SynapseMediaEntry {
            kind: SynapseMediaKind::UrlCacheThumbnail,
            server_name: None,
            media_id: format!("{}{}{}", components[1], components[2], components[3]),
            thumbnail: Some(parse_thumbnail_filename(&components[4])?),
            relative_path: rel_path.to_path_buf(),
        }),
        _ => Err(unrecognized()),
    }
}

/// Whether `s` alone looks like the `<YYYY-MM-DD>` directory component the dated URL-cache layout
/// uses (the full check, [`is_dated_media_id`], also allows for the joined `<date>-<random>`
/// form; this one only needs to recognize the bare date directory name).
fn is_dated_media_id_prefix(s: &str) -> bool {
    is_dated_media_id(&format!("{s}-x"))
}

/// A read-only view over a Synapse media store directory on local disk.
pub struct SynapseMediaStore {
    root: PathBuf,
}

impl SynapseMediaStore {
    /// Opens a Synapse media store rooted at `root`. Does not touch the filesystem yet (no
    /// existence check — [`SynapseMediaStore::iter_entries`] reports that naturally).
    #[must_use]
    pub fn open(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// The root directory this store reads from.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Walks every file under `root` and classifies it, skipping (not erroring on) any file whose
    /// path does not match a known shape — a Synapse media store in the wild may contain stray
    /// files (`.DS_Store`, lock files, a partially written upload) the importer should ignore
    /// rather than abort on.
    ///
    /// # Errors
    /// Returns [`MediaError::Store`] if `root` cannot be read at all (missing, permissions).
    pub fn iter_entries(&self) -> Result<Vec<SynapseMediaEntry>, MediaError> {
        let mut out = Vec::new();
        walk(&self.root, &self.root, &mut out)?;
        Ok(out)
    }

    /// Reads one entry's file contents.
    ///
    /// # Errors
    /// Returns [`MediaError::Store`] on any I/O failure.
    pub fn read_content(&self, entry: &SynapseMediaEntry) -> Result<Vec<u8>, MediaError> {
        std::fs::read(self.root.join(&entry.relative_path)).map_err(|e| {
            MediaError::Store(format!("reading {}: {e}", entry.relative_path.display()))
        })
    }
}

fn walk(root: &Path, dir: &Path, out: &mut Vec<SynapseMediaEntry>) -> Result<(), MediaError> {
    let read_dir = match std::fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound && dir == root => {
            return Err(MediaError::Store(format!(
                "media store root {} does not exist",
                root.display()
            )));
        }
        Err(e) => return Err(MediaError::Store(format!("reading {}: {e}", dir.display()))),
    };
    for entry in read_dir {
        let entry = entry.map_err(|e| MediaError::Store(e.to_string()))?;
        let path = entry.path();
        let file_type = entry
            .file_type()
            .map_err(|e| MediaError::Store(e.to_string()))?;
        if file_type.is_dir() {
            walk(root, &path, out)?;
        } else if file_type.is_file() {
            let Ok(rel) = path.strip_prefix(root) else {
                continue;
            };
            if let Ok(classified) = classify_relative_path(rel) {
                out.push(classified);
            }
            // Unrecognized files are silently skipped -- see `iter_entries`'s doc comment.
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_local_content() {
        let entry = classify_relative_path(Path::new("local_content/Ab/Cd/EfGh")).unwrap();
        assert_eq!(entry.kind, SynapseMediaKind::LocalContent);
        assert_eq!(entry.media_id, "AbCdEfGh");
        assert_eq!(entry.server_name, None);
        assert!(entry.thumbnail.is_none());
    }

    #[test]
    fn classifies_remote_content() {
        let entry =
            classify_relative_path(Path::new("remote_content/matrix.org/Ab/Cd/EfGh")).unwrap();
        assert_eq!(entry.kind, SynapseMediaKind::RemoteContent);
        assert_eq!(entry.server_name.as_deref(), Some("matrix.org"));
        assert_eq!(entry.media_id, "AbCdEfGh");
    }

    #[test]
    fn classifies_local_thumbnail_with_method() {
        let entry = classify_relative_path(Path::new(
            "local_thumbnails/Ab/Cd/EfGh/96-96-image-png-crop",
        ))
        .unwrap();
        assert_eq!(entry.kind, SynapseMediaKind::LocalThumbnail);
        assert_eq!(entry.media_id, "AbCdEfGh");
        let thumb = entry.thumbnail.unwrap();
        assert_eq!(thumb.width, 96);
        assert_eq!(thumb.height, 96);
        assert_eq!(thumb.content_type, "image/png");
        assert_eq!(thumb.method.as_deref(), Some("crop"));
    }

    #[test]
    fn classifies_remote_thumbnail_legacy_without_method() {
        let entry = classify_relative_path(Path::new(
            "remote_thumbnail/matrix.org/Ab/Cd/EfGh/320-240-image-jpeg",
        ))
        .unwrap();
        assert_eq!(entry.kind, SynapseMediaKind::RemoteThumbnail);
        let thumb = entry.thumbnail.unwrap();
        assert_eq!(thumb.content_type, "image/jpeg");
        assert_eq!(thumb.method, None);
    }

    #[test]
    fn classifies_dated_url_cache() {
        let entry =
            classify_relative_path(Path::new("url_cache/2020-01-15/fsdRDt24DS234dsf")).unwrap();
        assert_eq!(entry.kind, SynapseMediaKind::UrlCache);
        assert_eq!(entry.media_id, "2020-01-15-fsdRDt24DS234dsf");
    }

    #[test]
    fn classifies_legacy_url_cache() {
        let entry = classify_relative_path(Path::new("url_cache/Ab/Cd/EfGh")).unwrap();
        assert_eq!(entry.kind, SynapseMediaKind::UrlCache);
        assert_eq!(entry.media_id, "AbCdEfGh");
    }

    #[test]
    fn classifies_dated_url_cache_thumbnail() {
        let entry = classify_relative_path(Path::new(
            "url_cache_thumbnails/2020-01-15/fsdRDt24DS234dsf/800-600-image-png-scale",
        ))
        .unwrap();
        assert_eq!(entry.kind, SynapseMediaKind::UrlCacheThumbnail);
        assert_eq!(entry.media_id, "2020-01-15-fsdRDt24DS234dsf");
        assert_eq!(entry.thumbnail.unwrap().width, 800);
    }

    #[test]
    fn unrecognized_shapes_are_rejected_not_panicking() {
        assert!(classify_relative_path(Path::new("")).is_err());
        assert!(classify_relative_path(Path::new("not_a_real_subtree/x/y/z")).is_err());
        assert!(classify_relative_path(Path::new("local_content/only/two")).is_err());
    }

    #[test]
    fn thumbnail_filename_parsing_rejects_garbage() {
        assert!(parse_thumbnail_filename("not-enough-parts").is_err());
        assert!(parse_thumbnail_filename("abc-96-image-png-crop").is_err());
    }

    #[test]
    fn store_walks_a_real_directory_tree_read_only() {
        let dir = tempfile::tempdir().unwrap();
        let content_dir = dir.path().join("local_content/Ab/Cd");
        std::fs::create_dir_all(&content_dir).unwrap();
        std::fs::write(content_dir.join("EfGh"), b"hello world").unwrap();
        let thumb_dir = dir.path().join("local_thumbnails/Ab/Cd/EfGh");
        std::fs::create_dir_all(&thumb_dir).unwrap();
        std::fs::write(thumb_dir.join("96-96-image-png-crop"), b"thumb bytes").unwrap();
        // A stray file the importer should silently ignore.
        std::fs::write(dir.path().join(".DS_Store"), b"").unwrap();

        let store = SynapseMediaStore::open(dir.path());
        let mut entries = store.iter_entries().unwrap();
        entries.sort_by(|a, b| a.relative_path.cmp(&b.relative_path));
        assert_eq!(entries.len(), 2);

        let content_entry = entries
            .iter()
            .find(|e| e.kind == SynapseMediaKind::LocalContent)
            .unwrap();
        assert_eq!(content_entry.media_id, "AbCdEfGh");
        assert_eq!(store.read_content(content_entry).unwrap(), b"hello world");

        let thumb_entry = entries
            .iter()
            .find(|e| e.kind == SynapseMediaKind::LocalThumbnail)
            .unwrap();
        assert_eq!(store.read_content(thumb_entry).unwrap(), b"thumb bytes");
    }

    #[test]
    fn missing_root_is_a_clean_error_not_a_panic() {
        let store = SynapseMediaStore::open("/definitely/not/a/real/path/xyz-hs-media-test");
        assert!(store.iter_entries().is_err());
    }
}
