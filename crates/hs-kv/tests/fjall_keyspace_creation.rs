//! What a first boot pays for its keyspaces on the Fjall backend, and that the layout which makes
//! it cheap (every `hs-kv` keyspace behind a prefix in one shared Fjall keyspace) keeps keyspaces
//! apart, keeps old data directories readable, and survives a crash right after a first boot.
//!
//! A server opens about eighty keyspaces at boot. When each one was its own Fjall keyspace, a
//! fresh data directory paid for eighty Fjall keyspace creations, one after another under
//! Fjall's keyspace lock, each a handful of fsyncs: about five seconds of a first boot
//! (`docs/status/01-storage-engine.md`, "The cold boot measured").
//!
//! Regression guards: `a_fresh_store_creates_one_fjall_keyspace_however_many_keyspaces_open`
//! (fails on the per-table layout, which created one Fjall keyspace, and one directory, per
//! table), and the isolation, legacy and crash tests. Timing only (ignored, run by hand):
//! `opening_a_boots_worth_of_keyspaces_over_an_empty_directory_is_fast`.

use std::ops::Bound;
use std::path::Path;
use std::time::Instant;

use bytes::Bytes;
use hs_kv::fjall_backend::{FjallBackend, SHARED_KEYSPACE};
use hs_kv::{KvBackend, KvRead, KvWrite, RangeSpec, TransactConfig, transact};

/// About what `hs serve` opens at boot (77 on 2026-09-27, more since).
const BOOT_KEYSPACES: usize = 90;

fn keyspace_name(i: usize) -> String {
    format!("hs_test.table_{i:03}")
}

/// The Fjall keyspace directories under a data directory (Fjall's own meta keyspace, `0`,
/// included).
fn fjall_keyspace_dirs(path: &Path) -> usize {
    std::fs::read_dir(path.join("keyspaces"))
        .expect("keyspaces directory")
        .count()
}

fn keys(iter: impl Iterator<Item = hs_kv::RangeItem>) -> Vec<Bytes> {
    iter.map(|item| item.expect("range item").0).collect()
}

#[test]
fn a_fresh_store_creates_one_fjall_keyspace_however_many_keyspaces_open() {
    let dir = tempfile::tempdir().expect("tempdir");
    let backend = FjallBackend::open(dir.path()).expect("open");
    assert!(backend.created_fresh());
    for i in 0..BOOT_KEYSPACES {
        backend.keyspace(&keyspace_name(i)).expect("keyspace");
    }
    assert_eq!(backend.keyspaces_opened(), BOOT_KEYSPACES);
    assert_eq!(
        backend.fjall_keyspaces_created(),
        1,
        "only the shared keyspace is created"
    );
    // On disk too: Fjall's meta keyspace and the shared one, not one directory per table.
    assert_eq!(fjall_keyspace_dirs(dir.path()), 2);
    drop(backend);

    let reopened = FjallBackend::open(dir.path()).expect("reopen");
    assert!(!reopened.created_fresh());
    for i in 0..BOOT_KEYSPACES {
        reopened.keyspace(&keyspace_name(i)).expect("keyspace");
    }
    assert_eq!(reopened.fjall_keyspaces_created(), 0);
    assert_eq!(fjall_keyspace_dirs(dir.path()), 2);
}

