//! [`MediaRepository`]: ties [`crate::store`] (bytes), [`crate::metadata`] (rows),
//! [`crate::policy`] (quotas) and [`crate::thumbnail`] (generation) together into the operations
//! [`crate::routes`] calls. This is the one place that knows how a synchronous upload, an
//! async-upload reservation, a download and a thumbnail request each work end to end; the HTTP
//! layer only translates requests into calls here and results into responses.

use std::sync::Arc;
use std::time::Instant;

use bytes::Bytes;
use hs_kv::KvBackend;
use object_store::path::Path as ObjectPath;
use object_store::{ObjectStore, ObjectStoreExt};

use hs_config::MediaConfig;

use crate::error::MediaError;
use crate::id::MediaId;
use crate::metadata::{MediaRecord, MetadataStore, ThumbnailRecord};
use crate::policy::{UploadContext, UploadPolicy};
use crate::scanning::config::ScanMode;
use crate::scanning::{EngineDecision, ScanContext, ScanEngine, ScanSourceKind};
use crate::security::{ByteRange, RangeOutcome, parse_range};
use crate::sniff::DecodeLimits;
use crate::thumbnail::{self, ThumbnailMethod, ThumbnailPolicy};

/// The `quarantined_by` marker `defer` mode uses while a scan is still in flight (RFC 0008
/// section 5: "the upload succeeds but the media is not retrievable... until a verdict arrives").
/// Identical in effect to an admin quarantine — [`MediaError::Quarantined`]'s doc already commits
/// to quarantined media looking exactly like not-found to a non-admin caller, which is precisely
/// the "indistinguishable from quarantine" property the brief asks defer mode to have — but a
/// distinct string so an admin (or a test) inspecting a row can tell "the system is still
/// deciding" apart from an actual admin action or a completed bad-verdict quarantine
/// ([`SCAN_QUARANTINE_MARKER`]).
const PENDING_SCAN_MARKER: &str = "system:pending-scan";

/// The `quarantined_by` marker used once a background scan (`defer`/`quarantine` mode) actually
/// decides to quarantine (a bad verdict, or a policy-blocked outcome that arrived after the
/// upload had already been accepted and cannot be literally rejected anymore — see
/// [`MediaRepository::apply_background_scan_decision`]'s doc for that case).
const SCAN_QUARANTINE_MARKER: &str = "system:scan";

/// What a scan decided a caller of [`MediaRepository::decide_scan`] should do, resolved down to
/// concrete storage actions (as opposed to [`EngineDecision`], which is `crate::scanning`'s own
/// vocabulary and does not know about quarantine markers, defer's hide-until-resolved trick, or
/// background scanning at all).
enum ScanDecision {
    /// Store `bytes`/`content_type` normally (bypassed, mode off, or a synchronous clean/replaced
    /// verdict under `block` mode).
    Store { bytes: Bytes, content_type: String },
    /// Store `bytes`/`content_type`, but immediately quarantined (a synchronous bad verdict under
    /// `block` mode from a per-reason `unscannable`/`oversize` policy set to `quarantine` rather
    /// than `block` — `block` mode's *default* bad-verdict action is `Reject`, not this, but the
    /// per-reason policy knobs are independent of `mode` and can still produce this even here).
    StoreQuarantined {
        bytes: Bytes,
        content_type: String,
        reason: String,
    },
    /// Reject outright; the caller must not store anything (`block` mode's default bad-verdict
    /// action).
    Reject(String),
    /// Store `bytes`/`content_type` as given (the original, unscanned upload) and have the caller
    /// spawn a background scan afterward (`defer`/`quarantine` mode). `hide_until_resolved` is
    /// `true` for `defer` (start quarantined under [`PENDING_SCAN_MARKER`], clear it once the
    /// background scan resolves) and `false` for `quarantine` (serve immediately; quarantine only
    /// if the background scan turns up a bad verdict).
    Background { hide_until_resolved: bool },
}

/// How long an async-upload reservation (`POST .../create`) stays valid before it expires if the
/// content is never `PUT`. The spec leaves the exact duration to the server; this matches
/// Synapse's observed default (`synapse/config/media.py`'s `unused_expiration_time`, 24 hours).
pub const DEFAULT_RESERVATION_TTL_MS: u64 = 24 * 60 * 60 * 1000;

/// Everything needed to serve a download or thumbnail response body: the record plus the actual
/// bytes (already sliced to a [`ByteRange`] if one was requested).
#[derive(Debug, Clone)]
pub struct ContentBytes {
    /// The full resource length (even for a partial response).
    pub total_len: u64,
    /// The bytes actually being returned (the whole resource, or one range of it).
    pub bytes: Bytes,
    /// `Some` if this is a partial (`206`) response.
    pub range: Option<ByteRange>,
}

/// The media repository.
#[derive(Clone)]
pub struct MediaRepository<B: KvBackend> {
    object_store: Arc<dyn ObjectStore>,
    metadata: MetadataStore<B>,
    config: Arc<MediaConfig>,
    policy: Arc<dyn UploadPolicy>,
    thumbnail_policy: ThumbnailPolicy,
    decode_limits: DecodeLimits,
    /// This homeserver's own name (used as `server_name` for locally uploaded media).
    server_name: String,
    clock: Arc<dyn Fn() -> u64 + Send + Sync>,
    /// Content scanning (`docs/rfcs/0008-content-scanning.md`), if configured. `None` behaves
    /// exactly like [`crate::scanning::config::ScanMode::Off`] (no scan point below ever
    /// constructs a [`crate::scanning::ScanContext`] or calls the engine) — kept as a distinct
    /// `Option` rather than requiring every `MediaRepository` to carry an always-off engine so
    /// `MediaRepository::new`'s existing callers (this crate's own tests, `crate::test_support`)
    /// keep compiling unchanged. See [`MediaRepository::with_scanning`].
    scanning: Option<Arc<ScanEngine<B>>>,
}

impl<B: KvBackend> MediaRepository<B> {
    /// Builds a repository from its parts. `now_ms` is injectable so tests control time (the real
    /// server passes `|| system time in ms`).
    #[must_use]
    pub fn new(
        object_store: Arc<dyn ObjectStore>,
        metadata: MetadataStore<B>,
        config: Arc<MediaConfig>,
        policy: Arc<dyn UploadPolicy>,
        thumbnail_policy: ThumbnailPolicy,
        server_name: String,
        now_ms: impl Fn() -> u64 + Send + Sync + 'static,
    ) -> Self {
        Self {
            object_store,
            metadata,
            config,
            policy,
            thumbnail_policy,
            decode_limits: DecodeLimits::default(),
            server_name,
            clock: Arc::new(now_ms),
            scanning: None,
        }
    }

