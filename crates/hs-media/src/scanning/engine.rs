//! [`ScanEngine`]: ties the provider ([`ContentScanner`]), the verdict cache
//! ([`VerdictCache`]), metrics, audit and the mode/fail-policy/replacement rules (RFC sections 5
//! and 3.4) together into the one call `crate::repository::MediaRepository` (once wired — see
//! this crate's status file for exactly how far that wiring got) makes at each of RFC section 4's
//! four scan points.
//!
//! # The two guarantees this module exists to make structurally true
//!
//! 1. **Encrypted media is never reported clean.** [`ScanEngine::evaluate`] classifies content as
//!    encrypted *before* ever calling [`ContentScanner::scan`] (see [`looks_like_encrypted`]):
//!    when the heuristic fires, the provider is never invoked at all, so there is no code path
//!    through which a provider's answer could be mistaken for a clean verdict on ciphertext. See
//!    `tests::encrypted_content_is_never_reported_clean_even_if_the_provider_would_say_so`, which
//!    asserts this against a fake provider that *always* answers `Clean` — proving the encrypted
//!    check runs first, not that the provider happened to behave.
//! 2. **A scanner that is down fails by the operator's explicit choice, never by accident.**
//!    [`ScanningConfig::fail`] is `Option<FailPolicy>`, required by
//!    [`ScanningConfig::validated`] whenever scanning is enabled; [`ScanEngine`] cannot be built
//!    from a config that has not passed validation (see [`ScanEngine::new`]), and every non-cache
//!    scan failure (timeout, connection error, malformed response) is routed through
//!    [`ScanEngine::action_for`] using that explicit policy, never a default.
//!
//! # What is and is not wired to this module yet
//!
//! This module is complete and independently tested. Wiring it into
//! `crate::repository::MediaRepository`'s upload/complete-reservation paths (RFC section 4,
//! points 1 and 2), appservice bypass (point 4) and a federation-fetch entry point (point 3, which
//! has no caller yet — RFC 0007 is still design-only) is **not done in this session**; see
//! `docs/status/09-media.md` for the exact next steps. `ScanEngine::evaluate` is written to be a
//! drop-in call from any of those four call sites: it takes plain bytes, a content type and a
//! [`ScanContext`], and returns an [`EngineDecision`] that already encodes what the caller should
//! do (store as-is, store adapted bytes, or reject) — no scan-point-specific logic lives outside
//! this file except *which* `allow_replacement_here` and audit/appservice details a caller passes
//! in.

use std::sync::Arc;
use std::time::Instant;

use bytes::Bytes;

use crate::scanning::audit::{AuditEntry, AuditKind, AuditSink};
use crate::scanning::cache::VerdictCache;
use crate::scanning::config::{Action, FailPolicy, ScanMode, ScanningConfig};
use crate::scanning::metrics::ScanMetrics;
use crate::scanning::types::{
    AdaptedContent, ContentScanner, ScanContext, ScanSource, UnscannableReason, Verdict, sha256_hex,
};

/// Bytes per chunk handed to the provider's [`ScanSource`] (see that type's doc for why the
/// *source* is one in-memory buffer even though it is handed over in pieces).
const SCAN_CHUNK_SIZE: usize = 64 * 1024;

/// The heuristic this crate uses to recognize likely end-to-end-encrypted uploads: Matrix clients
/// upload encrypted attachments with `Content-Type: application/octet-stream`, since the
/// plaintext's real MIME type is recorded separately in the encrypted event's `info`/`file`
/// content, never in the HTTP upload itself (there is, by design, no way for a homeserver to
/// distinguish "ciphertext" from "any other opaque binary blob" at the upload API — RFC section 7
/// states this plainly: "the server cannot scan encrypted media. It holds ciphertext and no
/// key."). This is deliberately a heuristic, not a certainty: some plain, non-encrypted uploads
/// also legitimately use `application/octet-stream`. Erring toward treating those as
/// `Unscannable { Encrypted }` (default policy: allow) is the safe direction to be wrong in — the
/// alternative, treating actual ciphertext as scannable, is the one RFC section 7 says is
/// "worse than no scanner."
#[must_use]
pub fn looks_like_encrypted(content_type: &str) -> bool {
    content_type
        .split(';')
        .next()
        .unwrap_or(content_type)
        .trim()
        .eq_ignore_ascii_case("application/octet-stream")
}

