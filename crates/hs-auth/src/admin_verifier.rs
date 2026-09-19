//! [`hs_admin::auth::TokenVerifier`] implemented over this crate's own [`crate::store::AuthStore`].
//!
//! `hs-admin`'s 142 `/api/v1` operations are mounted behind `hs_admin::auth::require_scope`, which
//! calls a `TokenVerifier` to turn a bearer token into a `hs_admin::model::Principal`
//! (`docs/rfcs/0004-admin-api.md` section 8.1; `crates/hs-admin/src/auth.rs`). Until this module
//! existed, `hs serve` wired an empty `hs_admin::auth::StaticVerifier`
//! (`crates/hs-cli/src/serve.rs::dummy_admin_state`), so every admin request answered `401`
//! regardless of credentials.
//!
//! [`AdminTokenVerifier`] recognizes exactly one kind of credential, per the trait's own doc
//! comment ("legacy Matrix access tokens of server-administrator users, which are treated as
//! `admin:read` and `admin:write`"): a normal `syt_...` client-server access token
//! (`crate::token`) belonging to a [`crate::store::UserRecord`] with `is_admin: true`. There is no
//! separate admin-token type or admin login flow — an operator becomes an admin API caller by
//! logging in as (or being given a token for) a user this server's `is_admin` flag is set on, the
//! same account that can otherwise use the ordinary client-server API. The native OAuth issuer's
//! own admin scopes (RFC 0003) are a Phase 1/2 addition; this verifier is the Phase 0 legacy path
//! the trait's doc comment names, and the design already anticipates both existing side by side
//! (`Principal.issued_by` distinguishes them).
//!
//! # Scope model
//!
//! A legacy admin token is unconditionally granted both `admin:read` and `admin:write`
//! (`hs_admin::model::Scope`) — there is no finer-grained legacy admin role to map from, and
//! `Scope::AdminWrite::satisfies` already implies every other scope in the catalog
//! (`crates/hs-admin/src/model.rs`), so this is "full access", matching what `is_admin` means
//! everywhere else in this server (see `crate::requester::Requester::is_admin`). A non-admin
//! user's otherwise-valid access token is rejected with [`hs_admin::auth::AuthError::Invalid`],
//! the same answer an unrecognized token gets — deliberately, so a client-server token leaking
//! into a probe of the admin surface does not confirm "this credential is real, it's just
//! unprivileged" to an attacker; see "Decisions made" in `docs/status/07-auth-and-identity.md`.
//!
//! # Construction
//!
//! [`AdminTokenVerifier::from_auth_state`] is the constructor `hs serve` should use: it shares the
//! already-open [`crate::store::AuthStore`] and [`crate::clock::Clock`] an [`crate::state::AuthState`]
//! holds, so the admin surface and the client-server surface read the same user/token data through
//! the same open backend handle rather than opening a second one. See
//! `docs/status/07-auth-and-identity.md`'s "Interfaces provided" for the exact `hs-cli` diff.

use std::sync::Arc;

use hs_admin::auth::{AuthError, TokenVerifier};
use hs_admin::model::{Principal, PrincipalKind, Scope};

use crate::clock::Clock;
use crate::state::AuthState;
use crate::store::{AuthStore, StoreError};
use crate::token::TokenHash;

/// Verifies a legacy Matrix access token belonging to a server-administrator user
/// (`is_admin: true`), for `hs-admin`'s `/api/v1` surface. See the module doc for the scope model
/// and why non-admin tokens and unrecognized tokens answer identically.
pub struct AdminTokenVerifier {
    store: Arc<dyn AuthStore>,
    clock: Arc<dyn Clock>,
}

impl AdminTokenVerifier {
    /// Builds a verifier over an explicit store and clock. Prefer [`Self::from_auth_state`] when
    /// an [`AuthState`] is already in hand, so the admin surface shares its open backend handle.
    #[must_use]
    pub fn new(store: Arc<dyn AuthStore>, clock: Arc<dyn Clock>) -> Self {
        Self { store, clock }
    }

    /// Builds a verifier that shares `state`'s store and clock, rather than opening a second
    /// handle onto the same backend. This is what `hs serve` should construct
    /// `hs_admin::router::AdminState`'s verifier from once it has built its `AuthState` — see
    /// `docs/status/07-auth-and-identity.md`'s "Interfaces provided".
    #[must_use]
    pub fn from_auth_state(state: &AuthState) -> Self {
        Self::new(state.store.clone(), state.clock.clone())
    }
}

