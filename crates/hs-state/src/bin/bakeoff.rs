//! The `PLAN.md` section 6.3 state-representation bake-off harness.
//!
//! Runs one `(candidate, backend, scenario)` combination and prints one line of JSON metrics to
//! stdout. A separate driver script (`crates/hs-state/corpus/run_bakeoff.sh`) invokes this binary
//! once per combination -- each in its own process, wrapped in `/usr/bin/time -l` for an
//! isolated resident-memory reading -- and assembles the results into
//! `docs/decisions/0006-state-bakeoff-results.md`. See
//! `docs/decisions/0005-state-bakeoff-methodology.md` for what every field below means and how it
//! is meant to be read; this file is the "how it's measured," that document is the "why."
//!
//! Usage: `cargo run -p hs-state --release --bin bakeoff -- <candidate> <backend> <scenario> [fjall_dir]`
//! - `candidate`: `a` (snapshot+delta), `b` (frames), `c` (persistent map)
//! - `backend`: `memory`, `fjall`
//! - `scenario`: `large_churn`, `support_churn`, `policy`, `fork_backfill`, `small_rooms:<N>`
//! - `fjall_dir`: required, and must not already exist, when `backend` is `fjall`

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use hs_kv::fjall_backend::FjallBackend;
use hs_kv::memory::MemoryBackend;
use hs_model::canonical::to_canonical_object;
use hs_model::ids::EventSn;
use hs_state::api::StateStore;
use hs_state::bakeoff::{PersistentMapRepr, SnapshotDeltaRepr};
use hs_state::corpus::{self, Scenario};
use hs_state::frames::FrameRepr;
use hs_state::kv_store::KvStateStore as GenericStore;
use hs_state::repr::{ReprStats as BakeoffStats, StateRepr};
use ruma::{EventId, RoomId, RoomVersionId, UserId};
use serde::Serialize;

#[derive(Debug, Serialize)]
struct Metrics {
    candidate: String,
    backend: String,
    scenario: String,
    rooms: usize,
    events: usize,
    state_events: usize,
    ingest_ms: f64,
    bytes_on_disk: u64,
    bytes_written: u64,
    logical_bytes: u64,
    write_amplification: f64,
    lookup_warm_p50_us: f64,
    lookup_warm_p99_us: f64,
    lookup_cold_p50_us: Option<f64>,
    lookup_cold_p99_us: Option<f64>,
    diff_probe_us: Vec<(String, f64)>,
    resolution_p50_us: Option<f64>,
    resolution_p99_us: Option<f64>,
    resolution_samples: usize,
    compaction_events: Option<u64>,
    dedup_hits: Option<u64>,
    gc_ms: Option<f64>,
    gc_nodes_before: Option<u64>,
    gc_nodes_deleted: Option<u64>,
    gc_bytes_freed: Option<u64>,
}

/// One room's replay result: `(roots, resolve-forcing-event timings, total ingest time, logical
/// bytes of state change applied, state event count)`.
type ReplayResult<R> = (
    Vec<<R as StateRepr>::Root>,
    Vec<Duration>,
    Duration,
    u64,
    usize,
);

/// [`run`]'s result: filled-in metrics, the repr (handed back so the caller can do
/// candidate-specific follow-up like GC), and the final root of the last scenario/room replayed.
type RunResult<R> = (Metrics, R, Vec<<R as StateRepr>::Root>);

fn percentile_us(durations: &mut [Duration], p: f64) -> f64 {
    if durations.is_empty() {
        return 0.0;
    }
    durations.sort_unstable();
    let idx = (((durations.len() - 1) as f64) * p).round() as usize;
    durations[idx].as_secs_f64() * 1_000_000.0
}

