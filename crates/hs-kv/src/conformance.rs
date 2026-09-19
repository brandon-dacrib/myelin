//! A backend-agnostic conformance suite. Every [`crate::KvBackend`] implementation must pass this
//! suite; it is the executable version of the crate-level semantic contract, not just a smoke
//! test. Run it against a fresh backend with [`run_conformance_suite`].
//!
//! ```
//! use hs_kv::conformance::run_conformance_suite;
//! use hs_kv::memory::MemoryBackend;
//!
//! run_conformance_suite(MemoryBackend::new);
//! ```

use std::ops::Bound;
use std::time::Duration;

use bytes::Bytes;

use crate::error::Conflict;
use crate::retry::{TransactConfig, transact};
use crate::traits::{KvBackend, KvRead, KvWrite, RangeSpec};
use crate::watch::WatchOutcome;

fn b(s: &str) -> Bytes {
    Bytes::copy_from_slice(s.as_bytes())
}

/// Runs every scenario in the suite against a freshly constructed backend per scenario (`make` is
/// called once per scenario, never shared, so scenarios cannot interfere with each other).
///
/// Panics (via `assert!`) on the first violated guarantee, naming the scenario in the assertion
/// message. Each scenario function below is also individually `pub`, so a backend that is known
/// to diverge from one specific scenario (see `docs/status/01-storage-engine.md`'s PostgreSQL
/// entry for the one known case, a phantom-range guarantee stronger than true serializability)
/// can still report an honest per-scenario breakdown instead of a single all-or-nothing result
/// that stops at the first failure.
pub fn run_conformance_suite<B: KvBackend>(make: impl Fn() -> B) {
    get_put_delete_roundtrip(make());
    multi_get_preserves_order_and_absence(make());
    range_boundaries_inclusive_exclusive_reverse_limit(make());
    snapshot_visibility_is_repeatable_read(make());
    lost_update_is_prevented(make());
    write_skew_is_prevented(make());
    phantom_insert_inside_a_scanned_range_conflicts(make());
    phantom_insert_outside_a_scanned_range_does_not_conflict(make());
    atomic_add_under_contention(make());
    watch_wakes_on_write_and_times_out_otherwise(make());
    read_only_transactions_never_conflict(make());
}

/// Basic get/put/delete round-trip, including that an empty value is present and distinct from absence.
pub fn get_put_delete_roundtrip<B: KvBackend>(backend: B) {
    let ks = backend.keyspace("t").expect("keyspace");

    assert_eq!(
        backend.snapshot().get(&ks, b"missing").unwrap(),
        None,
        "absent key reads as None"
    );

    transact(&backend, TransactConfig::default(), |txn| {
        txn.put(&ks, b"a", b"1")?;
        txn.put(&ks, b"empty", b"")?;
        Ok(())
    })
    .expect("put commits");

    assert_eq!(backend.snapshot().get(&ks, b"a").unwrap(), Some(b("1")));
    assert_eq!(
        backend.snapshot().get(&ks, b"empty").unwrap(),
        Some(Bytes::new()),
        "an empty value is a valid, present value, distinct from absence"
    );

    transact(&backend, TransactConfig::default(), |txn| {
        txn.delete(&ks, b"a")?;
        Ok(())
    })
    .expect("delete commits");
    assert_eq!(
        backend.snapshot().get(&ks, b"a").unwrap(),
        None,
        "delete removes the key"
    );

    transact(&backend, TransactConfig::default(), |txn| {
        txn.delete(&ks, b"never-existed")
    })
    .expect("deleting an absent key is not an error");
}

/// `multi_get` preserves input order and length, with `None` for absent keys.
pub fn multi_get_preserves_order_and_absence<B: KvBackend>(backend: B) {
    let ks = backend.keyspace("t").expect("keyspace");
    transact(&backend, TransactConfig::default(), |txn| {
        txn.put(&ks, b"a", b"A")?;
        txn.put(&ks, b"c", b"C")?;
        Ok(())
    })
    .unwrap();

    let snap = backend.snapshot();
    let keys: Vec<&[u8]> = vec![b"c", b"missing", b"a"];
    let got = snap.multi_get(&ks, &keys).unwrap();
    assert_eq!(
        got,
        vec![Some(b("C")), None, Some(b("A"))],
        "multi_get preserves input order and length"
    );
}

