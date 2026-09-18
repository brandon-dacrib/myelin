//! Fuzzes `hs_federation::xmatrix::parse_x_matrix_header` against arbitrary bytes used as the
//! `Authorization` header value — every request to the federation listener carries one, entirely
//! attacker-controlled before verification happens. Property under test (threat model section
//! 2.3's "confuse a naive parser" threat): never panics, whatever bytes arrive, including bytes
//! that are not valid UTF-8 or not valid `HeaderValue` bytes at all (skipped, since a real
//! `HeaderMap` could never hold them either).

#![no_main]

use axum::http::{HeaderMap, HeaderValue, header::AUTHORIZATION};
use hs_federation::xmatrix::parse_x_matrix_header;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Ok(value) = HeaderValue::from_bytes(data) else {
        return;
    };
    let mut headers = HeaderMap::new();
    headers.insert(AUTHORIZATION, value);
    let _ = parse_x_matrix_header(&headers);
});
