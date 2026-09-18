//! The chain-cover auth index, in interned form.
//!
//! `PLAN.md` section 6.4: "Auth chain difference is the expensive primitive under state
//! resolution v2. The chain-cover index (each event assigned a `(chain_id, sequence_number)` such
//! that an event's auth chain is a set of chain prefixes, with a small table of links between
//! chains) answers it with a handful of range reads instead of a graph walk, and it is the best
//! known structure; Synapse introduced it and Palpo adopted it."
//!
//! # The idea
//!
//! An event's full auth chain (every event reachable by following `auth_events` transitively) can
//! be enormous, but it decomposes into long runs: consecutive versions of the same piece of state
//! (successive `m.room.power_levels` events, or a chain of a single user's membership changes)
//! almost always cite their immediate predecessor in `auth_events`, because every event's auth
//! events include "the current `m.room.power_levels` event" and similar. Group events into
//! *chains* along those predecessor edges, number each event's position within its chain, and:
//!
//! - Whether ancestor *A* is in event *E*'s auth chain, when *A* and *E* are on the *same* chain,
//!   is one comparison: `A.sequence <= E.sequence`.
//! - When they are on different chains, a chain carries a short list of *links* -- "at sequence
//!   *s* on this chain, the auth chain also depends on chain *C* up to sequence *t*" -- recorded
//!   only for the auth-event edges that were *not* chosen to extend the chain. Walking from *E* to
//!   *A* is then a search over chains and their links, not over individual events: normally a
//!   handful of hops, however large the room's history is.
//!
//! This module builds and queries that structure. It does not decide *which* auth event to extend
//! a chain along beyond a simple, documented heuristic (below); a from-scratch rebuild for an
//! imported room, background verification, and incremental maintenance as new events arrive are
//! Phase 1/2 work per `docs/workstreams/02-state-and-model.md`.
//!
//! Read (for the general approach, not copied -- Synapse is AGPL-3.0 and is a behavioral reference
//! only) alongside `refs/synapse/synapse/docs/auth_chain_difference_algorithm.md` and adopted
//! independently, matching the same idea Palpo's `chain_cover` also implements.

use std::collections::BTreeMap;

use hs_model::ids::{EventSn, StateKeyId};

/// A chain identifier: interned, dense, assigned in creation order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ChainId(pub u32);

/// An event's position within the chain-cover: which chain it is on, and how far along.
///
/// Positions on the same chain are totally ordered by `sequence`, and the ordering is causal:
/// for any two events on the same chain, the one with the lower `sequence` is an ancestor (via
/// the chain's chosen predecessor edges) of the one with the higher `sequence`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ChainPosition {
    /// Which chain.
    pub chain: ChainId,
    /// Position within the chain, starting at 1.
    pub sequence: u32,
}

/// The chain-cover index for one room.
///
/// Build by calling [`ChainCoverIndex::add_event`] once per event, in an order where every event's
/// `auth_events` have already been added (any topological order of the auth-events DAG works;
/// receipt order satisfies this for events accepted normally, since an event's auth events must
/// already be known and accepted before the event itself is).
#[derive(Debug, Clone, Default)]
pub struct ChainCoverIndex {
    /// Every known event's position.
    position: BTreeMap<EventSn, ChainPosition>,
    /// The inverse of `position`, for materializing concrete events out of a chain range.
    event_at: BTreeMap<ChainPosition, EventSn>,
    /// The interned `(type, state_key)` of every known event, used only to prefer extending a
    /// chain along an auth event for the *same* key (see [`ChainCoverIndex::add_event`]).
    key_of: BTreeMap<EventSn, StateKeyId>,
    /// Links out of a chain: `chain -> sequence -> other positions the auth chain also depends on
    /// as of that sequence`. Recorded only for auth-event edges that were not chosen to extend the
    /// chain itself.
    links: BTreeMap<ChainId, BTreeMap<u32, Vec<ChainPosition>>>,
    /// The highest `sequence` used so far on each chain: only a position exactly at its chain's
    /// current tip may be extended (see [`ChainCoverIndex::add_event`]), otherwise two events that
    /// both cite the same ancestor as "the next version" would collide on one position.
    chain_tip: BTreeMap<ChainId, u32>,
    next_chain: u32,
}

impl ChainCoverIndex {
    /// An empty index.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The number of chains created so far.
    #[must_use]
    pub fn chain_count(&self) -> usize {
        self.next_chain as usize
    }

