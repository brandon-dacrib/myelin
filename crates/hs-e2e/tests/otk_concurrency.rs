//! The property test the atomic one-time-key claim exists to pass: under real concurrent
//! access, from real OS threads, two claimants can never receive the same key. Run against both
//! the in-memory and the embedded (Fjall) backends, in the style
//! `crates/hs-kv/src/conformance.rs::atomic_add_under_contention` uses for its own concurrency
//! proof (`std::thread::scope`, no tokio task scheduling in the way) — see
//! `crates/hs-e2e/src/store/mod.rs`'s module docs for why this is expected to hold: it follows
//! directly from `hs-kv`'s serializable snapshot isolation, not from any lock this crate adds.
//!
//! A double claim here is exactly the bug class this crate exists to make impossible: two
//! clients that both believe they hold the same one-time key produce a session neither peer can
//! actually decrypt.

use std::collections::{BTreeMap, HashSet};
use std::sync::Arc;

use hs_e2e::store::OneTimeKeyStore;
use hs_e2e::store::tables::TablesE2eStore;
use hs_kv::KvBackend;
use ruma::{OwnedDeviceId, OwnedUserId};

fn user() -> OwnedUserId {
    ruma::user_id!("@concurrency:example.org").to_owned()
}

fn device() -> OwnedDeviceId {
    ruma::OwnedDeviceId::from("CONC1")
}

/// Uploads `total` one-time keys for one device, then spawns `total` OS threads that each
/// attempt exactly one claim, all racing against the same backend at once. Asserts every claim
/// is unique, the union of claimed key ids is exactly the uploaded set, and nothing is left
/// behind afterward.
fn assert_no_double_claim<B: KvBackend>(backend: B, total: usize) {
    let store = Arc::new(TablesE2eStore::open(backend).expect("open store"));
    let user = user();
    let device = device();

    let mut uploaded = BTreeMap::new();
    for i in 0..total {
        uploaded.insert(
            format!("signed_curve25519:K{i}"),
            serde_json::json!({"key": format!("key-{i}")}),
        );
    }
    let expected_ids: HashSet<String> = (0..total).map(|i| format!("K{i}")).collect();

    futures::executor::block_on(store.upload_one_time_keys(&user, &device, uploaded))
        .expect("upload succeeds");

    let claims: Vec<Option<(String, serde_json::Value)>> = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..total)
            .map(|_| {
                let store = Arc::clone(&store);
                let user = user.clone();
                let device = device.clone();
                scope.spawn(move || {
                    futures::executor::block_on(store.claim_one_time_key(
                        &user,
                        &device,
                        "signed_curve25519",
                    ))
                    .expect("claim does not error, only conflicts-and-retries internally")
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|h| h.join().expect("claiming thread panicked"))
            .collect()
    });

    let mut claimed_ids = HashSet::new();
    for claim in claims {
        let (key_id, _value) =
            claim.expect("as many claimants as keys, so every thread must get one");
        assert!(
            claimed_ids.insert(key_id.clone()),
            "key {key_id} was handed out to more than one claimant -- a double claim"
        );
    }
    assert_eq!(
        claimed_ids, expected_ids,
        "every uploaded key must be claimed exactly once, no more, no fewer"
    );

    let remaining = futures::executor::block_on(store.count_one_time_keys(&user, &device))
        .expect("count succeeds");
    assert!(
        remaining.is_empty(),
        "no one-time keys should remain once every key has been claimed"
    );
}

#[test]
fn no_double_claim_under_concurrency_in_memory_backend() {
    assert_no_double_claim(hs_kv::memory::MemoryBackend::new(), 32);
}

#[test]
fn no_double_claim_under_concurrency_fjall_backend() {
    let dir = tempfile::tempdir().expect("tempdir");
    let backend = hs_kv::fjall_backend::FjallBackend::open(dir.path()).expect("open fjall");
    assert_no_double_claim(backend, 16);
}

proptest::proptest! {
    #![proptest_config(proptest::prelude::ProptestConfig::with_cases(8))]

    /// The same property, swept over a range of claimant counts against the in-memory backend
    /// (kept off Fjall to avoid repeated disk-backed runs on a shared machine) -- a
    /// randomized-width version of the two fixed-size tests above.
    #[test]
    fn no_double_claim_holds_for_varying_thread_counts(total in 2usize..24) {
        assert_no_double_claim(hs_kv::memory::MemoryBackend::new(), total);
    }
}
