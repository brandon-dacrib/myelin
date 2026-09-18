//! HTTP handlers: the authenticated `client/v1/media` routes and the legacy `media/v3` routes
//! (mounted behind a config flag by [`crate::router`], with freeze semantics — see
//! [`legacy`]'s module doc).

pub mod config;
pub mod download;
pub mod legacy;
pub mod thumbnail;
pub mod upload;

use crate::error::MediaError;
use crate::id::MediaId;

/// Builds an `mxc://` URI (spec: "Matrix Content (MXC) URIs").
pub(crate) fn mxc_uri(server_name: &str, media_id: &str) -> String {
    format!("mxc://{server_name}/{media_id}")
}

pub(crate) fn parse_media_id(raw: &str) -> Result<MediaId, MediaError> {
    MediaId::parse(raw).map_err(|e| MediaError::InvalidInput(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mxc_uri_shape() {
        assert_eq!(mxc_uri("example.org", "abc123"), "mxc://example.org/abc123");
    }
}
