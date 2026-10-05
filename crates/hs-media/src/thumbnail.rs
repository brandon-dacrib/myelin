//! Thumbnail generation: crop and scale, the default size table, animated-image and
//! dynamic-thumbnail handling.
//!
//! Re-exports `hs_config::media::{ThumbnailMethod, ThumbnailSize}` rather than defining a
//! parallel type, so a server's configured `thumbnail_sizes` and this module's generator agree by
//! construction.

use image::imageops::FilterType;
use image::{DynamicImage, GenericImageView, ImageFormat};
use prometheus_client::metrics::counter::Counter;
use prometheus_client::metrics::family::Family;

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

    /// The size to serve for a request of `(width, height, method)`: the request itself when
    /// [`ThumbnailPolicy::allows`] it, otherwise the nearest of `configured_sizes`, the way
    /// Synapse serves a server without `dynamic_thumbnails` (`ThumbnailProvider._select_thumbnail`
    /// in `synapse/media/thumbnailer.py`, read for behaviour): among the sizes of the requested
    /// method, those at least as wide or as tall as asked come first, and of those the one whose
    /// width and height differ least from the request (`|(w - tw) * (h - th)|`); a crop request
    /// also prefers the closest aspect ratio, and a request bigger than every size gets the
    /// biggest. A method no size is configured for falls back to the other method's sizes.
    /// `None` only when nothing is configured, or for a zero width or height.
    #[must_use]
    pub fn select(
        &self,
        width: u32,
        height: u32,
        method: ThumbnailMethod,
    ) -> Option<ThumbnailSize> {
        if width == 0 || height == 0 {
            return None;
        }
        if self.allows(width, height, method) {
            return Some(ThumbnailSize {
                width,
                height,
                method,
            });
        }
        let of_method: Vec<&ThumbnailSize> = self
            .configured_sizes
            .iter()
            .filter(|s| s.method == method)
            .collect();
        let candidates = if of_method.is_empty() {
            self.configured_sizes.iter().collect()
        } else {
            of_method
        };
        let (w, h) = (i64::from(width), i64::from(height));
        candidates
            .into_iter()
            .min_by_key(|s| {
                let (tw, th) = (i64::from(s.width), i64::from(s.height));
                // Sizes at least as big on one side as the request sort before smaller ones.
                let too_small = !(tw >= w || th >= h);
                let aspect = if method == ThumbnailMethod::Crop {
                    (w * th - h * tw).abs()
                } else {
                    0
                };
                let size = ((w - tw) * (h - th)).abs();
                (too_small, aspect, size, tw, th)
            })
            .copied()
    }
}

