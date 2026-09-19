//! Consumer-defined data-source traits, following the pattern of
//! `crates/hs-federation/src/room_source.rs`: the trait lives here, next to the handlers that
//! call it, and the implementation lives in whichever crate owns the real data (07's user store
//! for [`UserDirectory`]). This crate depends on nothing beyond `async_trait`, `serde` and
//! `thiserror` to describe the trait; the real implementation is free to depend on `hs-admin`
//! without a cycle.
//!
//! Publish this contract verbatim — do not add or remove trait methods without updating whoever
//! is implementing them, since that happens in a separate crate track 15 does not own.

use std::collections::HashMap;
use std::sync::RwLock;

use async_trait::async_trait;
use serde::Deserialize;

use crate::model::{AdminRoom, AdminUser, ExternalId, ThreePid};

/// Why a data-source call failed. Mirrors [`crate::auth::AuthError`]'s "only unavailable escapes
/// as something other than the obvious status" shape: [`SourceError::NotFound`] maps to `404
/// not-found`, [`SourceError::Unavailable`] to `503 unavailable`, [`SourceError::Invalid`] to
/// `400 validation-failed`, [`SourceError::Conflict`] to `409 conflict` (see
/// [`SourceError::to_problem`]).
#[derive(Debug, thiserror::Error)]
pub enum SourceError {
    #[error("not found")]
    NotFound,
    #[error("the data source is temporarily unavailable: {0}")]
    Unavailable(String),
    #[error("invalid request: {0}")]
    Invalid(String),
    #[error("conflict: {0}")]
    Conflict(String),
}

impl SourceError {
    /// Maps this error onto the RFC 9457 problem catalog (RFC 0004 section 3.5).
    pub fn to_problem(&self) -> hs_http::Problem {
        match self {
            SourceError::NotFound => hs_http::Problem::not_found().with_detail(self.to_string()),
            SourceError::Unavailable(detail) => {
                hs_http::Problem::unavailable().with_detail(detail.clone())
            }
            SourceError::Invalid(detail) => {
                hs_http::Problem::validation_failed().with_detail(detail.clone())
            }
            SourceError::Conflict(detail) => {
                hs_http::Problem::conflict().with_detail(detail.clone())
            }
        }
    }
}

/// A `users.create` request (the OpenAPI `UserCreate` schema): either `localpart` or a full
/// `user_id` is expected to be set (validated by the handler, not this struct); which one a real
/// implementation needs is a detail of how it resolves a homeserver domain, which this crate does
/// not own — see [`UserDirectory::create_user`]'s doc comment.
///
/// Derives [`Deserialize`] directly (field-for-field match with the OpenAPI `UserCreate` schema)
/// so `router::users_create` can parse the request body straight into this type rather than a
/// separate wire struct that would need to be kept in sync with it by hand.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct UserCreateRequest {
    pub localpart: Option<String>,
    pub user_id: Option<String>,
    pub password: Option<String>,
    pub display_name: Option<String>,
    pub admin: bool,
    pub user_type: Option<String>,
    pub threepids: Vec<ThreePid>,
    pub external_ids: Vec<ExternalId>,
}

/// The match criteria `users.lookup` accepts (the OpenAPI operation's `medium`+`address` /
/// `provider`+`external_id` query parameter pairs). Exactly one variant per call; the handler is
/// responsible for rejecting a request that names neither pair (`400 validation-failed`) before
/// calling [`UserDirectory::lookup_user`].
#[derive(Debug, Clone)]
pub enum UserLookupQuery {
    Threepid {
        medium: String,
        address: String,
    },
    ExternalId {
        provider: String,
        external_id: String,
    },
}

/// Filters for `GET /users` (RFC 0004 section 4.2 / the OpenAPI `users.list` operation).
/// `q` is free text matched against `user_id` and `display_name`, case-insensitively.
#[derive(Debug, Default, Clone)]
pub struct UserFilter {
    pub q: Option<String>,
    pub admin: Option<bool>,
    pub deactivated: Option<bool>,
    pub locked: Option<bool>,
    pub suspended: Option<bool>,
    pub guests: Option<bool>,
}

