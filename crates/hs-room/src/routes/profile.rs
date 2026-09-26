//! `PUT /profile/{userId}/displayname` and `PUT /profile/{userId}/avatar_url`: the write half of
//! profile propagation.
//!
//! # The bug this closes
//!
//! Per the spec (and Synapse's `ProfileHandler.on_profile_update`/`_update_join_states`),
//! changing a display name or avatar is supposed to re-send the user's `m.room.member` event, with
//! the same membership but a fresh `displayname`/`avatar_url`, in every room they are currently
//! joined to -- that re-send is the *only* way another client's `/sync` ever learns the profile
//! changed at all (a bare `PUT /profile/...` touches no room). `hs-auth`
//! (`crates/hs-auth/src/routes/profile.rs`) writes the new profile into its own `UserRecord` and
//! stops there; `crate::routes::membership`'s `extra()` only reads a user's profile into a
//! *new* `m.room.member` event at join/invite/knock time, never revisiting an already-sent one.
//! Confirmed live by track 05 (`docs/status/05-sync.md`, "Session 4"): a real client's rename
//! round-trips through `GET`/`PUT /profile` but never reaches a second, already-joined user's
//! `/sync`.
//!
//! # Why the write endpoint is mounted from `hs-room`, not `hs-auth`
//!
//! `hs-auth` cannot depend on `hs-room` -- this crate already depends on `hs-auth` (for
//! `AuthState`/`UserRecord`), and the reverse would be a cycle -- so the crate that *writes* the
//! profile record has no way to reach the per-room actors that need to learn about it.
//! [`crate::state::RoomState`] already embeds a full `hs_auth::state::AuthState`, so a handler
//! mounted here has both halves in one place: it calls straight into `hs-auth`'s own
//! `put_displayname`/`put_avatar_url` for the write (reusing that function's validation,
//! self-only check and error shapes unchanged), then fans the refresh out to this user's joined
//! rooms.
//!
//! `hs-auth`'s router (`crates/hs-auth/src/routes/mod.rs`) no longer registers a `PUT` for either
//! path (only the `GET`s, which need no room context, stay there); this crate's router registers
//! the `PUT`s at the identical spec-relative paths, and both routers are merged at the same
//! `/_matrix/client/{v3,r0}` prefixes in `hs-cli`'s `serve.rs`, unchanged -- a client sees no
//! difference between the two paths being served by different crates. This mirrors the
//! `GET`/`POST /publicRooms` split `docs/status/04-room-and-events.md` (session 4) already
//! recorded between this crate and `hs-user`: different HTTP methods on the same path, served by
//! different routers merged at the same prefix, coexist without collision -- only an identical
//! method *and* path registered twice panics at router-build time.
//!
//! # What a rename costs
//!
//! A user in many rooms must not have their `PUT /profile/.../displayname` block on re-stamping
//! every one of them: [`spawn_refresh`] returns as soon as the two request-scoped calls above
//! finish (the store write, already durable) and does the actual per-room fan-out on a detached
//! background task, so the HTTP response never waits on it. That background task itself bounds
//! its own concurrency to [`MAX_CONCURRENT_ROOM_REFRESHES`] rooms at a time (a
//! [`tokio::sync::Semaphore`]) rather than loading/locking every joined room's actor at once --
//! a user in a thousand rooms produces a thousand cheap, mostly-idle `tokio::spawn`ed tasks
//! (each just awaiting a semaphore permit) rather than a thousand-way concurrent burst of KV
//! transactions and room-actor locks. Each room's own refresh is a single `send_event`-shaped
//! write, already idempotent (`RoomActor::idempotent_state_reuse`): a room whose stored profile
//! already matches the new one (for example a second `PUT` with the same value) sends no event at
//! all, so this fan-out is also safe to run redundantly.

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::response::Response;
use hs_http::body::PermissiveJson;
use hs_http::error::MatrixError;
use hs_kv::KvBackend;
use ruma::UserId;
use serde_json::Value;

use crate::registry::RoomRegistry;
use crate::state::{RoomRequester, RoomState, auth_error_to_matrix_error};

/// How many of a user's joined rooms this crate re-stamps concurrently in the background fan-out.
/// Deliberately small and fixed rather than scaling with the user's room count: bounding
/// concurrency is the whole point (see the module docs' "What a rename costs").
const MAX_CONCURRENT_ROOM_REFRESHES: usize = 16;

fn now_ms() -> i64 {
    i64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis(),
    )
    .unwrap_or(i64::MAX)
}

