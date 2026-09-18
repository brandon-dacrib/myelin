//! Decode-time safety: the [`security`](crate::security) module's Rule 3 (decompression bombs)
//! and Rule 4 (content-type sniffing is never fed back into what is served) both live here.
//!
//! Two separate things happen when this crate decodes an uploaded image (always for
//! thumbnailing; never merely to "validate" an upload the spec says must be accepted regardless
//! of content):
//!
//! 1. **Format detection** ([`sniff_format`]): magic-byte detection of what the file actually is,
//!    used only to pick a decoder and, in [`format_matches_declared_type`], to decide whether
//!    thumbnailing is even possible — **never** to change the `Content-Type` this crate serves
//!    back (that stays exactly what the uploader declared; see `crate::security`'s Rule 4).
//! 2. **Bounded decoding** ([`decode_with_limits`]): the decoder is given explicit width, height
//!    and allocation ceilings *before* it is asked to produce pixel data, so a small file that
//!    declares an enormous image (a decompression bomb — PNG and GIF in particular can express a
//!    multi-gigapixel image in a few hundred compressed bytes) is rejected the instant its header
//!    is parsed, never during an attempted multi-gigabyte allocation.

use std::io::Cursor;

use image::{DynamicImage, ImageDecoder, ImageFormat, ImageReader};

use crate::error::MediaError;

/// Decoding ceilings passed to every decoder this crate constructs. The defaults are generous
/// for a legitimate photo (up to roughly a 64-megapixel image, comparable to a 40MP camera RAW
/// converted to a bitmap) while still bounding worst-case memory: `64_000_000 px * 4 bytes/px`
/// (RGBA8) is 256 MiB, which is also this struct's default `max_alloc_bytes`.
#[derive(Debug, Clone, Copy)]
pub struct DecodeLimits {
    /// Maximum width, pixels.
    pub max_width: u32,
    /// Maximum height, pixels.
    pub max_height: u32,
    /// Maximum total allocation the decoder may perform, bytes.
    pub max_alloc_bytes: u64,
}

impl Default for DecodeLimits {
    fn default() -> Self {
        Self {
            max_width: 10_000,
            max_height: 10_000,
            max_alloc_bytes: 256 * 1024 * 1024,
        }
    }
}

impl DecodeLimits {
    fn to_image_limits(self) -> image::Limits {
        let mut limits = image::Limits::default();
        limits.max_image_width = Some(self.max_width);
        limits.max_image_height = Some(self.max_height);
        limits.max_alloc = Some(self.max_alloc_bytes);
        limits
    }
}

/// Detects the actual image format from magic bytes, independent of any `Content-Type` claim.
/// Returns `None` if the bytes do not match any format the `image` crate (with this workspace's
/// enabled codecs: JPEG, PNG, GIF, WebP) recognizes.
#[must_use]
pub fn sniff_format(bytes: &[u8]) -> Option<ImageFormat> {
    image::guess_format(bytes).ok()
}

/// Whether `declared_content_type`'s implied format matches the sniffed format of `bytes`. Used
/// only to decide whether this crate can/should attempt to thumbnail content (a PNG claimed to be
/// a JPEG will fail to thumbnail either way, but detecting the mismatch up front gives a cleaner
/// error than a decoder failure) — **never** to override what `Content-Type` is served on
/// download (see the module docs and `crate::security`'s Rule 4).
#[must_use]
pub fn format_matches_declared_type(declared_content_type: &str, bytes: &[u8]) -> bool {
    let Some(expected) = format_for_mime(declared_content_type) else {
        return false;
    };
    sniff_format(bytes) == Some(expected)
}

/// Maps a MIME type to the `image` crate's [`ImageFormat`], for the four formats this workspace
/// builds `image` with (see the workspace `Cargo.toml`'s `image` feature list).
#[must_use]
pub fn format_for_mime(mime: &str) -> Option<ImageFormat> {
    match mime
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase()
        .as_str()
    {
        "image/jpeg" | "image/jpg" => Some(ImageFormat::Jpeg),
        "image/png" | "image/apng" => Some(ImageFormat::Png),
        "image/gif" => Some(ImageFormat::Gif),
        "image/webp" => Some(ImageFormat::WebP),
        _ => None,
    }
}

/// The canonical MIME type for an [`ImageFormat`] this crate thumbnails to.
#[must_use]
pub fn mime_for_format(format: ImageFormat) -> &'static str {
    match format {
        ImageFormat::Jpeg => "image/jpeg",
        ImageFormat::Png => "image/png",
        ImageFormat::Gif => "image/gif",
        ImageFormat::WebP => "image/webp",
        _ => "application/octet-stream",
    }
}

