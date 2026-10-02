//! `/pushrules` in every form the spec names: the whole ruleset, one scope, one kind, one rule,
//! and one rule's `actions`/`enabled` sub-resources.
//!
//! # Paths and status codes
//!
//! The spec spells the whole-ruleset path with a trailing slash (`GET /pushrules/`), lists a
//! scope or kind with one too (`/pushrules/global/`, `/pushrules/global/{kind}/`), and names a
//! rule without one. Everything else under `/pushrules/` is a malformed request and answers
//! `400 M_UNRECOGNIZED`, as Synapse does and as Sytest's `61push/80torture.pl` checks: a missing
//! or unknown scope, a kind without its trailing slash, an empty rule ID, an unknown attribute.
//! `/pushrules` with no slash at all is not a `/pushrules` path and stays the router's `404`.
//!
//! The spec's paths are registered verbatim (`/pushrules/global/{kind}/{ruleId}`), so the spec
//! coverage tool matches them; the `{scope}` variants beside them exist to turn a wrong scope
//! into a `400` instead of a `404`. axum resolves the literal `global` ahead of the parameter.

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use hs_http::body::PermissiveJson;
use hs_http::error::{MatrixError, MatrixErrorCode};
use hs_kv::KvBackend;
use ruma::UserId;
use ruma::push::{Action, PushCondition};
use serde::Deserialize;
use serde_json::{Map, Value, json};

use crate::ruleset::{NewRule, RuleError, RuleKind, Ruleset};
use crate::state::{PushRequester, PushState};

/// `400 M_UNRECOGNIZED`: the shape Synapse gives a `/pushrules` path it cannot parse.
fn unrecognized(what: impl Into<String>) -> MatrixError {
    MatrixError::custom(StatusCode::BAD_REQUEST, MatrixErrorCode::Unrecognized, what)
}

fn invalid(what: impl Into<String>) -> MatrixError {
    MatrixError::custom(StatusCode::BAD_REQUEST, MatrixErrorCode::InvalidParam, what)
}

fn not_found(rule_id: &str) -> MatrixError {
    MatrixError::not_found(format!("no push rule {rule_id:?}"))
}

fn scope(raw: &str) -> Result<(), MatrixError> {
    if raw == "global" {
        Ok(())
    } else {
        Err(unrecognized(format!("unknown push rule scope {raw:?}")))
    }
}

fn kind(raw: &str) -> Result<RuleKind, MatrixError> {
    RuleKind::parse(raw).ok_or_else(|| unrecognized(format!("unknown push rule kind {raw:?}")))
}

fn rule_edit_error(e: RuleError) -> MatrixError {
    match e {
        RuleError::NotFound => MatrixError::not_found("no such push rule"),
        RuleError::ServerDefault => invalid(e.to_string()),
        other => invalid(other.to_string()),
    }
}

/// `GET /pushrules/`: the caller's whole ruleset, every scope.
pub async fn get_pushrules_all<B: KvBackend + 'static>(
    PushRequester(requester): PushRequester,
    State(state): State<PushState<B>>,
) -> Result<Json<Value>, MatrixError> {
    let ruleset = effective(&state, &requester.user_id).await?;
    Ok(Json(json!({ "global": ruleset.as_ref() })))
}

/// `GET /pushrules/global/`: the global scope's rules, by kind.
pub async fn get_pushrules_global<B: KvBackend + 'static>(
    PushRequester(requester): PushRequester,
    State(state): State<PushState<B>>,
) -> Result<Json<Value>, MatrixError> {
    let ruleset = effective(&state, &requester.user_id).await?;
    Ok(Json(
        serde_json::to_value(ruleset.as_ref()).unwrap_or(Value::Null),
    ))
}

/// `GET /pushrules/{scope}/`: one scope's rules, by kind.
pub async fn get_pushrules_scope<B: KvBackend + 'static>(
    PushRequester(requester): PushRequester,
    State(state): State<PushState<B>>,
    Path(raw_scope): Path<String>,
) -> Result<Json<Value>, MatrixError> {
    scope(&raw_scope)?;
    let ruleset = effective(&state, &requester.user_id).await?;
    Ok(Json(
        serde_json::to_value(ruleset.as_ref()).unwrap_or(Value::Null),
    ))
}

