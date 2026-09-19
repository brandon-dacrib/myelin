//! `/pushrules` in every form the brief names: the whole ruleset, one rule, and one rule's
//! `actions`/`enabled` sub-resources.

use axum::Json;
use axum::extract::{Path, Query, State};
use hs_kv::KvBackend;
use ruma::UserId;
use ruma::push::{
    Action, NewConditionalPushRule, NewPatternedPushRule, NewPushRule, NewSimplePushRule,
    PushCondition, RuleKind,
};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::state::{PushRequester, PushState};

fn rule_kind(raw: &str) -> Result<RuleKind, hs_http::error::MatrixError> {
    let kind = RuleKind::from(raw);
    if matches!(kind, RuleKind::_Custom(_)) {
        return Err(hs_http::error::MatrixError::custom(
            axum::http::StatusCode::BAD_REQUEST,
            hs_http::error::MatrixErrorCode::InvalidParam,
            format!("unknown push rule kind {raw:?}"),
        ));
    }
    Ok(kind)
}

fn unknown_kind(kind: &RuleKind) -> hs_http::error::MatrixError {
    hs_http::error::MatrixError::custom(
        axum::http::StatusCode::BAD_REQUEST,
        hs_http::error::MatrixErrorCode::InvalidParam,
        format!("unsupported push rule kind {}", kind.as_str()),
    )
}

fn not_found(rule_id: &str) -> hs_http::error::MatrixError {
    hs_http::error::MatrixError::not_found(format!("no push rule {rule_id:?}"))
}

/// `GET /pushrules/`: the caller's whole global ruleset.
pub async fn get_pushrules_all<B: KvBackend + 'static>(
    PushRequester(requester): PushRequester,
    State(state): State<PushState<B>>,
) -> Result<Json<Value>, hs_http::error::MatrixError> {
    let ruleset = effective(&state, &requester.user_id).await?;
    Ok(Json(json!({ "global": ruleset.as_ref() })))
}

/// `GET /pushrules/global/{kind}/{ruleId}`: one rule.
pub async fn get_pushrule<B: KvBackend + 'static>(
    PushRequester(requester): PushRequester,
    State(state): State<PushState<B>>,
    Path((kind, rule_id)): Path<(String, String)>,
) -> Result<Json<Value>, hs_http::error::MatrixError> {
    let kind = rule_kind(&kind)?;
    let ruleset = effective(&state, &requester.user_id).await?;
    let rule = ruleset
        .get(kind, &rule_id)
        .ok_or_else(|| not_found(&rule_id))?;
    let wire: ruma::api::client::push::PushRule = rule.to_owned().into();
    Ok(Json(serde_json::to_value(wire).unwrap()))
}

/// `GET /pushrules/global/{kind}/{ruleId}/actions`.
pub async fn get_pushrule_actions<B: KvBackend + 'static>(
    PushRequester(requester): PushRequester,
    State(state): State<PushState<B>>,
    Path((kind, rule_id)): Path<(String, String)>,
) -> Result<Json<Value>, hs_http::error::MatrixError> {
    let kind = rule_kind(&kind)?;
    let ruleset = effective(&state, &requester.user_id).await?;
    let rule = ruleset
        .get(kind, &rule_id)
        .ok_or_else(|| not_found(&rule_id))?;
    Ok(Json(json!({ "actions": rule.actions() })))
}

/// `GET /pushrules/global/{kind}/{ruleId}/enabled`.
pub async fn get_pushrule_enabled<B: KvBackend + 'static>(
    PushRequester(requester): PushRequester,
    State(state): State<PushState<B>>,
    Path((kind, rule_id)): Path<(String, String)>,
) -> Result<Json<Value>, hs_http::error::MatrixError> {
    let kind = rule_kind(&kind)?;
    let ruleset = effective(&state, &requester.user_id).await?;
    let rule = ruleset
        .get(kind, &rule_id)
        .ok_or_else(|| not_found(&rule_id))?;
    Ok(Json(json!({ "enabled": rule.enabled() })))
}

/// The `before`/`after` query parameters that position a newly inserted rule.
#[derive(Deserialize)]
pub struct BeforeAfter {
    before: Option<String>,
    after: Option<String>,
}

/// `PUT /pushrules/global/{kind}/{ruleId}`'s request body.
#[derive(Deserialize)]
pub struct SetRuleBody {
    actions: Vec<Action>,
    #[serde(default)]
    conditions: Vec<PushCondition>,
    #[serde(default)]
    pattern: Option<String>,
}

