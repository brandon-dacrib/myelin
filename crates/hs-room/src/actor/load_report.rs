//! Where a room load's time went, phase by phase ([`LoadReport`]).
//!
//! A load ([`super::RoomActor::load`]) is a handful of passes over the room's rows, and a slow
//! one is slow in one of them: on 2026-10-15 the demo's loads took 30 s to 11 minutes with
//! nothing to say which pass. Each pass is now a phase: a `room.load.phase` span (named
//! `room.load.<phase>` in a trace, decision 0048), an observation in
//! `hs_room_load_phase_seconds{phase}`, and a line in the report the registry logs at `warn`
//! when a load takes longer than [`SLOW_LOAD`] ([`crate::registry::RoomRegistry::get_or_load`]).
//!
//! The phases of the read itself, in order: `open` (the room's metadata, its state store, the
//! rewrites it has seen), `outliers` (the outliers' rows), `state_rows` (the explicit states
//! some events were held with), `timeline` (the timeline's rows), `held_states` (the outliers
//! held with a fetched state), `extremities` (the forward extremities and the timeline's gaps)
//! and `finish` (the create event, the state store's migration marker, the cache). The
//! registry adds its own around them: `slot_wait` (one of the load slots), `blocking_wait` (a
//! blocking thread) and `register` (written-back repairs and the registry's entry).

use std::time::{Duration, Instant};

/// A load that takes longer than this, start to registration, is logged at `warn` with its
/// report: the room and the phase that took longest.
pub const SLOW_LOAD: Duration = Duration::from_secs(10);

/// Where one room load's time went: each phase's duration, in the order they ran, and what the
/// load read.
#[derive(Debug, Clone, Default)]
pub struct LoadReport {
    /// `(phase, how long it took)`, in the order the phases ran.
    pub phases: Vec<(&'static str, Duration)>,
    /// Outlier rows the load read.
    pub outliers: usize,
    /// Explicit-state rows (`Tables::state_snapshots`) the load read.
    pub state_rows: usize,
    /// Timeline rows the load read.
    pub timeline_events: usize,
    /// Explicit states whose root the load did not compute, left to the first read that needs
    /// one ([`super::RoomActor::root_after`]).
    pub deferred_roots: usize,
    /// The room's forward extremities: more than one makes the load's `finish` (and every
    /// later read of the current state) a state resolution of that many forks.
    pub extremities: usize,
}

impl LoadReport {
    /// The phases' durations together.
    #[must_use]
    pub fn total(&self) -> Duration {
        self.phases.iter().map(|(_, took)| *took).sum()
    }

    /// The phase that took longest, if any ran.
    #[must_use]
    pub fn slowest(&self) -> Option<(&'static str, Duration)> {
        self.phases.iter().copied().max_by_key(|(_, took)| *took)
    }

    /// How long `phase` took, if it ran (summed, should it have run twice).
    #[must_use]
    pub fn phase(&self, phase: &str) -> Option<Duration> {
        let mut found = None;
        for (name, took) in &self.phases {
            if *name == phase {
                *found.get_or_insert(Duration::ZERO) += *took;
            }
        }
        found
    }

    /// Records `phase` as having taken `took`, observing it in `hs_room_load_phase_seconds`.
    pub fn record(&mut self, phase: &'static str, took: Duration) {
        crate::metrics::observe_load_phase(phase, took);
        self.phases.push((phase, took));
    }

    /// The phases as `phase=1.234s` pairs, for a log line.
    #[must_use]
    pub fn phases_line(&self) -> String {
        self.phases
            .iter()
            .map(|(name, took)| format!("{name}={:.3}s", took.as_secs_f64()))
            .collect::<Vec<_>>()
            .join(" ")
    }
}

/// The phase a load is in: its span entered and its clock running, until the next phase begins
/// or [`PhaseClock::finish`] ends it.
pub(crate) struct PhaseClock {
    report: LoadReport,
    current: Option<(&'static str, Instant, tracing::span::EnteredSpan)>,
}

impl PhaseClock {
    /// A clock in no phase yet.
    pub(crate) fn new() -> Self {
        Self {
            report: LoadReport::default(),
            current: None,
        }
    }

    /// Ends the phase under way, if any, and begins `phase`.
    pub(crate) fn begin(&mut self, phase: &'static str) {
        self.end_current();
        let span = tracing::info_span!(
            "room.load.phase",
            phase,
            "otel.name" = format_args!("room.load.{phase}"),
        )
        .entered();
        self.current = Some((phase, Instant::now(), span));
    }

    fn end_current(&mut self) {
        if let Some((phase, started, span)) = self.current.take() {
            drop(span);
            self.report.record(phase, started.elapsed());
        }
    }

    /// The report so far, to add counts to.
    pub(crate) fn report_mut(&mut self) -> &mut LoadReport {
        &mut self.report
    }

    /// Ends the phase under way and hands back the report.
    pub(crate) fn finish(mut self) -> LoadReport {
        self.end_current();
        std::mem::take(&mut self.report)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_report_names_its_slowest_phase_and_sums_them() {
        let mut report = LoadReport::default();
        report.record("open", Duration::from_millis(5));
        report.record("timeline", Duration::from_millis(50));
        report.record("finish", Duration::from_millis(1));
        assert_eq!(report.total(), Duration::from_millis(56));
        assert_eq!(
            report.slowest(),
            Some(("timeline", Duration::from_millis(50)))
        );
        assert_eq!(report.phase("open"), Some(Duration::from_millis(5)));
        assert_eq!(report.phase("outliers"), None);
        assert_eq!(
            report.phases_line(),
            "open=0.005s timeline=0.050s finish=0.001s"
        );
    }

    #[test]
    fn a_clock_records_each_phase_once_it_ends() {
        let mut clock = PhaseClock::new();
        clock.begin("open");
        clock.begin("timeline");
        clock.report_mut().timeline_events = 3;
        let report = clock.finish();
        let names: Vec<_> = report.phases.iter().map(|(name, _)| *name).collect();
        assert_eq!(names, ["open", "timeline"]);
        assert_eq!(report.timeline_events, 3);
    }
}