    /// Attaches content scanning (`docs/rfcs/0008-content-scanning.md`). A repository with no
    /// scanning attached (the default from [`MediaRepository::new`]) behaves exactly as if `mode`
    /// were [`crate::scanning::config::ScanMode::Off`] — this builder is the only way scanning
    /// ever runs.
    #[must_use]
    pub fn with_scanning(mut self, engine: ScanEngine<B>) -> Self {
        self.scanning = Some(Arc::new(engine));
        self
    }

    fn now_ms(&self) -> u64 {
        (self.clock)()
    }

    /// This homeserver's own server name (the `server_name` recorded for anything uploaded
    /// through [`MediaRepository::upload`] or [`MediaRepository::create_reservation`]).
    #[must_use]
    pub fn server_name(&self) -> &str {
        &self.server_name
    }

    /// The configured maximum upload size, for the `/config` endpoint's `m.upload.size`
    /// capability.
    #[must_use]
    pub fn max_upload_size(&self) -> u64 {
        self.config.max_upload_size.as_u64()
    }

    /// A synchronous upload (`POST .../upload`): generates a new media ID, checks size and quota,
    /// stores the bytes, and records a completed row.
    ///
    /// # Errors
    /// [`MediaError::TooLarge`] if `bytes` exceeds `max_upload_size`; [`MediaError::QuotaExceeded`]
    /// if `ctx`'s policy check fails; [`MediaError::Store`]/[`MediaError::Metadata`] on backend
    /// failure.
    pub async fn upload(
        &self,
        ctx: &UploadContext,
        content_type: &str,
        filename: Option<String>,
        bytes: Bytes,
    ) -> Result<MediaId, MediaError> {
        self.check_size(bytes.len() as u64)?;
        self.policy.check(ctx, Some(bytes.len() as u64)).await?;

        let media_id = MediaId::generate();
        let decision = self
            .decide_scan(&media_id, content_type, bytes.clone(), ctx)
            .await?;

        let (store_bytes, store_content_type, quarantined_by, spawn_background) = match decision {
            ScanDecision::Store {
                bytes,
                content_type,
            } => (bytes, content_type, None, false),
            ScanDecision::StoreQuarantined {
                bytes,
                content_type,
                reason: _reason, // already in the audit log via `ScanEngine::evaluate`
            } => (
                bytes,
                content_type,
                Some(SCAN_QUARANTINE_MARKER.to_string()),
                false,
            ),
            ScanDecision::Reject(reason) => return Err(MediaError::RejectedByScanner(reason)),
            ScanDecision::Background {
                hide_until_resolved,
            } => {
                let marker = hide_until_resolved.then(|| PENDING_SCAN_MARKER.to_string());
                (bytes.clone(), content_type.to_string(), marker, true)
            }
        };

        let key = crate::store::content_key(&self.server_name, &media_id);
        self.object_store
            .put(&key, store_bytes.clone().into())
            .await
            .map_err(MediaError::from)?;

        let record = MediaRecord {
            server_name: self.server_name.clone(),
            media_id: media_id.as_str().to_string(),
            content_type: store_content_type.clone(),
            upload_name: filename,
            byte_length: Some(store_bytes.len() as u64),
            created_ms: self.now_ms(),
            uploader: Some(ctx.user_id.clone()),
            completed: true,
            expires_at_ms: None,
            quarantined_by,
            safe_from_quarantine: false,
        };
        self.metadata.put_media(&record)?;
        self.policy.record(ctx, store_bytes.len() as u64).await;

        if spawn_background {
            self.spawn_background_scan(media_id.clone(), store_content_type, bytes, ctx.clone());
        }

        Ok(media_id)
    }

    /// Async upload, step 1 (`POST .../create`): reserves a media ID and returns it, with no
    /// content yet. The reservation expires after [`DEFAULT_RESERVATION_TTL_MS`] unless
    /// [`MediaRepository::complete_reservation`] is called first.
    ///
    /// # Errors
    /// [`MediaError::Metadata`] on backend failure.
    ///
    /// Returns the new media ID and the reservation's expiry (milliseconds since the Unix epoch)
    /// — the spec's `POST .../create` response carries this as `unused_expires_at`.
    pub fn create_reservation(&self, ctx: &UploadContext) -> Result<(MediaId, u64), MediaError> {
        let media_id = MediaId::generate();
        let now = self.now_ms();
        let expires_at = now + DEFAULT_RESERVATION_TTL_MS;
        let record = MediaRecord {
            server_name: self.server_name.clone(),
            media_id: media_id.as_str().to_string(),
            content_type: "application/octet-stream".to_string(),
            upload_name: None,
            byte_length: None,
            created_ms: now,
            uploader: Some(ctx.user_id.clone()),
            completed: false,
            expires_at_ms: Some(expires_at),
            quarantined_by: None,
            safe_from_quarantine: false,
        };
        self.metadata.put_media(&record)?;
        Ok((media_id, expires_at))
    }

    /// Async upload, step 2 (`PUT .../upload/{server}/{mediaId}`): fills a reservation created by
    /// [`MediaRepository::create_reservation`].
    ///
    /// The spec requires this to only ever be called for media on *this* server, by the same user
    /// that reserved it, exactly once; [`crate::routes`] is responsible for checking `server_name
    /// == this server` and `ctx.user_id == record.uploader` before calling this (kept out of this
    /// method so it stays a pure "does the reservation still exist and is it still open" check,
    /// testable without an `hs-auth` `Requester` in hand).
    ///
    /// # Errors
    /// [`MediaError::NotFound`] if no such reservation exists; [`MediaError::UploadExpired`] if it
    /// expired; [`MediaError::TooLarge`]/[`MediaError::QuotaExceeded`] as for
    /// [`MediaRepository::upload`]; a plain `Err` with `completed: true` semantics is represented
    /// as [`MediaError::InvalidInput`] ("already uploaded") since re-`PUT`ing is a caller bug, not
    /// a missing/expired reservation.
    pub async fn complete_reservation(
        &self,
        ctx: &UploadContext,
        media_id: &MediaId,
        content_type: &str,
        bytes: Bytes,
    ) -> Result<(), MediaError> {
        let record = self
            .metadata
            .get_media(&self.server_name, media_id.as_str())?
            .ok_or(MediaError::NotFound)?;
        if record.completed {
            return Err(MediaError::InvalidInput(
                "this media ID has already been uploaded".into(),
            ));
        }
        if let Some(expires_at) = record.expires_at_ms
            && expires_at < self.now_ms()
        {
            return Err(MediaError::UploadExpired);
        }
        self.check_size(bytes.len() as u64)?;
        self.policy.check(ctx, Some(bytes.len() as u64)).await?;

        let decision = self
            .decide_scan(media_id, content_type, bytes.clone(), ctx)
            .await?;

        let (store_bytes, store_content_type, quarantined_by, spawn_background) = match decision {
            ScanDecision::Store {
                bytes,
                content_type,
            } => (bytes, content_type, None, false),
            ScanDecision::StoreQuarantined {
                bytes,
                content_type,
                reason: _reason,
            } => (
                bytes,
                content_type,
                Some(SCAN_QUARANTINE_MARKER.to_string()),
                false,
            ),
            ScanDecision::Reject(reason) => return Err(MediaError::RejectedByScanner(reason)),
            ScanDecision::Background {
                hide_until_resolved,
            } => {
                let marker = hide_until_resolved.then(|| PENDING_SCAN_MARKER.to_string());
                (bytes.clone(), content_type.to_string(), marker, true)
            }
        };

        let key = crate::store::content_key(&self.server_name, media_id);
        self.object_store
            .put(&key, store_bytes.clone().into())
            .await
            .map_err(MediaError::from)?;
        self.metadata.complete_upload(
            &self.server_name,
            media_id.as_str(),
            &store_content_type,
            store_bytes.len() as u64,
        )?;
        if let Some(marker) = &quarantined_by {
            self.metadata
                .set_quarantined(&self.server_name, media_id.as_str(), Some(marker))?;
        }
        self.policy.record(ctx, store_bytes.len() as u64).await;

        if spawn_background {
            self.spawn_background_scan(media_id.clone(), store_content_type, bytes, ctx.clone());
        }

        Ok(())
    }

