//! The `registration_tokens.*` operations: tokens that let someone register an account, which is
//! how an operator invites a person by link while open registration stays off.
//!
//! The handlers here own the admin API's half: scope checks, request validation (the token's
//! alphabet and length, generating one when none is given, turning `expires_at` into a time),
//! idempotency, and the audit entry and event every mutation writes. The tokens themselves --
//! durable storage, and the `pending`/`completed` counts that client-server registration moves --
//! live behind [`RegistrationTokenSource`], which `hs-auth` implements over the same store its
//! `m.login.registration_token` stage checks.

use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use hs_http::Problem;
use rand::Rng;
use rand::distr::Alphanumeric;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::auth::{ScopeDecision, require_scope};
use crate::idempotency::{Replay, StoredResponse};
use crate::model::{AuditChange, Page, ResourceRef, Scope};
use crate::router::{
    AdminState, authorization_header, idempotency_key, record_mutation,
    record_mutation_with_status, replay_response, source_unavailable,
};
use crate::sources::SourceError;

/// The longest token the API accepts, generated or given. Synapse's limit, which clients that
/// build invite links already assume.
pub const MAX_TOKEN_LENGTH: usize = 64;

/// How long a generated token is when the request does not say.
pub const DEFAULT_GENERATED_LENGTH: usize = 16;

/// The OpenAPI `RegistrationToken` schema.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdminRegistrationToken {
    /// The token itself, as a person types it or an invite link carries it.
    pub token: String,
    /// Whether the token would admit a new registration right now.
    pub valid: bool,
    /// How many accounts the token may create; `None` for no limit.
    pub uses_allowed: Option<u64>,
    /// Registrations that have presented the token and not finished yet.
    pub pending: u64,
    /// Accounts created with the token.
    pub completed: u64,
    /// When the token stops working (RFC 3339), or `None` for never.
    pub expires_at: Option<String>,
    /// When the token was created (RFC 3339).
    pub created_at: String,
}

/// A token to create, after the handler has validated the request and chosen the token string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewRegistrationToken {
    /// The token, already checked against [`is_valid_token`].
    pub token: String,
    /// How many accounts it may create; `None` for no limit.
    pub uses_allowed: Option<u64>,
    /// When it stops working, in milliseconds since the Unix epoch; `None` for never.
    pub expires_at_ms: Option<i64>,
}

/// A change to a token's limits. The outer `Option` says whether the request named the field;
/// the inner one is its new value, where `None` removes the limit.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RegistrationTokenPatch {
    /// A new `uses_allowed`, if the request named it.
    pub uses_allowed: Option<Option<u64>>,
    /// A new expiry in milliseconds since the Unix epoch, if the request named it.
    pub expires_at_ms: Option<Option<i64>>,
}

/// Where the registration tokens live. Implemented by `hs-auth` over the store that client-server
/// registration reads, so a token created here is the token `/register` accepts, and the counts
/// read here are the ones registration moves.
#[async_trait::async_trait]
pub trait RegistrationTokenSource: Send + Sync + 'static {
    /// Every token, oldest first.
    async fn list(&self) -> Result<Vec<AdminRegistrationToken>, SourceError>;
    /// One token, or `None` if there is no such token.
    async fn get(&self, token: &str) -> Result<Option<AdminRegistrationToken>, SourceError>;
    /// Creates a token. [`SourceError::Conflict`] if one with the same string exists.
    async fn create(
        &self,
        token: NewRegistrationToken,
    ) -> Result<AdminRegistrationToken, SourceError>;
    /// Changes a token's limits. [`SourceError::NotFound`] if there is no such token.
    async fn update(
        &self,
        token: &str,
        patch: RegistrationTokenPatch,
    ) -> Result<AdminRegistrationToken, SourceError>;
    /// Deletes a token. [`SourceError::NotFound`] if there is no such token. Registrations that
    /// already presented it and have not finished can no longer finish with it.
    async fn delete(&self, token: &str) -> Result<(), SourceError>;
}