    /// The number of events indexed so far.
    #[must_use]
    pub fn len(&self) -> usize {
        self.position.len()
    }

    /// Whether the index is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.position.is_empty()
    }

    /// This event's chain position, if it has been added.
    #[must_use]
    pub fn position(&self, event: EventSn) -> Option<ChainPosition> {
        self.position.get(&event).copied()
    }

    /// The concrete event at a chain position, if any event was placed exactly there.
    #[must_use]
    pub fn event_at(&self, position: ChainPosition) -> Option<EventSn> {
        self.event_at.get(&position).copied()
    }

    /// Adds one event to the index and returns its assigned position.
    ///
    /// `key` is the event's interned `(type, state_key)` (only used to prefer keeping a state
    /// key's version history on one chain; the algorithm is correct without this, but the
    /// resulting chains are shorter and less informative without it). `auth_events` is the
    /// event's `auth_events`, which must already have been added.
    ///
    /// The chain to extend is chosen, in order of preference, among auth events that are
    /// currently the *tip* of their chain (nothing has extended past them yet -- an auth event
    /// that has already been superseded as someone else's predecessor cannot be extended again
    /// without colliding two different events onto the same position):
    /// 1. An auth event with the same `key` (successive versions of the same piece of state
    ///    almost always cite their own predecessor).
    /// 2. Otherwise, the auth event whose existing position has the greatest `sequence` (a
    ///    heavy-path heuristic: extending the already-longest chain keeps the chain count low).
    ///
    /// If no known auth event is a usable tip (there are none, or every one of them has already
    /// been extended by an earlier fork of the same history), a new chain is started instead, and
    /// every known auth event becomes a link recorded at the new position. Otherwise, every known
    /// auth event *other than* the one chosen to extend becomes a link.
    ///
    /// # Panics
    /// Never panics; an `auth_events` entry that has not itself been added is silently ignored
    /// (treated as outside the indexed region, exactly like an event whose ancestor chain reaches
    /// further back than what the caller has loaded).
    pub fn add_event(
        &mut self,
        event: EventSn,
        key: StateKeyId,
        auth_events: &[EventSn],
    ) -> ChainPosition {
        self.key_of.insert(event, key);

        let known: Vec<(EventSn, ChainPosition)> = auth_events
            .iter()
            .filter_map(|a| self.position.get(a).map(|p| (*a, *p)))
            .collect();
        let is_tip =
            |p: ChainPosition| self.chain_tip.get(&p.chain).copied().unwrap_or(0) == p.sequence;

        let extend_from = known
            .iter()
            .filter(|(_, p)| is_tip(*p))
            .find(|(id, _)| self.key_of.get(id) == Some(&key))
            .copied()
            .or_else(|| {
                known
                    .iter()
                    .filter(|(_, p)| is_tip(*p))
                    .max_by_key(|(id, p)| (p.sequence, std::cmp::Reverse(*id)))
                    .copied()
            });

        let position = match extend_from {
            Some((extend_id, extend_pos)) => {
                let new_position = ChainPosition {
                    chain: extend_pos.chain,
                    sequence: extend_pos.sequence + 1,
                };
                self.chain_tip
                    .insert(new_position.chain, new_position.sequence);
                self.record_links(
                    new_position,
                    known
                        .iter()
                        .filter(|(id, _)| *id != extend_id)
                        .map(|(_, p)| *p),
                );
                new_position
            }
            None => {
                let chain = ChainId(self.next_chain);
                self.next_chain += 1;
                let new_position = ChainPosition { chain, sequence: 1 };
                self.chain_tip.insert(chain, 1);
                self.record_links(new_position, known.iter().map(|(_, p)| *p));
                new_position
            }
        };

        self.position.insert(event, position);
        self.event_at.insert(position, event);
        position
    }

    /// Records links from `at` to `targets`, keeping only the furthest position per target chain
    /// (a link to `(C, s)` subsumes any link to `(C, s')` for `s' <= s`, since chain prefixes are
    /// already transitively covered).
    fn record_links(&mut self, at: ChainPosition, targets: impl Iterator<Item = ChainPosition>) {
        let mut link_targets: Vec<ChainPosition> = targets.collect();
        link_targets.sort_by_key(|p| (p.chain, std::cmp::Reverse(p.sequence)));
        link_targets.dedup_by_key(|p| p.chain);
        if !link_targets.is_empty() {
            self.links
                .entry(at.chain)
                .or_default()
                .insert(at.sequence, link_targets);
        }
    }

    /// The chain coverage reachable from `roots`: for every chain the auth chains of `roots`
    /// touch, the furthest `sequence` reached on it.
    ///
    /// This is the primitive both [`ChainCoverIndex::contains`] and
    /// [`ChainCoverIndex::auth_chain_difference`] are built from: computing "how far does this set
    /// of events' auth chains reach on each chain" costs one traversal over chains and their link
    /// tables, not over individual events.
    #[must_use]
    pub fn coverage(&self, roots: impl IntoIterator<Item = EventSn>) -> BTreeMap<ChainId, u32> {
        let mut reach: BTreeMap<ChainId, u32> = BTreeMap::new();
        let mut stack: Vec<ChainPosition> = Vec::new();

        // A root's *auth chain* is its ancestors, not the root itself: seed the search with the
        // chain prefix strictly before the root (sequence - 1) and the root's own links (recorded
        // at its own sequence, for auth events other than the one that extends the chain).
        for root in roots {
            let Some(pos) = self.position.get(&root).copied() else {
                continue;
            };
            if pos.sequence > 1 {
                stack.push(ChainPosition {
                    chain: pos.chain,
                    sequence: pos.sequence - 1,
                });
            }
            if let Some(targets) = self
                .links
                .get(&pos.chain)
                .and_then(|by_seq| by_seq.get(&pos.sequence))
            {
                stack.extend(targets.iter().copied());
            }
        }

        while let Some(pos) = stack.pop() {
            let entry = reach.entry(pos.chain).or_insert(0);
            if pos.sequence <= *entry {
                // Already covered at least this far on this chain; its links were already
                // followed when that coverage was recorded.
                continue;
            }
            *entry = pos.sequence;

            if let Some(by_sequence) = self.links.get(&pos.chain) {
                for targets in by_sequence.range(..=pos.sequence).map(|(_, t)| t) {
                    stack.extend(targets.iter().copied());
                }
            }
        }

        reach
    }

    /// Whether `ancestor` is in `event`'s auth chain. `event` is trivially in its own auth chain
    /// (`ancestor == event` is always `Some(true)` once `event` is indexed), matching how callers
    /// use this -- "has this event's causal history already accounted for `ancestor`" -- even
    /// though the spec's own auth-chain definition (walking `auth_events` edges) does not include
    /// an event in its own chain.
    ///
    /// Returns `None` if either event has not been added to the index.
    #[must_use]
    pub fn contains(&self, event: EventSn, ancestor: EventSn) -> Option<bool> {
        let ancestor_position = self.position(ancestor)?;
        if !self.position.contains_key(&event) {
            return None;
        }
        if event == ancestor {
            return Some(true);
        }
        let reach = self.coverage([event]);
        Some(
            reach
                .get(&ancestor_position.chain)
                .is_some_and(|&max| max >= ancestor_position.sequence),
        )
    }

    /// The [auth difference](https://spec.matrix.org/v1.19/rooms/v2/#definitions) of `sets`: every
    /// event that is in the union of `sets`' auth chains but not in their intersection.
    ///
    /// This is the primitive state resolution v2/v2.1 spends most of its time on
    /// ([`crate::state_res::v2`], [`crate::state_res::oracle`]); with the chain-cover index it
    /// costs one [`coverage`](Self::coverage) traversal per input set (each bounded by the number
    /// of chains touched, not the number of events) plus materializing the resulting chain
    /// ranges, rather than a full graph walk per set.
    #[must_use]
    pub fn auth_chain_difference(&self, sets: &[Vec<EventSn>]) -> Vec<EventSn> {
        let coverages: Vec<BTreeMap<ChainId, u32>> = sets
            .iter()
            .map(|set| self.coverage(set.iter().copied()))
            .collect();

        let mut all_chains: std::collections::BTreeSet<ChainId> = std::collections::BTreeSet::new();
        for coverage in &coverages {
            all_chains.extend(coverage.keys().copied());
        }

        let mut difference = Vec::new();
        for chain in all_chains {
            let maxima: Vec<u32> = coverages
                .iter()
                .map(|c| c.get(&chain).copied().unwrap_or(0))
                .collect();
            let min = maxima.iter().copied().min().unwrap_or(0);
            let max = maxima.iter().copied().max().unwrap_or(0);
            for sequence in (min + 1)..=max {
                if let Some(event) = self.event_at(ChainPosition { chain, sequence }) {
                    difference.push(event);
                }
            }
        }
        difference
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sn(n: u64) -> EventSn {
        EventSn::new(n)
    }

    fn key(n: u32) -> StateKeyId {
        StateKeyId::new(n)
    }

    /// A straight-line chain (each event's only auth event is its predecessor, same key) should
    /// end up as one chain with sequential positions, and `contains` should agree with the
    /// obvious total order.
    #[test]
    fn straight_line_history_is_one_chain() {
        let mut index = ChainCoverIndex::new();
        let power_levels_key = key(1);
        let mut prev = None;
        let mut positions = Vec::new();
        for i in 1..=5u64 {
            let auth: Vec<EventSn> = prev.into_iter().collect();
            let pos = index.add_event(sn(i), power_levels_key, &auth);
            positions.push(pos);
            prev = Some(sn(i));
        }

        assert_eq!(index.chain_count(), 1);
        for (i, pos) in positions.iter().enumerate() {
            assert_eq!(pos.sequence, i as u32 + 1);
        }
        assert_eq!(index.contains(sn(5), sn(1)), Some(true));
        assert_eq!(index.contains(sn(1), sn(5)), Some(false));
        assert_eq!(index.contains(sn(3), sn(3)), Some(true));
    }

    /// A branch that cites an event on another chain records a link, and `contains` follows it.
    #[test]
    fn cross_chain_link_is_followed() {
        let mut index = ChainCoverIndex::new();
        let create_key = key(0);
        let member_key = key(1);
        let power_key = key(2);

        // create (chain 0)
        index.add_event(sn(1), create_key, &[]);
        // power_levels citing create (extends: no same-key auth event, only candidate is create;
        // heavy-path picks it) -- to keep power_levels on its own chain, give it a self-citing
        // predecessor first.
        index.add_event(sn(2), power_key, &[sn(1)]);
        index.add_event(sn(3), power_key, &[sn(2)]);
        // A member event whose auth_events cite the *latest* power_levels event (sn(3)) and create
        // (sn(1)), but has no same-key predecessor of its own: it should extend along whichever
        // known auth event has the deepest chain (sn(3), sequence 2) and link to the other.
        let member_pos = index.add_event(sn(4), member_key, &[sn(3), sn(1)]);

        assert_eq!(index.contains(sn(4), sn(3)), Some(true));
        assert_eq!(
            index.contains(sn(4), sn(2)),
            Some(true),
            "must follow the power_levels chain prefix"
        );
        assert_eq!(
            index.contains(sn(4), sn(1)),
            Some(true),
            "must follow the link to create's chain"
        );
        assert_eq!(member_pos.chain, index.position(sn(3)).unwrap().chain);
    }

    /// `auth_chain_difference` matches a brute-force transitive closure over a small hand-built
    /// DAG with two forks that share a common ancestor and diverge.
    #[test]
    fn auth_chain_difference_matches_brute_force() {
        let mut index = ChainCoverIndex::new();
        index.add_event(sn(1), key(0), &[]); // create
        index.add_event(sn(2), key(1), &[sn(1)]); // power_levels v1
        index.add_event(sn(3), key(1), &[sn(2)]); // power_levels v2 (fork point ancestor)

        // Fork A: a member event citing power_levels v2.
        index.add_event(sn(4), key(2), &[sn(3), sn(1)]);
        // Fork B: a different power_levels v3 citing v2, plus an unrelated member event.
        index.add_event(sn(5), key(1), &[sn(3)]);
        index.add_event(sn(6), key(3), &[sn(5), sn(1)]);

        let set_a = vec![sn(4)];
        let set_b = vec![sn(6)];

        // Brute-force auth chains via direct graph walk over the same edges.
        let edges: BTreeMap<EventSn, Vec<EventSn>> = BTreeMap::from([
            (sn(1), vec![]),
            (sn(2), vec![sn(1)]),
            (sn(3), vec![sn(2)]),
            (sn(4), vec![sn(3), sn(1)]),
            (sn(5), vec![sn(3)]),
            (sn(6), vec![sn(5), sn(1)]),
        ]);
        fn closure(
            roots: &[EventSn],
            edges: &BTreeMap<EventSn, Vec<EventSn>>,
        ) -> std::collections::BTreeSet<EventSn> {
            let mut seen = std::collections::BTreeSet::new();
            let mut stack: Vec<EventSn> = roots.to_vec();
            while let Some(id) = stack.pop() {
                for &parent in edges.get(&id).into_iter().flatten() {
                    if seen.insert(parent) {
                        stack.push(parent);
                    }
                }
            }
            seen
        }
        let chain_a = closure(&set_a, &edges);
        let chain_b = closure(&set_b, &edges);
        let expected: std::collections::BTreeSet<EventSn> =
            chain_a.symmetric_difference(&chain_b).copied().collect();

        let actual: std::collections::BTreeSet<EventSn> = index
            .auth_chain_difference(&[set_a, set_b])
            .into_iter()
            .collect();

        assert_eq!(actual, expected);
    }

    #[test]
    fn unknown_events_are_none() {
        let index = ChainCoverIndex::new();
        assert_eq!(index.contains(sn(1), sn(2)), None);
        assert_eq!(index.position(sn(1)), None);
    }
}

