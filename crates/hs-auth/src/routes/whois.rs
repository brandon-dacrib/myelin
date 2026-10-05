//! `GET /_matrix/client/v3/admin/whois/{userId}`: the spec's "Server administration" module --
//! the devices an account is signed in on and where each was last seen.
//!
//! A server administrator may ask about anyone; anyone else only about themself, as Synapse
//! allows (`WhoisRestServlet`: "You are not a server admin" for someone else's account). The
//! shape is the spec's: `devices.<device_id>.sessions[].connections[]` with `ip`, `last_seen`
//! and `user_agent`. This server keeps one "last seen" per device rather than a history of
//! connections, so each device has one session with at most one connection -- the latest. The
//! user agent is not recorded, so it is `null`.

use axum::Json;
use axum::extract::{Path, State};
use ruma::UserId;
use serde_json::{Map, Value, json};

use crate::error::MatrixError;
use crate::requester::Requester;
use crate::state::AuthState;

/// `GET /admin/whois/{userId}`.
///
/// # Errors
/// `403` asking about another account without being an administrator; `404` for an account
/// this server does not have; `400` for a malformed user ID.
pub async fn get_whois(
    State(state): State<AuthState>,
    requester: Requester,
    Path(user_id): Path<String>,
) -> Result<Json<Value>, MatrixError> {
    let user_id = UserId::parse(&user_id)
        .map_err(|_| MatrixError::invalid_param(format!("'{user_id}' is not a user ID")))?;
    if user_id != requester.user_id && !requester.is_admin {
        return Err(MatrixError::forbidden("You are not a server admin"));
    }
    if user_id.server_name() != state.server_name()
        || state.store.get_user(&user_id).await?.is_none()
    {
        return Err(MatrixError::not_found("No such user on this server"));
    }
    let mut devices = Map::new();
    for device in state.store.list_devices(&user_id).await? {
        let connections: Vec<Value> = device
            .last_seen_ms
            .map(|last_seen| {
                json!({
                    "ip": device.last_seen_ip,
                    "last_seen": last_seen,
                    "user_agent": Value::Null,
                })
            })
            .into_iter()
            .collect();
        devices.insert(
            device.device_id.to_string(),
            json!({"sessions": [{"connections": connections}]}),
        );
    }
    if user_id != requester.user_id {
        tracing::info!(admin = %requester.user_id, user = %user_id, "an administrator looked up a user's sessions (whois)");
    }
    Ok(Json(json!({ "user_id": user_id, "devices": devices })))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{DeviceRecord, UserRecord};

    async fn state_with_alice() -> AuthState {
        let state = AuthState::in_memory();
        let alice = ruma::user_id!("@alice:example.org").to_owned();
        state
            .store
            .create_user(UserRecord::new(alice.clone(), 0))
            .await
            .unwrap();
        state
            .store
            .upsert_device(DeviceRecord {
                user_id: alice.clone(),
                device_id: "PHONE".into(),
                display_name: None,
                last_seen_ms: None,
                last_seen_ip: None,
            })
            .await
            .unwrap();
        state
            .store
            .record_seen(&alice, "PHONE".into(), 1234, Some("10.0.0.1".into()))
            .await
            .unwrap();
        state
    }

    /// Sytest's "/whois" (`tests/48admin.pl`): an account asking about itself.
    #[tokio::test]
    async fn an_account_sees_its_own_sessions() {
        let state = state_with_alice().await;
        let alice = ruma::user_id!("@alice:example.org").to_owned();
        let Json(body) = get_whois(
            State(state),
            Requester::for_user(alice.clone()),
            Path(alice.to_string()),
        )
        .await
        .unwrap();
        assert_eq!(body["user_id"], "@alice:example.org");
        let connection = &body["devices"]["PHONE"]["sessions"][0]["connections"][0];
        assert_eq!(connection["ip"], "10.0.0.1");
        assert_eq!(connection["last_seen"], 1234);
        assert!(connection.get("user_agent").is_some());
    }

    #[tokio::test]
    async fn only_an_administrator_asks_about_somebody_else() {
        let state = state_with_alice().await;
        let bob = Requester::for_user(ruma::user_id!("@bob:example.org").to_owned());
        let err = get_whois(
            State(state.clone()),
            bob.clone(),
            Path("@alice:example.org".to_owned()),
        )
        .await
        .unwrap_err();
        assert_eq!(err.status(), axum::http::StatusCode::FORBIDDEN);
        let mut admin = bob;
        admin.is_admin = true;
        assert!(
            get_whois(
                State(state.clone()),
                admin.clone(),
                Path("@alice:example.org".to_owned())
            )
            .await
            .is_ok()
        );
        let err = get_whois(State(state), admin, Path("@nobody:example.org".to_owned()))
            .await
            .unwrap_err();
        assert_eq!(err.status(), axum::http::StatusCode::NOT_FOUND);
    }
}
