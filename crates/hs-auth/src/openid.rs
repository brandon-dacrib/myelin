//! OpenID tokens: `POST /_matrix/client/v3/user/{userId}/openid/request_token` mints one, and
//! a third party (an integration manager, a widget) presents it to this server's
//! `GET /_matrix/federation/v1/openid/userinfo` to learn whose it is ([`userinfo`]). It proves an
//! identity and grants nothing else: it is not an access token, and the userinfo endpoint is the
//! one place it is accepted.
//!
//! A token lives an hour (`expires_in: 3600`, Synapse's `openid_token_lifetime`), may be checked
//! any number of times while it lives, and is kept as its hash only, like every other token
//! this crate issues.

use std::sync::LazyLock;

use axum::Json;
use axum::extract::{Path, State};
use prometheus_client::metrics::counter::Counter;
use ruma::OwnedUserId;
use serde_json::{Value, json};

use crate::error::MatrixError;
use crate::requester::Requester;
use crate::state::AuthState;
use crate::store::{AuthStore, OpenIdTokenRecord, StoreError};
use crate::token::TokenHash;

/// How long an OpenID token lives, in seconds.
pub const OPENID_TOKEN_LIFETIME_SECS: u64 = 3600;

static ISSUED: LazyLock<Counter> = LazyLock::new(Counter::default);
static CHECKED: LazyLock<Counter> = LazyLock::new(Counter::default);

/// Registers `hs_auth_openid_tokens_issued_total` and `hs_auth_openid_tokens_checked_total`
/// (checks that named a live token) into `registry`.
pub fn register_metrics(registry: &mut prometheus_client::registry::Registry) {
    registry.register(
        "hs_auth_openid_tokens_issued",
        "OpenID tokens issued by POST /user/{userId}/openid/request_token",
        ISSUED.clone(),
    );
    registry.register(
        "hs_auth_openid_tokens_checked",
        "OpenID tokens a third party checked successfully at /openid/userinfo",
        CHECKED.clone(),
    );
}

/// `POST /user/{userId}/openid/request_token`: an OpenID token for the requester, who must be
/// `userId` (`403 M_FORBIDDEN` otherwise, as Synapse answers "Cannot request tokens for other
/// users.").
///
/// # Errors
/// `403` for another user's ID; `500` on a storage failure.
pub async fn post_request_token(
    State(state): State<AuthState>,
    requester: Requester,
    Path(user_id): Path<String>,
) -> Result<Json<Value>, MatrixError> {
    if user_id != requester.user_id.as_str() {
        return Err(MatrixError::forbidden(
            "Cannot request OpenID tokens for other users",
        ));
    }
    let token = crate::token::generate_openid_token();
    let now = state.now_ms();
    state
        .store
        .put_openid_token(
            OpenIdTokenRecord {
                hash: TokenHash::of(&token),
                user_id: requester.user_id.clone(),
                expires_at_ms: now + OPENID_TOKEN_LIFETIME_SECS * 1000,
            },
            now,
        )
        .await?;
    ISSUED.inc();
    tracing::debug!(user = %requester.user_id, "issued an OpenID token");
    Ok(Json(json!({
        "access_token": token,
        "token_type": "Bearer",
        "matrix_server_name": state.server_name(),
        "expires_in": OPENID_TOKEN_LIFETIME_SECS,
    })))
}

/// The user an OpenID token proves, for `GET /_matrix/federation/v1/openid/userinfo`: `None`
/// for a token this server did not issue, one that expired, or one whose account has since been
/// deactivated.
///
/// # Errors
/// A storage failure.
pub async fn userinfo(
    store: &dyn AuthStore,
    token: &str,
    now_ms: u64,
) -> Result<Option<OwnedUserId>, StoreError> {
    let Some(record) = store
        .get_openid_token(&TokenHash::of(token), now_ms)
        .await?
    else {
        return Ok(None);
    };
    let active = store
        .get_user(&record.user_id)
        .await?
        .is_some_and(|u| !u.deactivated);
    if active {
        CHECKED.inc();
    }
    Ok(active.then_some(record.user_id))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::UserRecord;

    #[tokio::test]
    async fn a_token_names_its_user_until_it_expires_or_the_account_goes() {
        let state = AuthState::in_memory();
        let alice = ruma::user_id!("@alice:example.org").to_owned();
        state
            .store
            .create_user(UserRecord::new(alice.clone(), 0))
            .await
            .unwrap();
        let Json(body) = post_request_token(
            State(state.clone()),
            Requester::for_user(alice.clone()),
            Path(alice.to_string()),
        )
        .await
        .unwrap();
        assert_eq!(body["token_type"], "Bearer");
        assert_eq!(body["matrix_server_name"], "example.org");
        assert_eq!(body["expires_in"], 3600);
        let token = body["access_token"].as_str().unwrap();

        let now = state.now_ms();
        assert_eq!(
            userinfo(state.store.as_ref(), token, now).await.unwrap(),
            Some(alice.clone())
        );
        assert_eq!(
            userinfo(state.store.as_ref(), token, now).await.unwrap(),
            Some(alice.clone()),
            "not single-use"
        );
        assert_eq!(
            userinfo(state.store.as_ref(), "an/invalid/token", now)
                .await
                .unwrap(),
            None
        );
        assert_eq!(
            userinfo(state.store.as_ref(), token, now + 3_601_000)
                .await
                .unwrap(),
            None,
            "expired"
        );
        state.store.set_deactivated(&alice, true).await.unwrap();
        assert_eq!(
            userinfo(state.store.as_ref(), token, now).await.unwrap(),
            None,
            "deactivated"
        );
    }

    #[tokio::test]
    async fn nobody_requests_a_token_for_somebody_else() {
        let state = AuthState::in_memory();
        let err = post_request_token(
            State(state),
            Requester::for_user(ruma::user_id!("@alice:example.org").to_owned()),
            Path("@bob:example.org".to_owned()),
        )
        .await
        .unwrap_err();
        assert_eq!(err.status(), axum::http::StatusCode::FORBIDDEN);
    }
}
