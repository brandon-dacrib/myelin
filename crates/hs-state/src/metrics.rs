//! Process-wide metrics of this crate: what opening and feeding the production state store
//! costs (`docs/rfcs/0025-a-room-load-that-does-not-replay-its-history.md`).
//!
//! Process-wide statics, like `hs_room::metrics` and `hs_kv::metrics`: a store is opened by a
//! room actor far from any registry, and a metric is only atomics. [`register_metrics`] puts
//! them on `/metrics`; `hs-cli` calls it once at startup.

use std::sync::LazyLock;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::time::Duration;

use prometheus_client::metrics::counter::Counter;
use prometheus_client::metrics::gauge::Gauge;
use prometheus_client::metrics::histogram::Histogram;

/// `hs_state_open_seconds`: how long `ProductionStateStore::open` took -- opening the durable
/// keyspaces and nothing else, since 2026-10-10. Before, the open was free and the room actor
/// paid for the store with a replay of the room's history.
static OPEN_SECONDS: LazyLock<Histogram> = LazyLock::new(|| {
    Histogram::new([
        0.0001, 0.00025, 0.0005, 0.001, 0.0025, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5,
        5.0,
    ])
});

/// `hs_state_events_replayed_total`: events ingested whose durable `state_at` row already
/// existed -- a replay of what the store already held. Zero once every room's actor loads
/// from the durable records instead of feeding the store its history again.
static EVENTS_REPLAYED: LazyLock<Counter> = LazyLock::new(Counter::default);

/// `hs_state_resolution_events_cached`: event records (`ResolutionEvent`s) resident in the
/// stores' caches across every open room, read back from the store for state resolution and
/// kept until evicted.
static RESOLUTION_EVENTS_CACHED: LazyLock<Gauge<i64, AtomicI64>> = LazyLock::new(Gauge::default);

/// `hs_state_migrations_total`: rooms whose durable records were built from their events on
/// first open after the change (`KvStateStore::mark_migrated`).
static MIGRATIONS: LazyLock<Counter> = LazyLock::new(Counter::default);

/// `hs_state_event_records_read_total`: event records read from the store (for state
/// resolution, or an explicit-state ingestion). Zero on an open that reads only current state.
static EVENT_RECORDS_READ: LazyLock<Counter> = LazyLock::new(Counter::default);

/// `hs_state_explicit_state_entries_unknown_total`: entries of an explicit state handed to an
/// ingestion (`KvStateStore::ingest_in`) that the store had no record of -- events the room
/// held as rejected, or never fed -- each left out of the state recorded after the event and
/// logged with the event and room. Nonzero means a room's state has a hole the log names.
static EXPLICIT_STATE_ENTRIES_UNKNOWN: LazyLock<Counter> = LazyLock::new(Counter::default);

/// A plain copy of the replayed count, for tests that run without a registry.
static EVENTS_REPLAYED_PLAIN: AtomicU64 = AtomicU64::new(0);

/// A plain copy of the unknown explicit-state entry count, for tests.
static EXPLICIT_STATE_ENTRIES_UNKNOWN_PLAIN: AtomicU64 = AtomicU64::new(0);

/// Records how long one `open` took.
pub(crate) fn observe_open(elapsed: Duration) {
    OPEN_SECONDS.observe(elapsed.as_secs_f64());
}

/// Counts `n` replayed events.
pub(crate) fn count_replayed(n: u64) {
    if n > 0 {
        EVENTS_REPLAYED.inc_by(n);
        EVENTS_REPLAYED_PLAIN.fetch_add(n, Ordering::Relaxed);
    }
}

/// `hs_state_events_replayed_total` as it stands.
#[must_use]
pub fn events_replayed() -> u64 {
    EVENTS_REPLAYED_PLAIN.load(Ordering::Relaxed)
}

/// Counts `n` explicit-state entries left out of an ingestion because the store had no record
/// of them (`hs_state_explicit_state_entries_unknown_total`).
pub(crate) fn count_explicit_state_entries_unknown(n: u64) {
    if n > 0 {
        EXPLICIT_STATE_ENTRIES_UNKNOWN.inc_by(n);
        EXPLICIT_STATE_ENTRIES_UNKNOWN_PLAIN.fetch_add(n, Ordering::Relaxed);
    }
}