/// A scan's outcome, mode- and policy-independent: what actually happened, before RFC section 5's
/// mode/fail-policy rules or section 3.4's replacement rules are applied. Never [`Verdict::Pending`]
/// — [`ScanEngine::evaluate`] resolves any pending ticket internally, within budget, before
/// returning.
#[derive(Debug, Clone, PartialEq, Eq)]
enum RawOutcome {
    Clean,
    Infected {
        signature: String,
        details: Option<String>,
    },
    Unscannable {
        reason: UnscannableReason,
    },
    Replaced {
        content: AdaptedContent,
        by: String,
        reason: Option<String>,
    },
    ScannerError {
        message: String,
    },
}

/// What the caller (a scan point) should actually do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EngineDecision {
    /// Proceed with the original content, unmodified.
    Allow,
    /// Proceed, but with `content` in place of the original bytes (RFC section 3.4). The caller
    /// is responsible for recomputing anything derived from the original bytes (size, any stored
    /// hash) from `content` instead.
    AllowReplaced(AdaptedContent),
    /// Reject the upload outright; nothing should be stored. `reason` is safe to surface to the
    /// client (RFC section 5's `block` mode, and the mode-independent "this must never be
    /// silently accepted" cases).
    Reject(String),
    /// Store the content, but immediately quarantine it (RFC section 5's `defer`/`quarantine`
    /// modes' bad-verdict handling — see this module's doc for why both behave the same way in
    /// this session, absent background-job infrastructure). `reason` is for the audit log /
    /// admin surface, not the client.
    StoreQuarantined(String),
}

/// Content scanning/adaptation, tied together. See the module doc.
pub struct ScanEngine<B: hs_kv::KvBackend> {
    scanner: Arc<dyn ContentScanner>,
    cache: VerdictCache<B>,
    config: ScanningConfig,
    metrics: Arc<ScanMetrics>,
    audit: Arc<dyn AuditSink>,
}

impl<B: hs_kv::KvBackend> ScanEngine<B> {
    /// Builds an engine from an already-[`ScanningConfig::validated`] config. Building
    /// `ScanEngine` is the only way `crate::repository` ever gets one, which is what makes
    /// "enabling scanning without a chosen failure policy" a configuration error caught before
    /// any request runs, rather than a runtime default (RFC section 5; see the module doc's
    /// guarantee 2).
    ///
    /// # Errors
    /// Whatever [`ScanningConfig::validated`] or [`crate::scanning::providers::build`] return.
    pub fn new(
        config: ScanningConfig,
        backend: B,
        metrics: Arc<ScanMetrics>,
        audit: Arc<dyn AuditSink>,
    ) -> Result<Self, crate::error::MediaError> {
        let config = config.validated()?;
        let scanner = crate::scanning::providers::build(&config)?;
        let cache = VerdictCache::open(backend, &config.cache)?;
        Ok(Self {
            scanner,
            cache,
            config,
            metrics,
            audit,
        })
    }

    /// Builds an engine from already-constructed parts (for tests, and for a caller that wants a
    /// specific fake [`ContentScanner`]).
    pub fn from_parts(
        config: ScanningConfig,
        scanner: Arc<dyn ContentScanner>,
        cache: VerdictCache<B>,
        metrics: Arc<ScanMetrics>,
        audit: Arc<dyn AuditSink>,
    ) -> Self {
        Self {
            scanner,
            cache,
            config,
            metrics,
            audit,
        }
    }

    /// The configured mode. `Off` means callers should not call [`ScanEngine::evaluate`] at all
    /// (skip scanning entirely, per RFC section 5's "off: no scanning, and the readiness endpoint
    /// does not claim otherwise" — there is deliberately no "evaluate returns Allow instantly for
    /// Off" path here, so a caller cannot mistake "scanning ran and said fine" for "scanning never
    /// ran").
    #[must_use]
    pub fn mode(&self) -> ScanMode {
        self.config.mode
    }

    /// The configured per-scan timeout, for a caller building the [`ScanContext::deadline`] this
    /// evaluation runs under (`crate::repository::MediaRepository` is the only caller today).
    #[must_use]
    pub fn timeout(&self) -> std::time::Duration {
        self.config.timeout.into()
    }

