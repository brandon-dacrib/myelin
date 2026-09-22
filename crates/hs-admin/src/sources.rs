//! Consumer-defined data-source traits, following the pattern of
//! `crates/hs-federation/src/room_source.rs`: the trait lives here, next to the handlers that
//! call it, and the implementation lives in whichever crate owns the real data (07's user store
//! for [`UserDirectory`]). Describing a trait costs this crate almost nothing — `async_trait`,
//! `serde`, `thiserror`, and for [`ConfigSource`] the configuration schema itself, since the
//! admin API's whole job there is to describe `hs_config::Config` to a form — and the real
//! implementation is free to depend on `hs-admin` without a cycle.
//!
//! Publish this contract verbatim — do not add or remove trait methods without updating whoever
//! is implementing them, since that happens in a separate crate track 15 does not own.

use std::collections::{BTreeMap, HashMap};
use std::sync::RwLock;

use async_trait::async_trait;
use hs_config::document::Origin;
use hs_config::layered::{FileLayer, Layers, Resolved};
use serde::Deserialize;
use serde_json::{Map, Value, json};

use crate::model::{
    AdminAppservice, AdminAppserviceBacklogEntry, AdminAppserviceCreate, AdminAppserviceHealth,
    AdminAppserviceLinks, AdminAppserviceReplay, AdminAppserviceTokens, AdminDestination,
    AdminDevice, AdminPasswordReset, AdminRoom, AdminRoomMember, AdminUser, ClusterStatus,
    ConfigChange, ConfigReloadReport, ConfigSection, ConfigValidateReport, ExternalId,
    SetupRequest, SetupSession, StatisticsOverview, ThreePid,
};

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
    /// As [`SourceError::Invalid`], for a refusal that is about one field of the request:
    /// `pointer` is a JSON pointer into the body (`/password`), which the problem document
    /// carries in `errors[]` so that an interface can put the message beside the input.
    #[error("invalid request: {detail}")]
    InvalidField {
        pointer: &'static str,
        detail: String,
    },
    #[error("conflict: {0}")]
    Conflict(String),
    /// An `If-Match` precondition named a version that is no longer current: somebody else
    /// changed the resource in between, so the caller's change was computed against a view that
    /// has since moved. Distinct from [`SourceError::Conflict`] because the caller's remedy is
    /// different — re-read, re-apply, retry — and because RFC 9110 gives it its own status.
    #[error("precondition failed: {0}")]
    PreconditionFailed(String),
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
            SourceError::InvalidField { pointer, detail } => hs_http::Problem::validation_failed()
                .with_detail(detail.clone())
                .with_errors(vec![hs_http::ValidationError::new(
                    *pointer,
                    detail.clone(),
                )]),
            SourceError::Conflict(detail) => {
                hs_http::Problem::conflict().with_detail(detail.clone())
            }
            SourceError::PreconditionFailed(detail) => {
                hs_http::Problem::precondition_failed().with_detail(detail.clone())
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
#[derive(Clone, Default, Deserialize)]
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

/// Written by hand so that a password cannot reach a log line, a panic message or an error
/// report through a stray `{:?}`.
impl std::fmt::Debug for UserCreateRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UserCreateRequest")
            .field("localpart", &self.localpart)
            .field("user_id", &self.user_id)
            .field("password", &self.password.as_ref().map(|_| "<redacted>"))
            .field("display_name", &self.display_name)
            .field("admin", &self.admin)
            .field("user_type", &self.user_type)
            .field("threepids", &self.threepids)
            .field("external_ids", &self.external_ids)
            .finish()
    }
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

    /// The user's devices (`users.devices.list`). `SourceError::NotFound` if there is no such
    /// user; an empty list for a user with none.
    async fn list_devices(&self, user_id: &str) -> Result<Vec<AdminDevice>, SourceError> {
        let _ = user_id;
        Err(SourceError::Unavailable(
            "this user directory does not know about devices yet".to_string(),
        ))
    }

    /// Signs one device out (`users.devices.delete`): its sessions stop working at once and the
    /// device is gone from the list. `SourceError::NotFound` for a user or device that is not
    /// there.
    async fn delete_device(&self, user_id: &str, device_id: &str) -> Result<(), SourceError> {
        let _ = (user_id, device_id);
        Err(SourceError::Unavailable(
            "this user directory does not know about devices yet".to_string(),
        ))
    }

    /// Signs the user out everywhere (`users.logout`): every session and every device.
    async fn logout_everywhere(&self, user_id: &str) -> Result<(), SourceError> {
        let _ = user_id;
        Err(SourceError::Unavailable(
            "this user directory cannot sign users out yet".to_string(),
        ))
    }

    /// Sets a new password (`users.reset_password`), signing the user out everywhere first if
    /// `logout_devices`. `SourceError::InvalidField` at `/password` for one the server's policy
    /// refuses.
    async fn reset_password(
        &self,
        user_id: &str,
        request: AdminPasswordReset,
    ) -> Result<(), SourceError> {
        let _ = (user_id, request);
        Err(SourceError::Unavailable(
            "this user directory cannot reset passwords yet".to_string(),
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
    /// Devices by user id. A user with none has no entry.
    devices: RwLock<HashMap<String, Vec<AdminDevice>>>,
    /// Passwords by user id, as set by `reset_password` -- kept only so a test can see one land.
    passwords: RwLock<HashMap<String, String>>,
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

    /// Gives `user_id` a device.
    pub fn with_device(self, user_id: &str, device: AdminDevice) -> Self {
        self.devices
            .write()
            .expect("InMemoryUserDirectory lock poisoned")
            .entry(user_id.to_owned())
            .or_default()
            .push(device);
        self
    }

    /// The password `reset_password` last set for `user_id`, if any.
    #[must_use]
    pub fn password_of(&self, user_id: &str) -> Option<String> {
        self.passwords
            .read()
            .expect("InMemoryUserDirectory lock poisoned")
            .get(user_id)
            .cloned()
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

    async fn list_devices(&self, user_id: &str) -> Result<Vec<AdminDevice>, SourceError> {
        if !self
            .users
            .read()
            .expect("InMemoryUserDirectory lock poisoned")
            .contains_key(user_id)
        {
            return Err(SourceError::NotFound);
        }
        Ok(self
            .devices
            .read()
            .expect("InMemoryUserDirectory lock poisoned")
            .get(user_id)
            .cloned()
            .unwrap_or_default())
    }

    async fn delete_device(&self, user_id: &str, device_id: &str) -> Result<(), SourceError> {
        let mut devices = self
            .devices
            .write()
            .expect("InMemoryUserDirectory lock poisoned");
        let list = devices.get_mut(user_id).ok_or(SourceError::NotFound)?;
        let before = list.len();
        list.retain(|d| d.device_id != device_id);
        if list.len() == before {
            return Err(SourceError::NotFound);
        }
        Ok(())
    }

    async fn logout_everywhere(&self, user_id: &str) -> Result<(), SourceError> {
        if !self
            .users
            .read()
            .expect("InMemoryUserDirectory lock poisoned")
            .contains_key(user_id)
        {
            return Err(SourceError::NotFound);
        }
        self.devices
            .write()
            .expect("InMemoryUserDirectory lock poisoned")
            .remove(user_id);
        Ok(())
    }

    async fn reset_password(
        &self,
        user_id: &str,
        request: AdminPasswordReset,
    ) -> Result<(), SourceError> {
        if !self
            .users
            .read()
            .expect("InMemoryUserDirectory lock poisoned")
            .contains_key(user_id)
        {
            return Err(SourceError::NotFound);
        }
        if request.password.len() < 8 {
            return Err(SourceError::InvalidField {
                pointer: "/password",
                detail: "the password must be at least 8 characters".to_owned(),
            });
        }
        if request.logout_devices {
            self.logout_everywhere(user_id).await?;
        }
        self.passwords
            .write()
            .expect("InMemoryUserDirectory lock poisoned")
            .insert(user_id.to_owned(), request.password);
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

    /// Everyone with a membership event in the room's current state, whatever its value
    /// (`rooms.members.list`): joined, invited, left, banned, knocking. The handler filters by
    /// `membership` if asked. `SourceError::NotFound` if the room does not exist.
    async fn list_members(&self, room_id: &str) -> Result<Vec<AdminRoomMember>, SourceError> {
        let _ = room_id;
        Err(SourceError::Unavailable(
            "this room directory does not list members yet".to_string(),
        ))
    }
}

/// An in-memory [`RoomDirectory`] for this crate's own handler tests, following
/// [`InMemoryUserDirectory`]'s shape exactly. Not a production implementation: `make_admin` only
/// validates the room exists (there is no membership list here to check `user_id` against).
#[derive(Debug, Default)]
pub struct InMemoryRoomDirectory {
    rooms: RwLock<HashMap<String, AdminRoom>>,
    members: RwLock<HashMap<String, Vec<AdminRoomMember>>>,
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

    /// Gives `room_id` a member.
    pub fn with_member(self, room_id: &str, member: AdminRoomMember) -> Self {
        self.members
            .write()
            .expect("InMemoryRoomDirectory lock poisoned")
            .entry(room_id.to_owned())
            .or_default()
            .push(member);
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

    async fn list_members(&self, room_id: &str) -> Result<Vec<AdminRoomMember>, SourceError> {
        if !self
            .rooms
            .read()
            .expect("InMemoryRoomDirectory lock poisoned")
            .contains_key(room_id)
        {
            return Err(SourceError::NotFound);
        }
        Ok(self
            .members
            .read()
            .expect("InMemoryRoomDirectory lock poisoned")
            .get(room_id)
            .cloned()
            .unwrap_or_default())
    }
}

// -------------------------------------------------------------------------------------------
// configuration (RFC 0004 section 4.12, the `/config*` operations)
// -------------------------------------------------------------------------------------------

/// One `config.update` request, already parsed and authorized.
///
/// `patch` is an RFC 7396 JSON Merge Patch against the named section, with the admin API's own
/// conventions already applied by the handler: a `null` member means "reset this setting to its
/// schema default", and any secret the client echoed back as `{"$secret": true}` has been
/// removed, because that means the operator did not touch the field.
#[derive(Debug, Clone)]
pub struct ConfigPatch {
    /// The top-level `hs_config::Config` field name being changed.
    pub section: String,
    /// The merge patch, with keys relative to the section.
    pub patch: Value,
    /// The admin API principal to record against the change, for the history.
    pub actor: Option<String>,
    /// The revision the caller believed was current, from `If-Match`. `None` means the caller
    /// sent no precondition and accepts whatever is there.
    pub expected_revision: Option<u64>,
}

/// The configuration seam `hs-admin`'s `/config*` handlers call. **Not implemented against a
/// real store by this crate** — the implementation belongs wherever `hs_config::ConfigStore` and
/// the process's `hs_config::Layers` live, which is the binary, not here.
/// [`InMemoryConfigSource`] is a fake for this crate's own tests only; `router::AdminState::config`
/// defaults to `None`, so every `/config*` operation answers an honest `503 unavailable` until a
/// real implementation is wired in with `AdminState::with_config`.
///
/// Contract notes for whoever implements this against `hs_config`:
/// - [`ConfigSection::values`] is the *effective* configuration (`Layers::resolve`'s `config`,
///   serialized), not just what the database holds, because that is what the server is running
///   on and what an operator is looking at. Return it unredacted; the handlers redact at the
///   boundary so that a secret cannot escape through an implementation that forgot to.
/// - `revision` must be the store's own counter (`ConfigMeta::revision`), the same number
///   [`ConfigPatch::expected_revision`] is compared against. It counts writes to the
///   configuration as a whole, which is what makes it safe as an `If-Match` token.
/// - [`ConfigSource::validate`] returns `Ok` with `valid: false` for a configuration that would
///   be rejected: answering the question succeeded, and the answer was "no". Reserve
///   `SourceError` for being unable to answer at all.
/// - [`ConfigSource::patch_section`] must validate the configuration the patch would *produce*
///   (`Layers::resolve_with_patch`) and write nothing if it is invalid, must refuse a bootstrap
///   section (`StoreError::BootstrapSection`) and a stale `expected_revision`
///   (`StoreError::RevisionMismatch` → [`SourceError::PreconditionFailed`]), and must refuse a
///   patch the environment pins. The handlers check all four first, so the implementation's own
///   checks are a backstop against a race, not the only guard — but they must be there, because
///   a write that is stored and then ignored is a lie.
#[async_trait]
pub trait ConfigSource: Send + Sync + 'static {
    /// Every section, in `hs_config::Config`'s own declaration order.
    async fn list_sections(&self) -> Result<Vec<ConfigSection>, SourceError>;

    /// One section by name, with its recent history. `Ok(None)` for a name that is not a section
    /// — distinct from `SourceError::NotFound`, matching `get_user`'s convention.
    async fn get_section(&self, name: &str) -> Result<Option<ConfigSection>, SourceError>;

    /// The settings in `patch` that an `HS__` environment variable pins, as whole-configuration
    /// JSON Pointers (`Layers::pinned_by_environment`). Empty means the patch is free to write.
    async fn environment_pinned(
        &self,
        section: &str,
        patch: &Value,
    ) -> Result<Vec<String>, SourceError>;

    /// Whether the configuration `candidate` would produce is one this server would accept.
    /// `candidate` is a sparse document keyed by section, applied as a merge patch over what is
    /// stored now — so `{"auth": {"enable_registration": true}}` asks about exactly that one
    /// change, and nothing is written either way.
    async fn validate(&self, candidate: &Value) -> Result<ConfigValidateReport, SourceError>;

    /// Applies one section's merge patch and returns the section as it now reads.
    async fn patch_section(&self, request: ConfigPatch) -> Result<ConfigSection, SourceError>;

    /// Re-reads the configuration layers and swaps what can be swapped into the running server,
    /// reporting what was reloaded and what still needs a restart.
    async fn reload(&self) -> Result<ConfigReloadReport, SourceError>;

    /// The most recent changes, newest first, at most `limit` of them; restricted to one section
    /// when `section` is set. An implementation filtering by section must do so before applying
    /// `limit`, or a busy neighbouring section will crowd this one's history out of the answer.
    async fn history(
        &self,
        section: Option<&str>,
        limit: usize,
    ) -> Result<Vec<ConfigChange>, SourceError>;
}

/// Renders `error` as the admin API's field-level validation errors, so a rejected configuration
/// tells an operator which settings are wrong instead of just that something is.
///
/// `hs_config` reports dotted paths (`auth.oidc_providers[0].client_secret`); RFC 0004 reports
/// JSON Pointers. Translating here rather than in each implementation keeps the two vocabularies
/// from leaking into each other, and keeps the pointers the same strings the schema, the origins
/// map and the redaction machinery all use.
#[must_use]
pub fn config_validation_errors(error: &hs_config::ConfigError) -> Vec<hs_http::ValidationError> {
    match error {
        hs_config::ConfigError::Validation(errors) => errors
            .0
            .iter()
            .map(|e| hs_http::ValidationError::new(dotted_path_to_pointer(&e.path), &e.message))
            .collect(),
        hs_config::ConfigError::SecretFile { field, .. }
        | hs_config::ConfigError::SecretConflict { field } => {
            vec![hs_http::ValidationError::new(
                dotted_path_to_pointer(field),
                error.to_string(),
            )]
        }
        // A parse failure has no field to point at: the document did not survive long enough to
        // have one. Pointing at the root is honest; inventing a field would not be.
        other => vec![hs_http::ValidationError::new("", other.to_string())],
    }
}

/// `auth.oidc_providers[0].client_secret` becomes `/auth/oidc_providers/0/client_secret`.
fn dotted_path_to_pointer(path: &str) -> String {
    let flattened = path.replace('[', ".").replace(']', "");
    let mut out = String::new();
    for token in flattened.split('.').filter(|t| !t.is_empty()) {
        out.push('/');
        out.push_str(&token.replace('~', "~0").replace('/', "~1"));
    }
    out
}

/// A [`ConfigSource`] backed by `hs_config::Layers` held in memory, for this crate's own handler
/// tests. Not a production implementation and not a shortcut to one: it has no database, so
/// every change it accepts is forgotten when the process exits, and `reload` has no running
/// server to swap anything into. What it *does* do for real is resolve, validate and merge
/// through `hs_config` itself, so the behaviour the handler tests pin — the precedence order, a
/// patch rejected for the configuration it would produce, a setting the environment pins, a
/// `null` that resets to the schema default — is the real behaviour and not a re-implementation
/// of it that could agree with the tests while disagreeing with the server.
#[derive(Debug)]
pub struct InMemoryConfigSource {
    inner: RwLock<ConfigState>,
}

#[derive(Debug)]
struct ConfigState {
    layers: Layers,
    revision: u64,
    history: Vec<ConfigChange>,
    reloaded_at: BTreeMap<String, String>,
}

impl Default for InMemoryConfigSource {
    fn default() -> Self {
        Self::new()
    }
}

impl InMemoryConfigSource {
    /// An empty configuration: no bootstrap file, nothing stored, no environment overrides, so
    /// every setting reads at its schema default.
    ///
    /// The layers are empty *objects*, not `Layers::default()`, whose `database` and
    /// `environment` are JSON `null`. A null layer is not an empty one: RFC 7396 says a non-object
    /// patch replaces its target wholesale, so `Layers::merged` would collapse the entire
    /// configuration to null and every section would read back at its default.
    #[must_use]
    pub fn new() -> Self {
        Self {
            inner: RwLock::new(ConfigState {
                layers: Layers {
                    file: None,
                    database: Value::Object(Map::new()),
                    environment: Value::Object(Map::new()),
                },
                revision: 0,
                history: Vec::new(),
                reloaded_at: BTreeMap::new(),
            }),
        }
    }

    /// Adds a bootstrap-file layer, as `-c homeserver.yaml` would.
    #[must_use]
    pub fn with_file(self, path: &str, document: Value) -> Self {
        {
            let mut state = self.state_mut();
            state.layers.file = Some(FileLayer {
                path: std::path::PathBuf::from(path),
                document,
            });
        }
        self
    }

    /// Seeds the database layer — what an operator has already changed through this API.
    #[must_use]
    pub fn with_database(self, document: Value) -> Self {
        {
            let mut state = self.state_mut();
            state.layers.database = document;
            state.revision = 1;
        }
        self
    }

    /// Adds `HS__` environment overrides, as a deployment's manifest would.
    #[must_use]
    pub fn with_environment(self, document: Value) -> Self {
        {
            let mut state = self.state_mut();
            state.layers.environment = document;
        }
        self
    }

    fn state(&self) -> std::sync::RwLockReadGuard<'_, ConfigState> {
        self.inner
            .read()
            .expect("InMemoryConfigSource lock poisoned")
    }

    fn state_mut(&self) -> std::sync::RwLockWriteGuard<'_, ConfigState> {
        self.inner
            .write()
            .expect("InMemoryConfigSource lock poisoned")
    }
}

impl ConfigState {
    /// The resolved configuration, or the error explaining why this server would not accept what
    /// its own layers currently say.
    fn resolve(&self) -> Result<Resolved, SourceError> {
        self.layers.resolve().map_err(|e| {
            SourceError::Unavailable(format!("the current configuration does not resolve: {e}"))
        })
    }

    fn section(&self, resolved: &Resolved, name: &str) -> ConfigSection {
        config_section(
            resolved,
            name,
            self.revision,
            self.reloaded_at.get(name).cloned(),
        )
    }
}

/// One section as the admin API reports it: effective values, per-setting origins, and the flags
/// the management interface needs to decide whether to offer an edit at all.
///
/// Public because there are two [`ConfigSource`] implementations -- this crate's in-memory fake
/// and the binary's real store-backed one -- and the wire shape must be identical between them.
/// Two hand-written copies of this would agree on the day they were written and drift the first
/// time a field was added to one of them.
#[must_use]
pub fn config_section(
    resolved: &Resolved,
    name: &str,
    revision: u64,
    last_reloaded_at: Option<String>,
) -> ConfigSection {
    let effective =
        serde_json::to_value(&resolved.config).unwrap_or_else(|_| Value::Object(Map::new()));
    let values = effective
        .get(name)
        .cloned()
        .unwrap_or_else(|| Value::Object(Map::new()));
    ConfigSection {
        name: name.to_owned(),
        reloadable: hs_config::reload::is_reloadable(name),
        bootstrap: hs_config::store::is_bootstrap_section(name),
        source: section_source(resolved, name),
        last_reloaded_at,
        origins: section_origins(resolved, name, &values),
        values,
        revision,
        history: Vec::new(),
    }
}

/// The highest-precedence layer that sets anything in `section`, or `default` when nothing does.
fn section_source(resolved: &Resolved, section: &str) -> String {
    let prefix = format!("/{section}/");
    resolved
        .origins
        .iter()
        .filter(|(pointer, _)| pointer.starts_with(&prefix))
        .map(|(_, origin)| *origin)
        .max()
        .unwrap_or(Origin::Default)
        .as_str()
        .to_owned()
}

/// Where each of `section`'s settings got its value. Every setting is listed, including the ones
/// nothing sets: the management interface should not have to know that an absent key means
/// "default".
fn section_origins(resolved: &Resolved, section: &str, values: &Value) -> BTreeMap<String, String> {
    let prefix = format!("/{section}");
    hs_config::document::leaf_pointers(values)
        .into_iter()
        .map(|leaf| {
            let pointer = format!("{prefix}{leaf}");
            let origin = resolved.origin(&pointer).as_str().to_owned();
            (pointer, origin)
        })
        .collect()
}

#[async_trait]
impl ConfigSource for InMemoryConfigSource {
    async fn list_sections(&self) -> Result<Vec<ConfigSection>, SourceError> {
        let state = self.state();
        let resolved = state.resolve()?;
        Ok(hs_config::reload::SECTION_NAMES
            .iter()
            .map(|name| state.section(&resolved, name))
            .collect())
    }

    async fn get_section(&self, name: &str) -> Result<Option<ConfigSection>, SourceError> {
        if !hs_config::reload::SECTION_NAMES.contains(&name) {
            return Ok(None);
        }
        let state = self.state();
        let resolved = state.resolve()?;
        let mut section = state.section(&resolved, name);
        section.history = state
            .history
            .iter()
            .rev()
            .filter(|change| change.section == name)
            .take(20)
            .cloned()
            .collect();
        Ok(Some(section))
    }

    async fn environment_pinned(
        &self,
        section: &str,
        patch: &Value,
    ) -> Result<Vec<String>, SourceError> {
        Ok(self.state().layers.pinned_by_environment(section, patch))
    }

    async fn validate(&self, candidate: &Value) -> Result<ConfigValidateReport, SourceError> {
        let state = self.state();
        let mut proposed = state.layers.clone();
        let mut database = proposed.database.clone();
        hs_config::merge_patch(&mut database, candidate);
        proposed.database = database;

        match proposed.resolve() {
            Ok(new) => {
                let requires_restart = match state.layers.resolve() {
                    Ok(current) => {
                        hs_config::reload::sections_requiring_restart(&current.config, &new.config)
                            .into_iter()
                            .map(str::to_owned)
                            .collect()
                    }
                    // Nothing to compare against: a server whose current configuration does not
                    // resolve is being repaired, and every section is in play.
                    Err(_) => Vec::new(),
                };
                Ok(ConfigValidateReport::valid(requires_restart))
            }
            Err(e) => Ok(ConfigValidateReport::invalid(config_validation_errors(&e))),
        }
    }

    async fn patch_section(&self, request: ConfigPatch) -> Result<ConfigSection, SourceError> {
        let mut state = self.state_mut();
        if !hs_config::reload::SECTION_NAMES.contains(&request.section.as_str()) {
            return Err(SourceError::NotFound);
        }
        if hs_config::store::is_bootstrap_section(&request.section) {
            return Err(SourceError::Conflict(format!(
                "{:?} says where this server's database is, so it cannot be stored in it",
                request.section
            )));
        }
        if let Some(expected) = request.expected_revision
            && expected != state.revision
        {
            return Err(SourceError::PreconditionFailed(format!(
                "the configuration has changed since revision {expected} (it is now at {})",
                state.revision
            )));
        }
        let pinned = state
            .layers
            .pinned_by_environment(&request.section, &request.patch);
        if !pinned.is_empty() {
            return Err(SourceError::Conflict(format!(
                "pinned by the environment: {}",
                pinned.join(", ")
            )));
        }
        // Validate the configuration this patch would produce before writing anything: a stored
        // setting the server then refuses to boot on is worse than a rejected request.
        state
            .layers
            .resolve_with_patch(&request.section, &request.patch)
            .map_err(|e| SourceError::Invalid(e.to_string()))?;

        let mut database = state.layers.database.clone();
        hs_config::merge_patch(
            &mut database,
            &Value::Object(
                [(request.section.clone(), request.patch.clone())]
                    .into_iter()
                    .collect::<Map<String, Value>>(),
            ),
        );
        state.layers.database = database;
        state.revision += 1;
        let change = ConfigChange {
            revision: state.revision,
            section: request.section.clone(),
            patch: request.patch,
            actor: request.actor,
            at: hs_http::time::now_rfc3339(),
        };
        state.history.push(change);

        let resolved = state.resolve()?;
        Ok(state.section(&resolved, &request.section))
    }

    async fn reload(&self) -> Result<ConfigReloadReport, SourceError> {
        let mut state = self.state_mut();
        state.resolve()?;
        let now = hs_http::time::now_rfc3339();
        let reloaded_sections: Vec<String> = hs_config::reload::RELOADABLE_SECTIONS
            .iter()
            .map(|name| (*name).to_owned())
            .collect();
        for name in &reloaded_sections {
            state.reloaded_at.insert(name.clone(), now.clone());
        }
        Ok(ConfigReloadReport {
            reloaded_sections,
            errors: Vec::new(),
            // This fake has no running server to have drifted from, so nothing can need a
            // restart. A real implementation compares the configuration it booted on against
            // the one it just read and reports the difference.
            requires_restart: Vec::new(),
            revision: state.revision,
        })
    }

    async fn history(
        &self,
        section: Option<&str>,
        limit: usize,
    ) -> Result<Vec<ConfigChange>, SourceError> {
        Ok(self
            .state()
            .history
            .iter()
            .rev()
            .filter(|change| section.is_none_or(|name| change.section == name))
            .take(limit)
            .cloned()
            .collect())
    }
}

// -------------------------------------------------------------------------------------------
// The Overview page's numbers.
// -------------------------------------------------------------------------------------------

/// What `GET /statistics/overview` and `GET /cluster` read: the handful of numbers the
/// management interface's first page is made of. One trait rather than two because the only
/// implementation that can exist is the process that owns the stores and the cluster handle,
/// and it has all of it to hand.
///
/// # Contract for implementors
///
/// The dashboard polls these, by default every thirty seconds from every open tab, so an
/// implementation must not do work proportional to the size of the server on each call. Count
/// once and remember the answer for a while; a user count that is a minute old is fine.
#[async_trait]
pub trait OverviewSource: Send + Sync + 'static {
    async fn statistics(&self) -> Result<StatisticsOverview, SourceError>;
    async fn cluster(&self) -> Result<ClusterStatus, SourceError>;
}

/// A fixed [`OverviewSource`], for this crate's tests.
pub struct StaticOverviewSource {
    pub statistics: StatisticsOverview,
    pub cluster: ClusterStatus,
}

#[async_trait]
impl OverviewSource for StaticOverviewSource {
    async fn statistics(&self) -> Result<StatisticsOverview, SourceError> {
        Ok(self.statistics.clone())
    }

    async fn cluster(&self) -> Result<ClusterStatus, SourceError> {
        Ok(self.cluster.clone())
    }
}

// -------------------------------------------------------------------------------------------
// First-run setup: creating the first administrator on a server that has none.
// -------------------------------------------------------------------------------------------

/// Why `POST /setup` did not create an administrator.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SetupError {
    /// An administrator already exists, so there is nothing to set up. `409 conflict`.
    #[error("this server already has an administrator")]
    Closed,
    /// The setup token is not the one this server is offering. `401 unauthenticated`: the token
    /// is this operation's credential.
    #[error("that is not this server's setup token")]
    BadToken,
    /// A field of the request cannot be used. `400 validation-failed`, with `pointer` naming the
    /// field so the interface can put the message beside it.
    #[error("{detail}")]
    Invalid {
        /// A JSON pointer into the request body, `/username` or `/password`.
        pointer: &'static str,
        detail: String,
    },
    /// The store could not be reached. `503 unavailable`.
    #[error("setup is temporarily unavailable: {0}")]
    Unavailable(String),
}

