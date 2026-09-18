//! Thumbnail generation: crop and scale, the default size table, animated-image and
//! dynamic-thumbnail handling.
//!
//! Re-exports `hs_config::media::{ThumbnailMethod, ThumbnailSize}` rather than defining a
//! parallel type, so a server's configured `thumbnail_sizes` and this module's generator agree by
//! construction.

use image::imageops::FilterType;
use image::{DynamicImage, GenericImageView, ImageFormat};

pub use hs_config::media::{ThumbnailMethod, ThumbnailSize};

use crate::error::MediaError;
use crate::sniff;

/// Synapse's default `thumbnail_sizes` (`docs/workstreams/09-media.md`'s day-one size table):
/// 32x32 crop, 96x96 crop, 320x240 scale, 640x480 scale, 800x600 scale. Exposed here as a plain
/// constant, distinct from [`hs_config::media::MediaConfig::default`] (which is the same list),
/// so callers that just want "the spec-default sizes" do not need an `hs-config` value in hand.
#[must_use]
pub fn default_sizes() -> Vec<ThumbnailSize> {
    hs_config::media::MediaConfig::default().thumbnail_sizes
}

/// The wire string for a [`ThumbnailMethod`] (`crop` / `scale` — the spec's
/// `?method=` value on `GET .../thumbnail/...`, and this crate's on-disk/object-store variant
/// key component; see [`crate::metadata::ThumbnailRecord::variant_key`]).
#[must_use]
pub fn method_str(method: ThumbnailMethod) -> &'static str {
    match method {
        ThumbnailMethod::Crop => "crop",
        ThumbnailMethod::Scale => "scale",
    }
}

/// Parses a `?method=` query value. Unknown values are rejected rather than defaulted, since
/// silently picking a method the client did not ask for could change what gets served for a
/// cached variant key a client is about to request again.
///
/// # Errors
/// Returns [`MediaError::InvalidInput`] for anything other than `crop` or `scale`.
pub fn parse_method(s: &str) -> Result<ThumbnailMethod, MediaError> {
    match s {
        "crop" => Ok(ThumbnailMethod::Crop),
        "scale" => Ok(ThumbnailMethod::Scale),
        other => Err(MediaError::InvalidInput(format!(
            "unknown thumbnail method {other:?}, expected \"crop\" or \"scale\""
        ))),
    }
}

/// Bounds a dynamically requested thumbnail size may not exceed, and whether sizes outside the
/// server's preconfigured [`ThumbnailSize`] list are served at all. Mirrors Synapse's
/// `dynamic_thumbnails` config: when `allow_dynamic` is `false`, only an exact
/// `(width, height, method)` match against `configured_sizes` is served; when `true`, any size up
/// to `max_width`/`max_height` is generated on demand and (by [`crate::repository`]) cached for
/// reuse.
#[derive(Debug, Clone)]
pub struct ThumbnailPolicy {
    /// The server's preconfigured size table.
    pub configured_sizes: Vec<ThumbnailSize>,
    /// Whether a size not in `configured_sizes` may still be generated on request.
    pub allow_dynamic: bool,
    /// Ceiling on a dynamically requested width, pixels.
    pub max_dynamic_width: u32,
    /// Ceiling on a dynamically requested height, pixels.
    pub max_dynamic_height: u32,
}

impl Default for ThumbnailPolicy {
    fn default() -> Self {
        Self {
            configured_sizes: default_sizes(),
            allow_dynamic: false,
            max_dynamic_width: 1600,
            max_dynamic_height: 1600,
        }
    }
}

impl ThumbnailPolicy {
    /// Whether `(width, height, method)` may be generated under this policy.
    #[must_use]
    pub fn allows(&self, width: u32, height: u32, method: ThumbnailMethod) -> bool {
        let is_configured = self
            .configured_sizes
            .iter()
            .any(|s| s.width == width && s.height == height && s.method == method);
        if is_configured {
            return true;
        }
        self.allow_dynamic
            && width <= self.max_dynamic_width
            && height <= self.max_dynamic_height
            && width > 0
            && height > 0
    }
}

