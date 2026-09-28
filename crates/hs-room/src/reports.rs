//! Reports users file about events, rooms and other users, kept durably, and the admin API's
//! view of them.
//!
//! The client-server endpoints (`crate::routes::report`) write through [`ReportStore`], which
//! [`crate::registry::RoomRegistry`] opens over the same backend as every room. The admin API
//! reads and closes them through [`RoomReports`], this crate's implementation of
//! [`hs_admin::reports::ReportSource`]: it is here rather than in `hs-admin` because showing a
//! moderator the reported event needs the room, and rooms are this crate's.
//!
//! One keyspace, `room_reports`, keyed by the report's ULID, so key order is filing order.

use std::sync::Arc;

use async_trait::async_trait;
use hs_admin::reports::{AdminReport, ReportFilter, ReportResolve, ReportSource, apply_resolution};
use hs_admin::sources::SourceError;
use hs_kv::{KvBackend, RangeSpec, TransactConfig, transact};
use hs_tables::keyspace::TypedKeyspace;

use crate::error::RoomError;
use crate::registry::RoomRegistry;

/// The durable store of reports.
pub struct ReportStore<B: KvBackend> {
    backend: B,
    reports: TypedKeyspace<B::Keyspace, (String,)>,
    /// Every report this process files, once it is durable: what the admin API's
    /// `report.created` event is published from (`hs-cli` forwards it onto the event bus).
    filed: tokio::sync::broadcast::Sender<AdminReport>,
}

fn decode(bytes: &[u8]) -> Result<AdminReport, RoomError> {
    serde_json::from_slice(bytes).map_err(|e| RoomError::Internal(format!("a stored report: {e}")))
}

impl<B: KvBackend> ReportStore<B> {
    /// Opens (or creates) the reports keyspace on `backend`.
    ///
    /// # Errors
    /// The backend's, if the keyspace cannot be opened.
    pub fn open(backend: B) -> Result<Self, hs_kv::KvError> {
        let reports = TypedKeyspace::new(backend.keyspace("room_reports")?);
        // A burst of reports larger than this before anybody reads is reported to the
        // subscriber as `Lagged`; the reports themselves are durable either way.
        let (filed, _) = tokio::sync::broadcast::channel(256);
        Ok(Self {
            backend,
            reports,
            filed,
        })
    }

    /// Every report filed through this store from now on, sent once it is durable. A report
    /// filed on another replica is not seen here.
    #[must_use]
    pub fn subscribe(&self) -> tokio::sync::broadcast::Receiver<AdminReport> {
        self.filed.subscribe()
    }

    /// Keeps a newly filed report.
    ///
    /// # Errors
    /// [`RoomError::Store`] on a storage failure.
    pub fn file(&self, report: &AdminReport) -> Result<(), RoomError> {
        let mut stored = report.clone();
        stored.event = None;
        let value = serde_json::to_vec(&stored)
            .map_err(|e| RoomError::Internal(format!("encoding a report: {e}")))?;
        transact(&self.backend, TransactConfig::default(), |txn| {
            self.reports
                .put(txn, &(stored.id.clone(),), &value)
                .map_err(hs_kv::KvError::backend)
        })?;
        // Nobody listening is not an error: the report is kept either way.
        let _ = self.filed.send(stored);
        Ok(())
    }

    /// One report.
    ///
    /// # Errors
    /// On a storage failure or an undecodable row.
    pub fn get(&self, id: &str) -> Result<Option<AdminReport>, RoomError> {
        let snapshot = self.backend.snapshot();
        self.reports
            .get(&snapshot, &(id.to_owned(),))?
            .as_deref()
            .map(decode)
            .transpose()
    }

    /// Every report, newest first.
    ///
    /// # Errors
    /// On a storage failure or an undecodable row.
    pub fn list(&self) -> Result<Vec<AdminReport>, RoomError> {
        let snapshot = self.backend.snapshot();
        let spec = RangeSpec {
            reverse: true,
            ..RangeSpec::full()
        };
        let mut out = Vec::new();
        for item in self.reports.range(&snapshot, spec) {
            let (_key, value) = item?;
            out.push(decode(&value)?);
        }
        Ok(out)
    }

