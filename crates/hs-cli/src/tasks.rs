//! A durable [`hs_admin::tasks::TaskStore`] over `hs-kv`, so the Tasks page still knows what ran
//! (and what was interrupted) after a restart.
//!
//! One keyspace, `hs_admin.tasks`, keyed by task id, each value the JSON of an
//! [`hs_admin::tasks::TaskRecord`]. Listing reads them all and the registry sorts them: there are
//! few (long-running admin work is rare, and finished tasks are pruned after thirty days by
//! `TaskRegistry::recover_interrupted` at startup), so an index would cost more than it saves.

use async_trait::async_trait;
use hs_admin::sources::SourceError;
use hs_admin::tasks::{TaskRecord, TaskStore};
use hs_kv::{KvBackend, RangeSpec, TransactConfig, transact};
use hs_tables::keyspace::TypedKeyspace;

/// A durable task store over any `hs-kv` backend.
pub struct TablesTaskStore<B: KvBackend> {
    backend: B,
    tasks: TypedKeyspace<B::Keyspace, (String,)>,
}

impl<B: KvBackend> TablesTaskStore<B> {
    /// Opens (or creates) the tasks keyspace on `backend`.
    ///
    /// # Errors
    /// The backend's, if the keyspace cannot be opened.
    pub fn open(backend: B) -> Result<Self, hs_kv::KvError> {
        let tasks = TypedKeyspace::new(backend.keyspace("hs_admin.tasks")?);
        Ok(Self { backend, tasks })
    }
}

fn unavailable(e: impl std::fmt::Display) -> SourceError {
    SourceError::Unavailable(format!("task store: {e}"))
}

fn decode(bytes: &[u8]) -> Result<TaskRecord, SourceError> {
    serde_json::from_slice(bytes).map_err(unavailable)
}

#[async_trait]
impl<B: KvBackend + 'static> TaskStore for TablesTaskStore<B> {
    async fn put(&self, record: &TaskRecord) -> Result<(), SourceError> {
        let value = serde_json::to_vec(record).map_err(unavailable)?;
        let key = (record.task.id.clone(),);
        transact(&self.backend, TransactConfig::default(), |txn| {
            self.tasks
                .put(txn, &key, &value)
                .map_err(hs_kv::KvError::backend)
        })
        .map_err(unavailable)
    }

    async fn get(&self, id: &str) -> Result<Option<TaskRecord>, SourceError> {
        let snapshot = self.backend.snapshot();
        self.tasks
            .get(&snapshot, &(id.to_owned(),))
            .map_err(unavailable)?
            .as_deref()
            .map(decode)
            .transpose()
    }

    async fn list(&self) -> Result<Vec<TaskRecord>, SourceError> {
        let snapshot = self.backend.snapshot();
        let mut out = Vec::new();
        for item in self.tasks.range(&snapshot, RangeSpec::full()) {
            let (_key, value) = item.map_err(unavailable)?;
            out.push(decode(&value)?);
        }
        Ok(out)
    }

    async fn delete(&self, id: &str) -> Result<(), SourceError> {
        let key = (id.to_owned(),);
        transact(&self.backend, TransactConfig::default(), |txn| {
            self.tasks
                .delete(txn, &key)
                .map_err(hs_kv::KvError::backend)
        })
        .map_err(unavailable)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use hs_admin::model::{Actor, ActorKind, Task, TaskStatus};
    use hs_admin::tasks::TaskRegistry;
    use hs_kv::memory::MemoryBackend;

    use super::*;

    fn system() -> Actor {
        Actor {
            kind: ActorKind::System,
            id: "server".to_owned(),
            display_name: None,
            token_id: None,
            ip: None,
            user_agent: None,
        }
    }

    #[tokio::test]
    async fn a_task_left_running_is_failed_by_the_next_process_over_the_same_store() {
        let backend = MemoryBackend::new();
        let first = TaskRegistry::new(
            Arc::new(TablesTaskStore::open(backend.clone()).unwrap()),
            "single-node",
        );
        let running = first
            .spawn("rooms.purge_history", None, system(), |_| async {
                std::future::pending().await
            })
            .await
            .unwrap();
        let mut finished = Task::scheduled("appservices.replay", None, system());
        finished.status = TaskStatus::Succeeded;
        first.record_finished(finished.clone()).await.unwrap();

        // The process stops; the next one opens the same store.
        let second = TaskRegistry::new(
            Arc::new(TablesTaskStore::open(backend).unwrap()),
            "single-node",
        );
        assert_eq!(second.recover_interrupted().await.unwrap(), 1);
        let after = second.get(&running.id).await.unwrap().unwrap();
        assert_eq!(after.status, TaskStatus::Failed);
        assert_eq!(
            second.get(&finished.id).await.unwrap().unwrap().status,
            TaskStatus::Succeeded
        );
        assert_eq!(
            second
                .list(&hs_admin::tasks::TaskFilter::default())
                .await
                .unwrap()
                .len(),
            2
        );
    }
}
