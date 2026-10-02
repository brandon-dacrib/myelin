//! Which members' membership events a device has already been sent under lazy loading
//! (`room.state.lazy_load_members`), so an incremental `/sync` does not send them again.
//!
//! The spec lets a server send a member event once per "session" and leave it out of later
//! incremental syncs unless `include_redundant_members` asks for it, and Sytest's "We don't send
//! redundant membership state across incremental syncs by default" expects exactly that. Synapse
//! keeps an in-memory LRU per `(user, device)` of `state_key -> event_id`; so does this, bounded
//! and in process memory only. Forgetting is always safe: a forgotten entry costs the client one
//! member event it already had, never one it lacks. A member whose membership *changed* (a new
//! event id) is sent again, whatever was remembered.
//!
//! The memory is cleared by an initial sync (`since` absent): a client starting over has no
//! members, whatever this device was sent before.

use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;

use ruma::{DeviceId, OwnedDeviceId, OwnedRoomId, OwnedUserId, RoomId, UserId};

/// How many `(room, member)` entries one device remembers before the oldest are forgotten.
pub const ENTRIES_PER_DEVICE: usize = 4_000;
/// How many devices are remembered before the least recently started one is forgotten.
pub const DEVICES: usize = 10_000;

#[derive(Default)]
struct DeviceMemory {
    /// `(room, state_key) -> event_id` of the member event this device was sent.
    sent: HashMap<(OwnedRoomId, String), String>,
    /// Insertion order, for forgetting the oldest once [`ENTRIES_PER_DEVICE`] is reached.
    order: VecDeque<(OwnedRoomId, String)>,
}

impl DeviceMemory {
    fn record(&mut self, room_id: &RoomId, state_key: String, event_id: String) {
        let key = (room_id.to_owned(), state_key);
        if self.sent.insert(key.clone(), event_id).is_none() {
            self.order.push_back(key);
            while self.order.len() > ENTRIES_PER_DEVICE {
                if let Some(oldest) = self.order.pop_front() {
                    self.sent.remove(&oldest);
                }
            }
        }
    }
}

/// The per-device memory of lazily loaded members, see the module docs.
#[derive(Default)]
pub struct LazyMembersSent {
    inner: Mutex<Memories>,
}

#[derive(Default)]
struct Memories {
    devices: HashMap<(OwnedUserId, OwnedDeviceId), DeviceMemory>,
    order: VecDeque<(OwnedUserId, OwnedDeviceId)>,
}

impl LazyMembersSent {
    /// Forgets everything `device` was sent: an initial sync starts the memory over.
    pub fn forget(&self, user_id: &UserId, device_id: &DeviceId) {
        let mut inner = self.lock();
        inner
            .devices
            .remove(&(user_id.to_owned(), device_id.to_owned()));
    }

    /// `state_key -> event_id` of every member event of `room_id` that `device` was sent.
    #[must_use]
    pub fn sent_in(
        &self,
        user_id: &UserId,
        device_id: &DeviceId,
        room_id: &RoomId,
    ) -> HashMap<String, String> {
        let inner = self.lock();
        inner
            .devices
            .get(&(user_id.to_owned(), device_id.to_owned()))
            .map(|memory| {
                memory
                    .sent
                    .iter()
                    .filter(|((room, _), _)| room == room_id)
                    .map(|((_, state_key), event_id)| (state_key.clone(), event_id.clone()))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Remembers that `device` was sent these member events (`(state_key, event_id)`) of
    /// `room_id`.
    pub fn record(
        &self,
        user_id: &UserId,
        device_id: &DeviceId,
        room_id: &RoomId,
        sent: impl IntoIterator<Item = (String, String)>,
    ) {
        let mut sent = sent.into_iter().peekable();
        if sent.peek().is_none() {
            return;
        }
        let mut inner = self.lock();
        let key = (user_id.to_owned(), device_id.to_owned());
        if !inner.devices.contains_key(&key) {
            inner.order.push_back(key.clone());
            while inner.order.len() > DEVICES {
                if let Some(oldest) = inner.order.pop_front() {
                    inner.devices.remove(&oldest);
                }
            }
        }
        let memory = inner.devices.entry(key).or_default();
        for (state_key, event_id) in sent {
            memory.record(room_id, state_key, event_id);
        }
    }

    /// How many devices are remembered, for an operator's metric and for tests.
    #[must_use]
    pub fn devices_remembered(&self) -> usize {
        self.lock().devices.len()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Memories> {
        // The memory is a cache: a poisoned lock means a panic while holding it, and whatever
        // was half-written is still only member events to send once more.
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ruma::{device_id, room_id, user_id};

    #[test]
    fn remembers_per_device_and_room_and_forgets_on_request() {
        let memory = LazyMembersSent::default();
        let alice = user_id!("@alice:lazy.test");
        let phone = device_id!("PHONE");
        let laptop = device_id!("LAPTOP");
        let room = room_id!("!a:lazy.test");
        let other = room_id!("!b:lazy.test");
        assert!(memory.sent_in(alice, phone, room).is_empty());

        memory.record(
            alice,
            phone,
            room,
            [("@bob:lazy.test".to_owned(), "$join1".to_owned())],
        );
        memory.record(
            alice,
            phone,
            other,
            [("@carol:lazy.test".to_owned(), "$join2".to_owned())],
        );
        assert_eq!(
            memory.sent_in(alice, phone, room),
            HashMap::from([("@bob:lazy.test".to_owned(), "$join1".to_owned())])
        );
        assert!(memory.sent_in(alice, laptop, room).is_empty(), "per device");

        // A changed membership replaces the remembered event.
        memory.record(
            alice,
            phone,
            room,
            [("@bob:lazy.test".to_owned(), "$leave1".to_owned())],
        );
        assert_eq!(
            memory.sent_in(alice, phone, room)["@bob:lazy.test"],
            "$leave1"
        );

        memory.forget(alice, phone);
        assert!(memory.sent_in(alice, phone, room).is_empty());
        assert!(memory.sent_in(alice, phone, other).is_empty());
        assert_eq!(memory.devices_remembered(), 0);
    }

    #[test]
    fn the_oldest_entries_and_devices_are_forgotten_past_the_bounds() {
        let memory = LazyMembersSent::default();
        let alice = user_id!("@alice:lazy.test");
        let phone = device_id!("PHONE");
        let room = room_id!("!a:lazy.test");
        memory.record(
            alice,
            phone,
            room,
            (0..=ENTRIES_PER_DEVICE).map(|i| (format!("@m{i}:lazy.test"), format!("$e{i}"))),
        );
        let sent = memory.sent_in(alice, phone, room);
        assert_eq!(sent.len(), ENTRIES_PER_DEVICE);
        assert!(!sent.contains_key("@m0:lazy.test"), "the oldest went");
        assert!(sent.contains_key(&format!("@m{ENTRIES_PER_DEVICE}:lazy.test")));

        for i in 0..DEVICES {
            let device = ruma::OwnedDeviceId::from(format!("D{i}"));
            memory.record(
                alice,
                &device,
                room,
                [("@bob:lazy.test".to_owned(), "$j".to_owned())],
            );
        }
        assert_eq!(memory.devices_remembered(), DEVICES);
        assert!(
            memory.sent_in(alice, phone, room).is_empty(),
            "the first device was the one forgotten"
        );
    }
}
