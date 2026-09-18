//! Property tests for [`hs_tables::index::maintain_index`]: after any sequence of insert, update
//! and delete on the primary table, every index (unique and non-unique) must have exactly the
//! entries the current primary rows imply -- no orphans (an index entry whose primary key is gone,
//! or whose value no longer derives that index key) and no missing entries.

use std::collections::HashMap;

use hs_kv::memory::MemoryBackend;
use hs_kv::{KvBackend, KvRead, KvWrite, RangeSpec, TransactConfig, transact};
use hs_tables::index::{IndexDef, lookup, maintain_index};
use hs_tables::key::TupleKey;
use proptest::prelude::*;

type Pk = (u8,);
type ByValueKey = (u8,); // non-unique: derived from the stored byte value, so many pks can share it
type ByPkKey = (u8,); // unique: derived from the pk itself, so it is unique by construction

#[derive(Debug, Clone)]
enum Op {
    Insert { pk: u8, value: u8 },
    Update { pk: u8, value: u8 },
    Delete { pk: u8 },
}

fn op_strategy() -> impl Strategy<Value = Op> {
    prop_oneof![
        (0u8..6, 0u8..4).prop_map(|(pk, value)| Op::Insert { pk, value }),
        (0u8..6, 0u8..4).prop_map(|(pk, value)| Op::Update { pk, value }),
        (0u8..6).prop_map(|pk| Op::Delete { pk }),
    ]
}

/// Scans an index keyspace end to end and returns every `(index_key, primary_key)` pair actually
/// stored, decoded.
fn scan_index<Ks, K, IK, R>(txn: &R, index: &IndexDef<Ks, K, IK>) -> Vec<(IK, K)>
where
    K: TupleKey,
    IK: TupleKey,
    R: KvRead<Keyspace = Ks>,
{
    txn.range(index.keyspace(), RangeSpec::full())
        .map(|item| {
            let (raw_key, _value) =
                item.expect("range scan does not fail against the in-memory backend");
            let decoded: (IK, K) =
                TupleKey::decode(&raw_key).expect("every stored composite index key decodes");
            decoded
        })
        .collect()
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn no_orphans_after_any_sequence_of_insert_update_delete(ops in proptest::collection::vec(op_strategy(), 1..40)) {
        let backend = MemoryBackend::new();
        let rows = backend.keyspace("rows").unwrap();
        let by_value_ks = backend.keyspace("by_value").unwrap();
        let by_pk_ks = backend.keyspace("by_pk").unwrap();

        let by_value: IndexDef<_, Pk, ByValueKey> =
            IndexDef::new(by_value_ks, false, |_pk: &Pk, value: &[u8]| value.first().map(|&b| (b,)));
        let by_pk: IndexDef<_, Pk, ByPkKey> =
            IndexDef::new(by_pk_ks, true, |pk: &Pk, _value: &[u8]| Some((pk.0,)));

        // The model: the ground truth this test checks the real store against.
        let mut model: HashMap<u8, u8> = HashMap::new();

        for op in ops {
            transact(&backend, TransactConfig::default(), |txn| {
                match op {
                    Op::Insert { pk, value } | Op::Update { pk, value } => {
                        let key = (pk,);
                        let old = txn.get(&rows, &key.encode())?;
                        txn.put(&rows, &key.encode(), &[value])?;
                        maintain_index(txn, &by_value, &key, old.as_deref(), Some(&[value]))
                            .expect("non-unique index never conflicts");
                        maintain_index(txn, &by_pk, &key, old.as_deref(), Some(&[value]))
                            .expect("by_pk is derived from pk alone, so it can never collide");
                    }
                    Op::Delete { pk } => {
                        let key = (pk,);
                        let old = txn.get(&rows, &key.encode())?;
                        txn.delete(&rows, &key.encode())?;
                        maintain_index(txn, &by_value, &key, old.as_deref(), None)
                            .expect("deleting only ever removes an index entry");
                        maintain_index(txn, &by_pk, &key, old.as_deref(), None)
                            .expect("deleting only ever removes an index entry");
                    }
                }
                Ok(())
            })
            .unwrap();

            match op {
                Op::Insert { pk, value } | Op::Update { pk, value } => {
                    model.insert(pk, value);
                }
                Op::Delete { pk } => {
                    model.remove(&pk);
                }
            }

            // Invariant 1: every index row's primary key exists in the model with a value that
            // really does derive that index key (no orphan pointing at a gone or changed row).
            let snap = backend.snapshot();
            for (index_key, pk_tuple) in scan_index(&snap, &by_value) {
                let pk = pk_tuple.0;
                let stored_value = *model.get(&pk).unwrap_or_else(|| {
                    panic!("by_value index has an entry for pk {pk} but the model has no such row")
                });
                prop_assert_eq!(
                    index_key.0, stored_value,
                    "by_value index entry for pk {} says value {} but the row's real value is {}",
                    pk, index_key.0, stored_value
                );
            }
            for (index_key, pk_tuple) in scan_index(&snap, &by_pk) {
                prop_assert!(
                    model.contains_key(&pk_tuple.0),
                    "by_pk index has an entry for pk {} but the model has no such row",
                    pk_tuple.0
                );
                prop_assert_eq!(index_key.0, pk_tuple.0);
            }

            // Invariant 2: every live row appears in both indexes under its current value.
            for (&pk, &value) in &model {
                let found = lookup(&snap, &by_value, &(value,)).unwrap();
                prop_assert!(
                    found.contains(&(pk,)),
                    "row pk={pk} value={value} is missing from the by_value index"
                );
                let found_pk = lookup(&snap, &by_pk, &(pk,)).unwrap();
                prop_assert_eq!(found_pk, vec![(pk,)], "row pk={} is missing from the by_pk index", pk);
            }

            // Invariant 3: the unique index never has two different rows under the same key (it
            // is derived from pk alone here, so this also re-proves invariant 1 for it, but check
            // the uniqueness property explicitly since that's what "unique" means).
            let mut seen = std::collections::HashSet::new();
            for (index_key, _pk) in scan_index(&snap, &by_pk) {
                prop_assert!(seen.insert(index_key.0), "by_pk index key {} appears more than once", index_key.0);
            }
        }
    }
}