fn store_unavailable(err: StoreError) -> AuthError {
    AuthError::Unavailable(err.to_string())
}

/// Formats milliseconds-since-epoch as the RFC 3339 shape `hs-admin` uses elsewhere
/// (`hs_http::time::format_rfc3339`'s `2026-09-17T21:04:05.123Z` shape, reimplemented here rather
/// than depending on `hs-http` for one formatting call — see `docs/status/07-auth-and-identity.md`
/// "Decisions made" for why this crate does not depend on `hs-http`). Falls back to the Unix
/// epoch on an out-of-range value rather than panicking; `Principal.expires_at` is
/// display/audit-only information, never consulted for the accept/reject decision itself (that
/// uses `expires_at_ms` directly, before this function is ever called).
pub(crate) fn format_rfc3339_ms(ms: u64) -> String {
    use time::OffsetDateTime;
    use time::macros::format_description;

    // A fixed 3-digit subsecond field, unlike `well_known::Rfc3339`, which omits the fraction
    // entirely for a whole-second value -- this crate always wants `.SSS`, matching the shape
    // `hs_http::time::format_rfc3339` documents for the rest of the admin surface (RFC 0004
    // decision D15.2).
    const FORMAT: &[time::format_description::FormatItem<'_>] =
        format_description!("[year]-[month]-[day]T[hour]:[minute]:[second].[subsecond digits:3]Z");
    const EPOCH: &str = "1970-01-01T00:00:00.000Z";

    let secs = i64::try_from(ms / 1000).unwrap_or(0);
    let nanos = u32::try_from((ms % 1000) * 1_000_000).unwrap_or(0);
    OffsetDateTime::from_unix_timestamp(secs)
        .and_then(|dt| dt.replace_nanosecond(nanos))
        .map(|dt| dt.format(FORMAT).unwrap_or_else(|_| EPOCH.to_string()))
        .unwrap_or_else(|_| EPOCH.to_string())
}

#[async_trait::async_trait]
impl TokenVerifier for AdminTokenVerifier {
    async fn verify(&self, bearer: &str) -> Result<Principal, AuthError> {
        let hash = TokenHash::of(bearer);
        let record = self
            .store
            .get_access_token(&hash)
            .await
            .map_err(store_unavailable)?
            .ok_or(AuthError::Invalid)?;

        if let Some(expires_at_ms) = record.expires_at_ms
            && expires_at_ms < self.clock.now_ms()
        {
            return Err(AuthError::Expired);
        }

        let user = self
            .store
            .get_user(&record.user_id)
            .await
            .map_err(store_unavailable)?
            .ok_or(AuthError::Invalid)?;

        // Non-admin: answer exactly like an unrecognized token (see module doc). Checked before
        // deactivated/locked so those two states -- which imply the account *was* an admin token
        // holder and has since had access taken away -- stay distinguishable as `Revoked`.
        if !user.is_admin {
            return Err(AuthError::Invalid);
        }
        if user.deactivated || user.locked {
            return Err(AuthError::Revoked);
        }

        Ok(Principal {
            kind: PrincipalKind::Legacy,
            id: user.user_id.to_string(),
            display_name: None,
            scopes: vec![Scope::AdminRead, Scope::AdminWrite],
            token_id: Some(hash.to_hex()),
            expires_at: record.expires_at_ms.map(format_rfc3339_ms),
            issued_by: Some("legacy-admin-flag".to_string()),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::FixedClock;
    use crate::store::memory::InMemoryAuthStore;
    use crate::store::{AccessTokenRecord, TokenStore, UserRecord, UserStore};

    fn verifier_with(
        store: InMemoryAuthStore,
        now_ms: u64,
    ) -> (AdminTokenVerifier, Arc<FixedClock>) {
        let clock = Arc::new(FixedClock::new(now_ms));
        let verifier = AdminTokenVerifier::new(Arc::new(store), clock.clone());
        (verifier, clock)
    }

    async fn admin_user_with_token(
        store: &InMemoryAuthStore,
        user_id: &ruma::UserId,
        token: &str,
        expires_at_ms: Option<u64>,
    ) {
        store
            .create_user(UserRecord::new(user_id.to_owned(), 0))
            .await
            .unwrap();
        store.set_admin(user_id, true).await.unwrap();
        store
            .put_access_token(AccessTokenRecord {
                hash: TokenHash::of(token),
                user_id: user_id.to_owned(),
                device_id: None,
                expires_at_ms,
                refresh_token_hash: None,
                last_used_ms: None,
            })
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn unknown_token_is_invalid() {
        let (verifier, _clock) = verifier_with(InMemoryAuthStore::new(), 1000);
        let err = verifier.verify("syt_nope").await.unwrap_err();
        assert!(matches!(err, AuthError::Invalid));
    }

    #[tokio::test]
    async fn admin_user_token_grants_full_scope() {
        let store = InMemoryAuthStore::new();
        let user_id = ruma::user_id!("@admin:example.org");
        admin_user_with_token(&store, user_id, "syt_admintoken", None).await;
        let (verifier, _clock) = verifier_with(store, 1000);

        let principal = verifier.verify("syt_admintoken").await.unwrap();
        assert_eq!(principal.id, "@admin:example.org");
        assert_eq!(principal.kind, PrincipalKind::Legacy);
        assert!(principal.has_scope(Scope::AdminRead));
        assert!(principal.has_scope(Scope::AdminWrite));
        assert!(principal.has_scope(Scope::BridgesWrite));
        assert!(principal.token_id.is_some());
    }

    #[tokio::test]
    async fn non_admin_user_token_is_invalid_not_forbidden() {
        let store = InMemoryAuthStore::new();
        let user_id = ruma::user_id!("@plain:example.org");
        store
            .create_user(UserRecord::new(user_id.to_owned(), 0))
            .await
            .unwrap();
        store
            .put_access_token(AccessTokenRecord {
                hash: TokenHash::of("syt_plaintoken"),
                user_id: user_id.to_owned(),
                device_id: None,
                expires_at_ms: None,
                refresh_token_hash: None,
                last_used_ms: None,
            })
            .await
            .unwrap();
        let (verifier, _clock) = verifier_with(store, 1000);

        let err = verifier.verify("syt_plaintoken").await.unwrap_err();
        assert!(matches!(err, AuthError::Invalid));
    }

    #[tokio::test]
    async fn expired_admin_token_is_expired() {
        let store = InMemoryAuthStore::new();
        let user_id = ruma::user_id!("@admin:example.org");
        admin_user_with_token(&store, user_id, "syt_admintoken", Some(500)).await;
        let (verifier, _clock) = verifier_with(store, 1000);

        let err = verifier.verify("syt_admintoken").await.unwrap_err();
        assert!(matches!(err, AuthError::Expired));
    }

    #[tokio::test]
    async fn not_yet_expired_admin_token_still_verifies() {
        let store = InMemoryAuthStore::new();
        let user_id = ruma::user_id!("@admin:example.org");
        admin_user_with_token(&store, user_id, "syt_admintoken", Some(5000)).await;
        let (verifier, _clock) = verifier_with(store, 1000);

        let principal = verifier.verify("syt_admintoken").await.unwrap();
        assert_eq!(
            principal.expires_at.as_deref(),
            Some("1970-01-01T00:00:05.000Z")
        );
    }

    #[tokio::test]
    async fn locked_admin_user_is_revoked() {
        let store = InMemoryAuthStore::new();
        let user_id = ruma::user_id!("@admin:example.org");
        admin_user_with_token(&store, user_id, "syt_admintoken", None).await;
        store.set_locked(user_id, true).await.unwrap();
        let (verifier, _clock) = verifier_with(store, 1000);

        let err = verifier.verify("syt_admintoken").await.unwrap_err();
        assert!(matches!(err, AuthError::Revoked));
    }

    #[tokio::test]
    async fn deactivated_admin_user_is_revoked() {
        let store = InMemoryAuthStore::new();
        let user_id = ruma::user_id!("@admin:example.org");
        admin_user_with_token(&store, user_id, "syt_admintoken", None).await;
        store.set_deactivated(user_id, true).await.unwrap();
        let (verifier, _clock) = verifier_with(store, 1000);

        let err = verifier.verify("syt_admintoken").await.unwrap_err();
        assert!(matches!(err, AuthError::Revoked));
    }

    #[test]
    fn from_auth_state_shares_the_state_store() {
        let state = AuthState::in_memory();
        let _verifier = AdminTokenVerifier::from_auth_state(&state);
    }
}