/// Spawns the background fan-out that re-stamps `user_id`'s `m.room.member` event in every room
/// they are joined to, reading whatever profile is currently stored (not a value passed in, so a
/// rapid second profile change before the first fan-out finishes is reflected correctly rather
/// than racing to overwrite with a stale value). Never awaited by the caller -- see the module
/// docs.
fn spawn_refresh<B: KvBackend + 'static>(
    rooms: Arc<RoomRegistry<B>>,
    auth: hs_auth::state::AuthState,
    user_id: String,
) {
    tokio::spawn(async move {
        let Ok(user) = UserId::parse(&user_id) else {
            // Cannot happen in practice: the caller only reaches `spawn_refresh` after
            // `hs-auth`'s own handler already parsed and accepted this same string as a valid,
            // self-owned `userId`. Defensive, not a real code path.
            return;
        };
        let profile = match auth.store.get_user(&user).await {
            Ok(Some(record)) => record,
            Ok(None) => return,
            Err(error) => {
                tracing::warn!(%user, %error, "could not re-read profile for room refresh fan-out");
                return;
            }
        };
        let room_ids = match rooms.rooms_joined_by_user(&user) {
            Ok(ids) => ids,
            Err(error) => {
                tracing::warn!(%user, %error, "could not list joined rooms for profile refresh");
                return;
            }
        };
        let now = now_ms();
        let semaphore = Arc::new(tokio::sync::Semaphore::new(MAX_CONCURRENT_ROOM_REFRESHES));
        let mut tasks = Vec::with_capacity(room_ids.len());
        for room_id in room_ids {
            let rooms = rooms.clone();
            let user = user.to_owned();
            let display_name = profile.display_name.clone();
            let avatar_url = profile.avatar_url.clone();
            let semaphore = semaphore.clone();
            tasks.push(tokio::spawn(async move {
                let _permit = semaphore.acquire_owned().await;
                let handle = match rooms.get_or_load(&room_id).await {
                    Ok(handle) => handle,
                    Err(error) => {
                        tracing::warn!(%room_id, %error, "could not load room for profile refresh");
                        return;
                    }
                };
                if let Err(error) = handle
                    .refresh_own_profile(user, display_name, avatar_url, now)
                    .await
                {
                    tracing::warn!(%room_id, %error, "profile refresh failed for one room");
                }
            }));
        }
        for task in tasks {
            let _ = task.await;
        }
    });
}

/// `PUT /profile/{userId}/displayname`.
// `hs_http::error::MatrixError` is a fairly large `Err` variant (it carries a message and error
// code inline rather than boxing them); every other handler in this crate returns its own
// smaller `RoomError` instead, but this one has to speak `hs_http`'s type directly to relay
// `hs-auth`'s own error unchanged. Same tradeoff `hs-appservice`'s and `hs-push`'s route modules
// already accepted for the identical reason (see their own `#[allow(clippy::result_large_err)]`).
#[allow(clippy::result_large_err)]
pub async fn put_displayname<B: KvBackend + 'static>(
    State(state): State<RoomState<B>>,
    Path(user_id): Path<String>,
    RoomRequester(requester): RoomRequester,
    PermissiveJson(body): PermissiveJson<Value>,
) -> Result<Response, MatrixError> {
    let response = hs_auth::routes::profile::put_displayname(
        State(state.auth.clone()),
        Path(user_id.clone()),
        requester,
        PermissiveJson(body),
    )
    .await
    .map_err(auth_error_to_matrix_error)?;
    spawn_refresh(state.rooms.clone(), state.auth.clone(), user_id);
    Ok(response)
}

