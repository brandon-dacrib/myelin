//! The manager over an in-memory store and an in-memory appservice directory: what it does to
//! offerings, registrations and instances without a server around it. The state machine is
//! driven by hand (`tick`), and nothing here speaks Matrix: the manager is attached to a port
//! nothing listens on, so what needs the client API fails and is recorded, not reached.
//! `crates/hs-cli/tests/bridge_offerings.rs` runs the same machine through the real binary.

use std::sync::Arc;

use hs_admin::bridge_offerings::{BridgeOfferingSource, SHARED_INSTANCE};
use hs_admin::bridge_types::BRIDGE_INSTANCE_KEY;
use hs_admin::model::BridgeOfferingRequest;
use hs_admin::sources::{AppserviceDirectory, InMemoryAppserviceDirectory, SourceError};
use hs_bridges::manager::{BridgeManager, MANAGER_ID};
use hs_kv::memory::MemoryBackend;

const SERVER: &str = "example.org";

fn manager(
    directory: InMemoryAppserviceDirectory,
    public_base_url: &str,
) -> (
    Arc<BridgeManager<MemoryBackend>>,
    Arc<InMemoryAppserviceDirectory>,
) {
    let directory = Arc::new(directory);
    let manager = BridgeManager::new(
        MemoryBackend::new(),
        directory.clone(),
        None,
        SERVER,
        public_base_url,
    )
    .unwrap();
    // A port nothing listens on: the bots' requests fail fast and the manager carries on.
    manager.attach("http://127.0.0.1:9");
    (manager, directory)
}

fn elsewhere() -> BridgeOfferingRequest {
    BridgeOfferingRequest {
        runtime: Some("elsewhere".into()),
        ..BridgeOfferingRequest::default()
    }
}

fn namespaces_of(namespaces: &serde_json::Value) -> Vec<String> {
    namespaces["users"]
        .as_array()
        .unwrap()
        .iter()
        .map(|n| n["regex"].as_str().unwrap().to_owned())
        .collect()
}

