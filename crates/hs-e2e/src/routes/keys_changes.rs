//! `GET /keys/changes`.
//!
//! The spec's `from`/`to` parameters are opaque `/sync` batch tokens. A plain decimal string is
//! always accepted directly, as the decimal encoding of a [`crate::store::DeviceKeyStore`] stream
//! position (this crate's own token format, and what this route originally treated as the only
//! valid shape, back when `hs-user`'s `/sync` did not exist yet to issue a real one). Now that
//! `hs-user` issues real opaque tokens (`hsu1_...`), anything that doesn't parse as a plain
//! decimal is instead handed to [`crate::state::SyncTokenResolver`] if one has been installed
//! (see that trait's doc comment for why this crate cannot just decode `hs-user`'s token format
//! itself). A resolver that also makes the membership walk between two of its tokens
//! ([`crate::state::SyncTokenResolver::device_list_changes_between`]) answers the whole request,
//! `left` included; without one, `left` is always empty. See `docs/status/08-e2ee.md`.

use axum::Json;
use axum::extract::{Query, State};
use hs_kv::KvBackend;
use ruma::UserId;
use serde::Deserialize;
use serde_json::json;

use crate::error::E2eError;
use crate::state::{E2eRequester, E2eState};

/// `?from=&to=` — `to` is optional (an absent `to` means "up to now").
#[derive(Debug, Deserialize)]
pub struct KeysChangesQuery {
    from: String,
    to: Option<String>,
}

/// Resolves `raw` to a device-list stream position: a plain decimal parses directly; anything
/// else is handed to the installed [`crate::state::SyncTokenResolver`], if any. Split out from
/// [`get_keys_changes`] (rather than inlined) so a unit test can exercise it directly against a
/// fake resolver without needing an authenticated HTTP request, the same way
/// `crate::routes::keys_query::build_keys_query_response` is split out for its own tests.
pub(crate) async fn resolve_stream_pos<B: KvBackend + 'static>(
    state: &E2eState<B>,
    user_id: &UserId,
    raw: &str,
) -> Result<u64, E2eError> {
    if let Ok(pos) = raw.parse::<u64>() {
        return Ok(pos);
    }
    if let Some(resolver) = state.sync_token_resolver()
        && let Some(pos) = resolver.resolve_device_list_position(user_id, raw).await?
    {
        return Ok(pos);
    }
    Err(E2eError::BadRequest(format!(
        "not a valid device-list stream token: {raw:?}"
    )))
}

/// `GET /keys/changes?from=...&to=...`.
pub async fn get_keys_changes<B: KvBackend + 'static>(
    State(state): State<E2eState<B>>,
    E2eRequester(requester): E2eRequester,
    Query(query): Query<KeysChangesQuery>,
) -> Result<Json<serde_json::Value>, E2eError> {
    keys_changes_answer(&state, &requester.user_id, &query.from, query.to.as_deref())
        .await
        .map(Json)
}

