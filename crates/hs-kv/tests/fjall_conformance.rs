//! The Fjall backend must pass the shared conformance suite, and must survive a process restart
//! (reopen after drop) without losing committed data.

use std::sync::atomic::{AtomicUsize, Ordering};

use bytes::Bytes;
use hs_kv::fjall_backend::FjallBackend;
use hs_kv::{KvBackend, KvRead, KvWrite, TransactConfig, transact};

#[test]
fn fjall_backend_passes_conformance_suite() {
    let root = tempfile::tempdir().expect("tempdir");
    let counter = AtomicUsize::new(0);
    hs_kv::conformance::run_conformance_suite(|| {
        let n = counter.fetch_add(1, Ordering::Relaxed);
        let path = root.path().join(format!("scenario-{n}"));
        FjallBackend::open(&path).expect("open fjall backend")
    });
}

#[test]
fn reopen_after_drop_preserves_committed_data() {
    let dir = tempfile::tempdir().expect("tempdir");

    {
        let backend = FjallBackend::open(dir.path()).expect("open");
        let ks = backend.keyspace("durable").expect("keyspace");
        transact(&backend, TransactConfig::default(), |txn| {
            txn.put(&ks, b"key-1", b"value-1")?;
            txn.put(&ks, b"key-2", b"value-2")?;
            txn.delete(&ks, b"key-2")?;
            Ok(())
        })
        .expect("commit");
        backend.persist_all().expect("explicit persist");
        // `backend` (and with it every Fjall handle) is dropped here. Fjall's own `Drop` impl
        // additionally tries to persist, so this test covers both the explicit and implicit path.
    }

    let reopened = FjallBackend::open(dir.path()).expect("reopen");
    let ks = reopened.keyspace("durable").expect("keyspace");
    let snap = reopened.snapshot();
    assert_eq!(
        snap.get(&ks, b"key-1").unwrap(),
        Some(Bytes::from_static(b"value-1")),
        "a committed put must survive a reopen"
    );
    assert_eq!(
        snap.get(&ks, b"key-2").unwrap(),
        None,
        "a committed delete must also survive a reopen, not resurrect the value"
    );
}
