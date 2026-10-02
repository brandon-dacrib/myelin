//! The `admin_tokens.*` operations: admin API tokens narrower than a full administrator's.
//!
//! Until these existed the admin API had one kind of credential, a Matrix access token of a user
//! with the administrator flag, which the legacy verifier grants `admin:read` and `admin:write`
//! (RFC 0004 section 8.1's `legacy` principal). The six scopes were enforced on every operation
//! but no token could hold fewer than all of them, so an operator could not hand a bridge team a
//! `bridges:read` token or a moderator a `moderation:*` one. An admin token is minted here with
//! the scopes its holder needs (RFC 0004 section 8.1's `service_account` principal), shown once,
//! stored hashed, and verified by [`ScopedTokenVerifier`] in front of the legacy verifier.
//!
//! The handlers here own the admin API's half: scope checks, validating the request (a name, a
//! non-empty set of known scopes, an expiry in the future), minting the secret and hashing it,
//! idempotency, and the audit entry and event every mutation writes. The audit entry of a mint
//! carries the token's scopes, so the log says what a token could do even after it is revoked.
//! Durable storage lives behind [`AdminTokenSource`], which `hs-cli` implements over the same
//! backend as everything else; the in-memory implementation here serves tests and the mock.
//!
//! # The secret
//!
//! A token is `hsa_` and 40 letters and digits, so one is recognisable in a log or a config
//! file for what it is, and the verifier can tell it from a Matrix access token (`syt_...`)
//! without a lookup. Only its SHA-256 is stored; the plain token appears in exactly one
//! response, the `201` of `POST /admin-tokens`. A lost token is revoked and re-minted, as with
//! any API key.
//!
//! # What holding a token means
//!
//! `Principal.scopes` is exactly the list the token was minted with, and `Scope::satisfies`
//! does the rest: `admin:write` implies everything, each `*:write` its own `*:read`, and
//! `admin:read` every other `:read`. Only an `admin:write` holder can mint, list, inspect or
//! revoke tokens, so a token can never mint one wider than its own holder's grant.

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex, PoisonError};

use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use hs_http::Problem;
use rand::Rng;
use rand::distr::Alphanumeric;
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};

use crate::auth::{AuthError, ScopeDecision, TokenVerifier, require_scope};
use crate::idempotency::{Replay, StoredResponse};
use crate::model::{AuditChange, Page, Principal, PrincipalKind, ResourceRef, Scope};
use crate::router::{
    AdminState, authorization_header, idempotency_key, record_mutation,
    record_mutation_with_status, replay_response, source_unavailable,
};
use crate::sources::SourceError;

/// What every minted token starts with. The verifier routes a bearer with this prefix to the
/// admin token store and everything else to the legacy verifier.
pub const TOKEN_PREFIX: &str = "hsa_";

/// Letters and digits after the prefix: 40 of them is 238 bits of entropy.
pub const SECRET_LENGTH: usize = 40;

/// The longest name the API accepts.
pub const MAX_NAME_LENGTH: usize = 100;

/// The scopes a token gets when the request does not choose: a full administrator's, so a
/// token minted without thinking about scopes does what the legacy credential does.
pub const DEFAULT_SCOPES: [Scope; 2] = [Scope::AdminRead, Scope::AdminWrite];

/// What `issued_by` says on a principal this module verified.
pub const ISSUER: &str = "admin-tokens";

/// Every scope in the catalog, in the order the document lists them.
pub const ALL_SCOPES: [Scope; 6] = [
    Scope::AdminRead,
    Scope::AdminWrite,
    Scope::BridgesRead,
    Scope::BridgesWrite,
    Scope::ModerationRead,
    Scope::ModerationWrite,
];

/// The OpenAPI `AdminToken` schema: a token as it is listed, without its secret.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdminToken {
    /// The token's id (a ULID): what revocation and the audit log name it by.
    pub id: String,
    /// What the operator called it, for telling tokens apart; not unique.
    pub name: String,
    /// The scopes it carries, in catalog order.
    pub scopes: Vec<Scope>,
    /// When it was minted (RFC 3339).
    pub created_at: String,
    /// The principal that minted it: the id of the user or token.
    pub created_by: String,
    /// When it stops working (RFC 3339), or `None` for never.
    pub expires_at: Option<String>,
}

