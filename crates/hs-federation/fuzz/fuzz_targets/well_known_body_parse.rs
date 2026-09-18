//! Fuzzes `hs_federation::discovery::parse_well_known_body` against arbitrary bytes: the entire
//! `.well-known/matrix/server` response body from a server we do not control, before any
//! delegation decision is made from it (threat model section 2.1). Property under test: never
//! panics, never allocates unboundedly, whatever bytes arrive.

#![no_main]

use hs_federation::discovery::parse_well_known_body;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = parse_well_known_body(data);
});
