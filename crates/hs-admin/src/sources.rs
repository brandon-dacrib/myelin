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

// `AdminRoom` is defined in `crate::model` but not imported here yet: the `RoomDirectory`
// trait that will use it is the next slice of this seam (`docs/status/15-admin-api-and-modules.md`).
use crate::model::{AdminUser, ExternalId, ThreePid};

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
#[derive(Debug, Clone, Default)]
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
    Threepid { medium: String, address: String },
    ExternalId { provider: String, external_id: String },
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
    }
}