/// Keyspaces whose names are prefixes of one another ("t", "t2", "tt") share the Fjall keyspace
/// but never each other's keys, in point reads, ranges in both directions with and without
/// bounds, prefix scans, or a transaction's conflict tracking.
#[test]
fn keyspaces_in_the_shared_fjall_keyspace_are_independent() {
    let dir = tempfile::tempdir().expect("tempdir");
    let backend = FjallBackend::open(dir.path()).expect("open");
    let t = backend.keyspace("t").expect("t");
    let t2 = backend.keyspace("t2").expect("t2");
    let tt = backend.keyspace("tt").expect("tt");
    transact(&backend, TransactConfig::default(), |txn| {
        for k in ["a", "b", "c", "\u{7f}"] {
            txn.put(&t, k.as_bytes(), b"t")?;
        }
        txn.put(&t2, b"a", b"t2")?;
        txn.put(&t2, b"\x00", b"t2")?;
        txn.put(&tt, b"a", b"tt")?;
        txn.put(&tt, &[0xff, 0xff], b"tt")?;
        Ok(())
    })
    .expect("seed");

    let snap = backend.snapshot();
    assert_eq!(snap.get(&t, b"a").unwrap(), Some(Bytes::from_static(b"t")));
    assert_eq!(
        snap.get(&t2, b"a").unwrap(),
        Some(Bytes::from_static(b"t2"))
    );
    assert_eq!(snap.get(&t, b"\x00").unwrap(), None);
    assert_eq!(snap.get(&t2, b"b").unwrap(), None);
    assert_eq!(
        keys(snap.range(&t, RangeSpec::full())),
        vec![b"a".as_slice(), b"b", b"c", b"\x7f"]
    );
    assert_eq!(
        keys(snap.range(&t, RangeSpec::full().reverse())),
        vec![b"\x7f".as_slice(), b"c", b"b", b"a"]
    );
    assert_eq!(
        keys(snap.range(&t2, RangeSpec::full())),
        vec![b"\x00".as_slice(), b"a"]
    );
    assert_eq!(
        keys(snap.range(&tt, RangeSpec::full().reverse().limit(1))),
        vec![Bytes::from_static(&[0xff, 0xff])]
    );
    assert_eq!(
        keys(snap.range(
            &t,
            RangeSpec::new(Bound::Excluded(Bytes::from_static(b"a")), Bound::Unbounded)
        )),
        vec![b"b".as_slice(), b"c", b"\x7f"]
    );
    assert_eq!(
        keys(snap.range(
            &t,
            RangeSpec::new(Bound::Unbounded, Bound::Included(Bytes::from_static(b"b"))).reverse()
        )),
        vec![b"b".as_slice(), b"a"]
    );
    assert_eq!(
        keys(snap.range(&t, RangeSpec::prefix("c"))),
        vec![b"c".as_slice()]
    );
    assert!(keys(snap.range(&t2, RangeSpec::prefix("b"))).is_empty());

    // A transaction that scanned all of `t` is not disturbed by a write to `t2` or `tt`, and is
    // by a write inside `t`.
    let mut scanner = backend.begin().expect("begin");
    assert_eq!(keys(scanner.range(&t, RangeSpec::full())).len(), 4);
    scanner.put(&t, b"z", b"t").expect("put");
    transact(&backend, TransactConfig::default(), |txn| {
        txn.put(&t2, b"b", b"t2")?;
        txn.put(&tt, b"0", b"tt")
    })
    .expect("write elsewhere");
    assert!(
        backend.commit(scanner).expect("commit").is_ok(),
        "writes to other keyspaces must not conflict with a scan of this one"
    );
    let mut scanner = backend.begin().expect("begin");
    assert_eq!(keys(scanner.range(&t, RangeSpec::full())).len(), 5);
    scanner.put(&t, b"y", b"t").expect("put");
    transact(&backend, TransactConfig::default(), |txn| {
        txn.put(&t, b"phantom", b"t")
    })
    .expect("write inside");
    assert!(
        backend.commit(scanner).expect("commit").is_err(),
        "a write inside the scanned keyspace is a phantom"
    );
}

#[test]
fn the_shared_keyspace_name_and_overlong_keys_are_refused() {
    let dir = tempfile::tempdir().expect("tempdir");
    let backend = FjallBackend::open(dir.path()).expect("open");
    assert!(matches!(
        backend.keyspace(SHARED_KEYSPACE),
        Err(hs_kv::KvError::InvalidKeyspaceName(_))
    ));
    let ks = backend.keyspace("long").expect("keyspace");
    let mut txn = backend.begin().expect("begin");
    let too_long = vec![b'k'; usize::from(u16::MAX)];
    assert!(matches!(
        txn.put(&ks, &too_long, b"v"),
        Err(hs_kv::KvError::KeyTooLarge { .. })
    ));
    assert_eq!(txn.get(&ks, &too_long).expect("get"), None);
    let longest = vec![b'k'; usize::from(u16::MAX) - 1 - "long".len()];
    txn.put(&ks, &longest, b"v")
        .expect("the longest key that fits");
    assert!(backend.commit(txn).expect("commit").is_ok());
    assert_eq!(
        backend.snapshot().get(&ks, &longest).expect("get"),
        Some(Bytes::from_static(b"v"))
    );
}

/// A data directory written by the per-table layout (each keyspace its own Fjall keyspace,
/// unprefixed) is read where it is, and a keyspace it did not have goes into the shared one.
#[test]
fn a_store_written_one_fjall_keyspace_per_table_is_still_read() {
    let dir = tempfile::tempdir().expect("tempdir");
    {
        let db = fjall::OptimisticTxDatabase::builder(dir.path())
            .open()
            .expect("fjall");
        let users = db
            .keyspace("hs_auth.users", fjall::KeyspaceCreateOptions::default)
            .expect("keyspace");
        let mut tx = db.write_tx().expect("tx");
        tx.insert(&users, "@alice:example.org", "alice");
        tx.insert(&users, "@bob:example.org", "bob");
        tx.commit().expect("commit").expect("no conflict");
        db.persist(fjall::PersistMode::SyncAll).expect("persist");
    }

    let backend = FjallBackend::open(dir.path()).expect("open");
    assert!(!backend.created_fresh());
    let users = backend.keyspace("hs_auth.users").expect("users");
    let devices = backend.keyspace("hs_auth.devices").expect("devices");
    assert_eq!(backend.fjall_keyspaces_created(), 1, "the shared one only");
    assert_eq!(
        keys(backend.snapshot().range(&users, RangeSpec::full())),
        vec![
            Bytes::from_static(b"@alice:example.org"),
            Bytes::from_static(b"@bob:example.org")
        ]
    );
    transact(&backend, TransactConfig::default(), |txn| {
        txn.put(&users, b"@carol:example.org", b"carol")?;
        txn.put(&devices, b"@alice:example.org", b"DEVICE")
    })
    .expect("write");
    drop((users, devices, backend));

    let backend = FjallBackend::open(dir.path()).expect("reopen");
    let users = backend.keyspace("hs_auth.users").expect("users");
    let devices = backend.keyspace("hs_auth.devices").expect("devices");
    let snap = backend.snapshot();
    assert_eq!(keys(snap.range(&users, RangeSpec::full())).len(), 3);
    assert_eq!(
        keys(snap.range(&devices, RangeSpec::full())),
        vec![Bytes::from_static(b"@alice:example.org")]
    );
    assert_eq!(backend.fjall_keyspaces_created(), 0);
}

