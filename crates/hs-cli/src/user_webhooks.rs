//! The user webhooks API's caller: `hs_bridges::user_webhooks` authenticates a request through
//! this, which is the ordinary `Requester` middleware over the server's [`AuthState`]. The same
//! token a Matrix client uses, the same checks (expiry, deactivation, lock, guests refused,
//! an appservice masquerading as one of its users), and the token itself handed on so the
//! server can act as the caller over loopback for what they asked.

use async_trait::async_trait;
use axum::extract::FromRequestParts;
use axum::http::header::{AUTHORIZATION, USER_AGENT};
use axum::http::request::Parts;
use axum::response::{IntoResponse, Response};
use hs_auth::{AuthState, Requester};
use hs_bridges::user_webhooks::{Caller, CallerResolver};

/// [`CallerResolver`] over the server's [`AuthState`].
pub struct AuthCallerResolver {
    auth: AuthState,
}

impl AuthCallerResolver {
    /// Over `auth`.
    #[must_use]
    pub fn new(auth: AuthState) -> Self {
        Self { auth }
    }
}

#[async_trait]
impl CallerResolver for AuthCallerResolver {
    async fn resolve(&self, parts: &mut Parts) -> Result<Caller, Response> {
        let requester = Requester::from_request_parts(parts, &self.auth)
            .await
            .map_err(IntoResponse::into_response)?;
        // The token the request carried: the header, or the spec's deprecated query form,
        // which `Requester` accepts too.
        let token = parts
            .headers
            .get(AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .map(str::trim)
            .map(str::to_owned)
            .or_else(|| {
                parts.uri.query().and_then(|q| {
                    q.split('&')
                        .find_map(|pair| pair.strip_prefix("access_token="))
                        .map(str::to_owned)
                })
            })
            .ok_or_else(|| hs_auth::MatrixError::missing_token().into_response())?;
        Ok(Caller {
            user_id: requester.user_id.to_string(),
            token,
            ip: hs_http::buckets::ClientIp::from_parts(parts).key(),
            user_agent: parts
                .headers
                .get(USER_AGENT)
                .and_then(|v| v.to_str().ok())
                .map(str::to_owned),
        })
    }
}
