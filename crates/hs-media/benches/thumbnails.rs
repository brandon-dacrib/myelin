//! Thumbnail throughput benchmark: the default size table (32x32 crop, 96x96 crop, 320x240
//! scale, 640x480 scale, 800x600 scale — `docs/workstreams/09-media.md`'s day-one work item,
//! `crate::thumbnail::default_sizes`) generated from a realistic 1920x1080 source image.
//!
//! Run with `cargo bench -p hs-media --bench thumbnails`. Phase 0's deliverable
//! (`docs/workstreams/09-media.md`) calls for numbers "on arm64"; this benchmark is
//! architecture-agnostic (Criterion reports wall-clock time for whatever machine runs it) — an
//! arm64 run is an operational step (run this on arm64 hardware or in CI), not something the
//! benchmark code itself needs to special-case.
//!
//! The source is JPEG-encoded (not a solid color) so the decoder does real entropy-decoding work
//! and the resize step operates on non-trivial pixel data, closer to a real photo upload than a
//! degenerate all-one-color test image would be.

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use hs_media::sniff::DecodeLimits;
use hs_media::thumbnail;

/// Builds a synthetic-but-non-trivial JPEG source image: a smooth gradient plus a fast-varying
/// component (`x ^ y`) so JPEG's DCT has genuine high-frequency content to encode, rather than
/// compressing to almost nothing the way a flat color would.
fn make_source_jpeg(width: u32, height: u32) -> Vec<u8> {
    let mut img = image::RgbImage::new(width, height);
    for (x, y, px) in img.enumerate_pixels_mut() {
        *px = image::Rgb([(x % 256) as u8, (y % 256) as u8, ((x ^ y) % 256) as u8]);
    }
    let dynamic = image::DynamicImage::ImageRgb8(img);
    let mut out = Vec::new();
    dynamic
        .write_to(
            &mut std::io::Cursor::new(&mut out),
            image::ImageFormat::Jpeg,
        )
        .expect("encoding the synthetic benchmark source cannot fail");
    out
}

fn bench_default_sizes(c: &mut Criterion) {
    let source = make_source_jpeg(1920, 1080);
    let mut group = c.benchmark_group("thumbnail_default_sizes");
    for size in thumbnail::default_sizes() {
        let label = format!("{}x{}", size.width, size.height);
        group.bench_with_input(
            BenchmarkId::new(label, thumbnail::method_str(size.method)),
            &size,
            |b, size| {
                b.iter(|| {
                    thumbnail::generate(
                        criterion::black_box(&source),
                        size.width,
                        size.height,
                        size.method,
                        DecodeLimits::default(),
                    )
                    .expect("benchmark source is always a valid, in-limits JPEG")
                });
            },
        );
    }
    group.finish();
}

/// A second group at a larger, non-default ("dynamic thumbnail") size, so the benchmark also
/// reports a number for the resize-heavy end of the size range the `dynamic_thumbnails` policy
/// (`crate::thumbnail::ThumbnailPolicy`) can be configured to allow.
fn bench_dynamic_size(c: &mut Criterion) {
    let source = make_source_jpeg(1920, 1080);
    c.bench_function("thumbnail_dynamic_1600x1600_scale", |b| {
        b.iter(|| {
            thumbnail::generate(
                criterion::black_box(&source),
                1600,
                1600,
                thumbnail::ThumbnailMethod::Scale,
                DecodeLimits::default(),
            )
            .expect("benchmark source is always a valid, in-limits JPEG")
        });
    });
}

criterion_group!(benches, bench_default_sizes, bench_dynamic_size);
criterion_main!(benches);
