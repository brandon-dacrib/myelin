//! Writes the test corpus under `tests/fixtures/images/`: valid images in the four formats this
//! crate supports plus the adversarial cases `docs/workstreams/09-media.md`'s day-one work item
//! calls for (a decompression bomb, a truncated header, a mismatched extension, an SVG, an HTML
//! file with an image extension). Uses `crate::test_fixtures`, so this is a thin driver, not a
//! second copy of the generation logic; `crates/hs-media/tests/image_corpus.rs` is what actually
//! asserts this crate's decoder behaves correctly against every file here.
//!
//! Run with `cargo run -p hs-media --example gen_fixtures` from the repository root. Re-run any
//! time `hs_media::test_fixtures` changes; the corpus is committed, this is not run at build time.

use std::fs;
use std::path::Path;

fn write(dir: &Path, name: &str, bytes: &[u8]) {
    let path = dir.join(name);
    fs::write(&path, bytes).unwrap_or_else(|e| panic!("writing {}: {e}", path.display()));
    println!("wrote {} ({} bytes)", path.display(), bytes.len());
}

fn main() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/images");
    fs::create_dir_all(&dir).unwrap();

    write(&dir, "valid.png", &hs_media::test_fixtures::valid_png());
    write(&dir, "valid.jpg", &hs_media::test_fixtures::valid_jpeg());
    write(
        &dir,
        "valid_animated.gif",
        &hs_media::test_fixtures::valid_animated_gif(),
    );
    write(&dir, "valid.webp", &hs_media::test_fixtures::valid_webp());

    // Adversarial: a decompression bomb (tiny file, huge declared dimensions).
    write(
        &dir,
        "decompression_bomb.png",
        &hs_media::test_fixtures::decompression_bomb_png(50_000, 50_000),
    );

    // Adversarial: a truncated header (a valid PNG cut off partway through).
    let png = hs_media::test_fixtures::valid_png();
    write(
        &dir,
        "truncated_header.png",
        &hs_media::test_fixtures::truncate(&png, 20),
    );

    // Adversarial: a truncated body (past the header, short of the full pixel data).
    write(
        &dir,
        "truncated_body.png",
        &hs_media::test_fixtures::truncate(&png, png.len() - 5),
    );

    // Adversarial: a mismatched extension -- real JPEG bytes saved with a `.png` name (and, at
    // upload time, the corresponding mismatched `Content-Type: image/png` claim).
    write(
        &dir,
        "mismatched_extension.png",
        &hs_media::test_fixtures::valid_jpeg(),
    );

    // Adversarial: an SVG carrying an inline <script> -- must never be served inline
    // (`crate::security`'s Rule 1), and is not a raster format `crate::sniff` recognizes at all.
    write(
        &dir,
        "adversarial.svg",
        &hs_media::test_fixtures::svg_with_script(),
    );

    // Adversarial: an HTML file with an image extension, carrying an inline <script>.
    write(
        &dir,
        "html_masquerading_as_image.png",
        &hs_media::test_fixtures::html_masquerading_as_image(),
    );

    println!("done");
}
