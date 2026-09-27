//! Reporting content to the server's administrators: `POST /rooms/{roomId}/report/{eventId}`,
//! `POST /rooms/{roomId}/report` (client-server API v1.13) and `POST /users/{userId}/report`
//! (v1.14).
//!
//! Each keeps a report in the durable store the admin API's Reports inbox reads
//! (`crate::reports`). A client learns nothing about what happens next: the answer is `{}`
//! whatever a moderator later decides.
//!
//! What is refused, per the spec:
//!
//! - an event the reporter cannot see (not in the room, or before their history visibility
//!   allows): `404 M_NOT_FOUND`, the same answer as an event that does not exist, so a report
//!   cannot be used to probe for events;
//! - a room this server does not have: `404`;
//! - a local user who does not exist: `404`. A user on another server cannot be checked from
//!   here, so the report is kept and a moderator decides.
//! - a `score` outside -100..=0: `400 M_INVALID_PARAM`; a missing `reason` where the endpoint
//!   requires one: `400 M_MISSING_PARAM`.

use axum::Json;
use axum::extract::{Path, State};
use axum::response::{IntoResponse, Response};
use hs_admin::reports::{AdminReport, ReportKind, ReportStatus};
use hs_http::body::PermissiveJson;
use hs_http::error::MatrixError;
use hs_kv::KvBackend;
use ruma::{EventId, RoomId, UserId};
use serde_json::{Value, json};

use crate::error::RoomError;
use crate::state::{RoomRequester, RoomState};

fn new_report(kind: ReportKind, reporter: &UserId) -> AdminReport {
    AdminReport {
        id: hs_admin::reports::new_report_id(),
        kind,
        status: ReportStatus::Open,
        room_id: None,
        event_id: None,
        reporter_id: reporter.to_string(),
        reported_user_id: None,
        reason: None,
        score: None,
        received_at: hs_http::time::now_rfc3339(),
        resolution: None,
        resolution_note: None,
        resolved_at: None,
        resolved_by: None,
        event: None,
    }
}

/// `reason`, which must be a string if present.
// Answered once per request; boxing the error would only add noise.
#[allow(clippy::result_large_err)]
fn optional_reason(body: &Value) -> Result<Option<String>, MatrixError> {
    match body.get("reason") {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(reason)) => Ok(Some(reason.clone())),
        Some(_) => Err(MatrixError::bad_json("reason must be a string")),
    }
}

/// `reason`, which these endpoints require.
#[allow(clippy::result_large_err)]
fn required_reason(body: &Value) -> Result<String, MatrixError> {
    optional_reason(body)?.ok_or_else(|| MatrixError::missing_param("reason"))
}

fn keep<B: KvBackend + 'static>(
    state: &RoomState<B>,
    report: &AdminReport,
) -> Result<Response, RoomError> {
    state.rooms.reports().file(report)?;
    tracing::info!(
        report = report.id,
        kind = ?report.kind,
        reporter = report.reporter_id,
        "a report was filed"
    );
    Ok(Json(json!({})).into_response())
}

/// `POST /rooms/{roomId}/report/{eventId}`: `{"reason"?: string, "score"?: -100..=0}`.
pub async fn post_report_event<B: KvBackend + 'static>(
    State(state): State<RoomState<B>>,
    Path((room_id, event_id)): Path<(String, String)>,
    RoomRequester(requester): RoomRequester,
    PermissiveJson(body): PermissiveJson<Value>,
) -> Response {
    let reason = match optional_reason(&body) {
        Ok(reason) => reason,
        Err(e) => return e.into_response(),
    };
    let score = match body.get("score") {
        None | Some(Value::Null) => None,
        Some(score) => match score.as_i64() {
            Some(score) if (-100..=0).contains(&score) => Some(score),
            _ => {
                return MatrixError::custom(
                    axum::http::StatusCode::BAD_REQUEST,
                    hs_http::error::MatrixErrorCode::InvalidParam,
                    "score must be an integer between -100 and 0",
                )
                .into_response();
            }
        },
    };
    let result: Result<Response, RoomError> = async {
        let room_id = RoomId::parse(&room_id).map_err(|e| RoomError::BadRequest(e.to_string()))?;
        let target = EventId::parse(&event_id).map_err(|e| RoomError::BadRequest(e.to_string()))?;
        let handle = state.rooms.get_or_load(&room_id).await?;
        let user = requester.user_id.clone();
        let sender = handle
            .query(move |actor| {
                let event = actor
                    .event_by_id(&target)
                    .ok_or_else(|| RoomError::EventNotFound("event not found".into()))?;
                if !actor.event_visible_to(event, &user)? {
                    return Err(RoomError::EventNotFound("event not found".into()));
                }
                Ok::<_, RoomError>(event.header().sender.to_string())
            })
            .await?;
        let mut report = new_report(ReportKind::Event, &requester.user_id);
        report.room_id = Some(room_id.to_string());
        report.event_id = Some(event_id.clone());
        report.reported_user_id = Some(sender);
        report.reason = reason;
        report.score = score;
        keep(&state, &report)
    }
    .await;
    result.unwrap_or_else(IntoResponse::into_response)
}

/// `POST /rooms/{roomId}/report`: `{"reason": string}`.
pub async fn post_report_room<B: KvBackend + 'static>(
    State(state): State<RoomState<B>>,
    Path(room_id): Path<String>,
    RoomRequester(requester): RoomRequester,
    PermissiveJson(body): PermissiveJson<Value>,
) -> Response {
    let reason = match required_reason(&body) {
        Ok(reason) => reason,
        Err(e) => return e.into_response(),
    };
    let result: Result<Response, RoomError> = async {
        let room_id = RoomId::parse(&room_id).map_err(|e| RoomError::BadRequest(e.to_string()))?;
        // Loading it is how a room that does not exist is told apart.
        state.rooms.get_or_load(&room_id).await?;
        let mut report = new_report(ReportKind::Room, &requester.user_id);
        report.room_id = Some(room_id.to_string());
        report.reason = Some(reason);
        keep(&state, &report)
    }
    .await;
    result.unwrap_or_else(IntoResponse::into_response)
}

/// `POST /users/{userId}/report`: `{"reason": string}`.
pub async fn post_report_user<B: KvBackend + 'static>(
    State(state): State<RoomState<B>>,
    Path(user_id): Path<String>,
    RoomRequester(requester): RoomRequester,
    PermissiveJson(body): PermissiveJson<Value>,
) -> Response {
    let reason = match required_reason(&body) {
        Ok(reason) => reason,
        Err(e) => return e.into_response(),
    };
    let Ok(target) = UserId::parse(&user_id) else {
        return MatrixError::custom(
            axum::http::StatusCode::BAD_REQUEST,
            hs_http::error::MatrixErrorCode::InvalidParam,
            "not a valid user id",
        )
        .into_response();
    };
    if target.server_name() == state.identity.server_name {
        match state.auth.store.get_user(&target).await {
            Ok(Some(_)) => {}
            Ok(None) => return MatrixError::not_found("no such user").into_response(),
            Err(e) => {
                return RoomError::Internal(format!("looking up the reported user: {e}"))
                    .into_response();
            }
        }
    }
    let mut report = new_report(ReportKind::User, &requester.user_id);
    report.reported_user_id = Some(target.to_string());
    report.reason = Some(reason);
    keep(&state, &report).unwrap_or_else(IntoResponse::into_response)
}