/// Whether `token` is something the API will store: 1 to [`MAX_TOKEN_LENGTH`] characters from
/// `A-Z a-z 0-9 . _ ~ -`, the unreserved characters of a URL, so a token always survives being
/// put in an invite link unescaped.
#[must_use]
pub fn is_valid_token(token: &str) -> bool {
    !token.is_empty()
        && token.len() <= MAX_TOKEN_LENGTH
        && token
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'~' | b'-'))
}

/// A fresh random token of `length` letters and digits.
#[must_use]
pub fn generate_token(length: usize) -> String {
    let mut rng = rand::rng();
    (0..length)
        .map(|_| char::from(rng.sample(Alphanumeric)))
        .collect()
}

/// Whether a token with these numbers admits a registration at `now_ms`: not expired, and fewer
/// registrations finished or under way than it allows. The one definition of `valid`, shared by
/// every implementation of [`RegistrationTokenSource`] and by `/register` itself.
#[must_use]
pub fn token_is_usable(
    uses_allowed: Option<u64>,
    pending: u64,
    completed: u64,
    expires_at_ms: Option<i64>,
    now_ms: i64,
) -> bool {
    let unexpired = expires_at_ms.is_none_or(|at| at > now_ms);
    let has_room = uses_allowed.is_none_or(|allowed| pending.saturating_add(completed) < allowed);
    unexpired && has_room
}

/// Milliseconds since the Unix epoch, now.
#[must_use]
pub fn now_ms() -> i64 {
    let nanos = time::OffsetDateTime::now_utc().unix_timestamp_nanos();
    i64::try_from(nanos / 1_000_000).unwrap_or(i64::MAX)
}

/// The body of `POST /registration-tokens` as it arrives.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct CreateBody {
    token: Option<String>,
    uses_allowed: Option<i64>,
    expires_at: Option<String>,
    length: Option<i64>,
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

fn parse_expiry(pointer: &'static str, value: &str) -> Result<i64, SourceError> {
    let at = hs_http::time::parse_rfc3339(value).map_err(|e| {
        invalid_field(
            pointer,
            format!("expires_at must be an RFC 3339 date-time: {e}"),
        )
    })?;
    i64::try_from(at.unix_timestamp_nanos() / 1_000_000)
        .map_err(|_| invalid_field(pointer, "expires_at is out of range"))
}

fn parse_uses(pointer: &'static str, value: i64) -> Result<u64, SourceError> {
    u64::try_from(value).map_err(|_| invalid_field(pointer, "uses_allowed must be 0 or more"))
}

/// Turns a create request into the token to store, or says which field is wrong.
fn validate_create(body: CreateBody, now_ms: i64) -> Result<NewRegistrationToken, SourceError> {
    // `length` only says how long a generated token is; alongside a given token it means
    // nothing and is ignored (an interface may well send its default either way).
    let token = match (body.token, body.length) {
        (Some(token), _) => {
            if !is_valid_token(&token) {
                return Err(invalid_field(
                    "/token",
                    format!(
                        "a token is 1 to {MAX_TOKEN_LENGTH} characters from A-Z, a-z, 0-9, \
                         '.', '_', '~' and '-'"
                    ),
                ));
            }
            token
        }
        (None, length) => {
            let length = length.unwrap_or(DEFAULT_GENERATED_LENGTH as i64);
            let length = usize::try_from(length)
                .ok()
                .filter(|l| (1..=MAX_TOKEN_LENGTH).contains(l))
                .ok_or_else(|| {
                    invalid_field(
                        "/length",
                        format!("length must be between 1 and {MAX_TOKEN_LENGTH}"),
                    )
                })?;
            generate_token(length)
        }
    };
    let uses_allowed = body
        .uses_allowed
        .map(|u| parse_uses("/uses_allowed", u))
        .transpose()?;
    let expires_at_ms = body
        .expires_at
        .as_deref()
        .map(|e| parse_expiry("/expires_at", e))
        .transpose()?;
    if let Some(at) = expires_at_ms
        && at <= now_ms
    {
        return Err(invalid_field(
            "/expires_at",
            "expires_at is in the past; a new token must be usable",
        ));
    }
    Ok(NewRegistrationToken {
        token,
        uses_allowed,
        expires_at_ms,
    })
}