/// Resizes `image` to `(width, height)` using `method`:
///
/// - [`ThumbnailMethod::Scale`]: fits the image within the target box, preserving aspect ratio
///   (the result may be narrower or shorter than requested on one axis — never cropped, never
///   upscaled past the source's own size, matching Synapse's `scale` semantics).
/// - [`ThumbnailMethod::Crop`]: fills the target box exactly from the centred region of the
///   source that has the target's aspect ratio (the excess of the longer axis is cut off).
///
/// Both finish with [`FilterType::Lanczos3`], the highest-quality resampling filter the `image`
/// crate offers, since thumbnails are generated once and served many times.
///
/// Memory is bounded by the source and the target, whatever their shapes: crop cuts the region
/// out first rather than scaling the whole source to cover the box (`resize_to_fill`, which for
/// a 1326 x 0 GIF asked for a 4,294,967,295 x 1 intermediate: 16 GiB, the 2026-10-04 fuzz
/// finding), and a source more than twice the target on a side is first box-filtered down to
/// twice the target, because Lanczos3 resamples every source column to the new height in 32-bit
/// float RGBA (16 bytes a pixel) before it narrows them. An empty source (zero width or height,
/// which [`crate::sniff::decode_with_limits`] refuses) is returned as it is.
#[must_use]
pub fn resize(
    image: &DynamicImage,
    width: u32,
    height: u32,
    method: ThumbnailMethod,
) -> DynamicImage {
    let (src_w, src_h) = image.dimensions();
    if src_w == 0 || src_h == 0 {
        return image.clone();
    }
    let (width, height) = (width.max(1), height.max(1));
    match method {
        ThumbnailMethod::Scale => {
            let scale = (f64::from(width) / f64::from(src_w))
                .min(f64::from(height) / f64::from(src_h))
                .min(1.0);
            // `scale <= 1`, so these are at most the source's own sides.
            let new_w = ((f64::from(src_w) * scale).round() as u32).clamp(1, src_w);
            let new_h = ((f64::from(src_h) * scale).round() as u32).clamp(1, src_h);
            shrink_then_resize(image, new_w, new_h)
        }
        ThumbnailMethod::Crop => {
            let (w, h) = (u64::from(width), u64::from(height));
            let (sw, sh) = (u64::from(src_w), u64::from(src_h));
            // The largest centred region with the target's aspect ratio. Each computed side is
            // at most the source's (so it fits a u32); the fallback is unreachable.
            let (crop_w, crop_h) = if sw * h > sh * w {
                let cw = (sh * w / h).clamp(1, sw);
                (u32::try_from(cw).unwrap_or(src_w), src_h)
            } else {
                let ch = (sw * h / w).clamp(1, sh);
                (src_w, u32::try_from(ch).unwrap_or(src_h))
            };
            if (crop_w, crop_h) == (src_w, src_h) {
                shrink_then_resize(image, width, height)
            } else {
                let region =
                    image.crop_imm((src_w - crop_w) / 2, (src_h - crop_h) / 2, crop_w, crop_h);
                shrink_then_resize(&region, width, height)
            }
        }
    }
}

