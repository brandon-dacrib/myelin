//! The migration engine against a real Synapse database (`tests/fixtures/synapse-small`, see its
//! README), with this server's stores stood in for by an in-memory target: the reader, the
//! copy, pausing and resuming, abandoning, carrying on after a restart, verification that
//! notices a difference, and a cutover.
//!
//! `crates/hs-cli/tests/migration.rs` runs the same fixture through the real `hs` binary.
//!
//! Needs PostgreSQL (`HS_MIGRATION_TEST_POSTGRES_DSN`, or the local
//! `postgres://postgres:hspg@127.0.0.1:5439/postgres`); every test skips, saying so, without one.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use hs_admin::migration::{MigrationSource, MigrationStartRequest};
use hs_admin::model::Actor;
use hs_admin::sources::SourceError;
use hs_admin::tasks::TaskRegistry;
use hs_compat::migration::engine::MigratorParts;
use hs_compat::migration::model::{
    MigrationRecord, Phase, Stream, SynapseAccessToken, SynapseAccountData, SynapseDevice,
    SynapseEvent, SynapseMedia, SynapseRoom, SynapseUser,
};
use hs_compat::migration::order::replay_order;
use hs_compat::migration::{
    Imported, InMemoryMigrationStore, MigrationStore, MigrationTarget, Migrator, RoomOutcome,
    SourceConfigs, SynapseSource, TargetError, TargetMedia, TargetRoom, TargetUser,
};
use hs_config::migration::{SynapseDatabaseConfig, SynapseSourceConfig};
use serde_json::Value;
use tokio::sync::Semaphore;

fn fixture_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/synapse-small")
}

/// A fresh database holding the fixture, dropped when the test ends.
struct Fixture {
    admin_dsn: String,
    name: String,
    config: SynapseSourceConfig,
}

impl Fixture {
    async fn load() -> Option<Self> {
        let admin_dsn = std::env::var("HS_MIGRATION_TEST_POSTGRES_DSN")
            .unwrap_or_else(|_| "postgres://postgres:hspg@127.0.0.1:5439/postgres".to_owned());
        let parsed: tokio_postgres::Config = admin_dsn.parse().ok()?;
        let (admin, connection) = match parsed.connect(tokio_postgres::NoTls).await {
            Ok(pair) => pair,
            Err(e) => {
                eprintln!(
                    "SKIP: the migration tests need PostgreSQL at {admin_dsn:?}: {e}. Start one \
                     with: docker run --rm -d -e POSTGRES_PASSWORD=hspg -p 127.0.0.1:5439:5432 \
                     postgres:17"
                );
                return None;
            }
        };
        tokio::spawn(connection);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let name = format!("synapse_small_{}_{nanos}", std::process::id());
        admin
            .batch_execute(&format!(
                "CREATE DATABASE {name} ENCODING 'UTF8' LC_COLLATE 'C' LC_CTYPE 'C' TEMPLATE template0"
            ))
            .await
            .unwrap();
        let mut db = parsed.clone();
        db.dbname(&name);
        let (client, connection) = db.connect(tokio_postgres::NoTls).await.unwrap();
        tokio::spawn(connection);
        for file in ["schema.sql", "data.sql"] {
            let sql = std::fs::read_to_string(fixture_dir().join(file)).unwrap();
            client.batch_execute(&sql).await.unwrap();
        }
        let host = match parsed.get_hosts().first() {
            Some(tokio_postgres::config::Host::Tcp(host)) => host.clone(),
            _ => "127.0.0.1".to_owned(),
        };
        let config = SynapseSourceConfig {
            database: SynapseDatabaseConfig {
                host,
                port: parsed.get_ports().first().copied().unwrap_or(5432),
                database: name.clone(),
                user: parsed.get_user().unwrap_or("postgres").to_owned(),
                password: String::from_utf8_lossy(parsed.get_password().unwrap_or_default())
                    .into_owned()
                    .into(),
            },
            media_store_path: Some(fixture_dir().join("media_store")),
            batch_size: 2,
        };
        Some(Self {
            admin_dsn,
            name,
            config,
        })
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let dsn = self.admin_dsn.clone();
        let name = self.name.clone();
        // Dropped from inside a runtime: do it on a thread of its own.
        let _ = std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            runtime.block_on(async move {
                if let Ok((client, connection)) = dsn
                    .parse::<tokio_postgres::Config>()
                    .unwrap()
                    .connect(tokio_postgres::NoTls)
                    .await
                {
                    tokio::spawn(connection);
                    let _ = client
                        .batch_execute(&format!("DROP DATABASE IF EXISTS {name} WITH (FORCE)"))
                        .await;
                }
            });
        })
        .join();
    }
}

