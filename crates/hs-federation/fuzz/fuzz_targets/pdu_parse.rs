//! Fuzzes `hs_model::Event::parse` against arbitrary bytes from a hostile federation peer's
//! transaction body. This crate does not have its own PDU parser (`docs/status/06-federation.md`
//! item 10: "reuse track 02's"), but every PDU this crate's future `/send` handler and every
//! `make_join`/`send_join`-family seam will accept comes directly from a remote server, so the
//! parser it feeds into is exactly as much this crate's attack surface as its own code. Property
//! under test: never panics, whatever bytes arrive.

#![no_main]

use hs_model::Event;
use hs_model::ids::RoomVersionId;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok(value) = serde_json::from_slice::<serde_json::Value>(data) {
        let _ = Event::parse(&value, RoomVersionId::V11);
    }
});
