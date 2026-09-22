//! What drives [`crate::scheduler::Scheduler::drain`]: one worker per appservice.
//!
//! The scheduler is explicit that it starts nothing itself -- "call this repeatedly" -- and until
//! this module nothing did, so a queued transaction stayed queued. The choice of driver it left
//! open is made here.
//!
//! One worker *per appservice*, because delivery to one appservice is strictly ordered and so
//! necessarily serial, while delivery to two of them has no reason to be. A single loop over all
//! of them would make every bridge wait on the slowest: one that accepts a connection and never
//! answers holds a `drain` for the sender's whole timeout, and a server with a WhatsApp bridge
//! stuck in that state would stop delivering to its Signal bridge too.
//!
//! A worker is started the first time its appservice is [`Delivery::nudge`]d and ends when the
//! appservice is removed. It sleeps on a [`tokio::sync::Notify`] -- `notify_one` keeps a permit,
//! so a nudge that arrives while the worker is mid-`drain` is not lost -- and also wakes on a
//! timer, because two things change what it should do without nudging it: a backoff elapsing, and
//! an operator resuming a paused appservice.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use hs_kv::KvBackend;
use tokio::sync::Notify;

use crate::error::AppserviceError;
use crate::scheduler::{DrainOutcome, Scheduler};

/// How often a worker whose oldest entry is backing off looks again. The backoff itself is kept
/// in the queue row against the scheduler's clock; this is only how promptly its end is noticed.
const RETRY_CHECK: Duration = Duration::from_secs(1);

/// How often a worker with nothing to do looks anyway. Covers whatever does not nudge: an
/// appservice being resumed, an entry being replayed from the admin interface.
const IDLE_CHECK: Duration = Duration::from_secs(15);

struct Worker {
    wake: Arc<Notify>,
    task: tokio::task::AbortHandle,
}

/// See the module docs.
pub struct Delivery<B: KvBackend + 'static> {
    scheduler: Arc<Scheduler<B>>,
    workers: Mutex<HashMap<String, Worker>>,
}

impl<B: KvBackend + 'static> Delivery<B> {
    /// Builds the driver for `scheduler`. Starts nothing until something is nudged.
    #[must_use]
    pub fn new(scheduler: Arc<Scheduler<B>>) -> Arc<Self> {
        Arc::new(Self {
            scheduler,
            workers: Mutex::new(HashMap::new()),
        })
    }

    /// Says that `appservice_id` may have something to deliver, starting its worker if it has
    /// none. Cheap, and safe to call for an appservice that turns out to have nothing queued.
    pub fn nudge(self: &Arc<Self>, appservice_id: &str) {
        let mut workers = self
            .workers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(worker) = workers.get(appservice_id)
            && !worker.task.is_finished()
        {
            worker.wake.notify_one();
            return;
        }
        let wake = Arc::new(Notify::new());
        let task = tokio::spawn(Self::work(
            self.scheduler.clone(),
            appservice_id.to_owned(),
            wake.clone(),
        ))
        .abort_handle();
        workers.insert(appservice_id.to_owned(), Worker { wake, task });
    }

    /// Stops every worker. A delivery in flight is abandoned, which is safe: its queue entries
    /// are still pending, and are sent again, under the same transaction ID, by whatever starts
    /// next.
    pub fn stop(&self) {
        let mut workers = self
            .workers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for (_, worker) in workers.drain() {
            worker.task.abort();
        }
    }

    async fn work(scheduler: Arc<Scheduler<B>>, appservice_id: String, wake: Arc<Notify>) {
        loop {
            let pause = match scheduler.drain(&appservice_id).await {
                // There may be more behind it.
                Ok(DrainOutcome::Delivered { .. }) => continue,
                Ok(DrainOutcome::Empty | DrainOutcome::Paused | DrainOutcome::NoUrl) => IDLE_CHECK,
                Ok(DrainOutcome::Waiting) => RETRY_CHECK,
                Ok(DrainOutcome::Failed {
                    count,
                    dead_lettered,
                    error,
                }) => {
                    tracing::warn!(
                        appservice = %appservice_id,
                        count,
                        dead_lettered,
                        %error,
                        "could not deliver to this appservice; it will be tried again"
                    );
                    RETRY_CHECK
                }
                Err(AppserviceError::NotFound(_)) => {
                    tracing::info!(appservice = %appservice_id, "appservice removed; its delivery worker is stopping");
                    return;
                }
                Err(error) => {
                    tracing::error!(appservice = %appservice_id, %error, "appservice delivery could not read its queue");
                    IDLE_CHECK
                }
            };
            let _ = tokio::time::timeout(pause, wake.notified()).await;
        }
    }
}

