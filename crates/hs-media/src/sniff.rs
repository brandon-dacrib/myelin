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

/// Decoding ceilings passed to every decoder this crate constructs, normally built from the
/// `media` settings in force ([`DecodeLimits::from_config`]: `max_image_pixels`,
/// `max_image_dimension`, `max_image_decode_memory`). The defaults are those settings' defaults:
/// Synapse's `max_image_pixels` of `32M` (33,554,432 pixels), 32,768 pixels on either side, and
/// 256 MiB of decoded pixels (a 32-megapixel RGBA8 image is 128 MiB, a 16-bit one 256 MiB).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DecodeLimits {
    /// Maximum width, pixels.
    pub max_width: u32,
    /// Maximum height, pixels.
    pub max_height: u32,
    /// Maximum width times height, pixels.
    pub max_pixels: u64,
    /// Maximum total allocation the decoder may perform, bytes.
    pub max_alloc_bytes: u64,
}

impl Default for DecodeLimits {
    fn default() -> Self {
        Self::from_config(&hs_config::media::MediaConfig::default())
    }
}

impl DecodeLimits {
    /// The limits a `media` configuration sets.
    #[must_use]
    pub fn from_config(config: &hs_config::media::MediaConfig) -> Self {
        Self {
            max_width: config.max_image_dimension,
            max_height: config.max_image_dimension,
            max_pixels: config.max_image_pixels,
            max_alloc_bytes: config.max_image_decode_memory.as_u64(),
        }
    }

    fn to_image_limits(self) -> image::Limits {
        let mut limits = image::Limits::default();
        limits.max_image_width = Some(self.max_width);
        limits.max_image_height = Some(self.max_height);
        limits.max_alloc = Some(self.max_alloc_bytes);
        limits
    }
}

/// Why an image was refused before its pixels were decoded ([`MediaError::ImageRefused`]). The
/// wire label ([`RefusalReason::as_str`]) is the `reason` of
/// `hs_media_thumbnail_refused_total`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RefusalReason {
    /// Width times height is over [`DecodeLimits::max_pixels`].
    Pixels,
    /// Width or height is over [`DecodeLimits::max_width`] / [`DecodeLimits::max_height`].
    Dimensions,
    /// The decoded pixels would need more than [`DecodeLimits::max_alloc_bytes`].
    Memory,
    /// The image declares a width or height of zero: there is nothing to thumbnail, and a
    /// resize of it divides by zero (the 2026-10-04 fuzz finding asked for a 16 GiB buffer
    /// that way).
    Empty,
}

impl RefusalReason {
    /// The metric label and log value: `pixels`, `dimensions`, `memory` or `empty`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pixels => "pixels",
            Self::Dimensions => "dimensions",
            Self::Memory => "memory",
            Self::Empty => "empty",
        }
    }
}