    fn check_size(&self, len: u64) -> Result<(), MediaError> {
        let limit = self.config.max_upload_size.as_u64();
        if len > limit {
            return Err(MediaError::TooLarge { limit });
        }
        Ok(())
    }

    /// Decides what a scan point should do with `bytes` before it is stored (RFC 0008 section 4):
    /// skip scanning (no engine attached, `mode: off`, or an appservice's explicit bypass),
    /// resolve synchronously (`block` mode), or store-then-scan-in-the-background
    /// (`defer`/`quarantine` mode — see [`ScanDecision::Background`]'s doc for why both are
    /// represented the same way here even though they behave differently once the caller acts on
    /// them).
    async fn decide_scan(
        &self,
        media_id: &MediaId,
        content_type: &str,
        bytes: Bytes,
        ctx: &UploadContext,
    ) -> Result<ScanDecision, MediaError> {
        let Some(engine) = &self.scanning else {
            return Ok(ScanDecision::Store {
                bytes,
                content_type: content_type.to_string(),
            });
        };
        if engine.mode() == ScanMode::Off {
            return Ok(ScanDecision::Store {
                bytes,
                content_type: content_type.to_string(),
            });
        }

        let source = if ctx.appservice_id.is_some() {
            ScanSourceKind::Appservice
        } else {
            ScanSourceKind::Local
        };
        let scan_ctx = ScanContext {
            deadline: Instant::now() + engine.timeout(),
            uploader: Some(ctx.user_id.clone()),
            source,
            media_id: media_id.as_str().to_string(),
            server_name: self.server_name.clone(),
        };

        if let Some(appservice_id) = &ctx.appservice_id
            && engine.appservice_bypassed(appservice_id)
        {
            engine
                .record_bypass(appservice_id, &scan_ctx, self.now_ms())
                .await;
            return Ok(ScanDecision::Store {
                bytes,
                content_type: content_type.to_string(),
            });
        }

        match engine.mode() {
            ScanMode::Off => unreachable!("checked above"),
            ScanMode::Block => {
                let decision = engine
                    .evaluate(content_type, bytes.clone(), scan_ctx, self.now_ms(), true)
                    .await;
                Ok(match decision {
                    EngineDecision::Allow => ScanDecision::Store {
                        bytes,
                        content_type: content_type.to_string(),
                    },
                    EngineDecision::AllowReplaced(content) => ScanDecision::Store {
                        content_type: content
                            .content_type
                            .unwrap_or_else(|| content_type.to_string()),
                        bytes: content.bytes,
                    },
                    EngineDecision::Reject(reason) => ScanDecision::Reject(reason),
                    // `block` mode's *default* bad-verdict action is `Reject`
                    // (`mode_default_action` in `crate::scanning::engine`), but the per-
                    // `UnscannableReason`/`oversize` policy knobs are independent of `mode` and
                    // can still choose `quarantine` here; honor the operator's explicit choice
                    // rather than collapsing it into `Reject`.
                    EngineDecision::StoreQuarantined(reason) => ScanDecision::StoreQuarantined {
                        bytes,
                        content_type: content_type.to_string(),
                        reason,
                    },
                })
            }
            ScanMode::Defer => Ok(ScanDecision::Background {
                hide_until_resolved: true,
            }),
            ScanMode::Quarantine => Ok(ScanDecision::Background {
                hide_until_resolved: false,
            }),
        }
    }

    /// Runs a scan in the background, after the caller has already stored the original bytes and
    /// (for `quarantine` mode) already returned a success response to the client (`defer`/
    /// `quarantine` mode — see [`ScanDecision::Background`]).
    ///
    /// This is deliberately `tokio::spawn`, not real background-job infrastructure: nothing here
    /// persists across a process restart, is retried, or is observable except through the audit
    /// log and the row it eventually updates. A crash between "the client received a
    /// `content_uri`" and "this task resolves" leaves `defer`-mode media quarantined forever
    /// (safe, but stuck — an admin can always clear it manually) and leaves `quarantine`-mode
    /// media un-quarantined even if the verdict would have said otherwise (the same exposure
    /// window `quarantine` mode always accepts, just extended indefinitely). Both are recorded as
    /// a known limitation in `docs/status/09-media.md`; the real fix is track 03/12's
    /// background-job leasing, which this crate does not own and does not attempt to build here.
    fn spawn_background_scan(
        &self,
        media_id: MediaId,
        content_type: String,
        bytes: Bytes,
        ctx: UploadContext,
    ) {
        let Some(engine) = self.scanning.clone() else {
            return;
        };
        let repo = self.clone();
        tokio::spawn(async move {
            let source = if ctx.appservice_id.is_some() {
                ScanSourceKind::Appservice
            } else {
                ScanSourceKind::Local
            };
            let scan_ctx = ScanContext {
                deadline: Instant::now() + engine.timeout(),
                uploader: Some(ctx.user_id.clone()),
                source,
                media_id: media_id.as_str().to_string(),
                server_name: repo.server_name.clone(),
            };
            let now = repo.now_ms();
            let decision = engine
                .evaluate(&content_type, bytes, scan_ctx, now, true)
                .await;
            repo.apply_background_scan_decision(&media_id, decision)
                .await;
        });
    }