/// Replays every event of `scenario` into `store`, using `sn_base` as the `EventSn`/event-id
/// offset (so multiple scenarios/rooms can share one physical backend without colliding event
/// ids -- see the "many small rooms" case). Returns the resulting `state_at` roots (parallel to
/// `scenario.events`), timings for events that forced `resolve()` (more than one `prev_event`),
/// total ingest wall time, and the logical bytes of state change applied (one
/// `(StateKeyId, EventSn)` worth, 12 bytes, per state event -- the write-amplification
/// denominator).
#[allow(clippy::too_many_lines)]
fn replay<R: StateRepr>(
    store: &GenericStore<R>,
    scenario: &Scenario,
    sn_base: u64,
    room_index: usize,
) -> Result<ReplayResult<R>, Box<dyn std::error::Error>> {
    let room_id = RoomId::parse(format!("!bench{room_index}:hs1"))?;
    let mut roots = Vec::with_capacity(scenario.events.len());
    let mut resolution_times = Vec::new();
    let mut logical_bytes = 0u64;
    let mut state_events = 0usize;

    let ingest_start = Instant::now();
    for (i, e) in scenario.events.iter().enumerate() {
        let sn = EventSn::new(sn_base + i as u64 + 1);
        let event_id = EventId::parse(format!("${}r{}:hs1", sn_base + i as u64 + 1, room_index))?;
        let sender = UserId::parse(format!("@{}r{}:hs1", e.sender, room_index))?;
        let auth: Vec<EventSn> = e
            .auth_events
            .iter()
            .map(|&j| EventSn::new(sn_base + j as u64 + 1))
            .collect();
        let prev: Vec<EventSn> = e
            .prev_events
            .iter()
            .map(|&j| EventSn::new(sn_base + j as u64 + 1))
            .collect();
        let content = to_canonical_object(&e.content, true)?;
        let only_prev_is_create = e.prev_events.len() == 1 && e.prev_events[0] == 0;
        let is_merge = e.prev_events.len() > 1;

        let call_start = Instant::now();
        let root = store.add_event(
            sn,
            event_id,
            room_id.clone(),
            &e.event_type,
            e.state_key.as_deref(),
            sender,
            content,
            e.depth,
            i as i64 + 1,
            &auth,
            &prev,
            only_prev_is_create,
        )?;
        if is_merge {
            resolution_times.push(call_start.elapsed());
        }

        if e.state_key.is_some() {
            state_events += 1;
            logical_bytes += 12;
        }
        roots.push(root);
    }
    let ingest_dur = ingest_start.elapsed();
    Ok((
        roots,
        resolution_times,
        ingest_dur,
        logical_bytes,
        state_events,
    ))
}