struct Configs(Option<SynapseSourceConfig>);

#[async_trait]
impl SourceConfigs for Configs {
    async fn source(&self, pointer: &str) -> Result<Option<SynapseSourceConfig>, String> {
        Ok(if pointer == "/migration/synapse" {
            self.0.clone()
        } else {
            None
        })
    }
}

/// `(user, room, type)`.
type AccountDataKey = (String, Option<String>, String);

/// This server's stores, in memory. Accounts can be held up (`gate`) to catch a copy part way.
#[derive(Default)]
struct MemoryTarget {
    users: Mutex<HashMap<String, TargetUser>>,
    devices: Mutex<HashMap<(String, String), Option<String>>>,
    tokens: Mutex<HashMap<String, (String, Option<String>)>>,
    account_data: Mutex<HashMap<AccountDataKey, Value>>,
    rooms: Mutex<HashMap<String, TargetRoom>>,
    media: Mutex<HashMap<String, TargetMedia>>,
    gate: Option<Arc<Semaphore>>,
}

#[async_trait]
impl MigrationTarget for MemoryTarget {
    fn server_name(&self) -> &str {
        "fixture.test"
    }

    async fn import_user(&self, user: &SynapseUser) -> Result<Imported, TargetError> {
        if let Some(gate) = &self.gate {
            gate.acquire().await.unwrap().forget();
        }
        let ours = TargetUser {
            password_hash: user.password_hash.clone(),
            displayname: user.displayname.clone(),
            avatar_url: user.avatar_url.clone(),
            admin: user.admin,
            deactivated: user.deactivated,
        };
        let mut users = self.users.lock().unwrap();
        Ok(match users.insert(user.user_id.clone(), ours.clone()) {
            None => Imported::Created,
            Some(before) if before == ours => Imported::AlreadyThere,
            Some(_) => Imported::Updated,
        })
    }

    async fn import_device(&self, device: &SynapseDevice) -> Result<Imported, TargetError> {
        let key = (device.user_id.clone(), device.device_id.clone());
        let before = self
            .devices
            .lock()
            .unwrap()
            .insert(key, device.display_name.clone());
        Ok(if before.is_some() {
            Imported::AlreadyThere
        } else {
            Imported::Created
        })
    }

    async fn import_access_token(
        &self,
        token: &SynapseAccessToken,
    ) -> Result<Imported, TargetError> {
        let before = self.tokens.lock().unwrap().insert(
            token.token.clone(),
            (token.user_id.clone(), token.device_id.clone()),
        );
        Ok(if before.is_some() {
            Imported::AlreadyThere
        } else {
            Imported::Created
        })
    }

    async fn import_account_data(
        &self,
        data: &SynapseAccountData,
    ) -> Result<Imported, TargetError> {
        self.account_data.lock().unwrap().insert(
            (
                data.user_id.clone(),
                data.room_id.clone(),
                data.data_type.clone(),
            ),
            data.content.clone(),
        );
        Ok(Imported::Created)
    }

    async fn import_room(
        &self,
        room: &SynapseRoom,
        events: &[&SynapseEvent],
    ) -> Result<RoomOutcome, TargetError> {
        let mut rooms = self.rooms.lock().unwrap();
        let held = rooms.entry(room.room_id.clone()).or_default();
        let mut outcome = RoomOutcome::default();
        for event in events {
            if !held.event_ids.insert(event.event_id.clone()) {
                outcome.already_there += 1;
                continue;
            }
            outcome.stored += 1;
            if let Some(key) = event.json.get("state_key").and_then(Value::as_str) {
                let kind = event.json["type"].as_str().unwrap_or_default().to_owned();
                held.current_state
                    .insert((kind, key.to_owned()), event.event_id.clone());
            }
        }
        outcome.redactions = room.redactions.len() as u64;
        outcome.aliases = room.aliases.len() as u64;
        Ok(outcome)
    }

