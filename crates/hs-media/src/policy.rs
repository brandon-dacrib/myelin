//! Pluggable per-user and per-server upload limits.
//!
//! Kept as a trait rather than baked into [`crate::repository::MediaRepository`] so a deployment
//! can swap in a policy backed by real usage accounting (a running total per user/server, decayed
//! over time, exempting admins or appservices) without this crate's upload code changing at all —
//! `MediaRepository` only ever calls [`UploadPolicy::check`] before accepting bytes and
//! [`UploadPolicy::record`] after they are durably stored.

use async_trait::async_trait;

use crate::error::MediaError;

/// Who is uploading, for policy purposes. Kept minimal (not the full `hs_auth::Requester`) so
/// this trait does not pull an `hs-auth` dependency into policy implementations that do not need
/// device/appservice detail.
#[derive(Debug, Clone)]
pub struct UploadContext {
    /// The uploading user, e.g. `@alice:example.org`.
    pub user_id: String,
    /// The server this request arrived on behalf of (normally this homeserver's own name; kept
    /// distinct from `user_id`'s domain for the rare appservice-masquerade case).
    pub server_name: String,
}

/// A pluggable upload quota check.
#[async_trait]
pub trait UploadPolicy: Send + Sync {
    /// Called before accepting `declared_size` bytes (the `Content-Length` the client announced,
    /// or `None` for a chunked/unknown-length body, in which case an implementation should still
    /// enforce whatever whole-server or whole-user ceiling it tracks and let the streaming size
    /// check in `crate::repository` catch an oversized body as it arrives).
    ///
    /// # Errors
    /// Returns [`MediaError::QuotaExceeded`] to reject the upload before any bytes are read.
    async fn check(
        &self,
        ctx: &UploadContext,
        declared_size: Option<u64>,
    ) -> Result<(), MediaError>;

    /// Called once, after an upload's bytes are durably stored, with the *actual* size (which may
    /// differ from `declared_size` for a chunked body). Implementations that track a running
    /// total update it here; the default no-op is correct for any implementation that does not
    /// track cumulative usage (a flat per-request limit, for instance).
    async fn record(&self, _ctx: &UploadContext, _actual_size: u64) {}
}

/// A flat per-request size ceiling, with independent per-user and per-server *cumulative* totals
/// tracked in memory. This is the default policy: adequate for a single-process deployment; a
/// clustered deployment (`PLAN.md` section 6) will want a policy backed by shared storage instead
/// (an `hs-tables` counter keyspace, most naturally, via [`hs_kv::KvWrite::atomic_add`]) — kept as
/// an open follow-up (`docs/status/09-media.md`), not implemented here since it needs the
/// `hs-kv` backend `MediaRepository` was already given, not a new one.
pub struct InMemoryQuotaPolicy {
    max_per_user_bytes: u64,
    max_per_server_bytes: u64,
    per_user: std::sync::Mutex<std::collections::HashMap<String, u64>>,
    per_server: std::sync::Mutex<std::collections::HashMap<String, u64>>,
}

impl InMemoryQuotaPolicy {
    /// A policy with the given cumulative ceilings. `u64::MAX` effectively disables a given
    /// dimension.
    #[must_use]
    pub fn new(max_per_user_bytes: u64, max_per_server_bytes: u64) -> Self {
        Self {
            max_per_user_bytes,
            max_per_server_bytes,
            per_user: std::sync::Mutex::new(std::collections::HashMap::new()),
            per_server: std::sync::Mutex::new(std::collections::HashMap::new()),
        }
    }

    /// No limits at all: every [`UploadPolicy::check`] call succeeds. The default for tests and
    /// for a deployment that only wants the flat `max_upload_size` request-body ceiling
    /// (`hs_config::MediaConfig::max_upload_size`, enforced independently in
    /// `crate::repository`, not by this policy).
    #[must_use]
    pub fn unlimited() -> Self {
        Self::new(u64::MAX, u64::MAX)
    }

