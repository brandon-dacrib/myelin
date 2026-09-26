//! `hs-admin`'s [`AppserviceDirectory`] over this crate's [`Registry`]: what makes the admin
//! API's thirteen `appservices.*` operations, and the interface's Bridges section, say what the
//! server knows instead of `503`.
//!
//! The registry already spoke the admin API's vocabulary ([`Health`], [`BacklogEntry`], merge
//! patch updates, YAML export); this is the translation into `hs-admin`'s models and errors,
//! plus the two things the registry alone cannot do: send a ping ([`PingService`]) and put
//! dead-lettered transactions back ([`Scheduler::replay`]). A replay is followed by a nudge to
//! whatever drives delivery (`crate::delivery`), so that "replay" means "sent again now", not
//! "sent again within fifteen seconds".

use std::sync::Arc;

use async_trait::async_trait;
use hs_admin::model::{
    AdminAppservice, AdminAppserviceBacklogEntry, AdminAppserviceCreate, AdminAppserviceHealth,
    AdminAppserviceLinks, AdminAppserviceReplay, AdminAppserviceTokens,
};
use hs_admin::sources::{AdminAppserviceRegistration, AppserviceDirectory, SourceError};
use hs_kv::KvBackend;
use serde_json::Value;

use crate::error::AppserviceError;
use crate::ping::PingService;
use crate::registration::Registration;
use crate::registry::{BacklogEntry, Health, Registry};
use crate::scheduler::{ReplayRequest, Scheduler};
use crate::store::AppserviceRow;

/// Told the id of an appservice that has just had transactions put back in its queue.
pub type NudgeDelivery = Arc<dyn Fn(&str) + Send + Sync>;

/// See the module docs.
pub struct RegistryAppserviceDirectory<B: KvBackend + 'static> {
    registry: Arc<Registry<B>>,
    ping: Arc<PingService<B>>,
    scheduler: Arc<Scheduler<B>>,
    nudge: NudgeDelivery,
}

impl<B: KvBackend + 'static> RegistryAppserviceDirectory<B> {
    /// Builds the directory. `nudge` is called after a replay with the appservice's id.
    #[must_use]
    pub fn new(
        registry: Arc<Registry<B>>,
        ping: Arc<PingService<B>>,
        scheduler: Arc<Scheduler<B>>,
        nudge: NudgeDelivery,
    ) -> Self {
        Self {
            registry,
            ping,
            scheduler,
            nudge,
        }
    }

    fn view(&self, row: &AppserviceRow) -> Result<AdminAppservice, SourceError> {
        let health = self.registry.health(&row.id).map_err(to_source)?;
        Ok(AdminAppservice {
            id: row.id.clone(),
            sender_localpart: row.sender_localpart.clone(),
            url: row.url.clone(),
            namespaces: serde_json::to_value(&row.namespaces).unwrap_or(Value::Null),
            rate_limited: row.rate_limited,
            protocols: row.protocols.clone(),
            paused: row.paused,
            health: status_word(&health),
            created_at: hs_http::time::rfc3339_from_millis(
                i64::try_from(row.created_at_ms).unwrap_or(i64::MAX),
            ),
            bridge_type: row
                .extra
                .get(hs_admin::bridge_types::BRIDGE_TYPE_KEY)
                .and_then(Value::as_str)
                .map(str::to_owned),
            links: AdminAppserviceLinks::default(),
        })
    }
}

fn status_word(health: &Health) -> String {
    serde_json::to_value(health.status)
        .ok()
        .and_then(|v| v.as_str().map(str::to_owned))
        .unwrap_or_else(|| "unknown".to_owned())
}

fn admin_health(health: Health) -> AdminAppserviceHealth {
    AdminAppserviceHealth {
        status: status_word(&health),
        last_ping_at: health
            .last_ping_at_ms
            .map(|ms| hs_http::time::rfc3339_from_millis(i64::try_from(ms).unwrap_or(i64::MAX))),
        last_error: health.last_error,
    }
}

fn admin_backlog(entry: BacklogEntry) -> AdminAppserviceBacklogEntry {
    AdminAppserviceBacklogEntry {
        transaction_id: entry.transaction_id,
        age_ms: entry.age_ms,
        attempts: entry.attempts,
        last_error: entry.last_error,
        dead_lettered: entry.dead_lettered,
    }
}

