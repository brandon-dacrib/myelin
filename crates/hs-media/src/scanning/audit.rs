//! Audit hooks for content scanning (RFC section 8): "audit entries for every infected and every
//! error verdict, including provider, signature and media identifier," plus (RFC section 3.4)
//! "every replacement is audited, recording the service, the reason, and both content hashes,"
//! and (RFC section 4) "a bridge may be configured to bypass \[scanning\] only by explicit
//! per-appservice configuration, which is recorded in the audit log."

use std::collections::VecDeque;
use std::sync::Mutex;

use async_trait::async_trait;

/// Why an audit entry was written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuditKind {
    /// A scan found infected content.
    Infected {
        /// The signature/threat name.
        signature: String,
    },
    /// A scan could not be completed (timeout, connection failure, malformed response).
    ScannerError {
        /// The failure, in human-readable form.
        message: String,
    },
    /// A provider's replacement content was applied.
    ReplacementApplied {
        /// The service that performed the adaptation.
        by: String,
        /// The original content's SHA-256, hex-encoded.
        original_sha256: String,
        /// The adapted content's SHA-256, hex-encoded.
        adapted_sha256: String,
    },
    /// An appservice's upload bypassed scanning per its explicit configuration
    /// (`ScanningConfig::appservice_bypass`).
    AppserviceBypass {
        /// The appservice's registration id.
        appservice_id: String,
    },
}

/// One audit-log entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuditEntry {
    /// Milliseconds since the Unix epoch.
    pub timestamp_ms: u64,
    /// Why this entry exists.
    pub kind: AuditKind,
    /// The provider id that produced this outcome (`"icap"`, `"http"`, `"none"`).
    pub provider: String,
    /// The `server_name` the media is filed under.
    pub server_name: String,
    /// The media id.
    pub media_id: String,
}

/// Where audit entries go. Implementations must not block the scan path for long (this is called
/// from `crate::scanning::engine` on the request path for `block`/`defer` mode); a slow sink
/// should buffer internally rather than making `record` itself slow.
#[async_trait]
pub trait AuditSink: Send + Sync {
    /// Records one audit entry.
    async fn record(&self, entry: AuditEntry);
}

/// Logs every entry via `tracing::warn!` (infected/error/replacement) or `tracing::info!`
/// (appservice bypass, which is an operator's explicit choice, not a problem). The default sink:
/// always safe to use, since `tracing`'s own subscriber decides whether anything is actually
/// written anywhere.
#[derive(Debug, Default, Clone, Copy)]
pub struct TracingAuditSink;

#[async_trait]
impl AuditSink for TracingAuditSink {
    async fn record(&self, entry: AuditEntry) {
        match &entry.kind {
            AuditKind::Infected { signature } => tracing::warn!(
                provider = %entry.provider,
                server_name = %entry.server_name,
                media_id = %entry.media_id,
                signature = %signature,
                "media scan: infected"
            ),
            AuditKind::ScannerError { message } => tracing::warn!(
                provider = %entry.provider,
                server_name = %entry.server_name,
                media_id = %entry.media_id,
                error = %message,
                "media scan: error"
            ),
            AuditKind::ReplacementApplied {
                by,
                original_sha256,
                adapted_sha256,
            } => tracing::warn!(
                provider = %entry.provider,
                server_name = %entry.server_name,
                media_id = %entry.media_id,
                by = %by,
                original_sha256 = %original_sha256,
                adapted_sha256 = %adapted_sha256,
                "media scan: content replaced"
            ),
            AuditKind::AppserviceBypass { appservice_id } => tracing::info!(
                provider = %entry.provider,
                server_name = %entry.server_name,
                media_id = %entry.media_id,
                appservice_id = %appservice_id,
                "media scan: bypassed for appservice"
            ),
        }
    }
}

/// Keeps the last `capacity` entries in memory, for the admin "list recent verdicts" surface
/// (RFC section 8) and for tests. Not durable — a real deployment wanting a durable audit trail
/// composes this with (or replaces it by) a sink that writes to `hs-tables`/an external system;
/// this crate ships the in-memory version because the admin surface (`crate::scanning::admin`)
/// needs *something* to list against today, and the durable-storage question is track 15's own
/// audit-log infrastructure (`PLAN.md` D12), not this track's to build twice.
pub struct InMemoryAuditSink {
    capacity: usize,
    entries: Mutex<VecDeque<AuditEntry>>,
}

