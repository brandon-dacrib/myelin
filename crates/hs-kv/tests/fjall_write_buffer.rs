//! The Fjall write-buffer cap (RFC 0024, `hs_kv::fjall_backend` module docs): a backend over its
//! cap rotates memtables so Fjall flushes them, the cap is what `FjallBackend::open` says it is,
//! and every key reads back its latest value across the forced flushes, before and after a
//! reopen. The long run that shows resident memory flat under the cap is
//! `tests/fjall_write_buffer_soak.rs` (ignored).

use std::time::{Duration, Instant};

use hs_kv::fjall_backend::{
    FJALL_MEMTABLE_SIZE, FJALL_WRITE_BUFFER_CAP, FjallBackend, FjallOptions,
};
use hs_kv::{KvBackend, KvRead, KvWrite, RangeSpec};

const KEYSPACES: usize = 20;
const VALUE_BYTES: usize = 4 * 1024;

fn keyspace_name(i: usize) -> String {
    format!("hs_test.table_{i:03}")
}

/// Rewrites one key in each of `KEYSPACES` keyspaces, `rounds` times, with a value that names
/// the round. Returns the last value written.
fn rewrite_rounds(backend: &FjallBackend, rounds: usize) -> Vec<u8> {
    let keyspaces: Vec<_> = (0..KEYSPACES)
        .map(|i| backend.keyspace(&keyspace_name(i)).expect("keyspace"))
        .collect();
    let mut last = Vec::new();
    for round in 0..rounds {
        for ks in &keyspaces {
            let mut value = format!("round {round} ").into_bytes();
            value.resize(VALUE_BYTES, b'v');
            let mut txn = backend.begin().expect("begin");
            txn.put(ks, b"the-key", &value).expect("put");
            backend.commit(txn).expect("commit").expect("no conflict");
            last = value;
        }
    }
    last
}

/// Waits until the write buffer is at or under `bytes`, as flushes complete, or panics after ten
/// seconds. The cap is checked after a commit, so with `nudge` set and nothing sealed this keeps
/// committing that value to the first keyspace again, as a periodic writer would, which changes
/// no value.
fn wait_for_write_buffer_at_most(backend: &FjallBackend, bytes: u64, nudge: Option<&[u8]>) {
    let deadline = Instant::now() + Duration::from_secs(10);
    let ks = backend.keyspace(&keyspace_name(0)).expect("keyspace");
    loop {
        let stats = backend.write_buffer_stats();
        if stats.bytes <= bytes {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "the write buffer did not come down to {bytes} bytes: {stats:?}"
        );
        if let Some(last) = nudge.filter(|_| stats.sealed_memtables == 0) {
            let mut txn = backend.begin().expect("begin");
            txn.put(&ks, b"the-key", last).expect("put");
            backend.commit(txn).expect("commit").expect("no conflict");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn assert_every_key_reads_back(backend: &FjallBackend, expected: &[u8]) {
    let snapshot = backend.snapshot();
    for i in 0..KEYSPACES {
        let ks = backend.keyspace(&keyspace_name(i)).expect("keyspace");
        let got = snapshot.get(&ks, b"the-key").expect("get");
        assert_eq!(got.as_deref(), Some(expected), "keyspace {i}");
        let keys: Vec<_> = snapshot
            .range(&ks, RangeSpec::full())
            .map(|item| item.expect("item").0)
            .collect();
        assert_eq!(keys, vec![bytes::Bytes::from_static(b"the-key")]);
    }
}

#[test]
fn open_applies_the_default_cap_and_memtable_size() {
    let dir = tempfile::tempdir().expect("tempdir");
    let backend = FjallBackend::open(dir.path()).expect("open");
    assert_eq!(backend.options(), FjallOptions::default());
    let stats = backend.write_buffer_stats();
    assert_eq!(stats.cap, Some(FJALL_WRITE_BUFFER_CAP));
    assert_eq!(
        FjallOptions::default().max_memtable_size,
        FJALL_MEMTABLE_SIZE
    );
    const { assert!(FJALL_MEMTABLE_SIZE < FJALL_WRITE_BUFFER_CAP) };
    assert_eq!(stats.rotations, 0);
    assert_eq!(stats.bytes, 0);
}

#[test]
fn a_write_buffer_over_the_cap_is_rotated_and_flushed_and_reads_stay_right() {
    let dir = tempfile::tempdir().expect("tempdir");
    // A 256 KiB cap, and a memtable size Fjall would never reach here, so every flush in this
    // test is one the cap caused.
    let options = FjallOptions {
        write_buffer_cap: Some(256 * 1024),
        max_memtable_size: 64 * 1024 * 1024,
    };
    let backend = FjallBackend::open_with_options(dir.path(), options).expect("open");
    assert_eq!(backend.options(), options);

    // 20 keyspaces x 30 rounds x 4 KiB = 2.4 MiB written, about ten times the cap.
    let last = rewrite_rounds(&backend, 30);
    let stats = backend.write_buffer_stats();
    assert!(
        stats.rotations >= 1,
        "the cap never rotated a memtable: {stats:?}"
    );
    // One shared Fjall keyspace behind every table: at most one memtable awaits a flush at a
    // time, and a crossing while it does is skipped rather than rotated again.
    assert!(stats.sealed_memtables <= 1, "{stats:?}");
    wait_for_write_buffer_at_most(&backend, 256 * 1024, Some(&last));
    assert_every_key_reads_back(&backend, &last);

    // The tables Fjall wrote survive a reopen; so does whatever was still in the memtable.
    drop(backend);
    let reopened = FjallBackend::open_with_options(dir.path(), options).expect("reopen");
    assert_every_key_reads_back(&reopened, &last);
    assert_eq!(reopened.write_buffer_stats().rotations, 0);
}

#[test]
fn a_small_memtable_size_makes_fjall_flush_on_its_own() {
    let dir = tempfile::tempdir().expect("tempdir");
    // No cap of ours: only the keyspace's own `max_memtable_size`, set at creation.
    let options = FjallOptions {
        write_buffer_cap: None,
        max_memtable_size: 128 * 1024,
    };
    let backend = FjallBackend::open_with_options(dir.path(), options).expect("open");
    assert_eq!(backend.write_buffer_stats().cap, None);
    let last = rewrite_rounds(&backend, 20);
    // Fjall rotates on its own worker once an insert takes the memtable over the size; the
    // backend requested none.
    wait_for_write_buffer_at_most(&backend, 128 * 1024, Some(&last));
    assert_eq!(backend.write_buffer_stats().rotations, 0);
    assert_every_key_reads_back(&backend, &last);
}

#[test]
fn flush_memtables_empties_the_write_buffer_on_demand() {
    let dir = tempfile::tempdir().expect("tempdir");
    let backend = FjallBackend::open(dir.path()).expect("open");
    let last = rewrite_rounds(&backend, 2);
    assert!(backend.write_buffer_stats().bytes > 0);
    assert_eq!(backend.flush_memtables().expect("flush"), 1);
    wait_for_write_buffer_at_most(&backend, 0, None);
    // Nothing sealed is left, and a second call has nothing to rotate.
    assert_eq!(backend.write_buffer_stats().sealed_memtables, 0);
    assert_eq!(backend.flush_memtables().expect("flush"), 0);
    assert_every_key_reads_back(&backend, &last);
}
