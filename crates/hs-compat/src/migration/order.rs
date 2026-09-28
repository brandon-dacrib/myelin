//! The order a room's events are replayed in.
//!
//! This server stores an event only once it holds every event the event cites (its
//! `prev_events` and `auth_events`), and authorizes it against the state those imply -- exactly
//! as it does for an event arriving over federation. So a room is replayed in a topological order
//! of that graph, ties broken by depth and then by the order Synapse stored the events in (which
//! is the order they happened, for a room this server's users made).

use std::cmp::Reverse;
use std::collections::{BTreeSet, BinaryHeap, HashMap};

use serde_json::Value;

use super::model::{SynapseEvent, SynapseRoom};

/// A room's events in replay order, and those left out.
#[derive(Debug, Default)]
pub struct ReplayPlan<'a> {
    /// In an order where every event comes after the events it cites.
    pub events: Vec<&'a SynapseEvent>,
    /// `(event id, why)` for each event not replayed.
    pub skipped: Vec<(String, String)>,
}

fn cited(event: &SynapseEvent) -> Vec<String> {
    let mut ids = Vec::new();
    for key in ["prev_events", "auth_events"] {
        if let Some(Value::Array(items)) = event.json.get(key) {
            for item in items {
                match item {
                    // Room versions 3 and later: a list of ids.
                    Value::String(id) => ids.push(id.clone()),
                    // Versions 1 and 2: `[id, {hashes}]` pairs.
                    Value::Array(pair) => {
                        if let Some(Value::String(id)) = pair.first() {
                            ids.push(id.clone());
                        }
                    }
                    _ => {}
                }
            }
        }
    }
    ids
}

/// Plans `room`'s replay.
///
/// # Errors
/// Why the room cannot be replayed at all: it has no `m.room.create` in its own history, which
/// is a room Synapse joined over federation (its early history, and its create event, are held
/// elsewhere or only as outliers).
pub fn replay_order(room: &SynapseRoom) -> Result<ReplayPlan<'_>, String> {
    let mut plan = ReplayPlan::default();
    let mut candidates: Vec<&SynapseEvent> = Vec::new();
    for event in &room.events {
        if event.rejected {
            plan.skipped
                .push((event.event_id.clone(), "Synapse rejected it".to_owned()));
        } else if event.outlier {
            plan.skipped.push((
                event.event_id.clone(),
                "Synapse holds it outside the room's history (an outlier)".to_owned(),
            ));
        } else {
            candidates.push(event);
        }
    }
    let has_create = candidates.iter().any(|e| {
        e.json.get("type").and_then(Value::as_str) == Some("m.room.create")
            && e.json.get("state_key").and_then(Value::as_str) == Some("")
    });
    if !has_create {
        return Err(
            "its m.room.create is not part of its history here: this server's users joined it \
             over federation, and it is joined again after cutover rather than copied"
                .to_owned(),
        );
    }

    let index: HashMap<&str, usize> = candidates
        .iter()
        .enumerate()
        .map(|(i, e)| (e.event_id.as_str(), i))
        .collect();
    let mut waiting_on = vec![0usize; candidates.len()];
    let mut children: Vec<Vec<usize>> = vec![Vec::new(); candidates.len()];
    for (i, event) in candidates.iter().enumerate() {
        let parents: BTreeSet<usize> = cited(event)
            .iter()
            .filter_map(|id| index.get(id.as_str()).copied())
            .filter(|&p| p != i)
            .collect();
        waiting_on[i] = parents.len();
        for p in parents {
            children[p].push(i);
        }
    }
    let key = |i: usize| {
        let e = candidates[i];
        Reverse((e.depth, e.stream_ordering, e.event_id.clone(), i))
    };
    let mut ready: BinaryHeap<_> = (0..candidates.len())
        .filter(|&i| waiting_on[i] == 0)
        .map(key)
        .collect();
    let mut placed = vec![false; candidates.len()];
    while let Some(Reverse((_, _, _, i))) = ready.pop() {
        placed[i] = true;
        plan.events.push(candidates[i]);
        for &child in &children[i] {
            waiting_on[child] -= 1;
            if waiting_on[child] == 0 {
                ready.push(key(child));
            }
        }
    }
    // A cycle cannot happen in a real room; if the data says otherwise, whatever is left goes
    // last, in Synapse's order, and this server's authorization decides.
    for (i, event) in candidates.iter().enumerate() {
        if !placed[i] {
            plan.events.push(event);
        }
    }
    Ok(plan)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn event(id: &str, kind: &str, depth: i64, stream: i64, prev: &[&str]) -> SynapseEvent {
        SynapseEvent {
            event_id: id.to_owned(),
            json: json!({"type": kind, "state_key": "", "prev_events": prev, "auth_events": []}),
            depth,
            stream_ordering: stream,
            outlier: false,
            rejected: false,
        }
    }

    fn room(events: Vec<SynapseEvent>) -> SynapseRoom {
        SynapseRoom {
            room_id: "!r:x".into(),
            room_version: "11".into(),
            is_public: false,
            aliases: Vec::new(),
            events,
            redactions: Vec::new(),
        }
    }

    #[test]
    fn ancestors_come_first_even_when_synapse_stored_them_later() {
        // `b` was stored before `a` (a backfilled or delayed event), but cites it.
        let r = room(vec![
            event("$create", "m.room.create", 1, 1, &[]),
            event("$b", "m.room.message", 3, 2, &["$a"]),
            event("$a", "m.room.message", 2, 3, &["$create"]),
            event("$c", "m.room.message", 3, 4, &["$a"]),
        ]);
        let plan = replay_order(&r).unwrap();
        let ids: Vec<_> = plan.events.iter().map(|e| e.event_id.as_str()).collect();
        assert_eq!(ids, ["$create", "$a", "$b", "$c"]);
    }

    #[test]
    fn rejected_events_and_outliers_are_left_out_and_said_why() {
        let mut rejected = event("$bad", "m.room.message", 2, 2, &["$create"]);
        rejected.rejected = true;
        let mut outlier = event("$far", "m.room.member", 5, 3, &["$elsewhere"]);
        outlier.outlier = true;
        let r = room(vec![
            event("$create", "m.room.create", 1, 1, &[]),
            rejected,
            outlier,
        ]);
        let plan = replay_order(&r).unwrap();
        assert_eq!(plan.events.len(), 1);
        assert_eq!(plan.skipped.len(), 2);
    }

    #[test]
    fn a_room_without_its_create_event_is_not_replayed() {
        let mut create = event("$create", "m.room.create", 1, 1, &[]);
        create.outlier = true;
        let r = room(vec![
            create,
            event("$join", "m.room.member", 9, 2, &["$far"]),
        ]);
        assert!(replay_order(&r).unwrap_err().contains("federation"));
    }

    #[test]
    fn version_one_style_references_are_followed() {
        let mut b = event("$b", "m.room.message", 2, 1, &[]);
        b.json["prev_events"] = json!([["$create", {"sha256": "x"}]]);
        let r = room(vec![b, event("$create", "m.room.create", 1, 2, &[])]);
        let plan = replay_order(&r).unwrap();
        assert_eq!(plan.events[0].event_id, "$create");
    }
}