/// `GET /pushrules/{scope}/{kind}/`: one kind's rules, in priority order.
pub async fn get_pushrules_kind<B: KvBackend + 'static>(
    PushRequester(requester): PushRequester,
    State(state): State<PushState<B>>,
    Path((raw_scope, raw_kind)): Path<(String, String)>,
) -> Result<Json<Value>, MatrixError> {
    scope(&raw_scope)?;
    let kind = kind(&raw_kind)?;
    let ruleset = effective(&state, &requester.user_id).await?;
    Ok(Json(ruleset.kind_to_json(kind)))
}

/// `GET /pushrules/{scope}/{kind}/{ruleId}`: one rule.
pub async fn get_pushrule<B: KvBackend + 'static>(
    PushRequester(requester): PushRequester,
    State(state): State<PushState<B>>,
    Path((raw_scope, raw_kind, rule_id)): Path<(String, String, String)>,
) -> Result<Json<Value>, MatrixError> {
    scope(&raw_scope)?;
    let kind = kind(&raw_kind)?;
    let ruleset = effective(&state, &requester.user_id).await?;
    let rule = ruleset
        .get(kind, &rule_id)
        .ok_or_else(|| not_found(&rule_id))?;
    Ok(Json(rule.to_json()))
}

/// `GET /pushrules/{scope}/{kind}/{ruleId}/{attr}`: one attribute of a rule (`actions`,
/// `enabled`, or any other field the rule's JSON has), as `{attr: value}`.
pub async fn get_pushrule_attr<B: KvBackend + 'static>(
    PushRequester(requester): PushRequester,
    State(state): State<PushState<B>>,
    Path((raw_scope, raw_kind, rule_id, attr)): Path<(String, String, String, String)>,
) -> Result<Json<Value>, MatrixError> {
    scope(&raw_scope)?;
    let kind = kind(&raw_kind)?;
    let ruleset = effective(&state, &requester.user_id).await?;
    let rule = ruleset
        .get(kind, &rule_id)
        .ok_or_else(|| not_found(&rule_id))?;
    let json = rule.to_json();
    let value = json
        .get(&attr)
        .ok_or_else(|| unrecognized(format!("push rules have no attribute {attr:?}")))?;
    let mut out = Map::new();
    out.insert(attr, value.clone());
    Ok(Json(Value::Object(out)))
}

/// Any other `/pushrules/...` path: `400 M_UNRECOGNIZED`.
pub async fn malformed() -> Result<Json<Value>, MatrixError> {
    Err(unrecognized("malformed push rules path"))
}

/// The spec's literal `/pushrules/global/...` paths capture one path segment fewer than their
/// `{scope}` twins; these wrappers supply the scope and share the handlers above and below.
pub mod global {
    use super::*;

    const GLOBAL: &str = "global";

