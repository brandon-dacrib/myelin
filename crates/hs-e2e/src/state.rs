//! [`E2eState`]: this crate's axum shared state, and [`E2eRequester`], the bridge that lets a
//! handler mounted on `Router<E2eState<B>>` take [`hs_auth::requester::Requester`] as a
//! parameter.
//!
//! Follows the pattern `hs-room` established for the same problem (`crates/hs-room/src/state.rs`,
//! itself following `hs-media`'s): `hs-auth`'s `Requester: FromRequestParts<AuthState>` is
//! written against the concrete `AuthState` type, so every crate composing `hs-auth` into its own
//! state defines the same one-line bridge: embed `AuthState`, implement `FromRef`, and wrap
//! `Requester` in a newtype implementing `FromRequestParts<OwnState>` by delegating through
//! `AuthState::from_ref`.

use std::sync::{Arc, OnceLock};

use axum::extract::{FromRef, FromRequestParts};
use axum::http::request::Parts;
use hs_auth::requester::Requester;
use hs_auth::state::AuthState;
use hs_kv::KvBackend;
use ruma::UserId;

use crate::error::E2eError;
use crate::store::E2eStore;

/// A hook `GET /keys/changes` (`crate::routes::keys_changes::get_keys_changes`) calls when a
/// `from`/`to` value is not a plain decimal device-list stream position, to ask whoever mints a
/// *different* opaque token format (in this workspace, `hs-user`'s `/sync` `hsu1_...` token)
/// whether it recognizes `raw` and, if so, what device-list stream position it corresponds to.
///
/// # Why this indirection exists
///
/// The spec defines `/keys/changes`' `from`/`to` as opaque `/sync` batch tokens, but decoding one
/// needs whatever per-user bookkeeping the minting crate keeps (for `hs-user`, its own sync-token
/// encoding) — data this crate has no way to reach without depending on the crate that owns it,
/// which would be a cycle (`hs-user` already depends on `hs-e2e` for exactly the reverse reason:
/// it reads this crate's device-list/to-device/key-count store traits to populate `/sync`'s e2e
/// extensions — see `docs/rfcs/0013-e2ee-sync-extensions.md`). Defining this trait *here* and
/// letting the token-minting crate implement and [`E2eState::install_sync_token_resolver`] it at
/// construction time avoids the cycle in both directions, the same way `hs-room`'s
/// `GlobalTokenResolver` (`crates/hs-room/src/registry.rs`) solves the identical shape of problem
/// for `GET /rooms/{roomId}/messages`. A dedicated trait (rather than reusing `hs-room`'s, which
/// is parameterized on a `room_id` this crate's per-user device-list stream has no notion of) is
/// used because the resolved quantity here is a single device-list stream position, not a
/// per-room pagination position.
#[async_trait::async_trait]
pub trait SyncTokenResolver: Send + Sync {
    /// Attempts to resolve `raw` to a device-list stream position for `user_id`.
    ///
    /// Returns:
    /// - `Ok(None)` if `raw` is not shaped like a token this resolver mints at all -- the caller
    ///   should treat `raw` as an invalid token, not silently ignore the constraint.
    /// - `Ok(Some(pos))` if `raw` resolves to device-list stream position `pos`. A resolver is
    ///   free to return `Some(0)` for a token that predates its own device-list stream (i.e.
    ///   "everything since the beginning"), rather than `None` -- `None` means "not my format",
    ///   not "no information".
    /// - `Err` only for a genuine backing-store failure, never for "not my format".
    async fn resolve_device_list_position(
        &self,
        user_id: &UserId,
        raw: &str,
    ) -> Result<Option<u64>, E2eError>;
}

