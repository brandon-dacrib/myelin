//! Pusher storage (`/pushers`, `/pushers/set`) and delivery (`crate::pushers::http`).
//!
//! A "pusher" is the spec's term for one (user, device/app) subscription to push notifications —
//! `ruma::api::client::push::Pusher` is used directly as the stored record (it is already
//! `Serialize`/`Deserialize` end to end via its hand-written `Deserialize` impl, and is exactly
//! the shape `GET /pushers` must return): this crate does not define a second `Pusher` type.
//!
//! Scoped by `(user_id, app_id, pushkey)` per the spec's own uniqueness rule ("if the pushkey
//! already exists for this application ID and this user... it is updated, else the pusher is
//! added"). This implementation does not yet enforce the spec's *additional* global constraint
//! that a `(app_id, pushkey)` pair identifies one device across every user on the server (so a
//! second user registering the same pushkey should silently replace the first user's pusher for
//! it, not create a second one) — recorded in `docs/status/10-push.md`'s "Decisions made" as a
//! scoped gap: without it, two users could in principle both hold a "pusher" for the same device,
//! which would double-push to that device until one of them is deleted. Closing it needs a
//! secondary `(app_id, pushkey) -> user_id` index, the same pattern
//! `hs-auth`'s `users_by_localpart_lower` already uses.

pub mod http;
pub mod memory;
pub mod tables;

use ruma::UserId;
use ruma::api::client::push::{Pusher, PusherIds};

use crate::error::StoreError;

/// Persistence for pushers.
#[async_trait::async_trait]
pub trait PusherStore: Send + Sync {
    /// Every pusher registered for `user_id`.
    async fn get_pushers(&self, user_id: &UserId) -> Result<Vec<Pusher>, StoreError>;

    /// Creates a pusher, or replaces the one already registered with the same
    /// `(app_id, pushkey)` for this user.
    async fn set_pusher(&self, user_id: &UserId, pusher: Pusher) -> Result<(), StoreError>;

    /// Removes the pusher identified by `ids` for this user. Deleting an absent pusher is not an
    /// error (the spec's `DELETE` semantics for `/pushers/set` with no matching pusher).
    async fn delete_pusher(&self, user_id: &UserId, ids: &PusherIds) -> Result<(), StoreError>;
}
