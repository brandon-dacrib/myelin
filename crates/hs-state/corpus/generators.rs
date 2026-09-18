//! Synthetic corpus generators for the `PLAN.md` section 6.3 state-representation bake-off.
//!
//! **This corpus is entirely synthetic.** `PLAN.md` section 6.3 calls for two real-room corpora
//! (a large public room's full state history joined from a throwaway server, and a real
//! high-churn support room) obtained over the network; neither is reachable from this
//! environment (no network, no running homeserver to join from -- see
//! `docs/decisions/0005-state-bakeoff-methodology.md`, "known, declared limitations"). Every
//! scenario below is generated to match the *statistical shape* those two items and the three
//! purely synthetic items (policy room, fork/backfill, many small rooms) describe, at a size
//! scaled down to complete in minutes on a shared 10-core/16 GB host rather than the "100k
//! members, years of churn" / "hundreds of thousands of membership changes" / "100k policy
//! events" `PLAN.md` names. Each generator's doc comment states its scale-down factor.
//!
//! # What is and is not modeled
//!
//! Events here carry plausible but not spec-validated `auth_events`/content: enough for
//! [`crate::state_res::v2`]'s iterative auth checks (which run as part of resolving a fork) to
//! complete without erroring, not a claim that every event here would pass
//! [`crate::auth::check_event_auth`]. This corpus exists to drive [`crate::bakeoff`]'s storage
//! representations through realistic *shapes* of state change (churn rate, key cardinality, fork
//! topology) -- state resolution's own correctness is `crate::state_res`'s cross-checked test
//! suite, not this module's job. See `docs/decisions/0005-state-bakeoff-methodology.md`, "What is
//! and is not varied."
//!
//! A generator produces a [`Scenario`]: a flat, ordered list of [`GeneratedEvent`]s referencing
//! each other by index (event `i`'s `EventSn` is always `i + 1` when replayed, so
//! `prev_events`/`auth_events` here are plain `usize` indices into the same `Scenario`), plus a
//! few named index sets ([`Scenario::diff_probes`], [`Scenario::fork_points`]) marking where the
//! bake-off harness (`src/bin/bakeoff.rs`) should take its measurements.

use ruma::RoomVersionId;
use serde_json::{Value, json};

/// One synthetic event, addressed by its position in [`Scenario::events`].
#[derive(Debug, Clone)]
pub struct GeneratedEvent {
    /// `m.room.create`, `m.room.member`, ...
    pub event_type: String,
    /// `Some` for a state event (its state key), `None` for a timeline-only event.
    pub state_key: Option<String>,
    /// The sender's localpart (always on `hs1`, matching the existing `hs-state` test corpus's
    /// convention).
    pub sender: String,
    /// The event content.
    pub content: Value,
    /// Indices (into the same `Scenario::events`) of this event's `prev_events`.
    pub prev_events: Vec<usize>,
    /// Indices of this event's `auth_events`.
    pub auth_events: Vec<usize>,
    /// The event's depth (max of its `prev_events`' depths, plus one).
    pub depth: i64,
}

/// A labeled pair of event indices to diff, at a documented approximate distance.
#[derive(Debug, Clone, Copy)]
pub struct DiffProbe {
    /// Human-readable label, e.g. `"1 apart"`, `"~10000 apart"`.
    pub label: &'static str,
    /// The "from" state, as an event index.
    pub from: usize,
    /// The "to" state, as an event index.
    pub to: usize,
}

/// One generated scenario: a room's full synthetic event history plus measurement probes.
#[derive(Debug, Clone)]
pub struct Scenario {
    /// A short, stable name (used as a results-table row label).
    pub name: &'static str,
    /// One sentence on what real-world shape this approximates and at what scale-down.
    pub note: &'static str,
    /// The room version every event in this scenario is generated for.
    pub room_version: RoomVersionId,
    /// The full, ordered synthetic event history.
    pub events: Vec<GeneratedEvent>,
    /// Where to measure `diff` cost, at a range of distances.
    pub diff_probes: Vec<DiffProbe>,
    /// Sets of concurrent forward-extremity indices, each immediately followed (in `events`) by a
    /// merge event citing all of them as `prev_events` -- exactly where `resolve()` is forced and
    /// where "resolution time on forks" is measured.
    pub fork_points: Vec<Vec<usize>>,
}