/// The user-directory seam `hs-admin`'s `/users` handlers call. Implemented by track 07 against
/// its real user store; [`InMemoryUserDirectory`] below is a fake for this crate's own tests.
///
/// `create_user`/`lookup_user`/`check_localpart_available` were added in session 6, after 07 had
/// already implemented the first five methods against its real store. Adding them as *required*
/// methods would have broken that implementation's build the moment this crate's `Cargo.lock`
/// picked it up; each ships a default body answering `SourceError::Unavailable` (session 6's
/// handlers turn that into an honest `503`, never a fake `200`) so an existing implementor keeps
/// compiling untouched and only needs to override the methods it actually wants to back with real
/// data. See `docs/status/15-admin-api-and-modules.md` "Decisions made".
#[async_trait]
pub trait UserDirectory: Send + Sync + 'static {
    async fn get_user(&self, user_id: &str) -> Result<Option<AdminUser>, SourceError>;
    async fn list_users(&self, filter: &UserFilter) -> Result<Vec<AdminUser>, SourceError>;
    async fn set_admin(&self, user_id: &str, admin: bool) -> Result<(), SourceError>;
    async fn set_locked(&self, user_id: &str, locked: bool) -> Result<(), SourceError>;
    async fn set_deactivated(&self, user_id: &str, deactivated: bool) -> Result<(), SourceError>;

    /// Creates a new user (`users.create`). A real implementation resolves `request.localpart`
    /// (plus its own homeserver domain, which this crate does not know) or `request.user_id` into
    /// a full Matrix user id; `SourceError::Invalid` for a request naming neither or naming both
    /// inconsistently, `SourceError::Conflict` if the user already exists.
    async fn create_user(&self, request: UserCreateRequest) -> Result<AdminUser, SourceError> {
        let _ = request;
        Err(SourceError::Unavailable(
            "this user directory does not support creating users yet".to_string(),
        ))
    }

    /// Looks up a user by 3PID or external id (`users.lookup`). `Ok(None)` for "no such user",
    /// distinct from `SourceError::NotFound`, matching `get_user`'s convention.
    async fn lookup_user(&self, query: UserLookupQuery) -> Result<Option<AdminUser>, SourceError> {
        let _ = query;
        Err(SourceError::Unavailable(
            "this user directory does not support lookup by 3PID or external id yet".to_string(),
        ))
    }

    /// Whether `localpart` is free to register (`users.availability`).
    async fn check_localpart_available(&self, localpart: &str) -> Result<bool, SourceError> {
        let _ = localpart;
        Err(SourceError::Unavailable(
            "this user directory does not support availability checks yet".to_string(),
        ))
    }
}

/// An in-memory [`UserDirectory`] for this crate's own handler tests. Not a production
/// implementation: no persistence, no real "guest" concept beyond `user_type == "guest"` (this
/// crate's own convention, since RFC 0004 does not define one — see
/// `docs/status/15-admin-api-and-modules.md` "Decisions made").
#[derive(Debug, Default)]
pub struct InMemoryUserDirectory {
    users: RwLock<HashMap<String, AdminUser>>,
}

impl InMemoryUserDirectory {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Inserts (or replaces) one user, keyed by `user.user_id`. Consuming-and-returning `self` so
    /// callers can chain: `InMemoryUserDirectory::new().with_user(a).with_user(b)`.
    pub fn with_user(self, user: AdminUser) -> Self {
        self.users
            .write()
            .expect("InMemoryUserDirectory lock poisoned")
            .insert(user.user_id.clone(), user);
        self
    }
}

fn is_guest(user: &AdminUser) -> bool {
    user.user_type.as_deref() == Some("guest")
}

#[async_trait]
impl UserDirectory for InMemoryUserDirectory {
    async fn get_user(&self, user_id: &str) -> Result<Option<AdminUser>, SourceError> {
        Ok(self
            .users
            .read()
            .expect("InMemoryUserDirectory lock poisoned")
            .get(user_id)
            .cloned())
    }