/// `PUT /pushrules/global/{kind}/{ruleId}`: create or update a user-defined rule.
pub async fn put_pushrule<B: KvBackend + 'static>(
    PushRequester(requester): PushRequester,
    State(state): State<PushState<B>>,
    Path((kind, rule_id)): Path<(String, String)>,
    Query(ba): Query<BeforeAfter>,
    Json(body): Json<SetRuleBody>,
) -> Result<Json<Value>, hs_http::error::MatrixError> {
    let kind = rule_kind(&kind)?;
    let new_rule = match kind {
        RuleKind::Override => NewPushRule::Override(NewConditionalPushRule::new(
            rule_id,
            body.conditions,
            body.actions,
        )),
        RuleKind::Underride => NewPushRule::Underride(NewConditionalPushRule::new(
            rule_id,
            body.conditions,
            body.actions,
        )),
        RuleKind::Content => {
            let pattern = body.pattern.ok_or_else(|| {
                hs_http::error::MatrixError::bad_json("content rules require a pattern")
            })?;
            NewPushRule::Content(NewPatternedPushRule::new(rule_id, pattern, body.actions))
        }
        RuleKind::Room => {
            let room_id = <&ruma::RoomId>::try_from(rule_id.as_str()).map_err(|e| {
                hs_http::error::MatrixError::bad_json(format!("invalid room id: {e}"))
            })?;
            NewPushRule::Room(NewSimplePushRule::new(room_id.to_owned(), body.actions))
        }
        RuleKind::Sender => {
            let user_id = <&UserId>::try_from(rule_id.as_str()).map_err(|e| {
                hs_http::error::MatrixError::bad_json(format!("invalid user id: {e}"))
            })?;
            NewPushRule::Sender(NewSimplePushRule::new(user_id.to_owned(), body.actions))
        }
        // `rule_kind` already rejected `_Custom`, and `RuleKind` is `#[non_exhaustive]`, so a
        // wildcard is required: a variant added by a future Ruma is an unknown kind to us, which
        // is the same 400 the spec asks for on an unrecognised kind.
        _ => return Err(unknown_kind(&kind)),
    };

    let mut ruleset = (*effective(&state, &requester.user_id).await?).clone();
    ruleset
        .insert(new_rule, ba.after.as_deref(), ba.before.as_deref())
        .map_err(|e| hs_http::error::MatrixError::bad_json(e.to_string()))?;
    state
        .rulesets
        .set_ruleset(&requester.user_id, &ruleset)
        .await
        .map_err(store_err)?;
    Ok(Json(json!({})))
}

/// `DELETE /pushrules/global/{kind}/{ruleId}`.
pub async fn delete_pushrule<B: KvBackend + 'static>(
    PushRequester(requester): PushRequester,
    State(state): State<PushState<B>>,
    Path((kind, rule_id)): Path<(String, String)>,
) -> Result<Json<Value>, hs_http::error::MatrixError> {
    let kind = rule_kind(&kind)?;
    let mut ruleset = (*effective(&state, &requester.user_id).await?).clone();
    ruleset
        .remove(kind, rule_id.as_str())
        .map_err(|_| not_found(&rule_id))?;
    state
        .rulesets
        .set_ruleset(&requester.user_id, &ruleset)
        .await
        .map_err(store_err)?;
    Ok(Json(json!({})))
}

/// `PUT /pushrules/global/{kind}/{ruleId}/actions`'s request body.
#[derive(Deserialize)]
pub struct SetActionsBody {
    actions: Vec<Action>,
}

/// `PUT /pushrules/global/{kind}/{ruleId}/actions`.
pub async fn put_pushrule_actions<B: KvBackend + 'static>(
    PushRequester(requester): PushRequester,
    State(state): State<PushState<B>>,
    Path((kind, rule_id)): Path<(String, String)>,
    Json(body): Json<SetActionsBody>,
) -> Result<Json<Value>, hs_http::error::MatrixError> {
    let kind = rule_kind(&kind)?;
    let mut ruleset = (*effective(&state, &requester.user_id).await?).clone();
    ruleset
        .set_actions(kind, rule_id.as_str(), body.actions)
        .map_err(|_| not_found(&rule_id))?;
    state
        .rulesets
        .set_ruleset(&requester.user_id, &ruleset)
        .await
        .map_err(store_err)?;
    Ok(Json(json!({})))
}

/// `PUT /pushrules/global/{kind}/{ruleId}/enabled`'s request body.
#[derive(Deserialize)]
pub struct SetEnabledBody {
    enabled: bool,
}

/// `PUT /pushrules/global/{kind}/{ruleId}/enabled`.
pub async fn put_pushrule_enabled<B: KvBackend + 'static>(
    PushRequester(requester): PushRequester,
    State(state): State<PushState<B>>,
    Path((kind, rule_id)): Path<(String, String)>,
    Json(body): Json<SetEnabledBody>,
) -> Result<Json<Value>, hs_http::error::MatrixError> {
    let kind = rule_kind(&kind)?;
    let mut ruleset = (*effective(&state, &requester.user_id).await?).clone();
    ruleset
        .set_enabled(kind, rule_id.as_str(), body.enabled)
        .map_err(|_| not_found(&rule_id))?;
    state
        .rulesets
        .set_ruleset(&requester.user_id, &ruleset)
        .await
        .map_err(store_err)?;
    Ok(Json(json!({})))
}

async fn effective<B: KvBackend + 'static>(
    state: &PushState<B>,
    user_id: &UserId,
) -> Result<std::sync::Arc<ruma::push::Ruleset>, hs_http::error::MatrixError> {
    state
        .rulesets
        .effective_ruleset(user_id)
        .await
        .map_err(store_err)
}

fn store_err(e: crate::error::StoreError) -> hs_http::error::MatrixError {
    hs_http::error::MatrixError::custom(
        axum::http::StatusCode::INTERNAL_SERVER_ERROR,
        hs_http::error::MatrixErrorCode::Unknown,
        e.to_string(),
    )
}