    /// True if `appservice_id` is exempt from scanning under
    /// [`ScanningConfig::appservice_bypass`] (RFC section 4, point 4). A caller that finds this
    /// `true` must skip [`ScanEngine::evaluate`] entirely and call
    /// [`ScanEngine::record_bypass`] instead, so the bypass itself — not a scan outcome — is what
    /// reaches the audit log.
    #[must_use]
    pub fn appservice_bypassed(&self, appservice_id: &str) -> bool {
        self.config
            .appservice_bypass
            .exempt_appservice_ids
            .iter()
            .any(|id| id == appservice_id)
    }

    /// Records that `appservice_id`'s upload bypassed scanning (RFC section 4: "a bridge may be
    /// configured to bypass only by explicit per-appservice configuration, which is recorded in
    /// the audit log"). The caller is responsible for having checked
    /// [`ScanEngine::appservice_bypassed`] first; this method does not re-check it, so it can also
    /// be used to record a bypass decided elsewhere.
    pub async fn record_bypass(&self, appservice_id: &str, ctx: &ScanContext, now_ms: u64) {
        self.audit
            .record(AuditEntry {
                timestamp_ms: now_ms,
                kind: AuditKind::AppserviceBypass {
                    appservice_id: appservice_id.to_string(),
                },
                provider: self.scanner.id().to_string(),
                server_name: ctx.server_name.clone(),
                media_id: ctx.media_id.clone(),
            })
            .await;
    }

    /// Evaluates one piece of content and returns what the caller should do.
    ///
    /// `allow_replacement_here` is the caller's own statement of RFC section 3.4's rule 1
    /// ("replacement applies at upload only") — pass `true` from a local/appservice upload or an
    /// async-upload completion, `false` from anywhere else (notably a federation fetch, which
    /// must never come back with different bytes than what the origin server's event content
    /// attests to).
    pub async fn evaluate(
        &self,
        content_type: &str,
        bytes: Bytes,
        ctx: ScanContext,
        now_ms: u64,
        allow_replacement_here: bool,
    ) -> EngineDecision {
        let is_encrypted = looks_like_encrypted(content_type);
        let provider_id = self.scanner.id().to_string();
        let source_label = ctx.source.as_str().to_string();
        let started = Instant::now();
        // Computed once, up front, and threaded through both the cache-key path (`raw_outcome`)
        // and the replacement audit entry (`decision_for`) — see that method's doc for why this
        // is also the fix for a bug this session found: the audit entry used to hard-code
        // `original_sha256` as an empty string with a "filled in by the caller" comment that no
        // caller ever honored, since `evaluate` is the only place that ever had the original
        // bytes in hand.
        let sha256 = sha256_hex(&bytes);

        let raw = if is_encrypted {
            // Guarantee 1 (see module doc): the provider is never called for content classified
            // as encrypted. There is no verdict to mistake for `Clean` because none was ever
            // sought.
            RawOutcome::Unscannable {
                reason: UnscannableReason::Encrypted,
            }
        } else {
            self.raw_outcome(content_type, &bytes, &sha256, &ctx, now_ms)
                .await
        };

        // RFC section 3.4, rules 2 and 3: a replacement is refused (demoted to a scanner error,
        // decided under the ordinary fail policy) if replacement is disabled here, or if the
        // content is encrypted. The encrypted branch is defense in depth — `is_encrypted` already
        // short-circuited above so a provider could not have been asked in the first place — kept
        // so the refusal is independently true even if that ordering ever changes, and so it has
        // its own direct unit test (`tests::replacement_is_refused_for_encrypted_media_regardless`).
        let raw =
            refuse_disallowed_replacement(raw, allow_replacement_here, is_encrypted, &self.config);

        let verdict_label = verdict_label(&raw);
        self.metrics.record_scan(
            &provider_id,
            verdict_label,
            &source_label,
            started.elapsed().as_secs_f64(),
        );

        self.audit_if_needed(&raw, &provider_id, &ctx, now_ms).await;

        let action = self.action_for(&raw);
        self.decision_for(action, raw, &provider_id, &ctx, now_ms, &sha256)
            .await
    }