    async fn list_users(&self, filter: &UserFilter) -> Result<Vec<AdminUser>, SourceError> {
        let users = self
            .users
            .read()
            .expect("InMemoryUserDirectory lock poisoned");
        let q = filter.q.as_ref().map(|q| q.to_lowercase());
        let mut items: Vec<AdminUser> = users
            .values()
            .filter(|u| {
                if let Some(admin) = filter.admin
                    && u.admin != admin
                {
                    return false;
                }
                if let Some(deactivated) = filter.deactivated
                    && u.deactivated != deactivated
                {
                    return false;
                }
                if let Some(locked) = filter.locked
                    && u.locked != locked
                {
                    return false;
                }
                if let Some(suspended) = filter.suspended
                    && u.suspended != suspended
                {
                    return false;
                }
                if let Some(guests) = filter.guests
                    && is_guest(u) != guests
                {
                    return false;
                }
                if let Some(q) = &q {
                    let matches_id = u.user_id.to_lowercase().contains(q.as_str());
                    let matches_name = u
                        .display_name
                        .as_deref()
                        .map(|n| n.to_lowercase().contains(q.as_str()))
                        .unwrap_or(false);
                    if !matches_id && !matches_name {
                        return false;
                    }
                }
                true
            })
            .cloned()
            .collect();
        items.sort_by(|a, b| a.user_id.cmp(&b.user_id));
        Ok(items)
    }

    async fn set_admin(&self, user_id: &str, admin: bool) -> Result<(), SourceError> {
        let mut users = self
            .users
            .write()
            .expect("InMemoryUserDirectory lock poisoned");
        let user = users.get_mut(user_id).ok_or(SourceError::NotFound)?;
        user.admin = admin;
        Ok(())
    }

    async fn set_locked(&self, user_id: &str, locked: bool) -> Result<(), SourceError> {
        let mut users = self
            .users
            .write()
            .expect("InMemoryUserDirectory lock poisoned");
        let user = users.get_mut(user_id).ok_or(SourceError::NotFound)?;
        user.locked = locked;
        Ok(())
    }

    async fn set_deactivated(&self, user_id: &str, deactivated: bool) -> Result<(), SourceError> {
        let mut users = self
            .users
            .write()
            .expect("InMemoryUserDirectory lock poisoned");
        let user = users.get_mut(user_id).ok_or(SourceError::NotFound)?;
        user.deactivated = deactivated;
        Ok(())
    }
}

/// Filters for `GET /rooms` (the OpenAPI `rooms.list` operation). `q` is free text matched
/// against `room_id`, `name`, `topic` and `canonical_alias`, case-insensitively, mirroring
/// [`UserFilter::q`]'s convention.
#[derive(Debug, Default, Clone)]
pub struct RoomFilter {
    pub q: Option<String>,
    pub public: Option<bool>,
    pub empty: Option<bool>,
    pub blocked: Option<bool>,
    pub encrypted: Option<bool>,
    pub federatable: Option<bool>,
    pub room_type: Option<String>,
    pub version: Option<String>,
}