/// `PUT /profile/{userId}/avatar_url`.
#[allow(clippy::result_large_err)] // see `put_displayname`'s identical annotation just above
pub async fn put_avatar_url<B: KvBackend + 'static>(
    State(state): State<RoomState<B>>,
    Path(user_id): Path<String>,
    RoomRequester(requester): RoomRequester,
    PermissiveJson(body): PermissiveJson<Value>,
) -> Result<Response, MatrixError> {
    let response = hs_auth::routes::profile::put_avatar_url(
        State(state.auth.clone()),
        Path(user_id.clone()),
        requester,
        PermissiveJson(body),
    )
    .await
    .map_err(auth_error_to_matrix_error)?;
    spawn_refresh(state.rooms.clone(), state.auth.clone(), user_id);
    Ok(response)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::actor::CreateRoomRequest;
    use crate::identity::HomeserverIdentity;
    use hs_auth::state::AuthState;
    use hs_auth::store::UserRecord;
    use hs_kv::memory::MemoryBackend;
    use ruma::user_id;
    use std::time::Duration;

    async fn state_with_two_members() -> (RoomState<MemoryBackend>, ruma::OwnedRoomId) {
        let backend = MemoryBackend::new();
        let identity = HomeserverIdentity::for_tests("profile-sync.test");
        let auth = AuthState::in_memory();
        let alice = user_id!("@alice:profile-sync.test").to_owned();
        let bob = user_id!("@bob:profile-sync.test").to_owned();
        auth.store
            .create_user(UserRecord::new(alice.clone(), 0))
            .await
            .unwrap();
        auth.store
            .create_user(UserRecord::new(bob.clone(), 1))
            .await
            .unwrap();
        let rooms = Arc::new(RoomRegistry::open(backend, identity.clone()).unwrap());
        let handle = rooms
            .create_room(
                alice.clone(),
                CreateRoomRequest {
                    preset: Some("public_chat".to_owned()),
                    ..Default::default()
                },
                1,
            )
            .await
            .unwrap();
        handle
            .membership(
                alice.clone(),
                crate::membership::Action::Invite,
                bob.clone(),
                serde_json::json!({}),
                2,
            )
            .await
            .unwrap();
        handle
            .membership(
                bob.clone(),
                crate::membership::Action::Join,
                bob.clone(),
                serde_json::json!({}),
                3,
            )
            .await
            .unwrap();
        let room_id = handle.query(|a| a.room_id().to_owned()).await;
        (
            RoomState {
                auth,
                rooms,
                identity,
                remote_join: None,
            },
            room_id,
        )
    }

    /// The end-to-end proof this module exists for: `PUT .../displayname` re-stamps the target's
    /// `m.room.member` event in a room they share with someone else, with the new name, while
    /// leaving every other field (`membership: join`) untouched.
    #[tokio::test]
    async fn put_displayname_refreshes_membership_in_every_joined_room() {
        let (state, room_id) = state_with_two_members().await;
        let alice = user_id!("@alice:profile-sync.test").to_owned();

        put_displayname::<MemoryBackend>(
            State(state.clone()),
            Path(alice.to_string()),
            RoomRequester(hs_auth::requester::Requester::for_user(alice.clone())),
            PermissiveJson(serde_json::json!({"displayname": "Alice In Wonderland"})),
        )
        .await
        .unwrap();

        // The fan-out is a detached background task -- give it a moment to land rather than
        // asserting immediately after the response returns.
        let handle = state.rooms.get_or_load(&room_id).await.unwrap();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            let content = handle
                .query({
                    let alice = alice.clone();
                    move |actor| {
                        actor
                            .state_event("m.room.member", alice.as_str())
                            .unwrap()
                            .map(|e| e.json().clone())
                    }
                })
                .await;
            if let Some(json) = &content {
                let displayname = json
                    .get("content")
                    .and_then(hs_model::canonical::CanonicalJsonValue::as_object)
                    .and_then(|c| c.get("displayname"))
                    .and_then(hs_model::canonical::CanonicalJsonValue::as_str);
                if displayname == Some("Alice In Wonderland") {
                    let membership = json
                        .get("content")
                        .and_then(hs_model::canonical::CanonicalJsonValue::as_object)
                        .and_then(|c| c.get("membership"))
                        .and_then(hs_model::canonical::CanonicalJsonValue::as_str);
                    assert_eq!(membership, Some("join"));
                    return;
                }
            }
            if tokio::time::Instant::now() > deadline {
                panic!("profile refresh did not land within 5s: {content:?}");
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// A rename for a user with no joined rooms at all is still accepted and does not panic or
    /// hang the background task (`rooms_joined_by_user` returns an empty list; nothing to fan
    /// out).
    #[tokio::test]
    async fn put_displayname_with_no_joined_rooms_is_a_no_op_fanout() {
        let backend = MemoryBackend::new();
        let identity = HomeserverIdentity::for_tests("profile-sync-lonely.test");
        let auth = AuthState::in_memory();
        let carol = user_id!("@carol:profile-sync-lonely.test").to_owned();
        auth.store
            .create_user(UserRecord::new(carol.clone(), 0))
            .await
            .unwrap();
        let rooms = Arc::new(RoomRegistry::open(backend, identity.clone()).unwrap());
        let state = RoomState {
            auth,
            rooms,
            identity,
            remote_join: None,
        };
        let response = put_displayname::<MemoryBackend>(
            State(state),
            Path(carol.to_string()),
            RoomRequester(hs_auth::requester::Requester::for_user(carol.clone())),
            PermissiveJson(serde_json::json!({"displayname": "Carol"})),
        )
        .await
        .unwrap();
        assert_eq!(response.status(), axum::http::StatusCode::OK);
    }

    /// Not a self-`PUT` -- `hs-auth`'s own `403` must still surface unchanged through this crate's
    /// wrapper, and no refresh should be attempted.
    #[tokio::test]
    async fn put_displayname_for_another_user_is_still_forbidden() {
        let (state, _room_id) = state_with_two_members().await;
        let alice = user_id!("@alice:profile-sync.test").to_owned();
        let bob = user_id!("@bob:profile-sync.test").to_owned();
        let err = put_displayname::<MemoryBackend>(
            State(state),
            Path(alice.to_string()),
            RoomRequester(hs_auth::requester::Requester::for_user(bob)),
            PermissiveJson(serde_json::json!({"displayname": "Not Alice"})),
        )
        .await
        .unwrap_err();
        assert_eq!(err.status, axum::http::StatusCode::FORBIDDEN);
    }
}