/// [`FilterType::Lanczos3`] to exactly `(width, height)`, after a box-filter reduction
/// (`thumbnail_exact`, which allocates only its output) of any side more than twice the target,
/// so Lanczos's float intermediate is at most `2 * width * height` pixels. See [`resize`].
fn shrink_then_resize(image: &DynamicImage, width: u32, height: u32) -> DynamicImage {
    let (src_w, src_h) = image.dimensions();
    let pre_w = width.saturating_mul(2).min(src_w);
    let pre_h = height.saturating_mul(2).min(src_h);
    if pre_w < src_w || pre_h < src_h {
        image
            .thumbnail_exact(pre_w, pre_h)
            .resize_exact(width, height, FilterType::Lanczos3)
    } else {
        image.resize_exact(width, height, FilterType::Lanczos3)
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
/// [`MediaError::ImageRefused`] if the image's header declares an empty picture or one over
/// `limits`; [`MediaError::DecodeFailed`] if `source_bytes` cannot be decoded; an encode failure
/// as [`MediaError::Store`].
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

/// Labels for [`ThumbnailMetrics::refused`].
#[derive(Debug, Clone, PartialEq, Eq, Hash, prometheus_client::encoding::EncodeLabelSet)]
pub struct RefusedLabels {
    /// A [`crate::sniff::RefusalReason`] label (`pixels`, `dimensions`, `memory`, `empty`), or
    /// `undecodable` for a source that is not an image this server can read.
    pub reason: String,
}

/// The thumbnail metric family:
///
/// - `hs_media_thumbnail_refused_total{reason}`: thumbnails not made because the source image
///   was refused before decoding (`pixels`, `dimensions`, `memory`, `empty`; see
///   [`crate::sniff::RefusalReason`]) or could not be decoded (`undecodable`).
#[derive(Clone, Default)]
pub struct ThumbnailMetrics {
    /// `hs_media_thumbnail_refused_total`.
    pub refused: Family<RefusedLabels, Counter>,
}

impl ThumbnailMetrics {
    /// Registers the family into `metrics`'s shared registry.
    #[must_use]
    pub fn register(metrics: &hs_telemetry::metrics::Metrics) -> Self {
        let this = Self::default();
        metrics.with_registry(|registry| {
            // No `_total` suffix here: the text encoder appends it to a counter.
            registry.register(
                "hs_media_thumbnail_refused",
                "Thumbnails not made, by reason: the source image is over a media limit (pixels, dimensions, memory), declares no pixels (empty), or cannot be decoded (undecodable).",
                this.refused.clone(),
            );
        });
        this
    }

    pub(crate) fn refused(&self, reason: &str) {
        self.refused
            .get_or_create(&RefusedLabels {
                reason: reason.to_owned(),
            })
            .inc();
    }
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
    fn an_unconfigured_size_is_served_from_the_nearest_configured_one() {
        let policy = ThumbnailPolicy::default();
        let size = |w, h, method| ThumbnailSize {
            width: w,
            height: h,
            method,
        };
        // Complement's request (`TestLocalPngThumbnail`): 32x32 scale, where only crops are
        // that small. The smallest scale size covers it.
        assert_eq!(
            policy.select(32, 32, ThumbnailMethod::Scale),
            Some(size(320, 240, ThumbnailMethod::Scale))
        );
        assert_eq!(
            policy.select(32, 32, ThumbnailMethod::Crop),
            Some(size(32, 32, ThumbnailMethod::Crop))
        );
        assert_eq!(
            policy.select(50, 50, ThumbnailMethod::Crop),
            Some(size(96, 96, ThumbnailMethod::Crop))
        );
        assert_eq!(
            policy.select(700, 500, ThumbnailMethod::Scale),
            Some(size(800, 600, ThumbnailMethod::Scale))
        );
        // Bigger than every size: the biggest.
        assert_eq!(
            policy.select(4000, 3000, ThumbnailMethod::Scale),
            Some(size(800, 600, ThumbnailMethod::Scale))
        );
        assert_eq!(policy.select(0, 32, ThumbnailMethod::Scale), None);
        let crops_only = ThumbnailPolicy {
            configured_sizes: vec![size(64, 64, ThumbnailMethod::Crop)],
            ..ThumbnailPolicy::default()
        };
        assert_eq!(
            crops_only.select(320, 240, ThumbnailMethod::Scale),
            Some(size(64, 64, ThumbnailMethod::Crop))
        );
        let none = ThumbnailPolicy {
            configured_sizes: Vec::new(),
            ..ThumbnailPolicy::default()
        };
        assert_eq!(none.select(32, 32, ThumbnailMethod::Scale), None);
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
        assert!(matches!(err, MediaError::ImageRefused { .. }));
    }

    /// The `thumbnail_generate` fuzz target's out-of-memory input of 2026-10-04 (CI run on
    /// `f1cc1d56`, `libFuzzer: out-of-memory (malloc(17179869180))`): a GIF87a whose logical
    /// screen is 1326 x 0. It decoded to an empty image, and `resize_to_fill` to 27 x 11 scaled
    /// it by 11 / 0 = infinity, asking for a 4,294,967,295 x 1 RGBA buffer. The first three bytes
    /// are the harness's width, height and method.
    const FUZZ_OOM_2026_10_04: &[u8] = &[
        0x1a, 0x0a, 0x16, 0x47, 0x49, 0x46, 0x38, 0x37, 0x61, 0x2e, 0x05, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x2c, 0x4d, 0xff, 0x05, 0x00, 0xdb, 0xb8, 0x00, 0x00, 0xd2, 0x00, 0x00, 0x89, 0x2a,
        0x00, 0x00, 0x00, 0x00, 0xff, 0xff, 0xff, 0x21, 0x01, 0x00, 0x03, 0x00, 0x00, 0x08, 0x08,
        0x08, 0x08, 0x08, 0x08, 0x08, 0x08, 0x08, 0x08, 0x08, 0x00, 0x71, 0x6f, 0x69, 0x66, 0x00,
        0x00, 0x00, 0x00, 0x00,
    ];

    #[test]
    fn the_fuzz_oom_input_is_refused_as_empty_at_every_size() {
        let image = &FUZZ_OOM_2026_10_04[3..];
        let started = std::time::Instant::now();
        for size in default_sizes() {
            let err = generate(
                image,
                size.width,
                size.height,
                size.method,
                sniff::DecodeLimits::default(),
            )
            .unwrap_err();
            assert!(
                matches!(
                    err,
                    MediaError::ImageRefused {
                        width: 1326,
                        height: 0,
                        reason: sniff::RefusalReason::Empty
                    }
                ),
                "{err:?}"
            );
        }
        // The harness's own size and method too.
        assert!(
            generate(
                image,
                27,
                11,
                ThumbnailMethod::Crop,
                sniff::DecodeLimits::default()
            )
            .is_err()
        );
        assert!(started.elapsed() < std::time::Duration::from_secs(5));
    }

    #[test]
    fn resize_of_an_empty_image_allocates_nothing() {
        let empty = DynamicImage::new_rgba8(1326, 0);
        for method in [ThumbnailMethod::Crop, ThumbnailMethod::Scale] {
            assert_eq!(resize(&empty, 27, 11, method).dimensions(), (1326, 0));
        }
    }

    #[test]
    fn crop_of_an_extreme_aspect_ratio_stays_small() {
        // 20000 x 1 to 96 x 96: `resize_to_fill` would scale it to 1,920,000 x 96 first (737 MB
        // of RGBA8, and 16 bytes a pixel more in Lanczos's float pass). The crop takes the
        // centred 1 x 1 instead.
        let wide = DynamicImage::new_rgba8(20_000, 1);
        assert_eq!(
            resize(&wide, 96, 96, ThumbnailMethod::Crop).dimensions(),
            (96, 96)
        );
        let tall = DynamicImage::new_rgba8(1, 20_000);
        assert_eq!(
            resize(&tall, 96, 96, ThumbnailMethod::Crop).dimensions(),
            (96, 96)
        );
        assert_eq!(
            resize(&wide, 800, 600, ThumbnailMethod::Scale).dimensions(),
            (800, 1)
        );
        assert_eq!(
            resize(&tall, 800, 600, ThumbnailMethod::Scale).dimensions(),
            (1, 600)
        );
    }

    #[test]
    fn crop_keeps_the_centre_of_the_longer_axis() {
        // Left third black, middle third white, right third black: a centred square crop is
        // all white.
        let mut img = image::RgbImage::new(300, 100);
        for (x, _, pixel) in img.enumerate_pixels_mut() {
            *pixel = if (100..200).contains(&x) {
                image::Rgb([255, 255, 255])
            } else {
                image::Rgb([0, 0, 0])
            };
        }
        let out = resize(&DynamicImage::ImageRgb8(img), 10, 10, ThumbnailMethod::Crop).to_rgb8();
        assert!(
            out.pixels().all(|p| p.0.iter().all(|&c| c > 200)),
            "{:?}",
            out.get_pixel(0, 0)
        );
    }

    #[test]
    fn a_large_source_shrinks_through_the_box_filter_to_the_exact_size() {
        let big = DynamicImage::new_rgb8(4_000, 3_000);
        assert_eq!(
            resize(&big, 96, 96, ThumbnailMethod::Crop).dimensions(),
            (96, 96)
        );
        assert_eq!(
            resize(&big, 800, 600, ThumbnailMethod::Scale).dimensions(),
            (800, 600)
        );
        // Upscaling a crop stays exact too.
        let small = DynamicImage::new_rgb8(10, 5);
        assert_eq!(
            resize(&small, 96, 96, ThumbnailMethod::Crop).dimensions(),
            (96, 96)
        );
    }
}
