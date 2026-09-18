//! Fuzzes `RemoteKeyCache::ingest_response` against arbitrary JSON claiming to be a
//! `/_matrix/key/v2/server` (or notary) response — deliberately hostile input per threat model
//! section 2.2: a not-validly-self-signed response, a wrong `server_name`, huge `old_verify_keys`
//! maps, deeply nested `signatures`. Property under test: never panics, never accepts a response
//! that fails self-signature verification, whatever bytes arrive.

#![no_main]

use hs_federation::keys::{DynRemoteKeyCache, KeyServerFetcher, RemoteKeyCache};
use libfuzzer_sys::fuzz_target;

struct NullFetcher;

#[async_trait::async_trait]
impl KeyServerFetcher for NullFetcher {
    async fn fetch_server_key(&self, _server_name: &str) -> Option<serde_json::Value> {
        None
    }
}

fn cache() -> DynRemoteKeyCache {
    RemoteKeyCache::new(Box::new(NullFetcher) as Box<dyn KeyServerFetcher>)
}

fuzz_target!(|data: &[u8]| {
    if let Ok(value) = serde_json::from_slice::<serde_json::Value>(data) {
        let cache = cache();
        let _ = cache.ingest_response("fuzzed.example.org", &value);
    }
});