    fn current(map: &std::sync::Mutex<std::collections::HashMap<String, u64>>, key: &str) -> u64 {
        #[allow(
            clippy::unwrap_used,
            reason = "only poisoned if a prior holder panicked mid-update; a quota policy is not \
                       the place to propagate that, and a poisoned lock's data is still readable"
        )]
        map.lock().unwrap().get(key).copied().unwrap_or(0)
    }
}

#[async_trait]
impl UploadPolicy for InMemoryQuotaPolicy {
    async fn check(
        &self,
        ctx: &UploadContext,
        declared_size: Option<u64>,
    ) -> Result<(), MediaError> {
        let Some(size) = declared_size else {
            return Ok(());
        };
        let user_total = Self::current(&self.per_user, &ctx.user_id);
        if user_total.saturating_add(size) > self.max_per_user_bytes {
            return Err(MediaError::QuotaExceeded {
                reason: format!(
                    "user {} would exceed their {} byte upload quota",
                    ctx.user_id, self.max_per_user_bytes
                ),
            });
        }
        let server_total = Self::current(&self.per_server, &ctx.server_name);
        if server_total.saturating_add(size) > self.max_per_server_bytes {
            return Err(MediaError::QuotaExceeded {
                reason: format!(
                    "server {} would exceed its {} byte upload quota",
                    ctx.server_name, self.max_per_server_bytes
                ),
            });
        }
        Ok(())
    }

    async fn record(&self, ctx: &UploadContext, actual_size: u64) {
        #[allow(clippy::unwrap_used, reason = "see InMemoryQuotaPolicy::current")]
        {
            *self
                .per_user
                .lock()
                .unwrap()
                .entry(ctx.user_id.clone())
                .or_insert(0) += actual_size;
            *self
                .per_server
                .lock()
                .unwrap()
                .entry(ctx.server_name.clone())
                .or_insert(0) += actual_size;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> UploadContext {
        UploadContext {
            user_id: "@alice:example.org".into(),
            server_name: "example.org".into(),
        }
    }

    #[tokio::test]
    async fn unlimited_never_rejects() {
        let policy = InMemoryQuotaPolicy::unlimited();
        policy.check(&ctx(), Some(u64::MAX / 2)).await.unwrap();
    }

    #[tokio::test]
    async fn per_user_quota_is_enforced_cumulatively() {
        let policy = InMemoryQuotaPolicy::new(1000, u64::MAX);
        policy.check(&ctx(), Some(600)).await.unwrap();
        policy.record(&ctx(), 600).await;
        // A second 600-byte upload would push the user to 1200 > 1000.
        let err = policy.check(&ctx(), Some(600)).await.unwrap_err();
        assert!(matches!(err, MediaError::QuotaExceeded { .. }));
    }

    #[tokio::test]
    async fn per_server_quota_is_independent_of_per_user() {
        let policy = InMemoryQuotaPolicy::new(u64::MAX, 1000);
        let alice = UploadContext {
            user_id: "@alice:example.org".into(),
            server_name: "example.org".into(),
        };
        let bob = UploadContext {
            user_id: "@bob:example.org".into(),
            server_name: "example.org".into(),
        };
        policy.check(&alice, Some(700)).await.unwrap();
        policy.record(&alice, 700).await;
        // Bob shares the same server total, so this should now fail even though Bob personally
        // has uploaded nothing yet.
        let err = policy.check(&bob, Some(700)).await.unwrap_err();
        assert!(matches!(err, MediaError::QuotaExceeded { .. }));
    }

    #[tokio::test]
    async fn unknown_declared_size_is_not_rejected_up_front() {
        let policy = InMemoryQuotaPolicy::new(10, 10);
        // A chunked body with no declared Content-Length: `check` cannot know the size yet, so it
        // must not reject here (the streaming size check in `crate::repository` is what catches
        // an oversized chunked body as bytes actually arrive).
        policy.check(&ctx(), None).await.unwrap();
    }
}
