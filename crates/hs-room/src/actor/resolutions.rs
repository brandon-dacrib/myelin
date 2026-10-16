//! The state resolutions a [`super::RoomActor`] has run, remembered by the forks they resolved
//! ([`super::RoomActor::resolve_forks`]).
//!
//! A room whose forward extremities stay forked -- a large federated room this server only
//! partly holds, whose forks it never merges -- answers every read of its current state with a
//! resolution of those forks. Until 2026-10-16 each read ran it again: the session hub's nine
//! reads per room update took 30 to 50 seconds on the demo's `#ping` room, with the room's lock
//! held throughout, and the reads of every PDU, `/sync` and the federation sender kept about two
//! cores busy on a server with almost no client load.
//!
//! The forks are content-addressed roots, so the same key always means the same states and the
//! answer cannot go stale; the cache only bounds how many it keeps. A few entries cover what
//! repeats: the current extremities, and the prev events a burst of federated events cite.

use std::collections::VecDeque;

/// How many resolutions one actor keeps: the current state's, and a few recent events' prev
/// states. Each is a handful of 16-byte roots.
pub(crate) const KEPT: usize = 8;

/// A small most-recently-used map from a sorted, distinct set of fork roots to the root of their
/// resolution.
#[derive(Debug)]
pub(crate) struct ResolutionCache<R> {
    entries: VecDeque<(Vec<R>, R)>,
}

impl<R> Default for ResolutionCache<R> {
    fn default() -> Self {
        Self {
            entries: VecDeque::with_capacity(KEPT),
        }
    }
}

impl<R: PartialEq + Copy> ResolutionCache<R> {
    /// The resolution of `forks`, if kept, made the most recently used.
    pub(crate) fn get(&mut self, forks: &[R]) -> Option<R> {
        let at = self.entries.iter().position(|(key, _)| key == forks)?;
        let entry = self.entries.remove(at)?;
        let root = entry.1;
        self.entries.push_front(entry);
        Some(root)
    }

    /// Keeps `root` as the resolution of `forks`, dropping the least recently used beyond
    /// [`KEPT`].
    pub(crate) fn insert(&mut self, forks: Vec<R>, root: R) {
        if let Some(at) = self.entries.iter().position(|(key, _)| *key == forks) {
            self.entries.remove(at);
        }
        self.entries.push_front((forks, root));
        self.entries.truncate(KEPT);
    }

    /// How many forks the widest kept resolution resolved (0 if none is kept).
    #[cfg(test)]
    pub(crate) fn widest(&self) -> usize {
        self.entries
            .iter()
            .map(|(key, _)| key.len())
            .max()
            .unwrap_or(0)
    }

    /// How many resolutions are kept.
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_kept_resolution_is_found_by_its_forks() {
        let mut cache = ResolutionCache::default();
        assert_eq!(cache.get(&[1, 2]), None);
        cache.insert(vec![1, 2], 10);
        cache.insert(vec![1, 3], 11);
        assert_eq!(cache.get(&[1, 2]), Some(10));
        assert_eq!(cache.get(&[1, 3]), Some(11));
        assert_eq!(cache.get(&[2, 3]), None);
    }

    #[test]
    fn the_least_recently_used_goes_first() {
        let mut cache = ResolutionCache::default();
        for n in 0..KEPT {
            cache.insert(vec![n, n + 100], n);
        }
        // Using the oldest keeps it; the next insert drops the second oldest instead.
        assert_eq!(cache.get(&[0, 100]), Some(0));
        cache.insert(vec![7_000, 7_001], 7_000);
        assert_eq!(cache.len(), KEPT);
        assert_eq!(cache.get(&[0, 100]), Some(0));
        assert_eq!(cache.get(&[1, 101]), None);
        assert_eq!(cache.get(&[7_000, 7_001]), Some(7_000));
    }

    #[test]
    fn inserting_the_same_forks_again_replaces_the_entry() {
        let mut cache = ResolutionCache::default();
        cache.insert(vec![1, 2], 10);
        cache.insert(vec![1, 2], 10);
        assert_eq!(cache.len(), 1);
    }
}