/// The room-directory seam `hs-admin`'s `/rooms` handlers call. **Not implemented against a real
/// backing store by this session** — `crates/hs-room` (the real source of room state) is owned by
/// another track and was explicitly out of scope for this session's assignment (see
/// `docs/status/15-admin-api-and-modules.md` "Interfaces needed" for the exact contract track 04
/// should implement this against). [`InMemoryRoomDirectory`] is a fake for this crate's own tests
/// only; `router::AdminState::rooms` defaults to `None`, so `GET /rooms`, `GET /rooms/{room_id}`,
/// and the three moderation actions all answer an honest `503 unavailable` until a real
/// implementation is wired in with `AdminState::with_rooms`.
///
/// Contract notes for whoever implements this against `hs-room`:
/// - `list_rooms`/`get_room` should read from whatever room-summary index `hs-room` already
///   maintains for its own listing needs; this trait does not prescribe how membership counts or
///   `state_events_count` are computed, only that the resulting [`AdminRoom`] be accurate as of
///   the call.
/// - `set_blocked` both flips `AdminRoom::blocked`/`blocked_reason` **and** is expected to have a
///   real effect on the room (RFC 0004: a blocked room rejects new joins and events from local
///   users going forward) — this seam only carries the flag across the boundary; enforcing it is
///   `hs-room`'s job once it reads the flag back.
/// - `make_admin` grants `user_id` the room's highest power level (or `100`, whichever is lower of
///   "highest used" and "the room's own admin threshold" — see Synapse's `make_room_admin` for the
///   reference behavior this mirrors, read for behavior only, never copied per this track's brief)
///   by sending a new `m.room.power_levels` state event on `user_id`'s behalf. It does not change
///   any field of [`AdminRoom`] itself, which is why the trait returns `()` rather than an updated
///   room; the handler re-fetches via `get_room` to build its response, same as the user toggles do.
#[async_trait]
pub trait RoomDirectory: Send + Sync + 'static {
    async fn get_room(&self, room_id: &str) -> Result<Option<AdminRoom>, SourceError>;
    async fn list_rooms(&self, filter: &RoomFilter) -> Result<Vec<AdminRoom>, SourceError>;

    /// Sets `AdminRoom::blocked`/`blocked_reason` and, on a real implementation, enforces it
    /// (rejecting new joins/events). `SourceError::NotFound` if the room does not exist.
    async fn set_blocked(
        &self,
        room_id: &str,
        blocked: bool,
        reason: Option<String>,
    ) -> Result<(), SourceError>;

    /// Grants `user_id` room-admin power level in `room_id` (`rooms.make_admin`).
    /// `SourceError::NotFound` if the room does not exist; `SourceError::Invalid` if `user_id` is
    /// not a member of the room.
    async fn make_admin(&self, room_id: &str, user_id: &str) -> Result<(), SourceError>;
}

/// An in-memory [`RoomDirectory`] for this crate's own handler tests, following
/// [`InMemoryUserDirectory`]'s shape exactly. Not a production implementation: `make_admin` only
/// validates the room exists (there is no membership list here to check `user_id` against).
#[derive(Debug, Default)]
pub struct InMemoryRoomDirectory {
    rooms: RwLock<HashMap<String, AdminRoom>>,
}

impl InMemoryRoomDirectory {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Inserts (or replaces) one room, keyed by `room.room_id`.
    pub fn with_room(self, room: AdminRoom) -> Self {
        self.rooms
            .write()
            .expect("InMemoryRoomDirectory lock poisoned")
            .insert(room.room_id.clone(), room);
        self
    }
}

#[async_trait]
impl RoomDirectory for InMemoryRoomDirectory {
    async fn get_room(&self, room_id: &str) -> Result<Option<AdminRoom>, SourceError> {
        Ok(self
            .rooms
            .read()
            .expect("InMemoryRoomDirectory lock poisoned")
            .get(room_id)
            .cloned())
    }

    async fn list_rooms(&self, filter: &RoomFilter) -> Result<Vec<AdminRoom>, SourceError> {
        let rooms = self
            .rooms
            .read()
            .expect("InMemoryRoomDirectory lock poisoned");
        let q = filter.q.as_ref().map(|q| q.to_lowercase());
        let mut items: Vec<AdminRoom> = rooms
            .values()
            .filter(|r| {
                if let Some(public) = filter.public
                    && r.public != public
                {
                    return false;
                }
                if let Some(empty) = filter.empty
                    && (r.joined_members_count == 0) != empty
                {
                    return false;
                }
                if let Some(blocked) = filter.blocked
                    && r.blocked != blocked
                {
                    return false;
                }
                if let Some(encrypted) = filter.encrypted
                    && r.encrypted != encrypted
                {
                    return false;
                }
                if let Some(federatable) = filter.federatable
                    && r.federatable != federatable
                {
                    return false;
                }
                if let Some(room_type) = &filter.room_type
                    && r.room_type.as_deref() != Some(room_type.as_str())
                {
                    return false;
                }
                if let Some(version) = &filter.version
                    && &r.version != version
                {
                    return false;
                }
                if let Some(q) = &q {
                    let hay = [
                        Some(r.room_id.as_str()),
                        r.name.as_deref(),
                        r.topic.as_deref(),
                        r.canonical_alias.as_deref(),
                    ];
                    if !hay
                        .iter()
                        .flatten()
                        .any(|s| s.to_lowercase().contains(q.as_str()))
                    {
                        return false;
                    }
                }
                true
            })
            .cloned()
            .collect();
        items.sort_by(|a, b| a.room_id.cmp(&b.room_id));
        Ok(items)
    }