/// [`get_keys_changes`]'s answer for `user_id`, split out so a test can call it without an
/// authenticated request (as [`resolve_stream_pos`] is).
pub(crate) async fn keys_changes_answer<B: KvBackend + 'static>(
    state: &E2eState<B>,
    user_id: &UserId,
    from: &str,
    to: Option<&str>,
) -> Result<serde_json::Value, E2eError> {
    // The installed resolver's own answer, when it makes the membership walk between two of
    // its tokens (`hs-user` does: `crate::state::SyncTokenResolver::device_list_changes_between`).
    // That is the only way to answer `left`, and `changed` for a user who merely started sharing
    // a room, since this crate has no notion of room membership -- see `crate::appservice_feed`'s
    // doc comment for the same limitation on the appservice-facing side.
    if let Some(resolver) = state.sync_token_resolver()
        && let Some(changes) = resolver
            .device_list_changes_between(user_id, from, to)
            .await?
    {
        tracing::debug!(
            %user_id,
            changed = changes.changed.len(),
            left = changes.left.len(),
            "/keys/changes answered from the membership walk between two sync tokens"
        );
        return Ok(json!({
            "changed": changes.changed.iter().map(ToString::to_string).collect::<Vec<_>>(),
            "left": changes.left.iter().map(ToString::to_string).collect::<Vec<_>>(),
        }));
    }
    // A plain stream position (this crate's own token format), or a resolver that only decodes
    // positions: the device-list stream alone, with `left` necessarily empty.
    let from = resolve_stream_pos(state, user_id, from).await?;
    let to = match to {
        Some(raw) => Some(resolve_stream_pos(state, user_id, raw).await?),
        None => None,
    };
    let changed = state.store.changed_users_since(from, to).await?;
    Ok(json!({
        "changed": changed.into_iter().map(|u| u.to_string()).collect::<Vec<_>>(),
        "left": Vec::<String>::new(),
    }))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use hs_auth::state::AuthState;
    use hs_kv::memory::MemoryBackend;
    use ruma::user_id;

    use super::*;
    use crate::store::tables::TablesE2eStore;

    fn state() -> E2eState<MemoryBackend> {
        let store = TablesE2eStore::open(MemoryBackend::new()).unwrap();
        E2eState::new(AuthState::in_memory(), Arc::new(store))
    }

    /// A fake resolver mimicking `hs-user`'s installed one: recognizes only tokens of the shape
    /// `fake1_<n>`, resolving to stream position `n`; anything else is "not my format".
    struct FakeResolver;

    #[async_trait::async_trait]
    impl crate::state::SyncTokenResolver for FakeResolver {
        async fn resolve_device_list_position(
            &self,
            _user_id: &UserId,
            raw: &str,
        ) -> Result<Option<u64>, E2eError> {
            let Some(n) = raw.strip_prefix("fake1_") else {
                return Ok(None);
            };
            n.parse::<u64>()
                .map(Some)
                .map_err(|_| E2eError::BadRequest("bad fake token".to_string()))
        }
    }

    #[tokio::test]
    async fn plain_decimal_tokens_still_resolve_directly_with_no_resolver_installed() {
        let state = state();
        let pos = resolve_stream_pos(&state, user_id!("@alice:example.org"), "42")
            .await
            .unwrap();
        assert_eq!(pos, 42);
    }

    #[tokio::test]
    async fn opaque_token_is_rejected_when_no_resolver_is_installed() {
        let state = state();
        let err = resolve_stream_pos(&state, user_id!("@alice:example.org"), "hsu1_AQAAAA")
            .await
            .unwrap_err();
        assert!(matches!(err, E2eError::BadRequest(_)));
    }

    #[tokio::test]
    async fn opaque_token_resolves_once_a_matching_resolver_is_installed() {
        let state = state();
        state.install_sync_token_resolver(Arc::new(FakeResolver));
        let pos = resolve_stream_pos(&state, user_id!("@alice:example.org"), "fake1_7")
            .await
            .unwrap();
        assert_eq!(pos, 7);
    }

    #[tokio::test]
    async fn a_token_the_installed_resolver_does_not_recognize_is_still_rejected() {
        let state = state();
        state.install_sync_token_resolver(Arc::new(FakeResolver));
        let err = resolve_stream_pos(&state, user_id!("@alice:example.org"), "hsu1_somethingelse")
            .await
            .unwrap_err();
        assert!(matches!(err, E2eError::BadRequest(_)));
    }

    /// A resolver that also makes the membership walk: its answer is the route's, `left`
    /// included; a plain decimal still goes to the stream, where `left` is empty.
    struct WalkingResolver;

    #[async_trait::async_trait]
    impl crate::state::SyncTokenResolver for WalkingResolver {
        async fn resolve_device_list_position(
            &self,
            _user_id: &UserId,
            raw: &str,
        ) -> Result<Option<u64>, E2eError> {
            Ok(raw.strip_prefix("walk_").and_then(|n| n.parse().ok()))
        }

        async fn device_list_changes_between(
            &self,
            _user_id: &UserId,
            from: &str,
            _to: Option<&str>,
        ) -> Result<Option<crate::state::DeviceListChanges>, E2eError> {
            if !from.starts_with("walk_") {
                return Ok(None);
            }
            Ok(Some(crate::state::DeviceListChanges {
                changed: vec![ruma::user_id!("@new:example.org").to_owned()],
                left: vec![ruma::user_id!("@gone:example.org").to_owned()],
            }))
        }
    }

    #[tokio::test]
    async fn the_resolvers_walk_answers_changed_and_left_when_it_has_one() {
        let state = state();
        state.install_sync_token_resolver(Arc::new(WalkingResolver));
        let alice = user_id!("@alice:example.org");
        let answer = keys_changes_answer(&state, alice, "walk_1", Some("walk_2"))
            .await
            .unwrap();
        assert_eq!(
            answer,
            json!({"changed": ["@new:example.org"], "left": ["@gone:example.org"]})
        );

        // A plain decimal is not the walk's: the stream answers it, with `left` empty.
        state.store.record_device_list_change(alice).await.unwrap();
        let answer = keys_changes_answer(&state, alice, "0", None).await.unwrap();
        assert_eq!(
            answer,
            json!({"changed": ["@alice:example.org"], "left": []})
        );
    }

    #[tokio::test]
    async fn plain_decimal_still_wins_even_with_a_resolver_installed() {
        let state = state();
        state.install_sync_token_resolver(Arc::new(FakeResolver));
        let pos = resolve_stream_pos(&state, user_id!("@alice:example.org"), "99")
            .await
            .unwrap();
        assert_eq!(pos, 99);
    }
}