    /// Applies a background scan's decision to an already-stored media item: clears `defer`
    /// mode's pending-scan marker on [`EngineDecision::Allow`], overwrites the stored bytes and
    /// row on [`EngineDecision::AllowReplaced`], and quarantines on anything bad. Backend errors
    /// are logged, not propagated — by the time this runs, there is no request left to return
    /// them to.
    async fn apply_background_scan_decision(&self, media_id: &MediaId, decision: EngineDecision) {
        let server_name = self.server_name.clone();
        match decision {
            EngineDecision::Allow => {
                if let Err(e) = self
                    .metadata
                    .set_quarantined(&server_name, media_id.as_str(), None)
                {
                    tracing::error!(
                        error = %e,
                        media_id = %media_id.as_str(),
                        "background scan: failed to clear pending-scan quarantine"
                    );
                }
            }
            EngineDecision::AllowReplaced(content) => {
                let key = crate::store::content_key(&server_name, media_id);
                let content_type = content
                    .content_type
                    .unwrap_or_else(|| "application/octet-stream".to_string());
                let byte_length = content.bytes.len() as u64;
                if let Err(e) = self
                    .object_store
                    .put(&key, content.bytes.into())
                    .await
                    .map_err(MediaError::from)
                {
                    tracing::error!(
                        error = %e,
                        media_id = %media_id.as_str(),
                        "background scan: failed to store replaced content"
                    );
                    return;
                }
                if let Err(e) = self.metadata.update_content_type_and_length(
                    &server_name,
                    media_id.as_str(),
                    &content_type,
                    byte_length,
                ) {
                    tracing::error!(
                        error = %e,
                        media_id = %media_id.as_str(),
                        "background scan: failed to update replaced content's row"
                    );
                }
                if let Err(e) = self
                    .metadata
                    .set_quarantined(&server_name, media_id.as_str(), None)
                {
                    tracing::error!(
                        error = %e,
                        media_id = %media_id.as_str(),
                        "background scan: failed to clear pending-scan quarantine"
                    );
                }
            }
            EngineDecision::Reject(reason) | EngineDecision::StoreQuarantined(reason) => {
                // `Reject` should not be reachable for `defer`/`quarantine` mode's *default*
                // bad-verdict action (`mode_default_action` in `crate::scanning::engine` always
                // resolves an infected/scanner-error outcome to `Quarantine` for these modes),
                // but a per-reason `unscannable`/`oversize` policy knob set to `block` can still
                // produce it here — by this point the content is already stored and a
                // `content_uri` may already be in a client's hands, so there is no way to
                // literally reject it anymore; quarantining is the closest safe substitute. See
                // this method's doc and `docs/status/09-media.md`.
                let _ = reason; // already in the audit log via `ScanEngine::evaluate`
                if let Err(e) = self.metadata.set_quarantined(
                    &server_name,
                    media_id.as_str(),
                    Some(SCAN_QUARANTINE_MARKER),
                ) {
                    tracing::error!(
                        error = %e,
                        media_id = %media_id.as_str(),
                        "background scan: failed to quarantine"
                    );
                }
            }
        }
    }

    /// Looks up a media row for download/thumbnail purposes, applying the "quarantined looks like
    /// not-found" and "not-yet-uploaded"/"expired" rules uniformly.
    ///
    /// # Errors
    /// [`MediaError::NotFound`], [`MediaError::NotYetUploaded`], [`MediaError::UploadExpired`], or
    /// a backend [`MediaError::Metadata`].
    pub fn get_record(&self, server_name: &str, media_id: &str) -> Result<MediaRecord, MediaError> {
        let record = self
            .metadata
            .get_media(server_name, media_id)?
            .ok_or(MediaError::NotFound)?;
        if record.quarantined_by.is_some() && !record.safe_from_quarantine {
            return Err(MediaError::Quarantined);
        }
        if !record.completed {
            if let Some(expires_at) = record.expires_at_ms
                && expires_at < self.now_ms()
            {
                return Err(MediaError::UploadExpired);
            }
            return Err(MediaError::NotYetUploaded);
        }
        Ok(record)
    }

    /// Fetches content bytes for a download, honoring an HTTP `Range` header.
    ///
    /// # Errors
    /// [`MediaError::RangeNotSatisfiable`] if `range_header` names an unsatisfiable range;
    /// otherwise as [`MediaRepository::get_record`] plus [`MediaError::Store`] on object-store
    /// failure.
    pub async fn get_content(
        &self,
        record: &MediaRecord,
        range_header: Option<&str>,
    ) -> Result<ContentBytes, MediaError> {
        let key =
            crate::store::content_key(&record.server_name, &parse_media_id(&record.media_id)?);
        let total = record.byte_length.unwrap_or(0);
        match parse_range(range_header, total) {
            RangeOutcome::Full => {
                let bytes = self
                    .object_store
                    .get(&key)
                    .await
                    .map_err(MediaError::from)?
                    .bytes()
                    .await
                    .map_err(MediaError::from)?;
                Ok(ContentBytes {
                    total_len: total,
                    bytes,
                    range: None,
                })
            }
            RangeOutcome::Partial(r) => {
                let bytes = self
                    .object_store
                    .get_range(&key, r.start..(r.end + 1))
                    .await
                    .map_err(MediaError::from)?;
                Ok(ContentBytes {
                    total_len: total,
                    bytes,
                    range: Some(r),
                })
            }
            RangeOutcome::Unsatisfiable => Err(MediaError::RangeNotSatisfiable),
        }
    }

    /// Fetches (generating and caching if necessary) one thumbnail variant.
    ///
    /// # Errors
    /// [`MediaError::UnsupportedThumbnail`] if `(width, height, method)` is not allowed by this
    /// repository's [`ThumbnailPolicy`]; otherwise as [`MediaRepository::get_record`] plus
    /// [`MediaError::DecodeFailed`]/[`MediaError::Store`].
    pub async fn get_thumbnail(
        &self,
        record: &MediaRecord,
        width: u32,
        height: u32,
        method: ThumbnailMethod,
    ) -> Result<(ThumbnailRecord, Bytes), MediaError> {
        if !self.thumbnail_policy.allows(width, height, method) {
            return Err(MediaError::UnsupportedThumbnail);
        }
        let media_id = parse_media_id(&record.media_id)?;

        // Try each format already cached for this exact (width, height, method): thumbnails are
        // always PNG or JPEG (`crate::thumbnail::output_format_for`), so two lookups cover it.
        for content_type in ["image/png", "image/jpeg"] {
            if let Some(cached) = self.metadata.get_thumbnail(
                &record.server_name,
                &record.media_id,
                width,
                height,
                method,
                content_type,
            )? {
                let variant = ThumbnailRecord::variant_key(width, height, method, content_type);
                let key = crate::store::thumbnail_key(&record.server_name, &media_id, &variant);
                let bytes = self
                    .object_store
                    .get(&key)
                    .await
                    .map_err(MediaError::from)?
                    .bytes()
                    .await
                    .map_err(MediaError::from)?;
                return Ok((cached, bytes));
            }
        }

        // Not cached: fetch the source, generate, store, record.
        let source_key = crate::store::content_key(&record.server_name, &media_id);
        let source_bytes = self
            .object_store
            .get(&source_key)
            .await
            .map_err(MediaError::from)?
            .bytes()
            .await
            .map_err(MediaError::from)?;
        let (thumb_bytes, mime) =
            thumbnail::generate(&source_bytes, width, height, method, self.decode_limits)?;

        let variant = ThumbnailRecord::variant_key(width, height, method, mime);
        let key = crate::store::thumbnail_key(&record.server_name, &media_id, &variant);
        self.object_store
            .put(&key, Bytes::from(thumb_bytes.clone()).into())
            .await
            .map_err(MediaError::from)?;

        let thumb_record = ThumbnailRecord {
            width,
            height,
            method,
            content_type: mime.to_string(),
            byte_length: thumb_bytes.len() as u64,
            created_ms: self.now_ms(),
        };
        self.metadata
            .put_thumbnail(&record.server_name, &record.media_id, &thumb_record)?;

        Ok((thumb_record, Bytes::from(thumb_bytes)))
    }