/// Turns a `PATCH` body into a patch, telling "absent" from "null" for both fields.
fn validate_patch(body: &Value) -> Result<RegistrationTokenPatch, SourceError> {
    let Some(object) = body.as_object() else {
        return Err(SourceError::Invalid(
            "the body must be a JSON object".into(),
        ));
    };
    let mut patch = RegistrationTokenPatch::default();
    for (key, value) in object {
        match key.as_str() {
            "uses_allowed" => {
                patch.uses_allowed = Some(match value {
                    Value::Null => None,
                    other => Some(parse_uses(
                        "/uses_allowed",
                        other.as_i64().ok_or_else(|| {
                            invalid_field(
                                "/uses_allowed",
                                "uses_allowed must be an integer or null",
                            )
                        })?,
                    )?),
                });
            }
            "expires_at" => {
                patch.expires_at_ms = Some(match value {
                    Value::Null => None,
                    Value::String(s) => Some(parse_expiry("/expires_at", s)?),
                    _ => {
                        return Err(invalid_field(
                            "/expires_at",
                            "expires_at must be a date-time string or null",
                        ));
                    }
                });
            }
            other => {
                return Err(SourceError::Invalid(format!(
                    "unknown field {other:?}; only uses_allowed and expires_at can change"
                )));
            }
        }
    }
    Ok(patch)
}

fn no_such_token(token: &str, instance: &str) -> Response {
    Problem::not_found()
        .with_detail(format!("no such registration token: {token}"))
        .with_instance(instance.to_owned())
        .into_response()
}

fn instance_for(token: &str) -> String {
    format!("/api/v1/registration-tokens/{token}")
}