/// The OpenAPI `AdminTokenCreated` schema: the `201` of `POST /admin-tokens`, the one place
/// the plain token appears.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdminTokenCreated {
    /// The token's record.
    #[serde(flatten)]
    pub token: AdminToken,
    /// The bearer token itself. Shown once; only its hash is stored.
    #[serde(rename = "token")]
    pub secret: String,
}

/// One stored token, as the source keeps it and the verifier reads it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdminTokenRecord {
    /// The listed shape.
    #[serde(flatten)]
    pub token: AdminToken,
    /// Lowercase hex SHA-256 of the plain token.
    pub secret_hash: String,
    /// When it stops working, milliseconds since the Unix epoch; `None` for never.
    pub expires_at_ms: Option<i64>,
}

/// A token to store, after the handler has validated the request and minted the secret.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewAdminToken {
    /// The record to store, id and timestamps already set.
    pub record: AdminTokenRecord,
}

/// Where the admin tokens live. Implemented by `hs-cli` over the server's backend, so a token
/// survives a restart; [`InMemoryAdminTokens`] for tests and the mock.
#[async_trait::async_trait]
pub trait AdminTokenSource: Send + Sync + 'static {
    /// Every token, oldest first.
    async fn list(&self) -> Result<Vec<AdminToken>, SourceError>;
    /// One token by id, or `None` if there is no such token.
    async fn get(&self, id: &str) -> Result<Option<AdminToken>, SourceError>;
    /// Stores a token. [`SourceError::Conflict`] if its id or hash exists.
    async fn create(&self, token: NewAdminToken) -> Result<AdminToken, SourceError>;
    /// Removes a token, which revokes it. [`SourceError::NotFound`] if there is no such token.
    async fn delete(&self, id: &str) -> Result<(), SourceError>;
    /// The record whose `secret_hash` is `hash`, for the verifier.
    async fn find_by_hash(&self, hash: &str) -> Result<Option<AdminTokenRecord>, SourceError>;
}

/// The in-memory source: a mutex-held map, for tests and `hs-admin-mock`.
#[derive(Default)]
pub struct InMemoryAdminTokens {
    tokens: Mutex<Vec<AdminTokenRecord>>,
}

impl InMemoryAdminTokens {
    /// An empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Vec<AdminTokenRecord>> {
        self.tokens.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

#[async_trait::async_trait]
impl AdminTokenSource for InMemoryAdminTokens {
    async fn list(&self) -> Result<Vec<AdminToken>, SourceError> {
        Ok(self.lock().iter().map(|r| r.token.clone()).collect())
    }

    async fn get(&self, id: &str) -> Result<Option<AdminToken>, SourceError> {
        Ok(self
            .lock()
            .iter()
            .find(|r| r.token.id == id)
            .map(|r| r.token.clone()))
    }

    async fn create(&self, token: NewAdminToken) -> Result<AdminToken, SourceError> {
        let mut tokens = self.lock();
        if tokens.iter().any(|r| {
            r.token.id == token.record.token.id || r.secret_hash == token.record.secret_hash
        }) {
            return Err(SourceError::Conflict(format!(
                "admin token {} already exists",
                token.record.token.id
            )));
        }
        let listed = token.record.token.clone();
        tokens.push(token.record);
        Ok(listed)
    }

    async fn delete(&self, id: &str) -> Result<(), SourceError> {
        let mut tokens = self.lock();
        let before = tokens.len();
        tokens.retain(|r| r.token.id != id);
        if tokens.len() == before {
            return Err(SourceError::NotFound);
        }
        Ok(())
    }