    async fn set_blocked(
        &self,
        room_id: &str,
        blocked: bool,
        reason: Option<String>,
    ) -> Result<(), SourceError> {
        let mut rooms = self
            .rooms
            .write()
            .expect("InMemoryRoomDirectory lock poisoned");
        let room = rooms.get_mut(room_id).ok_or(SourceError::NotFound)?;
        room.blocked = blocked;
        room.blocked_reason = if blocked { reason } else { None };
        Ok(())
    }

    async fn make_admin(&self, room_id: &str, _user_id: &str) -> Result<(), SourceError> {
        let rooms = self
            .rooms
            .read()
            .expect("InMemoryRoomDirectory lock poisoned");
        if rooms.contains_key(room_id) {
            Ok(())
        } else {
            Err(SourceError::NotFound)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user(id: &str) -> AdminUser {
        AdminUser {
            user_id: id.to_string(),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn get_user_returns_none_when_absent() {
        let dir = InMemoryUserDirectory::new();
        assert_eq!(dir.get_user("@nobody:example.org").await.unwrap(), None);
    }

    #[tokio::test]
    async fn get_user_returns_the_inserted_user() {
        let dir = InMemoryUserDirectory::new().with_user(user("@alice:example.org"));
        let found = dir.get_user("@alice:example.org").await.unwrap();
        assert_eq!(found.map(|u| u.user_id), Some("@alice:example.org".into()));
    }

    #[tokio::test]
    async fn list_users_filters_by_admin_flag() {
        let mut admin = user("@ops:example.org");
        admin.admin = true;
        let dir = InMemoryUserDirectory::new()
            .with_user(admin)
            .with_user(user("@alice:example.org"));
        let filter = UserFilter {
            admin: Some(true),
            ..Default::default()
        };
        let items = dir.list_users(&filter).await.unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].user_id, "@ops:example.org");
    }

    #[tokio::test]
    async fn list_users_q_matches_user_id_and_display_name_case_insensitively() {
        let mut alice = user("@alice:example.org");
        alice.display_name = Some("Alice Anderson".to_string());
        let dir = InMemoryUserDirectory::new()
            .with_user(alice)
            .with_user(user("@bob:example.org"));
        let by_id = dir
            .list_users(&UserFilter {
                q: Some("ALICE".to_string()),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(by_id.len(), 1);
        assert_eq!(by_id[0].user_id, "@alice:example.org");

        let by_name = dir
            .list_users(&UserFilter {
                q: Some("anderson".to_string()),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(by_name.len(), 1);
        assert_eq!(by_name[0].user_id, "@alice:example.org");
    }

    #[tokio::test]
    async fn set_admin_on_unknown_user_is_not_found() {
        let dir = InMemoryUserDirectory::new();
        let err = dir
            .set_admin("@nobody:example.org", true)
            .await
            .unwrap_err();
        assert!(matches!(err, SourceError::NotFound));
    }

    #[tokio::test]
    async fn set_locked_and_set_deactivated_mutate_the_stored_user() {
        let dir = InMemoryUserDirectory::new().with_user(user("@alice:example.org"));
        dir.set_locked("@alice:example.org", true).await.unwrap();
        dir.set_deactivated("@alice:example.org", true)
            .await
            .unwrap();
        let found = dir.get_user("@alice:example.org").await.unwrap().unwrap();
        assert!(found.locked);
        assert!(found.deactivated);
    }

    #[test]
    fn source_error_maps_onto_the_problem_catalog() {
        assert_eq!(SourceError::NotFound.to_problem().status, 404);
        assert_eq!(
            SourceError::Unavailable("down".into()).to_problem().status,
            503
        );
        assert_eq!(SourceError::Invalid("bad".into()).to_problem().status, 400);
        assert_eq!(SourceError::Conflict("dup".into()).to_problem().status, 409);
    }

    #[tokio::test]
    async fn default_create_lookup_availability_are_unavailable() {
        // Guards session 6's contract: an implementor who only overrides the five original
        // methods must still compile, and the three new ones must answer 503, never a fake
        // success, until overridden.
        let dir = InMemoryUserDirectory::new();
        assert!(matches!(
            dir.create_user(UserCreateRequest::default()).await,
            Err(SourceError::Unavailable(_))
        ));
        assert!(matches!(
            dir.lookup_user(UserLookupQuery::Threepid {
                medium: "email".into(),
                address: "a@example.org".into(),
            })
            .await,
            Err(SourceError::Unavailable(_))
        ));
        assert!(matches!(
            dir.check_localpart_available("alice").await,
            Err(SourceError::Unavailable(_))
        ));
    }

    fn room(id: &str) -> AdminRoom {
        AdminRoom {
            room_id: id.to_string(),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn get_room_returns_none_when_absent() {
        let dir = InMemoryRoomDirectory::new();
        assert_eq!(dir.get_room("!nobody:example.org").await.unwrap(), None);
    }

    #[tokio::test]
    async fn get_room_returns_the_inserted_room() {
        let dir = InMemoryRoomDirectory::new().with_room(room("!abc:example.org"));
        let found = dir.get_room("!abc:example.org").await.unwrap();
        assert_eq!(found.map(|r| r.room_id), Some("!abc:example.org".into()));
    }

    #[tokio::test]
    async fn list_rooms_filters_by_blocked_flag() {
        let mut blocked = room("!blocked:example.org");
        blocked.blocked = true;
        let dir = InMemoryRoomDirectory::new()
            .with_room(blocked)
            .with_room(room("!ok:example.org"));
        let items = dir
            .list_rooms(&RoomFilter {
                blocked: Some(true),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].room_id, "!blocked:example.org");
    }

    #[tokio::test]
    async fn list_rooms_q_matches_name_and_room_id_case_insensitively() {
        let mut lounge = room("!abc:example.org");
        lounge.name = Some("The Lounge".to_string());
        let dir = InMemoryRoomDirectory::new()
            .with_room(lounge)
            .with_room(room("!other:example.org"));
        let items = dir
            .list_rooms(&RoomFilter {
                q: Some("lounge".to_string()),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].room_id, "!abc:example.org");
    }

    #[tokio::test]
    async fn set_blocked_on_unknown_room_is_not_found() {
        let dir = InMemoryRoomDirectory::new();
        let err = dir
            .set_blocked("!nobody:example.org", true, None)
            .await
            .unwrap_err();
        assert!(matches!(err, SourceError::NotFound));
    }

    #[tokio::test]
    async fn set_blocked_sets_and_clears_the_reason() {
        let dir = InMemoryRoomDirectory::new().with_room(room("!abc:example.org"));
        dir.set_blocked("!abc:example.org", true, Some("spam".to_string()))
            .await
            .unwrap();
        let found = dir.get_room("!abc:example.org").await.unwrap().unwrap();
        assert!(found.blocked);
        assert_eq!(found.blocked_reason, Some("spam".to_string()));

        dir.set_blocked("!abc:example.org", false, None)
            .await
            .unwrap();
        let found = dir.get_room("!abc:example.org").await.unwrap().unwrap();
        assert!(!found.blocked);
        assert_eq!(found.blocked_reason, None);
    }

    #[tokio::test]
    async fn make_admin_on_unknown_room_is_not_found() {
        let dir = InMemoryRoomDirectory::new();
        let err = dir
            .make_admin("!nobody:example.org", "@alice:example.org")
            .await
            .unwrap_err();
        assert!(matches!(err, SourceError::NotFound));
    }

    #[tokio::test]
    async fn make_admin_on_a_known_room_succeeds() {
        let dir = InMemoryRoomDirectory::new().with_room(room("!abc:example.org"));
        dir.make_admin("!abc:example.org", "@alice:example.org")
            .await
            .unwrap();
    }
}