impl SetupError {
    /// Maps this error onto the RFC 9457 problem catalog.
    pub fn to_problem(&self) -> hs_http::Problem {
        match self {
            SetupError::Closed => hs_http::Problem::conflict().with_detail(self.to_string()),
            SetupError::BadToken => {
                hs_http::Problem::unauthenticated().with_detail(self.to_string())
            }
            SetupError::Invalid { pointer, detail } => hs_http::Problem::validation_failed()
                .with_detail(detail.clone())
                .with_errors(vec![hs_http::ValidationError::new(
                    *pointer,
                    detail.clone(),
                )]),
            SetupError::Unavailable(detail) => {
                hs_http::Problem::unavailable().with_detail(detail.clone())
            }
        }
    }
}

/// What `GET /setup` and `POST /setup` call. The real implementation is track 07's
/// (`hs_auth::setup`), because creating an account and signing it in are that crate's business;
/// this crate only owns the HTTP shape.
///
/// # Contract for implementors
///
/// - `needs_setup` must be cheap. It is answered to anybody who can reach the port, so it must
///   not be a scan of every account.
/// - `create_first_admin` must check the token before it reveals anything else: a caller without
///   it must not be able to learn whether a username is taken or what the password policy is.
/// - It must succeed at most once, however many callers present the right token at once.
/// - It must refuse with [`SetupError::Closed`] when an administrator exists, even if a token is
///   somehow still outstanding, and withdraw the token when it does.
#[async_trait]
pub trait SetupSource: Send + Sync + 'static {
    async fn needs_setup(&self) -> Result<bool, SourceError>;
    async fn create_first_admin(&self, request: SetupRequest) -> Result<SetupSession, SetupError>;
}

