//! The PostgreSQL backend's commit flushes a transaction's buffered writes in bulk (RFC 0021):
//! one multi-row upsert and one multi-key delete per table, chunked, instead of one statement per
//! write. These tests check what the shared conformance suite does not reach -- a transaction
//! larger than one chunk, puts and deletes to one table in one commit, the statement count -- and
//! measure the fan-out shape the RFC describes (a batch of 100 members, ~300 puts over three
//! tables).
//!
//! Gated on a reachable server exactly like `postgres_conformance.rs`: `HS_KV_TEST_POSTGRES_DSN`,
//! defaulting to `postgres://postgres:hskvtest@localhost:5433/postgres`, and a `SKIP` line
//! otherwise. `HS_KV_FLUSH_BENCH_ITERS` (default 20) is how many fan-out commits the timing test
//! runs; it prints the mean and median per commit.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use hs_kv::postgres_backend::{PostgresBackend, PostgresOpenOptions};
use hs_kv::{KvBackend, KvRead, KvWrite, RangeSpec, TransactConfig, transact};

fn open_small(dsn: &str, schema: &str) -> Result<PostgresBackend, hs_kv::KvError> {
    PostgresBackend::open_with(
        dsn,
        &PostgresOpenOptions {
            schema: schema.to_owned(),
            pool_size: 2,
            ..PostgresOpenOptions::default()
        },
    )
}

fn reachable_dsn() -> Option<String> {
    let dsn = std::env::var("HS_KV_TEST_POSTGRES_DSN")
        .unwrap_or_else(|_| "postgres://postgres:hskvtest@localhost:5433/postgres".to_owned());
    match open_small(&dsn, "hs_kv_reachability_probe") {
        Ok(_backend) => Some(dsn),
        Err(e) => {
            eprintln!(
                "SKIP: postgres_bulk_flush tests skipped, no PostgreSQL reachable at {dsn:?}: {e}"
            );
            None
        }
    }
}

fn fresh_schema_name() -> String {
    static COUNTER: AtomicUsize = AtomicUsize::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("hs_kv_bulk_{}_{n}", std::process::id())
}

/// Every `(key, value)` of `ks`, in key order.
fn dump(
    backend: &PostgresBackend,
    ks: &<PostgresBackend as KvBackend>::Keyspace,
) -> Vec<(Vec<u8>, Vec<u8>)> {
    backend
        .snapshot()
        .range(ks, RangeSpec::full())
        .map(|item| {
            let (k, v) = item.expect("range item");
            (k.to_vec(), v.to_vec())
        })
        .collect()
}

