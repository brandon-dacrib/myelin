//! Synthetic valid and adversarial image bytes, shared by this crate's own unit tests, the
//! `gen_fixtures` example (which writes them to `tests/fixtures/` as the committed test corpus —
//! see `docs/workstreams/09-media.md`'s day-one work item), and the `hs-media-fuzz` crate's seed
//! corpus. Gated behind the `test-fixtures` Cargo feature (on by default — see `Cargo.toml`), not
//! `#[cfg(test)]`, specifically so the fuzz crate can reach it as an ordinary path dependency.

use std::io::Cursor;

use image::{DynamicImage, Frame, ImageFormat, Rgba, RgbaImage};

/// A small, real, valid PNG (4x4 RGB).
#[must_use]
pub fn valid_png() -> Vec<u8> {
    encode(DynamicImage::new_rgb8(4, 4), ImageFormat::Png)
}

/// A small, real, valid JPEG (4x4 RGB).
#[must_use]
pub fn valid_jpeg() -> Vec<u8> {
    encode(DynamicImage::new_rgb8(4, 4), ImageFormat::Jpeg)
}

/// A small, real, valid, *animated* GIF: three 4x4 frames of different solid colors. Used to
/// exercise "thumbnails only ever use the first frame" (`crate::sniff`'s decode path).
#[must_use]
pub fn valid_animated_gif() -> Vec<u8> {
    let mut out = Vec::new();
    {
        let mut encoder = image::codecs::gif::GifEncoder::new(&mut out);
        for shade in [0u8, 128, 255] {
            let mut img = RgbaImage::new(4, 4);
            for p in img.pixels_mut() {
                *p = Rgba([shade, 0, 0, 255]);
            }
            encoder
                .encode_frame(Frame::new(img))
                .expect("encoding a synthetic GIF frame cannot fail");
        }
    }
    out
}

/// A small, real, valid, static WebP (4x4 RGBA), via the `image` crate's pure-Rust lossless
/// WebP encoder.
#[must_use]
pub fn valid_webp() -> Vec<u8> {
    encode(DynamicImage::new_rgba8(4, 4), ImageFormat::WebP)
}

fn encode(image: DynamicImage, format: ImageFormat) -> Vec<u8> {
    let mut out = Vec::new();
    image
        .write_to(&mut Cursor::new(&mut out), format)
        .unwrap_or_else(|e| panic!("encoding a synthetic {format:?} fixture cannot fail: {e}"));
    out
}

/// Truncates `bytes` to `len`, simulating a connection that dropped mid-upload or a corrupted
/// store read. `len` shorter than any format's magic-number prefix produces a file no decoder
/// can even identify; a `len` inside the header but past the magic bytes produces one that is
/// identified but fails at header parsing; a `len` past the header but short of the full pixel
/// data produces one that fails mid-decode. All three are useful adversarial cases.
#[must_use]
pub fn truncate(bytes: &[u8], len: usize) -> Vec<u8> {
    bytes[..len.min(bytes.len())].to_vec()
}

/// A well-formed SVG carrying an inline `<script>` — the canonical "why SVG is never served
/// inline" adversarial case (`crate::security`'s Rule 1). Valid SVG/XML; not a raster format
/// `image` recognizes at all (see `crate::sniff::sniff_format`).
#[must_use]
pub fn svg_with_script() -> Vec<u8> {
    br#"<svg xmlns="http://www.w3.org/2000/svg" onload="alert(document.domain)">
  <script>alert(document.cookie)</script>
</svg>"#
        .to_vec()
}

/// An HTML file with an inline script, meant to be uploaded with a `Content-Type: image/png` (or
/// a `filename` ending `.png`) claim it does not match — the canonical "mismatched
/// extension/declared type" adversarial case. Neither the claimed type nor the extension changes
/// what `crate::sniff::sniff_format` detects (`None`: not a recognized raster format) or what
/// `crate::security` would ever serve inline (`text/html` is in `ALWAYS_ATTACHMENT`
/// unconditionally, regardless of what the uploader claims the type is).
#[must_use]
pub fn html_masquerading_as_image() -> Vec<u8> {
    b"<html><body><script>alert(document.cookie)</script></body></html>".to_vec()
}