/// Range scan boundary handling: inclusive/exclusive bounds, reverse order, and `limit`.
pub fn range_boundaries_inclusive_exclusive_reverse_limit<B: KvBackend>(backend: B) {
    let ks = backend.keyspace("t").expect("keyspace");
    transact(&backend, TransactConfig::default(), |txn| {
        for k in ["a", "b", "c", "d", "e"] {
            txn.put(&ks, k.as_bytes(), k.as_bytes())?;
        }
        Ok(())
    })
    .unwrap();

    let snap = backend.snapshot();
    let collect = |spec: RangeSpec| -> Vec<String> {
        snap.range(&ks, spec)
            .map(|r| String::from_utf8(r.unwrap().0.to_vec()).unwrap())
            .collect()
    };

    assert_eq!(
        collect(RangeSpec::new(
            Bound::Included(b("b")),
            Bound::Included(b("d"))
        )),
        vec!["b", "c", "d"],
        "inclusive end includes the boundary key"
    );
    assert_eq!(
        collect(RangeSpec::new(
            Bound::Included(b("b")),
            Bound::Excluded(b("d"))
        )),
        vec!["b", "c"],
        "exclusive end excludes the boundary key"
    );
    assert_eq!(
        collect(RangeSpec::new(
            Bound::Excluded(b("b")),
            Bound::Included(b("d"))
        )),
        vec!["c", "d"],
        "exclusive start excludes the boundary key"
    );
    assert_eq!(
        collect(RangeSpec::full()),
        vec!["a", "b", "c", "d", "e"],
        "unbounded range visits everything"
    );
    assert_eq!(
        collect(RangeSpec::full().reverse()),
        vec!["e", "d", "c", "b", "a"],
        "reverse visits from the high end down"
    );
    assert_eq!(
        collect(RangeSpec::full().limit(2)),
        vec!["a", "b"],
        "limit truncates a forward scan"
    );
    assert_eq!(
        collect(RangeSpec::full().reverse().limit(2)),
        vec!["e", "d"],
        "limit truncates a reverse scan from its starting end"
    );
    assert_eq!(
        collect(RangeSpec::new(Bound::Included(b("f")), Bound::Unbounded)),
        Vec::<String>::new(),
        "a range past the end is empty, not an error"
    );
}

/// A snapshot's view is fixed at the instant it is taken, unaffected by later commits.
pub fn snapshot_visibility_is_repeatable_read<B: KvBackend>(backend: B) {
    let ks = backend.keyspace("t").expect("keyspace");
    transact(&backend, TransactConfig::default(), |txn| {
        txn.put(&ks, b"k", b"before")
    })
    .unwrap();

    let snap = backend.snapshot();
    assert_eq!(snap.get(&ks, b"k").unwrap(), Some(b("before")));

    transact(&backend, TransactConfig::default(), |txn| {
        txn.put(&ks, b"k", b"after")
    })
    .unwrap();

    assert_eq!(
        snap.get(&ks, b"k").unwrap(),
        Some(b("before")),
        "a snapshot taken before a write must not see it, no matter when it is read"
    );
    assert_eq!(
        backend.snapshot().get(&ks, b"k").unwrap(),
        Some(b("after")),
        "a fresh snapshot sees the write"
    );
}

/// Two transactions read-modify-write the same key; the second committer must conflict, not silently overwrite.
pub fn lost_update_is_prevented<B: KvBackend>(backend: B) {
    let ks = backend.keyspace("t").expect("keyspace");
    transact(&backend, TransactConfig::default(), |txn| {
        txn.put(&ks, b"counter", b"0")
    })
    .unwrap();

    // Two transactions both read the same counter, then each tries to write it based on that
    // stale read (the read-modify-write anti-pattern SSI must block).
    let mut tx1 = backend.begin().unwrap();
    let mut tx2 = backend.begin().unwrap();

    let v1 = tx1.get(&ks, b"counter").unwrap();
    let v2 = tx2.get(&ks, b"counter").unwrap();
    assert_eq!(v1, v2);

    tx1.put(&ks, b"counter", b"1").unwrap();
    tx2.put(&ks, b"counter", b"1").unwrap();

    assert_eq!(
        backend.commit(tx1).unwrap(),
        Ok(()),
        "the first committer wins"
    );
    assert_eq!(
        backend.commit(tx2).unwrap(),
        Err(Conflict),
        "the second transaction read a key the first one wrote, so it must conflict, not silently \
         overwrite and lose the first update"
    );
}

/// The classic two-key write-skew anomaly must be rejected by SSI, not allowed through as under plain snapshot isolation.
pub fn write_skew_is_prevented<B: KvBackend>(backend: B) {
    // Classic write-skew setup: an invariant over two keys (on-call doctors, account balances,
    // ...) that neither transaction violates in isolation but both together would.
    let ks = backend.keyspace("t").expect("keyspace");
    transact(&backend, TransactConfig::default(), |txn| {
        txn.put(&ks, b"a", b"10")?;
        txn.put(&ks, b"b", b"10")?;
        Ok(())
    })
    .unwrap();

    let mut tx1 = backend.begin().unwrap();
    let mut tx2 = backend.begin().unwrap();

    let _ = tx1.get(&ks, b"a").unwrap();
    let _ = tx1.get(&ks, b"b").unwrap();
    let _ = tx2.get(&ks, b"a").unwrap();
    let _ = tx2.get(&ks, b"b").unwrap();

    tx1.put(&ks, b"a", b"0").unwrap();
    tx2.put(&ks, b"b", b"0").unwrap();

    assert_eq!(backend.commit(tx1).unwrap(), Ok(()));
    assert_eq!(
        backend.commit(tx2).unwrap(),
        Err(Conflict),
        "tx2 read a key tx1 wrote (each read both a and b), so SSI must reject the write skew \
         rather than let both commit and violate the invariant"
    );
}

