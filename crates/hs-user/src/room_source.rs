//! [`RoomSource`]: this crate's dependency on "give me a room's actor handle", without owning
//! `hs-room`'s registry.
//!
//! This track owns `hs-user` only (`.claude/agents/hs-05-sync.md`); `hs-room::registry::RoomRegistry`
//! is track 04's. Rather than naming `RoomRegistry` directly everywhere this crate needs a room
//! (which would still work today, but would make a future alternate room source -- a federation
//! or cluster-forwarding shim, for instance -- a breaking API change), every place in this crate
//! that needs a room goes through this trait, implemented once for `Arc<RoomRegistry<B>>` below.
//! `docs/rfcs/0011-room-registry-global-updates.md` explains the deeper reason a plain trait
//! object is not enough for room *discovery* (this crate also needs to learn about rooms it was
//! never told about, e.g. a user's first-ever invite) and what this crate does about it in the
//! meantime (`crate::hub`'s module docs, "The discovery gap").

use std::sync::Arc;

use hs_kv::KvBackend;
use hs_room::actor::RoomActorHandle;
use hs_room::registry::{GlobalTokenResolver, RoomRegistry};
use ruma::RoomId;

/// Errors [`RoomSource::get_or_load`] can return. A thin, `Send`-safe wrapper around
/// [`hs_room::RoomError`] (which this crate also depends on directly for its own error
/// conversions -- see `crate::error::UserError::Room`) so the trait itself does not force every
/// possible implementation to depend on `hs-room`'s error type specifically.
pub type RoomSourceError = hs_room::RoomError;

/// "Give me this room's actor handle." The one operation `crate::hub::SessionHub` and
/// `crate::sync` need from wherever rooms live.
#[async_trait::async_trait]
pub trait RoomSource<B: KvBackend + 'static>: Send + Sync {
    /// Loads (or returns the already-resident) actor handle for `room_id`.
    ///
    /// # Errors
    /// Returns [`RoomSourceError::RoomNotFound`] if the room does not exist, or any error the
    /// underlying source's load path can return.
    async fn get_or_load(&self, room_id: &RoomId) -> Result<RoomActorHandle<B>, RoomSourceError>;

    /// Wires up `resolver` so `hs-room`'s `GET /messages` can resolve a token minted by this
    /// crate's own `/sync` (see [`GlobalTokenResolver`]'s doc comment for the full "why").
    /// Default no-op: only a source backed by a real `hs_room::registry::RoomRegistry` (the
    /// production case, and the one override below) has anywhere to put this; a test double that
    /// never calls `hs-room`'s HTTP layer at all has no need for it.
    fn install_global_token_resolver(&self, _resolver: Arc<dyn GlobalTokenResolver>) {}

    /// The number of the newest update published on this source's global stream
    /// (`hs_room::registry::RoomRegistry::global_published_seq`): what `/sync` waits for the
    /// hub to have consumed before reading feeds. `0`, the default, means "do not wait".
    fn global_published_seq(&self) -> u64 {
        0
    }

    /// Every room resident in this source right now. What the hub re-reads after falling
    /// behind the global stream. Empty by default: a source with no such notion has nothing
    /// to re-read.
    async fn resident_handles(&self) -> Vec<RoomActorHandle<B>> {
        Vec::new()
    }
}

#[async_trait::async_trait]
impl<B: KvBackend + 'static> RoomSource<B> for Arc<RoomRegistry<B>> {
    async fn get_or_load(&self, room_id: &RoomId) -> Result<RoomActorHandle<B>, RoomSourceError> {
        RoomRegistry::get_or_load(self, room_id).await
    }

    fn install_global_token_resolver(&self, resolver: Arc<dyn GlobalTokenResolver>) {
        RoomRegistry::install_global_token_resolver(self, resolver);
    }

    fn global_published_seq(&self) -> u64 {
        RoomRegistry::global_published_seq(self)
    }

    async fn resident_handles(&self) -> Vec<RoomActorHandle<B>> {
        RoomRegistry::resident_handles(self).await
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    //! A trivial in-memory [`RoomSource`] for this crate's own unit tests, wrapping a real
    //! `hs_room::registry::RoomRegistry<hs_kv::memory::MemoryBackend>` (this crate is not
    //! re-implementing room storage in a mock -- it is depending on the real thing the same way
    //! production code will, just over the in-memory backend rather than Fjall).

    use super::*;
    use hs_kv::memory::MemoryBackend;
    use hs_room::identity::HomeserverIdentity;

    /// Builds a throwaway room registry (in-memory backend) usable as a [`RoomSource`].
    pub(crate) fn registry(server_name: &str) -> Arc<RoomRegistry<MemoryBackend>> {
        Arc::new(
            RoomRegistry::open(
                MemoryBackend::new(),
                HomeserverIdentity::for_tests(server_name),
            )
            .expect("opening an in-memory room registry cannot fail"),
        )
    }
}
