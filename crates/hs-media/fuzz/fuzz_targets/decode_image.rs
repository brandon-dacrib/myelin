//! Fuzzes the image decoder path (`hs_media::sniff::decode_with_limits`): the attack surface for
//! a hostile upload (a malformed file, a decompression bomb, a truncated header) reaching a
//! decoder written in Rust but still built on complex, hand-rolled parsers (JPEG, PNG, GIF,
//! WebP) that have all had real CVEs in other implementations. The property under test is simply
//! "never panics, never hangs, never allocates unboundedly" — `decode_with_limits` returning
//! `Err` is always an acceptable outcome; the run-time and memory bounds are what
//! `crate::sniff::DecodeLimits` exists to guarantee regardless of what libFuzzer throws at it.
//!
//! Run with `cargo +nightly fuzz run decode_image` from `crates/hs-media/fuzz/` (requires the
//! `cargo-fuzz` subcommand and a nightly toolchain; see `docs/status/09-media.md` for this
//! session's note that neither was available in the sandbox this was written in, so this has been
//! type-checked but not run to a fault yet).

#![no_main]

use hs_media::sniff::{DecodeLimits, decode_with_limits};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = decode_with_limits(data, DecodeLimits::default());
});