/// Builds a [`Scenario::events`] list incrementally, tracking the bookkeeping (current
/// `m.room.create`/`m.room.power_levels`/per-user `m.room.member` indices) every generator needs
/// to produce plausible `auth_events` without hand-threading indices everywhere.
struct Builder {
    events: Vec<GeneratedEvent>,
    create: usize,
    power_levels: usize,
    member_of: std::collections::HashMap<String, usize>,
}

fn power_levels_content(creator: &str) -> Value {
    json!({
        "users": {creator: 100},
        "ban": 50, "kick": 50, "redact": 50, "invite": 0,
        "users_default": 0, "events_default": 0, "state_default": 50,
    })
}

impl Builder {
    /// Starts a new room: `m.room.create`, the creator's join, and initial power levels. Returns
    /// the index of the power-levels event (the natural "tip" to build from next).
    fn start_room(creator: &str) -> Self {
        let create_event = GeneratedEvent {
            event_type: "m.room.create".to_owned(),
            state_key: Some(String::new()),
            sender: creator.to_owned(),
            content: json!({"creator": creator}),
            prev_events: vec![],
            auth_events: vec![],
            depth: 1,
        };
        let mut b = Self {
            events: vec![create_event],
            create: 0,
            power_levels: 0,
            member_of: std::collections::HashMap::new(),
        };
        let join_idx = b.push(
            "m.room.member",
            Some(creator.to_owned()),
            creator,
            json!({"membership": "join"}),
            vec![b.create],
            vec![b.create],
        );
        b.member_of.insert(creator.to_owned(), join_idx);
        let pl_idx = b.push(
            "m.room.power_levels",
            Some(String::new()),
            creator,
            power_levels_content(creator),
            vec![join_idx],
            vec![b.create, join_idx],
        );
        b.power_levels = pl_idx;
        b
    }

    fn push(
        &mut self,
        event_type: &str,
        state_key: Option<String>,
        sender: &str,
        content: Value,
        prev_events: Vec<usize>,
        auth_events: Vec<usize>,
    ) -> usize {
        let depth = prev_events
            .iter()
            .map(|&i| self.events[i].depth)
            .max()
            .unwrap_or(0)
            + 1;
        self.events.push(GeneratedEvent {
            event_type: event_type.to_owned(),
            state_key,
            sender: sender.to_owned(),
            content,
            prev_events,
            auth_events,
            depth,
        });
        self.events.len() - 1
    }

    /// The standard auth-events set for a non-membership event by `sender`, given a single
    /// current tip.
    fn std_auth(&self, sender: &str) -> Vec<usize> {
        let mut auth = vec![self.create, self.power_levels];
        if let Some(&m) = self.member_of.get(sender) {
            auth.push(m);
        }
        auth.sort_unstable();
        auth.dedup();
        auth
    }

    /// Appends a membership change for `user` (join, leave, or any other membership string),
    /// following from `tip`.
    fn membership(&mut self, user: &str, membership: &str, tip: usize) -> usize {
        let mut auth = vec![self.create, self.power_levels];
        if let Some(&m) = self.member_of.get(user) {
            auth.push(m);
        }
        auth.sort_unstable();
        auth.dedup();
        let idx = self.push(
            "m.room.member",
            Some(user.to_owned()),
            user,
            json!({"membership": membership}),
            vec![tip],
            auth,
        );
        self.member_of.insert(user.to_owned(), idx);
        idx
    }

    /// Appends a state event of `event_type`/`state_key` set by `sender`, following from `tip`.
    fn state(
        &mut self,
        event_type: &str,
        state_key: &str,
        sender: &str,
        content: Value,
        tip: usize,
    ) -> usize {
        let auth = self.std_auth(sender);
        self.push(
            event_type,
            Some(state_key.to_owned()),
            sender,
            content,
            vec![tip],
            auth,
        )
    }

    /// Appends a non-state message from `sender`, following from `tip`.
    fn message(&mut self, sender: &str, tip: usize) -> usize {
        let auth = self.std_auth(sender);
        self.push(
            "m.room.message",
            None,
            sender,
            json!({"body": "synthetic corpus message", "msgtype": "m.text"}),
            vec![tip],
            auth,
        )
    }

