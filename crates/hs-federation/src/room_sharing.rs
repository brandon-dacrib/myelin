//! Which other servers this one shares a room with (decision 0042): the fact that decides
//! whether a federation destination is a relationship (kept) or state (forgettable).
//!
//! A room is shared with a server when both it and this server have a joined member in it.
//! [`RoomSharing`] is what `hs-cli` implements over its room registry; [`Sharing`] is the map
//! built from one reading of it, which [`crate::admin_source::DestinationStoreSource`] keeps
//! for a short while between reads (one reading loads every room) and refreshes before it
//! forgets anything.

use std::collections::{BTreeMap, BTreeSet};

use async_trait::async_trait;

/// One room, as [`RoomSharing::shared_rooms`] reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SharedRoom {
    pub room_id: String,
    /// Every server with a joined member in it, this one included when it has one.
    pub servers: Vec<String>,
}

/// Where the rooms come from.
#[async_trait]
pub trait RoomSharing: Send + Sync {
    /// Every room this server holds, with the servers that have a joined member in it. A room
    /// this server has no joined member in may be reported (with the others' servers) or left
    /// out; [`Sharing::new`] counts only rooms this server is in.
    ///
    /// # Errors
    /// A description of what failed; the caller treats the sharing as unknown.
    async fn shared_rooms(&self) -> Result<Vec<SharedRoom>, String>;
}

/// The map built from one reading of [`RoomSharing`]: how many rooms each other server shares
/// with this one, and which rooms this one is in.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Sharing {
    rooms_by_server: BTreeMap<String, u64>,
    joined_rooms: BTreeSet<String>,
}

impl Sharing {
    /// Folds `rooms` for `own_server_name`: a room counts only when this server has a joined
    /// member in it, and then every other server in it shares it.
    #[must_use]
    pub fn new(own_server_name: &str, rooms: Vec<SharedRoom>) -> Self {
        let mut sharing = Self::default();
        for room in rooms {
            if !room.servers.iter().any(|s| s == own_server_name) {
                continue;
            }
            sharing.joined_rooms.insert(room.room_id);
            let others: BTreeSet<&str> = room
                .servers
                .iter()
                .map(String::as_str)
                .filter(|s| *s != own_server_name)
                .collect();
            for server in others {
                *sharing
                    .rooms_by_server
                    .entry(server.to_owned())
                    .or_default() += 1;
            }
        }
        sharing
    }

    /// How many rooms this server shares with `server_name`.
    #[must_use]
    pub fn rooms_shared_with(&self, server_name: &str) -> u64 {
        self.rooms_by_server
            .get(server_name)
            .copied()
            .unwrap_or_default()
    }

    /// Whether this server still has a joined member in `room_id`.
    #[must_use]
    pub fn still_in(&self, room_id: &str) -> bool {
        self.joined_rooms.contains(room_id)
    }

    /// How many rooms this server is in.
    #[must_use]
    pub fn joined_room_count(&self) -> usize {
        self.joined_rooms.len()
    }

    /// Every server this one shares at least one room with, by name.
    #[must_use]
    pub fn servers(&self) -> Vec<&str> {
        self.rooms_by_server.keys().map(String::as_str).collect()
    }
}

/// A [`RoomSharing`] over a fixed list, for tests.
#[derive(Debug, Default)]
pub struct FixedRoomSharing {
    rooms: std::sync::Mutex<Vec<SharedRoom>>,
}

impl FixedRoomSharing {
    /// Shares nothing yet.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Replaces what is reported.
    pub fn set(&self, rooms: Vec<SharedRoom>) {
        *self
            .rooms
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = rooms;
    }
}

#[async_trait]
impl RoomSharing for FixedRoomSharing {
    async fn shared_rooms(&self) -> Result<Vec<SharedRoom>, String> {
        Ok(self
            .rooms
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn room(id: &str, servers: &[&str]) -> SharedRoom {
        SharedRoom {
            room_id: id.to_owned(),
            servers: servers.iter().map(|s| (*s).to_owned()).collect(),
        }
    }

    #[test]
    fn a_room_is_shared_only_when_this_server_is_in_it() {
        let sharing = Sharing::new(
            "us.example",
            vec![
                room(
                    "!a:us.example",
                    &["us.example", "them.example", "other.example"],
                ),
                room("!b:us.example", &["us.example", "them.example"]),
                // Left: nobody of ours is joined any more.
                room("!c:us.example", &["them.example", "gone.example"]),
                // Only us.
                room("!d:us.example", &["us.example"]),
            ],
        );
        assert_eq!(sharing.rooms_shared_with("them.example"), 2);
        assert_eq!(sharing.rooms_shared_with("other.example"), 1);
        assert_eq!(sharing.rooms_shared_with("gone.example"), 0);
        assert_eq!(
            sharing.rooms_shared_with("us.example"),
            0,
            "never with itself"
        );
        assert!(sharing.still_in("!a:us.example"));
        assert!(sharing.still_in("!d:us.example"));
        assert!(!sharing.still_in("!c:us.example"));
        assert_eq!(sharing.joined_room_count(), 3);
        assert_eq!(sharing.servers(), vec!["other.example", "them.example"]);
    }
}
