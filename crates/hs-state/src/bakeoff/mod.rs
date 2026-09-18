//! The `PLAN.md` section 6.3 state-representation bake-off's two **losing** candidates, kept as
//! benchmark-only implementations of [`crate::repr::StateRepr`], not production code.
//!
//! See `docs/decisions/0005-state-bakeoff-methodology.md` for the published scoring weights and
//! measurement methodology (written before any candidate was measured), and
//! `docs/decisions/0006-state-bakeoff-results.md` for the results and the decision itself.
//! Candidate B, the winner, has been promoted out of this module into `crate::frames` (the
//! representation) and `crate::kv_store` (the `StateStore` built on it) -- see
//! `docs/status/02-state-and-model.md` for when and why. Candidates A
//! ([`snapshot_delta::SnapshotDeltaRepr`]) and C ([`persistent_map::PersistentMapRepr`]) remain
//! here, working and tested (`crate::kv_store`'s `tests` module still runs the same
//! ingest-and-resolve correctness scenario through both, alongside the production candidate), but
//! not wired into anything a production caller should reach for: they exist so
//! `docs/decisions/0006-state-bakeoff-results.md`'s "What would change this decision" re-runs stay
//! possible without reconstructing either candidate from scratch, per this project's "build as
//! little as is reasonable" principle (`docs/decisions/0007-build-less-reuse-more.md`) -- keeping
//! working, already-tested code around for a documented possible re-run is cheaper than deleting
//! and rewriting it if that re-run happens.
//!
//! The corpus generators these candidates (and the production one) are measured against live at
//! `crates/hs-state/corpus/generators.rs` (`crate::corpus`). The harness that ties corpus,
//! candidates and both `hs_kv` backends together and produces the numbers in the results document
//! is `src/bin/bakeoff.rs` (`cargo run -p hs-state --bin bakeoff --release`).

pub mod persistent_map;
pub mod snapshot_delta;

pub use persistent_map::{PersistentMapRepr, RootC};
pub use snapshot_delta::{RootA, SnapshotDeltaRepr};
