//! `GET /_matrix/client/v1/rooms/{roomId}/hierarchy`: the space hierarchy (MSC2946). The walk
//! itself is `crate::hierarchy::walk`; this is its parameter parsing and its log line.

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::response::{IntoResponse, Response};
use hs_kv::KvBackend;
use ruma::RoomId;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::error::RoomError;
use crate::hierarchy::{HierarchyRequest, MAX_LIMIT, walk};
use crate::state::{RoomRequester, RoomState};

/// The endpoint's query string. Every parameter is optional; each is checked here rather than
/// by the deserializer so that a bad one is `M_INVALID_PARAM` naming it, not a generic `400`.
#[derive(Debug, Default, Deserialize)]
pub struct HierarchyParams {
    /// `suggested_only`: `true` or `false`.
    #[serde(default)]
    pub suggested_only: Option<String>,
    /// `limit`: an integer greater than zero.
    #[serde(default)]
    pub limit: Option<String>,
    /// `max_depth`: a non-negative integer.
    #[serde(default)]
    pub max_depth: Option<String>,
    /// `from`: a `next_batch` from the previous page.
    #[serde(default)]
    pub from: Option<String>,
}

impl HierarchyParams {
    /// The parameters as a request for `requester` about `root`.
    ///
    /// # Errors
    /// [`RoomError::InvalidParam`] naming the parameter that is not what the spec says.
    pub fn into_request(
        self,
        root: ruma::OwnedRoomId,
        requester: ruma::OwnedUserId,
    ) -> Result<HierarchyRequest, RoomError> {
        let suggested_only = match self.suggested_only.as_deref() {
            None | Some("false") => false,
            Some("true") => true,
            Some(other) => {
                return Err(RoomError::InvalidParam(format!(
                    "suggested_only must be true or false, not {other:?}"
                )));
            }
        };
        let limit = match self.limit.as_deref() {
            None => MAX_LIMIT,
            Some(raw) => raw
                .parse::<usize>()
                .ok()
                .filter(|limit| *limit > 0)
                .ok_or_else(|| {
                    RoomError::InvalidParam(format!(
                        "limit must be an integer greater than zero, not {raw:?}"
                    ))
                })?,
        };
        let max_depth = match self.max_depth.as_deref() {
            None => None,
            Some(raw) => Some(raw.parse::<u32>().map_err(|_| {
                RoomError::InvalidParam(format!(
                    "max_depth must be a non-negative integer, not {raw:?}"
                ))
            })?),
        };
        Ok(HierarchyRequest {
            root,
            requester,
            suggested_only,
            limit,
            max_depth,
            from: self.from.filter(|from| !from.is_empty()),
        })
    }
}

/// `GET /_matrix/client/v1/rooms/{roomId}/hierarchy`.
pub async fn get_hierarchy<B: KvBackend + 'static>(
    State(state): State<RoomState<B>>,
    Path(room_id): Path<String>,
    Query(params): Query<HierarchyParams>,
    RoomRequester(requester): RoomRequester,
) -> Result<Response, RoomError> {
    let root = RoomId::parse(&room_id)
        .map(|r| r.to_owned())
        .map_err(|e| RoomError::InvalidParam(format!("{room_id:?} is not a room ID: {e}")))?;
    let request = params.into_request(root, requester.user_id.clone())?;
    let started = std::time::Instant::now();
    let page = walk(&state.rooms, &request).await?;
    tracing::debug!(
        root = %request.root,
        requester = %request.requester,
        suggested_only = request.suggested_only,
        limit = request.limit,
        max_depth = ?request.max_depth,
        paginated = request.from.is_some(),
        rooms = page.rooms.len(),
        depth_reached = page.stats.depth_reached,
        hidden = page.stats.hidden,
        remote_fetches = page.stats.remote_fetches,
        remote_failures = page.stats.remote_failures,
        remote_skipped = page.stats.remote_skipped,
        has_more = page.next_batch.is_some(),
        elapsed_ms = started.elapsed().as_millis(),
        "walked a space hierarchy"
    );
    let mut body = json!({ "rooms": page.rooms });
    if let Some(next_batch) = page.next_batch {
        body["next_batch"] = Value::String(next_batch);
    }
    Ok(Json(body).into_response())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ruma::{room_id, user_id};

    fn request(params: HierarchyParams) -> Result<HierarchyRequest, RoomError> {
        params.into_request(
            room_id!("!space:hs1").to_owned(),
            user_id!("@alice:hs1").to_owned(),
        )
    }

    #[test]
    fn defaults_are_the_spec_and_synapse_defaults() {
        let request = request(HierarchyParams::default()).unwrap();
        assert!(!request.suggested_only);
        assert_eq!(request.limit, MAX_LIMIT);
        assert_eq!(request.max_depth, None);
        assert_eq!(request.from, None);
    }

    #[test]
    fn every_parameter_is_read() {
        let request = request(HierarchyParams {
            suggested_only: Some("true".into()),
            limit: Some("4".into()),
            max_depth: Some("1".into()),
            from: Some("abc".into()),
        })
        .unwrap();
        assert!(request.suggested_only);
        assert_eq!(request.limit, 4);
        assert_eq!(request.max_depth, Some(1));
        assert_eq!(request.from.as_deref(), Some("abc"));
    }

    #[test]
    fn a_bad_parameter_is_invalid_param() {
        for params in [
            HierarchyParams {
                suggested_only: Some("yes".into()),
                ..Default::default()
            },
            HierarchyParams {
                limit: Some("0".into()),
                ..Default::default()
            },
            HierarchyParams {
                limit: Some("-1".into()),
                ..Default::default()
            },
            HierarchyParams {
                max_depth: Some("-1".into()),
                ..Default::default()
            },
            HierarchyParams {
                max_depth: Some("deep".into()),
                ..Default::default()
            },
        ] {
            assert!(matches!(request(params), Err(RoomError::InvalidParam(_))));
        }
    }
}