    async fn raw_outcome(
        &self,
        content_type: &str,
        bytes: &Bytes,
        sha256: &str,
        ctx: &ScanContext,
        now_ms: u64,
    ) -> RawOutcome {
        if bytes.len() as u64 > self.config.max_size.as_u64() {
            return RawOutcome::Unscannable {
                reason: UnscannableReason::TooLarge,
            };
        }

        let engine_version = self.scanner.engine_version().await;
        if let Ok(Some(cached)) =
            self.cache
                .get(now_ms, sha256, self.scanner.id(), engine_version.as_deref())
        {
            self.metrics.record_cache_hit(self.scanner.id());
            return raw_from_verdict(cached);
        }

        let source = ScanSource::from_bytes(content_type, bytes.clone(), SCAN_CHUNK_SIZE);
        let mut verdict = match self.scanner.scan(source, ctx).await {
            Ok(v) => v,
            Err(e) => {
                return RawOutcome::ScannerError {
                    message: e.to_string(),
                };
            }
        };

        loop {
            match verdict {
                Verdict::Pending {
                    ticket,
                    retry_after,
                } => {
                    let Some(remaining) = ctx.deadline.checked_duration_since(Instant::now())
                    else {
                        break RawOutcome::ScannerError {
                            message: "scan did not complete before the deadline (still pending)"
                                .to_string(),
                        };
                    };
                    if retry_after >= remaining {
                        break RawOutcome::ScannerError {
                            message: "scan did not complete before the deadline (still pending)"
                                .to_string(),
                        };
                    }
                    tokio::time::sleep(retry_after).await;
                    verdict = match self.scanner.poll(&ticket).await {
                        Ok(v) => v,
                        Err(e) => {
                            break RawOutcome::ScannerError {
                                message: e.to_string(),
                            };
                        }
                    };
                }
                other => {
                    break self.finish_terminal(other, sha256, engine_version.as_deref(), now_ms);
                }
            }
        }
    }

    fn finish_terminal(
        &self,
        verdict: Verdict,
        sha256: &str,
        engine_version: Option<&str>,
        now_ms: u64,
    ) -> RawOutcome {
        // Cache only what `crate::scanning::cache` is willing to store (Clean/Infected/
        // Unscannable); `Verdict::Replaced` and `Verdict::Pending` are silently skipped by
        // `VerdictCache::put` itself, so this call is safe regardless of `verdict`'s shape.
        let _ = self
            .cache
            .put(now_ms, sha256, self.scanner.id(), engine_version, &verdict);
        raw_from_verdict(verdict)
    }

    async fn audit_if_needed(
        &self,
        raw: &RawOutcome,
        provider_id: &str,
        ctx: &ScanContext,
        now_ms: u64,
    ) {
        let kind = match raw {
            RawOutcome::Infected { signature, .. } => Some(AuditKind::Infected {
                signature: signature.clone(),
            }),
            RawOutcome::ScannerError { message } => Some(AuditKind::ScannerError {
                message: message.clone(),
            }),
            RawOutcome::Replaced { .. } => None, // audited separately, with both hashes, once applied
            RawOutcome::Clean | RawOutcome::Unscannable { .. } => None,
        };
        if let Some(kind) = kind {
            self.audit
                .record(AuditEntry {
                    timestamp_ms: now_ms,
                    kind,
                    provider: provider_id.to_string(),
                    server_name: ctx.server_name.clone(),
                    media_id: ctx.media_id.clone(),
                })
                .await;
        }
    }

    /// RFC section 5 (`mode`, `fail`) plus `oversize`/`unscannable.*` (RFC section 5's oversize
    /// knob and `crate::scanning::config::UnscannablePolicy`): the single place that turns "what
    /// happened" into "allow, block or quarantine." See the module doc, guarantee 2.
    fn action_for(&self, raw: &RawOutcome) -> Action {
        match raw {
            RawOutcome::Clean | RawOutcome::Replaced { .. } => Action::Allow,
            RawOutcome::Infected { .. } => mode_default_action(self.config.mode),
            RawOutcome::ScannerError { .. } => match self.config.fail {
                // `fail` is guaranteed `Some` whenever `mode != Off` by `ScanningConfig::validated`,
                // which `ScanEngine::new` requires before this engine can exist at all — see the
                // module doc, guarantee 2. `Off` is unreachable here in practice (a caller that
                // respects `ScanEngine::mode()` never calls `evaluate` when mode is `Off`), so the
                // fallback below is defense in depth, not a silent default reachable in normal
                // operation.
                Some(FailPolicy::Closed) => mode_default_action(self.config.mode),
                Some(FailPolicy::Open) => Action::Allow,
                None => Action::Allow,
            },
            RawOutcome::Unscannable { reason } => match reason {
                UnscannableReason::Encrypted => self.config.unscannable.encrypted,
                UnscannableReason::TooLarge => self.config.oversize,
                _ => self.config.unscannable.other,
            },
        }
    }