/// `GET /api/v1/registration-tokens` (`admin:read`).
pub(crate) async fn list(
    State(state): State<AdminState>,
    headers: HeaderMap,
    Query(query): Query<ListQuery>,
) -> Response {
    let instance = "/api/v1/registration-tokens";
    match require_scope(
        state.verifier.as_ref(),
        authorization_header(&headers),
        Some(Scope::AdminRead),
    )
    .await
    {
        ScopeDecision::Allowed(_) => {
            let Some(source) = &state.registration_tokens else {
                return source_unavailable("registration token", instance);
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

/// `GET /api/v1/registration-tokens/{token}` (`admin:read`).
pub(crate) async fn get(
    State(state): State<AdminState>,
    headers: HeaderMap,
    Path(token): Path<String>,
) -> Response {
    let instance = instance_for(&token);
    match require_scope(
        state.verifier.as_ref(),
        authorization_header(&headers),
        Some(Scope::AdminRead),
    )
    .await
    {
        ScopeDecision::Allowed(_) => {
            let Some(source) = &state.registration_tokens else {
                return source_unavailable("registration token", &instance);
            };
            match source.get(&token).await {
                Ok(Some(t)) => axum::Json(t).into_response(),
                Ok(None) => no_such_token(&token, &instance),
                Err(e) => e.to_problem().with_instance(instance).into_response(),
            }
        }
        ScopeDecision::Unauthenticated(p) | ScopeDecision::InsufficientScope(p) => {
            p.with_instance(instance).into_response()
        }
    }
}

/// `POST /api/v1/registration-tokens` (`admin:write`): `201` with the token. Honors
/// `Idempotency-Key`, so a retried create does not make a second token.
pub(crate) async fn create(
    State(state): State<AdminState>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    let instance = "/api/v1/registration-tokens";
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
    let Some(source) = &state.registration_tokens else {
        return source_unavailable("registration token", instance);
    };
    if let Some(key) = idempotency_key(&headers) {
        match state
            .idempotency
            .check("registration_tokens.create", key, &body)
        {
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
    let new_token = match validate_create(request, now_ms()) {
        Ok(t) => t,
        Err(e) => return e.to_problem().with_instance(instance).into_response(),
    };
    let created = match source.create(new_token).await {
        Ok(t) => t,
        Err(e) => return e.to_problem().with_instance(instance).into_response(),
    };
    if let Err(resp) = record_mutation_with_status(
        &state,
        &principal,
        "registration_tokens.create",
        "registration_token.created",
        ResourceRef::new("registration_token", created.token.clone()),
        vec![
            AuditChange {
                pointer: "/uses_allowed".into(),
                from: None,
                to: Some(json!(created.uses_allowed)),
            },
            AuditChange {
                pointer: "/expires_at".into(),
                from: None,
                to: Some(json!(created.expires_at)),
            },
        ],
        json!({ "token": created.token }),
        201,
    )
    .await
    {
        return resp;
    }
    let response_body = serde_json::to_vec(&created).unwrap_or_default();
    if let Some(key) = idempotency_key(&headers) {
        state.idempotency.record(
            "registration_tokens.create",
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

/// `PATCH /api/v1/registration-tokens/{token}` (`admin:write`): change `uses_allowed` and/or
/// `expires_at`; `null` removes the limit. Setting `expires_at` to now is how a token is expired.
pub(crate) async fn update(
    State(state): State<AdminState>,
    headers: HeaderMap,
    Path(token): Path<String>,
    body: axum::body::Bytes,
) -> Response {
    let instance = instance_for(&token);
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
    let Some(source) = &state.registration_tokens else {
        return source_unavailable("registration token", &instance);
    };
    let body: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => {
            return Problem::validation_failed()
                .with_detail(format!("invalid JSON body: {e}"))
                .with_instance(instance)
                .into_response();
        }
    };
    let patch = match validate_patch(&body) {
        Ok(p) => p,
        Err(e) => return e.to_problem().with_instance(instance).into_response(),
    };
    let before = match source.get(&token).await {
        Ok(Some(t)) => t,
        Ok(None) => return no_such_token(&token, &instance),
        Err(e) => return e.to_problem().with_instance(instance).into_response(),
    };
    let updated = match source.update(&token, patch.clone()).await {
        Ok(t) => t,
        Err(SourceError::NotFound) => return no_such_token(&token, &instance),
        Err(e) => return e.to_problem().with_instance(instance).into_response(),
    };
    let mut changes = Vec::new();
    if patch.uses_allowed.is_some() {
        changes.push(AuditChange {
            pointer: "/uses_allowed".into(),
            from: Some(json!(before.uses_allowed)),
            to: Some(json!(updated.uses_allowed)),
        });
    }
    if patch.expires_at_ms.is_some() {
        changes.push(AuditChange {
            pointer: "/expires_at".into(),
            from: Some(json!(before.expires_at)),
            to: Some(json!(updated.expires_at)),
        });
    }
    if let Err(resp) = record_mutation(
        &state,
        &principal,
        "registration_tokens.update",
        "registration_token.updated",
        ResourceRef::new("registration_token", token.clone()),
        changes,
        json!({ "token": token }),
    )
    .await
    {
        return resp;
    }
    axum::Json(updated).into_response()
}

/// `DELETE /api/v1/registration-tokens/{token}` (`admin:write`): `204`.
pub(crate) async fn delete(
    State(state): State<AdminState>,
    headers: HeaderMap,
    Path(token): Path<String>,
) -> Response {
    let instance = instance_for(&token);
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
    let Some(source) = &state.registration_tokens else {
        return source_unavailable("registration token", &instance);
    };
    match source.delete(&token).await {
        Ok(()) => {}
        Err(SourceError::NotFound) => return no_such_token(&token, &instance),
        Err(e) => return e.to_problem().with_instance(instance).into_response(),
    }
    if let Err(resp) = record_mutation(
        &state,
        &principal,
        "registration_tokens.delete",
        "registration_token.deleted",
        ResourceRef::new("registration_token", token.clone()),
        Vec::new(),
        json!({ "token": token }),
    )
    .await
    {
        return resp;
    }
    StatusCode::NO_CONTENT.into_response()
}

/// An in-memory [`RegistrationTokenSource`], for this crate's tests and the ones of crates that
/// build an [`AdminState`] without a real store. Counts never move here: nothing registers
/// against it.
#[derive(Default)]
pub struct InMemoryRegistrationTokens {
    rows: std::sync::Mutex<Vec<InMemoryRow>>,
}

#[derive(Clone)]
struct InMemoryRow {
    token: String,
    uses_allowed: Option<u64>,
    pending: u64,
    completed: u64,
    expires_at_ms: Option<i64>,
    created_at_ms: i64,
}

impl InMemoryRow {
    fn view(&self) -> AdminRegistrationToken {
        AdminRegistrationToken {
            token: self.token.clone(),
            valid: token_is_usable(
                self.uses_allowed,
                self.pending,
                self.completed,
                self.expires_at_ms,
                now_ms(),
            ),
            uses_allowed: self.uses_allowed,
            pending: self.pending,
            completed: self.completed,
            expires_at: self.expires_at_ms.map(hs_http::time::rfc3339_from_millis),
            created_at: hs_http::time::rfc3339_from_millis(self.created_at_ms),
        }
    }
}

impl InMemoryRegistrationTokens {
    /// An empty set of tokens.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets a token's counts, as a registration would have moved them. For tests.
    pub fn set_counts(&self, token: &str, pending: u64, completed: u64) {
        let mut rows = self
            .rows
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(row) = rows.iter_mut().find(|r| r.token == token) {
            row.pending = pending;
            row.completed = completed;
        }
    }

    fn rows(&self) -> std::sync::MutexGuard<'_, Vec<InMemoryRow>> {
        self.rows
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

#[async_trait::async_trait]
impl RegistrationTokenSource for InMemoryRegistrationTokens {
    async fn list(&self) -> Result<Vec<AdminRegistrationToken>, SourceError> {
        Ok(self.rows().iter().map(InMemoryRow::view).collect())
    }

    async fn get(&self, token: &str) -> Result<Option<AdminRegistrationToken>, SourceError> {
        Ok(self
            .rows()
            .iter()
            .find(|r| r.token == token)
            .map(InMemoryRow::view))
    }

    async fn create(
        &self,
        token: NewRegistrationToken,
    ) -> Result<AdminRegistrationToken, SourceError> {
        let mut rows = self.rows();
        if rows.iter().any(|r| r.token == token.token) {
            return Err(SourceError::Conflict(format!(
                "a registration token {:?} already exists",
                token.token
            )));
        }
        let row = InMemoryRow {
            token: token.token,
            uses_allowed: token.uses_allowed,
            pending: 0,
            completed: 0,
            expires_at_ms: token.expires_at_ms,
            created_at_ms: now_ms(),
        };
        let view = row.view();
        rows.push(row);
        Ok(view)
    }

    async fn update(
        &self,
        token: &str,
        patch: RegistrationTokenPatch,
    ) -> Result<AdminRegistrationToken, SourceError> {
        let mut rows = self.rows();
        let row = rows
            .iter_mut()
            .find(|r| r.token == token)
            .ok_or(SourceError::NotFound)?;
        if let Some(uses) = patch.uses_allowed {
            row.uses_allowed = uses;
        }
        if let Some(at) = patch.expires_at_ms {
            row.expires_at_ms = at;
        }
        Ok(row.view())
    }

    async fn delete(&self, token: &str) -> Result<(), SourceError> {
        let mut rows = self.rows();
        let before = rows.len();
        rows.retain(|r| r.token != token);
        if rows.len() == before {
            return Err(SourceError::NotFound);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_token_alphabet_is_the_unreserved_url_characters() {
        assert!(is_valid_token("abc-DEF_123.~"));
        assert!(!is_valid_token(""));
        assert!(!is_valid_token("has space"));
        assert!(!is_valid_token("slash/"));
        assert!(!is_valid_token("é"));
        assert!(is_valid_token(&"a".repeat(64)));
        assert!(!is_valid_token(&"a".repeat(65)));
    }

    #[test]
    fn generated_tokens_have_the_requested_length_and_are_valid() {
        for len in [1, 16, 64] {
            let t = generate_token(len);
            assert_eq!(t.len(), len);
            assert!(is_valid_token(&t));
        }
    }

    #[test]
    fn usable_means_unexpired_and_below_the_limit_counting_pending() {
        assert!(token_is_usable(None, 0, 0, None, 10));
        assert!(token_is_usable(Some(2), 1, 0, None, 10));
        assert!(!token_is_usable(Some(2), 1, 1, None, 10));
        assert!(!token_is_usable(Some(0), 0, 0, None, 10));
        assert!(token_is_usable(None, 0, 0, Some(11), 10));
        assert!(!token_is_usable(None, 0, 0, Some(10), 10));
    }

    #[test]
    fn create_generates_by_default_and_ignores_a_length_beside_a_token() {
        let t = validate_create(CreateBody::default(), 0).unwrap();
        assert_eq!(t.token.len(), DEFAULT_GENERATED_LENGTH);
        let t = validate_create(
            CreateBody {
                token: Some("abc".into()),
                length: Some(16),
                ..CreateBody::default()
            },
            0,
        )
        .unwrap();
        assert_eq!(t.token, "abc");
    }

    #[test]
    fn create_refuses_bad_tokens_negative_uses_and_past_expiry() {
        let bad_token = validate_create(
            CreateBody {
                token: Some("no spaces".into()),
                ..CreateBody::default()
            },
            0,
        );
        assert!(matches!(
            bad_token,
            Err(SourceError::InvalidField {
                pointer: "/token",
                ..
            })
        ));
        let negative = validate_create(
            CreateBody {
                uses_allowed: Some(-1),
                ..CreateBody::default()
            },
            0,
        );
        assert!(matches!(
            negative,
            Err(SourceError::InvalidField {
                pointer: "/uses_allowed",
                ..
            })
        ));
        let past = validate_create(
            CreateBody {
                expires_at: Some("2020-01-01T00:00:00.000Z".into()),
                ..CreateBody::default()
            },
            now_ms(),
        );
        assert!(matches!(
            past,
            Err(SourceError::InvalidField {
                pointer: "/expires_at",
                ..
            })
        ));
        let zero_length = validate_create(
            CreateBody {
                length: Some(0),
                ..CreateBody::default()
            },
            0,
        );
        assert!(matches!(
            zero_length,
            Err(SourceError::InvalidField {
                pointer: "/length",
                ..
            })
        ));
    }

    #[test]
    fn patch_tells_absent_from_null() {
        let p = validate_patch(&json!({"uses_allowed": null})).unwrap();
        assert_eq!(p.uses_allowed, Some(None));
        assert_eq!(p.expires_at_ms, None);
        let p =
            validate_patch(&json!({"expires_at": "1970-01-01T00:00:01.000Z", "uses_allowed": 3}))
                .unwrap();
        assert_eq!(p.expires_at_ms, Some(Some(1000)));
        assert_eq!(p.uses_allowed, Some(Some(3)));
        assert!(validate_patch(&json!({"token": "x"})).is_err());
        assert!(validate_patch(&json!([])).is_err());
    }
}
