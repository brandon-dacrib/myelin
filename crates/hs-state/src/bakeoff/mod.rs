//! The `PLAN.md` section 6.3 state-representation bake-off: three candidate implementations of
//! resolved-state storage behind the frozen [`crate::api::StateStore`] trait, plus the shared
//! ingestion/resolution glue ([`generic_store`]) that lets all three reuse one tested
//! implementation of everything that is not state storage itself.
//!
//! See `docs/decisions/0005-state-bakeoff-methodology.md` for the published scoring weights and
//! measurement methodology (written before this module's candidates were measured), and
//! `docs/decisions/0006-state-bakeoff-results.md` for the results. The corpus generators these
//! candidates are measured against live at `crates/hs-state/corpus/generators.rs`
//! (`crate::corpus`). The harness that ties corpus, candidates and both `hs_kv` backends together
//! and produces the numbers in the results document is `src/bin/bakeoff.rs`
//! (`cargo run -p hs-state --bin bakeoff --release`).

pub mod frames;
pub mod generic_store;
pub mod persistent_map;
pub mod repr;
pub mod snapshot_delta;
mod varint;

pub use frames::{FrameRepr, RootB};
pub use generic_store::{BakeoffError, GenericStore};
pub use persistent_map::{PersistentMapRepr, RootC};
pub use repr::{BakeoffStats, StateRepr};
pub use snapshot_delta::{RootA, SnapshotDeltaRepr};