    /// Sets or clears quarantine on a media item (admin operation; `crate::routes` is responsible
    /// for checking `Requester::is_admin` before calling this).
    ///
    /// # Errors
    /// [`MediaError::NotFound`] if no such item exists.
    pub fn set_quarantined(
        &self,
        server_name: &str,
        media_id: &str,
        by: Option<&str>,
    ) -> Result<(), MediaError> {
        if self.metadata.set_quarantined(server_name, media_id, by)? {
            Ok(())
        } else {
            Err(MediaError::NotFound)
        }
    }

    /// Direct access to the object store, for callers (the Synapse importer, admin tooling) that
    /// need to write content this repository did not itself receive through
    /// [`MediaRepository::upload`] (an already-known key and bytes, imported verbatim).
    #[must_use]
    pub fn object_store(&self) -> &Arc<dyn ObjectStore> {
        &self.object_store
    }

    /// Direct access to the metadata store, for the same reason.
    #[must_use]
    pub fn metadata(&self) -> &MetadataStore<B> {
        &self.metadata
    }
}

fn parse_media_id(s: &str) -> Result<MediaId, MediaError> {
    MediaId::parse(s).map_err(|e| MediaError::Metadata(format!("stored an invalid media id: {e}")))
}

/// The object-store key for arbitrary content under `server_name`/`media_id` — re-exported at
/// this module's surface for callers that already have a [`MediaId`] and just need the same
/// sharding scheme [`MediaRepository`] itself uses (the Synapse importer, most notably).
#[must_use]
pub fn content_object_key(server_name: &str, media_id: &MediaId) -> ObjectPath {
    crate::store::content_key(server_name, media_id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use hs_kv::memory::MemoryBackend;
    use object_store::memory::InMemory;

    fn repo() -> MediaRepository<MemoryBackend> {
        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let metadata = MetadataStore::open(MemoryBackend::new()).unwrap();
        MediaRepository::new(
            store,
            metadata,
            Arc::new(MediaConfig::default()),
            Arc::new(crate::policy::InMemoryQuotaPolicy::unlimited()),
            ThumbnailPolicy::default(),
            "example.org".to_string(),
            || 1_000_000,
        )
    }

    fn ctx() -> UploadContext {
        UploadContext {
            user_id: "@alice:example.org".into(),
            server_name: "example.org".into(),
            appservice_id: None,
        }
    }

    #[tokio::test]
    async fn synchronous_upload_round_trips() {
        let repo = repo();
        let bytes = Bytes::from(crate::test_fixtures::valid_png());
        let id = repo
            .upload(&ctx(), "image/png", Some("cat.png".into()), bytes.clone())
            .await
            .unwrap();

        let record = repo.get_record("example.org", id.as_str()).unwrap();
        assert!(record.is_servable());
        assert_eq!(record.content_type, "image/png");

        let content = repo.get_content(&record, None).await.unwrap();
        assert_eq!(content.bytes, bytes);
        assert_eq!(content.total_len, bytes.len() as u64);
        assert!(content.range.is_none());
    }

    #[tokio::test]
    async fn upload_over_the_size_limit_is_rejected() {
        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let metadata = MetadataStore::open(MemoryBackend::new()).unwrap();
        let config = MediaConfig {
            max_upload_size: hs_config::ByteSize::bytes(0),
            ..MediaConfig::default()
        };
        let repo = MediaRepository::new(
            store,
            metadata,
            Arc::new(config),
            Arc::new(crate::policy::InMemoryQuotaPolicy::unlimited()),
            ThumbnailPolicy::default(),
            "example.org".to_string(),
            || 0,
        );
        let err = repo
            .upload(&ctx(), "image/png", None, Bytes::from_static(b"abc"))
            .await
            .unwrap_err();
        assert!(matches!(err, MediaError::TooLarge { .. }));
    }

    #[tokio::test]
    async fn range_request_returns_a_slice() {
        let repo = repo();
        let bytes = Bytes::from_static(b"0123456789");
        let id = repo
            .upload(&ctx(), "text/plain", None, bytes)
            .await
            .unwrap();
        let record = repo.get_record("example.org", id.as_str()).unwrap();

        let partial = repo.get_content(&record, Some("bytes=2-4")).await.unwrap();
        assert_eq!(partial.bytes, Bytes::from_static(b"234"));
        assert_eq!(partial.range.unwrap().start, 2);
    }

    #[tokio::test]
    async fn async_upload_lifecycle() {
        let repo = repo();
        let (id, _expires_at) = repo.create_reservation(&ctx()).unwrap();

        // Not yet uploaded.
        let err = repo.get_record("example.org", id.as_str()).unwrap_err();
        assert!(matches!(err, MediaError::NotYetUploaded));

        repo.complete_reservation(
            &ctx(),
            &id,
            "image/png",
            Bytes::from(crate::test_fixtures::valid_png()),
        )
        .await
        .unwrap();

        let record = repo.get_record("example.org", id.as_str()).unwrap();
        assert!(record.is_servable());
        assert_eq!(record.content_type, "image/png");
    }

    #[tokio::test]
    async fn completing_an_unknown_reservation_is_not_found() {
        let repo = repo();
        let fake = MediaId::generate();
        let err = repo
            .complete_reservation(&ctx(), &fake, "image/png", Bytes::from_static(b"x"))
            .await
            .unwrap_err();
        assert!(matches!(err, MediaError::NotFound));
    }

    #[tokio::test]
    async fn completing_twice_is_rejected() {
        let repo = repo();
        let (id, _expires_at) = repo.create_reservation(&ctx()).unwrap();
        repo.complete_reservation(&ctx(), &id, "image/png", Bytes::from_static(b"x"))
            .await
            .unwrap();
        let err = repo
            .complete_reservation(&ctx(), &id, "image/png", Bytes::from_static(b"y"))
            .await
            .unwrap_err();
        assert!(matches!(err, MediaError::InvalidInput(_)));
    }

    #[tokio::test]
    async fn expired_reservation_cannot_be_completed() {
        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let metadata = MetadataStore::open(MemoryBackend::new()).unwrap();
        // A clock that starts at 0 (so the reservation's expiry is computed relative to 0) and
        // is advanced past `DEFAULT_RESERVATION_TTL_MS` before the completion attempt.
        let now = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let now_for_clock = now.clone();
        let repo = MediaRepository::new(
            store,
            metadata,
            Arc::new(MediaConfig::default()),
            Arc::new(crate::policy::InMemoryQuotaPolicy::unlimited()),
            ThumbnailPolicy::default(),
            "example.org".to_string(),
            move || now_for_clock.load(std::sync::atomic::Ordering::SeqCst),
        );
        let (id, expires_at) = repo.create_reservation(&ctx()).unwrap();
        assert_eq!(expires_at, DEFAULT_RESERVATION_TTL_MS);

        now.store(
            DEFAULT_RESERVATION_TTL_MS + 1,
            std::sync::atomic::Ordering::SeqCst,
        );
        let err = repo
            .complete_reservation(&ctx(), &id, "image/png", Bytes::from_static(b"x"))
            .await
            .unwrap_err();
        assert!(matches!(err, MediaError::UploadExpired));
    }

    #[tokio::test]
    async fn thumbnail_is_generated_then_cached() {
        let repo = repo();
        let png = crate::test_fixtures::valid_png();
        let id = repo
            .upload(&ctx(), "image/png", None, Bytes::from(png))
            .await
            .unwrap();
        let record = repo.get_record("example.org", id.as_str()).unwrap();

        let (thumb1, bytes1) = repo
            .get_thumbnail(&record, 32, 32, ThumbnailMethod::Crop)
            .await
            .unwrap();
        assert_eq!(thumb1.width, 32);
        assert!(!bytes1.is_empty());

        let (thumb2, bytes2) = repo
            .get_thumbnail(&record, 32, 32, ThumbnailMethod::Crop)
            .await
            .unwrap();
        assert_eq!(thumb1, thumb2);
        assert_eq!(bytes1, bytes2);
    }

    #[tokio::test]
    async fn unconfigured_thumbnail_size_is_rejected_by_default() {
        let repo = repo();
        let png = crate::test_fixtures::valid_png();
        let id = repo
            .upload(&ctx(), "image/png", None, Bytes::from(png))
            .await
            .unwrap();
        let record = repo.get_record("example.org", id.as_str()).unwrap();
        let err = repo
            .get_thumbnail(&record, 123, 123, ThumbnailMethod::Crop)
            .await
            .unwrap_err();
        assert!(matches!(err, MediaError::UnsupportedThumbnail));
    }

    #[tokio::test]
    async fn quarantine_hides_content_but_admin_can_still_flag_it() {
        let repo = repo();
        let png = crate::test_fixtures::valid_png();
        let id = repo
            .upload(&ctx(), "image/png", None, Bytes::from(png))
            .await
            .unwrap();
        repo.set_quarantined("example.org", id.as_str(), Some("@admin:example.org"))
            .unwrap();
        let err = repo.get_record("example.org", id.as_str()).unwrap_err();
        assert!(matches!(err, MediaError::Quarantined));
    }

    #[tokio::test]
    async fn not_found_for_unknown_media() {
        let repo = repo();
        let err = repo.get_record("example.org", "doesnotexist").unwrap_err();
        assert!(matches!(err, MediaError::NotFound));
    }
}

/// Integration tests for `crate::scanning::ScanEngine` wired into
/// [`MediaRepository::upload`]/[`MediaRepository::complete_reservation`] (RFC 0008 section 4,
/// points 1 and 2) — through the real upload path, not against the engine in isolation (that
/// coverage already lives in `crate::scanning::engine::tests`). A small local `FixedVerdict`
/// fake provider stands in for a real scanner (`icap`/`http` are already covered independently by
/// their own provider tests), the same pattern `crate::scanning::engine::tests::FakeScanner` used
/// for the engine's own tests.
#[cfg(test)]
mod scanning_integration {
    use super::*;
    use crate::scanning::audit::{AuditKind, InMemoryAuditSink};
    use crate::scanning::cache::VerdictCache;
    use crate::scanning::config::{FailPolicy, ProviderKind, ScanMode};
    use crate::scanning::metrics::ScanMetrics;
    use crate::scanning::types::{AdaptedContent, ContentScanner, ScanError, ScanSource, Verdict};
    use crate::scanning::{ScanContext, ScanEngine, ScanningConfig};
    use async_trait::async_trait;
    use hs_kv::memory::MemoryBackend;
    use object_store::ObjectStore;
    use object_store::memory::InMemory;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    fn repo() -> MediaRepository<MemoryBackend> {
        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let metadata = MetadataStore::open(MemoryBackend::new()).unwrap();
        MediaRepository::new(
            store,
            metadata,
            Arc::new(MediaConfig::default()),
            Arc::new(crate::policy::InMemoryQuotaPolicy::unlimited()),
            ThumbnailPolicy::default(),
            "example.org".to_string(),
            || 1_000_000,
        )
    }

    fn ctx() -> UploadContext {
        UploadContext {
            user_id: "@alice:example.org".into(),
            server_name: "example.org".into(),
            appservice_id: None,
        }
    }

    /// A fake provider returning a fixed verdict every time, counting calls so a test can prove
    /// it was (or was never) actually reached — the same proof
    /// `crate::scanning::engine::tests::encrypted_content_is_never_reported_clean_...` relies on.
    struct FixedVerdict {
        id: &'static str,
        verdict: Verdict,
        calls: AtomicUsize,
    }

    impl FixedVerdict {
        fn new(id: &'static str, verdict: Verdict) -> Self {
            Self {
                id,
                verdict,
                calls: AtomicUsize::new(0),
            }
        }
    }

    #[async_trait]
    impl ContentScanner for FixedVerdict {
        fn id(&self) -> &str {
            self.id
        }
        async fn engine_version(&self) -> Option<String> {
            Some("v1".to_string())
        }
        async fn scan(
            &self,
            _content: ScanSource<'_>,
            _ctx: &ScanContext,
        ) -> Result<Verdict, ScanError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(self.verdict.clone())
        }
    }

    /// A fake provider that always times out, for the failure-policy tests.
    struct AlwaysTimesOut;

    #[async_trait]
    impl ContentScanner for AlwaysTimesOut {
        fn id(&self) -> &str {
            "always-times-out"
        }
        async fn engine_version(&self) -> Option<String> {
            None
        }
        async fn scan(
            &self,
            _content: ScanSource<'_>,
            _ctx: &ScanContext,
        ) -> Result<Verdict, ScanError> {
            Err(ScanError::Timeout)
        }
    }

    fn engine_from_config(
        config: ScanningConfig,
        scanner: Arc<dyn ContentScanner>,
    ) -> (ScanEngine<MemoryBackend>, Arc<InMemoryAuditSink>) {
        let audit = Arc::new(InMemoryAuditSink::new(100));
        let cache = VerdictCache::open(MemoryBackend::new(), &config.cache).unwrap();
        let engine = ScanEngine::from_parts(
            config,
            scanner,
            cache,
            Arc::new(ScanMetrics::standalone()),
            audit.clone(),
        );
        (engine, audit)
    }

    fn block_closed(
        scanner: Arc<dyn ContentScanner>,
    ) -> (ScanEngine<MemoryBackend>, Arc<InMemoryAuditSink>) {
        engine_from_config(
            ScanningConfig {
                mode: ScanMode::Block,
                provider: ProviderKind::Icap, // irrelevant: `from_parts` never calls `providers::build`
                fail: Some(FailPolicy::Closed),
                ..ScanningConfig::default()
            },
            scanner,
        )
    }

    /// Polls `cond` until it is true or ~1 second has elapsed, for asserting on a
    /// `tokio::spawn`-ed background scan's effect (`defer`/`quarantine` mode) without a fixed
    /// sleep. Panics if `cond` never becomes true in time.
    async fn wait_until(mut cond: impl FnMut() -> bool) {
        for _ in 0..200 {
            if cond() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!("condition was not met within the test's wait budget");
    }

    #[tokio::test]
    async fn clean_upload_is_retrievable() {
        let scanner = Arc::new(FixedVerdict::new("fake", Verdict::Clean));
        let (engine, _audit) = block_closed(scanner);
        let repo = repo().with_scanning(engine);

        let bytes = Bytes::from(crate::test_fixtures::valid_png());
        let id = repo
            .upload(&ctx(), "image/png", None, bytes.clone())
            .await
            .unwrap();

        let record = repo.get_record("example.org", id.as_str()).unwrap();
        assert!(record.is_servable());
        let content = repo.get_content(&record, None).await.unwrap();
        assert_eq!(content.bytes, bytes);
    }

    #[tokio::test]
    async fn infected_upload_is_rejected_under_block_mode() {
        let scanner = Arc::new(FixedVerdict::new(
            "fake",
            Verdict::Infected {
                signature: "Eicar-Test-Signature".into(),
                details: None,
            },
        ));
        let (engine, _audit) = block_closed(scanner);
        let repo = repo().with_scanning(engine);

        let bytes = Bytes::from(crate::test_fixtures::valid_png());
        let err = repo
            .upload(&ctx(), "image/png", None, bytes)
            .await
            .unwrap_err();
        assert!(matches!(err, MediaError::RejectedByScanner(_)));
    }

    #[tokio::test]
    async fn infected_upload_is_quarantined_under_quarantine_mode() {
        let scanner = Arc::new(FixedVerdict::new(
            "fake",
            Verdict::Infected {
                signature: "Eicar-Test-Signature".into(),
                details: None,
            },
        ));
        let (engine, audit) = engine_from_config(
            ScanningConfig {
                mode: ScanMode::Quarantine,
                provider: ProviderKind::Icap,
                fail: Some(FailPolicy::Closed),
                ..ScanningConfig::default()
            },
            scanner,
        );
        let repo = repo().with_scanning(engine);

        let bytes = Bytes::from(crate::test_fixtures::valid_png());
        let id = repo.upload(&ctx(), "image/png", None, bytes).await.unwrap();

        // `quarantine` mode's defining property: served immediately, the scan runs after.
        let record = repo.get_record("example.org", id.as_str()).unwrap();
        assert!(record.is_servable());

        wait_until(|| repo.get_record("example.org", id.as_str()).is_err()).await;
        let err = repo.get_record("example.org", id.as_str()).unwrap_err();
        assert!(matches!(err, MediaError::Quarantined));
        assert_eq!(audit.recent(10).len(), 1);
    }

    #[tokio::test]
    async fn defer_mode_hides_the_upload_until_a_clean_verdict_arrives() {
        let scanner = Arc::new(FixedVerdict::new("fake", Verdict::Clean));
        let (engine, _audit) = engine_from_config(
            ScanningConfig {
                mode: ScanMode::Defer,
                provider: ProviderKind::Icap,
                fail: Some(FailPolicy::Closed),
                ..ScanningConfig::default()
            },
            scanner,
        );
        let repo = repo().with_scanning(engine);

        let bytes = Bytes::from(crate::test_fixtures::valid_png());
        let id = repo.upload(&ctx(), "image/png", None, bytes).await.unwrap();

        // `defer` mode's defining property, distinct from `quarantine`: not retrievable yet, and
        // indistinguishable from quarantined/not-found to the caller.
        let err = repo.get_record("example.org", id.as_str()).unwrap_err();
        assert!(matches!(err, MediaError::Quarantined));

        wait_until(|| repo.get_record("example.org", id.as_str()).is_ok()).await;
        let record = repo.get_record("example.org", id.as_str()).unwrap();
        assert!(record.is_servable());
    }

    #[tokio::test]
    async fn defer_mode_stays_hidden_on_a_bad_verdict() {
        let scanner = Arc::new(FixedVerdict::new(
            "fake",
            Verdict::Infected {
                signature: "Eicar-Test-Signature".into(),
                details: None,
            },
        ));
        let (engine, _audit) = engine_from_config(
            ScanningConfig {
                mode: ScanMode::Defer,
                provider: ProviderKind::Icap,
                fail: Some(FailPolicy::Closed),
                ..ScanningConfig::default()
            },
            scanner,
        );
        let repo = repo().with_scanning(engine);

        let bytes = Bytes::from(crate::test_fixtures::valid_png());
        let id = repo.upload(&ctx(), "image/png", None, bytes).await.unwrap();

        // Give the background scan a chance to run, then assert it is still (still, not newly)
        // quarantined.
        tokio::time::sleep(Duration::from_millis(50)).await;
        let err = repo.get_record("example.org", id.as_str()).unwrap_err();
        assert!(matches!(err, MediaError::Quarantined));
    }

    #[tokio::test]
    async fn quarantine_mode_applies_a_replacement_from_the_background_scan() {
        // The background path (`MediaRepository::apply_background_scan_decision`) has its own
        // `AllowReplaced` branch, distinct from the synchronous one `block` mode exercises
        // (`replacement_is_applied_at_upload_when_enabled`) -- this proves it independently:
        // stored bytes and the row's `content_type`/`byte_length` both end up reflecting the
        // replacement once the background scan resolves, not just the immediately-served
        // (pre-scan) original.
        let scanner = Arc::new(FixedVerdict::new("icap", replaced_verdict()));
        let (engine, _audit) = engine_from_config(
            ScanningConfig {
                mode: ScanMode::Quarantine,
                provider: ProviderKind::Icap,
                fail: Some(FailPolicy::Closed),
                allow_replacement: true,
                ..ScanningConfig::default()
            },
            scanner,
        );
        let repo = repo().with_scanning(engine);

        let bytes = Bytes::from(crate::test_fixtures::valid_png());
        let id = repo
            .upload(&ctx(), "image/jpeg", None, bytes)
            .await
            .unwrap();

        wait_until(|| {
            repo.get_record("example.org", id.as_str())
                .map(|r| r.byte_length == Some(b"exif-stripped-bytes".len() as u64))
                .unwrap_or(false)
        })
        .await;

        let record = repo.get_record("example.org", id.as_str()).unwrap();
        assert!(record.is_servable());
        let content = repo.get_content(&record, None).await.unwrap();
        assert_eq!(content.bytes.as_ref(), b"exif-stripped-bytes");
    }

    #[tokio::test]
    async fn scanner_timeout_is_rejected_under_fail_closed() {
        let (engine, _audit) = engine_from_config(
            ScanningConfig {
                mode: ScanMode::Block,
                provider: ProviderKind::Icap,
                fail: Some(FailPolicy::Closed),
                ..ScanningConfig::default()
            },
            Arc::new(AlwaysTimesOut),
        );
        let repo = repo().with_scanning(engine);

        let bytes = Bytes::from(crate::test_fixtures::valid_png());
        let err = repo
            .upload(&ctx(), "image/png", None, bytes)
            .await
            .unwrap_err();
        assert!(matches!(err, MediaError::RejectedByScanner(_)));
    }

    #[tokio::test]
    async fn scanner_timeout_is_allowed_under_fail_open() {
        let (engine, _audit) = engine_from_config(
            ScanningConfig {
                mode: ScanMode::Block,
                provider: ProviderKind::Icap,
                fail: Some(FailPolicy::Open),
                ..ScanningConfig::default()
            },
            Arc::new(AlwaysTimesOut),
        );
        let repo = repo().with_scanning(engine);

        let bytes = Bytes::from(crate::test_fixtures::valid_png());
        let id = repo.upload(&ctx(), "image/png", None, bytes).await.unwrap();
        let record = repo.get_record("example.org", id.as_str()).unwrap();
        assert!(record.is_servable());
    }

    #[tokio::test]
    async fn encrypted_upload_is_accepted_and_never_reported_clean() {
        // A provider that would say "clean" to anything -- proves the encrypted short-circuit in
        // `crate::scanning::engine::ScanEngine::evaluate` runs before this fake is ever reached,
        // through the real upload path (not just `evaluate` called directly, as
        // `crate::scanning::engine::tests` already covers).
        let scanner = Arc::new(FixedVerdict::new("always-clean", Verdict::Clean));
        let (engine, _audit) = block_closed(scanner.clone());
        let repo = repo().with_scanning(engine);

        let bytes = Bytes::from_static(b"totally-opaque-ciphertext");
        let id = repo
            .upload(&ctx(), "application/octet-stream", None, bytes)
            .await
            .unwrap();
        let record = repo.get_record("example.org", id.as_str()).unwrap();
        assert!(record.is_servable());
        assert_eq!(scanner.calls.load(Ordering::SeqCst), 0);
    }

    fn replaced_verdict() -> Verdict {
        Verdict::Replaced {
            content: AdaptedContent {
                bytes: Bytes::from_static(b"exif-stripped-bytes"),
                content_type: None,
            },
            by: "icap".to_string(),
            reason: Some("exif-stripped".to_string()),
        }
    }

    #[tokio::test]
    async fn replacement_is_applied_at_upload_when_enabled() {
        let scanner = Arc::new(FixedVerdict::new("icap", replaced_verdict()));
        let (engine, audit) = engine_from_config(
            ScanningConfig {
                mode: ScanMode::Block,
                provider: ProviderKind::Icap,
                fail: Some(FailPolicy::Closed),
                allow_replacement: true,
                ..ScanningConfig::default()
            },
            scanner,
        );
        let repo = repo().with_scanning(engine);

        let bytes = Bytes::from(crate::test_fixtures::valid_png());
        let id = repo
            .upload(&ctx(), "image/jpeg", None, bytes)
            .await
            .unwrap();

        let record = repo.get_record("example.org", id.as_str()).unwrap();
        assert_eq!(
            record.byte_length,
            Some(b"exif-stripped-bytes".len() as u64)
        );
        let content = repo.get_content(&record, None).await.unwrap();
        assert_eq!(content.bytes.as_ref(), b"exif-stripped-bytes");

        let entries = audit.recent(10);
        assert!(
            entries
                .iter()
                .any(|e| matches!(e.kind, AuditKind::ReplacementApplied { .. }))
        );
    }

    #[tokio::test]
    async fn replacement_is_refused_when_allow_replacement_is_false() {
        let scanner = Arc::new(FixedVerdict::new("icap", replaced_verdict()));
        // `allow_replacement` defaults to `false`.
        let (engine, _audit) = block_closed(scanner);
        let repo = repo().with_scanning(engine);

        let bytes = Bytes::from(crate::test_fixtures::valid_png());
        // Refused -> treated as a scanner error -> fail: closed -> reject.
        let err = repo
            .upload(&ctx(), "image/jpeg", None, bytes)
            .await
            .unwrap_err();
        assert!(matches!(err, MediaError::RejectedByScanner(_)));
    }

    #[tokio::test]
    async fn async_upload_completion_is_scanned_too() {
        let scanner = Arc::new(FixedVerdict::new(
            "fake",
            Verdict::Infected {
                signature: "Eicar-Test-Signature".into(),
                details: None,
            },
        ));
        let (engine, _audit) = block_closed(scanner);
        let repo = repo().with_scanning(engine);

        let (id, _expires_at) = repo.create_reservation(&ctx()).unwrap();
        let err = repo
            .complete_reservation(
                &ctx(),
                &id,
                "image/png",
                Bytes::from(crate::test_fixtures::valid_png()),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, MediaError::RejectedByScanner(_)));
    }

    #[tokio::test]
    async fn appservice_bypass_skips_scanning_and_is_audited() {
        let scanner = Arc::new(FixedVerdict::new(
            "fake",
            Verdict::Infected {
                signature: "Eicar-Test-Signature".into(),
                details: None,
            },
        ));
        let (engine, audit) = engine_from_config(
            ScanningConfig {
                mode: ScanMode::Block,
                provider: ProviderKind::Icap,
                fail: Some(FailPolicy::Closed),
                appservice_bypass: crate::scanning::config::AppserviceBypass {
                    exempt_appservice_ids: vec!["bridge-1".into()],
                },
                ..ScanningConfig::default()
            },
            scanner.clone(),
        );
        let repo = repo().with_scanning(engine);

        let appservice_ctx = UploadContext {
            user_id: "@bot:example.org".into(),
            server_name: "example.org".into(),
            appservice_id: Some("bridge-1".into()),
        };
        let bytes = Bytes::from(crate::test_fixtures::valid_png());
        let id = repo
            .upload(&appservice_ctx, "image/png", None, bytes)
            .await
            .unwrap();

        // Would have been rejected had it been scanned -- proof the bypass, not a lucky verdict,
        // is what let this through.
        let record = repo.get_record("example.org", id.as_str()).unwrap();
        assert!(record.is_servable());
        assert_eq!(scanner.calls.load(Ordering::SeqCst), 0);

        let entries = audit.recent(10);
        assert!(
            entries
                .iter()
                .any(|e| matches!(e.kind, AuditKind::AppserviceBypass { .. }))
        );
    }
}