/// A [`SetupSource`] over one token held in memory, for this crate's tests and `hs-admin-mock`.
/// It honours the contract above, but its "account" is only the session it hands back.
pub struct InMemorySetupSource {
    token: RwLock<Option<String>>,
    server_name: String,
}

impl InMemorySetupSource {
    /// A server named `server_name` that needs setting up and is offering `token`.
    pub fn open(server_name: impl Into<String>, token: impl Into<String>) -> Self {
        Self {
            token: RwLock::new(Some(token.into())),
            server_name: server_name.into(),
        }
    }

    /// A server that already has an administrator.
    pub fn closed(server_name: impl Into<String>) -> Self {
        Self {
            token: RwLock::new(None),
            server_name: server_name.into(),
        }
    }
}

#[async_trait]
impl SetupSource for InMemorySetupSource {
    async fn needs_setup(&self) -> Result<bool, SourceError> {
        Ok(self
            .token
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_some())
    }

    async fn create_first_admin(&self, request: SetupRequest) -> Result<SetupSession, SetupError> {
        let mut token = self
            .token
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(current) = token.as_deref() else {
            return Err(SetupError::Closed);
        };
        if current != request.setup_token {
            return Err(SetupError::BadToken);
        }
        let localpart = request.username.trim().trim_start_matches('@');
        let localpart = localpart.split(':').next().unwrap_or_default();
        if localpart.is_empty() {
            return Err(SetupError::Invalid {
                pointer: "/username",
                detail: "a username is required".to_owned(),
            });
        }
        if request.password.len() < 8 {
            return Err(SetupError::Invalid {
                pointer: "/password",
                detail: "the password must be at least 8 characters".to_owned(),
            });
        }
        *token = None;
        Ok(SetupSession {
            user_id: format!("@{}:{}", localpart.to_ascii_lowercase(), self.server_name),
            access_token: format!("syt_mock_{}", crate::model::new_id()),
            device_id: "SETUP".to_owned(),
        })
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

// ---------------------------------------------------------------------------------------------
// Federation
// ---------------------------------------------------------------------------------------------

/// Where the `federation.destinations.*` operations read: every remote server this one has
/// tried to reach, with its backoff state. Implemented for real by `hs-federation` over its
/// destination store; by [`InMemoryFederationSource`] for tests and the mock.
#[async_trait]
pub trait FederationSource: Send + Sync + 'static {
    async fn list_destinations(&self) -> Result<Vec<AdminDestination>, SourceError>;
    /// `Ok(None)` for a server this one has never tried to reach.
    async fn get_destination(
        &self,
        server_name: &str,
    ) -> Result<Option<AdminDestination>, SourceError>;
    /// Clears the backoff, so the next request is attempted at once. `SourceError::NotFound`
    /// for a server there is no record of.
    async fn reset_destination(&self, server_name: &str) -> Result<AdminDestination, SourceError>;
}

/// A [`FederationSource`] over a list held in memory.
#[derive(Debug, Default)]
pub struct InMemoryFederationSource {
    destinations: RwLock<BTreeMap<String, AdminDestination>>,
}

impl InMemoryFederationSource {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_destination(self, destination: AdminDestination) -> Self {
        self.destinations
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(destination.server_name.clone(), destination);
        self
    }
}

#[async_trait]
impl FederationSource for InMemoryFederationSource {
    async fn list_destinations(&self) -> Result<Vec<AdminDestination>, SourceError> {
        Ok(self
            .destinations
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
            .cloned()
            .collect())
    }