    /// Appends a merge event citing every index in `tips` as `prev_events`, forcing `resolve()`.
    fn merge(&mut self, sender: &str, tips: &[usize]) -> usize {
        let auth = self.std_auth(sender);
        self.push(
            "m.room.message",
            None,
            sender,
            json!({"body": "merge"}),
            tips.to_vec(),
            auth,
        )
    }
}

/// Scenario 1: a large room with heavy membership churn.
///
/// `PLAN.md` describes "on the order of 100k members and years of membership churn." Scaled down
/// by roughly 65x on member count and generates a comparable *ratio* of churn events to members
/// (each member joins, and roughly a third leave and about half of those later rejoin), for
/// `N_USERS` = 1,500 and a bit over 8,000 total membership events.
#[must_use]
pub fn large_room_membership_churn() -> Scenario {
    const N_USERS: usize = 1_500;
    let creator = "u0";
    let mut b = Builder::start_room(creator);
    let mut tip = b.power_levels;
    let mut diff_probes = Vec::new();
    let mut checkpoint_1_apart_from = None;
    let mut checkpoint_100_from = None;

    for i in 1..N_USERS {
        let user = format!("u{i}");
        tip = b.membership(&user, "join", tip);
        if i == 500 {
            checkpoint_100_from = Some(tip);
        }
        if i % 3 == 0 {
            tip = b.membership(&user, "leave", tip);
            if i % 6 == 0 {
                tip = b.membership(&user, "join", tip);
            }
        }
        if i == 100 {
            checkpoint_1_apart_from = Some(tip);
        }
    }
    let final_tip = tip;

    if let Some(from) = checkpoint_1_apart_from {
        diff_probes.push(DiffProbe {
            label: "1 apart",
            from,
            to: from + 1,
        });
    }
    if let Some(from) = checkpoint_100_from {
        diff_probes.push(DiffProbe {
            label: "100 apart",
            from,
            to: (from + 100).min(final_tip),
        });
    }
    diff_probes.push(DiffProbe {
        label: "~10000 apart",
        from: b.power_levels,
        to: final_tip,
    });

    Scenario {
        name: "large_room_membership_churn",
        note: "PLAN.md: ~100k members, years of churn. Scaled down ~65x to 1,500 users, ~8,000 membership events.",
        room_version: RoomVersionId::V11,
        events: b.events,
        diff_probes,
        fork_points: Vec::new(),
    }
}

/// Scenario 2: a high-churn support room (rapid join/leave cycling among a small set of users,
/// the MSC4242 discussion's motivating case).
///
/// `PLAN.md` describes "hundreds of thousands of membership changes." Scaled down to ~12,000
/// membership events among 250 users cycling in and out repeatedly (a support room where the same
/// small set of agents and a rotating set of visitors join and leave constantly), roughly a
/// 40-60x scale-down from the low end of "hundreds of thousands."
#[must_use]
pub fn high_churn_support_room() -> Scenario {
    const N_USERS: usize = 250;
    const CYCLES: usize = 48;
    let creator = "agent0";
    let mut b = Builder::start_room(creator);
    let mut tip = b.power_levels;
    let mut diff_probes = Vec::new();
    let mut marker_1 = None;

    for cycle in 0..CYCLES {
        for i in 0..N_USERS {
            let user = format!("visitor{i}");
            tip = b.membership(&user, "join", tip);
            tip = b.membership(&user, "leave", tip);
            if cycle == 0 && i == 10 {
                marker_1 = Some(tip);
            }
        }
    }
    let final_tip = tip;

    if let Some(from) = marker_1 {
        diff_probes.push(DiffProbe {
            label: "1 apart",
            from,
            to: from + 1,
        });
        diff_probes.push(DiffProbe {
            label: "100 apart",
            from,
            to: (from + 100).min(final_tip),
        });
    }
    diff_probes.push(DiffProbe {
        label: "~10000 apart",
        from: b.power_levels,
        to: final_tip,
    });

    Scenario {
        name: "high_churn_support_room",
        note: "PLAN.md: hundreds of thousands of membership changes. Scaled down ~40x to 250 users x 48 join/leave cycles (~24,000 membership events).",
        room_version: RoomVersionId::V11,
        events: b.events,
        diff_probes,
        fork_points: Vec::new(),
    }
}