/// This crate's axum shared state: the e2e store and an embedded [`AuthState`] so
/// [`E2eRequester`] can authenticate requests.
#[derive(Clone)]
pub struct E2eState<B: KvBackend> {
    /// Authentication (`hs-auth`'s `AuthState`), embedded so [`AuthState`] can be produced from
    /// `&E2eState<B>` via [`FromRef`] — see the module docs.
    pub auth: AuthState,
    /// Device keys, one-time/fallback keys, cross-signing keys, backups and to-device storage.
    pub store: Arc<dyn E2eStore>,
    /// See [`SyncTokenResolver`] and [`E2eState::install_sync_token_resolver`]. `Arc`-wrapped (not
    /// a bare `OnceLock`) so every clone of this state produced by axum's per-request `Clone`
    /// shares the same cell -- installing the resolver once, on any clone, makes it visible to
    /// all of them. Unset (`None` from [`E2eState::sync_token_resolver`]) means no other crate has
    /// installed one -- `GET /keys/changes` then treats a `from`/`to` value that isn't a plain
    /// decimal stream position as invalid, same as before this hook existed.
    sync_token_resolver: Arc<OnceLock<Arc<dyn SyncTokenResolver>>>,
    /// Marker so `B` (the backend `E2eState` was constructed over) is nameable in code that
    /// otherwise only touches `store` through the trait object — kept even though `store` itself
    /// erases `B`, so `E2eState<B>: FromRequestParts` bounds line up the same way `RoomState<B>`'s
    /// do.
    _backend: std::marker::PhantomData<B>,
}

/// Implements `hs-auth`'s [`hs_auth::state::DeviceListChangeNotifier`] over this crate's own
/// [`E2eStore`], so `hs-auth`'s device rename/delete routes
/// (`crates/hs-auth/src/routes/devices.rs`) can bump this crate's device-list stream without
/// `hs-auth` depending on this crate at all. Installed once per [`E2eState::new`] call -- see
/// that constructor's doc comment.
struct AuthDeviceListNotifier {
    store: Arc<dyn E2eStore>,
}

#[async_trait::async_trait]
impl hs_auth::state::DeviceListChangeNotifier for AuthDeviceListNotifier {
    async fn notify_device_list_changed(&self, user_id: &UserId) {
        if let Err(error) = self.store.record_device_list_change(user_id).await {
            tracing::warn!(
                %user_id,
                %error,
                "could not record a device-list change from hs-auth's device routes"
            );
        }
    }
}

impl<B: KvBackend> E2eState<B> {
    /// Builds an `E2eState` around an already-open store and an existing [`AuthState`] (sharing
    /// its appservice registry, rate limiter and clock with whatever other crate's state also
    /// embeds it in the same process).
    ///
    /// As a side effect, installs this crate's [`hs_auth::state::DeviceListChangeNotifier`] onto
    /// `auth` (see [`AuthDeviceListNotifier`]) -- this is the one call site in the whole workspace
    /// that has both an [`AuthState`] and an `Arc<dyn E2eStore>` in hand at once, which is exactly
    /// what `hs-auth`'s device rename/delete routes need without `hs-auth` ever depending on this
    /// crate. `AuthState::install_device_list_notifier` is idempotent past its first call, so
    /// constructing more than one `E2eState` around clones of the *same* `AuthState` (which does
    /// not happen in `hs-cli`'s `serve.rs` today, but would in a test that built two) is safe.
    #[must_use]
    pub fn new(auth: AuthState, store: Arc<dyn E2eStore>) -> Self {
        auth.install_device_list_notifier(Arc::new(AuthDeviceListNotifier {
            store: store.clone(),
        }));
        Self {
            auth,
            store,
            sync_token_resolver: Arc::new(OnceLock::new()),
            _backend: std::marker::PhantomData,
        }
    }

    /// Installs the [`SyncTokenResolver`] `GET /keys/changes` consults for a `from`/`to` value
    /// that is not a plain decimal stream position. Idempotent past the first call: a second
    /// install is silently ignored (logged, not panicked), matching
    /// `hs_room::registry::RoomRegistry::install_global_token_resolver`'s same convention -- one
    /// state is expected to have exactly one installer for the lifetime of the process.
    pub fn install_sync_token_resolver(&self, resolver: Arc<dyn SyncTokenResolver>) {
        if self.sync_token_resolver.set(resolver).is_err() {
            tracing::warn!(
                "a sync token resolver was already installed on this e2e state; ignoring the \
                 second install"
            );
        }
    }