    /// Closes an open report, atomically: two moderators deciding at once cannot both win.
    ///
    /// # Errors
    /// [`SourceError::NotFound`], [`SourceError::Conflict`] (already closed), or
    /// [`SourceError::Unavailable`] on a storage failure.
    pub fn resolve(
        &self,
        id: &str,
        resolve: &ReportResolve,
        resolved_by: &str,
    ) -> Result<AdminReport, SourceError> {
        let key = (id.to_owned(),);
        let mut outcome: Option<Result<AdminReport, SourceError>> = None;
        transact(&self.backend, TransactConfig::default(), |txn| {
            let Some(bytes) = self
                .reports
                .get(txn, &key)
                .map_err(hs_kv::KvError::backend)?
            else {
                outcome = Some(Err(SourceError::NotFound));
                return Ok(());
            };
            let mut report = match decode(&bytes) {
                Ok(report) => report,
                Err(e) => {
                    outcome = Some(Err(SourceError::Unavailable(e.to_string())));
                    return Ok(());
                }
            };
            if let Err(e) = apply_resolution(
                &mut report,
                resolve,
                resolved_by,
                hs_http::time::now_rfc3339(),
            ) {
                outcome = Some(Err(e));
                return Ok(());
            }
            let value = serde_json::to_vec(&report).map_err(hs_kv::KvError::backend)?;
            self.reports
                .put(txn, &key, &value)
                .map_err(hs_kv::KvError::backend)?;
            outcome = Some(Ok(report));
            Ok(())
        })
        .map_err(|e| SourceError::Unavailable(e.to_string()))?;
        outcome.unwrap_or_else(|| Err(SourceError::Unavailable("the report was not read".into())))
    }

    /// Removes a report. `false` if there was none.
    ///
    /// # Errors
    /// On a storage failure.
    pub fn delete(&self, id: &str) -> Result<bool, RoomError> {
        let key = (id.to_owned(),);
        let mut existed = false;
        transact(&self.backend, TransactConfig::default(), |txn| {
            existed = self
                .reports
                .get(txn, &key)
                .map_err(hs_kv::KvError::backend)?
                .is_some();
            if existed {
                self.reports
                    .delete(txn, &key)
                    .map_err(hs_kv::KvError::backend)?;
            }
            Ok(())
        })?;
        Ok(existed)
    }
}

fn unavailable(e: RoomError) -> SourceError {
    SourceError::Unavailable(e.to_string())
}

/// [`ReportSource`] over a [`RoomRegistry`]'s report store, showing the reported event from the
/// room itself.
pub struct RoomReports<B: KvBackend> {
    registry: Arc<RoomRegistry<B>>,
}

impl<B: KvBackend + 'static> RoomReports<B> {
    /// Wraps `registry` for the admin API.
    #[must_use]
    pub fn new(registry: Arc<RoomRegistry<B>>) -> Self {
        Self { registry }
    }

    /// The reported event as the room holds it now, if it does.
    async fn event_of(&self, report: &AdminReport) -> Option<serde_json::Value> {
        let room_id = ruma::RoomId::parse(report.room_id.as_deref()?).ok()?;
        let event_id = ruma::EventId::parse(report.event_id.as_deref()?).ok()?;
        let handle = self.registry.get_or_load(&room_id).await.ok()?;
        handle
            .query(move |actor| {
                actor.event_by_id(&event_id).map(|event| {
                    let rendered = crate::routes::render::client_event_json(event);
                    let redacted = event.header().flags.is_redacted();
                    serde_json::json!({
                        "event_id": rendered["event_id"],
                        "type": rendered["type"],
                        "sender": rendered["sender"],
                        "origin_server_ts": rendered["origin_server_ts"],
                        "content": rendered["content"],
                        "redacted": redacted,
                    })
                })
            })
            .await
    }
}