/// Runs one candidate/backend/scenario combination end to end, returning the filled-in metrics
/// except `gc_*` (candidate-specific, filled in by the caller for candidate C).
fn run<R: StateRepr + BakeoffStats + Clone>(
    repr: R,
    room_version: RoomVersionId,
    candidate: &str,
    backend: &str,
    scenario_name: &str,
    scenarios: &[Scenario],
) -> Result<RunResult<R>, Box<dyn std::error::Error>> {
    let mut all_roots: Vec<R::Root> = Vec::new();
    let mut all_resolution_times = Vec::new();
    let mut total_ingest = Duration::ZERO;
    let mut total_events = 0usize;
    let mut total_state_events = 0usize;
    let mut total_logical_bytes = 0u64;
    let mut sn_base = 0u64;
    let mut diff_probe_results: Vec<(String, f64)> = Vec::new();
    let mut sample_keys: Vec<hs_model::ids::StateKeyId> = Vec::new();

    for (room_index, scenario) in scenarios.iter().enumerate() {
        let store = GenericStore::new(room_version.clone(), repr.clone())?;
        let (roots, resolution_times, ingest_dur, logical_bytes, state_events) =
            replay(&store, scenario, sn_base, room_index)?;

        // Diff probes only make sense within one room's own history.
        for probe in &scenario.diff_probes {
            let from = roots[probe.from];
            let to = roots[probe.to];
            let mut times = Vec::with_capacity(20);
            for _ in 0..20 {
                let t0 = Instant::now();
                let _ = StateStore::diff(&store, from, to)?;
                times.push(t0.elapsed());
            }
            let label = format!("{}/{}", scenario.name, probe.label);
            diff_probe_results.push((label, percentile_us(&mut times, 0.5)));
        }

        // Collect a handful of sample keys (from this room's state events) for lookup timing.
        for e in scenario.events.iter().take(200) {
            if let Some(sk) = &e.state_key {
                sample_keys.push(store.intern(&e.event_type, sk));
            }
        }

        sn_base += scenario.events.len() as u64;
        total_events += scenario.events.len();
        total_state_events += state_events;
        total_logical_bytes += logical_bytes;
        total_ingest += ingest_dur;
        all_resolution_times.extend(resolution_times);
        if let Some(last) = roots.last() {
            all_roots.push(*last);
        }
    }

    // Warm lookup: repeat the sample keys against the last room's final root until we have a
    // few hundred timed calls.
    let mut warm_times = Vec::new();
    if let Some(&final_root) = all_roots.last()
        && !sample_keys.is_empty()
    {
        let reps = 300usize.div_ceil(sample_keys.len().max(1));
        for _ in 0..reps.max(1) {
            for &key in &sample_keys {
                let t0 = Instant::now();
                let _ = repr.get(final_root, key)?;
                warm_times.push(t0.elapsed());
            }
        }
    }

    let bytes_on_disk = repr.bytes_on_disk()?;
    let bytes_written = repr.bytes_written();
    let write_amplification = if total_logical_bytes == 0 {
        0.0
    } else {
        bytes_written as f64 / total_logical_bytes as f64
    };

    let resolution_p50 = if all_resolution_times.is_empty() {
        None
    } else {
        Some(percentile_us(&mut all_resolution_times, 0.5))
    };
    let resolution_p99 = if all_resolution_times.is_empty() {
        None
    } else {
        Some(percentile_us(&mut all_resolution_times, 0.99))
    };

    let metrics = Metrics {
        candidate: candidate.to_owned(),
        backend: backend.to_owned(),
        scenario: scenario_name.to_owned(),
        rooms: scenarios.len(),
        events: total_events,
        state_events: total_state_events,
        ingest_ms: total_ingest.as_secs_f64() * 1000.0,
        bytes_on_disk,
        bytes_written,
        logical_bytes: total_logical_bytes,
        write_amplification,
        lookup_warm_p50_us: percentile_us(&mut warm_times.clone(), 0.5),
        lookup_warm_p99_us: percentile_us(&mut warm_times.clone(), 0.99),
        lookup_cold_p50_us: None,
        lookup_cold_p99_us: None,
        diff_probe_us: diff_probe_results,
        resolution_p50_us: resolution_p50,
        resolution_p99_us: resolution_p99,
        resolution_samples: all_resolution_times.len(),
        compaction_events: repr.compaction_events(),
        dedup_hits: repr.dedup_hits(),
        gc_ms: None,
        gc_nodes_before: None,
        gc_nodes_deleted: None,
        gc_bytes_freed: None,
    };

    // Every room's own final root, not just the last: a correct GC live-set for a many-room
    // scenario is every room still "open" (every room in the corpus, here), not only the one the
    // cold-lookup/GC caller happens to spot-check with `.last()`.
    Ok((metrics, repr, all_roots))
}