    async fn get_destination(
        &self,
        server_name: &str,
    ) -> Result<Option<AdminDestination>, SourceError> {
        Ok(self
            .destinations
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(server_name)
            .cloned())
    }

    async fn reset_destination(&self, server_name: &str) -> Result<AdminDestination, SourceError> {
        let mut destinations = self
            .destinations
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let destination = destinations
            .get_mut(server_name)
            .ok_or(SourceError::NotFound)?;
        destination.failing_since = None;
        destination.retry_interval_ms = None;
        Ok(destination.clone())
    }
}

// ---------------------------------------------------------------------------------------------
// Appservices (bridges)
// ---------------------------------------------------------------------------------------------

/// Where the `appservices.*` operations read and write: the appservice registry, its health,
/// its delivery queue. Implemented for real by `hs-appservice` over its `Registry`; by
/// [`InMemoryAppserviceDirectory`] for this crate's tests and `hs-admin-mock`.
///
/// Every method that names an appservice answers [`SourceError::NotFound`] for one that is not
/// registered. `create` answers [`SourceError::Conflict`] for an id, a token or an exclusive
/// namespace already taken, and [`SourceError::Invalid`] for a registration that does not
/// parse -- with a [`SourceError::InvalidField`] pointer when it can say which field.
#[async_trait]
pub trait AppserviceDirectory: Send + Sync + 'static {
    async fn list(&self) -> Result<Vec<AdminAppservice>, SourceError>;
    async fn get(&self, id: &str) -> Result<Option<AdminAppservice>, SourceError>;
    /// Registers a new appservice from the JSON form of a registration file (or the file's text,
    /// if that is what was given). Live at once: the bridge can authenticate as soon as this
    /// returns.
    async fn create(&self, request: AdminAppserviceCreate) -> Result<AdminAppservice, SourceError>;
    /// Applies an RFC 7396 merge patch to the registration.
    async fn update(&self, id: &str, patch: Value) -> Result<AdminAppservice, SourceError>;
    async fn delete(&self, id: &str) -> Result<(), SourceError>;
    async fn health(&self, id: &str) -> Result<AdminAppserviceHealth, SourceError>;
    /// Pending and dead-lettered transactions, oldest first.
    async fn backlog(&self, id: &str) -> Result<Vec<AdminAppserviceBacklogEntry>, SourceError>;
    /// Stops delivery; the queue keeps growing and nothing is lost.
    async fn pause(&self, id: &str) -> Result<AdminAppservice, SourceError>;
    async fn resume(&self, id: &str) -> Result<AdminAppservice, SourceError>;
    /// Mints a fresh `as_token` and `hs_token`. The old ones stop working at once; the bridge's
    /// own file has to be updated by hand, which is why the new ones are returned.
    async fn rotate_tokens(&self, id: &str) -> Result<AdminAppserviceTokens, SourceError>;
    /// The registration as a bridge's config file would hold it -- tokens included, since that
    /// is what the file is for.
    async fn registration(&self, id: &str) -> Result<AdminAppserviceRegistration, SourceError>;
    /// Sends the bridge a ping now and records the result. A bridge that does not answer is a
    /// `Ok(health)` saying so, not an error: an unreachable bridge is the thing being reported.
    async fn ping(&self, id: &str) -> Result<AdminAppserviceHealth, SourceError>;
    /// Puts dead-lettered transactions back in the queue for immediate retry. Returns how many.
    async fn replay(&self, id: &str, request: AdminAppserviceReplay) -> Result<usize, SourceError>;
}