/// `hs_state_explicit_state_entries_unknown_total` as it stands.
#[must_use]
pub fn explicit_state_entries_unknown() -> u64 {
    EXPLICIT_STATE_ENTRIES_UNKNOWN_PLAIN.load(Ordering::Relaxed)
}

/// Adjusts the resident event-record gauge by `delta`.
pub(crate) fn adjust_resolution_events_cached(delta: i64) {
    if delta > 0 {
        RESOLUTION_EVENTS_CACHED.inc_by(delta);
    } else if delta < 0 {
        RESOLUTION_EVENTS_CACHED.dec_by(-delta);
    }
}

/// `hs_state_resolution_events_cached` as it stands.
#[must_use]
pub fn resolution_events_cached() -> i64 {
    RESOLUTION_EVENTS_CACHED.get()
}

/// Counts one migrated room.
pub(crate) fn count_migration() {
    MIGRATIONS.inc();
}

/// `hs_state_migrations_total` as it stands.
#[must_use]
pub fn migrations() -> u64 {
    MIGRATIONS.get()
}

/// Counts `n` event records read from the store.
pub(crate) fn count_records_read(n: u64) {
    if n > 0 {
        EVENT_RECORDS_READ.inc_by(n);
    }
}

/// `hs_state_resolution_rows_read_total`: rows of the store's keyspaces read inside state
/// resolutions (records, event IDs, keys, chain-cover positions and links, frames). What a
/// resolution costs on a slow disk: about one cold read each. Since 2026-10-16 a resolution
/// reads each row it needs about once, however many forks it resolves.
static RESOLUTION_ROWS_READ: LazyLock<Counter> = LazyLock::new(Counter::default);

/// `hs_state_event_records_read_total` as it stands.
#[must_use]
pub fn event_records_read() -> u64 {
    EVENT_RECORDS_READ.get()
}

/// Registers this crate's metrics into `registry`: `hs_state_open_seconds`,
/// `hs_state_events_replayed_total`, `hs_state_resolution_events_cached`,
/// `hs_state_migrations_total`, `hs_state_event_records_read_total`,
/// `hs_state_resolution_rows_read_total` and `hs_state_explicit_state_entries_unknown_total`.
pub fn register_metrics(registry: &mut prometheus_client::registry::Registry) {
    registry.register(
        "hs_state_open_seconds",
        "Time to open a room's production state store (its durable keyspaces)",
        OPEN_SECONDS.clone(),
    );
    // Counters are registered without `_total`: the text encoder appends it.
    registry.register(
        "hs_state_events_replayed",
        "Events ingested into a state store that already held their durable state row",
        EVENTS_REPLAYED.clone(),
    );
    registry.register(
        "hs_state_resolution_events_cached",
        "Event records resident in the state stores' caches, across every open room",
        RESOLUTION_EVENTS_CACHED.clone(),
    );
    registry.register(
        "hs_state_migrations",
        "Rooms whose durable state records were built from their events on first open",
        MIGRATIONS.clone(),
    );
    registry.register(
        "hs_state_event_records_read",
        "Event records read from the store by state resolution or explicit-state ingestion",
        EVENT_RECORDS_READ.clone(),
    );
    registry.register(
        "hs_state_resolution_rows_read",
        "Rows of the state store read inside state resolutions",
        RESOLUTION_ROWS_READ.clone(),
    );
    registry.register(
        "hs_state_explicit_state_entries_unknown",
        "Entries of an explicit state (a remote join's snapshot, the state fetched for \
         backfilled history) that the state store had no record of and left out of the state \
         after the event, each logged at warn with the event and room",
        EXPLICIT_STATE_ENTRIES_UNKNOWN.clone(),
    );
}

/// Where state resolution spends its time, and how many rows the store reads, summed over the
/// process: plain atomics, for measurements (`hs-room`'s `tests/fork_resolution.rs`) and the
/// slow-resolution log line. A resolution's cost on a server whose disk is slow is mostly its
/// row reads: a cold read of a network volume (the demo's Longhorn) costs about a millisecond.
pub mod resolution_profile {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::Duration;