/// Resizes `image` to `(width, height)` using `method`:
///
/// - [`ThumbnailMethod::Scale`]: fits the image within the target box, preserving aspect ratio
///   (the result may be narrower or shorter than requested on one axis — never cropped, never
///   upscaled past the source's own size, matching Synapse's `scale` semantics).
/// - [`ThumbnailMethod::Crop`]: resizes to fill the target box exactly, cropping any excess from
///   the longer axis, centered.
///
/// Both use [`FilterType::Lanczos3`], the highest-quality resampling filter the `image` crate
/// offers, since thumbnails are generated once and served many times (the cost is amortized).
#[must_use]
pub fn resize(
    image: &DynamicImage,
    width: u32,
    height: u32,
    method: ThumbnailMethod,
) -> DynamicImage {
    match method {
        ThumbnailMethod::Scale => {
            let (src_w, src_h) = image.dimensions();
            let target_w = width.max(1);
            let target_h = height.max(1);
            let scale = (f64::from(target_w) / f64::from(src_w))
                .min(f64::from(target_h) / f64::from(src_h))
                .min(1.0);
            let new_w = ((f64::from(src_w) * scale).round() as u32).max(1);
            let new_h = ((f64::from(src_h) * scale).round() as u32).max(1);
            image.resize_exact(new_w, new_h, FilterType::Lanczos3)
        }
        ThumbnailMethod::Crop => {
            image.resize_to_fill(width.max(1), height.max(1), FilterType::Lanczos3)
        }
    }
}

/// Which format a generated thumbnail is encoded as. Synapse's rule (behavior only, no code
/// copied, `synapse/media/thumbnailer.py`): a source with an alpha channel-capable format (PNG,
/// GIF, WebP) thumbnails to PNG so transparency survives; everything else (JPEG, and any format
/// without meaningful alpha) thumbnails to JPEG, which is smaller for photographic content.
#[must_use]
pub fn output_format_for(source_format: ImageFormat) -> ImageFormat {
    match source_format {
        ImageFormat::Png | ImageFormat::Gif | ImageFormat::WebP => ImageFormat::Png,
        _ => ImageFormat::Jpeg,
    }
}

