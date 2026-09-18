//! Shared test scaffolding for `crate::routes`' integration-style tests: a small router over
//! `hs_kv::memory::MemoryBackend`, a helper to mint a usable access token via `hs-auth`'s
//! in-memory store, and a helper to seed a media item directly through the repository (bypassing
//! HTTP, for tests that are about something other than the upload path itself).

use std::sync::Arc;

use bytes::Bytes;
use hs_auth::state::AuthState;
use hs_auth::store::{AccessTokenRecord, UserRecord};
use hs_auth::token::TokenHash;
use hs_config::{ByteSize, MediaConfig};
use hs_kv::memory::MemoryBackend;
use object_store::ObjectStore;
use object_store::memory::InMemory;

use crate::metadata::MetadataStore;
use crate::policy::{InMemoryQuotaPolicy, UploadContext};
use crate::repository::MediaRepository;
use crate::state::MediaState;
use crate::thumbnail::ThumbnailPolicy;

/// A generous-but-bounded upload ceiling for tests that are not themselves about the size limit
/// (small enough that an intentionally oversized test body still trips it).
const TEST_MAX_UPLOAD_SIZE: u64 = 5_000;

fn build_repository(max_upload_size: u64) -> MediaRepository<MemoryBackend> {
    let object_store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
    let metadata = MetadataStore::open(MemoryBackend::new()).unwrap();
    let config = MediaConfig {
        max_upload_size: ByteSize::bytes(max_upload_size),
        ..MediaConfig::default()
    };
    MediaRepository::new(
        object_store,
        metadata,
        Arc::new(config),
        Arc::new(InMemoryQuotaPolicy::unlimited()),
        ThumbnailPolicy::default(),
        "example.org".to_string(),
        || 1_000,
    )
}

fn build_state(freeze_ms: Option<u64>) -> MediaState<MemoryBackend> {
    MediaState {
        auth: AuthState::in_memory(),
        repository: Arc::new(build_repository(TEST_MAX_UPLOAD_SIZE)),
        legacy_media_enabled: true,
        legacy_freeze_ms: freeze_ms,
    }
}

/// Builds the authenticated router over a fresh in-memory state.
pub(crate) fn router() -> (axum::Router, MediaState<MemoryBackend>) {
    let state = build_state(None);
    let (router, _manifest) = crate::router::authenticated_router::<MemoryBackend>();
    (router.with_state(state.clone()), state)
}

/// Builds the legacy router over a fresh in-memory state, with the given freeze cutover.
pub(crate) fn legacy_router(freeze_ms: Option<u64>) -> (axum::Router, MediaState<MemoryBackend>) {
    let state = build_state(freeze_ms);
    let (router, _manifest) = crate::router::legacy_router::<MemoryBackend>();
    (router.with_state(state.clone()), state)
}

/// Creates a user and a usable access token in `state`'s `hs-auth` store, returning the raw
/// bearer token string.
pub(crate) async fn seed_token(state: &MediaState<MemoryBackend>, user_id: &str) -> String {
    let uid = ruma::UserId::parse(user_id).unwrap().to_owned();
    state
        .auth
        .store
        .create_user(UserRecord::new(uid.clone(), 0))
        .await
        .unwrap();
    let token = format!("syt_test_{}", uid.localpart());
    state
        .auth
        .store
        .put_access_token(AccessTokenRecord {
            hash: TokenHash::of(&token),
            user_id: uid,
            device_id: None,
            expires_at_ms: None,
            refresh_token_hash: None,
            last_used_ms: None,
        })
        .await
        .unwrap();
    token
}

/// Uploads a fixture image directly through the repository (bypassing HTTP), returning the new
/// media ID's raw string form.
pub(crate) async fn upload_fixture(
    state: &MediaState<MemoryBackend>,
    user_id: &str,
    content_type: &str,
    filename: Option<&str>,
) -> String {
    let ctx = UploadContext {
        user_id: user_id.to_string(),
        server_name: state.repository.server_name().to_string(),
    };
    let bytes = Bytes::from(crate::test_fixtures::valid_png());
    let id = state
        .repository
        .upload(&ctx, content_type, filename.map(str::to_string), bytes)
        .await
        .unwrap();
    id.as_str().to_string()
}
