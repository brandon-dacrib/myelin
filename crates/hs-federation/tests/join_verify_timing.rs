//! The timing of verifying events from a server that is gone, through the public API only, so
//! the same test measures the tree before and after the 2026-10-10 fix (status 06): before it,
//! every event from a gone server cost the federation client's full 30 s request timeout and
//! the key cache kept no memory of the failure; after it, a fetch has a 10 s budget and a
//! server whose fetch failed is not asked again until a backoff ends. Time is tokio's paused
//! clock, so the numbers are virtual seconds and the test is instant.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use hs_federation::inbound::verify_pdu;
use hs_federation::keys::{KeyServerFetcher, OwnSigningKeys, RemoteKeyCache};
use hs_model::canonical::{CanonicalJsonObject, CanonicalJsonValue, to_canonical_object};
use hs_model::signing::{SigningKeyPair, sign_object};
use ruma::RoomVersionId;
use serde_json::Value;

/// The client's request timeout before the fix: what a fetch of a gone server's keys cost.
const CLIENT_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// A key server that hangs for the client's request timeout and then fails, like a server
/// whose address no longer answers.
struct GoneServer {
    fetches: Arc<AtomicUsize>,
}

#[async_trait]
impl KeyServerFetcher for GoneServer {
    async fn fetch_server_key(&self, _server_name: &str) -> Option<Value> {
        self.fetches.fetch_add(1, Ordering::SeqCst);
        tokio::time::sleep(CLIENT_REQUEST_TIMEOUT).await;
        None
    }
}

/// A member event of `!r:gone.example.org` by a user of `gone.example.org`, hashed, redacted
/// and signed under that server's name, as a conformant sender would.
fn signed_event(keys: &OwnSigningKeys, depth: i64) -> Value {
    let sender = format!("@u{depth}:gone.example.org");
    let mut object = to_canonical_object(
        &serde_json::json!({
            "type": "m.room.member",
            "room_id": "!r:gone.example.org",
            "sender": sender,
            "state_key": sender,
            "origin_server_ts": depth * 1000,
            "depth": depth,
            "content": {"membership": "join"},
            "prev_events": [],
            "auth_events": [],
        }),
        true,
    )
    .unwrap();
    let hash = hs_model::hash::content_hash_base64(&object);
    object.insert(
        "hashes".to_owned(),
        CanonicalJsonValue::Object(CanonicalJsonObject::from([(
            "sha256".to_owned(),
            CanonicalJsonValue::String(hash),
        )])),
    );
    let server = ruma::ServerName::parse("gone.example.org").unwrap();
    let rules = hs_model::room_version::rules_for(&RoomVersionId::V11).unwrap();
    let mut redacted = hs_model::redaction::redact(&object, &rules.redaction).unwrap();
    sign_object(&mut redacted, &server, keys.primary()).unwrap();
    object.insert(
        "signatures".to_owned(),
        redacted.remove("signatures").unwrap(),
    );
    serde_json::from_slice(&CanonicalJsonValue::Object(object).to_canonical_bytes()).unwrap()
}

/// Fifty events from one gone server, verified one after another as the join did before the
/// fix. Before: 50 fetches, 50 x 30 s = 1500 s. After: one fetch given up at 10 s, the other
/// 49 refused at once by the backoff, 10 s in all.
#[tokio::test(start_paused = true)]
async fn fifty_events_from_a_gone_server_verified_one_after_another() {
    let keys = OwnSigningKeys::from_keys(vec![SigningKeyPair::generate("a_1")]);
    let fetches = Arc::new(AtomicUsize::new(0));
    let cache = RemoteKeyCache::new(Box::new(GoneServer {
        fetches: fetches.clone(),
    }) as Box<dyn KeyServerFetcher>);
    let events: Vec<Value> = (1..=50).map(|depth| signed_event(&keys, depth)).collect();

    let started = tokio::time::Instant::now();
    let mut dropped = 0;
    for raw in &events {
        if verify_pdu(raw, &RoomVersionId::V11, &cache).await.is_err() {
            dropped += 1;
        }
    }
    let elapsed = started.elapsed();
    eprintln!(
        "fifty events from a gone server, one after another: {} fetches, {} s",
        fetches.load(Ordering::SeqCst),
        elapsed.as_secs()
    );
    assert_eq!(dropped, 50);
    assert_eq!(
        fetches.load(Ordering::SeqCst),
        1,
        "one fetch, not one per event"
    );
    assert_eq!(
        elapsed,
        Duration::from_secs(10),
        "one fetch budget, not 50 request timeouts"
    );
}