impl<B: KvBackend + 'static> Drop for Delivery<B> {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registration::Registration;
    use crate::registry::Registry;
    use crate::scheduler::{MockSender, SchedulerConfig, TransactionSender};
    use crate::transaction::Transaction;
    use async_trait::async_trait;
    use hs_auth::clock::SystemClock;
    use hs_kv::memory::MemoryBackend;
    use serde_json::{Value, json};

    fn registration(id: &str) -> Registration {
        Registration::parse_yaml(&format!(
            "id: {id}\nurl: 'http://{id}.local'\nas_token: as_{id}\nhs_token: hs_{id}\n\
             sender_localpart: {id}bot\nnamespaces: {{}}\n"
        ))
        .unwrap()
    }

    fn said(body: &str) -> Transaction {
        Transaction {
            events: vec![json!({"type": "m.room.message", "content": {"body": body}})],
            ..Transaction::default()
        }
    }

    fn scheduler_with(
        sender: Arc<dyn TransactionSender>,
        ids: &[&str],
    ) -> Arc<Scheduler<MemoryBackend>> {
        let registry = Arc::new(
            Registry::open(MemoryBackend::new(), ruma::server_name!("example.org")).unwrap(),
        );
        for id in ids {
            registry.add(&registration(id)).unwrap();
        }
        Arc::new(
            Scheduler::new(registry, Arc::new(SystemClock), sender).with_config(SchedulerConfig {
                base_backoff_ms: 1,
                max_backoff_ms: 5,
                ..SchedulerConfig::default()
            }),
        )
    }

    async fn eventually(what: &str, mut done: impl FnMut() -> bool) {
        for _ in 0..400 {
            if done() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("never happened: {what}");
    }

    #[tokio::test]
    async fn a_nudge_delivers_what_is_queued_and_keeps_going_until_the_queue_is_empty() {
        let sender = MockSender::new();
        let scheduler = scheduler_with(Arc::new(sender.clone()), &["irc"]);
        let delivery = Delivery::new(scheduler.clone());
        // More than one batch's worth.
        for n in 0..45 {
            scheduler
                .enqueue("irc", &said(&format!("message {n}")))
                .unwrap();
        }
        delivery.nudge("irc");

        let delivered = || -> usize {
            sender
                .calls()
                .iter()
                .map(|(_, _, body)| body["events"].as_array().map_or(0, Vec::len))
                .sum()
        };
        eventually("all 45 delivered", || delivered() == 45).await;
        assert!(
            sender.calls().len() >= 3,
            "20 to a batch: {}",
            sender.calls().len()
        );
    }

    #[tokio::test]
    async fn a_failed_delivery_is_tried_again_without_anybody_asking() {
        let sender = MockSender::new();
        sender.fail_next(2);
        let scheduler = scheduler_with(Arc::new(sender.clone()), &["irc"]);
        let delivery = Delivery::new(scheduler.clone());
        scheduler.enqueue("irc", &said("third time lucky")).unwrap();
        delivery.nudge("irc");

        // `calls` records what was accepted; two refusals come first.
        eventually("delivered after two failures", || sender.calls().len() == 1).await;
        assert_eq!(
            sender.calls()[0].2["events"][0]["content"]["body"],
            "third time lucky"
        );
    }

    /// An appservice that takes the connection and never answers.
    struct OneHangs {
        inner: MockSender,
    }

    #[async_trait]
    impl TransactionSender for OneHangs {
        async fn send(
            &self,
            url: &str,
            hs_token: &str,
            txn_id: &str,
            body: &Value,
        ) -> Result<(), String> {
            if url.contains("stuck") {
                std::future::pending::<()>().await;
            }
            self.inner.send(url, hs_token, txn_id, body).await
        }
    }

    #[tokio::test]
    async fn one_appservice_that_never_answers_does_not_hold_up_another() {
        let inner = MockSender::new();
        let scheduler = scheduler_with(
            Arc::new(OneHangs {
                inner: inner.clone(),
            }),
            &["stuck", "fine"],
        );
        let delivery = Delivery::new(scheduler.clone());
        scheduler.enqueue("stuck", &said("into the void")).unwrap();
        scheduler
            .enqueue("fine", &said("straight through"))
            .unwrap();
        delivery.nudge("stuck");
        delivery.nudge("fine");

        eventually("the healthy one delivered", || inner.calls().len() == 1).await;
        assert!(inner.calls()[0].0.contains("fine"));
    }

    #[tokio::test]
    async fn a_worker_ends_when_its_appservice_is_removed_and_a_new_one_starts_on_demand() {
        let sender = MockSender::new();
        let scheduler = scheduler_with(Arc::new(sender.clone()), &["irc"]);
        let delivery = Delivery::new(scheduler.clone());
        delivery.nudge("gone");
        eventually("the worker for nobody stopped", || {
            delivery.workers.lock().unwrap()["gone"].task.is_finished()
        })
        .await;

        scheduler.enqueue("irc", &said("still working")).unwrap();
        delivery.nudge("irc");
        eventually("delivered", || sender.calls().len() == 1).await;
    }
}