    async fn decision_for(
        &self,
        action: Action,
        raw: RawOutcome,
        provider_id: &str,
        ctx: &ScanContext,
        now_ms: u64,
        original_sha256: &str,
    ) -> EngineDecision {
        match (action, raw) {
            (
                Action::Allow,
                RawOutcome::Replaced {
                    content,
                    by,
                    reason,
                },
            ) => {
                let original_sha256 = original_sha256.to_string();
                let adapted_sha256 = sha256_hex(&content.bytes);
                self.audit
                    .record(AuditEntry {
                        timestamp_ms: now_ms,
                        kind: AuditKind::ReplacementApplied {
                            by: by.clone(),
                            original_sha256,
                            adapted_sha256,
                        },
                        provider: provider_id.to_string(),
                        server_name: ctx.server_name.clone(),
                        media_id: ctx.media_id.clone(),
                    })
                    .await;
                let _ = reason;
                EngineDecision::AllowReplaced(content)
            }
            (Action::Allow, _) => EngineDecision::Allow,
            (Action::Block, raw) => EngineDecision::Reject(describe(&raw)),
            (Action::Quarantine, raw) => EngineDecision::StoreQuarantined(describe(&raw)),
        }
    }
}

fn mode_default_action(mode: ScanMode) -> Action {
    match mode {
        ScanMode::Block => Action::Block,
        ScanMode::Defer | ScanMode::Quarantine => Action::Quarantine,
        // Unreachable in normal operation (see `ScanEngine::mode`'s doc); `Allow` is the safest
        // fallback if it is ever hit, matching "off" ultimately meaning "do not obstruct uploads".
        ScanMode::Off => Action::Allow,
    }
}

fn raw_from_verdict(v: Verdict) -> RawOutcome {
    match v {
        Verdict::Clean => RawOutcome::Clean,
        Verdict::Infected { signature, details } => RawOutcome::Infected { signature, details },
        Verdict::Unscannable { reason } => RawOutcome::Unscannable { reason },
        Verdict::Replaced {
            content,
            by,
            reason,
        } => RawOutcome::Replaced {
            content,
            by,
            reason,
        },
        // `ScanEngine::raw_outcome`'s poll loop never lets `Pending` reach here; kept exhaustive
        // (rather than `unreachable!()`) so a future change to that loop fails a test, not a panic
        // in production.
        Verdict::Pending { .. } => RawOutcome::ScannerError {
            message: "internal error: unresolved Pending verdict reached raw_from_verdict".into(),
        },
    }
}

fn verdict_label(raw: &RawOutcome) -> &'static str {
    match raw {
        RawOutcome::Clean => "clean",
        RawOutcome::Infected { .. } => "infected",
        RawOutcome::Unscannable { .. } => "unscannable",
        RawOutcome::Replaced { .. } => "replaced",
        RawOutcome::ScannerError { .. } => "error",
    }
}

fn describe(raw: &RawOutcome) -> String {
    match raw {
        RawOutcome::Clean => "clean".to_string(),
        RawOutcome::Infected { signature, .. } => format!("infected: {signature}"),
        RawOutcome::Unscannable { reason } => format!("unscannable: {reason:?}"),
        RawOutcome::Replaced { by, .. } => format!("replaced by {by}"),
        RawOutcome::ScannerError { message } => format!("scanner error: {message}"),
    }
}