/// Scenario 3: a moderation policy room (Draupnir-style ban/mute lists), one distinct state key
/// per rule, almost never overwritten -- the worst case for a periodic-full-snapshot
/// representation (candidate A), since every snapshot must carry every rule ever added.
///
/// `PLAN.md` asks for "100k policy state events." Scaled down ~12.5x to 8,000.
#[must_use]
pub fn moderation_policy_room() -> Scenario {
    const N_RULES: usize = 8_000;
    let creator = "moderator0";
    let mut b = Builder::start_room(creator);
    let mut tip = b.power_levels;
    let mut diff_probes = Vec::new();

    for i in 0..N_RULES {
        let rule_id = format!("rule-{i}");
        tip = b.state(
            "m.policy.rule.user",
            &rule_id,
            creator,
            json!({
                "entity": format!("@bad-actor-{i}:evil.example"),
                "recommendation": "m.ban",
                "reason": "synthetic corpus entry",
            }),
            tip,
        );
        if i == 50 {
            diff_probes.push(DiffProbe {
                label: "1 apart",
                from: tip,
                to: tip + 1,
            });
        }
        if i == 500 {
            diff_probes.push(DiffProbe {
                label: "100 apart",
                from: tip,
                to: tip + 100,
            });
        }
    }
    let final_tip = tip;
    diff_probes.push(DiffProbe {
        label: "~7900 apart",
        from: b.power_levels,
        to: final_tip,
    });

    Scenario {
        name: "moderation_policy_room",
        note: "PLAN.md: 100k policy state events. Scaled down ~12.5x to 8,000, each a distinct state key (no overwrites) -- the stress case for periodic-full-snapshot representations.",
        room_version: RoomVersionId::V11,
        events: b.events,
        diff_probes,
        fork_points: Vec::new(),
    }
}

/// Scenario 4: fork and backfill -- repeated rounds of several concurrent forward extremities
/// (simulating a federated room where multiple servers send events before seeing each other's
/// latest) merged back together, plus long-distance diff probes standing in for deep backfill
/// (this environment cannot run a real multi-server federation partition/backfill; see the module
/// docs and `docs/decisions/0005-state-bakeoff-methodology.md`).
///
/// `PLAN.md` asks for "N concurrent forward extremities with periodic merges" and "deep backfill
/// after a long partition." 25 fork/merge rounds, 6 concurrent branches per round, 12 events per
/// branch: ~2,700 events, with `fork_points` marking every merge for the harness to time
/// `resolve()` on, and `diff_probes` at 1/100/~2000 apart standing in for the backfill case.
#[must_use]
pub fn fork_and_backfill() -> Scenario {
    const ROUNDS: usize = 25;
    const BRANCHES: usize = 6;
    const EVENTS_PER_BRANCH: usize = 12;
    let creator = "u0";
    let mut b = Builder::start_room(creator);
    // A handful of members so branches have distinct senders (like distinct servers each sending
    // its own events into the fork).
    let mut tip = b.power_levels;
    let mut senders = Vec::new();
    for i in 1..(BRANCHES + 1) {
        let user = format!("u{i}");
        tip = b.membership(&user, "join", tip);
        senders.push(user);
    }
    let base_after_joins = tip;

    let mut fork_points = Vec::new();
    let mut diff_probes = Vec::new();
    let mut marker_1 = None;
    let mut marker_100 = None;
    let mut cur_tip = base_after_joins;

    for round in 0..ROUNDS {
        let mut branch_tips = Vec::new();
        for sender in &senders {
            let mut branch_tip = cur_tip;
            for e in 0..EVENTS_PER_BRANCH {
                // Every third event is a state change (each branch racing to set the room topic
                // to its own value -- a real conflict `resolve()` must pick a winner for, and
                // what makes the diff probes below measure an actual state difference rather than
                // an identical-root no-op: without this, every event in this scenario after the
                // initial joins would be a non-state `m.room.message` and every `state_at` root
                // in it would be identical, which would make every diff probe trivially free
                // regardless of candidate).
                branch_tip = if e % 3 == 0 {
                    b.state(
                        "m.room.topic",
                        "",
                        sender,
                        json!({"topic": format!("round {round} branch {sender}")}),
                        branch_tip,
                    )
                } else {
                    b.message(sender, branch_tip)
                };
                if round == 0 && e == 0 && marker_1.is_none() {
                    marker_1 = Some(branch_tip);
                }
            }
            branch_tips.push(branch_tip);
        }
        fork_points.push(branch_tips.clone());
        cur_tip = b.merge(creator, &branch_tips);
        if round == 3 {
            marker_100 = Some(cur_tip);
        }
    }
    let final_tip = cur_tip;

    if let Some(from) = marker_1 {
        diff_probes.push(DiffProbe {
            label: "1 apart",
            from,
            to: from + 1,
        });
    }
    if let Some(from) = marker_100 {
        diff_probes.push(DiffProbe {
            label: "100 apart",
            from,
            to: (from + 100).min(final_tip),
        });
    }
    diff_probes.push(DiffProbe {
        label: "deep backfill stand-in (full history apart)",
        from: base_after_joins,
        to: final_tip,
    });

    Scenario {
        name: "fork_and_backfill",
        note: "PLAN.md: N concurrent forward extremities with periodic merges, deep backfill after a long partition. Modeled as 25 fork/merge rounds x 6 branches x 12 events; deep backfill approximated by a long-distance diff probe rather than a real network partition (no federation stack available here).",
        room_version: RoomVersionId::V11,
        events: b.events,
        diff_probes,
        fork_points,
    }
}

