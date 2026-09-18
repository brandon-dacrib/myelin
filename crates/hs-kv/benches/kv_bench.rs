//! Micro-benchmarks for `get`, `multi_get`, a bounded `range` scan and transaction commit, on
//! both backends. Run with `cargo bench -p hs-kv`.

use criterion::{Criterion, criterion_group, criterion_main};
use hs_kv::fjall_backend::FjallBackend;
use hs_kv::memory::MemoryBackend;
use hs_kv::{KvBackend, KvRead, KvWrite, RangeSpec, TransactConfig, transact};

const SEED_ROWS: u64 = 10_000;

fn seed<B: KvBackend>(backend: &B, ks: &B::Keyspace) {
    transact(backend, TransactConfig::default(), |txn| {
        for i in 0..SEED_ROWS {
            let k = i.to_be_bytes();
            txn.put(ks, &k, &k)?;
        }
        Ok(())
    })
    .expect("seed commits");
}

fn bench_backend<B: KvBackend>(c: &mut Criterion, label: &str, backend: B) {
    let ks = backend.keyspace("bench").expect("keyspace");
    seed(&backend, &ks);

    c.bench_function(&format!("get/{label}"), |b| {
        b.iter(|| {
            let snap = backend.snapshot();
            std::hint::black_box(snap.get(&ks, &5_000u64.to_be_bytes()).unwrap())
        });
    });

    let probe_keys: Vec<[u8; 8]> = (0..100)
        .map(|i| (i * 97 % SEED_ROWS).to_be_bytes())
        .collect();
    c.bench_function(&format!("multi_get_100/{label}"), |b| {
        b.iter(|| {
            let snap = backend.snapshot();
            let refs: Vec<&[u8]> = probe_keys.iter().map(|k| k.as_slice()).collect();
            std::hint::black_box(snap.multi_get(&ks, &refs).unwrap())
        });
    });

    c.bench_function(&format!("range_100/{label}"), |b| {
        b.iter(|| {
            let snap = backend.snapshot();
            let items: Vec<_> = snap.range(&ks, RangeSpec::full().limit(100)).collect();
            std::hint::black_box(items)
        });
    });

    c.bench_function(&format!("txn_commit/{label}"), |b| {
        let mut next_key = SEED_ROWS + 1_000_000;
        b.iter(|| {
            next_key += 1;
            transact(&backend, TransactConfig::default(), |txn| {
                txn.put(&ks, &next_key.to_be_bytes(), b"benchmark-value")
            })
            .expect("commit");
        });
    });
}

fn benches(c: &mut Criterion) {
    bench_backend(c, "memory", MemoryBackend::new());

    let dir = tempfile::tempdir().expect("tempdir");
    let fjall = FjallBackend::open(dir.path()).expect("open fjall backend");
    bench_backend(c, "fjall", fjall);
}

criterion_group!(kv_benches, benches);
criterion_main!(kv_benches);