    async fn import_media(
        &self,
        media: &SynapseMedia,
        bytes: Option<Vec<u8>>,
    ) -> Result<Imported, TargetError> {
        let Some(bytes) = bytes else {
            return Ok(Imported::Skipped("no file".to_owned()));
        };
        self.media.lock().unwrap().insert(
            media.media_id.clone(),
            TargetMedia {
                content_type: media.content_type.clone().unwrap_or_default(),
                bytes: Some(bytes),
            },
        );
        Ok(Imported::Created)
    }

    async fn user(&self, user_id: &str) -> Result<Option<TargetUser>, TargetError> {
        Ok(self.users.lock().unwrap().get(user_id).cloned())
    }

    async fn device(
        &self,
        user_id: &str,
        device_id: &str,
    ) -> Result<Option<Option<String>>, TargetError> {
        Ok(self
            .devices
            .lock()
            .unwrap()
            .get(&(user_id.to_owned(), device_id.to_owned()))
            .cloned())
    }

    async fn access_token(
        &self,
        token: &str,
    ) -> Result<Option<(String, Option<String>)>, TargetError> {
        Ok(self.tokens.lock().unwrap().get(token).cloned())
    }

    async fn account_data(
        &self,
        user_id: &str,
        room_id: Option<&str>,
        data_type: &str,
    ) -> Result<Option<Value>, TargetError> {
        Ok(self
            .account_data
            .lock()
            .unwrap()
            .get(&(
                user_id.to_owned(),
                room_id.map(str::to_owned),
                data_type.to_owned(),
            ))
            .cloned())
    }

    async fn room(&self, room_id: &str) -> Result<Option<TargetRoom>, TargetError> {
        Ok(self.rooms.lock().unwrap().get(room_id).cloned())
    }

    async fn media(&self, media_id: &str) -> Result<Option<TargetMedia>, TargetError> {
        Ok(self.media.lock().unwrap().get(media_id).cloned())
    }
}

fn operator() -> Actor {
    let mut actor = Actor::system();
    actor.id = "@ops:fixture.test".to_owned();
    actor
}

struct Rig {
    migrator: Arc<Migrator>,
    store: Arc<InMemoryMigrationStore>,
    target: Arc<MemoryTarget>,
    tasks: Arc<TaskRegistry>,
}

fn rig(
    config: Option<SynapseSourceConfig>,
    target: MemoryTarget,
    store: Option<Arc<InMemoryMigrationStore>>,
) -> Rig {
    let store = store.unwrap_or_else(|| Arc::new(InMemoryMigrationStore::new()));
    let target = Arc::new(target);
    let tasks = TaskRegistry::in_memory();
    let migrator = Migrator::new(MigratorParts {
        store: store.clone(),
        target: target.clone(),
        configs: Arc::new(Configs(config)),
        tasks: tasks.clone(),
        events: None,
        observer: None,
        sample_size: 25,
    });
    Rig {
        migrator,
        store,
        target,
        tasks,
    }
}