impl InMemoryAuditSink {
    /// Builds a sink retaining at most `capacity` entries (oldest evicted first).
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity: capacity.max(1),
            entries: Mutex::new(VecDeque::new()),
        }
    }

    /// The most recent `limit` entries, newest first.
    #[must_use]
    pub fn recent(&self, limit: usize) -> Vec<AuditEntry> {
        #[allow(
            clippy::unwrap_used,
            reason = "see crate::policy::InMemoryQuotaPolicy::current"
        )]
        let entries = self.entries.lock().unwrap();
        entries.iter().rev().take(limit).cloned().collect()
    }
}

#[async_trait]
impl AuditSink for InMemoryAuditSink {
    async fn record(&self, entry: AuditEntry) {
        #[allow(
            clippy::unwrap_used,
            reason = "see crate::policy::InMemoryQuotaPolicy::current"
        )]
        let mut entries = self.entries.lock().unwrap();
        entries.push_back(entry);
        while entries.len() > self.capacity {
            entries.pop_front();
        }
    }
}

/// Composes two sinks, writing to both. Useful to keep `TracingAuditSink` (so entries show up in
/// logs) while also feeding `InMemoryAuditSink` (so the admin surface has something to list).
pub struct FanOutAuditSink {
    sinks: Vec<std::sync::Arc<dyn AuditSink>>,
}

impl FanOutAuditSink {
    /// Builds a sink that writes to every one of `sinks`, in order.
    #[must_use]
    pub fn new(sinks: Vec<std::sync::Arc<dyn AuditSink>>) -> Self {
        Self { sinks }
    }
}

#[async_trait]
impl AuditSink for FanOutAuditSink {
    async fn record(&self, entry: AuditEntry) {
        for sink in &self.sinks {
            sink.record(entry.clone()).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(kind: AuditKind) -> AuditEntry {
        AuditEntry {
            timestamp_ms: 1000,
            kind,
            provider: "icap".into(),
            server_name: "example.org".into(),
            media_id: "abc123".into(),
        }
    }

    #[tokio::test]
    async fn in_memory_sink_keeps_newest_first() {
        let sink = InMemoryAuditSink::new(10);
        sink.record(entry(AuditKind::Infected {
            signature: "sig-1".into(),
        }))
        .await;
        sink.record(entry(AuditKind::Infected {
            signature: "sig-2".into(),
        }))
        .await;
        let recent = sink.recent(10);
        assert_eq!(recent.len(), 2);
        assert_eq!(
            recent[0].kind,
            AuditKind::Infected {
                signature: "sig-2".into()
            }
        );
    }

    #[tokio::test]
    async fn in_memory_sink_evicts_oldest_beyond_capacity() {
        let sink = InMemoryAuditSink::new(2);
        for i in 0..5 {
            sink.record(entry(AuditKind::Infected {
                signature: format!("sig-{i}"),
            }))
            .await;
        }
        let recent = sink.recent(10);
        assert_eq!(recent.len(), 2);
        assert_eq!(
            recent[0].kind,
            AuditKind::Infected {
                signature: "sig-4".into()
            }
        );
        assert_eq!(
            recent[1].kind,
            AuditKind::Infected {
                signature: "sig-3".into()
            }
        );
    }

    #[tokio::test]
    async fn fan_out_writes_to_every_sink() {
        let a = std::sync::Arc::new(InMemoryAuditSink::new(10));
        let b = std::sync::Arc::new(InMemoryAuditSink::new(10));
        let fan_out = FanOutAuditSink::new(vec![a.clone(), b.clone()]);
        fan_out
            .record(entry(AuditKind::AppserviceBypass {
                appservice_id: "bridge-1".into(),
            }))
            .await;
        assert_eq!(a.recent(10).len(), 1);
        assert_eq!(b.recent(10).len(), 1);
    }
}
