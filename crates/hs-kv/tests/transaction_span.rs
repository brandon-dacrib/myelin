//! A store transaction inside a recorded trace is a `kv.transaction` span named after the code
//! that asked, with its attempts, mutations and keyspaces, and its commit as a child.

use hs_kv::memory::MemoryBackend;
use hs_kv::{KvBackend, KvError, KvWrite, TransactConfig, transact};

#[test]
fn a_traced_transaction_says_what_it_wrote_and_where() {
    let backend = MemoryBackend::new();
    let things = backend.keyspace("things").unwrap();
    let others = backend.keyspace("others").unwrap();
    let (result, trace) = hs_telemetry::testing::trace_sync(|| {
        transact(&backend, TransactConfig::default(), |txn| {
            txn.put(&things, b"a", b"1")?;
            txn.put(&others, b"b", b"2")?;
            txn.delete(&things, b"c")?;
            Ok(())
        })
    });
    result.unwrap();
    let txn = trace
        .0
        .iter()
        .find(|s| s.name.starts_with("kv "))
        .unwrap_or_else(|| panic!("no transaction span in {:?}", trace.names()));
    assert_eq!(
        txn.name,
        "kv transaction_span::a_traced_transaction_says_what_it_wrote_and_where"
    );
    assert_eq!(txn.attr("mutations"), Some("3"));
    assert_eq!(txn.attr("attempts"), Some("1"));
    let mut keyspaces: Vec<&str> = txn.attr("keyspaces").unwrap().split(',').collect();
    keyspaces.sort_unstable();
    assert_eq!(keyspaces, ["others", "things"]);
    let commit = trace.find("kv.commit").expect("the commit's span");
    assert!(trace.is_under(commit, txn));
    assert!(!txn.failed);
}

#[test]
fn a_failed_transaction_is_a_failed_span() {
    let backend = MemoryBackend::new();
    let (result, trace) = hs_telemetry::testing::trace_sync(|| {
        transact(&backend, TransactConfig::default(), |_txn| {
            Err::<(), _>(KvError::RetriesExhausted { attempts: 0 })
        })
    });
    assert!(result.is_err());
    let txn = trace.0.iter().find(|s| s.name.starts_with("kv ")).unwrap();
    assert!(txn.failed);
    assert!(txn.attr("error.message").is_some());
    assert!(trace.find("kv.commit").is_none(), "nothing to commit");
}

#[test]
fn outside_a_trace_a_transaction_makes_no_span() {
    let backend = MemoryBackend::new();
    let things = backend.keyspace("things").unwrap();
    // Installs the recording subscriber, then runs a transaction with no span around it.
    let _ = hs_telemetry::testing::trace_sync(|| ());
    transact(&backend, TransactConfig::default(), |txn| {
        txn.put(&things, b"untraced", b"1")
    })
    .unwrap();
    let rooted = hs_telemetry::testing::traces_rooted_at(
        "kv transaction_span::outside_a_trace_a_transaction_makes_no_span",
    );
    assert!(rooted.is_empty(), "{rooted:?}");
}