    /// `GET /pushrules/global/{kind}/{ruleId}`.
    pub async fn get_pushrule<B: KvBackend + 'static>(
        requester: PushRequester,
        state: State<PushState<B>>,
        Path((kind, rule_id)): Path<(String, String)>,
    ) -> Result<Json<Value>, MatrixError> {
        super::get_pushrule(requester, state, Path((GLOBAL.to_owned(), kind, rule_id))).await
    }

    async fn get_attr<B: KvBackend + 'static>(
        requester: PushRequester,
        state: State<PushState<B>>,
        kind: String,
        rule_id: String,
        attr: &str,
    ) -> Result<Json<Value>, MatrixError> {
        super::get_pushrule_attr(
            requester,
            state,
            Path((GLOBAL.to_owned(), kind, rule_id, attr.to_owned())),
        )
        .await
    }

    /// `GET /pushrules/global/{kind}/{ruleId}/actions`.
    pub async fn get_actions<B: KvBackend + 'static>(
        requester: PushRequester,
        state: State<PushState<B>>,
        Path((kind, rule_id)): Path<(String, String)>,
    ) -> Result<Json<Value>, MatrixError> {
        get_attr(requester, state, kind, rule_id, "actions").await
    }

    /// `GET /pushrules/global/{kind}/{ruleId}/enabled`.
    pub async fn get_enabled<B: KvBackend + 'static>(
        requester: PushRequester,
        state: State<PushState<B>>,
        Path((kind, rule_id)): Path<(String, String)>,
    ) -> Result<Json<Value>, MatrixError> {
        get_attr(requester, state, kind, rule_id, "enabled").await
    }

    /// `PUT /pushrules/global/{kind}/{ruleId}`.
    pub async fn put_pushrule<B: KvBackend + 'static>(
        requester: PushRequester,
        state: State<PushState<B>>,
        Path((kind, rule_id)): Path<(String, String)>,
        query: Query<BeforeAfter>,
        body: PermissiveJson<SetRuleBody>,
    ) -> Result<Json<Value>, MatrixError> {
        super::put_pushrule(
            requester,
            state,
            Path((GLOBAL.to_owned(), kind, rule_id)),
            query,
            body,
        )
        .await
    }

    /// `DELETE /pushrules/global/{kind}/{ruleId}`.
    pub async fn delete_pushrule<B: KvBackend + 'static>(
        requester: PushRequester,
        state: State<PushState<B>>,
        Path((kind, rule_id)): Path<(String, String)>,
    ) -> Result<Json<Value>, MatrixError> {
        super::delete_pushrule(requester, state, Path((GLOBAL.to_owned(), kind, rule_id))).await
    }

    async fn put_attr<B: KvBackend + 'static>(
        requester: PushRequester,
        state: State<PushState<B>>,
        kind: String,
        rule_id: String,
        attr: &str,
        body: PermissiveJson<Value>,
    ) -> Result<Json<Value>, MatrixError> {
        super::put_pushrule_attr(
            requester,
            state,
            Path((GLOBAL.to_owned(), kind, rule_id, attr.to_owned())),
            body,
        )
        .await
    }

    /// `PUT /pushrules/global/{kind}/{ruleId}/actions`.
    pub async fn put_actions<B: KvBackend + 'static>(
        requester: PushRequester,
        state: State<PushState<B>>,
        Path((kind, rule_id)): Path<(String, String)>,
        body: PermissiveJson<Value>,
    ) -> Result<Json<Value>, MatrixError> {
        put_attr(requester, state, kind, rule_id, "actions", body).await
    }

    /// `PUT /pushrules/global/{kind}/{ruleId}/enabled`.
    pub async fn put_enabled<B: KvBackend + 'static>(
        requester: PushRequester,
        state: State<PushState<B>>,
        Path((kind, rule_id)): Path<(String, String)>,
        body: PermissiveJson<Value>,
    ) -> Result<Json<Value>, MatrixError> {
        put_attr(requester, state, kind, rule_id, "enabled", body).await
    }
}

/// The `before`/`after` query parameters that position a newly inserted rule.
#[derive(Deserialize)]
pub struct BeforeAfter {
    before: Option<String>,
    after: Option<String>,
}

/// `PUT /pushrules/global/{kind}/{ruleId}`'s request body. Every field is validated by hand
/// below rather than through serde defaults, because the spec's error cases are about which
/// keys are present: an override rule without `conditions` or a content rule without
/// `pattern` is a `400`, not a rule with an empty default.
#[derive(Deserialize)]
pub struct SetRuleBody {
    actions: Option<Vec<Value>>,
    conditions: Option<Vec<Value>>,
    pattern: Option<String>,
}

/// The actions the spec allows: `notify`, `dont_notify`, `coalesce`, and `set_tweak` objects.
/// Anything else is a `400`, which is also how a client finds out that an unstable action such
/// as MSC2625's `mark_unread` is not supported here.
fn parse_actions(raw: Vec<Value>) -> Result<Vec<Action>, MatrixError> {
    raw.into_iter()
        .map(|value| {
            let ok = match &value {
                Value::String(s) => matches!(s.as_str(), "notify" | "dont_notify" | "coalesce"),
                Value::Object(o) => o.get("set_tweak").is_some_and(Value::is_string),
                _ => false,
            };
            if !ok {
                return Err(invalid(format!("invalid push rule action {value}")));
            }
            serde_json::from_value::<Action>(value)
                .map_err(|e| invalid(format!("invalid push rule action: {e}")))
        })
        .collect()
}

fn parse_conditions(raw: Vec<Value>) -> Result<Vec<PushCondition>, MatrixError> {
    raw.into_iter()
        .map(|value| {
            if value.get("kind").is_none() {
                return Err(invalid("push rule condition without a kind"));
            }
            serde_json::from_value::<PushCondition>(value)
                .map_err(|e| invalid(format!("invalid push rule condition: {e}")))
        })
        .collect()
}