/// Generates one thumbnail: decodes `source_bytes` (bounded by `limits`, `crate::sniff`'s
/// decompression-bomb defense), resizes per `method`, and encodes to the format
/// [`output_format_for`] selects. For an animated source, only the first frame is ever used (see
/// `crate::sniff::decode_with_limits`'s docs) — this crate never generates an animated thumbnail,
/// matching Synapse's own behavior: dynamic thumbnails control *size*, not animation.
///
/// # Errors
/// Returns [`MediaError::DecodeFailed`] if `source_bytes` cannot be decoded (including exceeding
/// `limits`), or a store/encode failure wrapped as [`MediaError::Store`].
pub fn generate(
    source_bytes: &[u8],
    width: u32,
    height: u32,
    method: ThumbnailMethod,
    limits: sniff::DecodeLimits,
) -> Result<(Vec<u8>, &'static str), MediaError> {
    let (image, source_format) = sniff::decode_with_limits(source_bytes, limits)?;
    let resized = resize(&image, width, height, method);
    let output_format = output_format_for(source_format);
    let mut out = Vec::new();
    resized
        .write_to(&mut std::io::Cursor::new(&mut out), output_format)
        .map_err(|e| MediaError::Store(format!("encoding thumbnail: {e}")))?;
    Ok((out, sniff::mime_for_format(output_format)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_sizes_match_the_spec_table() {
        let sizes = default_sizes();
        assert_eq!(sizes.len(), 5);
        assert!(sizes.contains(&ThumbnailSize {
            width: 32,
            height: 32,
            method: ThumbnailMethod::Crop
        }));
        assert!(sizes.contains(&ThumbnailSize {
            width: 800,
            height: 600,
            method: ThumbnailMethod::Scale
        }));
    }

    #[test]
    fn parse_method_accepts_the_two_spec_values() {
        assert_eq!(parse_method("crop").unwrap(), ThumbnailMethod::Crop);
        assert_eq!(parse_method("scale").unwrap(), ThumbnailMethod::Scale);
    }

    #[test]
    fn parse_method_rejects_anything_else() {
        assert!(parse_method("stretch").is_err());
        assert!(parse_method("").is_err());
    }

    #[test]
    fn policy_allows_exact_configured_sizes() {
        let policy = ThumbnailPolicy::default();
        assert!(policy.allows(96, 96, ThumbnailMethod::Crop));
        assert!(!policy.allows(97, 97, ThumbnailMethod::Crop));
    }

    #[test]
    fn policy_rejects_dynamic_sizes_when_disabled() {
        let policy = ThumbnailPolicy::default();
        assert!(!policy.allows(200, 200, ThumbnailMethod::Crop));
    }

    #[test]
    fn policy_allows_dynamic_sizes_within_bounds_when_enabled() {
        let policy = ThumbnailPolicy {
            allow_dynamic: true,
            ..ThumbnailPolicy::default()
        };
        assert!(policy.allows(200, 200, ThumbnailMethod::Crop));
        assert!(!policy.allows(5000, 5000, ThumbnailMethod::Crop));
        assert!(!policy.allows(0, 100, ThumbnailMethod::Crop));
    }

    #[test]
    fn scale_preserves_aspect_ratio_and_never_upscales() {
        let img = DynamicImage::new_rgb8(200, 100);
        let out = resize(&img, 50, 50, ThumbnailMethod::Scale);
        // 200x100 fit within 50x50 preserving 2:1 aspect -> 50x25.
        assert_eq!(out.dimensions(), (50, 25));

        // Never upscale past the source.
        let small = DynamicImage::new_rgb8(10, 10);
        let out_small = resize(&small, 100, 100, ThumbnailMethod::Scale);
        assert_eq!(out_small.dimensions(), (10, 10));
    }

    #[test]
    fn crop_fills_the_exact_target_box() {
        let img = DynamicImage::new_rgb8(200, 100);
        let out = resize(&img, 50, 50, ThumbnailMethod::Crop);
        assert_eq!(out.dimensions(), (50, 50));
    }

    #[test]
    fn output_format_uses_png_for_alpha_capable_sources() {
        assert_eq!(output_format_for(ImageFormat::Png), ImageFormat::Png);
        assert_eq!(output_format_for(ImageFormat::Gif), ImageFormat::Png);
        assert_eq!(output_format_for(ImageFormat::WebP), ImageFormat::Png);
    }

    #[test]
    fn output_format_uses_jpeg_for_jpeg_sources() {
        assert_eq!(output_format_for(ImageFormat::Jpeg), ImageFormat::Jpeg);
    }

    #[cfg(feature = "test-fixtures")]
    #[test]
    fn generate_produces_a_decodable_thumbnail() {
        let png = crate::test_fixtures::valid_png();
        let (bytes, mime) = generate(
            &png,
            2,
            2,
            ThumbnailMethod::Crop,
            sniff::DecodeLimits::default(),
        )
        .unwrap();
        assert_eq!(mime, "image/png");
        let decoded = image::load_from_memory(&bytes).unwrap();
        assert_eq!(decoded.dimensions(), (2, 2));
    }

    #[cfg(feature = "test-fixtures")]
    #[test]
    fn generate_from_animated_gif_uses_only_the_first_frame() {
        let gif = crate::test_fixtures::valid_animated_gif();
        let (bytes, mime) = generate(
            &gif,
            4,
            4,
            ThumbnailMethod::Crop,
            sniff::DecodeLimits::default(),
        )
        .unwrap();
        // Animated source -> alpha-capable -> PNG output, and a single static frame.
        assert_eq!(mime, "image/png");
        assert!(image::load_from_memory(&bytes).is_ok());
    }

    #[cfg(feature = "test-fixtures")]
    #[test]
    fn generate_rejects_a_decompression_bomb() {
        let bomb = crate::test_fixtures::decompression_bomb_png(60_000, 60_000);
        let err = generate(
            &bomb,
            96,
            96,
            ThumbnailMethod::Crop,
            sniff::DecodeLimits::default(),
        )
        .unwrap_err();
        assert!(matches!(err, MediaError::DecodeFailed(_)));
    }
}