/// A registration in both notations, for `GET /appservices/{id}/registration` to answer in
/// whichever the caller accepts.
#[derive(Clone)]
pub struct AdminAppserviceRegistration {
    pub json: Value,
    pub yaml: String,
}

impl std::fmt::Debug for AdminAppserviceRegistration {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("AdminAppserviceRegistration(<redacted>)")
    }
}

/// An [`AppserviceDirectory`] held in memory, for this crate's handler tests and the mock server.
/// Registrations are kept as the JSON given; health is whatever was last set; the backlog is a
/// list the test fills. `ping` marks the appservice healthy or, if `unreachable` names it, down.
#[derive(Debug, Default)]
pub struct InMemoryAppserviceDirectory {
    rows: RwLock<BTreeMap<String, InMemoryAppserviceRow>>,
    unreachable: RwLock<Vec<String>>,
}

#[derive(Debug, Clone)]
struct InMemoryAppserviceRow {
    registration: Value,
    paused: bool,
    health: AdminAppserviceHealth,
    backlog: Vec<AdminAppserviceBacklogEntry>,
    created_at: String,
}

impl InMemoryAppserviceDirectory {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers `registration` (a registration file as JSON) outright.
    pub fn with_registration(self, registration: Value) -> Self {
        let id = registration["id"].as_str().unwrap_or_default().to_owned();
        self.rows
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(
                id,
                InMemoryAppserviceRow {
                    registration,
                    paused: false,
                    health: AdminAppserviceHealth {
                        status: "unknown".to_owned(),
                        ..AdminAppserviceHealth::default()
                    },
                    backlog: Vec::new(),
                    created_at: hs_http::time::now_rfc3339(),
                },
            );
        self
    }