#[async_trait]
impl<B: KvBackend + 'static> ReportSource for RoomReports<B> {
    async fn list(&self, filter: &ReportFilter) -> Result<Vec<AdminReport>, SourceError> {
        Ok(self
            .registry
            .reports()
            .list()
            .map_err(unavailable)?
            .into_iter()
            .filter(|r| filter.matches(r))
            .collect())
    }

    async fn get(&self, id: &str) -> Result<Option<AdminReport>, SourceError> {
        let Some(mut report) = self.registry.reports().get(id).map_err(unavailable)? else {
            return Ok(None);
        };
        report.event = self.event_of(&report).await;
        Ok(Some(report))
    }

    async fn resolve(
        &self,
        id: &str,
        resolve: &ReportResolve,
        resolved_by: &str,
    ) -> Result<AdminReport, SourceError> {
        self.registry.reports().resolve(id, resolve, resolved_by)
    }

    async fn delete(&self, id: &str) -> Result<(), SourceError> {
        if self.registry.reports().delete(id).map_err(unavailable)? {
            Ok(())
        } else {
            Err(SourceError::NotFound)
        }
    }
}

#[cfg(test)]
mod tests {
    use hs_admin::reports::{ReportKind, ReportResolution, ReportStatus};
    use hs_kv::memory::MemoryBackend;

    use super::*;

    fn report(id: &str) -> AdminReport {
        AdminReport {
            id: id.to_owned(),
            kind: ReportKind::User,
            status: ReportStatus::Open,
            room_id: None,
            event_id: None,
            reporter_id: "@alice:example.org".to_owned(),
            reported_user_id: Some("@mallory:example.org".to_owned()),
            reason: Some("rude".to_owned()),
            score: None,
            received_at: hs_http::time::now_rfc3339(),
            resolution: None,
            resolution_note: None,
            resolved_at: None,
            resolved_by: None,
            event: Some(serde_json::json!({"never": "stored"})),
        }
    }

    #[test]
    fn filed_reports_come_back_newest_first_and_without_a_rendered_event() {
        let store = ReportStore::open(MemoryBackend::new()).unwrap();
        store.file(&report("01A")).unwrap();
        store.file(&report("01B")).unwrap();
        let listed = store.list().unwrap();
        assert_eq!(listed[0].id, "01B");
        assert_eq!(listed[1].id, "01A");
        assert_eq!(listed[0].event, None);
        assert_eq!(
            store.get("01A").unwrap().unwrap().reason.as_deref(),
            Some("rude")
        );
        assert!(store.get("01C").unwrap().is_none());
    }

    #[test]
    fn a_filed_report_is_sent_to_subscribers_without_its_rendered_event() {
        let store = ReportStore::open(MemoryBackend::new()).unwrap();
        let mut filed = store.subscribe();
        store.file(&report("01A")).unwrap();
        let sent = filed.try_recv().unwrap();
        assert_eq!(sent.id, "01A");
        assert_eq!(sent.event, None);
        assert!(filed.try_recv().is_err(), "one report, one message");
    }

    #[test]
    fn resolving_is_once_and_deleting_says_whether_there_was_one() {
        let store = ReportStore::open(MemoryBackend::new()).unwrap();
        store.file(&report("01A")).unwrap();
        let resolve = ReportResolve {
            resolution: ReportResolution::Warned,
            note: Some("first warning".to_owned()),
        };
        let resolved = store.resolve("01A", &resolve, "@ops:example.org").unwrap();
        assert_eq!(resolved.status, ReportStatus::Resolved);
        assert_eq!(store.get("01A").unwrap().unwrap(), resolved);
        assert!(matches!(
            store.resolve("01A", &resolve, "@ops:example.org"),
            Err(SourceError::Conflict(_))
        ));
        assert!(matches!(
            store.resolve("nope", &resolve, "@ops:example.org"),
            Err(SourceError::NotFound)
        ));
        assert!(store.delete("01A").unwrap());
        assert!(!store.delete("01A").unwrap());
    }
}