    /// The installed [`SyncTokenResolver`], if any.
    #[must_use]
    pub fn sync_token_resolver(&self) -> Option<&Arc<dyn SyncTokenResolver>> {
        self.sync_token_resolver.get()
    }
}

impl<B: KvBackend> FromRef<E2eState<B>> for AuthState {
    fn from_ref(state: &E2eState<B>) -> AuthState {
        state.auth.clone()
    }
}

/// Wraps [`Requester`] so it can be used as an extractor on `Router<E2eState<B>>`. See the module
/// docs.
#[derive(Debug)]
pub struct E2eRequester(pub Requester);

impl<B: KvBackend> FromRequestParts<E2eState<B>> for E2eRequester {
    type Rejection = hs_http::error::MatrixError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &E2eState<B>,
    ) -> Result<Self, Self::Rejection> {
        let auth_state = AuthState::from_ref(state);
        Requester::from_request_parts(parts, &auth_state)
            .await
            .map(E2eRequester)
            .map_err(auth_error_to_matrix_error)
    }
}

/// `hs-auth`'s `MatrixError` and `hs-http`'s `MatrixError` are two distinct types (see
/// `docs/status/07-auth-and-identity.md`'s "Interfaces needed"). This crate is built against
/// `hs-http`'s, so every place `hs-auth` hands back its own error type needs this one conversion
/// — copied from `hs-room`'s identical bridge rather than re-derived, so the two stay consistent.
fn auth_error_to_matrix_error(e: hs_auth::error::MatrixError) -> hs_http::error::MatrixError {
    hs_http::error::MatrixError::custom(
        e.status(),
        hs_http::error::MatrixErrorCode::Other(e.errcode().as_str().to_owned()),
        e.to_string(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::DeviceKeyStore;
    use hs_kv::memory::MemoryBackend;
    use ruma::user_id;

    #[test]
    fn e2e_state_is_cloneable() {
        let backend = MemoryBackend::new();
        let store = crate::store::tables::TablesE2eStore::open(backend).unwrap();
        let state: E2eState<MemoryBackend> = E2eState::new(AuthState::in_memory(), Arc::new(store));
        let _ = state.clone();
    }

    /// The whole point of `AuthDeviceListNotifier`: an `AuthState::notify_device_list_changed`
    /// call reaches this crate's own device-list stream, through nothing but the hook
    /// `E2eState::new` installs -- no direct reference to `E2eState` or this crate's store type at
    /// the call site, exactly the shape `hs-auth`'s device rename/delete routes use.
    #[tokio::test]
    async fn constructing_e2e_state_wires_auth_state_notifications_into_this_crate_store() {
        let backend = MemoryBackend::new();
        let store = Arc::new(crate::store::tables::TablesE2eStore::open(backend).unwrap());
        let auth = AuthState::in_memory();
        let _state: E2eState<MemoryBackend> = E2eState::new(auth.clone(), store.clone());

        let user = user_id!("@alice:example.org");
        assert_eq!(store.current_stream_pos().await.unwrap(), 0);

        // This is the exact call `hs-auth`'s `crate::routes::devices` handlers make -- no
        // knowledge of `hs-e2e` or its store anywhere in that call.
        auth.notify_device_list_changed(user).await;

        assert!(store.current_stream_pos().await.unwrap() > 0);
        let changed = store.changed_users_since(0, None).await.unwrap();
        assert!(changed.contains(user));
    }

    /// Without any `E2eState` ever having been constructed, `notify_device_list_changed` is a
    /// silent no-op (no installed notifier) -- confirms `hs-auth`'s device routes cannot panic or
    /// error just because nothing in the process cares about device lists.
    #[tokio::test]
    async fn notify_is_a_no_op_before_any_e2e_state_installs_a_notifier() {
        let auth = AuthState::in_memory();
        let user = user_id!("@alice:example.org");
        // Must simply return, not panic -- there is nothing else to assert here.
        auth.notify_device_list_changed(user).await;
    }
}
