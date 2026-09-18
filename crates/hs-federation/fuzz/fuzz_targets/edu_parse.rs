//! Fuzzes `hs_federation::edu::parse_edu` against arbitrary bytes. EDUs (typing, receipts,
//! presence, device-list updates) arrive inside a `/send` transaction body from a remote,
//! potentially hostile server. Property under test: never panics, whatever bytes arrive.

#![no_main]

use hs_federation::edu::parse_edu;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok(value) = serde_json::from_slice::<serde_json::Value>(data) {
        let _ = parse_edu(&value);
    }
});
