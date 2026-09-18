//! Fuzzes the full thumbnail pipeline (`hs_media::thumbnail::generate`): decode, resize
//! (crop/scale), re-encode. Complements `decode_image` by also exercising the resize and
//! re-encode paths, which run on whatever `DynamicImage` the decoder produced, including
//! degenerate cases (1x1, extreme aspect ratios) a hostile-but-within-limits file can still
//! produce. Property under test: never panics, never hangs.
//!
//! The first two bytes of the fuzz input pick a target width and height (each `1..=256`, via a
//! cheap modulo — no `arbitrary` dependency needed for two bytes) and the crop/scale method
//! alternates on a third byte, so the harness exercises more than one fixed size; the remainder of
//! the input is the candidate image bytes.

#![no_main]

use hs_media::sniff::DecodeLimits;
use hs_media::thumbnail::{self, ThumbnailMethod};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if data.len() < 3 {
        return;
    }
    let width = u32::from(data[0]) % 256 + 1;
    let height = u32::from(data[1]) % 256 + 1;
    let method = if data[2] % 2 == 0 {
        ThumbnailMethod::Crop
    } else {
        ThumbnailMethod::Scale
    };
    let _ = thumbnail::generate(&data[3..], width, height, method, DecodeLimits::default());
});