impl std::fmt::Display for RefusalReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
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
/// The order is the defense: the decoder is built with only the allocation ceiling in force
/// (`ImageReader::limits`, so whatever it reads while parsing the header is bounded), the
/// header's declared width and height are then checked against `limits` — zero, either side,
/// the pixel count and the decoded size against the allocation ceiling — and only then are the
/// full `image::Limits` set on the decoder and the pixels decoded.
///
/// # Errors
/// [`MediaError::ImageRefused`] if the declared image is empty or over `limits` (from the header,
/// before any large buffer is allocated); [`MediaError::DecodeFailed`] if the format is
/// unrecognized or the bytes cannot be decoded.
pub fn decode_with_limits(
    bytes: &[u8],
    limits: DecodeLimits,
) -> Result<(DynamicImage, ImageFormat), MediaError> {
    let mut reader = ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|e| MediaError::DecodeFailed(e.to_string()))?;
    let format = reader
        .format()
        .ok_or_else(|| MediaError::DecodeFailed("unrecognized image format".into()))?;
    let mut header_limits = image::Limits::default();
    header_limits.max_alloc = Some(limits.max_alloc_bytes);
    reader.limits(header_limits);
    let mut decoder = reader
        .into_decoder()
        .map_err(|e| MediaError::DecodeFailed(e.to_string()))?;
    let (width, height) = decoder.dimensions();
    let refuse = |reason| MediaError::ImageRefused {
        width,
        height,
        reason,
    };
    if width == 0 || height == 0 {
        return Err(refuse(RefusalReason::Empty));
    }
    if width > limits.max_width || height > limits.max_height {
        return Err(refuse(RefusalReason::Dimensions));
    }
    if u64::from(width) * u64::from(height) > limits.max_pixels {
        return Err(refuse(RefusalReason::Pixels));
    }
    let mut image_limits = limits.to_image_limits();
    // What `ImageReader::decode` does: the output buffer counts against the ceiling, and the
    // decoder's own working buffers get what is left.
    image_limits
        .reserve(decoder.total_bytes())
        .map_err(|_| refuse(RefusalReason::Memory))?;
    decoder.set_limits(image_limits).map_err(|e| match e {
        image::ImageError::Limits(_) => refuse(RefusalReason::Memory),
        other => MediaError::DecodeFailed(other.to_string()),
    })?;
    let image = DynamicImage::from_decoder(decoder).map_err(|e| match e {
        image::ImageError::Limits(_) => refuse(RefusalReason::Memory),
        other => MediaError::DecodeFailed(other.to_string()),
    })?;
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
        let err = decode_with_limits(&bomb, DecodeLimits::default()).unwrap_err();
        assert!(matches!(
            err,
            MediaError::ImageRefused {
                width: 60_000,
                height: 60_000,
                reason: RefusalReason::Dimensions
            }
        ));
    }

    #[test]
    #[cfg(feature = "test-fixtures")]
    fn each_limit_refuses_from_the_header_with_its_reason() {
        let limits = DecodeLimits::default();
        // 8000 x 8000 is under the side limit and 64 megapixels, over 32Mi.
        let err = decode_with_limits(
            &crate::test_fixtures::decompression_bomb_png(8_000, 8_000),
            limits,
        )
        .unwrap_err();
        assert!(matches!(
            err,
            MediaError::ImageRefused {
                reason: RefusalReason::Pixels,
                ..
            }
        ));
        // Under both, but the decoded RGB8 buffer (3 x 4000 x 4000 = 48 MB) is over a 1 MiB
        // ceiling.
        let err = decode_with_limits(
            &crate::test_fixtures::decompression_bomb_png(4_000, 4_000),
            DecodeLimits {
                max_alloc_bytes: 1024 * 1024,
                ..limits
            },
        )
        .unwrap_err();
        assert!(matches!(
            err,
            MediaError::ImageRefused {
                reason: RefusalReason::Memory,
                ..
            }
        ));
        // A side over the dimension limit, however few pixels.
        let err = decode_with_limits(
            &crate::test_fixtures::decompression_bomb_png(40_000, 1),
            limits,
        )
        .unwrap_err();
        assert!(matches!(
            err,
            MediaError::ImageRefused {
                reason: RefusalReason::Dimensions,
                ..
            }
        ));
    }

    #[test]
    fn limits_follow_the_media_settings() {
        let config = hs_config::media::MediaConfig {
            max_image_pixels: 100,
            max_image_dimension: 50,
            max_image_decode_memory: hs_config::ByteSize::kib(64),
            ..Default::default()
        };
        assert_eq!(
            DecodeLimits::from_config(&config),
            DecodeLimits {
                max_width: 50,
                max_height: 50,
                max_pixels: 100,
                max_alloc_bytes: 65_536,
            }
        );
        // 4 x 4 is 16 pixels; a limit of 10 refuses it.
        let err = decode_with_limits(
            &tiny_png(),
            DecodeLimits {
                max_pixels: 10,
                ..DecodeLimits::default()
            },
        )
        .unwrap_err();
        assert!(matches!(
            err,
            MediaError::ImageRefused {
                width: 4,
                height: 4,
                reason: RefusalReason::Pixels
            }
        ));
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