    /// A phase of one resolution.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum Phase {
        /// Reading each fork's full state and naming its events and keys.
        StateMaps,
        /// The forks' auth difference, from the chain-cover index.
        AuthChains,
        /// The conflicted state subgraph (v2.1 only).
        Subgraph,
        /// The resolution algorithm proper (`ruma-state-res`), event reads included.
        Algorithm,
        /// Reading and decoding event records for the algorithm (inside `Algorithm`).
        EventReads,
        /// Writing the resolved state as a root.
        Write,
    }

    const PHASES: [(Phase, &str); 6] = [
        (Phase::StateMaps, "state_maps"),
        (Phase::AuthChains, "auth_chains"),
        (Phase::Subgraph, "subgraph"),
        (Phase::Algorithm, "algorithm"),
        (Phase::EventReads, "event_reads"),
        (Phase::Write, "write"),
    ];

    /// Which of the store's keyspaces a read was of.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum KvSite {
        /// A state frame (`crate::frames`).
        Frame,
        /// An event's `state_at` root.
        StateAt,
        /// An event ID by short ID.
        EventId,
        /// An interned `(type, state_key)`, either direction.
        Key,
        /// An event record.
        Record,
        /// An event's chain-cover position.
        ChainPosition,
        /// A chain's tip.
        ChainTip,
        /// Chain-cover links (point reads and range rows).
        ChainLink,
        /// Events of a chain range.
        ChainEvent,
    }

    const SITES: [(KvSite, &str); 9] = [
        (KvSite::Frame, "frame"),
        (KvSite::StateAt, "state_at"),
        (KvSite::EventId, "event_id"),
        (KvSite::Key, "key"),
        (KvSite::Record, "record"),
        (KvSite::ChainPosition, "chain_position"),
        (KvSite::ChainTip, "chain_tip"),
        (KvSite::ChainLink, "chain_link"),
        (KvSite::ChainEvent, "chain_event"),
    ];

    static NANOS: [AtomicU64; 6] = [const { AtomicU64::new(0) }; 6];
    static KV: [AtomicU64; 9] = [const { AtomicU64::new(0) }; 9];
    /// The rows read inside resolutions only, by site.
    static RESOLUTION_KV: [AtomicU64; 9] = [const { AtomicU64::new(0) }; 9];

    thread_local! {
        /// This thread's reads, by site: a resolution runs on one thread, so the difference
        /// across it is its own reads whatever other rooms read meanwhile.
        static THREAD_KV: [std::cell::Cell<u64>; 9] = const { [const { std::cell::Cell::new(0) }; 9] };
    }

    /// This thread's rows read so far, by site (for [`ResolutionReads`]).
    fn thread_reads() -> [u64; 9] {
        THREAD_KV.with(|cells| std::array::from_fn(|i| cells[i].get()))
    }

    /// Counts the rows one resolution reads: made at its start, [`ResolutionReads::finish`]ed
    /// at its end, on the same thread.
    pub(crate) struct ResolutionReads([u64; 9]);

    impl ResolutionReads {
        /// Starts counting.
        pub(crate) fn start() -> Self {
            Self(thread_reads())
        }

        /// Adds what was read since [`ResolutionReads::start`] to the resolution sums, and
        /// answers its total.
        pub(crate) fn finish(self) -> u64 {
            let now = thread_reads();
            let mut total = 0;
            for (i, (after, before)) in now.iter().zip(self.0).enumerate() {
                let n = after.saturating_sub(before);
                RESOLUTION_KV[i].fetch_add(n, Ordering::Relaxed);
                total += n;
            }
            LAST_RESOLUTION_ROWS.with(|c| c.set(total));
            super::RESOLUTION_ROWS_READ.inc_by(total);
            total
        }
    }

    thread_local! {
        static LAST_RESOLUTION_ROWS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
    }

    /// The store rows the last resolution on this thread read: what a caller that timed it
    /// logs beside the time.
    #[must_use]
    pub fn last_resolution_rows() -> u64 {
        LAST_RESOLUTION_ROWS.with(std::cell::Cell::get)
    }
    static RESOLUTIONS: AtomicU64 = AtomicU64::new(0);
    static EVENT_FETCHES: AtomicU64 = AtomicU64::new(0);
    static AUTH_DIFFERENCE_EVENTS: AtomicU64 = AtomicU64::new(0);

    fn phase_index(phase: Phase) -> usize {
        PHASES
            .iter()
            .position(|(p, _)| *p == phase)
            .unwrap_or_default()
    }

    fn site_index(site: KvSite) -> usize {
        SITES
            .iter()
            .position(|(s, _)| *s == site)
            .unwrap_or_default()
    }

    /// Adds `took` to `phase`.
    pub(crate) fn add(phase: Phase, took: Duration) {
        let nanos = u64::try_from(took.as_nanos()).unwrap_or(u64::MAX);
        NANOS[phase_index(phase)].fetch_add(nanos, Ordering::Relaxed);
    }

    /// Counts one resolution.
    pub(crate) fn count_resolution() {
        RESOLUTIONS.fetch_add(1, Ordering::Relaxed);
    }

    /// Counts `n` calls of the algorithm's event fetch.
    pub(crate) fn count_fetches(n: u64) {
        EVENT_FETCHES.fetch_add(n, Ordering::Relaxed);
    }

    /// Counts `n` events of an auth difference handed to the algorithm.
    pub(crate) fn count_auth_difference_events(n: u64) {
        AUTH_DIFFERENCE_EVENTS.fetch_add(n, Ordering::Relaxed);
    }

    /// Counts `n` rows read from the store's keyspaces at `site` (point reads, `multi_get`
    /// keys and range rows alike; any caller, not only resolution).
    pub(crate) fn count_kv(site: KvSite, n: u64) {
        let i = site_index(site);
        KV[i].fetch_add(n, Ordering::Relaxed);
        THREAD_KV.with(|cells| cells[i].set(cells[i].get() + n));
    }

    /// Rows read from the store's keyspaces so far, by any caller.
    #[must_use]
    pub fn kv_reads() -> u64 {
        KV.iter().map(|c| c.load(Ordering::Relaxed)).sum()
    }

    /// Rows read from the store's keyspaces at `site` so far, by any caller.
    #[must_use]
    pub fn kv_reads_at(site: KvSite) -> u64 {
        KV[site_index(site)].load(Ordering::Relaxed)
    }

    /// The time summed so far in `phase`.
    #[must_use]
    pub fn total(phase: Phase) -> Duration {
        Duration::from_nanos(NANOS[phase_index(phase)].load(Ordering::Relaxed))
    }

    /// Resolutions run so far.
    #[must_use]
    pub fn resolutions() -> u64 {
        RESOLUTIONS.load(Ordering::Relaxed)
    }

    /// Sets every sum back to zero (process-wide: for a measurement run alone).
    pub fn reset() {
        for n in NANOS.iter().chain(KV.iter()).chain(RESOLUTION_KV.iter()) {
            n.store(0, Ordering::Relaxed);
        }
        for c in [&RESOLUTIONS, &EVENT_FETCHES, &AUTH_DIFFERENCE_EVENTS] {
            c.store(0, Ordering::Relaxed);
        }
    }

    /// Everything summed so far, on one line.
    #[must_use]
    pub fn line() -> String {
        use std::fmt::Write as _;
        let mut out = format!("state resolutions {}:", resolutions());
        for (phase, name) in PHASES {
            let _ = write!(out, " {name}={:.3}s", total(phase).as_secs_f64());
        }
        let _ = write!(
            out,
            " event_fetches={} auth_difference_events={}; store rows read {}:",
            EVENT_FETCHES.load(Ordering::Relaxed),
            AUTH_DIFFERENCE_EVENTS.load(Ordering::Relaxed),
            kv_reads(),
        );
        for (site, name) in SITES {
            let _ = write!(out, " {name}={}", kv_reads_at(site));
        }
        let in_resolutions: u64 = RESOLUTION_KV
            .iter()
            .map(|c| c.load(Ordering::Relaxed))
            .sum();
        let _ = write!(out, "; of them in resolutions {in_resolutions}:");
        for (i, (_, name)) in SITES.iter().enumerate() {
            let _ = write!(out, " {name}={}", RESOLUTION_KV[i].load(Ordering::Relaxed));
        }
        out
    }
}

/// [`resolution_profile::line`].
#[must_use]
pub fn resolution_profile_line() -> String {
    resolution_profile::line()
}