    async fn find_by_hash(&self, hash: &str) -> Result<Option<AdminTokenRecord>, SourceError> {
        Ok(self.lock().iter().find(|r| r.secret_hash == hash).cloned())
    }
}

/// Lowercase hex SHA-256 of `token`: what the store keeps and the verifier looks up by.
#[must_use]
pub fn hash_token(token: &str) -> String {
    let digest = Sha256::digest(token.as_bytes());
    let mut out = String::with_capacity(64);
    for byte in digest {
        use std::fmt::Write as _;
        // Writing to a `String` cannot fail.
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// A fresh token: [`TOKEN_PREFIX`] and [`SECRET_LENGTH`] random letters and digits.
#[must_use]
pub fn generate_token() -> String {
    let mut rng = rand::rng();
    let secret: String = (0..SECRET_LENGTH)
        .map(|_| char::from(rng.sample(Alphanumeric)))
        .collect();
    format!("{TOKEN_PREFIX}{secret}")
}

/// Milliseconds since the Unix epoch, now.
#[must_use]
pub fn now_ms() -> i64 {
    crate::registration_tokens::now_ms()
}

/// Puts `scopes` in catalog order without duplicates: what a token is stored and listed with.
#[must_use]
pub fn normalize_scopes(scopes: &[Scope]) -> Vec<Scope> {
    let set: BTreeSet<usize> = scopes
        .iter()
        .filter_map(|s| ALL_SCOPES.iter().position(|a| a == s))
        .collect();
    set.into_iter().map(|i| ALL_SCOPES[i]).collect()
}

/// Verifies admin tokens minted here, and hands every other bearer to `fallback` (the legacy
/// verifier for Matrix access tokens of administrators). A bearer that starts with
/// [`TOKEN_PREFIX`] is decided here alone: an unknown one is [`AuthError::Invalid`], an expired
/// one [`AuthError::Expired`], and the legacy verifier is never asked about it.
pub struct ScopedTokenVerifier {
    tokens: Arc<dyn AdminTokenSource>,
    fallback: Arc<dyn TokenVerifier>,
}

impl ScopedTokenVerifier {
    /// A verifier over `tokens`, falling back to `fallback` for bearers that are not admin
    /// tokens.
    #[must_use]
    pub fn new(tokens: Arc<dyn AdminTokenSource>, fallback: Arc<dyn TokenVerifier>) -> Self {
        Self { tokens, fallback }
    }
}

/// The principal a stored token authenticates as. `id` is the token's id and `display_name`
/// its name, so the audit log names the token exactly and an operator reads which one it was.
#[must_use]
pub fn principal_for(record: &AdminTokenRecord) -> Principal {
    Principal {
        kind: PrincipalKind::ServiceAccount,
        id: record.token.id.clone(),
        display_name: Some(record.token.name.clone()),
        scopes: record.token.scopes.clone(),
        token_id: Some(record.token.id.clone()),
        expires_at: record.token.expires_at.clone(),
        issued_by: Some(ISSUER.to_owned()),
    }
}

#[async_trait::async_trait]
impl TokenVerifier for ScopedTokenVerifier {
    async fn verify(&self, bearer: &str) -> Result<Principal, AuthError> {
        if !bearer.starts_with(TOKEN_PREFIX) {
            return self.fallback.verify(bearer).await;
        }
        let record = self
            .tokens
            .find_by_hash(&hash_token(bearer))
            .await
            .map_err(|e| AuthError::Unavailable(e.to_string()))?
            .ok_or(AuthError::Invalid)?;
        if let Some(expires_at_ms) = record.expires_at_ms
            && expires_at_ms <= now_ms()
        {
            return Err(AuthError::Expired);
        }
        Ok(principal_for(&record))
    }
}

/// The body of `POST /admin-tokens` as it arrives.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct CreateBody {
    name: Option<String>,
    scopes: Option<Vec<String>>,
    expires_at: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct ListQuery {
    limit: Option<usize>,
    cursor: Option<String>,
    include_total: Option<bool>,
}

fn invalid_field(pointer: &'static str, detail: impl Into<String>) -> SourceError {
    SourceError::InvalidField {
        pointer,
        detail: detail.into(),
    }
}

/// What a valid create request asks for, before the secret and id are minted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidatedCreate {
    /// The name, trimmed.
    pub name: String,
    /// The scopes, normalized.
    pub scopes: Vec<Scope>,
    /// The expiry in milliseconds since the Unix epoch, if any.
    pub expires_at_ms: Option<i64>,
    /// The expiry as given, re-formatted (RFC 3339), if any.
    pub expires_at: Option<String>,
}

/// Turns a create request into what to mint, or says which field is wrong. `scopes` absent
/// means [`DEFAULT_SCOPES`]; present and empty is refused, since a token that can do nothing is
/// a mistake, not a choice.
fn validate_create(body: CreateBody, now_ms: i64) -> Result<ValidatedCreate, SourceError> {
    let name = body.name.unwrap_or_default().trim().to_owned();
    if name.is_empty() {
        return Err(invalid_field(
            "/name",
            "name is required: what this token is for, so it can be told from the others",
        ));
    }
    if name.chars().count() > MAX_NAME_LENGTH {
        return Err(invalid_field(
            "/name",
            format!("name is at most {MAX_NAME_LENGTH} characters"),
        ));
    }
    let scopes = match body.scopes {
        None => DEFAULT_SCOPES.to_vec(),
        Some(given) => {
            if given.is_empty() {
                return Err(invalid_field(
                    "/scopes",
                    "scopes must name at least one scope; omit it for a full administrator's",
                ));
            }
            let mut parsed = Vec::with_capacity(given.len());
            for scope in &given {
                match Scope::parse(scope) {
                    Some(s) => parsed.push(s),
                    None => {
                        return Err(invalid_field(
                            "/scopes",
                            format!(
                                "unknown scope {scope:?}; the scopes are {}",
                                ALL_SCOPES
                                    .iter()
                                    .map(|s| s.as_str())
                                    .collect::<Vec<_>>()
                                    .join(", ")
                            ),
                        ));
                    }
                }
            }
            normalize_scopes(&parsed)
        }
    };
    let expires_at_ms = match body.expires_at.as_deref() {
        None => None,
        Some(value) => {
            let at = hs_http::time::parse_rfc3339(value).map_err(|e| {
                invalid_field(
                    "/expires_at",
                    format!("expires_at must be an RFC 3339 date-time: {e}"),
                )
            })?;
            let ms = i64::try_from(at.unix_timestamp_nanos() / 1_000_000)
                .map_err(|_| invalid_field("/expires_at", "expires_at is out of range"))?;
            if ms <= now_ms {
                return Err(invalid_field(
                    "/expires_at",
                    "expires_at is in the past; a new token must be usable",
                ));
            }
            Some(ms)
        }
    };
    let expires_at = expires_at_ms.map(format_ms);
    Ok(ValidatedCreate {
        name,
        scopes,
        expires_at_ms,
        expires_at,
    })
}

/// Milliseconds since the epoch as the RFC 3339 shape the rest of the API uses.
fn format_ms(ms: i64) -> String {
    time::OffsetDateTime::from_unix_timestamp_nanos(i128::from(ms) * 1_000_000)
        .map(hs_http::time::format_rfc3339)
        .unwrap_or_else(|_| "1970-01-01T00:00:00.000Z".to_owned())
}

/// Mints the secret and builds the record for a validated request: what `create` stores, split
/// out so the CLI and tests can build one the same way.
#[must_use]
pub fn mint(validated: ValidatedCreate, created_by: &str, now_ms: i64) -> (NewAdminToken, String) {
    let secret = generate_token();
    let record = AdminTokenRecord {
        token: AdminToken {
            id: crate::model::new_id(),
            name: validated.name,
            scopes: validated.scopes,
            created_at: format_ms(now_ms),
            created_by: created_by.to_owned(),
            expires_at: validated.expires_at,
        },
        secret_hash: hash_token(&secret),
        expires_at_ms: validated.expires_at_ms,
    };
    (NewAdminToken { record }, secret)
}

fn no_such_token(id: &str, instance: &str) -> Response {
    Problem::not_found()
        .with_detail(format!("no such admin token: {id}"))
        .with_instance(instance.to_owned())
        .into_response()
}

fn instance_for(id: &str) -> String {
    format!("/api/v1/admin-tokens/{id}")
}

/// `GET /api/v1/admin-tokens` (`admin:read`).
pub(crate) async fn list(
    State(state): State<AdminState>,
    headers: HeaderMap,
    Query(query): Query<ListQuery>,
) -> Response {
    let instance = "/api/v1/admin-tokens";
    match require_scope(
        state.verifier.as_ref(),
        authorization_header(&headers),
        Some(Scope::AdminRead),
    )
    .await
    {
        ScopeDecision::Allowed(_) => {
            let Some(source) = &state.admin_tokens else {
                return source_unavailable("admin token", instance);
            };
            match source.list().await {
                Ok(items) => axum::Json(Page::paginate(
                    items,
                    query.cursor.as_deref(),
                    query.limit,
                    query.include_total.unwrap_or(false),
                ))
                .into_response(),
                Err(e) => e.to_problem().with_instance(instance).into_response(),
            }
        }
        ScopeDecision::Unauthenticated(p) | ScopeDecision::InsufficientScope(p) => {
            p.with_instance(instance).into_response()
        }
    }
}

/// `GET /api/v1/admin-tokens/{id}` (`admin:read`).
pub(crate) async fn get(
    State(state): State<AdminState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    let instance = instance_for(&id);
    match require_scope(
        state.verifier.as_ref(),
        authorization_header(&headers),
        Some(Scope::AdminRead),
    )
    .await
    {
        ScopeDecision::Allowed(_) => {
            let Some(source) = &state.admin_tokens else {
                return source_unavailable("admin token", &instance);
            };
            match source.get(&id).await {
                Ok(Some(t)) => axum::Json(t).into_response(),
                Ok(None) => no_such_token(&id, &instance),
                Err(e) => e.to_problem().with_instance(instance).into_response(),
            }
        }
        ScopeDecision::Unauthenticated(p) | ScopeDecision::InsufficientScope(p) => {
            p.with_instance(instance).into_response()
        }
    }
}

/// `POST /api/v1/admin-tokens` (`admin:write`): `201` with the token, the only time it is
/// shown. Honors `Idempotency-Key`, so a retried mint returns the same token rather than a
/// second one. The audit entry records the name, scopes and expiry; never the token.
pub(crate) async fn create(
    State(state): State<AdminState>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    let instance = "/api/v1/admin-tokens";
    let principal = match require_scope(
        state.verifier.as_ref(),
        authorization_header(&headers),
        Some(Scope::AdminWrite),
    )
    .await
    {
        ScopeDecision::Allowed(p) => p,
        ScopeDecision::Unauthenticated(p) | ScopeDecision::InsufficientScope(p) => {
            return p.with_instance(instance).into_response();
        }
    };
    let Some(source) = &state.admin_tokens else {
        return source_unavailable("admin token", instance);
    };
    if let Some(key) = idempotency_key(&headers) {
        match state.idempotency.check("admin_tokens.create", key, &body) {
            Replay::Same(stored) => return replay_response(stored),
            Replay::Mismatch => {
                return Problem::idempotency_key_payload_mismatch()
                    .with_instance(instance)
                    .into_response();
            }
            Replay::Fresh => {}
        }
    }
    let request: CreateBody = if body.is_empty() {
        CreateBody::default()
    } else {
        match serde_json::from_slice(&body) {
            Ok(v) => v,
            Err(e) => {
                return Problem::validation_failed()
                    .with_detail(format!("invalid JSON body: {e}"))
                    .with_instance(instance)
                    .into_response();
            }
        }
    };
    let now = now_ms();
    let validated = match validate_create(request, now) {
        Ok(v) => v,
        Err(e) => return e.to_problem().with_instance(instance).into_response(),
    };
    let (new_token, secret) = mint(validated, &principal.id, now);
    let created = match source.create(new_token).await {
        Ok(t) => t,
        Err(e) => return e.to_problem().with_instance(instance).into_response(),
    };
    tracing::info!(
        token_id = %created.id,
        name = %created.name,
        scopes = ?created.scopes.iter().map(|s| s.as_str()).collect::<Vec<_>>(),
        minted_by = %principal.id,
        "admin token minted"
    );
    if let Err(resp) = record_mutation_with_status(
        &state,
        &principal,
        "admin_tokens.create",
        "admin_token.created",
        ResourceRef::new("admin_token", created.id.clone()),
        vec![
            AuditChange {
                pointer: "/name".into(),
                from: None,
                to: Some(json!(created.name)),
            },
            AuditChange {
                pointer: "/scopes".into(),
                from: None,
                to: Some(json!(created.scopes)),
            },
            AuditChange {
                pointer: "/expires_at".into(),
                from: None,
                to: Some(json!(created.expires_at)),
            },
        ],
        json!({ "id": created.id, "name": created.name, "scopes": created.scopes }),
        201,
    )
    .await
    {
        return resp;
    }
    let response = AdminTokenCreated {
        token: created,
        secret,
    };
    let response_body = serde_json::to_vec(&response).unwrap_or_default();
    if let Some(key) = idempotency_key(&headers) {
        state.idempotency.record(
            "admin_tokens.create",
            key,
            &body,
            StoredResponse {
                status: 201,
                content_type: "application/json".to_string(),
                body: response_body.clone(),
            },
        );
    }
    (
        StatusCode::CREATED,
        [(axum::http::header::CONTENT_TYPE, "application/json")],
        response_body,
    )
        .into_response()
}

/// `DELETE /api/v1/admin-tokens/{id}` (`admin:write`): revokes the token; `204`. The next
/// request with it is `401`.
pub(crate) async fn delete(
    State(state): State<AdminState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    let instance = instance_for(&id);
    let principal = match require_scope(
        state.verifier.as_ref(),
        authorization_header(&headers),
        Some(Scope::AdminWrite),
    )
    .await
    {
        ScopeDecision::Allowed(p) => p,
        ScopeDecision::Unauthenticated(p) | ScopeDecision::InsufficientScope(p) => {
            return p.with_instance(instance).into_response();
        }
    };
    let Some(source) = &state.admin_tokens else {
        return source_unavailable("admin token", &instance);
    };
    let before = match source.get(&id).await {
        Ok(Some(t)) => t,
        Ok(None) => return no_such_token(&id, &instance),
        Err(e) => return e.to_problem().with_instance(instance).into_response(),
    };
    match source.delete(&id).await {
        Ok(()) => {}
        Err(SourceError::NotFound) => return no_such_token(&id, &instance),
        Err(e) => return e.to_problem().with_instance(instance).into_response(),
    }
    tracing::info!(token_id = %id, name = %before.name, revoked_by = %principal.id, "admin token revoked");
    if let Err(resp) = record_mutation(
        &state,
        &principal,
        "admin_tokens.delete",
        "admin_token.revoked",
        ResourceRef::new("admin_token", id.clone()),
        vec![AuditChange {
            pointer: "/scopes".into(),
            from: Some(json!(before.scopes)),
            to: None,
        }],
        json!({ "id": id, "name": before.name }),
    )
    .await
    {
        return resp;
    }
    StatusCode::NO_CONTENT.into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::StaticVerifier;

    fn body(json: serde_json::Value) -> CreateBody {
        serde_json::from_value(json).expect("a create body")
    }

    #[test]
    fn a_token_is_prefixed_and_hashes_deterministically() {
        let token = generate_token();
        assert!(token.starts_with(TOKEN_PREFIX));
        assert_eq!(token.len(), TOKEN_PREFIX.len() + SECRET_LENGTH);
        assert_eq!(hash_token(&token), hash_token(&token));
        assert_ne!(hash_token(&token), hash_token(&generate_token()));
        assert_eq!(hash_token("").len(), 64);
    }

    #[test]
    fn scopes_default_to_a_full_administrators_and_are_normalized() {
        let v = validate_create(body(json!({"name": "ci"})), 0).expect("valid");
        assert_eq!(v.scopes, DEFAULT_SCOPES.to_vec());
        let v = validate_create(
            body(json!({"name": "bridges", "scopes": ["bridges:write", "bridges:read", "bridges:read"]})),
            0,
        )
        .expect("valid");
        assert_eq!(v.scopes, vec![Scope::BridgesRead, Scope::BridgesWrite]);
    }

    #[test]
    fn a_bad_request_names_its_field() {
        let field = |b: serde_json::Value| match validate_create(body(b), 1_000) {
            Err(SourceError::InvalidField { pointer, .. }) => pointer,
            other => panic!("expected a field error, got {other:?}"),
        };
        assert_eq!(field(json!({})), "/name");
        assert_eq!(field(json!({"name": "   "})), "/name");
        assert_eq!(field(json!({"name": "x", "scopes": []})), "/scopes");
        assert_eq!(field(json!({"name": "x", "scopes": ["root"]})), "/scopes");
        assert_eq!(
            field(json!({"name": "x", "expires_at": "1970-01-01T00:00:00Z"})),
            "/expires_at"
        );
        assert_eq!(
            field(json!({"name": "x", "expires_at": "soon"})),
            "/expires_at"
        );
    }

    fn legacy() -> Principal {
        Principal {
            kind: PrincipalKind::Legacy,
            id: "@ops:example.org".into(),
            display_name: None,
            scopes: vec![Scope::AdminRead, Scope::AdminWrite],
            token_id: None,
            expires_at: None,
            issued_by: None,
        }
    }

    #[tokio::test]
    async fn the_verifier_decides_admin_tokens_and_defers_the_rest() {
        let store = Arc::new(InMemoryAdminTokens::new());
        let fallback = Arc::new(StaticVerifier::new().with_token("syt_legacy", legacy()));
        let verifier = ScopedTokenVerifier::new(store.clone(), fallback);

        let validated = validate_create(
            body(json!({"name": "bridge team", "scopes": ["bridges:read"]})),
            0,
        )
        .expect("valid");
        let (new_token, secret) = mint(validated, "@ops:example.org", 0);
        let id = new_token.record.token.id.clone();
        store.create(new_token).await.expect("stored");

        let principal = verifier
            .verify(&secret)
            .await
            .expect("the minted token verifies");
        assert_eq!(principal.scopes, vec![Scope::BridgesRead]);
        assert_eq!(principal.id, id);
        assert_eq!(principal.display_name.as_deref(), Some("bridge team"));
        assert!(matches!(principal.kind, PrincipalKind::ServiceAccount));
        assert_eq!(principal.issued_by.as_deref(), Some(ISSUER));
        assert!(principal.has_scope(Scope::BridgesRead));
        assert!(!principal.has_scope(Scope::AdminRead));

        // The legacy path still works through the same verifier.
        let legacy = verifier
            .verify("syt_legacy")
            .await
            .expect("legacy verifies");
        assert!(matches!(legacy.kind, PrincipalKind::Legacy));

        // An admin-shaped token nobody minted is invalid here, never deferred.
        assert!(matches!(
            verifier.verify("hsa_nope").await,
            Err(AuthError::Invalid)
        ));

        // Revoked: the next request is refused.
        store.delete(&id).await.expect("deleted");
        assert!(matches!(
            verifier.verify(&secret).await,
            Err(AuthError::Invalid)
        ));
        assert!(matches!(
            store.delete(&id).await,
            Err(SourceError::NotFound)
        ));
    }

    #[tokio::test]
    async fn an_expired_token_is_expired_not_invalid() {
        let store = Arc::new(InMemoryAdminTokens::new());
        let verifier = ScopedTokenVerifier::new(store.clone(), Arc::new(StaticVerifier::new()));
        let validated = ValidatedCreate {
            name: "old".into(),
            scopes: vec![Scope::AdminRead],
            expires_at_ms: Some(1),
            expires_at: Some(format_ms(1)),
        };
        let (new_token, secret) = mint(validated, "@ops:example.org", 0);
        store.create(new_token).await.expect("stored");
        assert!(matches!(
            verifier.verify(&secret).await,
            Err(AuthError::Expired)
        ));
    }

    #[test]
    fn the_created_shape_carries_the_secret_as_token() {
        let created = AdminTokenCreated {
            token: AdminToken {
                id: "01J".into(),
                name: "n".into(),
                scopes: vec![Scope::AdminRead],
                created_at: "2026-10-02T00:00:00.000Z".into(),
                created_by: "@ops:example.org".into(),
                expires_at: None,
            },
            secret: "hsa_x".into(),
        };
        let value = serde_json::to_value(&created).expect("serializes");
        assert_eq!(value["token"], "hsa_x");
        assert_eq!(value["id"], "01J");
        assert_eq!(value["scopes"], json!(["admin:read"]));
        assert!(value["secret_hash"].is_null());
    }
}