    /// Gives `id` a backlog.
    pub fn with_backlog(self, id: &str, entries: Vec<AdminAppserviceBacklogEntry>) -> Self {
        if let Some(row) = self
            .rows
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get_mut(id)
        {
            row.backlog = entries;
        }
        self
    }

    /// Makes `ping` fail for `id`.
    pub fn unreachable(self, id: &str) -> Self {
        self.unreachable
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(id.to_owned());
        self
    }

    fn view(row: &InMemoryAppserviceRow) -> AdminAppservice {
        let r = &row.registration;
        AdminAppservice {
            id: r["id"].as_str().unwrap_or_default().to_owned(),
            sender_localpart: r["sender_localpart"]
                .as_str()
                .unwrap_or_default()
                .to_owned(),
            url: r["url"].as_str().map(str::to_owned),
            namespaces: r.get("namespaces").cloned().unwrap_or_else(|| json!({})),
            rate_limited: r["rate_limited"].as_bool().unwrap_or(true),
            protocols: r["protocols"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(str::to_owned))
                        .collect()
                })
                .unwrap_or_default(),
            paused: row.paused,
            health: if row.paused {
                "paused".to_owned()
            } else {
                row.health.status.clone()
            },
            created_at: row.created_at.clone(),
            links: AdminAppserviceLinks::default(),
        }
    }

    fn with_row<T>(
        &self,
        id: &str,
        f: impl FnOnce(&mut InMemoryAppserviceRow) -> T,
    ) -> Result<T, SourceError> {
        let mut rows = self
            .rows
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        rows.get_mut(id).map(f).ok_or(SourceError::NotFound)
    }
}

