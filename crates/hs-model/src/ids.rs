//! Identifiers.
//!
//! The string identifiers are Ruma's, re-exported so every crate uses the same types. The interned
//! short IDs are the compact integers of `PLAN.md` section 6.1: the *types* live here so that every
//! track agrees on them; *assigning* them (the interning tables) is track 01's job in `hs-tables`.

pub use ruma::{
    EventId, OwnedEventId, OwnedRoomId, OwnedServerName, OwnedUserId, RoomId, RoomVersionId,
    ServerName, UserId,
};

macro_rules! short_id {
    ($(#[$doc:meta])* $name:ident, $repr:ty, $bytes:literal) => {
        $(#[$doc])*
        #[derive(
            Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
        )]
        #[serde(transparent)]
        #[repr(transparent)]
        pub struct $name(pub $repr);

        impl $name {
            /// Wraps a raw integer.
            #[must_use]
            pub const fn new(raw: $repr) -> Self {
                Self(raw)
            }

            /// The raw integer.
            #[must_use]
            pub const fn get(self) -> $repr {
                self.0
            }

            /// Big-endian fixed-width encoding, for use as a key component.
            #[must_use]
            pub const fn to_be_bytes(self) -> [u8; $bytes] {
                self.0.to_be_bytes()
            }

            /// Decodes the big-endian fixed-width encoding.
            #[must_use]
            pub const fn from_be_bytes(bytes: [u8; $bytes]) -> Self {
                Self(<$repr>::from_be_bytes(bytes))
            }
        }

        impl From<$repr> for $name {
            fn from(raw: $repr) -> Self {
                Self(raw)
            }
        }

        impl From<$name> for $repr {
            fn from(id: $name) -> Self {
                id.0
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(f, "{}#{}", stringify!($name), self.0)
            }
        }
    };
}

short_id!(
    /// Interned room ID (`room_sn`), assigned on creation or first sight.
    RoomSn, u32, 4
);
short_id!(
    /// Interned user ID (`user_sn`), local and remote.
    UserSn, u32, 4
);
short_id!(
    /// Interned server name (`server_sn`).
    ServerSn, u32, 4
);
short_id!(
    /// Interned event ID (`event_sn`): store-wide monotonic, assigned at persist time, the primary
    /// key of the event record.
    EventSn, u64, 8
);
short_id!(
    /// Interned `(type, state_key)` pair (`state_key_id`). Membership state keys dominate.
    StateKeyId, u32, 4
);
short_id!(
    /// Interned event type (`type_id`), for filtering without string compares.
    TypeId, u32, 4
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_ids_round_trip_through_bytes_and_serde() {
        let sn = EventSn::new(0x0102_0304_0506_0708);
        assert_eq!(EventSn::from_be_bytes(sn.to_be_bytes()), sn);
        assert_eq!(sn.to_be_bytes(), [1, 2, 3, 4, 5, 6, 7, 8]);
        let json = serde_json::to_string(&sn).unwrap();
        assert_eq!(json, "72623859790382856");
        assert_eq!(serde_json::from_str::<EventSn>(&json).unwrap(), sn);

        let key = StateKeyId::new(7);
        assert_eq!(u32::from(key), 7);
        assert_eq!(StateKeyId::from(7u32), key);
        assert_eq!(key.to_string(), "StateKeyId#7");
    }

    #[test]
    fn short_ids_order_like_their_integers() {
        assert!(RoomSn::new(1) < RoomSn::new(2));
        assert!(UserSn::new(u32::MAX) > UserSn::new(0));
        assert!(ServerSn::new(3) == ServerSn::new(3));
        assert!(TypeId::new(1).to_be_bytes() < TypeId::new(256).to_be_bytes());
    }
}
