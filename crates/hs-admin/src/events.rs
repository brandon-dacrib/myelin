//! The server-sent-events stream (RFC 0004 section 10): an `mpsc`-backed publisher with a bounded
//! replay buffer, so a reconnecting client can resume with `Last-Event-ID` instead of missing
//! events. This is the only thing the tracks that own state (04, 06, 09, 11, ...) depend on from
//! this crate: they call [`EventBus::publish`] when something happens; `GET /api/v1/events`
//! subscribes and replays.

use std::collections::VecDeque;
use std::sync::Mutex;

use tokio::sync::broadcast;

use crate::model::Event;

/// The replay buffer holds the last 10,000 events or 24 hours, whichever is smaller (RFC 0004
/// section 10). The mock and the skeleton use the count bound only; the time bound is left to a
/// real implementation's clock-driven eviction, which needs a background task this crate does not
/// run on its own.
const DEFAULT_BUFFER_CAPACITY: usize = 10_000;

/// Why [`EventBus::replay_since`] could not find where to resume.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplayOutcome {
    /// `last_event_id` is within the buffer: resume from the event after it.
    Resumed,
    /// `last_event_id` is older than the buffer's oldest retained event: the client is behind and
    /// must refetch (RFC 0004 section 10: emit `stream.reset` first).
    Behind,
    /// `last_event_id` was not recognized at all (never issued, or the server restarted): treated
    /// the same as [`ReplayOutcome::Behind`] per the RFC's "unknown but inside the buffer's window"
    /// case not applying.
    Unknown,
}

/// The in-process event bus: a broadcast channel (for live subscribers) plus a bounded ring
/// buffer (for replay on reconnect).
pub struct EventBus {
    sender: broadcast::Sender<Event>,
    buffer: Mutex<VecDeque<Event>>,
    capacity: usize,
    /// RFC 0004 section 10: "ids are monotonic per server". A plain `Ulid::new()` per event is
    /// not guaranteed to sort correctly against another `Ulid::new()` produced in the same
    /// millisecond (the random component does not order), so the bus assigns ids itself from a
    /// monotonic generator at publish time rather than trusting whatever id the caller's `Event`
    /// happened to carry.
    generator: Mutex<ulid::Generator>,
}

impl EventBus {
    pub fn new() -> Self {
        Self::with_capacity(DEFAULT_BUFFER_CAPACITY)
    }

    pub fn with_capacity(capacity: usize) -> Self {
        let (sender, _receiver) = broadcast::channel(capacity.max(16));
        Self {
            sender,
            buffer: Mutex::new(VecDeque::with_capacity(capacity)),
            capacity,
            generator: Mutex::new(ulid::Generator::new()),
        }
    }

    fn next_id(&self) -> String {
        let mut generator = self.generator.lock().expect("ulid generator poisoned");
        match generator.generate() {
            Ok(id) => id.to_string(),
            // Only fails if generating far faster than the random bits can order within one
            // millisecond; falling back to a fresh id keeps publishing available rather than
            // panicking, at the cost of that one event's strict ordering.
            Err(_) => ulid::Ulid::new().to_string(),
        }
    }

    /// Publishes one event: assigns it a fresh, monotonic id (overwriting whatever the caller
    /// set), stores it in the replay buffer, and broadcasts it to live subscribers. Subscribers
    /// that are not currently receiving (none connected) simply miss the broadcast; they will see
    /// it in their replay window if they connect before it is evicted. Returns the id-assigned
    /// event.
    pub fn publish(&self, mut event: Event) -> Event {
        event.id = self.next_id();
        {
            let mut buffer = self.buffer.lock().expect("event buffer poisoned");
            if buffer.len() >= self.capacity {
                buffer.pop_front();
            }
            buffer.push_back(event.clone());
        }
        // No receivers is not an error: it just means nobody is watching the live stream right
        // now, which is normal (the replay buffer is what makes that safe).
        let _ = self.sender.send(event.clone());
        event
    }

    /// Subscribes to the live stream. Combine with [`EventBus::replay_since`] to avoid a gap
    /// between "read the buffer" and "start receiving broadcasts".
    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.sender.subscribe()
    }

    /// The oldest event id currently retained, for the `stream.hello` event's `buffer_oldest_id`.
    pub fn oldest_id(&self) -> Option<String> {
        self.buffer
            .lock()
            .expect("event buffer poisoned")
            .front()
            .map(|e| e.id.clone())
    }

    /// Events strictly after `last_event_id`, oldest first, for resuming a connection. See
    /// [`ReplayOutcome`] for what a missing or too-old id means.
    pub fn replay_since(&self, last_event_id: Option<&str>) -> (ReplayOutcome, Vec<Event>) {
        let buffer = self.buffer.lock().expect("event buffer poisoned");
        let Some(last_id) = last_event_id else {
            return (ReplayOutcome::Resumed, buffer.iter().cloned().collect());
        };
        match buffer.iter().position(|e| e.id == last_id) {
            Some(idx) => (
                ReplayOutcome::Resumed,
                buffer.iter().skip(idx + 1).cloned().collect(),
            ),
            None => {
                let oldest = buffer.front().map(|e| e.id.as_str());
                match oldest {
                    Some(oldest) if last_id < oldest => {
                        (ReplayOutcome::Behind, buffer.iter().cloned().collect())
                    }
                    _ => (ReplayOutcome::Unknown, buffer.iter().cloned().collect()),
                }
            }
        }
    }
}

impl Default for EventBus {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replay_since_none_returns_everything() {
        let bus = EventBus::new();
        bus.publish(Event::new("a", serde_json::json!({})));
        bus.publish(Event::new("b", serde_json::json!({})));
        let (outcome, events) = bus.replay_since(None);
        assert_eq!(outcome, ReplayOutcome::Resumed);
        assert_eq!(events.len(), 2);
    }

    #[test]
    fn replay_since_known_id_resumes_after_it() {
        let bus = EventBus::new();
        bus.publish(Event::new("a", serde_json::json!({})));
        let second = bus.publish(Event::new("b", serde_json::json!({})));
        let second_id = second.id.clone();
        bus.publish(Event::new("c", serde_json::json!({})));
        let (outcome, events) = bus.replay_since(Some(&second_id));
        assert_eq!(outcome, ReplayOutcome::Resumed);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].r#type, "c");
    }

    #[test]
    fn replay_since_old_id_reports_behind() {
        let bus = EventBus::with_capacity(2);
        bus.publish(Event::new("a", serde_json::json!({})));
        let a_id_captured = {
            let buffer = bus.buffer.lock().unwrap();
            buffer.front().unwrap().id.clone()
        };
        bus.publish(Event::new("b", serde_json::json!({})));
        bus.publish(Event::new("c", serde_json::json!({}))); // evicts "a"
        let (outcome, _events) = bus.replay_since(Some(&a_id_captured));
        assert_eq!(outcome, ReplayOutcome::Behind);
    }

    #[tokio::test]
    async fn live_subscriber_receives_published_events() {
        let bus = EventBus::new();
        let mut rx = bus.subscribe();
        bus.publish(Event::new("user.suspended", serde_json::json!({})));
        let received = rx.recv().await.unwrap();
        assert_eq!(received.r#type, "user.suspended");
    }

    #[test]
    fn buffer_capacity_is_bounded() {
        let bus = EventBus::with_capacity(3);
        for i in 0..10 {
            bus.publish(Event::new(format!("event.{i}"), serde_json::json!({})));
        }
        let (_outcome, events) = bus.replay_since(None);
        assert_eq!(events.len(), 3);
        assert_eq!(events[0].r#type, "event.7");
    }
}