#[cfg(test)]
mod property_tests {
    use super::*;
    use proptest::prelude::*;
    use std::collections::BTreeSet;

    fn sn(n: u64) -> EventSn {
        EventSn::new(n)
    }

    /// Generates a random small DAG: for each event `i` (in order), 0 to 2 auth events chosen
    /// from `0..i`, and a key from a small pool (so some events share a key and can extend a
    /// chain along it).
    fn dag_strategy(n: usize) -> impl Strategy<Value = (Vec<Vec<usize>>, Vec<u32>)> {
        let keys = prop::collection::vec(0u32..3, n);
        let auth: Vec<_> = (0..n)
            .map(|i| {
                if i == 0 {
                    Just(Vec::<usize>::new()).boxed()
                } else {
                    prop::collection::vec(0..i, 0..=2usize.min(i))
                        .prop_map(|mut v| {
                            v.sort_unstable();
                            v.dedup();
                            v
                        })
                        .boxed()
                }
            })
            .collect();
        (auth, keys)
    }

    fn closure(roots: &[usize], edges: &[Vec<usize>]) -> BTreeSet<usize> {
        let mut seen = BTreeSet::new();
        let mut stack: Vec<usize> = roots.to_vec();
        while let Some(id) = stack.pop() {
            for &parent in &edges[id] {
                if seen.insert(parent) {
                    stack.push(parent);
                }
            }
        }
        seen
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(256))]

        #[test]
        fn contains_matches_brute_force_ancestry((auth, keys) in dag_strategy(12)) {
            let mut index = ChainCoverIndex::new();
            for (i, a) in auth.iter().enumerate() {
                let auth_sn: Vec<EventSn> = a.iter().map(|&j| sn(j as u64)).collect();
                index.add_event(sn(i as u64), StateKeyId::new(keys[i]), &auth_sn);
            }

            for i in 0..auth.len() {
                let ancestors = closure(&[i], &auth);
                for j in 0..auth.len() {
                    let expected = i == j || ancestors.contains(&j);
                    let actual = index.contains(sn(i as u64), sn(j as u64)).unwrap();
                    prop_assert_eq!(actual, expected, "contains({}, {}) mismatch", i, j);
                }
            }
        }

        #[test]
        fn auth_chain_difference_matches_brute_force_random((auth, keys) in dag_strategy(14)) {
            let mut index = ChainCoverIndex::new();
            for (i, a) in auth.iter().enumerate() {
                let auth_sn: Vec<EventSn> = a.iter().map(|&j| sn(j as u64)).collect();
                index.add_event(sn(i as u64), StateKeyId::new(keys[i]), &auth_sn);
            }

            // Two arbitrary "state maps": the last event and the second-to-last, each treated as
            // a one-event set (matching state resolution's actual usage: the auth chain of a
            // state map is the union of its members' auth chains).
            let n = auth.len();
            if n < 2 { return Ok(()); }
            let set_a = vec![n - 1];
            let set_b = vec![n - 2];

            let chain_a = closure(&set_a, &auth);
            let chain_b = closure(&set_b, &auth);
            let expected: BTreeSet<usize> =
                chain_a.symmetric_difference(&chain_b).copied().collect();

            let actual: BTreeSet<usize> = index
                .auth_chain_difference(&[
                    vec![sn((n - 1) as u64)],
                    vec![sn((n - 2) as u64)],
                ])
                .into_iter()
                .map(|id| id.get() as usize)
                .collect();

            let expected_sn: BTreeSet<usize> = expected;
            prop_assert_eq!(actual, expected_sn);
        }
    }
}