#[tokio::test]
async fn without_a_runtime_an_offering_runs_elsewhere_and_its_front_door_is_registered() {
    let (manager, directory) = manager(InMemoryAppserviceDirectory::new(), "https://example.org");

    let target = manager.target().await;
    assert!(!target.available);
    assert!(target.reason.is_some(), "{target:?}");

    let refused = manager
        .put(
            "mautrix-whatsapp",
            BridgeOfferingRequest {
                runtime: Some("cluster".into()),
                ..BridgeOfferingRequest::default()
            },
        )
        .await
        .unwrap_err();
    assert!(
        matches!(
            &refused,
            SourceError::InvalidField {
                pointer: "/runtime",
                ..
            }
        ),
        "{refused:?}"
    );
    assert!(
        manager.list().await.unwrap().is_empty(),
        "a refused put makes nothing"
    );
    assert!(
        manager
            .put("mautrix-nothing", elsewhere())
            .await
            .is_err_and(|e| matches!(e, SourceError::NotFound))
    );

    // With nothing said, a server that cannot deploy offers the type elsewhere.
    let offering = manager
        .put("mautrix-whatsapp", BridgeOfferingRequest::default())
        .await
        .unwrap();
    assert_eq!(offering.runtime, "elsewhere");
    assert_eq!(offering.mode, "per_user");
    assert_eq!(offering.image, "dock.mau.dev/mautrix/whatsapp:latest");
    assert_eq!(offering.image_tag, "latest");
    assert_eq!(
        offering.front_door.as_deref(),
        Some("@whatsappbot:example.org")
    );
    assert!(offering.enabled && offering.access.all_local_users);

    // The manager's own registration: `@bridges` and the offering's front door, exclusively,
    // its URL under this server's own route.
    let me = directory.get(MANAGER_ID).await.unwrap().unwrap();
    assert_eq!(me.sender_localpart, "bridges");
    assert_eq!(
        me.url.as_deref(),
        Some("http://127.0.0.1:9/_myelin/bridges")
    );
    assert_eq!(
        namespaces_of(&me.namespaces),
        vec!["@bridges:example\\.org", "@whatsappbot:example\\.org"]
    );

    // A second offering joins the namespace; an edit keeps what it does not say.
    manager.put("mautrix-signal", elsewhere()).await.unwrap();
    let edited = manager
        .put(
            "mautrix-whatsapp",
            BridgeOfferingRequest {
                image_tag: Some("v0.12.1".into()),
                ..BridgeOfferingRequest::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(edited.image, "dock.mau.dev/mautrix/whatsapp:v0.12.1");
    assert_eq!(edited.image_tag, "v0.12.1");
    assert_eq!(edited.runtime, "elsewhere");
    let me = directory.get(MANAGER_ID).await.unwrap().unwrap();
    assert_eq!(
        namespaces_of(&me.namespaces),
        vec![
            "@bridges:example\\.org",
            "@signalbot:example\\.org",
            "@whatsappbot:example\\.org"
        ]
    );
    assert_eq!(manager.list().await.unwrap().len(), 2);

    // Gone: the front door leaves the namespace.
    manager.delete("mautrix-signal", false).await.unwrap();
    let me = directory.get(MANAGER_ID).await.unwrap().unwrap();
    assert_eq!(
        namespaces_of(&me.namespaces),
        vec!["@bridges:example\\.org", "@whatsappbot:example\\.org"]
    );
    assert!(
        manager
            .delete("mautrix-signal", false)
            .await
            .is_err_and(|e| matches!(e, SourceError::NotFound))
    );
}

#[tokio::test]
async fn an_instance_is_registered_then_waits_for_its_bridge_then_is_ready() {
    let (manager, directory) = manager(InMemoryAppserviceDirectory::new(), "https://example.org");
    manager.put("mautrix-whatsapp", elsewhere()).await.unwrap();

    let instance = manager
        .put_instance("mautrix-whatsapp", "@alice:example.org")
        .await
        .unwrap();
    assert_eq!(instance.state, "requested");
    assert_eq!(instance.user_id.as_deref(), Some("@alice:example.org"));
    assert!(instance.appservice_id.is_none() && instance.bot.is_none());
    assert!(
        manager
            .instance_files("mautrix-whatsapp", "@alice:example.org")
            .await
            .is_err(),
        "no files before it is registered"
    );

    // One step: an appservice id, tokens, and a registration in the directory.
    manager.tick().await;
    let instance = manager
        .instance("mautrix-whatsapp", "@alice:example.org")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(instance.state, "registered", "{instance:?}");
    assert_eq!(instance.appservice_id.as_deref(), Some("whatsapp-alice"));
    assert_eq!(
        instance.bot.as_deref(),
        Some("@whatsappbot_alice:example.org")
    );
    let registration = directory.registration("whatsapp-alice").await.unwrap().json;
    assert_eq!(registration[BRIDGE_INSTANCE_KEY], "@alice:example.org");
    assert_eq!(registration["io.myelin.bridge_type"], "mautrix-whatsapp");
    assert_eq!(registration["sender_localpart"], "whatsappbot_alice");
    assert_eq!(registration["url"], "http://whatsapp-alice:29318");
    assert_eq!(
        namespaces_of(&registration["namespaces"]),
        vec![
            "@whatsapp_alice_.*:example\\.org",
            "@whatsappbot_alice:example\\.org",
            "@alice:example\\.org"
        ]
    );

    // The next: with nothing to deploy, it waits for whoever runs it, and says so.
    manager.tick().await;
    let instance = manager
        .instance("mautrix-whatsapp", "@alice:example.org")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(instance.state, "starting", "{instance:?}");
    assert!(
        instance
            .reason
            .as_deref()
            .unwrap_or_default()
            .contains("files")
    );
    let files = manager
        .instance_files("mautrix-whatsapp", "@alice:example.org")
        .await
        .unwrap();
    let config = files.config_yaml.unwrap();
    assert!(config.contains(registration["as_token"].as_str().unwrap()));
    assert!(config.contains("address: https://example.org"), "{config}");
    // The instance's own provisioning secret, in the config it runs with and the registration
    // the server keeps, so that the admin API can ask it who has signed in; and the admin API
    // asks about its owner without being told whom.
    let secret = registration["io.myelin.provisioning_secret"]
        .as_str()
        .expect("the instance's registration keeps a provisioning secret");
    assert_eq!(secret.len(), 64);
    assert!(
        config.contains(&format!("shared_secret: {secret}")),
        "{config}"
    );
    let plan = hs_admin::bridge_logins::plan("whatsapp-alice", &registration, None).unwrap();
    match plan {
        hs_admin::bridge_logins::LoginsPlan::Ask(request) => {
            assert_eq!(request.user_id, "@alice:example.org");
            assert_eq!(
                request.url,
                "http://whatsapp-alice:29318/_matrix/provision/v3/whoami"
            );
        }
        other => panic!("an instance is asked: {other:?}"),
    }
    assert!(files.registration_yaml.contains("id: whatsapp-alice"));
    assert!(
        files
            .compose_yaml
            .contains("dock.mau.dev/mautrix/whatsapp:latest")
    );
    assert!(files.manifest_yaml.contains("kind: Bridge"));
    let counts = manager
        .get("mautrix-whatsapp")
        .await
        .unwrap()
        .unwrap()
        .instances;
    assert_eq!(counts.get("starting"), Some(&1), "{counts:?}");

    // The directory's ping reaches it (the in-memory one always does): ready, with the time.
    manager.tick().await;
    let instance = manager
        .instance("mautrix-whatsapp", "@alice:example.org")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(instance.state, "ready", "{instance:?}");
    assert!(instance.ready_at.is_some());
    assert_eq!(instance.health.as_deref(), Some("healthy"));
    assert!(instance.last_ping_at.is_some());
    assert!(instance.last_error.is_none());

    // Asking again is idempotent: the same instance, not a second.
    let again = manager
        .put_instance("mautrix-whatsapp", "@alice:example.org")
        .await
        .unwrap();
    assert_eq!(again.state, "ready");
    assert_eq!(
        manager.instances("mautrix-whatsapp").await.unwrap().len(),
        1
    );

    // Removed: its registration goes with it.
    manager
        .delete_instance("mautrix-whatsapp", "@alice:example.org")
        .await
        .unwrap();
    assert!(
        manager
            .instance("mautrix-whatsapp", "@alice:example.org")
            .await
            .unwrap()
            .is_none()
    );
    assert!(directory.get("whatsapp-alice").await.unwrap().is_none());
    assert!(
        manager
            .delete_instance("mautrix-whatsapp", "@alice:example.org")
            .await
            .is_err_and(|e| matches!(e, SourceError::NotFound))
    );
}

#[tokio::test]
async fn a_bridge_that_never_answers_keeps_the_instance_starting_with_the_error() {
    let (manager, _) = manager(
        InMemoryAppserviceDirectory::new().unreachable("whatsapp-alice"),
        "https://example.org",
    );
    manager.put("mautrix-whatsapp", elsewhere()).await.unwrap();
    manager
        .put_instance("mautrix-whatsapp", "@alice:example.org")
        .await
        .unwrap();
    for _ in 0..4 {
        manager.tick().await;
    }
    let instance = manager
        .instance("mautrix-whatsapp", "@alice:example.org")
        .await
        .unwrap()
        .unwrap();
    // Run elsewhere, it may take someone days to start: not a failure, just not yet.
    assert_eq!(instance.state, "starting", "{instance:?}");
    assert_eq!(instance.health.as_deref(), Some("down"));
    assert_eq!(instance.last_error.as_deref(), Some("connection refused"));
}

#[tokio::test]
async fn instances_are_for_local_users_of_the_right_kind_of_offering() {
    let (manager, directory) = manager(InMemoryAppserviceDirectory::new(), "https://example.org");
    manager.put("mautrix-whatsapp", elsewhere()).await.unwrap();

    for (user, why) in [
        ("@alice:elsewhere.net", "not a user of this server"),
        ("alice", "not a user of this server"),
        (SHARED_INSTANCE, "per user"),
    ] {
        let refused = manager
            .put_instance("mautrix-whatsapp", user)
            .await
            .unwrap_err();
        assert!(
            matches!(&refused, SourceError::Invalid(detail) if detail.contains(why)),
            "{user}: {refused:?}"
        );
    }
    assert!(
        manager
            .put_instance("mautrix-nothing", "@alice:example.org")
            .await
            .is_err_and(|e| matches!(e, SourceError::NotFound))
    );

    // A shared type has its one instance from the moment it is offered, and it is `_`.
    let heisenbridge = manager.put("heisenbridge", elsewhere()).await.unwrap();
    assert_eq!(heisenbridge.mode, "shared");
    assert!(heisenbridge.front_door.is_none(), "{heisenbridge:?}");
    let instances = manager.instances("heisenbridge").await.unwrap();
    assert_eq!(instances.len(), 1);
    assert!(instances[0].user_id.is_none());
    assert_eq!(instances[0].state, "requested");
    let refused = manager
        .put_instance("heisenbridge", "@alice:example.org")
        .await
        .unwrap_err();
    assert!(
        matches!(&refused, SourceError::Invalid(detail) if detail.contains("shared")),
        "{refused:?}"
    );
    manager.tick().await;
    manager.tick().await;
    let shared = manager
        .instance("heisenbridge", SHARED_INSTANCE)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(shared.state, "starting", "{shared:?}");
    assert_eq!(shared.appservice_id.as_deref(), Some("heisenbridge"));
    assert_eq!(shared.bot.as_deref(), Some("@heisenbridge:example.org"));
    let files = manager
        .instance_files("heisenbridge", SHARED_INSTANCE)
        .await
        .unwrap();
    assert!(
        files.config_yaml.is_none(),
        "heisenbridge has no config file"
    );
    assert!(files.compose_yaml.contains("hif1/heisenbridge:latest"));
    assert!(
        !files.compose_yaml.contains("\"-o\""),
        "a shared bouncer names no owner: {}",
        files.compose_yaml
    );
    assert!(files.compose_yaml.contains("https://example.org"));
    // The manager's namespace does not grow a front door for a shared type: the bridge's own
    // bot is the one people talk to.
    let me = directory.get(MANAGER_ID).await.unwrap().unwrap();
    assert_eq!(
        namespaces_of(&me.namespaces),
        vec!["@bridges:example\\.org", "@whatsappbot:example\\.org"]
    );

    // Stopping an offering with instances is refused until they are removed with it.
    let refused = manager.delete("heisenbridge", false).await.unwrap_err();
    assert!(
        matches!(&refused, SourceError::Conflict(detail) if detail.contains('1')),
        "{refused:?}"
    );
    assert!(directory.get("heisenbridge").await.unwrap().is_some());
    manager.delete("heisenbridge", true).await.unwrap();
    assert!(manager.get("heisenbridge").await.unwrap().is_none());
    assert!(directory.get("heisenbridge").await.unwrap().is_none());
    assert!(
        manager
            .instances("heisenbridge")
            .await
            .is_err_and(|e| matches!(e, SourceError::NotFound))
    );
}

#[tokio::test]
async fn a_server_with_no_public_base_url_tells_a_bridge_its_bound_address() {
    let (manager, _) = manager(InMemoryAppserviceDirectory::new(), "");
    manager.put("mautrix-whatsapp", elsewhere()).await.unwrap();
    manager
        .put_instance("mautrix-whatsapp", "@alice:example.org")
        .await
        .unwrap();
    manager.tick().await;
    let files = manager
        .instance_files("mautrix-whatsapp", "@alice:example.org")
        .await
        .unwrap();
    let config = files.config_yaml.unwrap();
    assert!(
        config.contains("address: http://127.0.0.1:9\n"),
        "an empty address helps nobody: {config}"
    );
}

#[tokio::test]
async fn a_second_offering_for_the_same_user_gets_its_own_names() {
    let (manager, directory) = manager(InMemoryAppserviceDirectory::new(), "https://example.org");
    manager.put("mautrix-whatsapp", elsewhere()).await.unwrap();
    manager.put("mautrix-signal", elsewhere()).await.unwrap();
    // A localpart with an underscore is encoded so its namespace cannot contain another's.
    manager
        .put_instance("mautrix-whatsapp", "@ali_ce:example.org")
        .await
        .unwrap();
    manager
        .put_instance("mautrix-signal", "@ali_ce:example.org")
        .await
        .unwrap();
    manager.tick().await;
    let whatsapp = directory
        .registration("whatsapp-ali=5fce")
        .await
        .unwrap()
        .json;
    let signal = directory
        .registration("signal-ali=5fce")
        .await
        .unwrap()
        .json;
    assert_eq!(whatsapp["sender_localpart"], "whatsappbot_ali=5fce");
    assert_eq!(signal["sender_localpart"], "signalbot_ali=5fce");
    assert_eq!(
        directory.list().await.unwrap().len(),
        3,
        "the manager and two instances"
    );
    let all: Vec<String> = manager
        .list()
        .await
        .unwrap()
        .into_iter()
        .map(|o| format!("{}={:?}", o.bridge_type, o.instances))
        .collect();
    assert_eq!(
        all,
        vec![
            "mautrix-signal={\"registered\": 1}",
            "mautrix-whatsapp={\"registered\": 1}"
        ]
    );
}