/// A transaction larger than one flush chunk, with puts and deletes to the same table and to
/// several tables, lands exactly as the map of pending writes says: the last write to a key wins,
/// a delete of a key the same transaction put removes it, and nothing outside the written keys
/// moves.
#[test]
fn a_commit_larger_than_a_chunk_applies_every_put_and_delete() {
    let Some(dsn) = reachable_dsn() else {
        return;
    };
    let backend = open_small(&dsn, &fresh_schema_name()).expect("open");
    let a = backend.keyspace("bulk_a").expect("keyspace a");
    let b = backend.keyspace("bulk_b").expect("keyspace b");

    // 3,000 rows in `a`, 100 in `b`: well over one chunk for `a` (see
    // `postgres_backend::FLUSH_CHUNK_ROWS`), in one commit.
    const SEED: u32 = 3_000;
    transact(&backend, TransactConfig::default(), |txn| {
        for i in 0..SEED {
            txn.put(&a, &i.to_be_bytes(), format!("v{i}").as_bytes())?;
        }
        for i in 0..100u32 {
            txn.put(&b, &i.to_be_bytes(), b"b")?;
        }
        Ok(())
    })
    .expect("seed commits");
    assert_eq!(dump(&backend, &a).len(), SEED as usize);
    assert_eq!(dump(&backend, &b).len(), 100);

    // One transaction: overwrite the even keys of `a`, delete the odd ones, add 2,500 new keys
    // above the seed (so puts alone exceed a chunk again), put-then-delete one new key and
    // delete-then-put another, and delete all of `b` but one key it also overwrites.
    transact(&backend, TransactConfig::default(), |txn| {
        for i in 0..SEED {
            if i % 2 == 0 {
                txn.put(&a, &i.to_be_bytes(), format!("w{i}").as_bytes())?;
            } else {
                txn.delete(&a, &i.to_be_bytes())?;
            }
        }
        for i in SEED..SEED + 2_500 {
            txn.put(&a, &i.to_be_bytes(), b"new")?;
        }
        txn.put(&a, &u32::MAX.to_be_bytes(), b"doomed")?;
        txn.delete(&a, &u32::MAX.to_be_bytes())?;
        txn.delete(&a, &(u32::MAX - 1).to_be_bytes())?;
        txn.put(&a, &(u32::MAX - 1).to_be_bytes(), b"revived")?;
        for i in 0..100u32 {
            txn.delete(&b, &i.to_be_bytes())?;
        }
        txn.put(&b, &7u32.to_be_bytes(), b"kept")?;
        // The transaction sees its own buffered writes before they are flushed.
        assert_eq!(txn.get(&a, &1u32.to_be_bytes())?, None);
        assert_eq!(
            txn.get(&a, &(u32::MAX - 1).to_be_bytes())?.as_deref(),
            Some(&b"revived"[..])
        );
        Ok(())
    })
    .expect("bulk commit");

    let rows_a = dump(&backend, &a);
    let mut expected_a: Vec<(Vec<u8>, Vec<u8>)> = (0..SEED)
        .filter(|i| i % 2 == 0)
        .map(|i| (i.to_be_bytes().to_vec(), format!("w{i}").into_bytes()))
        .chain((SEED..SEED + 2_500).map(|i| (i.to_be_bytes().to_vec(), b"new".to_vec())))
        .collect();
    expected_a.push(((u32::MAX - 1).to_be_bytes().to_vec(), b"revived".to_vec()));
    expected_a.sort();
    assert_eq!(rows_a.len(), expected_a.len());
    assert_eq!(rows_a, expected_a);
    assert_eq!(
        dump(&backend, &b),
        vec![(7u32.to_be_bytes().to_vec(), b"kept".to_vec())]
    );

    let stats = backend.flush_stats();
    assert_eq!(stats.flushes, 2, "{stats:?}");
    assert_eq!(
        stats.writes,
        (SEED + 100) as u64 + (SEED + 2_500 + 2 + 100) as u64,
        "{stats:?}"
    );
    // Seed: `a` 3,000 puts = 2 chunks, `b` 100 puts = 1. Second: `a` 1,500 + 2,500 + 1 = 4,001
    // puts = 3 chunks and 1,500 + 1 deletes = 1; `b` 1 put and 99 deletes = 2.
    assert_eq!(stats.statements, 3 + 4 + 2, "{stats:?}");
}

/// A transaction that writes nothing flushes nothing: `COMMIT` is the only statement.
#[test]
fn an_empty_commit_sends_no_write_statement() {
    let Some(dsn) = reachable_dsn() else {
        return;
    };
    let backend = open_small(&dsn, &fresh_schema_name()).expect("open");
    let ks = backend.keyspace("bulk_empty").expect("keyspace");
    transact(&backend, TransactConfig::default(), |txn| {
        txn.get(&ks, b"absent").map(|_| ())
    })
    .expect("read-only commit");
    let stats = backend.flush_stats();
    assert_eq!(stats.flushes, 1, "{stats:?}");
    assert_eq!(stats.writes, 0, "{stats:?}");
    assert_eq!(stats.statements, 0, "{stats:?}");
}