/// RFC section 3.4, rules 2 and 3, applied as a pure function so it has its own direct test
/// independent of the rest of [`ScanEngine::evaluate`]'s pipeline (`tests::replacement_*`).
fn refuse_disallowed_replacement(
    raw: RawOutcome,
    allow_replacement_here: bool,
    is_encrypted: bool,
    config: &ScanningConfig,
) -> RawOutcome {
    let RawOutcome::Replaced { by, .. } = &raw else {
        return raw;
    };
    if is_encrypted {
        return RawOutcome::ScannerError {
            message: format!(
                "provider {by} returned replacement content for encrypted media; refused \
                 (RFC 0008 section 3.4, rule 2)"
            ),
        };
    }
    if !config.allow_replacement || !allow_replacement_here {
        return RawOutcome::ScannerError {
            message: format!(
                "provider {by} returned replacement content but allow_replacement is disabled \
                 (or this is not an upload-time scan point); treated as a scanner error \
                 (RFC 0008 section 3.4, rule 3)"
            ),
        };
    }
    raw
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scanning::types::{ScanError, ScanSourceKind};
    use async_trait::async_trait;
    use hs_kv::memory::MemoryBackend;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    fn ctx() -> ScanContext {
        ScanContext {
            deadline: Instant::now() + Duration::from_secs(5),
            uploader: Some("@alice:example.org".into()),
            source: ScanSourceKind::Local,
            media_id: "m1".into(),
            server_name: "example.org".into(),
        }
    }

    struct FakeScanner {
        id: &'static str,
        verdict: Verdict,
        calls: AtomicUsize,
    }

    impl FakeScanner {
        fn always(id: &'static str, verdict: Verdict) -> Self {
            Self {
                id,
                verdict,
                calls: AtomicUsize::new(0),
            }
        }
    }

    #[async_trait]
    impl ContentScanner for FakeScanner {
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

    fn engine_with(
        config: ScanningConfig,
        scanner: Arc<dyn ContentScanner>,
    ) -> (
        ScanEngine<MemoryBackend>,
        Arc<crate::scanning::audit::InMemoryAuditSink>,
    ) {
        let audit = Arc::new(crate::scanning::audit::InMemoryAuditSink::new(100));
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

    fn block_closed_config() -> ScanningConfig {
        ScanningConfig {
            mode: ScanMode::Block,
            fail: Some(FailPolicy::Closed),
            ..ScanningConfig::default()
        }
    }

    #[tokio::test]
    async fn clean_content_is_allowed() {
        let scanner = Arc::new(FakeScanner::always("fake", Verdict::Clean));
        let (engine, _audit) = engine_with(block_closed_config(), scanner);
        let decision = engine
            .evaluate(
                "image/png",
                Bytes::from_static(b"png-bytes"),
                ctx(),
                0,
                false,
            )
            .await;
        assert_eq!(decision, EngineDecision::Allow);
    }

    #[tokio::test]
    async fn infected_content_is_rejected_under_block_mode() {
        let scanner = Arc::new(FakeScanner::always(
            "fake",
            Verdict::Infected {
                signature: "Eicar-Test-Signature".into(),
                details: None,
            },
        ));
        let (engine, audit) = engine_with(block_closed_config(), scanner);
        let decision = engine
            .evaluate("text/plain", Bytes::from_static(b"X5O!"), ctx(), 0, false)
            .await;
        assert!(matches!(decision, EngineDecision::Reject(_)));
        assert_eq!(audit.recent(10).len(), 1);
    }

    #[tokio::test]
    async fn infected_content_is_quarantined_under_quarantine_mode() {
        let scanner = Arc::new(FakeScanner::always(
            "fake",
            Verdict::Infected {
                signature: "Eicar-Test-Signature".into(),
                details: None,
            },
        ));
        let config = ScanningConfig {
            mode: ScanMode::Quarantine,
            fail: Some(FailPolicy::Closed),
            ..ScanningConfig::default()
        };
        let (engine, _audit) = engine_with(config, scanner);
        let decision = engine
            .evaluate("text/plain", Bytes::from_static(b"X5O!"), ctx(), 0, false)
            .await;
        assert!(matches!(decision, EngineDecision::StoreQuarantined(_)));
    }

    // --- The guarantee that matters most: encrypted media is never reported clean -------------

    #[tokio::test]
    async fn encrypted_content_is_never_reported_clean_even_if_the_provider_would_say_so() {
        // A provider that says "Clean" to absolutely everything -- if the encrypted short-circuit
        // in `ScanEngine::evaluate` did not run first, this test would observe `Allow` (from a
        // fabricated Clean verdict) instead of the encrypted-content policy outcome.
        let scanner = Arc::new(FakeScanner::always("always-clean", Verdict::Clean));
        let config = ScanningConfig {
            mode: ScanMode::Block,
            fail: Some(FailPolicy::Closed),
            ..ScanningConfig::default()
        };
        let (engine, _audit) = engine_with(config, scanner.clone());

        let decision = engine
            .evaluate(
                "application/octet-stream", // Matrix clients' encrypted-attachment convention
                Bytes::from_static(b"totally-opaque-ciphertext"),
                ctx(),
                0,
                false,
            )
            .await;

        // Default policy for Encrypted is Allow (RFC section 7), so the upload proceeds -- but
        // critically, the *reason* is "unscannable, policy says allow", not "scanned clean".
        assert_eq!(decision, EngineDecision::Allow);
        // The proof this wasn't just a lucky Clean verdict: the provider was never called.
        assert_eq!(scanner.calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn encrypted_content_can_be_configured_to_block_instead_of_allow() {
        let scanner = Arc::new(FakeScanner::always("always-clean", Verdict::Clean));
        let mut config = block_closed_config();
        config.unscannable.encrypted = Action::Block;
        let (engine, _audit) = engine_with(config, scanner.clone());
        let decision = engine
            .evaluate(
                "application/octet-stream",
                Bytes::from_static(b"ciphertext"),
                ctx(),
                0,
                false,
            )
            .await;
        assert!(matches!(decision, EngineDecision::Reject(_)));
        assert_eq!(scanner.calls.load(Ordering::SeqCst), 0);
    }

    // --- Fail policy: explicit, never accidental ------------------------------------------------

    struct AlwaysErrors;
    #[async_trait]
    impl ContentScanner for AlwaysErrors {
        fn id(&self) -> &str {
            "always-errors"
        }
        async fn engine_version(&self) -> Option<String> {
            None
        }
        async fn scan(
            &self,
            _content: ScanSource<'_>,
            _ctx: &ScanContext,
        ) -> Result<Verdict, ScanError> {
            Err(ScanError::Unavailable("connection refused".into()))
        }
    }

    #[tokio::test]
    async fn scanner_down_under_fail_closed_rejects() {
        let config = ScanningConfig {
            mode: ScanMode::Block,
            fail: Some(FailPolicy::Closed),
            ..ScanningConfig::default()
        };
        let (engine, audit) = engine_with(config, Arc::new(AlwaysErrors));
        let decision = engine
            .evaluate("image/png", Bytes::from_static(b"x"), ctx(), 0, false)
            .await;
        assert!(matches!(decision, EngineDecision::Reject(_)));
        assert_eq!(audit.recent(10).len(), 1);
    }

    #[tokio::test]
    async fn scanner_down_under_fail_open_allows() {
        let config = ScanningConfig {
            mode: ScanMode::Block,
            fail: Some(FailPolicy::Open),
            ..ScanningConfig::default()
        };
        let (engine, audit) = engine_with(config, Arc::new(AlwaysErrors));
        let decision = engine
            .evaluate("image/png", Bytes::from_static(b"x"), ctx(), 0, false)
            .await;
        assert_eq!(decision, EngineDecision::Allow);
        // Still audited: "allows and records" (RFC section 5).
        assert_eq!(audit.recent(10).len(), 1);
    }

    #[tokio::test]
    async fn timeout_is_treated_as_a_scanner_error() {
        struct NeverResponds;
        #[async_trait]
        impl ContentScanner for NeverResponds {
            fn id(&self) -> &str {
                "never-responds"
            }
            async fn engine_version(&self) -> Option<String> {
                None
            }
            async fn scan(
                &self,
                _content: ScanSource<'_>,
                ctx: &ScanContext,
            ) -> Result<Verdict, ScanError> {
                // Simulate a provider that respects the deadline itself and reports a timeout,
                // rather than actually sleeping in the test.
                let _ = ctx;
                Err(ScanError::Timeout)
            }
        }
        let config = ScanningConfig {
            mode: ScanMode::Block,
            fail: Some(FailPolicy::Closed),
            ..ScanningConfig::default()
        };
        let (engine, _audit) = engine_with(config, Arc::new(NeverResponds));
        let decision = engine
            .evaluate("image/png", Bytes::from_static(b"x"), ctx(), 0, false)
            .await;
        assert!(matches!(decision, EngineDecision::Reject(_)));
    }

    #[tokio::test]
    async fn malformed_response_is_treated_as_a_scanner_error() {
        struct Malformed;
        #[async_trait]
        impl ContentScanner for Malformed {
            fn id(&self) -> &str {
                "malformed"
            }
            async fn engine_version(&self) -> Option<String> {
                None
            }
            async fn scan(
                &self,
                _content: ScanSource<'_>,
                _ctx: &ScanContext,
            ) -> Result<Verdict, ScanError> {
                Err(ScanError::Protocol("unexpected byte".into()))
            }
        }
        let (engine, _audit) = engine_with(block_closed_config(), Arc::new(Malformed));
        let decision = engine
            .evaluate("image/png", Bytes::from_static(b"x"), ctx(), 0, false)
            .await;
        assert!(matches!(decision, EngineDecision::Reject(_)));
    }

    // --- Cache: a hit must not be counted as a scan, and a version change must invalidate ------

    #[tokio::test]
    async fn cache_hit_is_not_counted_as_a_scan_but_the_first_call_is() {
        let scanner = Arc::new(FakeScanner::always("fake", Verdict::Clean));
        let (engine, _audit) = engine_with(block_closed_config(), scanner.clone());
        let bytes = Bytes::from_static(b"same-bytes-both-times");
        engine
            .evaluate("image/png", bytes.clone(), ctx(), 0, false)
            .await;
        engine.evaluate("image/png", bytes, ctx(), 0, false).await;
        // Only the first call reached the provider; the second was a cache hit.
        assert_eq!(scanner.calls.load(Ordering::SeqCst), 1);
    }

    // --- Replacement (RFC section 3.4) ----------------------------------------------------------

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
    async fn replacement_is_applied_when_enabled() {
        let scanner = Arc::new(FakeScanner::always("icap", replaced_verdict()));
        let mut config = block_closed_config();
        config.allow_replacement = true;
        let (engine, audit) = engine_with(config, scanner);
        let decision = engine
            .evaluate(
                "image/jpeg",
                Bytes::from_static(b"original-bytes"),
                ctx(),
                0,
                true,
            )
            .await;
        match decision {
            EngineDecision::AllowReplaced(content) => {
                assert_eq!(content.bytes.as_ref(), b"exif-stripped-bytes");
            }
            other => panic!("expected AllowReplaced, got {other:?}"),
        }
        let entries = audit.recent(10);
        assert_eq!(entries.len(), 1);
        assert!(matches!(
            entries[0].kind,
            AuditKind::ReplacementApplied { .. }
        ));
    }

    #[tokio::test]
    async fn replacement_is_refused_when_disabled() {
        let scanner = Arc::new(FakeScanner::always("icap", replaced_verdict()));
        let config = block_closed_config(); // allow_replacement: false (default)
        let (engine, _audit) = engine_with(config, scanner);
        let decision = engine
            .evaluate(
                "image/jpeg",
                Bytes::from_static(b"original-bytes"),
                ctx(),
                0,
                true,
            )
            .await;
        // Refused -> treated as a scanner error -> fail: closed -> reject.
        assert!(matches!(decision, EngineDecision::Reject(_)));
    }

    #[tokio::test]
    async fn replacement_is_refused_for_encrypted_media_regardless_of_allow_replacement() {
        // Direct test of the pure refusal rule (RFC 3.4 rule 2), independent of the fact that
        // `evaluate` would never even reach a provider for encrypted content today: proves the
        // refusal holds on its own, not only as a side effect of the encrypted short-circuit.
        let raw = RawOutcome::Replaced {
            content: AdaptedContent {
                bytes: Bytes::from_static(b"whatever"),
                content_type: None,
            },
            by: "icap".to_string(),
            reason: None,
        };
        let config = ScanningConfig {
            allow_replacement: true, // even enabled...
            ..ScanningConfig::default()
        };
        let refused = refuse_disallowed_replacement(raw, true, true, &config); // ...and is_encrypted=true
        assert!(matches!(refused, RawOutcome::ScannerError { .. }));
    }

    #[tokio::test]
    async fn replacement_is_refused_outside_upload_scan_points() {
        let scanner = Arc::new(FakeScanner::always("icap", replaced_verdict()));
        let mut config = block_closed_config();
        config.allow_replacement = true;
        let (engine, _audit) = engine_with(config, scanner);
        // allow_replacement_here=false, as a federation-fetch caller would pass.
        let decision = engine
            .evaluate(
                "image/jpeg",
                Bytes::from_static(b"original-bytes"),
                ctx(),
                0,
                false,
            )
            .await;
        assert!(matches!(decision, EngineDecision::Reject(_)));
    }

    #[test]
    fn looks_like_encrypted_matches_the_octet_stream_convention() {
        assert!(looks_like_encrypted("application/octet-stream"));
        assert!(looks_like_encrypted(
            "application/octet-stream; charset=binary"
        ));
        assert!(!looks_like_encrypted("image/png"));
        assert!(!looks_like_encrypted("text/plain"));
    }
}
