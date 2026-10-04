//! Disjoint keys of one small table, written concurrently, against a real PostgreSQL: the shape
//! of `hs-auth`'s request authentication, where every request reads its own access token's row
//! and writes it back (`last_used_ms`), and a few dozen tokens share one table page.
//!
//! PostgreSQL's SSI tracks reads of a small table at page or relation granularity (a sequential
//! scan, which the planner picks for a one-page table, takes a relation-level read lock), so two
//! such transactions on *different* keys still form a read/write cycle and one is cancelled as a
//! pivot. `hs_kv::transact` retries that; what it must not do is retry every party of the cycle
//! on the same schedule, or they meet again on every attempt until the budget is spent. That is
//! how a merge gate's `cluster_mirror` run on 2026-10-04 answered a send with a 500: the
//! PostgreSQL log showed two connections each cancelled ten times within a few milliseconds of
//! each other on `hs_auth.access_tokens`, the backoff having been a function of the attempt
//! number alone.
//!
//! Gated like `postgres_conformance.rs`: `HS_KV_TEST_POSTGRES_DSN` (default
//! `postgres://postgres:hskvtest@localhost:5433/postgres`), and `SKIP` when unreachable.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use hs_kv::postgres_backend::{PostgresBackend, PostgresOpenOptions};
use hs_kv::{KvBackend, KvError, KvRead, KvWrite, TransactConfig, transact};

fn open(dsn: &str, schema: &str, pool_size: u32) -> Result<PostgresBackend, KvError> {
    PostgresBackend::open_with(
        dsn,
        &PostgresOpenOptions {
            schema: schema.to_owned(),
            pool_size,
            ..PostgresOpenOptions::default()
        },
    )
}

fn reachable_dsn() -> Option<String> {
    let dsn = std::env::var("HS_KV_TEST_POSTGRES_DSN")
        .unwrap_or_else(|_| "postgres://postgres:hskvtest@localhost:5433/postgres".to_owned());
    match open(&dsn, "hs_kv_reachability_probe", 1) {
        Ok(_backend) => Some(dsn),
        Err(e) => {
            eprintln!(
                "SKIP: postgres_contention tests skipped, no PostgreSQL reachable at {dsn:?}: {e}"
            );
            None
        }
    }
}

/// Four threads, each authenticating "its own token" 100 times: read its row, write it back,
/// in one `transact` with the default configuration. Every one of the 400 must commit.
#[test]
fn concurrent_writers_of_disjoint_keys_in_a_small_table_all_commit() {
    let Some(dsn) = reachable_dsn() else {
        return;
    };
    const THREADS: usize = 4;
    const ROUNDS: usize = 100;
    let schema = format!("hs_kv_contention_{}", std::process::id());
    let backend = open(&dsn, &schema, THREADS as u32).expect("open");
    let tokens = backend.keyspace("tokens").expect("keyspace");
    // Thirty rows, as a small server's access tokens: one page.
    transact(&backend, TransactConfig::default(), |txn| {
        for i in 0..30u32 {
            txn.put(
                &tokens,
                format!("token{i:02}").as_bytes(),
                &0u64.to_be_bytes(),
            )?;
        }
        Ok(())
    })
    .expect("seed");
    {
        // So the planner knows the table is one page and scans it whole, as it does for a
        // small server's real tables once autovacuum has analysed them.
        let mut client = postgres::Client::connect(&dsn, postgres::NoTls).expect("admin connect");
        client
            .batch_execute(&format!("ANALYZE \"{schema}\".\"kv_tokens\""))
            .expect("analyze");
    }

    let exhausted = Arc::new(AtomicUsize::new(0));
    let threads: Vec<_> = (0..THREADS)
        .map(|t| {
            let backend = backend.clone();
            let tokens = tokens.clone();
            let exhausted = exhausted.clone();
            std::thread::spawn(move || {
                let key = format!("token{t:02}").into_bytes();
                for _ in 0..ROUNDS {
                    let result = transact(&backend, TransactConfig::default(), |txn| {
                        let used = txn
                            .get(&tokens, &key)?
                            .map(|v| u64::from_be_bytes(v.as_ref().try_into().unwrap_or([0; 8])))
                            .unwrap_or(0);
                        txn.put(&tokens, &key, &(used + 1).to_be_bytes())
                    });
                    match result {
                        Ok(()) => {}
                        Err(KvError::RetriesExhausted { .. }) => {
                            exhausted.fetch_add(1, Ordering::Relaxed);
                        }
                        Err(e) => panic!("unexpected error: {e}"),
                    }
                }
            })
        })
        .collect();
    for thread in threads {
        thread.join().expect("writer thread");
    }

    let snapshot = backend.snapshot();
    let total: u64 = (0..THREADS)
        .map(|t| {
            snapshot
                .get(&tokens, format!("token{t:02}").as_bytes())
                .expect("get")
                .map(|v| u64::from_be_bytes(v.as_ref().try_into().expect("eight bytes")))
                .unwrap_or(0)
        })
        .sum();
    let exhausted = exhausted.load(Ordering::Relaxed);
    eprintln!(
        "{THREADS} writers x {ROUNDS} rounds: {total} committed, {exhausted} ran out of retries"
    );
    assert_eq!(exhausted, 0, "a transaction ran out of retries");
    assert_eq!(total, (THREADS * ROUNDS) as u64);
}