#[test]
fn a_unique_index_rejects_a_second_primary_key_claiming_the_same_value() {
    let backend = MemoryBackend::new();
    let rows = backend.keyspace("rows").unwrap();
    let by_value_ks = backend.keyspace("by_value_unique").unwrap();
    let by_value: IndexDef<_, Pk, ByValueKey> =
        IndexDef::new(by_value_ks, true, |_pk: &Pk, value: &[u8]| {
            value.first().map(|&b| (b,))
        });

    transact(&backend, TransactConfig::default(), |txn| {
        let pk = (1u8,);
        txn.put(&rows, &pk.encode(), &[9u8])?;
        maintain_index(txn, &by_value, &pk, None, Some(&[9u8])).expect("first claim succeeds");
        Ok(())
    })
    .unwrap();

    let err = transact::<MemoryBackend, ()>(&backend, TransactConfig::default(), |txn| {
        let pk = (2u8,);
        txn.put(&rows, &pk.encode(), &[9u8])?;
        // A different pk claiming the same indexed value must be rejected.
        match maintain_index(txn, &by_value, &pk, None, Some(&[9u8])) {
            Ok(()) => panic!("expected a unique-index conflict"),
            Err(hs_tables::index::IndexError::UniqueConflict) => Err(hs_kv::KvError::backend(
                std::io::Error::other("unique conflict, abort"),
            )),
            Err(other) => panic!("unexpected error: {other}"),
        }
    })
    .unwrap_err();

    assert!(err.to_string().contains("unique conflict"));
}
