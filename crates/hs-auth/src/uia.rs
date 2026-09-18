//! The User-Interactive Authentication (UIA) session state machine.
//!
//! Wire types (`AuthType`, `AuthData`, `AuthFlow`, `UiaaInfo`) are reused from `ruma`'s
//! `client-api` feature rather than redefined here — they are exactly the spec's shapes and
//! `ruma::api::client::uiaa::AuthData` already parses every stage's request body. This module owns
//! the *session bookkeeping* on top of them: creating and validating sessions
//! ([`session_id_for`]), recording completed stages, and deciding whether a flow is satisfied
//! ([`flow_satisfied`]), against the [`crate::store::UiaStore`] trait.
//!
//! Stage-specific verification (is this password correct, is this registration token valid) is
//! deliberately *not* here: only the caller (a route handler) has the context to check it (which
//! user's password, which registration-token store), so handlers call [`advance`] with the
//! already-computed `stage_ok` for whatever stage was just submitted.

use ruma::api::client::uiaa::{AuthFlow, AuthType, UiaaInfo};

use crate::error::MatrixError;
use crate::store::UiaStore;

/// The outcome of one round through the UIA state machine.
#[derive(Debug)]
pub struct UiaOutcome {
    /// The session id in play (freshly created, or the one the client supplied).
    pub session_id: String,
    /// True once every stage of at least one flow has completed.
    pub complete: bool,
    /// Stages completed so far, for building the response body either way.
    pub completed: Vec<AuthType>,
}

/// Resolves the session id for this round: validates a client-supplied one still exists and has
/// not timed out, or creates a fresh one if the client supplied none (the first call of a UIA
/// dance). Matches Synapse's `check_ui_auth`, which creates a session on an absent `session` key
/// and 400s with `M_UNKNOWN`/"Unknown session ID" on a stale or unrecognized one.
///
/// # Errors
/// Returns `400 M_UNKNOWN` if `requested` is `Some` but does not name a live session.
pub async fn session_id_for(
    store: &dyn UiaStore,
    requested: Option<&str>,
    now_ms: u64,
    timeout_ms: u64,
) -> Result<String, MatrixError> {
    match requested {
        Some(id) => {
            if store.session_exists(id, now_ms, timeout_ms).await? {
                Ok(id.to_string())
            } else {
                Err(MatrixError::new(
                    axum::http::StatusCode::BAD_REQUEST,
                    crate::error::ErrCode::InvalidParam,
                    format!("Unknown UI auth session ID: {id}"),
                ))
            }
        }
        None => Ok(store.create_session(now_ms).await?),
    }
}

/// Records that `stage` completed successfully for `session_id`.
pub async fn complete_stage(
    store: &dyn UiaStore,
    session_id: &str,
    stage: AuthType,
) -> Result<(), MatrixError> {
    Ok(store
        .mark_stage_complete(session_id, stage.as_ref())
        .await?)
}

/// True if every stage of at least one flow is present in `completed` (order does not matter,
/// matching the spec: flows list an order for UX purposes, but the server only checks set
/// membership).
#[must_use]
pub fn flow_satisfied(flows: &[AuthFlow], completed: &[String]) -> bool {
    flows.iter().any(|flow| {
        flow.stages
            .iter()
            .all(|want| completed.iter().any(|have| have == want.as_ref()))
    })
}

/// Advances the state machine by one round.
///
/// - `submitted`: the auth type the client just attempted, if the request body carried an `auth`
///   object at all (the first call of a dance normally carries none, and gets back the flow list).
/// - `stage_ok`: whether that stage's own verification (checked by the caller) succeeded. Ignored
///   when `submitted` is `None`.
///
/// # Errors
/// - Propagates [`session_id_for`]'s "unknown session" error.
/// - Returns `401 M_FORBIDDEN` if `stage_ok` is `false` for a submitted stage.
pub async fn advance(
    store: &dyn UiaStore,
    flows: &[AuthFlow],
    session_id_param: Option<&str>,
    submitted: Option<AuthType>,
    stage_ok: bool,
    now_ms: u64,
    timeout_ms: u64,
) -> Result<UiaOutcome, MatrixError> {
    let session_id = session_id_for(store, session_id_param, now_ms, timeout_ms).await?;

    if let Some(auth_type) = submitted {
        if !stage_ok {
            return Err(MatrixError::forbidden(
                "Invalid authentication data for this stage",
            ));
        }
        complete_stage(store, &session_id, auth_type).await?;
    }

    let completed_strings = store.completed_stages(&session_id).await?;
    let complete = flow_satisfied(flows, &completed_strings);
    let completed = completed_strings.into_iter().map(AuthType::from).collect();

    Ok(UiaOutcome {
        session_id,
        complete,
        completed,
    })
}

