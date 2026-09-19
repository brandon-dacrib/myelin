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

use crate::model::AdminUser;

/// Why a data-source call failed. Mirrors [`crate::auth::AuthError`]'s "only unavailable escapes
/// as something other than the obvious status" shape: [`SourceError::NotFound`] maps to `404
/// not-found`, [`SourceError::Unavailable`] to `503 unavailable`, [`SourceError::Invalid`] to
/// `400 validation-failed` (see [`SourceError::to_problem`]).
#[derive(Debug, thiserror::Error)]
pub enum SourceError {
    #[error("not found")]
    NotFound,
    #[error("the data source is temporarily unavailable: {0}")]
    Unavailable(String),
    #[error("invalid request: {0}")]
    Invalid(String),
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
        }
    }
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
#[async_trait]
pub trait UserDirectory: Send + Sync + 'static {
    async fn get_user(&self, user_id: &str) -> Result<Option<AdminUser>, SourceError>;
    async fn list_users(&self, filter: &UserFilter) -> Result<Vec<AdminUser>, SourceError>;
    async fn set_admin(&self, user_id: &str, admin: bool) -> Result<(), SourceError>;
    async fn set_locked(&self, user_id: &str, locked: bool) -> Result<(), SourceError>;
    async fn set_deactivated(&self, user_id: &str, deactivated: bool) -> Result<(), SourceError>;
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