/// `PUT /pushrules/{scope}/{kind}/{ruleId}`: create or update a user-defined rule.
pub async fn put_pushrule<B: KvBackend + 'static>(
    PushRequester(requester): PushRequester,
    State(state): State<PushState<B>>,
    Path((raw_scope, raw_kind, rule_id)): Path<(String, String, String)>,
    Query(ba): Query<BeforeAfter>,
    PermissiveJson(body): PermissiveJson<SetRuleBody>,
) -> Result<Json<Value>, MatrixError> {
    scope(&raw_scope)?;
    let kind = kind(&raw_kind)?;
    if rule_id.starts_with('.') {
        return Err(invalid(
            "rule IDs starting with '.' are reserved for server-default rules",
        ));
    }
    if rule_id.contains('/') || rule_id.contains('\\') {
        return Err(invalid("rule IDs may not contain slashes"));
    }
    let actions = parse_actions(
        body.actions
            .ok_or_else(|| MatrixError::missing_param("actions"))?,
    )?;
    let conditions = match kind {
        RuleKind::Override | RuleKind::Underride => parse_conditions(
            body.conditions
                .ok_or_else(|| MatrixError::missing_param("conditions"))?,
        )?,
        _ => Vec::new(),
    };
    if kind == RuleKind::Content && body.pattern.is_none() {
        return Err(MatrixError::missing_param("pattern"));
    }
    let new_rule = NewRule {
        kind,
        rule_id,
        actions,
        conditions,
        pattern: body.pattern,
    };

    let mut ruleset = (*effective(&state, &requester.user_id).await?).clone();
    ruleset
        .insert(new_rule, ba.after.as_deref(), ba.before.as_deref())
        .map_err(rule_edit_error)?;
    save(&state, &requester.user_id, &ruleset).await?;
    Ok(Json(json!({})))
}

/// `DELETE /pushrules/{scope}/{kind}/{ruleId}`.
pub async fn delete_pushrule<B: KvBackend + 'static>(
    PushRequester(requester): PushRequester,
    State(state): State<PushState<B>>,
    Path((raw_scope, raw_kind, rule_id)): Path<(String, String, String)>,
) -> Result<Json<Value>, MatrixError> {
    scope(&raw_scope)?;
    let kind = kind(&raw_kind)?;
    let mut ruleset = (*effective(&state, &requester.user_id).await?).clone();
    ruleset
        .remove(kind, rule_id.as_str())
        .map_err(|_| not_found(&rule_id))?;
    save(&state, &requester.user_id, &ruleset).await?;
    Ok(Json(json!({})))
}

/// `PUT /pushrules/{scope}/{kind}/{ruleId}/{attr}`: the `actions` or `enabled` of a rule. Any
/// other attribute is a `400`.
pub async fn put_pushrule_attr<B: KvBackend + 'static>(
    PushRequester(requester): PushRequester,
    State(state): State<PushState<B>>,
    Path((raw_scope, raw_kind, rule_id, attr)): Path<(String, String, String, String)>,
    PermissiveJson(body): PermissiveJson<Value>,
) -> Result<Json<Value>, MatrixError> {
    scope(&raw_scope)?;
    let kind = kind(&raw_kind)?;
    let mut ruleset = (*effective(&state, &requester.user_id).await?).clone();
    match attr.as_str() {
        "actions" => {
            let raw = body
                .get("actions")
                .and_then(Value::as_array)
                .cloned()
                .ok_or_else(|| MatrixError::missing_param("actions"))?;
            let actions = parse_actions(raw)?;
            ruleset
                .set_actions(kind, rule_id.as_str(), actions)
                .map_err(rule_edit_error)?;
        }
        "enabled" => {
            let enabled = body
                .get("enabled")
                .and_then(Value::as_bool)
                .ok_or_else(|| invalid("enabled must be a boolean"))?;
            ruleset
                .set_enabled(kind, rule_id.as_str(), enabled)
                .map_err(rule_edit_error)?;
        }
        other => {
            return Err(unrecognized(format!(
                "push rule attribute {other:?} cannot be set"
            )));
        }
    }
    save(&state, &requester.user_id, &ruleset).await?;
    Ok(Json(json!({})))
}

async fn effective<B: KvBackend + 'static>(
    state: &PushState<B>,
    user_id: &UserId,
) -> Result<std::sync::Arc<Ruleset>, MatrixError> {
    state
        .rulesets
        .effective_ruleset(user_id)
        .await
        .map_err(store_err)
}

async fn save<B: KvBackend + 'static>(
    state: &PushState<B>,
    user_id: &UserId,
    ruleset: &Ruleset,
) -> Result<(), MatrixError> {
    state
        .rulesets
        .set_ruleset(user_id, ruleset)
        .await
        .map_err(store_err)?;
    tracing::debug!(user = %user_id, "push rules changed");
    Ok(())
}

fn store_err(e: crate::error::StoreError) -> MatrixError {
    MatrixError::custom(
        StatusCode::INTERNAL_SERVER_ERROR,
        MatrixErrorCode::Unknown,
        e.to_string(),
    )
}
