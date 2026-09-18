//! The audit log (RFC 0004 section 9): every mutation writes exactly one [`AuditEntry`] before
//! the response is sent. [`AuditSink`] is the seam the storage track implements on `hs-tables`;
//! [`InMemoryAuditSink`] is for tests and `hs-admin-mock`.

use crate::model::AuditEntry;

/// A filter for [`AuditSink::query`], mirroring `GET /audit-log`'s query parameters (RFC 0004
/// section 9).
#[derive(Debug, Clone, Default)]
pub struct AuditFilter {
    pub actor: Option<String>,
    pub action: Option<String>,
    pub target_type: Option<String>,
    pub target_id: Option<String>,
    /// `true` for successful outcomes (2xx/3xx), `false` for failures.
    pub outcome_success: Option<bool>,
    pub recorded_after: Option<String>,
    pub recorded_before: Option<String>,
    pub cursor: Option<String>,
    pub limit: usize,
}

impl AuditFilter {
    pub fn matches(&self, entry: &AuditEntry) -> bool {
        if let Some(actor) = &self.actor
            && &entry.actor.id != actor
        {
            return false;
        }
        if let Some(action) = &self.action
            && &entry.action != action
        {
            return false;
        }
        if let Some(t) = &self.target_type
            && &entry.target.r#type != t
        {
            return false;
        }
        if let Some(id) = &self.target_id
            && &entry.target.id != id
        {
            return false;
        }
        if let Some(success) = self.outcome_success {
            let is_success = (200..400).contains(&entry.outcome.status);
            if is_success != success {
                return false;
            }
        }
        if let Some(after) = &self.recorded_after
            && entry.recorded_at.as_str() < after.as_str()
        {
            return false;
        }
        if let Some(before) = &self.recorded_before
            && entry.recorded_at.as_str() >= before.as_str()
        {
            return false;
        }
        true
    }
}

#[derive(Debug, thiserror::Error)]
pub enum AuditError {
    #[error("audit store unavailable: {0}")]
    Unavailable(String),
}

impl AuditError {
    /// A failed audit write fails the request with `503` (RFC 0004 section 9: "a failed write
    /// fails the request with 503").
    pub fn to_problem(&self) -> hs_http::Problem {
        hs_http::Problem::unavailable().with_detail(self.to_string())
    }
}

/// Appends to, and queries, the audit log. Entries are immutable once written.
#[async_trait::async_trait]
pub trait AuditSink: Send + Sync {
    async fn append(&self, entry: AuditEntry) -> Result<(), AuditError>;
    async fn get(&self, id: &str) -> Result<Option<AuditEntry>, AuditError>;
    /// Returns entries matching `filter`, newest first, up to `filter.limit`, plus whether more
    /// remain (for cursor-style pagination by the caller).
    async fn query(&self, filter: &AuditFilter) -> Result<Vec<AuditEntry>, AuditError>;
}

/// An in-memory [`AuditSink`] for tests and `hs-admin-mock`. Not durable: a process restart loses
/// everything, which is fine for a mock and wrong for production (the real implementation is
/// track 01/02's job on `hs-tables`).
#[derive(Default)]
pub struct InMemoryAuditSink {
    entries: tokio::sync::RwLock<Vec<AuditEntry>>,
}

impl InMemoryAuditSink {
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait::async_trait]
impl AuditSink for InMemoryAuditSink {
    async fn append(&self, entry: AuditEntry) -> Result<(), AuditError> {
        self.entries.write().await.push(entry);
        Ok(())
    }

    async fn get(&self, id: &str) -> Result<Option<AuditEntry>, AuditError> {
        Ok(self
            .entries
            .read()
            .await
            .iter()
            .find(|e| e.id == id)
            .cloned())
    }

    async fn query(&self, filter: &AuditFilter) -> Result<Vec<AuditEntry>, AuditError> {
        let entries = self.entries.read().await;
        let mut matched: Vec<AuditEntry> = entries
            .iter()
            .rev()
            .filter(|e| filter.matches(e))
            .cloned()
            .collect();
        let limit = if filter.limit == 0 { 50 } else { filter.limit };
        matched.truncate(limit);
        Ok(matched)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Actor, ActorKind, AuditOutcome, ResourceRef};

    fn entry(action: &str, status: u16) -> AuditEntry {
        AuditEntry::new(
            action,
            Actor {
                kind: ActorKind::User,
                id: "@ops:example.org".into(),
                display_name: None,
                token_id: None,
                ip: None,
                user_agent: None,
            },
            ResourceRef::new("user", "@mallory:example.org"),
            AuditOutcome::success(status),
        )
    }

    #[tokio::test]
    async fn append_then_get() {
        let sink = InMemoryAuditSink::new();
        let e = entry("users.suspend", 200);
        let id = e.id.clone();
        sink.append(e).await.unwrap();
        let fetched = sink.get(&id).await.unwrap();
        assert!(fetched.is_some());
        assert_eq!(fetched.unwrap().action, "users.suspend");
    }

    #[tokio::test]
    async fn query_filters_by_action_and_outcome() {
        let sink = InMemoryAuditSink::new();
        sink.append(entry("users.suspend", 200)).await.unwrap();
        sink.append(entry("users.deactivate", 500)).await.unwrap();
        let filter = AuditFilter {
            action: Some("users.suspend".into()),
            limit: 10,
            ..Default::default()
        };
        let results = sink.query(&filter).await.unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].action, "users.suspend");

        let failures = AuditFilter {
            outcome_success: Some(false),
            limit: 10,
            ..Default::default()
        };
        let results = sink.query(&failures).await.unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].action, "users.deactivate");
    }

    #[tokio::test]
    async fn query_respects_limit_and_newest_first() {
        let sink = InMemoryAuditSink::new();
        for i in 0..5 {
            sink.append(entry(&format!("action.{i}"), 200))
                .await
                .unwrap();
        }
        let filter = AuditFilter {
            limit: 2,
            ..Default::default()
        };
        let results = sink.query(&filter).await.unwrap();
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].action, "action.4");
        assert_eq!(results[1].action, "action.3");
    }
}