/// A key inserted into a range a still-open transaction scanned is a phantom; that transaction's commit must conflict.
pub fn phantom_insert_inside_a_scanned_range_conflicts<B: KvBackend>(backend: B) {
    let ks = backend.keyspace("t").expect("keyspace");

    let mut reader = backend.begin().unwrap();
    let seen: Vec<_> = reader.range(&ks, RangeSpec::prefix(b("order/"))).collect();
    assert!(seen.is_empty());
    reader.put(&ks, b"unrelated", b"v").unwrap();

    let mut writer = backend.begin().unwrap();
    writer.put(&ks, b"order/1", b"v").unwrap();
    assert_eq!(backend.commit(writer).unwrap(), Ok(()));

    assert_eq!(
        backend.commit(reader).unwrap(),
        Err(Conflict),
        "a key inserted into a range the reader scanned is a phantom; SSI must conflict"
    );
}

/// A write outside every range a transaction scanned must not cause that transaction's commit to conflict.
pub fn phantom_insert_outside_a_scanned_range_does_not_conflict<B: KvBackend>(backend: B) {
    let ks = backend.keyspace("t").expect("keyspace");

    let mut reader = backend.begin().unwrap();
    let _: Vec<_> = reader.range(&ks, RangeSpec::prefix(b("order/"))).collect();
    reader.put(&ks, b"unrelated", b"v").unwrap();

    let mut writer = backend.begin().unwrap();
    writer.put(&ks, b"invoice/1", b"v").unwrap();
    assert_eq!(backend.commit(writer).unwrap(), Ok(()));

    assert_eq!(
        backend.commit(reader).unwrap(),
        Ok(()),
        "a write outside every range the reader scanned must not conflict"
    );
}

/// `atomic_add` under real concurrent contention: every increment from every thread lands exactly once.
pub fn atomic_add_under_contention<B: KvBackend>(backend: B) {
    let ks = backend.keyspace("t").expect("keyspace");
    let threads = 8usize;
    let per_thread = 25i64;

    std::thread::scope(|scope| {
        for _ in 0..threads {
            let backend = backend.clone();
            let ks = ks.clone();
            scope.spawn(move || {
                for _ in 0..per_thread {
                    transact(&backend, TransactConfig::default(), |txn| {
                        txn.atomic_add(&ks, b"counter", 1)
                    })
                    .expect("atomic_add eventually commits under the retry budget");
                }
            });
        }
    });

    let total = i64::try_from(threads).unwrap() * per_thread;
    let final_value = backend
        .snapshot()
        .get(&ks, b"counter")
        .unwrap()
        .expect("counter exists");
    let arr: [u8; 8] = final_value.as_ref().try_into().unwrap();
    assert_eq!(
        i64::from_be_bytes(arr),
        total,
        "every increment from every thread must land exactly once, no lost updates"
    );
}

/// A watch fires on a write to its key and times out otherwise.
pub fn watch_wakes_on_write_and_times_out_otherwise<B: KvBackend>(backend: B) {
    let ks = backend.keyspace("t").expect("keyspace");
    let mut idle = backend.watch(&ks, b"idle-key");
    assert_eq!(
        idle.wait(Duration::from_millis(50)),
        WatchOutcome::Timeout,
        "no write, no wake-up"
    );

    let mut watched = backend.watch(&ks, b"watched-key");
    let writer_backend = backend.clone();
    let ks2 = ks.clone();
    let handle = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(20));
        transact(&writer_backend, TransactConfig::default(), |txn| {
            txn.put(&ks2, b"watched-key", b"v")
        })
        .unwrap();
    });

    assert_eq!(
        watched.wait(Duration::from_secs(5)),
        WatchOutcome::Changed,
        "a write to the watched key must wake a waiter"
    );
    handle.join().unwrap();
}

/// A transaction with no writes has nothing to protect and must always commit successfully.
pub fn read_only_transactions_never_conflict<B: KvBackend>(backend: B) {
    let ks = backend.keyspace("t").expect("keyspace");
    transact(&backend, TransactConfig::default(), |txn| {
        txn.put(&ks, b"k", b"v")
    })
    .unwrap();

    let reader = backend.begin().unwrap();
    let _ = reader.get(&ks, b"k").unwrap();

    transact(&backend, TransactConfig::default(), |txn| {
        txn.put(&ks, b"k", b"v2")
    })
    .unwrap();

    assert_eq!(
        backend.commit(reader).unwrap(),
        Ok(()),
        "a transaction with no writes has nothing to protect and must always succeed, even if its \
         reads are now stale"
    );
}