async fn reaches(store: &InMemoryMigrationStore, phase: Phase) -> MigrationRecord {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let record = store.load().await.unwrap();
        if record.phase == phase {
            return record;
        }
        assert!(
            Instant::now() < deadline,
            "never reached {phase:?}: {record:#?}\n{:#?}",
            store.log().await.unwrap()
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn copied(record: &MigrationRecord, stream: Stream) -> u64 {
    record.stream(stream).map_or(0, |s| s.copied)
}

#[tokio::test]
async fn the_reader_sees_what_synapse_holds() {
    let Some(fixture) = Fixture::load().await else {
        return;
    };
    let source = SynapseSource::connect(&fixture.config).await.unwrap();
    assert_eq!(
        source.server_name().await.unwrap().as_deref(),
        Some("fixture.test")
    );
    let users = source.users(None, 100).await.unwrap();
    assert_eq!(users.len(), 4);
    let alice = users
        .iter()
        .find(|u| u.user_id == "@alice:fixture.test")
        .unwrap();
    assert!(alice.admin);
    assert!(
        alice
            .password_hash
            .as_deref()
            .is_some_and(|h| h.starts_with("$2b$"))
    );
    assert_eq!(alice.displayname.as_deref(), Some("Alice Liddell"));
    assert!(alice.created_at_ms > 1_000_000_000_000);
    let dave = users
        .iter()
        .find(|u| u.user_id == "@dave:fixture.test")
        .unwrap();
    assert!(dave.deactivated);

    // Keyset paging reads each row once.
    let first = source.users(None, 3).await.unwrap();
    let rest = source
        .users(Some(&first.last().unwrap().user_id), 3)
        .await
        .unwrap();
    assert_eq!(first.len() + rest.len(), 4);

    for room_id in source.room_ids(None, 10).await.unwrap() {
        let room = source.room(&room_id).await.unwrap().unwrap();
        assert_eq!(room.room_version, "11");
        let plan = replay_order(&room).unwrap();
        assert_eq!(plan.events.len(), room.events.len(), "{room_id}");
        assert_eq!(
            plan.events[0].json["type"], "m.room.create",
            "the create event goes first"
        );
    }
    let data = source.account_data(None, 100).await.unwrap();
    assert!(
        data.iter()
            .any(|(d, _)| d.data_type == "m.tag" && d.content["tags"]["m.favourite"].is_object()),
        "{data:?}"
    );
    let media = source.media(None, 10).await.unwrap();
    assert_eq!(media.len(), 2);
    for (item, _) in &media {
        let bytes = source.media_bytes(&item.media_id).await.unwrap().unwrap();
        assert_eq!(Some(bytes.len() as u64), item.length);
    }
}

#[tokio::test]
async fn a_copy_is_verified_and_cut_over_and_nothing_can_follow_it() {
    let Some(fixture) = Fixture::load().await else {
        return;
    };
    let rig = rig(Some(fixture.config.clone()), MemoryTarget::default(), None);
    let change = rig
        .migrator
        .start(&MigrationStartRequest::default(), &operator())
        .await
        .unwrap();
    assert_eq!(change.from, "idle");
    assert_eq!(change.status.status, "copying");
    let record = reaches(&rig.store, Phase::ReadyForCutover).await;
    assert_eq!(copied(&record, Stream::Users), 4);
    assert_eq!(copied(&record, Stream::Devices), 6);
    assert_eq!(copied(&record, Stream::AccessTokens), 6);
    assert_eq!(copied(&record, Stream::AccountData), 3);
    assert_eq!(copied(&record, Stream::Rooms), 2);
    assert_eq!(copied(&record, Stream::Media), 2);
    assert!(record.streams.iter().all(|s| s.done && s.failed == 0));

    // Verification passes, and then notices a difference.
    rig.migrator.verify(&operator()).await.unwrap();
    let record = reaches(&rig.store, Phase::ReadyForCutover).await;
    let report = record.verification.clone().unwrap();
    assert!(report.passed, "{report:#?}");
    assert!(
        report
            .streams
            .iter()
            .any(|s| s.name == "events" && s.source_count > 30)
    );
    rig.target
        .users
        .lock()
        .unwrap()
        .get_mut("@bob:fixture.test")
        .unwrap()
        .password_hash = Some("tampered".to_owned());
    rig.migrator.verify(&operator()).await.unwrap();
    // Verifying, then back to where it was.
    let record = loop {
        let record = reaches(&rig.store, Phase::ReadyForCutover).await;
        if record.verification.as_ref().is_some_and(|v| !v.passed) {
            break record;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    let users = &record.verification.unwrap().streams[0];
    assert!(
        users
            .mismatches
            .iter()
            .any(|m| m.contains("@bob:fixture.test"))
            || users.sampled < 4,
        "{users:?}"
    );

    // The cutover's final pass brings bob's password back, verifies, and completes.
    rig.migrator.cutover(&operator()).await.unwrap();
    let record = reaches(&rig.store, Phase::Completed).await;
    assert!(record.verification.as_ref().unwrap().passed);
    assert_eq!(record.cutover_by.as_deref(), Some("@ops:fixture.test"));
    assert_ne!(
        rig.target.users.lock().unwrap()["@bob:fixture.test"].password_hash,
        Some("tampered".to_owned())
    );
    for refused in [
        rig.migrator
            .start(&MigrationStartRequest::default(), &operator())
            .await
            .err(),
        rig.migrator.abort(&operator()).await.err(),
        rig.migrator.cutover(&operator()).await.err(),
    ] {
        assert!(
            matches!(refused, Some(SourceError::Conflict(_))),
            "{refused:?}"
        );
    }
    let log = rig.store.log().await.unwrap();
    assert!(
        log.iter()
            .any(|e| e.message.contains("cut over by @ops:fixture.test"))
    );
}

#[tokio::test]
async fn a_paused_copy_resumes_where_it_stopped_and_an_abort_leaves_what_was_copied() {
    let Some(fixture) = Fixture::load().await else {
        return;
    };
    let gate = Arc::new(Semaphore::new(1));
    let target = MemoryTarget {
        gate: Some(gate.clone()),
        ..MemoryTarget::default()
    };
    let rig = rig(Some(fixture.config.clone()), target, None);
    rig.migrator
        .start(&MigrationStartRequest::default(), &operator())
        .await
        .unwrap();
    // One account copied; the copy waits on the second.
    while rig.target.users.lock().unwrap().is_empty() {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let task = rig.store.load().await.unwrap().task_id.unwrap();
    let paused = rig.migrator.pause(&operator()).await.unwrap().unwrap();
    assert_eq!(paused.status.status, "paused");
    assert!(rig.migrator.pause(&operator()).await.unwrap().is_none());
    assert_eq!(
        rig.tasks.get(&task).await.unwrap().unwrap().status,
        hs_admin::model::TaskStatus::Cancelled
    );
    // Nothing more is copied while paused, even with the gate open.
    gate.add_permits(1_000);
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(rig.target.users.lock().unwrap().len(), 1);
    assert_eq!(rig.store.load().await.unwrap().phase, Phase::Paused);

    rig.migrator.resume(&operator()).await.unwrap().unwrap();
    let record = reaches(&rig.store, Phase::ReadyForCutover).await;
    assert_eq!(copied(&record, Stream::Users), 4, "{record:#?}");
    assert_eq!(copied(&record, Stream::Rooms), 2);

    // Abort, then start again: nothing is copied twice.
    let aborted = rig.migrator.abort(&operator()).await.unwrap().unwrap();
    assert_eq!(aborted.from, "ready_for_cutover");
    assert_eq!(aborted.status.status, "aborted");
    assert_eq!(rig.target.users.lock().unwrap().len(), 4);
    rig.migrator
        .start(&MigrationStartRequest::default(), &operator())
        .await
        .unwrap();
    let record = reaches(&rig.store, Phase::ReadyForCutover).await;
    assert_eq!(copied(&record, Stream::Users), 4);
}

#[tokio::test]
async fn a_copy_interrupted_by_a_restart_carries_on_in_the_next_process() {
    let Some(fixture) = Fixture::load().await else {
        return;
    };
    // The first process copied two accounts and stopped mid-copy.
    let store = Arc::new(InMemoryMigrationStore::new());
    let mut record = MigrationRecord {
        phase: Phase::Copying,
        source: Some(fixture.config.database.describe()),
        source_ref: Some("/migration/synapse".to_owned()),
        run: 7,
        task_id: Some("gone".to_owned()),
        ..MigrationRecord::default()
    };
    let users = record.stream_mut(Stream::Users);
    users.copied = 2;
    users.total = Some(4);
    users.checkpoint = Some("@bob:fixture.test".to_owned());
    store.save(&record).await.unwrap();

    let next = rig(
        Some(fixture.config.clone()),
        MemoryTarget::default(),
        Some(store),
    );
    assert_eq!(next.migrator.recover().await.unwrap(), Some(Phase::Copying));
    let record = reaches(&next.store, Phase::ReadyForCutover).await;
    // Only the accounts after the checkpoint were read again.
    let here: HashSet<String> = next.target.users.lock().unwrap().keys().cloned().collect();
    assert_eq!(
        here,
        HashSet::from([
            "@carol:fixture.test".to_owned(),
            "@dave:fixture.test".to_owned()
        ])
    );
    assert_eq!(copied(&record, Stream::Users), 4);
    assert!(record.run > 7);
    assert!(
        next.store
            .log()
            .await
            .unwrap()
            .iter()
            .any(|e| e.message.contains("restarted"))
    );
    // A second recover finds nothing running.
    assert_eq!(next.migrator.recover().await.unwrap(), None);
}

#[tokio::test]
async fn a_start_is_refused_without_a_source_or_for_another_server() {
    let none = rig(None, MemoryTarget::default(), None);
    let refused = none
        .migrator
        .start(&MigrationStartRequest::default(), &operator())
        .await
        .unwrap_err();
    assert!(
        matches!(&refused, SourceError::Invalid(d) if d.contains("/migration/synapse")),
        "{refused:?}"
    );
    assert_eq!(none.store.load().await.unwrap().phase, Phase::Idle);

    let Some(fixture) = Fixture::load().await else {
        return;
    };
    struct Elsewhere(MemoryTarget);
    #[async_trait]
    impl MigrationTarget for Elsewhere {
        fn server_name(&self) -> &str {
            "other.example"
        }
        async fn import_user(&self, u: &SynapseUser) -> Result<Imported, TargetError> {
            self.0.import_user(u).await
        }
        async fn import_device(&self, d: &SynapseDevice) -> Result<Imported, TargetError> {
            self.0.import_device(d).await
        }
        async fn import_access_token(
            &self,
            t: &SynapseAccessToken,
        ) -> Result<Imported, TargetError> {
            self.0.import_access_token(t).await
        }
        async fn import_account_data(
            &self,
            d: &SynapseAccountData,
        ) -> Result<Imported, TargetError> {
            self.0.import_account_data(d).await
        }
        async fn import_room(
            &self,
            r: &SynapseRoom,
            e: &[&SynapseEvent],
        ) -> Result<RoomOutcome, TargetError> {
            self.0.import_room(r, e).await
        }
        async fn import_media(
            &self,
            m: &SynapseMedia,
            b: Option<Vec<u8>>,
        ) -> Result<Imported, TargetError> {
            self.0.import_media(m, b).await
        }
        async fn user(&self, u: &str) -> Result<Option<TargetUser>, TargetError> {
            self.0.user(u).await
        }
        async fn device(&self, u: &str, d: &str) -> Result<Option<Option<String>>, TargetError> {
            self.0.device(u, d).await
        }
        async fn access_token(
            &self,
            t: &str,
        ) -> Result<Option<(String, Option<String>)>, TargetError> {
            self.0.access_token(t).await
        }
        async fn account_data(
            &self,
            u: &str,
            r: Option<&str>,
            t: &str,
        ) -> Result<Option<Value>, TargetError> {
            self.0.account_data(u, r, t).await
        }
        async fn room(&self, r: &str) -> Result<Option<TargetRoom>, TargetError> {
            self.0.room(r).await
        }
        async fn media(&self, m: &str) -> Result<Option<TargetMedia>, TargetError> {
            self.0.media(m).await
        }
    }
    let store = Arc::new(InMemoryMigrationStore::new());
    let migrator = Migrator::new(MigratorParts {
        store: store.clone(),
        target: Arc::new(Elsewhere(MemoryTarget::default())),
        configs: Arc::new(Configs(Some(fixture.config.clone()))),
        tasks: TaskRegistry::in_memory(),
        events: None,
        observer: None,
        sample_size: 5,
    });
    let refused = migrator
        .start(&MigrationStartRequest::default(), &operator())
        .await
        .unwrap_err();
    assert!(
        matches!(&refused, SourceError::Invalid(d) if d.contains("fixture.test") && d.contains("other.example")),
        "{refused:?}"
    );

    // And an unreachable database is said to be unreachable.
    let mut unreachable = fixture.config.clone();
    unreachable.database.port = 1;
    let rig = rig(Some(unreachable), MemoryTarget::default(), None);
    let refused = rig
        .migrator
        .start(&MigrationStartRequest::default(), &operator())
        .await
        .unwrap_err();
    assert!(
        matches!(&refused, SourceError::Invalid(d) if d.contains("could not connect")),
        "{refused:?}"
    );
}