/// A hand-crafted, structurally minimal PNG whose `IHDR` chunk declares a `width` x `height`
/// image (typically far larger than any real photo) while carrying no actual pixel data — the
/// canonical decompression-bomb shape: the compressed file is a few dozen bytes, but a decoder
/// that trusted the declared dimensions and allocated a buffer for them before checking limits
/// would try to allocate `width * height * 4` bytes. `crate::sniff::decode_with_limits` is built
/// specifically to reject this at the header-parsing stage, never reaching that allocation.
///
/// Built by hand (signature + `IHDR` + empty `IDAT` + `IEND`, each with a correct CRC-32) rather
/// than via `image`'s own encoder, since encoding a genuine `width` x `height` pixel buffer would
/// itself perform the large allocation this fixture exists to avoid needing.
#[must_use]
pub fn decompression_bomb_png(width: u32, height: u32) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&[0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A]);
    let mut ihdr = Vec::with_capacity(13);
    ihdr.extend_from_slice(&width.to_be_bytes());
    ihdr.extend_from_slice(&height.to_be_bytes());
    ihdr.push(8); // bit depth
    ihdr.push(2); // color type: truecolor (RGB), needs no palette
    ihdr.push(0); // compression method
    ihdr.push(0); // filter method
    ihdr.push(0); // interlace method
    write_png_chunk(&mut out, b"IHDR", &ihdr);
    write_png_chunk(&mut out, b"IDAT", &[]);
    write_png_chunk(&mut out, b"IEND", &[]);
    out
}

fn write_png_chunk(out: &mut Vec<u8>, chunk_type: &[u8; 4], data: &[u8]) {
    out.extend_from_slice(&(u32::try_from(data.len()).unwrap_or(u32::MAX)).to_be_bytes());
    out.extend_from_slice(chunk_type);
    out.extend_from_slice(data);
    let mut hasher = crc32fast::Hasher::new();
    hasher.update(chunk_type);
    hasher.update(data);
    out.extend_from_slice(&hasher.finalize().to_be_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_png_round_trips_through_the_image_crate() {
        let bytes = valid_png();
        let img = image::load_from_memory(&bytes).unwrap();
        assert_eq!((img.width(), img.height()), (4, 4));
    }

    #[test]
    fn valid_jpeg_round_trips() {
        let bytes = valid_jpeg();
        let img = image::load_from_memory(&bytes).unwrap();
        assert_eq!((img.width(), img.height()), (4, 4));
    }

    #[test]
    fn valid_animated_gif_decodes_and_is_recognized_as_gif() {
        let bytes = valid_animated_gif();
        assert_eq!(image::guess_format(&bytes).unwrap(), ImageFormat::Gif);
        let img = image::load_from_memory(&bytes).unwrap();
        assert_eq!((img.width(), img.height()), (4, 4));
    }

    #[test]
    fn valid_webp_round_trips() {
        let bytes = valid_webp();
        assert_eq!(image::guess_format(&bytes).unwrap(), ImageFormat::WebP);
    }

    #[test]
    fn decompression_bomb_has_a_tiny_byte_size_but_huge_declared_dimensions() {
        let bomb = decompression_bomb_png(50_000, 50_000);
        assert!(bomb.len() < 200, "bomb fixture should be a few dozen bytes");
        // The declared dimensions really are in the header: prove it by reading IHDR back.
        let width = u32::from_be_bytes(bomb[16..20].try_into().unwrap());
        let height = u32::from_be_bytes(bomb[20..24].try_into().unwrap());
        assert_eq!(width, 50_000);
        assert_eq!(height, 50_000);
    }

    #[test]
    fn truncate_never_panics_on_a_length_past_the_end() {
        let bytes = valid_png();
        let out = truncate(&bytes, bytes.len() + 1000);
        assert_eq!(out, bytes);
    }

    #[test]
    fn svg_and_html_fixtures_are_not_empty() {
        assert!(!svg_with_script().is_empty());
        assert!(!html_masquerading_as_image().is_empty());
    }
}
