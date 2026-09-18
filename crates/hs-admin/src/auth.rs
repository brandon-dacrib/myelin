//! Authentication and scope enforcement (RFC 0004 section 8).
//!
//! [`TokenVerifier`] is the trait track 07 implements once its OAuth issuer and legacy-token
//! path exist; [`require_scope`] is the enforcement point the router skeleton (`crate::router`)
//! calls before running a handler. Neither depends on how 07 verifies a token, only on the
//! [`Principal`](crate::model::Principal) it produces.

use crate::model::{Principal, Scope};

/// Why authentication failed. Only [`AuthError::Unavailable`] becomes `503`; everything else is
/// `401 unauthenticated` (RFC 0004 section 8.1).
#[derive(Debug, Clone, thiserror::Error)]
pub enum AuthError {
    #[error("the token is not recognized")]
    Invalid,
    #[error("the token has expired")]
    Expired,
    #[error("the token has been revoked")]
    Revoked,
    #[error("the token verifier is temporarily unavailable: {0}")]
    Unavailable(String),
}

impl AuthError {
    pub fn to_problem(&self) -> hs_http::Problem {
        match self {
            AuthError::Unavailable(detail) => {
                hs_http::Problem::unavailable().with_detail(detail.clone())
            }
            other => hs_http::Problem::unauthenticated()
                .with_detail(other.to_string())
                .with_header(
                    axum::http::header::WWW_AUTHENTICATE,
                    axum::http::HeaderValue::from_static(
                        r#"Bearer realm="hs-admin", error="invalid_token""#,
                    ),
                ),
        }
    }
}

/// Verifies a bearer token into a [`Principal`]. Implemented by track 07 (OAuth 2.0 access
/// tokens, plus legacy Matrix access tokens of server-administrator users, which are treated as
/// `admin:read` and `admin:write`; see RFC 0004 section 8.1).
#[async_trait::async_trait]
pub trait TokenVerifier: Send + Sync {
    async fn verify(&self, bearer: &str) -> Result<Principal, AuthError>;
}

/// A verifier for tests and the mock server: an in-memory map from bearer token to the
/// [`Principal`] it authenticates as.
pub struct StaticVerifier {
    tokens: std::collections::HashMap<String, Principal>,
}

impl StaticVerifier {
    pub fn new() -> Self {
        Self {
            tokens: std::collections::HashMap::new(),
        }
    }

    pub fn with_token(mut self, token: impl Into<String>, principal: Principal) -> Self {
        self.tokens.insert(token.into(), principal);
        self
    }
}

impl Default for StaticVerifier {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl TokenVerifier for StaticVerifier {
    async fn verify(&self, bearer: &str) -> Result<Principal, AuthError> {
        self.tokens.get(bearer).cloned().ok_or(AuthError::Invalid)
    }
}

/// The outcome of enforcing one operation's scope requirement.
#[derive(Debug, Clone)]
pub enum ScopeDecision {
    Allowed(Principal),
    Unauthenticated(hs_http::Problem),
    InsufficientScope(hs_http::Problem),
}

/// Extracts the bearer token from an `Authorization` header, verifies it, and checks it against
/// `required` (`None` means "any authenticated principal", as for `GET /me`). This is the single
/// enforcement point the router skeleton wires every operation through.
pub async fn require_scope(
    verifier: &dyn TokenVerifier,
    authorization: Option<&str>,
    required: Option<Scope>,
) -> ScopeDecision {
    let Some(header) = authorization else {
        return ScopeDecision::Unauthenticated(
            hs_http::Problem::unauthenticated().with_detail("missing Authorization header"),
        );
    };
    let Some(bearer) = header.strip_prefix("Bearer ") else {
        return ScopeDecision::Unauthenticated(
            hs_http::Problem::unauthenticated()
                .with_detail("Authorization header is not a Bearer token"),
        );
    };
    let principal = match verifier.verify(bearer).await {
        Ok(p) => p,
        Err(e) => return ScopeDecision::Unauthenticated(e.to_problem()),
    };
    if let Some(required) = required
        && !principal.has_scope(required)
    {
        return ScopeDecision::InsufficientScope(
            hs_http::Problem::insufficient_scope()
                .with_detail(format!(
                    "this operation requires the {} scope",
                    required.as_str()
                ))
                .with_required_scope(required.as_str()),
        );
    }
    ScopeDecision::Allowed(principal)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::PrincipalKind;

    fn principal(scopes: Vec<Scope>) -> Principal {
        Principal {
            kind: PrincipalKind::User,
            id: "@ops:example.org".into(),
            display_name: None,
            scopes,
            token_id: None,
            expires_at: None,
            issued_by: None,
        }
    }

    #[tokio::test]
    async fn missing_header_is_unauthenticated() {
        let verifier = StaticVerifier::new();
        let decision = require_scope(&verifier, None, Some(Scope::AdminRead)).await;
        assert!(matches!(decision, ScopeDecision::Unauthenticated(_)));
    }

    #[tokio::test]
    async fn unknown_token_is_unauthenticated() {
        let verifier = StaticVerifier::new();
        let decision = require_scope(&verifier, Some("Bearer nope"), Some(Scope::AdminRead)).await;
        assert!(matches!(decision, ScopeDecision::Unauthenticated(_)));
    }

    #[tokio::test]
    async fn insufficient_scope_is_reported() {
        let verifier = StaticVerifier::new().with_token("tok", principal(vec![Scope::BridgesRead]));
        let decision = require_scope(&verifier, Some("Bearer tok"), Some(Scope::AdminWrite)).await;
        assert!(matches!(decision, ScopeDecision::InsufficientScope(_)));
    }

    #[tokio::test]
    async fn sufficient_scope_is_allowed() {
        let verifier = StaticVerifier::new().with_token("tok", principal(vec![Scope::AdminWrite]));
        let decision =
            require_scope(&verifier, Some("Bearer tok"), Some(Scope::BridgesWrite)).await;
        assert!(matches!(decision, ScopeDecision::Allowed(_)));
    }

    #[tokio::test]
    async fn none_required_allows_any_authenticated_principal() {
        let verifier = StaticVerifier::new().with_token("tok", principal(vec![]));
        let decision = require_scope(&verifier, Some("Bearer tok"), None).await;
        assert!(matches!(decision, ScopeDecision::Allowed(_)));
    }
}