/// The registry's errors as the admin API's. A conflict of any kind -- id, token, namespace --
/// is a `409` whose detail says which; a registration that does not parse is a `400`.
fn to_source(error: AppserviceError) -> SourceError {
    match error {
        AppserviceError::NotFound(_) => SourceError::NotFound,
        AppserviceError::AlreadyExists(_)
        | AppserviceError::TokenConflict { .. }
        | AppserviceError::NamespaceConflict { .. } => SourceError::Conflict(error.to_string()),
        AppserviceError::Registration(_) | AppserviceError::Namespace(_) => {
            SourceError::Invalid(error.to_string())
        }
        other => SourceError::Unavailable(other.to_string()),
    }
}

#[async_trait]
impl<B: KvBackend + 'static> AppserviceDirectory for RegistryAppserviceDirectory<B> {
    async fn list(&self) -> Result<Vec<AdminAppservice>, SourceError> {
        self.registry
            .list()
            .map_err(to_source)?
            .iter()
            .map(|row| self.view(row))
            .collect()
    }

    async fn get(&self, id: &str) -> Result<Option<AdminAppservice>, SourceError> {
        match self.registry.get(id).map_err(to_source)? {
            Some(row) => self.view(&row).map(Some),
            None => Ok(None),
        }
    }

    async fn create(&self, request: AdminAppserviceCreate) -> Result<AdminAppservice, SourceError> {
        let registration = match (request.registration, request.registration_yaml) {
            (Some(json), _) => {
                Registration::from_json(json).map_err(|e| SourceError::InvalidField {
                    pointer: "/registration",
                    detail: e.to_string(),
                })?
            }
            (None, Some(yaml)) => {
                Registration::parse_yaml(&yaml).map_err(|e| SourceError::InvalidField {
                    pointer: "/registration_yaml",
                    detail: e.to_string(),
                })?
            }
            (None, None) => {
                return Err(SourceError::InvalidField {
                    pointer: "/registration",
                    detail: "a registration is required, as JSON or as YAML".to_owned(),
                });
            }
        };
        let row = self.registry.add(&registration).map_err(to_source)?;
        self.view(&row)
    }

    async fn update(&self, id: &str, patch: Value) -> Result<AdminAppservice, SourceError> {
        let row = self.registry.update(id, &patch).map_err(to_source)?;
        self.view(&row)
    }

    async fn delete(&self, id: &str) -> Result<(), SourceError> {
        self.registry.remove(id).map_err(to_source)
    }

    async fn health(&self, id: &str) -> Result<AdminAppserviceHealth, SourceError> {
        self.registry
            .health(id)
            .map(admin_health)
            .map_err(to_source)
    }

    async fn backlog(&self, id: &str) -> Result<Vec<AdminAppserviceBacklogEntry>, SourceError> {
        if self.registry.get(id).map_err(to_source)?.is_none() {
            return Err(SourceError::NotFound);
        }
        Ok(self
            .registry
            .backlog(id)
            .map_err(to_source)?
            .into_iter()
            .map(admin_backlog)
            .collect())
    }

    async fn pause(&self, id: &str) -> Result<AdminAppservice, SourceError> {
        let row = self.registry.pause(id).map_err(to_source)?;
        self.view(&row)
    }

    async fn resume(&self, id: &str) -> Result<AdminAppservice, SourceError> {
        let row = self.registry.resume(id).map_err(to_source)?;
        // Delivery may have been waiting on this.
        (self.nudge)(id);
        self.view(&row)
    }

    async fn rotate_tokens(&self, id: &str) -> Result<AdminAppserviceTokens, SourceError> {
        let row = self.registry.rotate_tokens(id).map_err(to_source)?;
        Ok(AdminAppserviceTokens {
            as_token: row.as_token,
            hs_token: row.hs_token,
        })
    }

    async fn registration(&self, id: &str) -> Result<AdminAppserviceRegistration, SourceError> {
        let row = self
            .registry
            .get(id)
            .map_err(to_source)?
            .ok_or(SourceError::NotFound)?;
        let registration = row.to_registration().map_err(to_source)?;
        Ok(AdminAppserviceRegistration {
            json: registration.to_json(),
            yaml: registration.to_yaml(),
        })
    }

    async fn ping(&self, id: &str) -> Result<AdminAppserviceHealth, SourceError> {
        // The outcome is recorded in the health row either way; that is the answer.
        let _ = self.ping.ping(id, None).await.map_err(to_source)?;
        self.registry
            .health(id)
            .map(admin_health)
            .map_err(to_source)
    }

    async fn replay(&self, id: &str, request: AdminAppserviceReplay) -> Result<usize, SourceError> {
        let since_ms = match request.since.as_deref() {
            Some(since) => Some(
                hs_http::time::parse_rfc3339(since)
                    .map(|dt| u64::try_from(dt.unix_timestamp() * 1000).unwrap_or(0))
                    .map_err(|e| SourceError::InvalidField {
                        pointer: "/since",
                        detail: format!("not an RFC 3339 timestamp: {e}"),
                    })?,
            ),
            None => None,
        };
        let replayed = self
            .scheduler
            .replay(
                id,
                &ReplayRequest {
                    transaction_ids: request.transaction_ids,
                    since_ms,
                },
            )
            .map_err(to_source)?;
        if replayed > 0 {
            (self.nudge)(id);
        }
        Ok(replayed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ping::PingTransport;
    use crate::scheduler::MockSender;
    use crate::transaction::Transaction;
    use hs_auth::clock::FixedClock;
    use hs_kv::memory::MemoryBackend;
    use serde_json::json;
    use std::sync::Mutex;

    struct RefusingTransport;

    #[async_trait]
    impl PingTransport for RefusingTransport {
        async fn ping(
            &self,
            _url: &str,
            _hs_token: &str,
            _transaction_id: Option<&str>,
        ) -> Result<(), crate::ping::PingTransportError> {
            Err(crate::ping::PingTransportError::ConnectionFailed(
                "connection refused".into(),
            ))
        }
    }

    /// A directory over a fresh registry, the scheduler it replays through, and who it nudged.
    struct Fixture {
        directory: RegistryAppserviceDirectory<MemoryBackend>,
        scheduler: Arc<Scheduler<MemoryBackend>>,
        nudged: Arc<Mutex<Vec<String>>>,
    }

    fn directory() -> Fixture {
        let clock = Arc::new(FixedClock::new(1_700_000_000_000));
        let registry = Arc::new(
            Registry::open(MemoryBackend::new(), ruma::server_name!("example.org"))
                .unwrap()
                .with_clock(clock.clone()),
        );
        let sender = MockSender::new();
        sender.fail_next(usize::MAX);
        let scheduler = Arc::new(
            Scheduler::new(registry.clone(), clock, Arc::new(sender)).with_config(
                crate::scheduler::SchedulerConfig {
                    max_attempts: 1,
                    ..crate::scheduler::SchedulerConfig::default()
                },
            ),
        );
        let ping = Arc::new(PingService::new(
            registry.clone(),
            Arc::new(RefusingTransport),
        ));
        let nudged = Arc::new(Mutex::new(Vec::new()));
        let directory = RegistryAppserviceDirectory::new(registry, ping, scheduler.clone(), {
            let nudged = nudged.clone();
            Arc::new(move |id: &str| nudged.lock().unwrap().push(id.to_owned()))
        });
        Fixture {
            directory,
            scheduler,
            nudged,
        }
    }

    fn irc() -> Value {
        json!({
            "id": "irc",
            "url": "http://irc.local:9898",
            "as_token": "as_secret",
            "hs_token": "hs_secret",
            "sender_localpart": "ircbot",
            "rate_limited": false,
            "protocols": ["irc"],
            "namespaces": {"users": [{"regex": "@irc_.*:example\\.org", "exclusive": true}]}
        })
    }

    #[tokio::test]
    async fn a_registration_goes_in_as_json_and_comes_out_the_same_in_both_notations() {
        let Fixture { directory, .. } = directory();
        let created = directory
            .create(AdminAppserviceCreate {
                registration: Some(irc()),
                registration_yaml: None,
            })
            .await
            .unwrap();
        assert_eq!(created.id, "irc");
        assert_eq!(created.health, "unknown");
        assert_eq!(created.protocols, vec!["irc"]);
        assert_eq!(created.created_at, "2023-11-14T22:13:20.000Z");
        assert_eq!(
            created.namespaces["users"][0]["regex"],
            "@irc_.*:example\\.org"
        );

        let registration = directory.registration("irc").await.unwrap();
        assert_eq!(registration.json["as_token"], "as_secret");
        assert!(registration.yaml.contains("sender_localpart: ircbot"));

        // Again is a conflict; a different id claiming the same exclusive namespace is too.
        let again = directory
            .create(AdminAppserviceCreate {
                registration: Some(irc()),
                registration_yaml: None,
            })
            .await
            .unwrap_err();
        assert!(matches!(again, SourceError::Conflict(_)), "{again:?}");
        let mut rival = irc();
        rival["id"] = json!("irc2");
        rival["as_token"] = json!("other_as");
        rival["hs_token"] = json!("other_hs");
        rival["sender_localpart"] = json!("ircbot2");
        let rival = directory
            .create(AdminAppserviceCreate {
                registration: Some(rival),
                registration_yaml: None,
            })
            .await
            .unwrap_err();
        assert!(matches!(rival, SourceError::Conflict(_)), "{rival:?}");

        // Not a registration at all is a 400 that names the field.
        let junk = directory
            .create(AdminAppserviceCreate {
                registration: Some(json!({"id": "x"})),
                registration_yaml: None,
            })
            .await
            .unwrap_err();
        assert!(
            matches!(
                junk,
                SourceError::InvalidField {
                    pointer: "/registration",
                    ..
                }
            ),
            "{junk:?}"
        );
    }

    #[tokio::test]
    async fn health_pause_ping_and_tokens_read_back_from_the_registry() {
        let Fixture {
            directory, nudged, ..
        } = directory();
        directory
            .create(AdminAppserviceCreate {
                registration: Some(irc()),
                registration_yaml: None,
            })
            .await
            .unwrap();

        let paused = directory.pause("irc").await.unwrap();
        assert!(paused.paused);
        assert_eq!(paused.health, "paused");
        assert_eq!(directory.health("irc").await.unwrap().status, "paused");
        directory.resume("irc").await.unwrap();
        assert_eq!(nudged.lock().unwrap().as_slice(), ["irc"]);

        let health = directory.ping("irc").await.unwrap();
        assert_eq!(health.status, "degraded", "{health:?}");
        assert!(health.last_ping_at.is_some());
        assert!(health.last_error.as_deref().unwrap().contains("refused"));

        let tokens = directory.rotate_tokens("irc").await.unwrap();
        assert_ne!(tokens.as_token, "as_secret");
        assert_eq!(
            directory.registration("irc").await.unwrap().json["hs_token"],
            tokens.hs_token
        );

        assert!(matches!(
            directory.health("nope").await.unwrap_err(),
            SourceError::NotFound
        ));
        assert!(matches!(
            directory.backlog("nope").await.unwrap_err(),
            SourceError::NotFound
        ));
    }

    #[tokio::test]
    async fn a_dead_lettered_transaction_is_in_the_backlog_and_a_replay_puts_it_back_and_nudges() {
        let Fixture {
            directory,
            scheduler,
            nudged,
        } = directory();
        directory
            .create(AdminAppserviceCreate {
                registration: Some(irc()),
                registration_yaml: None,
            })
            .await
            .unwrap();
        scheduler
            .enqueue(
                "irc",
                &Transaction {
                    events: vec![json!({"type": "m.room.message"})],
                    ..Transaction::default()
                },
            )
            .unwrap();
        // One attempt is the budget, and the sender refuses everything.
        let outcome = scheduler.drain("irc").await.unwrap();
        assert!(
            matches!(
                outcome,
                crate::scheduler::DrainOutcome::Failed {
                    dead_lettered: 1,
                    ..
                }
            ),
            "{outcome:?}"
        );

        let backlog = directory.backlog("irc").await.unwrap();
        assert_eq!(backlog.len(), 1);
        assert!(backlog[0].dead_lettered);
        assert_eq!(backlog[0].attempts, 1);

        let replayed = directory
            .replay("irc", AdminAppserviceReplay::default())
            .await
            .unwrap();
        assert_eq!(replayed, 1);
        assert_eq!(nudged.lock().unwrap().as_slice(), ["irc"]);
        let backlog = directory.backlog("irc").await.unwrap();
        assert!(!backlog[0].dead_lettered);

        let bad_since = directory
            .replay(
                "irc",
                AdminAppserviceReplay {
                    since: Some("yesterday".into()),
                    ..AdminAppserviceReplay::default()
                },
            )
            .await
            .unwrap_err();
        assert!(matches!(
            bad_since,
            SourceError::InvalidField {
                pointer: "/since",
                ..
            }
        ));

        directory.delete("irc").await.unwrap();
        assert!(directory.get("irc").await.unwrap().is_none());
    }
}