/// Builds the `401` response body for an incomplete session.
#[must_use]
pub fn incomplete_body(
    flows: Vec<AuthFlow>,
    completed: Vec<AuthType>,
    session_id: String,
) -> UiaaInfo {
    let mut info = UiaaInfo::new(flows);
    info.completed = completed;
    info.session = Some(session_id);
    info
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::memory::InMemoryAuthStore;

    fn dummy_flow() -> Vec<AuthFlow> {
        vec![AuthFlow::new(vec![AuthType::Dummy])]
    }

    fn password_then_terms_flow() -> Vec<AuthFlow> {
        vec![AuthFlow::new(vec![AuthType::Password, AuthType::Terms])]
    }

    #[tokio::test]
    async fn fresh_session_is_created_when_none_supplied() {
        let store = InMemoryAuthStore::new();
        let id = session_id_for(&store, None, 0, 10_000).await.unwrap();
        assert!(!id.is_empty());
    }

    #[tokio::test]
    async fn unknown_session_id_is_rejected() {
        let store = InMemoryAuthStore::new();
        let err = session_id_for(&store, Some("bogus"), 0, 10_000)
            .await
            .unwrap_err();
        assert_eq!(err.status(), axum::http::StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn single_stage_flow_completes_in_one_round() {
        let store = InMemoryAuthStore::new();
        let outcome = advance(
            &store,
            &dummy_flow(),
            None,
            Some(AuthType::Dummy),
            true,
            0,
            10_000,
        )
        .await
        .unwrap();
        assert!(outcome.complete);
        assert_eq!(outcome.completed, vec![AuthType::Dummy]);
    }

    #[tokio::test]
    async fn multi_stage_flow_requires_every_stage() {
        let store = InMemoryAuthStore::new();
        let flows = password_then_terms_flow();

        // First round: no auth submitted yet, just establishes a session.
        let round1 = advance(&store, &flows, None, None, true, 0, 10_000)
            .await
            .unwrap();
        assert!(!round1.complete);
        let session_id = round1.session_id;

        // Second round: password stage.
        let round2 = advance(
            &store,
            &flows,
            Some(&session_id),
            Some(AuthType::Password),
            true,
            0,
            10_000,
        )
        .await
        .unwrap();
        assert!(!round2.complete);

        // Third round: terms stage completes the flow.
        let round3 = advance(
            &store,
            &flows,
            Some(&session_id),
            Some(AuthType::Terms),
            true,
            0,
            10_000,
        )
        .await
        .unwrap();
        assert!(round3.complete);
        assert_eq!(round3.completed.len(), 2);
    }

    #[tokio::test]
    async fn failed_stage_verification_is_forbidden_and_does_not_complete_it() {
        let store = InMemoryAuthStore::new();
        let flows = dummy_flow();
        let round1 = advance(&store, &flows, None, None, true, 0, 10_000)
            .await
            .unwrap();
        let err = advance(
            &store,
            &flows,
            Some(&round1.session_id),
            Some(AuthType::Password),
            false,
            0,
            10_000,
        )
        .await
        .unwrap_err();
        assert_eq!(err.status(), axum::http::StatusCode::FORBIDDEN);

        let completed = store.completed_stages(&round1.session_id).await.unwrap();
        assert!(completed.is_empty());
    }

    #[tokio::test]
    async fn session_expires_after_timeout() {
        let store = InMemoryAuthStore::new();
        let id = session_id_for(&store, None, 0, 5_000).await.unwrap();
        let err = session_id_for(&store, Some(&id), 10_000, 5_000)
            .await
            .unwrap_err();
        assert_eq!(err.status(), axum::http::StatusCode::BAD_REQUEST);
    }

    #[test]
    fn flow_satisfied_requires_every_stage_of_one_flow() {
        let flows = vec![
            AuthFlow::new(vec![AuthType::Dummy]),
            AuthFlow::new(vec![AuthType::Password, AuthType::Terms]),
        ];
        assert!(flow_satisfied(&flows, &["m.login.dummy".to_string()]));
        assert!(!flow_satisfied(&flows, &["m.login.password".to_string()]));
        assert!(flow_satisfied(
            &flows,
            &["m.login.password".to_string(), "m.login.terms".to_string()]
        ));
    }
}