/// Set in the child process of the crash test: the data directory it boots over.
const CRASH_CHILD_DIR: &str = "HS_KV_CRASH_CHILD_DIR";

/// A process that opens a fresh store, opens a boot's worth of keyspaces, commits a write to
/// each and is then killed (`abort`, no destructor, no persist) leaves a store that reopens with
/// every write. The child is this same test binary, run again with [`CRASH_CHILD_DIR`] set.
#[test]
fn a_crash_right_after_a_first_boot_loses_no_committed_write() {
    if let Some(dir) = std::env::var_os(CRASH_CHILD_DIR) {
        let backend = FjallBackend::open(&dir).expect("open");
        for i in 0..BOOT_KEYSPACES {
            let ks = backend.keyspace(&keyspace_name(i)).expect("keyspace");
            transact(&backend, TransactConfig::default(), |txn| {
                txn.put(&ks, b"written", keyspace_name(i).as_bytes())
            })
            .expect("commit");
        }
        std::process::abort();
    }

    let dir = tempfile::tempdir().expect("tempdir");
    let status = std::process::Command::new(std::env::current_exe().expect("test binary"))
        .args([
            "--exact",
            "a_crash_right_after_a_first_boot_loses_no_committed_write",
            "--test-threads=1",
        ])
        .env(CRASH_CHILD_DIR, dir.path())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .expect("child");
    assert!(!status.success(), "the child must have been killed");

    let backend = FjallBackend::open(dir.path()).expect("reopen after the crash");
    let snap = backend.snapshot();
    for i in 0..BOOT_KEYSPACES {
        let ks = backend.keyspace(&keyspace_name(i)).expect("keyspace");
        assert_eq!(
            snap.get(&ks, b"written").expect("get"),
            Some(Bytes::from(keyspace_name(i))),
            "keyspace {i}"
        );
    }
    assert_eq!(backend.fjall_keyspaces_created(), 0);
}

/// Timing, not a regression guard: how long opening [`BOOT_KEYSPACES`] keyspaces over an empty
/// directory takes, against the same number of Fjall keyspaces of their own (the layout before
/// the shared keyspace). Run with `cargo test -p hs-kv --release --test fjall_keyspace_creation
/// -- --ignored --nocapture`; the numbers are in `docs/status/01-storage-engine.md`.
#[test]
#[ignore = "timing; run by hand, numbers in docs/status/01-storage-engine.md"]
fn opening_a_boots_worth_of_keyspaces_over_an_empty_directory_is_fast() {
    let per_table = tempfile::tempdir().expect("tempdir");
    let start = Instant::now();
    {
        let db = fjall::OptimisticTxDatabase::builder(per_table.path())
            .open()
            .expect("fjall");
        for i in 0..BOOT_KEYSPACES {
            db.keyspace(&keyspace_name(i), || {
                fjall::KeyspaceCreateOptions::default()
                    .with_kv_separation(Some(fjall::KvSeparationOptions::default()))
            })
            .expect("keyspace");
        }
    }
    let per_table_cold = start.elapsed();

    let dir = tempfile::tempdir().expect("tempdir");
    let start = Instant::now();
    let backend = FjallBackend::open(dir.path()).expect("open");
    let opened = start.elapsed();
    for i in 0..BOOT_KEYSPACES {
        backend.keyspace(&keyspace_name(i)).expect("keyspace");
    }
    let cold = start.elapsed();
    drop(backend);

    let start = Instant::now();
    let backend = FjallBackend::open(dir.path()).expect("reopen");
    for i in 0..BOOT_KEYSPACES {
        backend.keyspace(&keyspace_name(i)).expect("keyspace");
    }
    let warm = start.elapsed();
    println!(
        "{BOOT_KEYSPACES} keyspaces: one Fjall keyspace each, cold {per_table_cold:?}; shared: \
         database open {opened:?}, cold {cold:?}, warm {warm:?}"
    );
    assert!(
        cold.as_secs_f64() < 1.0,
        "a cold open of {BOOT_KEYSPACES} keyspaces took {cold:?}"
    );
}