#[async_trait]
impl AppserviceDirectory for InMemoryAppserviceDirectory {
    async fn list(&self) -> Result<Vec<AdminAppservice>, SourceError> {
        Ok(self
            .rows
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
            .map(Self::view)
            .collect())
    }

    async fn get(&self, id: &str) -> Result<Option<AdminAppservice>, SourceError> {
        Ok(self
            .rows
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(id)
            .map(Self::view))
    }

    async fn create(&self, request: AdminAppserviceCreate) -> Result<AdminAppservice, SourceError> {
        let registration = match (request.registration, request.registration_yaml) {
            (Some(json), _) => json,
            (None, Some(yaml)) => {
                serde_yaml_ng::from_str::<Value>(&yaml).map_err(|e| SourceError::InvalidField {
                    pointer: "/registration_yaml",
                    detail: e.to_string(),
                })?
            }
            (None, None) => {
                return Err(SourceError::InvalidField {
                    pointer: "/registration",
                    detail: "a registration is required, as JSON or as YAML".to_owned(),
                });
            }
        };
        let Some(id) = registration["id"].as_str().map(str::to_owned) else {
            return Err(SourceError::InvalidField {
                pointer: "/registration/id",
                detail: "required".to_owned(),
            });
        };
        let mut rows = self
            .rows
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if rows.contains_key(&id) {
            return Err(SourceError::Conflict(format!(
                "an appservice with id {id:?} is already registered"
            )));
        }
        let row = InMemoryAppserviceRow {
            registration,
            paused: false,
            health: AdminAppserviceHealth {
                status: "unknown".to_owned(),
                ..AdminAppserviceHealth::default()
            },
            backlog: Vec::new(),
            created_at: hs_http::time::now_rfc3339(),
        };
        let view = Self::view(&row);
        rows.insert(id, row);
        Ok(view)
    }