/// Cold lookup: on the Fjall backend only, reopen the database from scratch before each timed
/// read (see `docs/decisions/0005-state-bakeoff-methodology.md`, "lookup cold" -- an honest
/// caveat about what "cold" can mean without OS-level cache control is recorded there).
fn measure_cold_fjall<R, F>(
    dir: &Path,
    root: R::Root,
    keys: &[hs_model::ids::StateKeyId],
    reopen: F,
) -> Result<(f64, f64), Box<dyn std::error::Error>>
where
    R: StateRepr,
    F: Fn(FjallBackend) -> Result<R, Box<dyn std::error::Error>>,
{
    let mut times = Vec::new();
    for &key in keys.iter().take(10) {
        let backend = FjallBackend::open(dir)?;
        let repr = reopen(backend)?;
        let t0 = Instant::now();
        let _ = repr.get(root, key)?;
        times.push(t0.elapsed());
    }
    Ok((
        percentile_us(&mut times.clone(), 0.5),
        percentile_us(&mut times, 0.99),
    ))
}

fn scenarios_for(scenario_arg: &str) -> (String, Vec<Scenario>) {
    if let Some(count) = scenario_arg.strip_prefix("small_rooms:") {
        let n: usize = count.parse().unwrap_or(200);
        ("small_rooms".to_owned(), corpus::many_small_rooms(n))
    } else {
        let s = match scenario_arg {
            "large_churn" => corpus::large_room_membership_churn(),
            "support_churn" => corpus::high_churn_support_room(),
            "policy" => corpus::moderation_policy_room(),
            "fork_backfill" => corpus::fork_and_backfill(),
            other => {
                eprintln!("unknown scenario {other:?}");
                std::process::exit(2);
            }
        };
        (s.name.to_owned(), vec![s])
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 4 {
        eprintln!(
            "usage: bakeoff <candidate:a|b|c> <backend:memory|fjall> <scenario> [fjall_dir]\n\
             scenario: large_churn | support_churn | policy | fork_backfill | small_rooms:<N>"
        );
        std::process::exit(2);
    }
    let candidate = args[1].as_str();
    let backend_kind = args[2].as_str();
    let scenario_arg = args[3].as_str();
    let fjall_dir: Option<PathBuf> = args.get(4).map(PathBuf::from);

    let (scenario_name, scenarios) = scenarios_for(scenario_arg);
    let room_version = RoomVersionId::V11;

    let metrics = match (candidate, backend_kind) {
        ("a", "memory") => {
            let repr = SnapshotDeltaRepr::new(MemoryBackend::default())?;
            let (m, _, _) = run(
                repr,
                room_version,
                "a",
                "memory",
                &scenario_name,
                &scenarios,
            )?;
            m
        }
        ("b", "memory") => {
            let repr = FrameRepr::new(MemoryBackend::default())?;
            let (m, _, _) = run(
                repr,
                room_version,
                "b",
                "memory",
                &scenario_name,
                &scenarios,
            )?;
            m
        }
        ("c", "memory") => {
            let repr = PersistentMapRepr::new(MemoryBackend::default())?;
            let (mut m, repr, roots) = run(
                repr,
                room_version,
                "c",
                "memory",
                &scenario_name,
                &scenarios,
            )?;
            if !roots.is_empty() {
                let t0 = Instant::now();
                let stats = repr.gc(&roots)?;
                m.gc_ms = Some(t0.elapsed().as_secs_f64() * 1000.0);
                m.gc_nodes_before = Some(stats.nodes_before);
                m.gc_nodes_deleted = Some(stats.nodes_deleted);
                m.gc_bytes_freed = Some(stats.bytes_freed);
            }
            m
        }
        ("a", "fjall") => {
            let dir = fjall_dir.expect("fjall_dir required for backend=fjall");
            let backend = FjallBackend::open(&dir)?;
            let repr = SnapshotDeltaRepr::new(backend)?;
            let (mut m, repr, roots) =
                run(repr, room_version, "a", "fjall", &scenario_name, &scenarios)?;
            let last_root = roots.last().copied();
            // Fjall holds an exclusive lock on `dir` for as long as this handle is open; drop it
            // before reopening for the cold-lookup measurement below.
            drop(repr);
            if let Some(root) = last_root {
                let keys = sample_keys_for(&scenarios);
                let (p50, p99) = measure_cold_fjall(&dir, root, &keys, |b| {
                    SnapshotDeltaRepr::new(b).map_err(|e| Box::new(e) as Box<dyn std::error::Error>)
                })?;
                m.lookup_cold_p50_us = Some(p50);
                m.lookup_cold_p99_us = Some(p99);
            }
            m
        }
        ("b", "fjall") => {
            let dir = fjall_dir.expect("fjall_dir required for backend=fjall");
            let backend = FjallBackend::open(&dir)?;
            let repr = FrameRepr::new(backend)?;
            let (mut m, repr, roots) =
                run(repr, room_version, "b", "fjall", &scenario_name, &scenarios)?;
            let last_root = roots.last().copied();
            drop(repr);
            if let Some(root) = last_root {
                let keys = sample_keys_for(&scenarios);
                let (p50, p99) = measure_cold_fjall(&dir, root, &keys, |b| {
                    FrameRepr::new(b).map_err(|e| Box::new(e) as Box<dyn std::error::Error>)
                })?;
                m.lookup_cold_p50_us = Some(p50);
                m.lookup_cold_p99_us = Some(p99);
            }
            m
        }
        ("c", "fjall") => {
            let dir = fjall_dir.expect("fjall_dir required for backend=fjall");
            let backend = FjallBackend::open(&dir)?;
            let repr = PersistentMapRepr::new(backend)?;
            let (mut m, repr, roots) =
                run(repr, room_version, "c", "fjall", &scenario_name, &scenarios)?;
            if !roots.is_empty() {
                let t0 = Instant::now();
                let stats = repr.gc(&roots)?;
                m.gc_ms = Some(t0.elapsed().as_secs_f64() * 1000.0);
                m.gc_nodes_before = Some(stats.nodes_before);
                m.gc_nodes_deleted = Some(stats.nodes_deleted);
                m.gc_bytes_freed = Some(stats.bytes_freed);
            }
            let last_root = roots.last().copied();
            drop(repr);
            if let Some(root) = last_root {
                let keys = sample_keys_for(&scenarios);
                let (p50, p99) = measure_cold_fjall(&dir, root, &keys, |b| {
                    PersistentMapRepr::new(b).map_err(|e| Box::new(e) as Box<dyn std::error::Error>)
                })?;
                m.lookup_cold_p50_us = Some(p50);
                m.lookup_cold_p99_us = Some(p99);
            }
            m
        }
        _ => {
            eprintln!("unknown candidate/backend combination: {candidate} {backend_kind}");
            std::process::exit(2);
        }
    };

    println!("{}", serde_json::to_string(&metrics)?);
    Ok(())
}

/// Reproduces the `StateKeyId`s [`run`] would have interned for the *last* scenario/room in
/// `scenarios` (the one whose final root the cold-lookup and GC measurements use -- see `main`).
///
/// Interning ([`GenericStore::intern`]) is get-or-create, assigning ids in first-seen order and
/// living entirely in [`GenericStore`]'s own bookkeeping, not in any `StateRepr`; replaying the
/// same event prefix (in the same order) through a throwaway, disposable store therefore
/// reproduces the exact same ids [`run`]'s real ingest assigned, without needing to plumb the ids
/// themselves back out of that call.
fn sample_keys_for(scenarios: &[Scenario]) -> Vec<hs_model::ids::StateKeyId> {
    let Some(scenario) = scenarios.last() else {
        return Vec::new();
    };
    let Ok(throwaway_repr) = SnapshotDeltaRepr::new(MemoryBackend::default()) else {
        return Vec::new();
    };
    let Ok(store) = GenericStore::new(RoomVersionId::V11, throwaway_repr) else {
        return Vec::new();
    };
    scenario
        .events
        .iter()
        .take(200)
        .filter_map(|e| {
            e.state_key
                .as_deref()
                .map(|sk| store.intern(&e.event_type, sk))
        })
        .collect()
}
