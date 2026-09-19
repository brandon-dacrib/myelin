//! `GET /keys/changes`.
//!
//! The spec's `from`/`to` parameters are opaque `/sync` batch tokens. A plain decimal string is
//! always accepted directly, as the decimal encoding of a [`crate::store::DeviceKeyStore`] stream
//! position (this crate's own token format, and what this route originally treated as the only
//! valid shape, back when `hs-user`'s `/sync` did not exist yet to issue a real one). Now that
//! `hs-user` issues real opaque tokens (`hsu1_...`), anything that doesn't parse as a plain
//! decimal is instead handed to [`crate::state::SyncTokenResolver`] if one has been installed
//! (see that trait's doc comment for why this crate cannot just decode `hs-user`'s token format
//! itself). See `docs/status/08-e2ee.md`.

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
    let from = resolve_stream_pos(&state, &requester.user_id, &query.from).await?;
    let to = match query.to.as_deref() {
        Some(raw) => Some(resolve_stream_pos(&state, &requester.user_id, raw).await?),
        None => None,
    };
    let changed = state.store.changed_users_since(from, to).await?;
    // This crate has no notion of room membership, so it cannot compute `left` (users who
    // stopped sharing an encrypted room) -- see `crate::appservice_feed`'s doc comment for the
    // same limitation on the appservice-facing side. Always empty here, documented as a seam.
    Ok(Json(json!({
        "changed": changed.into_iter().map(|u| u.to_string()).collect::<Vec<_>>(),
        "left": Vec::<String>::new(),
    })))
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