/// A failed flush (here a value the table's own constraint refuses) rolls the transaction back
/// and reports the error, and the pool's next transaction commits.
#[test]
fn a_failed_bulk_flush_rolls_back_and_the_next_commit_succeeds() {
    let Some(dsn) = reachable_dsn() else {
        return;
    };
    let schema = fresh_schema_name();
    let backend = open_small(&dsn, &schema).expect("open");
    let ks = backend.keyspace("bulk_checked").expect("keyspace");
    transact(&backend, TransactConfig::default(), |txn| {
        txn.put(&ks, b"ok", b"first")
    })
    .expect("first commit");
    {
        // A check constraint on the table, outside the backend's knowledge.
        let mut client = postgres::Client::connect(&dsn, postgres::NoTls).expect("admin connect");
        client
            .batch_execute(&format!(
                "ALTER TABLE \"{schema}\".\"kv_bulk_checked\" ADD CONSTRAINT no_bad CHECK (v <> 'bad'::bytea)"
            ))
            .expect("add constraint");
    }
    let mut txn = backend.begin().expect("begin");
    for i in 0..10u32 {
        txn.put(&ks, &i.to_be_bytes(), b"fine").expect("put");
    }
    txn.put(&ks, b"ok", b"bad").expect("put");
    let err = backend
        .commit(txn)
        .expect_err("the constraint fails the flush");
    assert!(err.to_string().contains("no_bad"), "{err}");

    let rows = dump(&backend, &ks);
    assert_eq!(
        rows,
        vec![(b"ok".to_vec(), b"first".to_vec())],
        "nothing of the failed commit landed"
    );
    transact(&backend, TransactConfig::default(), |txn| {
        txn.put(&ks, b"ok", b"second")
    })
    .expect("the next commit succeeds");
    assert_eq!(
        dump(&backend, &ks),
        vec![(b"ok".to_vec(), b"second".to_vec())]
    );
    assert_eq!(backend.notices_received().warnings, 0);
}

/// The RFC's fan-out shape: a batch of 100 members, three puts per member over three tables
/// (the feed row, the `feed_by_room` pointer, the feed head), one commit. Prints the per-commit
/// mean and median; the numbers go in RFC 0021 and the status file.
#[test]
fn fan_out_shaped_commit_timing() {
    let Some(dsn) = reachable_dsn() else {
        return;
    };
    let iters: usize = std::env::var("HS_KV_FLUSH_BENCH_ITERS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(20);
    let backend = open_small(&dsn, &fresh_schema_name()).expect("open");
    let feed = backend.keyspace("hs_user.feed").expect("feed");
    let by_room = backend
        .keyspace("hs_user.feed_by_room")
        .expect("feed_by_room");
    let heads = backend.keyspace("hs_user.feed_heads").expect("feed_heads");
    const MEMBERS: u32 = 100;
    let value = vec![0xABu8; 64];

    let mut times = Vec::with_capacity(iters);
    for round in 0..iters as u64 {
        let started = Instant::now();
        transact(&backend, TransactConfig::default(), |txn| {
            for member in 0..MEMBERS {
                let user = format!("@user{member}:example.org");
                let seq = round.to_be_bytes();
                let mut feed_key = user.clone().into_bytes();
                feed_key.push(0);
                feed_key.extend_from_slice(&seq);
                txn.put(&feed, &feed_key, &value)?;
                let mut pointer_key = user.clone().into_bytes();
                pointer_key.push(0);
                pointer_key.extend_from_slice(b"!room:example.org");
                txn.put(&by_room, &pointer_key, &seq)?;
                txn.put(&heads, user.as_bytes(), &seq)?;
            }
            Ok(())
        })
        .expect("fan-out commit");
        times.push(started.elapsed());
    }
    times.sort();
    let mean = times.iter().sum::<Duration>() / iters as u32;
    let median = times[iters / 2];
    let stats = backend.flush_stats();
    eprintln!(
        "fan-out commit ({MEMBERS} members, {} puts, 3 tables), {iters} rounds: mean {:.2?}, median {:.2?}, min {:.2?}, max {:.2?}; \
         {} write statements over {} flushes",
        MEMBERS * 3,
        mean,
        median,
        times[0],
        times[iters - 1],
        stats.statements,
        stats.flushes
    );
    assert_eq!(dump(&backend, &heads).len(), MEMBERS as usize);
    assert_eq!(dump(&backend, &feed).len(), MEMBERS as usize * iters);
}