/// Scenario 5: one small, ordinary room (create, a handful of joins, power levels, name, topic,
/// a few messages) -- the common case. `PLAN.md` asks for "a thousand small rooms"; see
/// [`many_small_rooms`], which calls this once per room. Scaled down: 12 events per room here.
#[must_use]
pub fn small_ordinary_room(room_index: usize) -> Scenario {
    let creator = format!("r{room_index}u0");
    let mut b = Builder::start_room(&creator);
    let mut tip = b.power_levels;
    for i in 1..5 {
        let user = format!("r{room_index}u{i}");
        tip = b.membership(&user, "join", tip);
    }
    tip = b.state(
        "m.room.name",
        "",
        &creator,
        json!({"name": format!("Room {room_index}")}),
        tip,
    );
    tip = b.state("m.room.topic", "", &creator, json!({"topic": "chat"}), tip);
    for _ in 0..3 {
        tip = b.message(&creator, tip);
    }
    Scenario {
        name: "small_ordinary_room",
        note: "one instance of PLAN.md's 'a thousand small rooms' scenario; see many_small_rooms for the count actually used.",
        room_version: RoomVersionId::V11,
        events: b.events,
        diff_probes: vec![DiffProbe {
            label: "1 apart",
            from: 0,
            to: 1,
        }],
        fork_points: Vec::new(),
    }
}

/// `PLAN.md`'s "a thousand small rooms," scaled down to `count` rooms (the bake-off harness
/// passes the documented, tractable count -- see its own scale-down note when it runs this).
#[must_use]
pub fn many_small_rooms(count: usize) -> Vec<Scenario> {
    (0..count).map(small_ordinary_room).collect()
}

/// Every named scenario except [`many_small_rooms`] (which is parameterized by count and run
/// separately by the harness).
#[must_use]
pub fn all_single_room_scenarios() -> Vec<Scenario> {
    vec![
        large_room_membership_churn(),
        high_churn_support_room(),
        moderation_policy_room(),
        fork_and_backfill(),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_well_formed(s: &Scenario) {
        assert!(!s.events.is_empty(), "{}: no events generated", s.name);
        for (i, e) in s.events.iter().enumerate() {
            for &p in &e.prev_events {
                assert!(p < i, "{}: event {i} cites a later prev_event {p}", s.name);
            }
            for &a in &e.auth_events {
                assert!(a < i, "{}: event {i} cites a later auth_event {a}", s.name);
            }
        }
        for probe in &s.diff_probes {
            assert!(
                probe.from < s.events.len(),
                "{}: probe.from out of range",
                s.name
            );
            assert!(
                probe.to < s.events.len(),
                "{}: probe.to out of range",
                s.name
            );
        }
        for fork in &s.fork_points {
            for &idx in fork {
                assert!(idx < s.events.len(), "{}: fork point out of range", s.name);
            }
        }
    }

    #[test]
    fn every_scenario_is_well_formed() {
        for s in all_single_room_scenarios() {
            assert_well_formed(&s);
        }
        for s in many_small_rooms(5) {
            assert_well_formed(&s);
        }
    }

    #[test]
    fn small_room_count_matches_request() {
        assert_eq!(many_small_rooms(37).len(), 37);
    }
}