/// Decodes `bytes` into a [`DynamicImage`], enforcing `limits` before any pixel data is
/// materialized. For an animated format (GIF; WebP animation is not decoded by this workspace's
/// `image` build), this yields only the first frame — this crate never thumbnails past frame
/// zero, matching Synapse's observed behavior of always producing static thumbnails.
///
/// # Errors
/// Returns [`MediaError::DecodeFailed`] if the format is unrecognized, the header cannot be
/// parsed, or the declared dimensions/allocation exceed `limits` — this last case is exactly the
/// decompression-bomb defense: the error fires from the header check, before any large buffer is
/// allocated.
pub fn decode_with_limits(
    bytes: &[u8],
    limits: DecodeLimits,
) -> Result<(DynamicImage, ImageFormat), MediaError> {
    let reader = ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|e| MediaError::DecodeFailed(e.to_string()))?;
    let format = reader
        .format()
        .ok_or_else(|| MediaError::DecodeFailed("unrecognized image format".into()))?;
    let mut decoder = reader
        .into_decoder()
        .map_err(|e| MediaError::DecodeFailed(e.to_string()))?;
    decoder
        .set_limits(limits.to_image_limits())
        .map_err(|e| MediaError::DecodeFailed(format!("rejected by decode limits: {e}")))?;
    let image =
        DynamicImage::from_decoder(decoder).map_err(|e| MediaError::DecodeFailed(e.to_string()))?;
    Ok((image, format))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tiny_png() -> Vec<u8> {
        let img = DynamicImage::new_rgb8(4, 4);
        let mut out = Vec::new();
        img.write_to(&mut Cursor::new(&mut out), ImageFormat::Png)
            .unwrap();
        out
    }

    fn tiny_jpeg() -> Vec<u8> {
        let img = DynamicImage::new_rgb8(4, 4);
        let mut out = Vec::new();
        img.write_to(&mut Cursor::new(&mut out), ImageFormat::Jpeg)
            .unwrap();
        out
    }

    #[test]
    fn sniff_detects_png_from_magic_bytes() {
        assert_eq!(sniff_format(&tiny_png()), Some(ImageFormat::Png));
    }

    #[test]
    fn sniff_detects_jpeg_from_magic_bytes() {
        assert_eq!(sniff_format(&tiny_jpeg()), Some(ImageFormat::Jpeg));
    }

    #[test]
    fn sniff_returns_none_for_garbage() {
        assert_eq!(sniff_format(b"not an image, just text"), None);
    }

    #[test]
    fn sniff_returns_none_for_svg_text() {
        // SVG is XML text, not a binary image format `image` recognizes -- confirms this crate
        // cannot be tricked into treating an SVG as a "sniffed" raster format.
        assert_eq!(
            sniff_format(b"<svg xmlns='http://www.w3.org/2000/svg'></svg>"),
            None
        );
    }

    #[test]
    fn declared_type_matches_when_bytes_agree() {
        assert!(format_matches_declared_type("image/png", &tiny_png()));
    }

    #[test]
    fn declared_type_mismatch_is_detected() {
        // A PNG's bytes uploaded with a claimed image/jpeg content type.
        assert!(!format_matches_declared_type("image/jpeg", &tiny_png()));
    }

    #[test]
    fn declared_type_mismatch_when_claim_is_not_even_an_image_format() {
        assert!(!format_matches_declared_type("text/html", &tiny_png()));
    }

    #[test]
    fn decode_with_limits_succeeds_for_a_small_image() {
        let (img, format) = decode_with_limits(&tiny_png(), DecodeLimits::default()).unwrap();
        assert_eq!(format, ImageFormat::Png);
        assert_eq!(img.width(), 4);
        assert_eq!(img.height(), 4);
    }

    #[test]
    #[cfg(feature = "test-fixtures")]
    fn decode_with_limits_rejects_a_declared_dimension_bomb() {
        // A hand-crafted PNG whose IHDR declares an enormous image but carries no real pixel
        // data: the point of this test is that rejection happens from the *header*, without the
        // decoder ever trying to allocate gigabytes of pixel buffer.
        let bomb = crate::test_fixtures::decompression_bomb_png(60_000, 60_000);
        let limits = DecodeLimits {
            max_width: 10_000,
            max_height: 10_000,
            max_alloc_bytes: 256 * 1024 * 1024,
        };
        let err = decode_with_limits(&bomb, limits).unwrap_err();
        assert!(matches!(err, MediaError::DecodeFailed(_)));
    }

    #[test]
    fn decode_with_limits_rejects_truncated_input() {
        let mut png = tiny_png();
        png.truncate(png.len() / 2);
        assert!(decode_with_limits(&png, DecodeLimits::default()).is_err());
    }

    #[test]
    fn decode_with_limits_rejects_empty_input() {
        assert!(decode_with_limits(&[], DecodeLimits::default()).is_err());
    }

    #[test]
    fn mime_round_trips_for_all_supported_formats() {
        for (mime, format) in [
            ("image/png", ImageFormat::Png),
            ("image/jpeg", ImageFormat::Jpeg),
            ("image/gif", ImageFormat::Gif),
            ("image/webp", ImageFormat::WebP),
        ] {
            assert_eq!(format_for_mime(mime), Some(format));
            assert_eq!(mime_for_format(format), mime);
        }
    }
}