    async fn update(&self, id: &str, patch: Value) -> Result<AdminAppservice, SourceError> {
        self.with_row(id, |row| {
            hs_config::merge_patch(&mut row.registration, &patch);
            Self::view(row)
        })
    }

    async fn delete(&self, id: &str) -> Result<(), SourceError> {
        let mut rows = self
            .rows
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        rows.remove(id).map(|_| ()).ok_or(SourceError::NotFound)
    }

    async fn health(&self, id: &str) -> Result<AdminAppserviceHealth, SourceError> {
        self.with_row(id, |row| {
            let mut health = row.health.clone();
            if row.paused {
                health.status = "paused".to_owned();
            }
            health
        })
    }

    async fn backlog(&self, id: &str) -> Result<Vec<AdminAppserviceBacklogEntry>, SourceError> {
        self.with_row(id, |row| row.backlog.clone())
    }

    async fn pause(&self, id: &str) -> Result<AdminAppservice, SourceError> {
        self.with_row(id, |row| {
            row.paused = true;
            Self::view(row)
        })
    }

    async fn resume(&self, id: &str) -> Result<AdminAppservice, SourceError> {
        self.with_row(id, |row| {
            row.paused = false;
            Self::view(row)
        })
    }

    async fn rotate_tokens(&self, id: &str) -> Result<AdminAppserviceTokens, SourceError> {
        self.with_row(id, |row| {
            let tokens = AdminAppserviceTokens {
                as_token: format!(
                    "as_{}",
                    hs_http::time::now_rfc3339().replace([':', '.'], "")
                ),
                hs_token: format!(
                    "hs_{}",
                    hs_http::time::now_rfc3339().replace([':', '.'], "")
                ),
            };
            row.registration["as_token"] = json!(tokens.as_token);
            row.registration["hs_token"] = json!(tokens.hs_token);
            tokens
        })
    }

    async fn registration(&self, id: &str) -> Result<AdminAppserviceRegistration, SourceError> {
        self.with_row(id, |row| AdminAppserviceRegistration {
            json: row.registration.clone(),
            yaml: serde_yaml_ng::to_string(&row.registration).unwrap_or_default(),
        })
    }

    async fn ping(&self, id: &str) -> Result<AdminAppserviceHealth, SourceError> {
        let unreachable = self
            .unreachable
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .any(|u| u == id);
        self.with_row(id, |row| {
            row.health = AdminAppserviceHealth {
                status: if unreachable { "down" } else { "healthy" }.to_owned(),
                last_ping_at: Some(hs_http::time::now_rfc3339()),
                last_error: unreachable.then(|| "connection refused".to_owned()),
            };
            let mut health = row.health.clone();
            if row.paused {
                health.status = "paused".to_owned();
            }
            health
        })
    }

    async fn replay(&self, id: &str, request: AdminAppserviceReplay) -> Result<usize, SourceError> {
        self.with_row(id, |row| {
            let mut replayed = 0;
            for entry in &mut row.backlog {
                let wanted = request.transaction_ids.is_empty()
                    || request.transaction_ids.contains(&entry.transaction_id);
                if entry.dead_lettered && wanted {
                    entry.dead_lettered = false;
                    entry.attempts = 0;
                    replayed += 1;
                }
            }
            replayed
        })
    }
}
