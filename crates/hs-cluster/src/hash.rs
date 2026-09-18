//! Stable hashing and rendezvous (highest random weight) hashing.
//!
//! Both the shard mapping and the ownership assignment must produce the same
//! answer on every replica, on every architecture, across releases: a rolling
//! update runs two versions at once. That is why this module uses xxh3 with a
//! fixed seed and never `std::hash::DefaultHasher` (documented as unstable).

use xxhash_rust::xxh3::Xxh3;

use crate::types::{ReplicaId, ShardId};

const SEED: u64 = 0x6873_636c_7573_7472; // "hsclustr"

/// Hashes the concatenation of `parts` with a fixed seed.
pub fn stable_hash64(parts: &[&[u8]]) -> u64 {
    let mut h = Xxh3::with_seed(SEED);
    for p in parts {
        h.update(p);
    }
    h.digest()
}

/// The rendezvous score of `replica` for `shard`. Higher wins.
pub fn score(shard: ShardId, replica: &ReplicaId) -> u64 {
    stable_hash64(&[&shard.key_bytes(), b"|", replica.as_str().as_bytes()])
}

/// The desired owner of `shard` among `candidates`: the candidate with the
/// highest score, ties broken by the smaller id so the function is total.
/// `None` when there are no candidates.
pub fn desired_owner<'a, I>(shard: ShardId, candidates: I) -> Option<&'a ReplicaId>
where
    I: IntoIterator<Item = &'a ReplicaId>,
{
    let mut best: Option<(u64, &ReplicaId)> = None;
    for r in candidates {
        let s = score(shard, r);
        best = match best {
            None => Some((s, r)),
            Some((bs, br)) => {
                if s > bs || (s == bs && r < br) {
                    Some((s, r))
                } else {
                    Some((bs, br))
                }
            }
        };
    }
    best.map(|(_, r)| r)
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;
    use crate::types::{ShardKind, ShardLayout};

    fn replicas(n: usize) -> Vec<ReplicaId> {
        (0..n).map(|i| ReplicaId::new(format!("hs-{i}"))).collect()
    }

    #[test]
    fn hash_is_pinned() {
        // If these change, ownership and shard mapping change under a rolling
        // update. Do not "fix" the expected values.
        assert_eq!(stable_hash64(&[b"hello"]), 0x36d9_73a4_aac2_fc79);
        assert_eq!(
            score(ShardId::new(ShardKind::Room, 7), &ReplicaId::new("hs-0")),
            0xbc1d_f7c1_55d7_0945
        );
    }

    #[test]
    fn empty_candidates_have_no_owner() {
        assert_eq!(desired_owner(ShardId::GLOBAL, &[]), None);
    }

    #[test]
    fn assignment_is_deterministic_and_order_independent() {
        let rs = replicas(5);
        let mut rev = rs.clone();
        rev.reverse();
        for s in ShardLayout::default().all_shards() {
            assert_eq!(desired_owner(s, &rs), desired_owner(s, &rev));
        }
    }

    #[test]
    fn removing_a_replica_only_moves_its_shards() {
        let rs = replicas(6);
        let layout = ShardLayout::default();
        let before: HashMap<_, _> = layout
            .all_shards()
            .map(|s| (s, desired_owner(s, &rs).cloned()))
            .collect();
        let survivors: Vec<_> = rs
            .iter()
            .filter(|r| r.as_str() != "hs-3")
            .cloned()
            .collect();
        for s in layout.all_shards() {
            let after = desired_owner(s, &survivors).cloned();
            let was = &before[&s];
            if was.as_ref().map(|r| r.as_str()) != Some("hs-3") {
                assert_eq!(&after, was, "shard {s} moved although its owner survived");
            } else {
                assert_ne!(after.as_ref().map(|r| r.as_str()), Some("hs-3"));
            }
        }
    }

    #[test]
    fn distribution_is_roughly_even() {
        let rs = replicas(8);
        let layout = ShardLayout::default();
        let mut counts: HashMap<&ReplicaId, u32> = HashMap::new();
        for s in (0..layout.rooms).map(|i| ShardId::new(ShardKind::Room, i)) {
            *counts
                .entry(desired_owner(s, &rs).expect("owner"))
                .or_default() += 1;
        }
        let expected = 256.0 / 8.0;
        for (r, c) in counts {
            // Binomial(256, 1/8): mean 32, sd about 5.3; allow four sigma.
            assert!((c as f64 - expected).abs() < 22.0, "{r}: {c}");
        }
    }
}
