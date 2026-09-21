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
/// A stage submitted with no `session` id gets a **fresh** session created for it and, if
/// `stage_ok`, completes that stage on it in the same call — this is what lets a client that
/// already knows the flow (single-stage `m.login.dummy`, most commonly) skip the round trip that
/// would otherwise just hand back the flow list it already knows. This is not a shortcut this
/// crate invented: it is Synapse's actual, documented behavior. Read
/// `refs/synapse/synapse/handlers/auth.py::AuthHandler.check_ui_auth`: `sid = authdict.get
/// ("session")` then `if not sid: session = await self.store.create_ui_auth_session(...)` --
/// unconditionally, whether or not `authdict` also carries a `type` -- immediately followed by
/// checking and completing that same `type` on the brand-new session in the same function call.
///
/// Complement's `apidoc_register_test.go` "Registration without a session fails" wants a
/// stricter rule (session becomes mandatory once one has ever been issued for this dance), but
/// explicitly `runtime.SkipIf(t, runtime.Synapse, runtime.Dendrite, runtime.Conduit)`s all three
/// reference servers "because [they] historically did not enforce this requirement strictly" --
/// i.e. this is a known aspirational check, not a baseline every conformant server is expected to
/// pass. Confirmed experimentally too: implementing the stricter rule here broke this crate's own
/// `single_stage_flow_completes_in_one_round`-shaped tests and
/// `routes::tests::full_register_then_whoami_round_trip_through_the_router`, i.e. it would also
/// break any real client (matrix-rust-sdk included) that registers by sending `username`/
/// `password`/`auth: {"type": "m.login.dummy"}` in one shot instead of round-tripping first.
/// Matching Synapse's real behavior here, not the stricter aspirational reading, was the
/// deliberate call -- see `docs/status/07-auth-and-identity.md`.
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
            // The spec, on user-interactive authentication: "If the homeserver decides that an
            // attempt on a stage was unsuccessful, but the client may make a second attempt, it
            // returns the same HTTP status 401 response as above, with the addition of the
            // standard `errcode` and `error` fields describing the error." This was a bare
            // `403`, which tells a client the dance is over and takes away the `session` it
            // would need to try the password again.
            let completed = store
                .completed_stages(&session_id)
                .await?
                .into_iter()
                .map(AuthType::from)
                .collect();
            let challenge =
                serde_json::to_value(incomplete_body(flows.to_vec(), completed, session_id))
                    .unwrap_or_default();
            let mut error = MatrixError::new(
                axum::http::StatusCode::UNAUTHORIZED,
                crate::error::ErrCode::Forbidden,
                "Invalid authentication data for this stage",
            );
            for (key, value) in challenge.as_object().into_iter().flatten() {
                error = error.with_extra(key, value.clone());
            }
            return Err(error);
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
///
/// `params` is always populated (as `{}` when no offered stage needs one), never left `None`.
/// `UiaaInfo`'s own `#[serde(skip_serializing_if = "Option::is_none")]` on that field means
/// leaving it `None` (`UiaaInfo::new`'s default) drops the key entirely, but the spec's UIA
/// example responses always show a `params` object and Synapse's `_auth_dict_for_flows`
/// (`refs/synapse/synapse/handlers/auth.py`) always initializes `params: dict = {}` before adding
/// per-stage entries -- it is never entirely absent. Confirmed against Complement's
/// `refs/complement/tests/csapi/apidoc_device_management_test.go`: "DELETE /device/{deviceId}
/// with no body gives a 401" asserts `match.JSONKeyPresent("params")` on the challenge body.
/// This crate offers no stage that has its own params (`m.login.dummy`, `m.login.password`,
/// `m.login.registration_token`, `m.login.terms` all take none), so the object is always empty.
#[must_use]
pub fn incomplete_body(
    flows: Vec<AuthFlow>,
    completed: Vec<AuthType>,
    session_id: String,
) -> UiaaInfo {
    let mut info = UiaaInfo::new(flows);
    info.completed = completed;
    info.session = Some(session_id);
    info.params = serde_json::value::to_raw_value(&serde_json::json!({})).ok();
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

    /// Matches Synapse's real, documented behavior (`AuthHandler.check_ui_auth`, see this
    /// function's own doc comment): a stage submitted with a `session` id the store has never
    /// seen -- including the always-fresh case of no `session` at all -- gets a brand-new session
    /// minted for it and, if `stage_ok`, completes that stage on it in the same call. This is
    /// deliberately *not* changed to require an id the server previously issued (Complement's
    /// `apidoc_register_test.go` "Registration without a session fails" wants that, but skips
    /// Synapse, Dendrite and Conduit for not implementing it either).
    #[tokio::test]
    async fn a_stage_submitted_without_a_session_id_gets_a_fresh_session_and_can_still_complete() {
        let store = InMemoryAuthStore::new();
        let round1 = advance(&store, &dummy_flow(), None, None, true, 0, 10_000)
            .await
            .unwrap();
        assert!(!round1.complete);

        // A second, sessionless submission of the stage does not touch `round1`'s session (a
        // fresh one is minted for it instead) but does complete on its own.
        let round2 = advance(
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
        assert!(round2.complete);
        assert_ne!(round1.session_id, round2.session_id);

        let original_completed = store.completed_stages(&round1.session_id).await.unwrap();
        assert!(original_completed.is_empty());
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

    /// A failed attempt is not the end of the dance: the spec wants the same `401` challenge
    /// again, with `errcode` and `error` added, so that the client still holds the `session` it
    /// needs to try a second time.
    #[tokio::test]
    async fn a_failed_stage_is_a_401_that_still_carries_the_challenge() {
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
        assert_eq!(err.status(), axum::http::StatusCode::UNAUTHORIZED);
        assert_eq!(err.errcode(), crate::error::ErrCode::Forbidden);

        use axum::response::IntoResponse;
        let bytes = axum::body::to_bytes(err.into_response().into_body(), 1 << 16)
            .await
            .unwrap();
        let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["errcode"], "M_FORBIDDEN");
        assert!(body["error"].is_string(), "{body}");
        assert_eq!(
            body["session"],
            round1.session_id.as_str(),
            "the same session, to retry with"
        );
        assert!(body["flows"].is_array(), "{body}");
        assert!(body["params"].is_object(), "{body}");

        let completed = store.completed_stages(&round1.session_id).await.unwrap();
        assert!(
            completed.is_empty(),
            "and the failed stage was not completed"
        );
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

    /// `refs/complement/tests/csapi/apidoc_device_management_test.go`'s "DELETE
    /// /device/{deviceId} with no body gives a 401" checks `params` is present in the challenge
    /// body, not merely `flows`/`session`. `UiaaInfo`'s `params` field is skipped entirely when
    /// `None` (see this function's doc comment), so it must be set to `Some` even when empty.
    #[test]
    fn incomplete_body_always_includes_a_params_object() {
        let info = incomplete_body(dummy_flow(), Vec::new(), "sess1".to_string());
        let value = serde_json::to_value(&info).unwrap();
        assert_eq!(value["params"], serde_json::json!({}));
    }
}
