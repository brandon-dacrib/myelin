//! The manager (RFC 0017 section 4.1): offerings, instances, and the state machine that takes
//! an instance from `requested` to `ready` -- registered, deployed, answering this server, and
//! its owner invited to it.
//!
//! # What an instance's Kubernetes objects are called
//!
//! An instance's `Bridge`, Deployment, Service and pods are named after what they are, so that
//! `kubectl get pods` says whose bridge each one is: [`deploy_name`] turns the appservice id into
//! `bridge-<short type>-<owner localpart>` (`bridge-whatsapp-brandon`, pods
//! `bridge-whatsapp-brandon-<replicaset hash>-<pod hash>`), or `bridge-<short type>` for a shared
//! instance (`bridge-heisenbridge`). The name is made a DNS-1123 label: lowercase, `[a-z0-9-]`,
//! every other character mapped to `-`, runs collapsed, no `-` at either end, and at most
//! [`DEPLOY_NAME_MAX`] characters so that even a pod's name stays within 63. When mapping or
//! shortening changed the id (so two ids could have come out the same), `-<6 hex of
//! sha256(appservice id)>` is appended; an id that needed no change gets no suffix. The Secret
//! and the claim hang off the name as `<name>-files` and `<name>-data` (the operator's rule).
//!
//! The name is decided once, when the instance is first named, and stored on its row
//! ([`InstanceRow::deploy_name`]); nothing recomputes it afterwards, so a running instance keeps
//! its name until it is removed and asked for again. Instances deployed before 2026-10-02 run
//! under [`legacy_deploy_name`] (`bridge-<8 hex of sha256(appservice id)>`): their rows carry
//! that name and they keep it. A row with an appservice id but no stored name (there should be
//! none; the field has been written since the manager's first version) is named on its next
//! step: if the runtime has an object under the hashed name, that name is adopted so nothing
//! already running is orphaned or deployed twice; otherwise it gets the readable name.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use async_trait::async_trait;
use hs_admin::bridge_offerings::{BridgeOfferingSource, SHARED_INSTANCE};
use hs_admin::bridge_types::{self, BRIDGE_INSTANCE_KEY, InstanceRender, InstanceSpec};
use hs_admin::model::{
    AdminAppserviceCreate, AdminOfferingOverlap, BridgeDeploymentTarget, BridgeInstance,
    BridgeInstanceFiles, BridgeOffering, BridgeOfferingRequest,
};
use hs_admin::sources::{AppserviceDirectory, SourceError};
use hs_kv::KvBackend;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::cross_signing::{BotIdentity, Seeds};
use crate::matrix::MatrixClient;
use crate::overlap::{self, Overlap};
use crate::runtime::{DeploySpec, Runtime, manifest_yaml};
use crate::store::{
    BridgeStore, CHAT_BY_BOT, CHAT_BY_OWNER, InstanceRow, InstanceState, ManagerRow, OfferingRow,
    StoreError,
};

/// The manager's own appservice id.
pub const MANAGER_ID: &str = "myelin-bridges";
/// The manager bot's localpart.
pub const MANAGER_BOT: &str = "bridges";
/// Where on this server's client listener the manager's appservice API is served.
pub const ROUTE_PREFIX: &str = "/_myelin/bridges";

/// How long a deployment may take to become ready before the instance is `failed`.
const DEPLOY_TIMEOUT_MS: u64 = 15 * 60 * 1000;
/// How long a ready deployment may take to answer this server's ping.
const START_TIMEOUT_MS: u64 = 10 * 60 * 1000;
/// How often the state machine runs when nothing wakes it.
const TICK: Duration = Duration::from_secs(3);
/// How often a ready instance whose bot device is signed has its bot's keys looked at again,
/// for a device the bridge made since (a reset database): one `/keys/query` over loopback.
const IDENTITY_RECHECK_MS: u64 = 60_000;
/// How long a bot's device must have gone unseen, while a newer device of the same bot with
/// keys is in use, before the manager removes it ([`bot_devices_plan`]). A day: a bridge that
/// is merely quiet keeps its device (the device in use is never removed whatever its age), and
/// one left behind by a reset stops drawing clients' room keys the next day.
pub const STALE_BOT_DEVICE_MS: u64 = 24 * 60 * 60 * 1000;
/// The start of the reason [`BridgeManager::settle_bot_identity`] leaves on a row when it
/// cannot, so that its next success clears only its own.
const IDENTITY_REASON: &str = "the bot's cross-signing";
/// How often the manager looks for bridges registered by hand that overlap an offering
/// ([`crate::overlap`]), to log the ones that appeared or went.
const OVERLAP_LOG_MS: u64 = 60_000;
/// The longest a failing instance waits before its step is tried again. Each failure in a row
/// doubles the wait from [`TICK`] up to this ([`BridgeManager::tick`]), so a step that cannot
/// succeed (a cluster refusing the `Bridge`) is tried a few times a minute at first and then
/// every five minutes, not every three seconds.
pub const STEP_BACKOFF_MAX: Duration = Duration::from_secs(5 * 60);

/// How long an instance whose step has failed `failures` times in a row waits before the next
/// try: [`TICK`] doubled for each failure after the first, at most [`STEP_BACKOFF_MAX`].
#[must_use]
pub fn step_backoff(failures: u32) -> Duration {
    let doublings = failures.saturating_sub(1).min(16);
    TICK.saturating_mul(1 << doublings).min(STEP_BACKOFF_MAX)
}

/// One instance's failing streak: how many steps in a row failed and when to try again.
#[derive(Debug, Clone, Copy)]
struct Backoff {
    failures: u32,
    retry_at_ms: u64,
}

/// The reason an instance run elsewhere carries once its registration changed under it.
const REREGISTERED_REASON: &str =
    "its registration changed: download its files again and restart it with them";

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

fn rfc3339(ms: u64) -> String {
    let nanos = i128::from(ms) * 1_000_000;
    time::OffsetDateTime::from_unix_timestamp_nanos(nanos)
        .ok()
        .and_then(|t| {
            t.format(time::macros::format_description!(
                "[year]-[month]-[day]T[hour]:[minute]:[second].[subsecond digits:3]Z"
            ))
            .ok()
        })
        .unwrap_or_default()
}

fn store_err(e: StoreError) -> SourceError {
    SourceError::Unavailable(e.to_string())
}

/// `whatsapp` for `mautrix-whatsapp`: what people call a type in a command, and the stem of an
/// instance's appservice id.
#[must_use]
pub fn short_name(bridge_type: &str) -> &str {
    bridge_type
        .strip_prefix("mautrix-")
        .or_else(|| bridge_type.strip_prefix("matrix-"))
        .unwrap_or(bridge_type)
}

/// The longest name an instance's objects get. A Deployment's pods are called
/// `<name>-<10 hex pod-template hash>-<5 characters>`, so a name of at most 46 characters keeps
/// even a pod's name within the 63 characters of a DNS-1123 label and nothing is cut short on
/// the way down.
pub const DEPLOY_NAME_MAX: usize = 46;
/// Every instance's objects start with this.
const DEPLOY_NAME_PREFIX: &str = "bridge-";
/// How much of the appservice id's hash disambiguates a mapped or shortened name.
const DEPLOY_NAME_HASH_LEN: usize = 6;

/// The Kubernetes name for an instance's objects, from its appservice id: `bridge-` and the id
/// made a DNS-1123 label (`whatsapp-brandon` gives `bridge-whatsapp-brandon`; a shared
/// `heisenbridge` gives `bridge-heisenbridge`). Lowercased, every character outside `[a-z0-9]`
/// mapped to `-`, runs of `-` collapsed, none at either end, and cut to fit [`DEPLOY_NAME_MAX`];
/// if any of that changed the id, `-<6 hex of sha256(id)>` is appended so that two ids that map
/// to the same text (`a.b` and `a-b`, `Alice` and `alice`, two long ids with one prefix) still
/// get different names. The same every time for the same id. The module doc has the rule in
/// full and says how an instance named before this rule keeps its old name.
#[must_use]
pub fn deploy_name(appservice_id: &str) -> String {
    let mut stem = String::with_capacity(appservice_id.len());
    for c in appservice_id.chars() {
        let c = c.to_ascii_lowercase();
        if c.is_ascii_lowercase() || c.is_ascii_digit() {
            stem.push(c);
        } else if !stem.is_empty() && !stem.ends_with('-') {
            stem.push('-');
        }
    }
    let stem = stem.trim_end_matches('-');
    let room = DEPLOY_NAME_MAX - DEPLOY_NAME_PREFIX.len();
    if stem == appservice_id && stem.len() <= room {
        return format!("{DEPLOY_NAME_PREFIX}{stem}");
    }
    let digest = hex::encode(Sha256::digest(appservice_id.as_bytes()));
    let hash = &digest[..DEPLOY_NAME_HASH_LEN];
    let cut = stem
        .get(..room - 1 - DEPLOY_NAME_HASH_LEN)
        .unwrap_or(stem)
        .trim_end_matches('-');
    if cut.is_empty() {
        format!("{DEPLOY_NAME_PREFIX}{hash}")
    } else {
        format!("{DEPLOY_NAME_PREFIX}{cut}-{hash}")
    }
}

/// The name an instance's objects got before 2026-10-02: `bridge-<8 hex of sha256(appservice
/// id)>`. Rows from then carry it; a row with no stored name is checked against the runtime
/// under this name before it is given a readable one, so a running instance is adopted rather
/// than deployed a second time.
#[must_use]
pub fn legacy_deploy_name(appservice_id: &str) -> String {
    let digest = Sha256::digest(appservice_id.as_bytes());
    format!("{DEPLOY_NAME_PREFIX}{}", &hex::encode(digest)[..8])
}

/// The manager.
pub struct BridgeManager<B: KvBackend> {
    pub(crate) store: BridgeStore<B>,
    directory: Arc<dyn AppserviceDirectory>,
    runtime: Option<Arc<dyn Runtime>>,
    pub(crate) server_name: String,
    /// How a bridge that runs elsewhere reaches this server: its public base URL. Replaced when
    /// `server.public_baseurl` changes ([`BridgeManager::set_public_base_url`]); files rendered
    /// from then on carry the new one.
    public_base_url: std::sync::RwLock<String>,
    loopback: OnceLock<String>,
    pub(crate) client: OnceLock<MatrixClient>,
    wake: tokio::sync::Notify,
    /// When each instance's bot identity was last looked at ([`Self::settle_bot_identity`]):
    /// a settled one is looked at again every [`IDENTITY_RECHECK_MS`], for a device the bridge
    /// made since. In memory: a restart looks once more, which is cheap.
    identity_checked_ms: Mutex<HashMap<(String, String), u64>>,
    /// The offerings the deployment declares ([`Self::set_declared`]), applied once per
    /// ownership of the global shard by [`Self::apply_declared`].
    declared: Mutex<Vec<(String, BridgeOfferingRequest)>>,
    /// When the overlaps were last looked for, and the `(offering, appservice)` pairs found
    /// then, so that each one is logged when it appears and when it goes. In memory: a restart
    /// logs the current ones once more.
    overlaps_logged: Mutex<(u64, BTreeSet<(String, String)>)>,
    /// Instances whose last step failed, by `(bridge type, owner)`, and when each is tried
    /// again ([`step_backoff`]). In memory: a restart tries each once more straight away.
    /// Cleared for an instance when someone asks for it again or its offering changes.
    backoff: Mutex<HashMap<(String, String), Backoff>>,
}

impl<B: KvBackend> std::fmt::Debug for BridgeManager<B> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BridgeManager")
            .field("server_name", &self.server_name)
            .field("runtime", &self.runtime.is_some())
            .finish_non_exhaustive()
    }
}

impl<B: KvBackend + 'static> BridgeManager<B> {
    /// Replaces the public base URL a bridge that runs elsewhere is given (`""`: none), for every
    /// file rendered from now on.
    pub fn set_public_base_url(&self, public_base_url: &str) {
        *self
            .public_base_url
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) =
            public_base_url.trim_end_matches('/').to_owned();
    }

    /// A manager over `backend`'s tables, registering through `directory`, running instances on
    /// `runtime` (none: they all run elsewhere).
    ///
    /// # Errors
    /// If the tables cannot be opened.
    pub fn new(
        backend: B,
        directory: Arc<dyn AppserviceDirectory>,
        runtime: Option<Arc<dyn Runtime>>,
        server_name: &str,
        public_base_url: &str,
    ) -> Result<Arc<Self>, StoreError> {
        Ok(Arc::new(Self {
            store: BridgeStore::open(backend)?,
            directory,
            runtime,
            server_name: server_name.to_owned(),
            public_base_url: std::sync::RwLock::new(
                public_base_url.trim_end_matches('/').to_owned(),
            ),
            loopback: OnceLock::new(),
            client: OnceLock::new(),
            wake: tokio::sync::Notify::new(),
            identity_checked_ms: Mutex::new(HashMap::new()),
            declared: Mutex::new(Vec::new()),
            overlaps_logged: Mutex::new((0, BTreeSet::new())),
            backoff: Mutex::new(HashMap::new()),
        }))
    }

    /// The offerings the deployment declares (`MYELIN_BRIDGES_OFFERINGS`, the chart's
    /// `bridges.offerings`): each is created as `request` says the first time the manager runs
    /// with it declared and no offering of that type exists, then left to the admin API
    /// (`ManagerRow::declared`). Set before [`Self::start`].
    pub fn set_declared(&self, declared: Vec<(String, BridgeOfferingRequest)>) {
        *self
            .declared
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = declared;
    }

    /// Creates the declared offerings not created before ([`Self::set_declared`]). One that
    /// already exists (made through the admin API first, as the demo's was) is adopted as it
    /// is; one that cannot be created now (the type needs a cluster this server has not got)
    /// is logged and tried again on the next start.
    ///
    /// # Errors
    /// On a store failure.
    pub async fn apply_declared(&self) -> Result<(), SourceError> {
        let declared = self
            .declared
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        if declared.is_empty() {
            return Ok(());
        }
        let mut row = self.tokens().map_err(store_err)?;
        for (bridge_type, request) in declared {
            if row.declared.contains(&bridge_type) {
                continue;
            }
            if self
                .store
                .offering(&bridge_type)
                .map_err(store_err)?
                .is_some()
            {
                tracing::info!(bridge_type = %bridge_type, "the deployment declares a bridge offering that already exists: keeping it as the admin API has it");
            } else {
                match self.put(&bridge_type, request).await {
                    Ok(offering) => {
                        tracing::info!(bridge_type = %bridge_type, runtime = %offering.runtime, "created the bridge offering the deployment declares");
                    }
                    Err(e) => {
                        tracing::warn!(bridge_type = %bridge_type, error = %e, "the deployment declares a bridge offering this server could not create; it will be tried again at the next start");
                        continue;
                    }
                }
            }
            row.declared.push(bridge_type);
            self.store.put_manager(&row).map_err(store_err)?;
        }
        Ok(())
    }

    /// Tells the manager where this server's own client API is (`http://127.0.0.1:8008`): what
    /// its bots speak to, what its registration's `url` is rooted at, and what a bridge run
    /// elsewhere is told to reach when the server has no public base URL. [`Self::start`] does
    /// this; it is separate so that a test can drive [`Self::tick`] by hand.
    pub fn attach(&self, loopback: &str) {
        let _ = self.loopback.set(loopback.trim_end_matches('/').to_owned());
        let _ = self.client.set(MatrixClient::new(loopback));
    }

    /// Starts the manager against the bound client listener. Reconciliation runs only while
    /// `is_owner` says this replica owns the global shard. Abort the returned task on shutdown.
    pub fn start(
        self: &Arc<Self>,
        loopback: &str,
        is_owner: impl Fn() -> bool + Send + 'static,
    ) -> tokio::task::JoinHandle<()> {
        self.attach(loopback);
        let manager = self.clone();
        tokio::spawn(async move {
            let mut registered = false;
            let mut declared = false;
            loop {
                if is_owner() {
                    if !registered {
                        match manager.sync_registration().await {
                            Ok(()) => registered = true,
                            Err(e) => {
                                tracing::warn!(error = %e, "the bridge manager could not register its front doors")
                            }
                        }
                    }
                    if registered && !declared {
                        match manager.apply_declared().await {
                            Ok(()) => declared = true,
                            Err(e) => {
                                tracing::warn!(error = %e, "the bridge manager could not apply the offerings the deployment declares")
                            }
                        }
                    }
                    if registered {
                        manager.tick().await;
                    }
                } else {
                    registered = false;
                    declared = false;
                }
                let _ = tokio::time::timeout(TICK, manager.wake.notified()).await;
            }
        })
    }

    /// The manager's own tokens.
    ///
    /// # Errors
    /// On a store failure.
    pub fn tokens(&self) -> Result<ManagerRow, StoreError> {
        self.store.manager(|| ManagerRow {
            as_token: crate::random_hex(32),
            hs_token: crate::random_hex(32),
            declared: Vec::new(),
        })
    }

    pub(crate) fn mxid(&self, localpart: &str) -> String {
        format!("@{localpart}:{}", self.server_name)
    }

    /// The type whose front door is `localpart`, among the offerings.
    pub(crate) fn type_for_front_door(&self, localpart: &str) -> Option<String> {
        self.store.offerings().ok()?.into_iter().find_map(|o| {
            (bridge_types::front_door_localpart(&o.bridge_type) == Some(localpart))
                .then_some(o.bridge_type)
        })
    }

    /// Whether `localpart` is one of the manager's bots.
    pub(crate) fn is_manager_bot(&self, localpart: &str) -> bool {
        localpart == MANAGER_BOT || self.type_for_front_door(localpart).is_some()
    }

    fn front_doors(&self) -> Result<Vec<(String, &'static str)>, StoreError> {
        Ok(self
            .store
            .offerings()?
            .into_iter()
            .filter(|o| mode(&o.bridge_type) == "per_user")
            .filter_map(|o| {
                bridge_types::front_door_localpart(&o.bridge_type).map(|l| (o.bridge_type, l))
            })
            .collect())
    }

    /// Registers the manager's appservice, or brings it up to date: its URL on this server, and
    /// a namespace with `@bridges` and every per-user offering's front door.
    ///
    /// # Errors
    /// [`SourceError::Conflict`] when a front door is taken, say by a shared bridge registered by
    /// hand under the same bot name; or a store or registry failure.
    pub async fn sync_registration(&self) -> Result<(), SourceError> {
        let Some(loopback) = self.loopback.get() else {
            return Ok(()); // not started yet; start() will call again
        };
        let tokens = self.tokens().map_err(store_err)?;
        let server = self.server_name.replace('.', "\\.");
        let mut users =
            vec![json!({"regex": format!("@{MANAGER_BOT}:{server}"), "exclusive": true})];
        let doors = self.front_doors().map_err(store_err)?;
        for (_, localpart) in &doors {
            users.push(json!({"regex": format!("@{localpart}:{server}"), "exclusive": true}));
        }
        let url = format!("{loopback}{ROUTE_PREFIX}");
        let namespaces = json!({"users": users, "aliases": [], "rooms": []});
        if self.directory.get(MANAGER_ID).await?.is_some() {
            self.directory
                .update(MANAGER_ID, json!({"url": url, "namespaces": namespaces}))
                .await?;
        } else {
            self.directory
                .create(AdminAppserviceCreate {
                    registration: Some(json!({
                        "id": MANAGER_ID,
                        "url": url,
                        "as_token": tokens.as_token,
                        "hs_token": tokens.hs_token,
                        "sender_localpart": MANAGER_BOT,
                        "rate_limited": false,
                        "namespaces": namespaces,
                    })),
                    registration_yaml: None,
                })
                .await?;
        }
        // The bots are accounts, and an account an operator did not make has no business on a
        // server that offers nothing: the Users page, and the overview's count, would show a
        // `bridges` account from the first boot. They are created with the first offering
        // (`put` re-syncs), and the namespace above is reserved from the start regardless.
        let offered = !self.store.offerings().map_err(store_err)?.is_empty();
        if let Some(client) = self.client.get()
            && offered
        {
            let names = std::iter::once((MANAGER_BOT, "Bridges".to_owned())).chain(
                doors.iter().map(|(t, l)| {
                    (
                        *l,
                        format!("{} bridge", bridge_types::display_name(t).unwrap_or(t)),
                    )
                }),
            );
            for (localpart, name) in names {
                if client
                    .ensure_user(&tokens.as_token, localpart)
                    .await
                    .is_ok()
                {
                    let _ = client
                        .set_display_name(&tokens.as_token, &self.mxid(localpart), &name)
                        .await;
                }
            }
        }
        Ok(())
    }

    /// Nudges the state machine to run now.
    pub fn wake(&self) {
        self.wake.notify_one();
    }

    // ---- views ------------------------------------------------------------------------------

    async fn offering_view(&self, row: &OfferingRow) -> Result<BridgeOffering, SourceError> {
        let kind =
            bridge_types::get(&row.bridge_type, &self.server_name).ok_or(SourceError::NotFound)?;
        let overlapping_appservices = self
            .overlaps_of(row)
            .await?
            .iter()
            .map(|o| overlap::for_offering(o, &kind.name, &self.server_name))
            .collect();
        let mut counts = BTreeMap::new();
        for i in self
            .store
            .instances(Some(&row.bridge_type))
            .map_err(store_err)?
        {
            *counts.entry(i.state.as_str().to_owned()).or_insert(0) += 1;
        }
        let repository = kind
            .image
            .rsplit_once(':')
            .map_or(kind.image.as_str(), |(r, _)| r);
        // A row from before the catalogue pinned its images says `latest`; it runs the pin.
        let image_tag = bridge_types::image_tag(&row.bridge_type, Some(&row.image_tag))
            .unwrap_or_else(|| row.image_tag.clone());
        Ok(BridgeOffering {
            bridge_type: row.bridge_type.clone(),
            name: kind.name.clone(),
            mode: kind.mode.clone(),
            enabled: row.enabled,
            runtime: row.runtime.clone(),
            image: format!("{repository}:{image_tag}"),
            image_tag,
            front_door: (kind.mode == "per_user")
                .then(|| bridge_types::front_door_localpart(&row.bridge_type).map(|l| self.mxid(l)))
                .flatten(),
            access: row.access.clone(),
            options: row.options.clone(),
            instances: counts,
            created_at: rfc3339(row.created_at_ms),
            overlapping_appservices,
        })
    }

    /// The bridges registered by hand that overlap `offering`'s instances
    /// ([`crate::overlap::overlaps`]): every registration but the manager's own and the
    /// instances'.
    ///
    /// # Errors
    /// On a store or directory failure.
    pub async fn overlaps_of(&self, offering: &OfferingRow) -> Result<Vec<Overlap>, SourceError> {
        let instance_ids: HashSet<String> = self
            .store
            .instances(Some(&offering.bridge_type))
            .map_err(store_err)?
            .into_iter()
            .filter_map(|i| i.appservice_id)
            .collect();
        let appservices = self.directory.list().await?;
        Ok(overlap::overlaps(
            &offering.bridge_type,
            &self.server_name,
            &appservices,
            &instance_ids,
            MANAGER_ID,
        )
        .unwrap_or_default())
    }

    /// The health line for appservice `id`, when it is a bridge registered by hand that an
    /// offering's instances overlap; `None` for any other appservice.
    ///
    /// # Errors
    /// On a store or directory failure.
    pub async fn overlap_of_appservice(
        &self,
        id: &str,
    ) -> Result<Option<AdminOfferingOverlap>, SourceError> {
        for offering in self.store.offerings().map_err(store_err)? {
            let Some(found) = self
                .overlaps_of(&offering)
                .await?
                .into_iter()
                .find(|o| o.appservice_id == id)
            else {
                continue;
            };
            let Some(kind) = bridge_types::get(&offering.bridge_type, &self.server_name) else {
                continue;
            };
            let front_door = (kind.mode == "per_user")
                .then(|| {
                    bridge_types::front_door_localpart(&offering.bridge_type).map(|l| self.mxid(l))
                })
                .flatten();
            return Ok(Some(overlap::for_appservice(
                &found,
                &offering.bridge_type,
                &kind.name,
                front_door.as_deref(),
            )));
        }
        Ok(None)
    }

    /// Logs the overlaps that appeared or went since the last look, at most every
    /// [`OVERLAP_LOG_MS`]: a `WARN` for a hand-registered bridge an offering's instances
    /// overlap, an `INFO` once it is gone.
    async fn log_overlaps(&self) {
        let now = now_ms();
        let due = {
            let logged = self
                .overlaps_logged
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            logged.0 == 0 || now.saturating_sub(logged.0) >= OVERLAP_LOG_MS
        };
        if !due {
            return;
        }
        let Ok(offerings) = self.store.offerings() else {
            return;
        };
        let mut current = BTreeSet::new();
        for offering in &offerings {
            match self.overlaps_of(offering).await {
                Ok(found) => {
                    for o in found {
                        current.insert((offering.bridge_type.clone(), o.appservice_id));
                    }
                }
                Err(e) => {
                    tracing::warn!(bridge_type = %offering.bridge_type, error = %e, "could not look for bridges registered by hand that overlap the offering");
                    return;
                }
            }
        }
        let mut logged = self
            .overlaps_logged
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for (bridge_type, id) in current.difference(&logged.1) {
            tracing::warn!(bridge_type = %bridge_type, appservice = %id, "a bridge registered by hand overlaps the offering's instances: its page says what to do");
        }
        for (bridge_type, id) in logged.1.difference(&current) {
            tracing::info!(bridge_type = %bridge_type, appservice = %id, "the bridge registered by hand that overlapped the offering's instances is gone");
        }
        *logged = (now, current);
    }

    async fn instance_view(&self, row: &InstanceRow) -> BridgeInstance {
        let deployment = match (&self.runtime, &row.deploy_name) {
            (Some(runtime), Some(name)) if row.state != InstanceState::Requested => {
                runtime.status(name).await.ok().flatten()
            }
            _ => None,
        };
        let health = match &row.appservice_id {
            Some(id) => self.directory.health(id).await.ok(),
            None => None,
        };
        let bot = row.appservice_id.as_ref().and_then(|_| {
            bridge_types::instance_names(&row.bridge_type, owner_of(row))
                .map(|(bot, _)| self.mxid(&bot))
        });
        // What the runtime says needs fixing, on every instance it runs (the step that
        // failed because of it has usually said so already, in the same words).
        let warning = match (&self.runtime, &deployment) {
            (Some(runtime), Some(d)) => {
                let notes: Vec<String> = d
                    .message
                    .as_deref()
                    .and_then(explain_deployment)
                    .into_iter()
                    .chain(runtime.warning())
                    .collect();
                (!notes.is_empty()).then(|| notes.join(". "))
            }
            _ => None,
        };
        let reason = match (row.reason.clone(), warning) {
            (Some(reason), Some(warning)) if !reason.contains(&warning) => {
                Some(format!("{reason}. {warning}"))
            }
            (None, Some(warning)) => Some(warning),
            (reason, _) => reason,
        };
        BridgeInstance {
            bridge_type: row.bridge_type.clone(),
            user_id: owner_of(row).map(str::to_owned),
            state: row.state.as_str().to_owned(),
            reason,
            appservice_id: row.appservice_id.clone(),
            bot,
            deployment,
            health: health.as_ref().map(|h| h.status.clone()),
            last_ping_at: health.as_ref().and_then(|h| h.last_ping_at.clone()),
            last_error: health.as_ref().and_then(|h| h.last_error.clone()),
            created_at: rfc3339(row.created_at_ms),
            ready_at: row.ready_at_ms.map(rfc3339),
            chat_room: row.dm_room.clone(),
            chat_started_by: row.dm_started_by.clone(),
            device_name: bridge_types::device_name(
                &row.bridge_type,
                &self.server_name,
                owner_of(row),
            ),
            signed_bot_device: row.signed_bot_device.clone(),
            last_key_withheld: health.and_then(|h| h.last_key_withheld),
            removed_bot_devices: row
                .removed_bot_devices
                .iter()
                .map(|d| hs_admin::model::AdminRemovedBotDevice {
                    device_id: d.device_id.clone(),
                    removed_at: rfc3339(d.removed_at_ms),
                    last_seen_at: d.last_seen_ms.map(rfc3339),
                    kept_device: d.kept_device.clone(),
                })
                .collect(),
        }
    }

    // ---- rendering --------------------------------------------------------------------------

    /// How a bridge of `offering` reaches this server: the cluster's internal address for an
    /// instance it deploys, otherwise the public base URL, or, on a server that has none, the
    /// address this server bound (a bridge on the same machine reaches that; one anywhere else
    /// needs `server.public_baseurl` set, and the address in its files says so plainly rather
    /// than being empty).
    fn homeserver_address(&self, offering: &OfferingRow) -> String {
        let outside = || {
            let public = self
                .public_base_url
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone();
            if public.is_empty() {
                self.loopback.get().cloned().unwrap_or_default()
            } else {
                public
            }
        };
        match (&self.runtime, offering.runtime.as_str()) {
            (Some(runtime), "cluster") => runtime.target().homeserver_url.unwrap_or_else(outside),
            _ => outside(),
        }
    }

    fn render(&self, row: &InstanceRow, offering: &OfferingRow) -> Option<InstanceRender> {
        bridge_types::render_instance(&InstanceSpec {
            type_id: &row.bridge_type,
            server_name: &self.server_name,
            appservice_id: row.appservice_id.as_deref()?,
            owner: owner_of(row),
            as_token: row.as_token.as_deref()?,
            hs_token: row.hs_token.as_deref()?,
            url: row.url.as_deref()?,
            homeserver_address: &self.homeserver_address(offering),
            image_tag: &offering.image_tag,
            encryption: offering.options.encryption,
            double_puppeting: offering.options.double_puppeting,
            backfill: offering.options.backfill,
            provisioning_secret: row.provisioning_secret.as_deref(),
            pickle_key: row.pickle_key.as_deref(),
        })
    }

    /// Renders `row` and applies its deployment to the runtime, recording what was applied
    /// ([`deploy_fingerprint`]) so that the next step can tell whether anything changed.
    async fn apply_deployment(
        &self,
        row: &InstanceRow,
        offering: &OfferingRow,
        runtime: &Arc<dyn Runtime>,
    ) -> Result<(), String> {
        let render = self
            .render(row, offering)
            .ok_or("the instance has no registration")?;
        let spec = Self::deploy_spec(row, &render).ok_or("the instance has no name")?;
        let fingerprint = deploy_fingerprint(&spec);
        runtime.apply(&spec).await?;
        self.store
            .update_instance(&row.bridge_type, &row.owner, |r| {
                if r.applied_fingerprint.as_deref() == Some(fingerprint.as_str()) {
                    return false;
                }
                r.applied_fingerprint = Some(fingerprint.clone());
                true
            })
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    /// Whether what the runtime would be asked for now differs from what it was last asked
    /// for: the offering's image tag or options changed, this server's address changed, or
    /// the files are rendered differently (a bridge's device name, say). `None` where the
    /// instance cannot be rendered yet.
    fn deployment_changed(&self, row: &InstanceRow, offering: &OfferingRow) -> Option<bool> {
        let render = self.render(row, offering)?;
        let spec = Self::deploy_spec(row, &render)?;
        Some(row.applied_fingerprint.as_deref() != Some(deploy_fingerprint(&spec).as_str()))
    }

    fn deploy_spec(row: &InstanceRow, render: &InstanceRender) -> Option<DeploySpec> {
        let appservice_id = row.appservice_id.clone()?;
        let owner = owner_of(row).map(str::to_owned);
        let mut labels = BTreeMap::from([
            ("myelin.dev/bridge-type".to_owned(), row.bridge_type.clone()),
            (
                "myelin.dev/appservice-id".to_owned(),
                label_safe(&appservice_id),
            ),
        ]);
        if let Some(owner) = &owner {
            labels.insert("myelin.dev/owner".to_owned(), label_safe(owner));
        }
        Some(DeploySpec {
            name: row.deploy_name.clone()?,
            labels,
            bridge_type: row.bridge_type.clone(),
            appservice_id,
            owner,
            image_repository: render.image_repository.clone(),
            image_tag: render.image_tag.clone(),
            port: render.port,
            args: render.args.clone(),
            files: render.files.clone(),
            storage_size: Some("1Gi".to_owned()),
        })
    }

    // ---- the state machine ------------------------------------------------------------------

    /// Advances every instance one step.
    pub async fn tick(&self) {
        self.log_overlaps().await;
        let rows = match self.store.instances(None) {
            Ok(rows) => rows,
            Err(e) => {
                tracing::warn!(error = %e, "the bridge manager could not read its instances");
                return;
            }
        };
        for row in rows {
            let key = (row.bridge_type.clone(), row.owner.clone());
            let waiting = self
                .backoff
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .get(&key)
                .copied();
            if waiting.is_some_and(|b| now_ms() < b.retry_at_ms) {
                continue;
            }
            let result = self.step(&row).await;
            let mut backoff = self
                .backoff
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Err(e) = result {
                let failures = waiting.map_or(1, |b| b.failures.saturating_add(1));
                let wait = step_backoff(failures);
                backoff.insert(
                    key,
                    Backoff {
                        failures,
                        retry_at_ms: now_ms()
                            .saturating_add(u64::try_from(wait.as_millis()).unwrap_or(u64::MAX)),
                    },
                );
                drop(backoff);
                tracing::warn!(
                    bridge_type = %row.bridge_type,
                    owner = %row.owner,
                    error = %e,
                    failures,
                    retry_in_secs = wait.as_secs(),
                    "bridge instance step failed; trying it again after the wait"
                );
                let message = e.to_string();
                let _ = self
                    .store
                    .update_instance(&row.bridge_type, &row.owner, |r| {
                        if r.reason.as_deref() == Some(message.as_str()) {
                            return false;
                        }
                        r.reason = Some(message.clone());
                        true
                    });
            } else if let Some(streak) = backoff.remove(&key) {
                drop(backoff);
                tracing::info!(
                    bridge_type = %row.bridge_type,
                    owner = %row.owner,
                    failures = streak.failures,
                    "bridge instance step went through again"
                );
            }
        }
    }

    /// Forgets the failing streak of `owner`'s instance of `bridge_type` (all of the type's,
    /// with `owner` `None`), so that its next step runs on the next tick: someone asked for it
    /// again, or its offering changed.
    fn clear_backoff(&self, bridge_type: &str, owner: Option<&str>) {
        self.backoff
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .retain(|(t, o), _| t != bridge_type || owner.is_some_and(|owner| o != owner));
    }

    async fn step(&self, row: &InstanceRow) -> Result<(), String> {
        let row = &self.settle_deploy_name(row).await?;
        // No pickle key is minted for an instance registered before the manager kept one
        // (2026-10-02): its bridge made its own on its first start and its crypto store is
        // pickled with that, so its config is rendered without one and the operator's init
        // container carries the bridge's own into every new copy. Minting one here (as the
        // manager did until 2026-10-09) put a second key in front of a bridge that had lost its
        // own, which made its store unreadable.
        let Some(offering) = self
            .store
            .offering(&row.bridge_type)
            .map_err(|e| e.to_string())?
        else {
            // Its offering is gone: it goes too.
            tracing::info!(bridge_type = %row.bridge_type, owner = %row.owner, "its offering is gone: removing the instance");
            if row.state != InstanceState::Removing {
                self.set_state(row, InstanceState::Removing, None);
            }
            return self.remove(row).await;
        };
        let row = &self.settle_registration(row, &offering).await?;
        let now = now_ms();
        match row.state {
            InstanceState::Requested => self.allocate_and_register(row, &offering).await,
            InstanceState::Registered => {
                if offering.runtime == "cluster" {
                    let runtime = self
                        .runtime
                        .clone()
                        .ok_or("this server cannot deploy bridges any more")?;
                    self.apply_deployment(row, &offering, &runtime).await?;
                    self.set_state(
                        row,
                        InstanceState::Deploying,
                        Some("waiting for the pod".into()),
                    );
                } else {
                    self.set_state(
                        row,
                        InstanceState::Starting,
                        Some(
                            "waiting for someone to run it: its files are in the admin interface"
                                .into(),
                        ),
                    );
                }
                self.wake();
                Ok(())
            }
            InstanceState::Deploying | InstanceState::Starting | InstanceState::Ready
                if offering.runtime == "cluster"
                    && self.deployment_changed(row, &offering) == Some(true) =>
            {
                // The offering or this server changed under a deployed instance (an image tag,
                // an option, the bridge's device name): ask the runtime for the new deployment,
                // which updates its files Secret and rolls its pod, and watch it come back.
                let runtime = self
                    .runtime
                    .clone()
                    .ok_or("this server cannot deploy bridges any more")?;
                self.apply_deployment(row, &offering, &runtime).await?;
                tracing::info!(
                    bridge_type = %row.bridge_type,
                    owner = %row.owner,
                    deploy_name = row.deploy_name.as_deref().unwrap_or_default(),
                    from = row.state.as_str(),
                    "the bridge instance's deployment changed: applied it, which restarts the pod"
                );
                self.set_state(
                    row,
                    InstanceState::Deploying,
                    Some("its configuration changed: restarting the pod with it".into()),
                );
                self.wake();
                Ok(())
            }
            InstanceState::Deploying => {
                let runtime = self
                    .runtime
                    .clone()
                    .ok_or("this server cannot deploy bridges any more")?;
                let name = row.deploy_name.clone().ok_or("the instance has no name")?;
                match runtime.status(&name).await? {
                    None => {
                        self.set_state(row, InstanceState::Registered, None);
                        self.wake();
                    }
                    Some(d) if d.ready => {
                        self.set_state(
                            row,
                            InstanceState::Starting,
                            Some("waiting for the bridge to answer this server".into()),
                        );
                        self.wake();
                    }
                    Some(d) => {
                        if now.saturating_sub(row.state_since_ms) > DEPLOY_TIMEOUT_MS {
                            self.set_state(
                                row,
                                InstanceState::Failed,
                                Some(
                                    explained(d.message)
                                        .unwrap_or_else(|| "the pod never became ready".into()),
                                ),
                            );
                        } else {
                            self.set_reason(row, explained(d.message));
                        }
                    }
                }
                Ok(())
            }
            InstanceState::Starting => {
                let id = row
                    .appservice_id
                    .clone()
                    .ok_or("the instance has no registration")?;
                let elsewhere = offering.runtime != "cluster";
                // The bridge may have announced itself already: a mautrix bridge pings itself
                // through this server as it starts (MSC2659), and an administrator running one
                // elsewhere can press Ping. The registry's `healthy` is the same evidence this
                // server's own ping would give, and it is there a tick earlier.
                let known = self
                    .directory
                    .health(&id)
                    .await
                    .map_err(|e| e.to_string())?;
                // An instance run by hand may take days: ask it every tick for the first two
                // minutes, when someone is most likely starting it, then every half minute.
                let since = now.saturating_sub(row.state_since_ms);
                let due = !elsewhere
                    || since < 120_000
                    || since % 30_000 <= u64::try_from(TICK.as_millis()).unwrap_or(u64::MAX);
                let health = if known.status == "healthy" {
                    known
                } else if due {
                    self.directory.ping(&id).await.map_err(|e| e.to_string())?
                } else {
                    return Ok(());
                };
                if health.status == "healthy" {
                    let _ = self
                        .store
                        .update_instance(&row.bridge_type, &row.owner, |r| {
                            if r.state != InstanceState::Starting {
                                return false;
                            }
                            r.enter(InstanceState::Ready, now);
                            r.reason = None;
                            r.ready_at_ms = Some(now);
                            true
                        });
                    tracing::info!(bridge_type = %row.bridge_type, owner = %row.owner, "bridge instance answered this server: ready");
                    self.wake();
                } else if !elsewhere && now.saturating_sub(row.state_since_ms) > START_TIMEOUT_MS {
                    self.set_state(
                        row,
                        InstanceState::Failed,
                        Some(format!(
                            "the bridge is running but never answered this server{}",
                            health
                                .last_error
                                .map(|e| format!(": {e}"))
                                .unwrap_or_default()
                        )),
                    );
                }
                Ok(())
            }
            InstanceState::Ready => {
                // The bot's identity first, so that it is published before the owner's client
                // ever looks at the bot. A failure here is noted on the row and does not stop
                // the chat from being made.
                match self.settle_bot_identity(row).await {
                    Ok(()) => {
                        if row
                            .reason
                            .as_deref()
                            .is_some_and(|r| r.starts_with(IDENTITY_REASON))
                        {
                            self.set_reason(row, None);
                        }
                    }
                    Err(e) => {
                        tracing::warn!(bridge_type = %row.bridge_type, owner = %row.owner, error = %e, "could not settle the bot's cross-signing identity");
                        self.set_reason(row, Some(format!("{IDENTITY_REASON}: {e}")));
                    }
                }
                if owner_of(row).is_some() {
                    if row.dm_room.is_none() {
                        self.invite_owner(row, &offering).await?;
                    } else if row.dm_started_by.as_deref() != Some(CHAT_BY_OWNER) {
                        self.settle_chat(row, &offering).await?;
                    }
                }
                Ok(())
            }
            InstanceState::Failed => Ok(()),
            InstanceState::Removing => self.remove(row).await,
        }
    }

    /// The row with its objects' name settled. A row that has one, or has no appservice id yet,
    /// is returned as it is. A registered row without one (see the module doc) is named now and
    /// stored: the old hashed name if the runtime has an object under it, so that a bridge
    /// already running is adopted rather than deployed a second time; otherwise the readable
    /// name. Once stored the name is never recomputed.
    async fn settle_deploy_name(&self, row: &InstanceRow) -> Result<InstanceRow, String> {
        let Some(appservice_id) = row.appservice_id.clone() else {
            return Ok(row.clone());
        };
        if row.deploy_name.is_some() {
            return Ok(row.clone());
        }
        let legacy = legacy_deploy_name(&appservice_id);
        let adopted = match &self.runtime {
            Some(runtime) => runtime.status(&legacy).await?.is_some(),
            None => false,
        };
        let name = if adopted {
            legacy
        } else {
            deploy_name(&appservice_id)
        };
        tracing::info!(
            bridge_type = %row.bridge_type,
            owner = %row.owner,
            appservice_id = %appservice_id,
            deploy_name = %name,
            adopted,
            "named the bridge instance's objects"
        );
        self.store
            .update_instance(&row.bridge_type, &row.owner, |r| {
                if r.deploy_name.is_some() {
                    return false;
                }
                r.deploy_name = Some(name.clone());
                true
            })
            .map_err(|e| e.to_string())?;
        self.store
            .instance(&row.bridge_type, &row.owner)
            .map_err(|e| e.to_string())?
            .ok_or_else(|| "the instance was removed".to_owned())
    }

    /// The row with the seeds of its bot's cross-signing keys, minted now if it had none and
    /// stored before anything is published, so that a crash between the two leaves keys the
    /// next step publishes again rather than a second identity.
    fn settle_cross_signing_seeds(&self, row: &InstanceRow) -> Result<InstanceRow, String> {
        if row.cross_signing_master_seed.is_some() && row.cross_signing_self_signing_seed.is_some()
        {
            return Ok(row.clone());
        }
        let seeds = Seeds::generate();
        tracing::info!(bridge_type = %row.bridge_type, owner = %row.owner, "minted the cross-signing keys of a bridge instance's bot");
        self.store
            .update_instance(&row.bridge_type, &row.owner, |r| {
                if r.cross_signing_master_seed.is_some()
                    && r.cross_signing_self_signing_seed.is_some()
                {
                    return false;
                }
                r.cross_signing_master_seed = Some(seeds.master.clone());
                r.cross_signing_self_signing_seed = Some(seeds.self_signing.clone());
                r.signed_bot_device = None;
                true
            })
            .map_err(|e| e.to_string())?;
        self.store
            .instance(&row.bridge_type, &row.owner)
            .map_err(|e| e.to_string())?
            .ok_or_else(|| "the instance was removed".to_owned())
    }

    /// Gives the instance's bot a cross-signing identity and signs its device with it
    /// (`crate::cross_signing`), so that a client which excludes insecure devices still shares
    /// room keys with the bridge. On every step of a ready instance until a device is signed,
    /// then every [`IDENTITY_RECHECK_MS`]: `/keys/query` as the bot; the master and
    /// self-signing keys published when the server's are not this identity's (none yet, or
    /// another instance's for the same owner, which the appservice may replace without
    /// user-interactive auth); each device of the bot without the self-signing key's signature
    /// signed, each one logged at `INFO` ("cross-signed the bridge bot's device") with the
    /// appservice, the bot, the device and the device the instance named before, so that an
    /// operator who reset a bridge's crypto store sees its new device signed; a look that signs
    /// nothing logs nothing. Records the signed device on the row for the admin API: the one
    /// signed in this look (after a reset, the bridge's current device) ahead of one found signed
    /// already.
    async fn settle_bot_identity(&self, row: &InstanceRow) -> Result<(), String> {
        let now = now_ms();
        let key = (row.bridge_type.clone(), row.owner.clone());
        if row.signed_bot_device.is_some()
            && self
                .identity_checked_ms
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .get(&key)
                .is_some_and(|at| now.saturating_sub(*at) < IDENTITY_RECHECK_MS)
        {
            return Ok(());
        }
        let client = self.client.get().ok_or("not started")?.clone();
        let token = row.as_token.clone().ok_or("the instance has no token")?;
        let (bot_localpart, _) = bridge_types::instance_names(&row.bridge_type, owner_of(row))
            .ok_or("its bridge type is no longer in the catalogue")?;
        let bot = self.mxid(&bot_localpart);
        let row = &self.settle_cross_signing_seeds(row)?;
        let seeds = Seeds {
            master: row
                .cross_signing_master_seed
                .clone()
                .ok_or("no master seed")?,
            self_signing: row
                .cross_signing_self_signing_seed
                .clone()
                .ok_or("no self-signing seed")?,
        };
        let identity = BotIdentity::from_seeds(&bot, &seeds)?;
        let keys = client
            .keys_query(&token, &bot)
            .await
            .map_err(|e| format!("could not read the bot's keys: {e}"))?;
        if !identity.is_published_master(keys["master_keys"].get(&bot)) {
            client
                .upload_cross_signing_keys(&token, &bot, identity.upload_body()?)
                .await
                .map_err(|e| format!("could not publish the bot's cross-signing keys: {e}"))?;
            tracing::info!(
                bridge_type = %row.bridge_type,
                owner = %row.owner,
                bot = %bot,
                master_key = %identity.master_key_id(),
                "published the bot's cross-signing keys"
            );
        }
        let devices = keys["device_keys"]
            .get(&bot)
            .and_then(serde_json::Value::as_object)
            .cloned()
            .unwrap_or_default();
        // A device found signed already, and the one signed in this look: the latter is the
        // one the instance names, since after a reset of the bridge's crypto store the old
        // device stays on the server, signed, in whatever order `/keys/query` lists them.
        let mut already_signed = None;
        let mut signed_now: Option<String> = None;
        for (device_id, device_keys) in &devices {
            if identity.has_signed_device(device_keys) {
                already_signed = Some(device_id.clone());
                continue;
            }
            if device_keys.get("keys").is_none() {
                continue; // a device without keys yet (nothing to sign)
            }
            client
                .upload_signatures(&token, &bot, identity.sign_device(device_id, device_keys)?)
                .await
                .map_err(|e| format!("could not sign the bot's device {device_id}: {e}"))?;
            // One line per signature uploaded, never per look: the previous device is the one
            // the instance named (none on the first signing by this identity).
            let previous_device = signed_now.as_deref().or(row.signed_bot_device.as_deref());
            tracing::info!(
                bridge_type = %row.bridge_type,
                owner = %row.owner,
                appservice = %row.appservice_id.as_deref().unwrap_or_default(),
                bot = %bot,
                device = %device_id,
                first_signing = previous_device.is_none(),
                previous_device = previous_device.map(tracing::field::display),
                "cross-signed the bridge bot's device with the server-held self-signing key"
            );
            signed_now = Some(device_id.clone());
        }
        // Every device with keys is signed now. The one the bridge uses is the one the instance
        // names; the ones it left behind are removed.
        let in_use = self
            .settle_bot_devices(row, &client, &token, &bot, &devices, now)
            .await?;
        let signed = in_use.or(signed_now).or(already_signed);
        self.identity_checked_ms
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(key, now);
        if signed.is_some() && signed != row.signed_bot_device {
            let _ = self
                .store
                .update_instance(&row.bridge_type, &row.owner, |r| {
                    if r.signed_bot_device == signed {
                        return false;
                    }
                    r.signed_bot_device = signed.clone();
                    true
                });
        }
        Ok(())
    }

    /// Removes the bot's devices the bridge left behind, and returns the one it uses.
    ///
    /// A bridge whose crypto store is reset (or whose database is) makes a new device and
    /// leaves the old one registered, with its keys: clients go on encrypting room keys to it,
    /// the bridge drops what arrives for it ("targeted to someone else"), and a client that
    /// withholds keys withholds them from both. The rule ([`bot_devices_plan`]): the device in
    /// use is the bot's device with keys that the server saw last (`GET /devices`'s
    /// `last_seen_ts`, written when an appservice acts as the device and when the device is
    /// made); any other device last seen before it and not for [`STALE_BOT_DEVICE_MS`] is
    /// removed, through the instance's own token (`DELETE /devices/{id}`, which MSC4190 lets
    /// an appservice do without user-interactive auth; the server removes the device's keys
    /// and tells the bot's rooms). The device in use is never removed, nor is a device the
    /// server has no time for, and nothing is removed when two devices were seen at the same
    /// moment. Each removal is logged at `INFO` with the instance, the bot, the device and the
    /// device kept, and recorded on the row for the admin API.
    async fn settle_bot_devices(
        &self,
        row: &InstanceRow,
        client: &MatrixClient,
        token: &str,
        bot: &str,
        keyed: &serde_json::Map<String, Value>,
        now: u64,
    ) -> Result<Option<String>, String> {
        let with_keys: Vec<&str> = keyed
            .iter()
            .filter(|(_, keys)| keys.get("keys").is_some())
            .map(|(id, _)| id.as_str())
            .collect();
        if with_keys.is_empty() {
            return Ok(None); // no device in use yet, so none left behind
        }
        let listed = client
            .devices(token, bot)
            .await
            .map_err(|e| format!("could not list the bot's devices: {e}"))?;
        let Some((in_use, stale)) = bot_devices_plan(&listed, &with_keys, now) else {
            return Ok(None);
        };
        for (device_id, last_seen) in stale {
            client
                .delete_device(token, bot, &device_id)
                .await
                .map_err(|e| format!("could not remove the bot's old device {device_id}: {e}"))?;
            tracing::info!(
                bridge_type = %row.bridge_type,
                owner = %row.owner,
                appservice = %row.appservice_id.as_deref().unwrap_or_default(),
                bot = %bot,
                device = %device_id,
                last_seen = %rfc3339(last_seen),
                kept_device = %in_use,
                "removed a device the bridge bot no longer uses (the bridge moved on to a newer one)"
            );
            let removed = crate::store::RemovedBotDevice {
                device_id,
                removed_at_ms: now,
                last_seen_ms: Some(last_seen),
                kept_device: in_use.clone(),
            };
            let _ = self
                .store
                .update_instance(&row.bridge_type, &row.owner, |r| {
                    r.removed_bot_devices.push(removed.clone());
                    let over = r
                        .removed_bot_devices
                        .len()
                        .saturating_sub(crate::store::REMOVED_BOT_DEVICES_KEPT);
                    r.removed_bot_devices.drain(..over);
                    true
                });
        }
        Ok(Some(in_use))
    }

    fn set_state(&self, row: &InstanceRow, state: InstanceState, reason: Option<String>) {
        let from = row.state;
        let now = now_ms();
        let moved = self
            .store
            .update_instance(&row.bridge_type, &row.owner, |r| {
                if r.state != from {
                    return false; // someone else moved it
                }
                r.enter(state, now);
                r.reason = reason.clone();
                true
            });
        if matches!(moved, Ok(Some(_))) {
            tracing::info!(
                bridge_type = %row.bridge_type,
                owner = %row.owner,
                from = from.as_str(),
                to = state.as_str(),
                reason = reason.as_deref().unwrap_or_default(),
                "bridge instance moved"
            );
        }
    }

    fn set_reason(&self, row: &InstanceRow, reason: Option<String>) {
        let _ = self
            .store
            .update_instance(&row.bridge_type, &row.owner, |r| {
                if r.reason == reason {
                    return false;
                }
                r.reason = reason.clone();
                true
            });
    }

    async fn allocate_and_register(
        &self,
        row: &InstanceRow,
        offering: &OfferingRow,
    ) -> Result<(), String> {
        let port = bridge_types::get(&row.bridge_type, &self.server_name)
            .ok_or("its bridge type is no longer in the catalogue")?
            .port;
        // Names and tokens first, stored, so that a retry (or another replica) registers the
        // same thing rather than a second one.
        let row = if row.appservice_id.is_some() {
            row.clone()
        } else {
            let appservice_id = self.free_appservice_id(row).await?;
            let name = deploy_name(&appservice_id);
            let url = match (&self.runtime, offering.runtime.as_str()) {
                (Some(runtime), "cluster") => runtime.service_url(&name, port),
                _ => format!("http://{appservice_id}:{port}"),
            };
            self.store
                .update_instance(&row.bridge_type, &row.owner, |r| {
                    if r.appservice_id.is_some() {
                        return false;
                    }
                    r.appservice_id = Some(appservice_id.clone());
                    r.deploy_name = Some(name.clone());
                    r.as_token = Some(crate::random_hex(32));
                    r.hs_token = Some(crate::random_hex(32));
                    r.provisioning_secret = Some(crate::random_hex(32));
                    r.pickle_key = Some(crate::random_hex(32));
                    r.url = Some(url.clone());
                    true
                })
                .map_err(|e| e.to_string())?;
            self.store
                .instance(&row.bridge_type, &row.owner)
                .map_err(|e| e.to_string())?
                .ok_or("the instance was removed")?
        };
        let render = self
            .render(&row, offering)
            .ok_or("the instance could not be rendered")?;
        let id = row.appservice_id.clone().unwrap_or_default();
        let fingerprint = registration_fingerprint(&render.registration);
        match self
            .directory
            .create(AdminAppserviceCreate {
                registration: Some(render.registration.clone()),
                registration_yaml: None,
            })
            .await
        {
            Ok(_) => {}
            Err(SourceError::Conflict(detail)) => {
                // Ours already, from an earlier attempt? Then carry on.
                let ours = self
                    .directory
                    .registration(&id)
                    .await
                    .map(|r| r.json["as_token"] == render.registration["as_token"])
                    .unwrap_or(false);
                if !ours {
                    self.set_state(
                        &row,
                        InstanceState::Failed,
                        Some(format!("could not register it: {detail}")),
                    );
                    return Ok(());
                }
            }
            Err(e) => return Err(format!("could not register it: {e}")),
        }
        let _ = self
            .store
            .update_instance(&row.bridge_type, &row.owner, |r| {
                r.registered_fingerprint = Some(fingerprint.clone());
                true
            });
        self.set_state(&row, InstanceState::Registered, None);
        self.wake();
        Ok(())
    }

    /// The row with the registry's copy of its registration brought up to date: when the
    /// offering's options change what the registration claims (double puppeting is the
    /// owner's non-exclusive namespace; encryption is the MSC3202 and MSC4190 flags), the
    /// namespaces and the flags are patched on the server's side, once per change
    /// (`InstanceRow::registered_fingerprint`). The files change with it, which rolls a
    /// deployed instance's pod through the deployment fingerprint; an instance run elsewhere
    /// gets a reason saying to fetch its files again. A row from before the fingerprint was
    /// kept is patched once, with what it already has.
    async fn settle_registration(
        &self,
        row: &InstanceRow,
        offering: &OfferingRow,
    ) -> Result<InstanceRow, String> {
        if !matches!(
            row.state,
            InstanceState::Registered
                | InstanceState::Deploying
                | InstanceState::Starting
                | InstanceState::Ready
        ) {
            return Ok(row.clone());
        }
        let (Some(id), Some(render)) = (row.appservice_id.clone(), self.render(row, offering))
        else {
            return Ok(row.clone());
        };
        let fingerprint = registration_fingerprint(&render.registration);
        if row.registered_fingerprint.as_deref() == Some(fingerprint.as_str()) {
            return Ok(row.clone());
        }
        let kind = bridge_types::get(&row.bridge_type, &self.server_name)
            .ok_or("its bridge type is no longer in the catalogue")?;
        let mut patch = serde_json::Map::new();
        patch.insert(
            "namespaces".to_owned(),
            render.registration["namespaces"].clone(),
        );
        for feature in &kind.required_features {
            patch.insert(
                feature.clone(),
                render
                    .registration
                    .get(feature)
                    .cloned()
                    .unwrap_or(Value::Null),
            );
        }
        self.directory
            .update(&id, Value::Object(patch))
            .await
            .map_err(|e| format!("could not update its registration: {e}"))?;
        let changed = row.registered_fingerprint.is_some();
        let elsewhere = offering.runtime != "cluster";
        tracing::info!(
            bridge_type = %row.bridge_type,
            owner = %row.owner,
            appservice = %id,
            double_puppeting = offering.options.double_puppeting.unwrap_or(true),
            encryption = offering.options.encryption.unwrap_or(true),
            first_time = !changed,
            "the bridge instance's registration changed: updated this server's copy of it"
        );
        let updated = self
            .store
            .update_instance(&row.bridge_type, &row.owner, |r| {
                r.registered_fingerprint = Some(fingerprint.clone());
                if changed && elsewhere && r.state != InstanceState::Registered {
                    r.reason = Some(REREGISTERED_REASON.to_owned());
                }
                true
            })
            .map_err(|e| e.to_string())?;
        Ok(updated.unwrap_or_else(|| row.clone()))
    }

    /// `whatsapp-alice`, or `whatsapp-alice-2` if that is somebody else's.
    async fn free_appservice_id(&self, row: &InstanceRow) -> Result<String, String> {
        let stem = match owner_of(row) {
            Some(owner) => {
                let localpart = owner
                    .trim_start_matches('@')
                    .split(':')
                    .next()
                    .unwrap_or_default();
                format!(
                    "{}-{}",
                    short_name(&row.bridge_type),
                    bridge_types::encode_localpart(localpart)
                )
            }
            None => short_name(&row.bridge_type).to_owned(),
        };
        for n in 1..100 {
            let candidate = if n == 1 {
                stem.clone()
            } else {
                format!("{stem}-{n}")
            };
            match self.directory.get(&candidate).await {
                Ok(None) => return Ok(candidate),
                Ok(Some(_)) => {
                    let owner_there = self
                        .directory
                        .registration(&candidate)
                        .await
                        .ok()
                        .and_then(|r| r.json[BRIDGE_INSTANCE_KEY].as_str().map(str::to_owned));
                    if owner_there.as_deref() == Some(row.owner.as_str()) {
                        return Ok(candidate);
                    }
                }
                Err(e) => return Err(e.to_string()),
            }
        }
        Err("no free appservice id".into())
    }

    async fn invite_owner(&self, row: &InstanceRow, offering: &OfferingRow) -> Result<(), String> {
        let client = self.client.get().ok_or("not started")?.clone();
        let owner = owner_of(row).ok_or("a shared instance has no owner")?;
        let token = row.as_token.clone().ok_or("the instance has no token")?;
        let (bot_localpart, _) = bridge_types::instance_names(&row.bridge_type, Some(owner))
            .ok_or("its bridge type is no longer in the catalogue")?;
        let bot = self.mxid(&bot_localpart);
        client
            .ensure_user(&token, &bot_localpart)
            .await
            .map_err(|e| e.to_string())?;
        let kind = bridge_types::get(&row.bridge_type, &self.server_name);
        let encrypted = offering.options.encryption.unwrap_or(true);
        // A mautrix bridge takes bare commands (`login qr`) only in a person's management
        // room, and it marks a room as that only when the person invites its bot into a chat
        // with just the two of them (bridgev2's `handleBotInvite`). Anywhere else a message
        // without the bridge's command prefix is dropped without a word, and a chat the bot
        // started and the owner accepted is anywhere else: typing in it did nothing
        // (`docs/status/11-appservices-and-bridges.md`, 2026-10-02). So, where the instance
        // may act as its owner (double puppeting: the registration's non-exclusive claim on
        // them), the chat is started as the owner with the bot invited, and the bot is joined
        // here as well so the steps can be posted at once; the bridge accepts the invite it is
        // sent, finds two members and marks the room. Without that claim the chat can only be
        // started by the bot, and the steps say to start one.
        let as_owner = may_act_as_owner(kind.as_ref(), offering);
        let room = if as_owner {
            let room = client
                .create_dm(&token, owner, &bot, encrypted)
                .await
                .map_err(|e| format!("could not start the chat as its owner: {e}"))?;
            client
                .join(&token, &bot, &room)
                .await
                .map_err(|e| format!("the bot could not join the chat: {e}"))?;
            if let Err(e) = client.add_direct(&token, &bot, owner, &room).await {
                tracing::debug!(error = %e, owner, "could not mark the chat direct for the bot");
            }
            room
        } else {
            client
                .create_dm(&token, &bot, owner, encrypted)
                .await
                .map_err(|e| e.to_string())?
        };
        tracing::info!(
            bridge_type = %row.bridge_type,
            owner,
            room,
            started_by = if as_owner { "owner" } else { "bot" },
            encrypted,
            "started the owner's chat with their bridge's bot"
        );
        let recorded = self
            .store
            .update_instance(&row.bridge_type, &row.owner, |r| {
                if r.dm_room.is_some() {
                    return false;
                }
                r.dm_room = Some(room.clone());
                r.dm_started_by = Some(if as_owner { CHAT_BY_OWNER } else { CHAT_BY_BOT }.into());
                true
            })
            .map_err(|e| e.to_string())?;
        if recorded.is_none() {
            return Ok(()); // another replica got there first
        }
        let name = bridge_types::display_name(&row.bridge_type).unwrap_or(&row.bridge_type);
        let steps = sign_in_steps(kind.as_ref());
        let mut text = format!("This is your own {name} bridge. To sign in:\n");
        let mut html = format!("<p>This is your own {name} bridge. To sign in:</p><ol>");
        if !as_owner {
            // The bridge will not take commands in a chat its bot started.
            let line = format!(
                "The bridge only takes commands in a chat you start: invite {bot} to a new direct chat, then follow these steps there."
            );
            text = format!("This is your own {name} bridge. {line}\n");
            html = format!(
                "<p>This is your own {name} bridge. {}</p><ol>",
                crate::front_door::inline_html(&line)
            );
        }
        for step in steps.iter() {
            text.push_str(&format!("- {step}\n"));
            html.push_str(&format!(
                "<li>{}</li>",
                crate::front_door::inline_html(step)
            ));
        }
        html.push_str("</ol>");
        let _ = client.notice(&token, &bot, &room, &text, &html).await;
        // And where they asked for it, say it is done.
        if let (Some(door_room), Some(door)) = (
            &row.front_door_room,
            bridge_types::front_door_localpart(&row.bridge_type),
        ) && let Ok(tokens) = self.tokens()
        {
            let (text, html) = if as_owner {
                (
                    format!(
                        "Your {name} bridge is ready. I've started a chat for you with {bot}: open it and follow the steps there to sign in."
                    ),
                    format!(
                        "Your {name} bridge is ready. I've started a chat for you with <a href=\"https://matrix.to/#/{bot}\">{bot}</a>: open it and follow the steps there to sign in."
                    ),
                )
            } else {
                (
                    format!(
                        "Your {name} bridge is ready. I've invited you to a chat with {bot}: accept it and follow the steps there to sign in."
                    ),
                    format!(
                        "Your {name} bridge is ready. I've invited you to a chat with <a href=\"https://matrix.to/#/{bot}\">{bot}</a>: accept it and follow the steps there to sign in."
                    ),
                )
            };
            let _ = client
                .notice(&tokens.as_token, &self.mxid(door), door_room, &text, &html)
                .await;
        }
        Ok(())
    }

    /// Settles whose the owner's chat is, and repairs one the bot started where it can.
    ///
    /// A mautrix bridge takes bare commands only in the person's management room, which it
    /// marks when *the person invites its bot* into a chat of two (bridgev2's
    /// `handleBotInvite`); a chat its bot started is never one, however long the person types
    /// in it. Every chat the manager made before 2026-10-02 was such a chat, and so is one made
    /// without double puppeting. Here, for a row whose chat has not been settled
    /// ([`InstanceRow::dm_started_by`] not `owner`):
    ///
    /// 1. If the bot is not in the chat any more, there is nothing to repair: the chat is
    ///    forgotten and the next step starts a new one.
    /// 2. If the owner started it, it is recorded as theirs. Nothing else to do.
    /// 3. If the bot started it and the instance may act as the owner (double puppeting), it is
    ///    repaired in place: the bot leaves, the owner re-invites it, the bot rejoins. The
    ///    bridge is sent the invitation, accepts it, finds two members and marks the room; the
    ///    bot then says why it had been silent and what to type. Should the owner's invitation
    ///    fail with the bot already out, the chat is forgotten and a new one is started as the
    ///    owner.
    /// 4. If the bot started it and the instance cannot act as the owner, the bot says so in
    ///    the chat, once: commands there need the bridge's prefix, or a chat the owner starts.
    ///
    /// The owner must be in the chat for 3: until they are (an invitation never accepted),
    /// the chat is recorded as the bot's and looked at again each step.
    async fn settle_chat(&self, row: &InstanceRow, offering: &OfferingRow) -> Result<(), String> {
        let kind = bridge_types::get(&row.bridge_type, &self.server_name);
        let as_owner = may_act_as_owner(kind.as_ref(), offering);
        let said_already = row.dm_started_by.as_deref() == Some(CHAT_BY_BOT);
        if said_already && !as_owner {
            return Ok(()); // case 4 was done; nothing more the manager can do
        }
        let client = self.client.get().ok_or("not started")?.clone();
        let owner = owner_of(row).ok_or("a shared instance has no owner")?;
        let token = row.as_token.clone().ok_or("the instance has no token")?;
        let room = row.dm_room.clone().ok_or("the instance has no chat")?;
        let (bot_localpart, _) = bridge_types::instance_names(&row.bridge_type, Some(owner))
            .ok_or("its bridge type is no longer in the catalogue")?;
        let bot = self.mxid(&bot_localpart);
        let name = bridge_types::display_name(&row.bridge_type).unwrap_or(&row.bridge_type);
        let members = match client.joined_members(&token, &bot, &room).await {
            Ok(members) => members,
            Err(e) if e.status == 403 || e.status == 404 => {
                tracing::info!(
                    bridge_type = %row.bridge_type,
                    owner,
                    room,
                    error = %e,
                    "the bot is not in the owner's chat any more: forgetting it, a new chat will be started"
                );
                self.forget_chat(row, &room);
                return Ok(());
            }
            Err(e) => return Err(format!("could not read the owner's chat: {e}")),
        };
        let creator = if said_already {
            bot.clone()
        } else {
            client
                .room_creator(&token, &bot, &room)
                .await
                .map_err(|e| format!("could not read who started the owner's chat: {e}"))?
        };
        if creator == owner {
            self.record_chat(row, &room, CHAT_BY_OWNER);
            tracing::debug!(bridge_type = %row.bridge_type, owner, room, "the owner's chat was started by the owner");
            return Ok(());
        }
        if !members.iter().any(|m| m == owner) {
            if self.record_chat(row, &room, CHAT_BY_BOT) {
                tracing::info!(
                    bridge_type = %row.bridge_type,
                    owner,
                    room,
                    "the owner's chat was started by its bot and the owner is not in it; it will be repaired once they are"
                );
            }
            return Ok(());
        }
        if !as_owner {
            self.record_chat(row, &room, CHAT_BY_BOT);
            tracing::info!(
                bridge_type = %row.bridge_type,
                owner,
                room,
                "the owner's chat was started by its bot and the instance cannot act as the owner: saying there that commands need the bridge's prefix"
            );
            let prefix = kind.as_ref().and_then(|k| k.command_prefix.clone());
            let (text, html) = prefixed_commands_notice(name, &bot, prefix.as_deref());
            let _ = client.notice(&token, &bot, &room, &text, &html).await;
            return Ok(());
        }
        // Case 3: the repair.
        client
            .leave(&token, &bot, &room)
            .await
            .map_err(|e| format!("the bot could not leave the chat it started: {e}"))?;
        let back = async {
            client
                .invite(&token, owner, &room, &bot)
                .await
                .map_err(|e| format!("the owner could not invite the bot back: {e}"))?;
            client
                .join(&token, &bot, &room)
                .await
                .map_err(|e| format!("the bot could not rejoin the chat: {e}"))
        }
        .await;
        if let Err(e) = back {
            tracing::warn!(
                bridge_type = %row.bridge_type,
                owner,
                room,
                error = %e,
                "could not repair the owner's chat; forgetting it, a new chat will be started as the owner"
            );
            self.forget_chat(row, &room);
            return Err(format!("{e}; a new chat will be started as the owner"));
        }
        self.record_chat(row, &room, CHAT_BY_OWNER);
        tracing::info!(
            bridge_type = %row.bridge_type,
            owner,
            room,
            "repaired the owner's chat with their bridge's bot: the bot left and came back on the owner's invitation, so the bridge takes commands there"
        );
        let steps = sign_in_steps(kind.as_ref());
        let (text, html) = repaired_chat_notice(name, &steps);
        let _ = client.notice(&token, &bot, &room, &text, &html).await;
        Ok(())
    }

    /// Records whose `room` is on the row, if the row still names it. `true` when that changed
    /// anything.
    fn record_chat(&self, row: &InstanceRow, room: &str, by: &str) -> bool {
        self.store
            .update_instance(&row.bridge_type, &row.owner, |r| {
                if r.dm_room.as_deref() != Some(room) || r.dm_started_by.as_deref() == Some(by) {
                    return false;
                }
                r.dm_started_by = Some(by.to_owned());
                true
            })
            .ok()
            .flatten()
            .is_some()
    }

    /// Forgets `room` as the owner's chat, if the row still names it, so the next step starts
    /// a new one.
    fn forget_chat(&self, row: &InstanceRow, room: &str) {
        let _ = self
            .store
            .update_instance(&row.bridge_type, &row.owner, |r| {
                if r.dm_room.as_deref() != Some(room) {
                    return false;
                }
                r.dm_room = None;
                r.dm_started_by = None;
                true
            });
        self.wake();
    }

    async fn remove(&self, row: &InstanceRow) -> Result<(), String> {
        tracing::info!(bridge_type = %row.bridge_type, owner = %row.owner, appservice_id = row.appservice_id.as_deref().unwrap_or_default(), "removing bridge instance");
        if let (Some(runtime), Some(name)) = (&self.runtime, &row.deploy_name) {
            runtime.delete(name).await?;
        }
        if let Some(id) = &row.appservice_id {
            match self.directory.delete(id).await {
                Ok(()) | Err(SourceError::NotFound) => {}
                Err(e) => return Err(e.to_string()),
            }
        }
        self.store
            .delete_instance(&row.bridge_type, &row.owner)
            .map_err(|e| e.to_string())
    }

    // ---- requests, from the admin API and the front doors -----------------------------------

    /// Whether `user_id` may have an instance of `offering`.
    pub(crate) fn allowed(&self, offering: &OfferingRow, user_id: &str) -> bool {
        self.is_local(user_id)
            && (offering.access.all_local_users
                || offering.access.users.iter().any(|u| u == user_id))
    }

    pub(crate) fn is_local(&self, user_id: &str) -> bool {
        user_id.starts_with('@')
            && user_id
                .rsplit_once(':')
                .is_some_and(|(_, s)| s == self.server_name)
    }

    /// Creates (or retries) the instance of `bridge_type` for `owner`, noting `front_door_room`
    /// to tell them in. Returns it as it now is, and whether this call created it.
    pub(crate) fn request(
        &self,
        bridge_type: &str,
        owner: &str,
        front_door_room: Option<&str>,
    ) -> Result<(InstanceRow, bool), StoreError> {
        let now = now_ms();
        self.clear_backoff(bridge_type, Some(owner));
        let mut row = InstanceRow::new(bridge_type, owner, now);
        row.front_door_room = front_door_room.map(str::to_owned);
        let (existing, created) = self.store.insert_instance(&row)?;
        let out = if created {
            existing
        } else {
            self.store
                .update_instance(bridge_type, owner, |r| {
                    let mut changed = false;
                    if r.state == InstanceState::Failed {
                        // Try again, with the same names and tokens.
                        r.enter(
                            if r.appservice_id.is_some() {
                                InstanceState::Registered
                            } else {
                                InstanceState::Requested
                            },
                            now,
                        );
                        r.reason = None;
                        changed = true;
                    }
                    if let Some(room) = front_door_room
                        && r.front_door_room.as_deref() != Some(room)
                    {
                        r.front_door_room = Some(room.to_owned());
                        changed = true;
                    }
                    changed
                })?
                .unwrap_or(existing)
        };
        self.wake();
        Ok((out, created))
    }

    pub(crate) fn offering_row(
        &self,
        bridge_type: &str,
    ) -> Result<Option<OfferingRow>, StoreError> {
        self.store.offering(bridge_type)
    }

    pub(crate) fn instance_row(
        &self,
        bridge_type: &str,
        owner: &str,
    ) -> Result<Option<InstanceRow>, StoreError> {
        self.store.instance(bridge_type, owner)
    }

    pub(crate) fn offering_rows(&self) -> Result<Vec<OfferingRow>, StoreError> {
        self.store.offerings()
    }

    /// Stops and removes `owner`'s instance of `bridge_type` now.
    pub(crate) async fn stop(&self, bridge_type: &str, owner: &str) -> Result<bool, String> {
        tracing::info!(bridge_type, owner, "asked to stop a bridge instance");
        self.clear_backoff(bridge_type, Some(owner));
        let Some(row) = self
            .store
            .instance(bridge_type, owner)
            .map_err(|e| e.to_string())?
        else {
            return Ok(false);
        };
        self.set_state(&row, InstanceState::Removing, None);
        let row = self
            .store
            .instance(bridge_type, owner)
            .map_err(|e| e.to_string())?
            .unwrap_or(row);
        self.remove(&row).await?;
        Ok(true)
    }
}

/// What a bridge that will not start is telling, in the words of its own log line (which the
/// operator carries into the `Bridge`'s status), said as what it means and how to recover; `None`
/// for anything not recognised.
///
/// - `the supplied account key is invalid` (mautrix): the bridge was started with a
///   `pickle_key` other than the one its crypto store was made with. It happened on the demo
///   on 2026-10-09, to a bridge first started before the server rendered a key, when a roll
///   dropped the bridge's own; the operator's init container has carried it since. The store
///   cannot be read without the old key, so the recovery is resetting it.
#[must_use]
pub fn explain_deployment(message: &str) -> Option<String> {
    message
        .contains("the supplied account key is invalid")
        .then(|| {
            "the bridge cannot read its encryption store: it was started with a different \
             pickle key from the one the store was made with (\"the supplied account key is \
             invalid\"), and it will not start until the store is reset. The steps are in \
             docs/bridges/mautrix.md, \"The supplied account key is invalid\"; the person's \
             sign-in survives them"
                .to_owned()
        })
}

/// `message` explained where [`explain_deployment`] knows it, else as it is.
fn explained(message: Option<String>) -> Option<String> {
    message.map(|m| explain_deployment(&m).unwrap_or(m))
}

/// A fingerprint of what a deployment asks the runtime for: the image, the port, the arguments
/// and every rendered file (16 hex digits of SHA-256). The manager keeps the last applied one
/// on the row (`InstanceRow::applied_fingerprint`) and applies the deployment again when it
/// changes. The name and labels are left out: they are decided once and never change.
#[must_use]
pub fn deploy_fingerprint(spec: &DeploySpec) -> String {
    let mut hasher = Sha256::new();
    for part in [
        spec.image_repository.as_str(),
        spec.image_tag.as_str(),
        &spec.port.to_string(),
        spec.owner.as_deref().unwrap_or_default(),
        spec.storage_size.as_deref().unwrap_or_default(),
    ] {
        hasher.update(part.as_bytes());
        hasher.update([0]);
    }
    for arg in &spec.args {
        hasher.update(arg.as_bytes());
        hasher.update([0]);
    }
    for (name, contents) in &spec.files {
        hasher.update(name.as_bytes());
        hasher.update([0]);
        hasher.update(contents.as_bytes());
        hasher.update([0]);
    }
    hex::encode(&hasher.finalize()[..8])
}

/// A fingerprint of what a rendered registration claims on this server: everything in it but
/// `url` (an administrator may point an instance run elsewhere at where it really listens, and
/// that is theirs to keep), 16 hex digits of SHA-256. The manager keeps the last one written
/// to the registry on the row (`InstanceRow::registered_fingerprint`) and patches the
/// registry's copy when it changes.
#[must_use]
pub fn registration_fingerprint(registration: &Value) -> String {
    let mut claims: BTreeMap<String, Value> = registration
        .as_object()
        .map(|o| o.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
        .unwrap_or_default();
    claims.remove("url");
    let canonical = serde_json::to_string(&claims).unwrap_or_default();
    hex::encode(&Sha256::digest(canonical.as_bytes())[..8])
}

/// Which of a bot's devices the bridge uses, and which it left behind.
///
/// `listed` is the bot's devices with their `last_seen_ts` (`GET /devices`), `with_keys` the ids
/// of those with device keys published (`/keys/query`). The device in use is the device with
/// keys seen last; `None` when no device with keys has a time, or when two share the latest
/// (nothing can be told apart then, so nothing is removed). Left behind: every other device
/// last seen before the one in use and at least [`STALE_BOT_DEVICE_MS`] before `now`, with its
/// last-seen time. A device without a time is never left behind; one without keys is, once a
/// device with keys was seen after it (a device a reset made and abandoned before uploading
/// any).
#[must_use]
pub fn bot_devices_plan(
    listed: &[(String, Option<u64>)],
    with_keys: &[&str],
    now: u64,
) -> Option<(String, Vec<(String, u64)>)> {
    let mut seen_with_keys: Vec<(&str, u64)> = listed
        .iter()
        .filter(|(id, _)| with_keys.contains(&id.as_str()))
        .filter_map(|(id, seen)| seen.map(|s| (id.as_str(), s)))
        .collect();
    seen_with_keys.sort_by_key(|(_, seen)| std::cmp::Reverse(*seen));
    let (in_use, in_use_seen) = *seen_with_keys.first()?;
    if seen_with_keys
        .get(1)
        .is_some_and(|(_, seen)| *seen == in_use_seen)
    {
        return None;
    }
    let stale = listed
        .iter()
        .filter(|(id, _)| id != in_use)
        .filter_map(|(id, seen)| seen.map(|s| (id.clone(), s)))
        .filter(|(_, seen)| *seen < in_use_seen && now.saturating_sub(*seen) >= STALE_BOT_DEVICE_MS)
        .collect();
    Some((in_use.to_owned(), stale))
}

pub(crate) fn owner_of(row: &InstanceRow) -> Option<&str> {
    (row.owner != SHARED_INSTANCE).then_some(row.owner.as_str())
}

fn mode(bridge_type: &str) -> String {
    bridge_types::get(bridge_type, "x")
        .map(|k| k.mode)
        .unwrap_or_default()
}

fn label_safe(s: &str) -> String {
    let cleaned: String = s
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                c
            } else {
                '-'
            }
        })
        .take(63)
        .collect();
    cleaned
        .trim_matches(|c: char| !c.is_ascii_alphanumeric())
        .to_owned()
}

#[async_trait]
impl<B: KvBackend + 'static> BridgeOfferingSource for BridgeManager<B> {
    async fn target(&self) -> BridgeDeploymentTarget {
        match &self.runtime {
            Some(runtime) => runtime.target(),
            None => BridgeDeploymentTarget {
                available: false,
                namespace: None,
                homeserver_url: None,
                reason: Some(
                    "this server is not running in Kubernetes with the chart's bridges enabled, so bridges run elsewhere, from their files".into(),
                ),
            },
        }
    }

    async fn list(&self) -> Result<Vec<BridgeOffering>, SourceError> {
        let rows = self.store.offerings().map_err(store_err)?;
        let rows = rows.iter();
        let mut views = Vec::new();
        for row in rows {
            views.push(self.offering_view(row).await?);
        }
        Ok(views)
    }

    async fn get(&self, bridge_type: &str) -> Result<Option<BridgeOffering>, SourceError> {
        match self.store.offering(bridge_type).map_err(store_err)? {
            Some(row) => Ok(Some(self.offering_view(&row).await?)),
            None => Ok(None),
        }
    }

    async fn put(
        &self,
        bridge_type: &str,
        request: BridgeOfferingRequest,
    ) -> Result<BridgeOffering, SourceError> {
        let kind =
            bridge_types::get(bridge_type, &self.server_name).ok_or(SourceError::NotFound)?;
        let current = self.store.offering(bridge_type).map_err(store_err)?;
        let can_deploy = self.runtime.is_some() && kind.deployable;
        let runtime = request
            .runtime
            .clone()
            .or_else(|| current.as_ref().map(|c| c.runtime.clone()))
            .unwrap_or_else(|| {
                if can_deploy {
                    "cluster".into()
                } else {
                    "elsewhere".into()
                }
            });
        match runtime.as_str() {
            "cluster" if self.runtime.is_none() => {
                return Err(SourceError::InvalidField {
                    pointer: "/runtime",
                    detail: "this server cannot deploy bridges: it is not running in Kubernetes with the chart's bridges enabled".into(),
                });
            }
            "cluster" if !kind.deployable => {
                return Err(SourceError::InvalidField {
                    pointer: "/runtime",
                    detail: format!(
                        "{} needs settings only an administrator can write, so it runs elsewhere",
                        kind.name
                    ),
                });
            }
            "cluster" | "elsewhere" => {}
            _ => {
                return Err(SourceError::InvalidField {
                    pointer: "/runtime",
                    detail: "runtime is `cluster` or `elsewhere`".into(),
                });
            }
        }
        // The tag asked for, else the one kept, else (and for `latest`, which is not a pin:
        // decision 0037) the catalogue's.
        let image_tag = bridge_types::image_tag(
            bridge_type,
            request
                .image_tag
                .as_deref()
                .filter(|t| !t.trim().is_empty())
                .or(current.as_ref().map(|c| c.image_tag.as_str())),
        )
        .unwrap_or_else(|| "latest".to_owned());
        let row = OfferingRow {
            bridge_type: bridge_type.to_owned(),
            enabled: request
                .enabled
                .or(current.as_ref().map(|c| c.enabled))
                .unwrap_or(true),
            runtime,
            image_tag,
            access: request
                .access
                .clone()
                .or(current.as_ref().map(|c| c.access.clone()))
                .unwrap_or_default(),
            options: request
                .options
                .clone()
                .or(current.as_ref().map(|c| c.options.clone()))
                .unwrap_or_default(),
            created_at_ms: current.as_ref().map_or_else(now_ms, |c| c.created_at_ms),
        };
        self.store.put_offering(&row).map_err(store_err)?;
        if let Err(e) = self.sync_registration().await {
            // Its front door could not be registered: put things back as they were.
            match &current {
                Some(c) => self.store.put_offering(c).map_err(store_err)?,
                None => self.store.delete_offering(bridge_type).map_err(store_err)?,
            }
            return Err(match e {
                SourceError::Conflict(detail) => SourceError::Conflict(format!(
                    "its front door could not be registered: {detail}. A bridge registered by hand under the same bot name has to be removed first"
                )),
                other => other,
            });
        }
        // Its instances are tried again now, with the offering as it is now.
        self.clear_backoff(bridge_type, None);
        self.wake();
        if kind.mode == "shared" {
            self.request(bridge_type, SHARED_INSTANCE, None)
                .map_err(store_err)?;
        }
        self.offering_view(&row).await
    }

    async fn delete(&self, bridge_type: &str, remove_instances: bool) -> Result<(), SourceError> {
        if self
            .store
            .offering(bridge_type)
            .map_err(store_err)?
            .is_none()
        {
            return Err(SourceError::NotFound);
        }
        let instances = self.store.instances(Some(bridge_type)).map_err(store_err)?;
        if !instances.is_empty() && !remove_instances {
            return Err(SourceError::Conflict(format!(
                "{} people have this bridge; remove their instances first, or pass remove_instances=true",
                instances.len()
            )));
        }
        for row in &instances {
            self.stop(&row.bridge_type, &row.owner)
                .await
                .map_err(SourceError::Unavailable)?;
        }
        self.store.delete_offering(bridge_type).map_err(store_err)?;
        self.sync_registration().await?;
        Ok(())
    }

    async fn instances(&self, bridge_type: &str) -> Result<Vec<BridgeInstance>, SourceError> {
        if self
            .store
            .offering(bridge_type)
            .map_err(store_err)?
            .is_none()
        {
            return Err(SourceError::NotFound);
        }
        let mut out = Vec::new();
        for row in self.store.instances(Some(bridge_type)).map_err(store_err)? {
            out.push(self.instance_view(&row).await);
        }
        Ok(out)
    }

    async fn instance(
        &self,
        bridge_type: &str,
        user_id: &str,
    ) -> Result<Option<BridgeInstance>, SourceError> {
        match self
            .store
            .instance(bridge_type, user_id)
            .map_err(store_err)?
        {
            Some(row) => Ok(Some(self.instance_view(&row).await)),
            None => Ok(None),
        }
    }

    async fn put_instance(
        &self,
        bridge_type: &str,
        user_id: &str,
    ) -> Result<BridgeInstance, SourceError> {
        let offering = self
            .store
            .offering(bridge_type)
            .map_err(store_err)?
            .ok_or(SourceError::NotFound)?;
        let shared = mode(bridge_type) == "shared";
        if shared != (user_id == SHARED_INSTANCE) {
            return Err(SourceError::Invalid(if shared {
                format!("{bridge_type} is shared: its one instance is `{SHARED_INSTANCE}`")
            } else {
                format!("{bridge_type} is per user: name a local user")
            }));
        }
        if !shared && !self.is_local(user_id) {
            return Err(SourceError::Invalid(format!(
                "{user_id} is not a user of this server"
            )));
        }
        let _ = offering;
        let (row, _) = self
            .request(bridge_type, user_id, None)
            .map_err(store_err)?;
        Ok(self.instance_view(&row).await)
    }

    async fn delete_instance(&self, bridge_type: &str, user_id: &str) -> Result<(), SourceError> {
        tracing::debug!(
            bridge_type,
            user_id,
            "an administrator removes a bridge instance"
        );
        match self.stop(bridge_type, user_id).await {
            Ok(true) => Ok(()),
            Ok(false) => Err(SourceError::NotFound),
            Err(e) => Err(SourceError::Unavailable(e)),
        }
    }

    async fn instance_files(
        &self,
        bridge_type: &str,
        user_id: &str,
    ) -> Result<BridgeInstanceFiles, SourceError> {
        let offering = self
            .store
            .offering(bridge_type)
            .map_err(store_err)?
            .ok_or(SourceError::NotFound)?;
        let row = self
            .store
            .instance(bridge_type, user_id)
            .map_err(store_err)?
            .ok_or(SourceError::NotFound)?;
        let render = self.render(&row, &offering).ok_or_else(|| {
            SourceError::Invalid(
                "the instance is not registered yet; its files exist once it is".into(),
            )
        })?;
        let spec = Self::deploy_spec(&row, &render)
            .ok_or_else(|| SourceError::Invalid("the instance has no name yet".into()))?;
        let namespace = self
            .runtime
            .as_ref()
            .and_then(|r| r.target().namespace)
            .unwrap_or_else(|| "default".into());
        Ok(BridgeInstanceFiles {
            config_yaml: render.config_yaml.clone(),
            registration_yaml: render.registration_yaml.clone(),
            compose_yaml: render.compose_yaml.clone(),
            manifest_yaml: manifest_yaml(&spec, &namespace),
        })
    }
}

/// Whether an instance of `offering` may act as its owner: the type supports double puppeting
/// and the offering has it on (its default), which is the registration's non-exclusive claim on
/// the owner.
fn may_act_as_owner(kind: Option<&hs_admin::model::BridgeType>, offering: &OfferingRow) -> bool {
    kind.is_some_and(|k| k.supports_double_puppeting)
        && offering.options.double_puppeting.unwrap_or(true)
}

/// The catalogue's sign-in steps as the bot says them in its own chat: `{bot}` is "me", and
/// "Start a direct chat with me and send" reads oddly there, so it is just "Send".
fn sign_in_steps(kind: Option<&hs_admin::model::BridgeType>) -> Vec<String> {
    kind.map(|k| {
        k.sign_in
            .steps
            .iter()
            .map(|s| {
                s.replace("{bot}", "me")
                    .replace("Start a direct chat with me and send", "Send")
            })
            .collect()
    })
    .unwrap_or_default()
}

/// What the bot says in a chat it started once it has left and come back on the owner's
/// invitation: why it had been silent, and the steps again. `(text, html)`.
fn repaired_chat_notice(name: &str, steps: &[String]) -> (String, String) {
    let why = format!(
        "I started this chat myself, so I did not take what you typed here: a {name} bridge only takes commands in a chat you invited it to. I have left and come back on your invitation, so this chat is one now. To sign in:"
    );
    let mut text = format!("{why}\n");
    let mut html = format!("<p>{}</p><ol>", crate::front_door::inline_html(&why));
    for step in steps {
        text.push_str(&format!("- {step}\n"));
        html.push_str(&format!(
            "<li>{}</li>",
            crate::front_door::inline_html(step)
        ));
    }
    html.push_str("</ol>");
    (text, html)
}

/// What the bot says in a chat it started when nothing can make the bridge take bare commands
/// there (no double puppeting): use the prefix, or start a chat. `(text, html)`.
fn prefixed_commands_notice(name: &str, bot: &str, prefix: Option<&str>) -> (String, String) {
    let line = match prefix {
        Some(prefix) => format!(
            "I started this chat myself, so I only take commands here with my prefix: `{prefix} login qr` rather than `login qr`, and so on. For bare commands, invite me ({bot}) to a new direct chat: a {name} bridge takes them only in a chat you invited it to."
        ),
        None => format!(
            "I started this chat myself, so I only take commands here with my command prefix (`!` and the bridge's short name, as its documentation says). For bare commands, invite me ({bot}) to a new direct chat: a {name} bridge takes them only in a chat you invited it to."
        ),
    };
    let html = format!("<p>{}</p>", crate::front_door::inline_html(&line));
    (line, html)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::Mutex;

    use hs_admin::model::BridgeDeployment;
    use hs_admin::sources::InMemoryAppserviceDirectory;
    use hs_kv::memory::MemoryBackend;

    use super::*;

    fn is_dns_label(name: &str) -> bool {
        !name.is_empty()
            && name.len() <= 63
            && name
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
            && !name.starts_with('-')
            && !name.ends_with('-')
            && !name.contains("--")
    }

    #[test]
    fn a_plain_id_names_its_objects_after_the_type_and_owner() {
        assert_eq!(deploy_name("whatsapp-brandon"), "bridge-whatsapp-brandon");
        assert_eq!(deploy_name("whatsapp-alice-2"), "bridge-whatsapp-alice-2");
        assert_eq!(deploy_name("signal-bob42"), "bridge-signal-bob42");
    }

    #[test]
    fn a_shared_instance_is_named_after_the_type_alone() {
        assert_eq!(deploy_name("heisenbridge"), "bridge-heisenbridge");
        assert_eq!(deploy_name("hookshot"), "bridge-hookshot");
    }

    #[test]
    fn dots_and_uppercase_are_mapped_and_the_name_is_disambiguated() {
        let name = deploy_name("whatsapp-Alice.Smith");
        assert!(is_dns_label(&name), "{name}");
        assert!(name.starts_with("bridge-whatsapp-alice-smith-"), "{name}");
        let suffix = name.rsplit('-').next().unwrap();
        assert_eq!(suffix.len(), 6);
        assert!(suffix.chars().all(|c| c.is_ascii_hexdigit()));
        assert_eq!(deploy_name("whatsapp-Alice.Smith"), name, "stable");
        // The encoded form the manager really produces (`_` is `=5f`) maps the same way.
        let encoded = deploy_name("whatsapp-ali=5fce");
        assert!(
            encoded.starts_with("bridge-whatsapp-ali-5fce-"),
            "{encoded}"
        );
        assert!(is_dns_label(&encoded));
    }

    #[test]
    fn a_very_long_owner_is_cut_to_fit_a_pod_name_and_disambiguated() {
        let long = format!("whatsapp-{}", "a".repeat(100));
        let name = deploy_name(&long);
        assert!(is_dns_label(&name), "{name}");
        assert_eq!(name.len(), DEPLOY_NAME_MAX, "{name}");
        assert!(name.starts_with("bridge-whatsapp-aaaa"));
        assert_eq!(name.rsplit('-').next().unwrap().len(), 6);
        // Same prefix, different tail: different names.
        let other = deploy_name(&format!("whatsapp-{}b", "a".repeat(99)));
        assert_ne!(name, other);
        assert_eq!(&name[..name.len() - 6], &other[..other.len() - 6]);
        // A pod's name has room for the ReplicaSet and pod suffixes under 63.
        assert!(name.len() + "-6d4b7f9c8f".len() + "-x7k2p".len() <= 63);
        // An id of exactly the room needs no suffix; one over does.
        let room = DEPLOY_NAME_MAX - "bridge-".len();
        assert_eq!(
            deploy_name(&"a".repeat(room)),
            format!("bridge-{}", "a".repeat(room))
        );
        let over = deploy_name(&"a".repeat(room + 1));
        assert_eq!(over.len(), DEPLOY_NAME_MAX);
        assert_eq!(over.matches('-').count(), 2, "{over}");
    }

    #[test]
    fn two_owners_that_map_to_the_same_text_get_different_names() {
        let dotted = deploy_name("whatsapp-a.b");
        let dashed = deploy_name("whatsapp-a-b");
        assert_eq!(dashed, "bridge-whatsapp-a-b");
        assert_ne!(dotted, dashed);
        assert!(dotted.starts_with("bridge-whatsapp-a-b-"), "{dotted}");
        let upper = deploy_name("whatsapp-Alice");
        let lower = deploy_name("whatsapp-alice");
        assert_eq!(lower, "bridge-whatsapp-alice");
        assert_ne!(upper, lower);
        // Runs collapse and the ends are trimmed, with the suffix saying it happened.
        let odd = deploy_name("whatsapp-=5f=5fa=5f");
        assert!(is_dns_label(&odd), "{odd}");
        assert!(odd.starts_with("bridge-whatsapp-5f-5fa-5f-"), "{odd}");
        // An id with nothing to keep still gets a valid, stable name.
        let empty = deploy_name("===");
        assert!(is_dns_label(&empty), "{empty}");
        assert_eq!(empty.len(), "bridge-".len() + 6);
    }

    #[test]
    fn the_legacy_name_is_the_old_eight_hex_hash() {
        let legacy = legacy_deploy_name("whatsapp-alice");
        assert_eq!(legacy.len(), "bridge-".len() + 8);
        assert!(legacy.starts_with("bridge-"));
        assert_ne!(legacy, deploy_name("whatsapp-alice"));
        assert_eq!(legacy, legacy_deploy_name("whatsapp-alice"));
    }

    #[test]
    fn a_row_stored_before_names_were_stored_still_reads() {
        let json = serde_json::json!({
            "bridge_type": "mautrix-whatsapp",
            "owner": "@alice:example.org",
            "state": "registered",
            "reason": null,
            "appservice_id": "whatsapp-alice",
            "as_token": "a",
            "hs_token": "h",
            "url": "http://x:1",
            "front_door_room": null,
            "dm_room": null,
            "created_at_ms": 1,
            "state_since_ms": 1,
            "ready_at_ms": null
        });
        let row: InstanceRow = serde_json::from_value(json).unwrap();
        assert!(row.deploy_name.is_none());
        assert_eq!(row.appservice_id.as_deref(), Some("whatsapp-alice"));
    }

    /// A runtime that remembers what it was asked to apply and reports it ready at once.
    #[derive(Default)]
    struct FakeRuntime {
        objects: Mutex<BTreeMap<String, DeploySpec>>,
        /// Every `apply` asked for, refused or not.
        applies: Mutex<usize>,
        /// When set, `apply` is refused with this message and nothing changes: what a cluster
        /// whose `Bridge` CRD is older than the server answers.
        refuse: Mutex<Option<String>>,
        /// What `warning` says.
        warning: Mutex<Option<String>>,
        /// When set, every deployment is degraded with this message: a crash-looping bridge.
        crash: Mutex<Option<String>>,
    }

    impl FakeRuntime {
        fn names(&self) -> Vec<String> {
            self.objects.lock().unwrap().keys().cloned().collect()
        }

        fn spec(&self, name: &str) -> Option<DeploySpec> {
            self.objects.lock().unwrap().get(name).cloned()
        }

        /// An object that was there before the manager looked: what an instance deployed under
        /// the hashed name left behind.
        fn preexisting(&self, name: &str) {
            self.objects.lock().unwrap().insert(
                name.to_owned(),
                DeploySpec {
                    name: name.to_owned(),
                    labels: BTreeMap::new(),
                    bridge_type: "mautrix-whatsapp".into(),
                    appservice_id: "whatsapp-alice".into(),
                    owner: None,
                    image_repository: "x".into(),
                    image_tag: "y".into(),
                    port: 1,
                    args: Vec::new(),
                    files: BTreeMap::new(),
                    storage_size: None,
                },
            );
        }

        fn deployment(spec: &DeploySpec) -> BridgeDeployment {
            BridgeDeployment {
                namespace: "chat".into(),
                name: spec.name.clone(),
                image: format!("{}:{}", spec.image_repository, spec.image_tag),
                service_url: format!("http://{}.chat.svc:{}", spec.name, spec.port),
                phase: "Ready".into(),
                ready: true,
                message: None,
            }
        }
    }

    #[async_trait]
    impl Runtime for FakeRuntime {
        fn target(&self) -> BridgeDeploymentTarget {
            BridgeDeploymentTarget {
                available: true,
                namespace: Some("chat".into()),
                homeserver_url: Some("http://hs.chat.svc:8008".into()),
                reason: None,
            }
        }

        fn service_url(&self, name: &str, port: u16) -> String {
            format!("http://{name}.chat.svc:{port}")
        }

        async fn apply(&self, spec: &DeploySpec) -> Result<BridgeDeployment, String> {
            *self.applies.lock().unwrap() += 1;
            if let Some(refusal) = self.refuse.lock().unwrap().clone() {
                return Err(refusal);
            }
            self.objects
                .lock()
                .unwrap()
                .insert(spec.name.clone(), spec.clone());
            Ok(Self::deployment(spec))
        }

        async fn status(&self, name: &str) -> Result<Option<BridgeDeployment>, String> {
            let crash = self.crash.lock().unwrap().clone();
            Ok(self.spec(name).as_ref().map(|spec| {
                let mut d = Self::deployment(spec);
                if let Some(message) = crash {
                    d.phase = "Degraded".into();
                    d.ready = false;
                    d.message = Some(message);
                }
                d
            }))
        }

        async fn delete(&self, name: &str) -> Result<(), String> {
            self.objects.lock().unwrap().remove(name);
            Ok(())
        }

        fn warning(&self) -> Option<String> {
            self.warning.lock().unwrap().clone()
        }
    }

    #[test]
    fn a_failing_step_waits_twice_as_long_each_time_up_to_five_minutes() {
        assert_eq!(step_backoff(0), TICK);
        assert_eq!(step_backoff(1), TICK);
        assert_eq!(step_backoff(2), TICK * 2);
        assert_eq!(step_backoff(3), TICK * 4);
        assert_eq!(step_backoff(7), STEP_BACKOFF_MAX.min(TICK * 64));
        assert_eq!(step_backoff(8), STEP_BACKOFF_MAX);
        assert_eq!(step_backoff(u32::MAX), STEP_BACKOFF_MAX);
    }

    /// Makes `owner`'s instance's wait run out, as if the time had passed.
    fn wait_out(manager: &BridgeManager<MemoryBackend>, owner: &str) {
        if let Some(b) = manager
            .backoff
            .lock()
            .unwrap()
            .get_mut(&("mautrix-whatsapp".to_owned(), owner.to_owned()))
        {
            b.retry_at_ms = 0;
        }
    }

    fn streak(manager: &BridgeManager<MemoryBackend>, owner: &str) -> Option<Backoff> {
        manager
            .backoff
            .lock()
            .unwrap()
            .get(&("mautrix-whatsapp".to_owned(), owner.to_owned()))
            .copied()
    }

    /// The demo on 2026-10-09: the cluster's `Bridge` CRD predates `spec.owner`, so every apply
    /// of a changed deployment was refused, and the manager asked again (and logged that it
    /// was restarting the pod) every three seconds. A refused apply is now asked once, then
    /// again after a wait that doubles, and the instance says why.
    #[tokio::test]
    async fn a_refused_deployment_is_applied_once_and_then_backed_off() {
        const OWNER: &str = "@brandon:example.org";
        const REFUSAL: &str = "kubernetes API: ApiError: failed to create typed patch object \
            (myelin/bridge-whatsapp-brandon; hs.matrix.org/v1alpha1, Kind=Bridge): .spec.owner: \
            field not declared in schema";
        let (manager, runtime) = cluster_manager();
        manager.put("mautrix-whatsapp", cluster()).await.unwrap();
        manager
            .put_instance("mautrix-whatsapp", OWNER)
            .await
            .unwrap();
        manager.tick().await; // registered
        manager.tick().await; // deploying
        let row = manager
            .store
            .update_instance("mautrix-whatsapp", OWNER, |r| {
                r.enter(InstanceState::Ready, 5);
                r.reason = None;
                r.dm_room = Some("!chat:example.org".into());
                r.dm_started_by = Some(CHAT_BY_OWNER.into());
                true
            })
            .unwrap()
            .unwrap();
        let applied = row.applied_fingerprint.clone().unwrap();
        let applies_before = *runtime.applies.lock().unwrap();

        // The deployment changes, and the cluster refuses it.
        *runtime.refuse.lock().unwrap() = Some(REFUSAL.into());
        manager
            .put(
                "mautrix-whatsapp",
                BridgeOfferingRequest {
                    image_tag: Some("v0.13.0".into()),
                    ..BridgeOfferingRequest::default()
                },
            )
            .await
            .unwrap();
        for _ in 0..10 {
            manager.tick().await;
        }
        assert_eq!(*runtime.applies.lock().unwrap(), applies_before + 1);
        let row = manager
            .store
            .instance("mautrix-whatsapp", OWNER)
            .unwrap()
            .unwrap();
        assert_eq!(row.state, InstanceState::Ready, "nothing was applied");
        assert_eq!(row.applied_fingerprint.as_deref(), Some(applied.as_str()));
        assert_eq!(row.reason.as_deref(), Some(REFUSAL));
        assert_eq!(streak(&manager, OWNER).unwrap().failures, 1);

        // The wait runs out: asked once more, and the next wait is twice as long.
        wait_out(&manager, OWNER);
        manager.tick().await;
        manager.tick().await;
        assert_eq!(*runtime.applies.lock().unwrap(), applies_before + 2);
        let second = streak(&manager, OWNER).unwrap();
        assert_eq!(second.failures, 2);
        let wait_ms = second.retry_at_ms.saturating_sub(now_ms());
        assert!(
            wait_ms > 3_000 && wait_ms <= 6_000,
            "the second wait is about six seconds: {wait_ms} ms"
        );

        // The CRD is applied; the next try goes through, once, and the streak is forgotten.
        *runtime.refuse.lock().unwrap() = None;
        wait_out(&manager, OWNER);
        manager.tick().await;
        manager.tick().await;
        assert_eq!(*runtime.applies.lock().unwrap(), applies_before + 3);
        assert!(streak(&manager, OWNER).is_none());
        assert_eq!(
            runtime.spec("bridge-whatsapp-brandon").unwrap().image_tag,
            "v0.13.0"
        );
        let row = manager
            .store
            .instance("mautrix-whatsapp", OWNER)
            .unwrap()
            .unwrap();
        assert_ne!(row.applied_fingerprint.as_deref(), Some(applied.as_str()));
        assert_ne!(
            row.state,
            InstanceState::Ready,
            "it watches the pod come back"
        );
    }

    #[tokio::test]
    async fn asking_for_an_instance_again_or_changing_its_offering_ends_the_wait() {
        const OWNER: &str = "@brandon:example.org";
        let (manager, runtime) = cluster_manager();
        manager.put("mautrix-whatsapp", cluster()).await.unwrap();
        manager
            .put_instance("mautrix-whatsapp", OWNER)
            .await
            .unwrap();
        manager.tick().await; // registered
        *runtime.refuse.lock().unwrap() = Some("refused".into());
        manager.tick().await; // the apply is refused
        assert_eq!(streak(&manager, OWNER).unwrap().failures, 1);
        manager
            .put_instance("mautrix-whatsapp", OWNER)
            .await
            .unwrap();
        assert!(streak(&manager, OWNER).is_none());
        manager.tick().await;
        assert_eq!(streak(&manager, OWNER).unwrap().failures, 1);
        manager.put("mautrix-whatsapp", cluster()).await.unwrap();
        assert!(streak(&manager, OWNER).is_none());
    }

    /// The demo's WhatsApp bridge on 2026-10-09, after a roll gave it a pickle key other than
    /// its own: the operator carries its last log line into the status, and the instance says
    /// what it means and where the recovery is, while deploying and once ready alike.
    #[tokio::test]
    async fn a_bridge_that_cannot_read_its_crypto_store_says_so_with_the_recovery() {
        const OWNER: &str = "@brandon:example.org";
        let (manager, runtime) = cluster_manager();
        manager.put("mautrix-whatsapp", cluster()).await.unwrap();
        manager
            .put_instance("mautrix-whatsapp", OWNER)
            .await
            .unwrap();
        manager.tick().await; // registered
        manager.tick().await; // deploying
        *runtime.crash.lock().unwrap() = Some(
            "bridge: CrashLoopBackOff: back-off 5m0s; it last exited saying: FTL Failed to \
             start bridge error=\"failed to start Matrix connector: the supplied account key is \
             invalid\""
                .into(),
        );
        manager.tick().await;
        let row = row_of(&manager);
        assert_eq!(row.state, InstanceState::Deploying);
        let reason = row.reason.unwrap();
        assert!(
            reason.starts_with("the bridge cannot read its encryption store"),
            "{reason}"
        );
        assert!(reason.contains("docs/bridges/mautrix.md"), "{reason}");

        // Crashing after it was ready: the page says so too.
        manager
            .store
            .update_instance("mautrix-whatsapp", OWNER, |r| {
                r.enter(InstanceState::Ready, 5);
                r.reason = None;
                true
            })
            .unwrap();
        let view = manager
            .instance("mautrix-whatsapp", OWNER)
            .await
            .unwrap()
            .unwrap();
        assert!(
            view.reason
                .as_deref()
                .is_some_and(|r| r.starts_with("the bridge cannot read its encryption store")),
            "{:?}",
            view.reason
        );
        assert!(explain_deployment("bridge: CrashLoopBackOff").is_none());
    }

    #[tokio::test]
    async fn what_the_runtime_says_needs_fixing_is_on_each_deployed_instance() {
        const OWNER: &str = "@brandon:example.org";
        let (manager, runtime) = cluster_manager();
        manager.put("mautrix-whatsapp", cluster()).await.unwrap();
        manager
            .put_instance("mautrix-whatsapp", OWNER)
            .await
            .unwrap();
        manager.tick().await; // registered
        manager.tick().await; // deploying
        let note = "the Bridge CRD in the cluster is older than this server (it does not \
                    declare .spec.owner): apply deploy/crds/bridge.yaml";
        *runtime.warning.lock().unwrap() = Some(note.into());
        let view = manager
            .instance("mautrix-whatsapp", OWNER)
            .await
            .unwrap()
            .unwrap();
        let reason = view.reason.unwrap();
        assert!(reason.contains(note), "{reason}");
        assert!(reason.starts_with("waiting for the pod"), "{reason}");
        *runtime.warning.lock().unwrap() = None;
        let view = manager
            .instance("mautrix-whatsapp", OWNER)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(view.reason.as_deref(), Some("waiting for the pod"));
    }

    fn cluster_manager() -> (Arc<BridgeManager<MemoryBackend>>, Arc<FakeRuntime>) {
        let runtime = Arc::new(FakeRuntime::default());
        let manager = BridgeManager::new(
            MemoryBackend::new(),
            Arc::new(InMemoryAppserviceDirectory::new()),
            Some(runtime.clone()),
            "example.org",
            "https://example.org",
        )
        .unwrap();
        manager.attach("http://127.0.0.1:9");
        (manager, runtime)
    }

    fn cluster() -> BridgeOfferingRequest {
        BridgeOfferingRequest {
            runtime: Some("cluster".into()),
            ..BridgeOfferingRequest::default()
        }
    }

    #[tokio::test]
    async fn a_new_instance_is_deployed_under_a_readable_name_with_the_owner_labelled() {
        let (manager, runtime) = cluster_manager();
        manager.put("mautrix-whatsapp", cluster()).await.unwrap();
        manager
            .put_instance("mautrix-whatsapp", "@brandon:example.org")
            .await
            .unwrap();
        manager.tick().await; // requested -> registered: named and registered
        manager.tick().await; // registered -> deploying: applied
        assert_eq!(runtime.names(), vec!["bridge-whatsapp-brandon".to_owned()]);
        let spec = runtime.spec("bridge-whatsapp-brandon").unwrap();
        assert_eq!(spec.owner.as_deref(), Some("@brandon:example.org"));
        assert_eq!(spec.labels["myelin.dev/owner"], "brandon-example.org");
        assert_eq!(spec.labels["myelin.dev/bridge-type"], "mautrix-whatsapp");
        assert_eq!(spec.labels["myelin.dev/appservice-id"], "whatsapp-brandon");
        let row = manager
            .store
            .instance("mautrix-whatsapp", "@brandon:example.org")
            .unwrap()
            .unwrap();
        assert_eq!(row.deploy_name.as_deref(), Some("bridge-whatsapp-brandon"));
        assert_eq!(
            row.url.as_deref(),
            Some("http://bridge-whatsapp-brandon.chat.svc:29318"),
            "the registration's url is the Service's"
        );
        let instance = manager
            .instance("mautrix-whatsapp", "@brandon:example.org")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(instance.deployment.unwrap().name, "bridge-whatsapp-brandon");
        // The manifest for running it elsewhere names the owner too.
        let files = manager
            .instance_files("mautrix-whatsapp", "@brandon:example.org")
            .await
            .unwrap();
        assert!(
            files
                .manifest_yaml
                .contains("name: bridge-whatsapp-brandon\n")
        );
        assert!(
            files
                .manifest_yaml
                .contains("owner: \"@brandon:example.org\""),
            "{}",
            files.manifest_yaml
        );
    }

    #[tokio::test]
    async fn a_shared_instance_is_named_after_its_type_and_carries_no_owner() {
        let (manager, runtime) = cluster_manager();
        manager.put("heisenbridge", cluster()).await.unwrap();
        manager.tick().await;
        manager.tick().await;
        assert_eq!(runtime.names(), vec!["bridge-heisenbridge".to_owned()]);
        let spec = runtime.spec("bridge-heisenbridge").unwrap();
        assert!(spec.owner.is_none());
        assert!(!spec.labels.contains_key("myelin.dev/owner"));
    }

    #[tokio::test]
    async fn a_deployed_instance_is_named_on_whatsapps_side_and_its_config_is_complete() {
        let (manager, runtime) = cluster_manager();
        manager.put("mautrix-whatsapp", cluster()).await.unwrap();
        manager
            .put_instance("mautrix-whatsapp", "@brandon:example.org")
            .await
            .unwrap();
        manager.tick().await; // registered
        manager.tick().await; // deploying
        let row = manager
            .store
            .instance("mautrix-whatsapp", "@brandon:example.org")
            .unwrap()
            .unwrap();
        let pickle_key = row.pickle_key.clone().expect("minted with the tokens");
        assert_eq!(pickle_key.len(), 64);
        assert!(
            row.applied_fingerprint.is_some(),
            "what was applied is kept"
        );
        let spec = runtime.spec("bridge-whatsapp-brandon").unwrap();
        let config = &spec.files["config.yaml"];
        assert!(
            config.contains("  os_name: \"Myelin WhatsApp bridge for brandon (example.org)\"\n"),
            "{config}"
        );
        assert!(config.contains("  browser_name: DESKTOP\n"), "{config}");
        assert!(
            config.contains(&format!("  pickle_key: {pickle_key}\n")),
            "{config}"
        );
        // The admin API says the same name, so the interface can tell the person what to
        // expect in WhatsApp's Linked devices.
        let instance = manager
            .instance("mautrix-whatsapp", "@brandon:example.org")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            instance.device_name.as_deref(),
            Some("Myelin WhatsApp bridge for brandon (example.org)")
        );
    }

    #[tokio::test]
    async fn a_changed_offering_is_applied_to_a_ready_instance_once_and_rolls_it() {
        let (manager, runtime) = cluster_manager();
        manager.put("mautrix-whatsapp", cluster()).await.unwrap();
        manager
            .put_instance("mautrix-whatsapp", "@brandon:example.org")
            .await
            .unwrap();
        manager.tick().await; // registered
        manager.tick().await; // deploying
        let to_ready = |manager: &BridgeManager<MemoryBackend>| {
            manager
                .store
                .update_instance("mautrix-whatsapp", "@brandon:example.org", |r| {
                    r.enter(InstanceState::Ready, 5);
                    r.reason = None;
                    r.dm_room = Some("!chat:example.org".into());
                    r.dm_started_by = Some(CHAT_BY_OWNER.into());
                    true
                })
                .unwrap()
                .unwrap()
        };
        let row = to_ready(&manager);
        let applied = row.applied_fingerprint.clone().unwrap();

        // Nothing changed: a ready instance is left alone.
        manager.tick().await;
        let row = manager
            .store
            .instance("mautrix-whatsapp", "@brandon:example.org")
            .unwrap()
            .unwrap();
        assert_eq!(row.state, InstanceState::Ready);
        assert_eq!(row.applied_fingerprint.as_deref(), Some(applied.as_str()));

        // The administrator picks another image tag: the deployment is applied again with it
        // and the instance watches its pod come back.
        manager
            .put(
                "mautrix-whatsapp",
                BridgeOfferingRequest {
                    image_tag: Some("v0.13.0".into()),
                    ..BridgeOfferingRequest::default()
                },
            )
            .await
            .unwrap();
        manager.tick().await;
        let row = manager
            .store
            .instance("mautrix-whatsapp", "@brandon:example.org")
            .unwrap()
            .unwrap();
        assert_eq!(row.state, InstanceState::Deploying);
        assert_eq!(
            row.reason.as_deref(),
            Some("its configuration changed: restarting the pod with it")
        );
        assert_ne!(row.applied_fingerprint.as_deref(), Some(applied.as_str()));
        assert_eq!(
            runtime.spec("bridge-whatsapp-brandon").unwrap().image_tag,
            "v0.13.0"
        );
        // And it is not applied again for the same change: the deployment reports ready, so
        // the instance moves on to waiting for the bridge.
        manager.tick().await;
        let row = manager
            .store
            .instance("mautrix-whatsapp", "@brandon:example.org")
            .unwrap()
            .unwrap();
        assert_eq!(row.state, InstanceState::Starting);

        // A row from before fingerprints were kept is applied once, so that a config rendered
        // differently by a newer server (a device name, say) reaches its pod.
        let row = to_ready(&manager);
        let fresh = row.applied_fingerprint.clone().unwrap();
        manager
            .store
            .update_instance("mautrix-whatsapp", "@brandon:example.org", |r| {
                r.applied_fingerprint = None;
                true
            })
            .unwrap()
            .unwrap();
        manager.tick().await;
        let row = manager
            .store
            .instance("mautrix-whatsapp", "@brandon:example.org")
            .unwrap()
            .unwrap();
        assert_eq!(row.state, InstanceState::Deploying);
        assert_eq!(row.applied_fingerprint.as_deref(), Some(fresh.as_str()));
    }

    /// A cluster manager whose directory the test keeps, to read what was registered.
    fn cluster_manager_with_directory() -> (
        Arc<BridgeManager<MemoryBackend>>,
        Arc<FakeRuntime>,
        Arc<InMemoryAppserviceDirectory>,
    ) {
        let runtime = Arc::new(FakeRuntime::default());
        let directory = Arc::new(InMemoryAppserviceDirectory::new());
        let manager = BridgeManager::new(
            MemoryBackend::new(),
            directory.clone(),
            Some(runtime.clone()),
            "example.org",
            "https://example.org",
        )
        .unwrap();
        manager.attach("http://127.0.0.1:9");
        (manager, runtime, directory)
    }

    /// The `users` rules of `id`'s registration as the directory holds it.
    async fn users_claimed(directory: &InMemoryAppserviceDirectory, id: &str) -> Vec<Value> {
        directory.get(id).await.unwrap().unwrap().namespaces["users"]
            .as_array()
            .cloned()
            .unwrap_or_default()
    }

    fn claims_owner(users: &[Value]) -> bool {
        users
            .iter()
            .any(|u| u["regex"] == "@brandon:example\\.org" && u["exclusive"] == false)
    }

    /// Double puppeting is the owner's non-exclusive claim in the instance's registration.
    /// Switching it off on the offering patches the registry's copy once, and the changed
    /// files roll the pod, as any other change does; switching it back on does the same.
    #[tokio::test]
    async fn a_changed_double_puppeting_updates_the_instances_claim_once_and_rolls_it() {
        let (manager, _runtime, directory) = cluster_manager_with_directory();
        manager.put("mautrix-whatsapp", cluster()).await.unwrap();
        manager
            .put_instance("mautrix-whatsapp", "@brandon:example.org")
            .await
            .unwrap();
        manager.tick().await; // registered: the claim is on the server
        let users = users_claimed(&directory, "whatsapp-brandon").await;
        assert!(claims_owner(&users), "{users:?}");
        let row = manager
            .store
            .instance("mautrix-whatsapp", "@brandon:example.org")
            .unwrap()
            .unwrap();
        let registered = row.registered_fingerprint.clone().unwrap();
        manager.tick().await; // deploying
        manager
            .store
            .update_instance("mautrix-whatsapp", "@brandon:example.org", |r| {
                r.enter(InstanceState::Ready, 5);
                r.reason = None;
                r.dm_room = Some("!chat:example.org".into());
                r.dm_started_by = Some(CHAT_BY_OWNER.into());
                true
            })
            .unwrap()
            .unwrap();

        // Off: the registry no longer lets the bridge act as brandon, and the pod rolls.
        manager
            .put(
                "mautrix-whatsapp",
                BridgeOfferingRequest {
                    options: Some(hs_admin::model::BridgeOfferingOptions {
                        double_puppeting: Some(false),
                        ..Default::default()
                    }),
                    ..BridgeOfferingRequest::default()
                },
            )
            .await
            .unwrap();
        manager.tick().await;
        let users = users_claimed(&directory, "whatsapp-brandon").await;
        assert!(!claims_owner(&users), "{users:?}");
        let row = manager
            .store
            .instance("mautrix-whatsapp", "@brandon:example.org")
            .unwrap()
            .unwrap();
        assert_ne!(
            row.registered_fingerprint.as_deref(),
            Some(registered.as_str())
        );
        assert_eq!(row.state, InstanceState::Deploying);
        assert_eq!(
            row.reason.as_deref(),
            Some("its configuration changed: restarting the pod with it")
        );
        let after_off = row.registered_fingerprint.clone().unwrap();
        // Once: the next step finds nothing to patch and the instance moves on.
        manager.tick().await;
        let row = manager
            .store
            .instance("mautrix-whatsapp", "@brandon:example.org")
            .unwrap()
            .unwrap();
        assert_eq!(row.state, InstanceState::Starting);
        assert_eq!(
            row.registered_fingerprint.as_deref(),
            Some(after_off.as_str())
        );

        // On again: the claim is back.
        manager
            .put(
                "mautrix-whatsapp",
                BridgeOfferingRequest {
                    options: Some(hs_admin::model::BridgeOfferingOptions {
                        double_puppeting: Some(true),
                        ..Default::default()
                    }),
                    ..BridgeOfferingRequest::default()
                },
            )
            .await
            .unwrap();
        manager.tick().await;
        let users = users_claimed(&directory, "whatsapp-brandon").await;
        assert!(claims_owner(&users), "{users:?}");
        let row = manager
            .store
            .instance("mautrix-whatsapp", "@brandon:example.org")
            .unwrap()
            .unwrap();
        assert_eq!(
            row.registered_fingerprint.as_deref(),
            Some(registered.as_str())
        );
        assert_eq!(row.state, InstanceState::Deploying);
    }

    /// An instance run elsewhere has nobody to roll it: its registration is patched the same
    /// way and its reason says to fetch the files again. A row from before the fingerprint
    /// was kept is patched once with what it already claims, and nothing is said.
    #[tokio::test]
    async fn an_instance_run_elsewhere_is_told_to_fetch_its_files_when_its_claim_changes() {
        // Nobody has run the bridge yet: its pings fail and it waits in `starting`.
        let directory =
            Arc::new(InMemoryAppserviceDirectory::new().unreachable("whatsapp-brandon"));
        let manager = BridgeManager::new(
            MemoryBackend::new(),
            directory.clone(),
            None,
            "example.org",
            "https://example.org",
        )
        .unwrap();
        manager.attach("http://127.0.0.1:9");
        manager
            .put(
                "mautrix-whatsapp",
                BridgeOfferingRequest {
                    runtime: Some("elsewhere".into()),
                    ..BridgeOfferingRequest::default()
                },
            )
            .await
            .unwrap();
        manager
            .put_instance("mautrix-whatsapp", "@brandon:example.org")
            .await
            .unwrap();
        manager.tick().await; // registered
        manager.tick().await; // starting, waiting for someone to run it
        let row = manager
            .store
            .instance("mautrix-whatsapp", "@brandon:example.org")
            .unwrap()
            .unwrap();
        assert_eq!(row.state, InstanceState::Starting);
        let registered = row.registered_fingerprint.clone().unwrap();

        // From before: patched once, silently.
        manager
            .store
            .update_instance("mautrix-whatsapp", "@brandon:example.org", |r| {
                r.registered_fingerprint = None;
                true
            })
            .unwrap();
        manager.tick().await;
        let row = manager
            .store
            .instance("mautrix-whatsapp", "@brandon:example.org")
            .unwrap()
            .unwrap();
        assert_eq!(
            row.registered_fingerprint.as_deref(),
            Some(registered.as_str())
        );
        assert_ne!(row.reason.as_deref(), Some(REREGISTERED_REASON));

        manager
            .put(
                "mautrix-whatsapp",
                BridgeOfferingRequest {
                    options: Some(hs_admin::model::BridgeOfferingOptions {
                        double_puppeting: Some(false),
                        ..Default::default()
                    }),
                    ..BridgeOfferingRequest::default()
                },
            )
            .await
            .unwrap();
        manager.tick().await;
        let users = users_claimed(&directory, "whatsapp-brandon").await;
        assert!(!claims_owner(&users), "{users:?}");
        let row = manager
            .store
            .instance("mautrix-whatsapp", "@brandon:example.org")
            .unwrap()
            .unwrap();
        assert_eq!(row.reason.as_deref(), Some(REREGISTERED_REASON));
        assert_eq!(row.state, InstanceState::Starting);
    }

    /// The deployment's declared offerings (`MYELIN_BRIDGES_OFFERINGS`) are created the first
    /// time the manager runs with them, adopted when they exist already, and not created
    /// again once an administrator removes one.
    #[tokio::test]
    async fn declared_offerings_are_created_once_and_an_existing_one_is_adopted() {
        let (manager, _runtime) = cluster_manager();
        // The demo's case: the offering was made through the admin API before the chart
        // declared it, with its own image tag.
        manager
            .put(
                "mautrix-whatsapp",
                BridgeOfferingRequest {
                    runtime: Some("cluster".into()),
                    image_tag: Some("v0.12.0".into()),
                    ..BridgeOfferingRequest::default()
                },
            )
            .await
            .unwrap();
        manager.set_declared(vec![
            ("mautrix-whatsapp".into(), cluster()),
            ("mautrix-signal".into(), cluster()),
        ]);
        manager.apply_declared().await.unwrap();
        let whatsapp = manager.get("mautrix-whatsapp").await.unwrap().unwrap();
        assert_eq!(whatsapp.image_tag, "v0.12.0");
        let signal = manager.get("mautrix-signal").await.unwrap().unwrap();
        assert_eq!(signal.runtime, "cluster");
        assert_eq!(
            manager.tokens().unwrap().declared,
            vec!["mautrix-whatsapp".to_owned(), "mautrix-signal".to_owned()]
        );
        // Removed by an administrator: the declaration does not bring it back.
        manager.delete("mautrix-signal", false).await.unwrap();
        manager.apply_declared().await.unwrap();
        assert!(manager.get("mautrix-signal").await.unwrap().is_none());
        // One this server cannot create is left for the next start.
        let elsewhere_only = BridgeManager::new(
            MemoryBackend::new(),
            Arc::new(InMemoryAppserviceDirectory::new()),
            None,
            "example.org",
            "https://example.org",
        )
        .unwrap();
        elsewhere_only.attach("http://127.0.0.1:9");
        elsewhere_only.set_declared(vec![("mautrix-telegram".into(), cluster())]);
        elsewhere_only.apply_declared().await.unwrap();
        assert!(
            elsewhere_only
                .get("mautrix-telegram")
                .await
                .unwrap()
                .is_none()
        );
        assert!(elsewhere_only.tokens().unwrap().declared.is_empty());
    }

    /// The demo's shared registration from 2026-09-25 beside the offering that replaces it:
    /// the offering names it, its health names the offering and what to do, the instance's
    /// own registration is not mistaken for one, and removing it clears both.
    #[tokio::test]
    async fn a_bridge_registered_by_hand_is_named_on_its_health_and_on_the_offering() {
        let directory = Arc::new(InMemoryAppserviceDirectory::new().with_registration(json!({
            "id": "whatsapp",
            "url": "http://mautrix-whatsapp:29318",
            "as_token": "a",
            "hs_token": "h",
            "sender_localpart": "whatsappbot_shared",
            "namespaces": {
                "users": [
                    {"regex": "@whatsapp_.*:example\\.org", "exclusive": true},
                    {"regex": "@.*:example\\.org", "exclusive": false}
                ],
                "aliases": [], "rooms": []
            },
            "io.myelin.bridge_type": "mautrix-whatsapp"
        })));
        let runtime = Arc::new(FakeRuntime::default());
        let manager = BridgeManager::new(
            MemoryBackend::new(),
            directory.clone(),
            Some(runtime),
            "example.org",
            "https://example.org",
        )
        .unwrap();
        manager.attach("http://127.0.0.1:9");
        manager.put("mautrix-whatsapp", cluster()).await.unwrap();
        manager
            .put_instance("mautrix-whatsapp", "@brandon:example.org")
            .await
            .unwrap();
        manager.tick().await; // registered

        let offering = manager.get("mautrix-whatsapp").await.unwrap().unwrap();
        assert_eq!(offering.overlapping_appservices.len(), 1, "{offering:?}");
        let line = &offering.overlapping_appservices[0];
        assert_eq!(line.id, "whatsapp");
        assert_eq!(line.sender_localpart, "whatsappbot_shared");
        assert!(
            line.detail.contains("catalogue's WhatsApp entry"),
            "{}",
            line.detail
        );

        let aware =
            crate::directory::OfferingAwareDirectory::new(directory.clone(), manager.clone());
        let health = aware.health("whatsapp").await.unwrap();
        let overlap = health
            .overlaps_offering
            .expect("the hand-registered bridge is told");
        assert_eq!(overlap.bridge_type, "mautrix-whatsapp");
        assert_eq!(overlap.name, "WhatsApp");
        assert_eq!(
            overlap.front_door.as_deref(),
            Some("@whatsappbot:example.org")
        );
        assert!(
            overlap
                .detail
                .contains("messaging @whatsappbot:example.org"),
            "{}",
            overlap.detail
        );
        assert!(
            aware
                .health("whatsapp-brandon")
                .await
                .unwrap()
                .overlaps_offering
                .is_none()
        );
        assert!(
            aware
                .ping("whatsapp")
                .await
                .unwrap()
                .overlaps_offering
                .is_some()
        );

        directory.delete("whatsapp").await.unwrap();
        let offering = manager.get("mautrix-whatsapp").await.unwrap().unwrap();
        assert!(offering.overlapping_appservices.is_empty(), "{offering:?}");
    }

    #[test]
    fn a_deploy_fingerprint_changes_with_the_files_and_the_image_and_not_the_name() {
        let base = DeploySpec {
            name: "bridge-whatsapp-alice".into(),
            labels: BTreeMap::from([("a".to_owned(), "b".to_owned())]),
            bridge_type: "mautrix-whatsapp".into(),
            appservice_id: "whatsapp-alice".into(),
            owner: Some("@alice:example.org".into()),
            image_repository: "dock.mau.dev/mautrix/whatsapp".into(),
            image_tag: "latest".into(),
            port: 29318,
            args: Vec::new(),
            files: BTreeMap::from([("config.yaml".to_owned(), "os_name: a\n".to_owned())]),
            storage_size: Some("1Gi".into()),
        };
        let same = deploy_fingerprint(&base);
        assert_eq!(same.len(), 16);
        let renamed = DeploySpec {
            name: "bridge-other".into(),
            labels: BTreeMap::new(),
            ..base.clone()
        };
        assert_eq!(deploy_fingerprint(&renamed), same);
        let mut files = base.files.clone();
        files.insert("config.yaml".to_owned(), "os_name: b\n".to_owned());
        let changed = DeploySpec {
            files,
            ..base.clone()
        };
        assert_ne!(deploy_fingerprint(&changed), same);
        let retagged = DeploySpec {
            image_tag: "v0.13.0".into(),
            ..base.clone()
        };
        assert_ne!(deploy_fingerprint(&retagged), same);
        let with_args = DeploySpec {
            args: vec!["-o".into()],
            ..base
        };
        assert_ne!(deploy_fingerprint(&with_args), same);
    }

    /// A row from before names were stored, whose objects run under the hashed name.
    async fn registered_without_a_name(manager: &BridgeManager<MemoryBackend>) -> (String, String) {
        manager.put("mautrix-whatsapp", cluster()).await.unwrap();
        manager
            .put_instance("mautrix-whatsapp", "@alice:example.org")
            .await
            .unwrap();
        manager.tick().await; // registered, named
        let row = manager
            .store
            .update_instance("mautrix-whatsapp", "@alice:example.org", |r| {
                assert_eq!(r.state, InstanceState::Registered);
                r.deploy_name = None;
                true
            })
            .unwrap()
            .unwrap();
        let id = row.appservice_id.unwrap();
        (legacy_deploy_name(&id), deploy_name(&id))
    }

    #[tokio::test]
    async fn an_instance_already_running_under_the_hashed_name_keeps_it() {
        let (manager, runtime) = cluster_manager();
        let (legacy, readable) = registered_without_a_name(&manager).await;
        runtime.preexisting(&legacy);

        manager.tick().await; // adopts the name, applies under it
        let row = manager
            .store
            .instance("mautrix-whatsapp", "@alice:example.org")
            .unwrap()
            .unwrap();
        assert_eq!(row.deploy_name.as_deref(), Some(legacy.as_str()));
        assert_eq!(row.state, InstanceState::Deploying);
        assert_eq!(
            runtime.names(),
            vec![legacy.clone()],
            "no second deployment"
        );
        assert_ne!(legacy, readable);

        // Later ticks never rename it, and removing it deletes the adopted object.
        manager.tick().await;
        manager.tick().await;
        let row = manager
            .store
            .instance("mautrix-whatsapp", "@alice:example.org")
            .unwrap()
            .unwrap();
        assert_eq!(row.deploy_name.as_deref(), Some(legacy.as_str()));
        assert_eq!(runtime.names(), vec![legacy.clone()]);
        manager
            .delete_instance("mautrix-whatsapp", "@alice:example.org")
            .await
            .unwrap();
        manager.tick().await;
        assert!(runtime.names().is_empty(), "{:?}", runtime.names());
        assert!(
            manager
                .store
                .instance("mautrix-whatsapp", "@alice:example.org")
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn an_unnamed_instance_with_nothing_running_gets_the_readable_name() {
        let (manager, runtime) = cluster_manager();
        let (legacy, readable) = registered_without_a_name(&manager).await;
        manager.tick().await;
        let row = manager
            .store
            .instance("mautrix-whatsapp", "@alice:example.org")
            .unwrap()
            .unwrap();
        assert_eq!(row.deploy_name.as_deref(), Some(readable.as_str()));
        assert_eq!(readable, "bridge-whatsapp-alice");
        assert_eq!(runtime.names(), vec![readable.clone()]);
        assert!(runtime.spec(&legacy).is_none());
    }

    #[test]
    fn label_safe_makes_an_owner_a_label_value() {
        assert_eq!(label_safe("@alice:example.org"), "alice-example.org");
        assert_eq!(label_safe("@al_ice:ex.org"), "al_ice-ex.org");
    }

    // ---- settling and repairing the owner's chat ------------------------------------------

    /// This server's client API, as far as settling a chat needs it: who created the room, who
    /// is in it, and the bot's leave, the owner's invite, the bot's join and what the bot says,
    /// each remembered as `METHOD path as=user`.
    #[derive(Default)]
    struct FakeMatrix {
        creator: Mutex<String>,
        members: Mutex<Vec<String>>,
        bot_in_room: Mutex<bool>,
        join_fails: Mutex<bool>,
        calls: Mutex<Vec<String>>,
        notices: Mutex<Vec<String>>,
        /// What `/keys/query` answers: `device_keys` and `master_keys`/`self_signing_keys` by
        /// user, kept up to date by the two upload routes the way the real server would.
        keys: Mutex<serde_json::Value>,
        /// What `GET /devices` answers, by user: each device's `last_seen_ts`. `add_device`
        /// makes one seen now; `DELETE /devices/{id}` removes it and its keys.
        devices: Mutex<BTreeMap<String, BTreeMap<String, Option<u64>>>>,
    }

    impl FakeMatrix {
        fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }

        /// Gives `user` a device with keys (unsigned by any cross-signing key).
        fn add_device(&self, user: &str, device_id: &str) {
            self.keys.lock().unwrap()["device_keys"][user][device_id] = json!({
                "user_id": user,
                "device_id": device_id,
                "algorithms": ["m.olm.v1.curve25519-aes-sha2", "m.megolm.v1.aes-sha2"],
                "keys": {format!("curve25519:{device_id}"): "c", format!("ed25519:{device_id}"): "e"},
                "signatures": {user: {format!("ed25519:{device_id}"): "own"}},
            });
            self.devices
                .lock()
                .unwrap()
                .entry(user.to_owned())
                .or_default()
                .insert(device_id.to_owned(), Some(now_ms()));
        }

        /// Says `user`'s device was last seen at `ms`.
        fn seen_at(&self, user: &str, device_id: &str, ms: Option<u64>) {
            self.devices
                .lock()
                .unwrap()
                .entry(user.to_owned())
                .or_default()
                .insert(device_id.to_owned(), ms);
        }

        fn device_ids(&self, user: &str) -> Vec<String> {
            self.devices
                .lock()
                .unwrap()
                .get(user)
                .map(|d| d.keys().cloned().collect())
                .unwrap_or_default()
        }

        fn keys(&self) -> serde_json::Value {
            self.keys.lock().unwrap().clone()
        }

        fn take_calls(&self) -> Vec<String> {
            std::mem::take(&mut *self.calls.lock().unwrap())
        }

        fn notices(&self) -> Vec<String> {
            self.notices.lock().unwrap().clone()
        }
    }

    async fn fake_matrix(fake: Arc<FakeMatrix>) -> String {
        use axum::body::Bytes;
        use axum::http::{Method, StatusCode, Uri};
        async fn handle(
            axum::extract::State(fake): axum::extract::State<Arc<FakeMatrix>>,
            method: Method,
            uri: Uri,
            body: Bytes,
        ) -> (StatusCode, axum::Json<serde_json::Value>) {
            let path = uri
                .path()
                .trim_start_matches("/_matrix/client/v3")
                .to_owned();
            let as_user = reqwest::Url::parse(&format!("http://x{uri}"))
                .ok()
                .and_then(|u| {
                    u.query_pairs()
                        .find(|(k, _)| k == "user_id")
                        .map(|(_, v)| v.into_owned())
                })
                .unwrap_or_default();
            let body: serde_json::Value = serde_json::from_slice(&body).unwrap_or_default();
            let short = match path.split('/').collect::<Vec<_>>().as_slice() {
                ["", "rooms", _, "send", ..] => "send".to_owned(),
                ["", "rooms", _, what] => (*what).to_owned(),
                ["", "join", _] => "join".to_owned(),
                ["", "user", _, "account_data", _] => "m.direct".to_owned(),
                _ => path.trim_start_matches('/').to_owned(),
            };
            fake.calls
                .lock()
                .unwrap()
                .push(format!("{method} {short} as={as_user}"));
            let forbidden = (
                StatusCode::FORBIDDEN,
                axum::Json(json!({"errcode": "M_FORBIDDEN", "error": "no"})),
            );
            let ok = |v: serde_json::Value| (StatusCode::OK, axum::Json(v));
            match short.as_str() {
                "joined_members" => {
                    if !*fake.bot_in_room.lock().unwrap() {
                        return forbidden;
                    }
                    let joined: serde_json::Map<String, serde_json::Value> = fake
                        .members
                        .lock()
                        .unwrap()
                        .iter()
                        .map(|m| (m.clone(), json!({})))
                        .collect();
                    ok(json!({"joined": joined}))
                }
                "state" => ok(json!([
                    {"type": "m.room.create", "state_key": "", "sender": *fake.creator.lock().unwrap(), "content": {"room_version": "11"}},
                    {"type": "m.room.encryption", "state_key": "", "sender": *fake.creator.lock().unwrap(), "content": {}}
                ])),
                "leave" => {
                    *fake.bot_in_room.lock().unwrap() = false;
                    ok(json!({}))
                }
                "invite" => ok(json!({})),
                "join" => {
                    if *fake.join_fails.lock().unwrap() {
                        return forbidden;
                    }
                    *fake.bot_in_room.lock().unwrap() = true;
                    ok(json!({}))
                }
                "send" => {
                    fake.notices
                        .lock()
                        .unwrap()
                        .push(body["body"].as_str().unwrap_or_default().to_owned());
                    ok(json!({"event_id": "$e"}))
                }
                "createRoom" => {
                    *fake.bot_in_room.lock().unwrap() = true;
                    ok(json!({"room_id": "!new:example.org"}))
                }
                "m.direct" => ok(json!({})),
                "register" => ok(json!({"user_id": "@x:example.org"})),
                "keys/query" => ok(fake.keys()),
                "devices" => {
                    let devices: Vec<serde_json::Value> = fake
                        .devices
                        .lock()
                        .unwrap()
                        .get(&as_user)
                        .cloned()
                        .unwrap_or_default()
                        .into_iter()
                        .map(|(id, seen)| json!({"device_id": id, "last_seen_ts": seen}))
                        .collect();
                    ok(json!({"devices": devices}))
                }
                s if method == Method::DELETE && s.starts_with("devices/") => {
                    let id = s.trim_start_matches("devices/");
                    let gone = fake
                        .devices
                        .lock()
                        .unwrap()
                        .get_mut(&as_user)
                        .and_then(|d| d.remove(id))
                        .is_none();
                    if gone {
                        return (
                            StatusCode::NOT_FOUND,
                            axum::Json(json!({"errcode": "M_NOT_FOUND"})),
                        );
                    }
                    if let Some(keys) = fake.keys.lock().unwrap()["device_keys"]
                        .get_mut(&as_user)
                        .and_then(serde_json::Value::as_object_mut)
                    {
                        keys.remove(id);
                    }
                    ok(json!({}))
                }
                "keys/device_signing/upload" => {
                    let mut keys = fake.keys.lock().unwrap();
                    keys["master_keys"][as_user.clone()] = body["master_key"].clone();
                    keys["self_signing_keys"][as_user.clone()] = body["self_signing_key"].clone();
                    ok(json!({}))
                }
                "keys/signatures/upload" => {
                    let mut keys = fake.keys.lock().unwrap();
                    for (user, by_device) in body.as_object().cloned().unwrap_or_default() {
                        for (device, signed) in by_device.as_object().cloned().unwrap_or_default() {
                            keys["device_keys"][&user][&device]["signatures"] =
                                signed["signatures"].clone();
                        }
                    }
                    ok(json!({"failures": {}}))
                }
                _ => (
                    StatusCode::NOT_FOUND,
                    axum::Json(json!({"errcode": "M_UNRECOGNIZED"})),
                ),
            }
        }
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let app = axum::Router::new().fallback(handle).with_state(fake);
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        url
    }

    const OWNER: &str = "@brandon:example.org";
    const BOT: &str = "@whatsappbot_brandon:example.org";
    const OLD_CHAT: &str = "!old:example.org";

    /// A manager against the fake, with brandon's instance ready and its chat recorded the way
    /// every row from before 2026-10-02 is: the room, and nothing about who started it.
    async fn ready_with_an_unsettled_chat(
        fake: &Arc<FakeMatrix>,
        double_puppeting: Option<bool>,
    ) -> Arc<BridgeManager<MemoryBackend>> {
        let runtime = Arc::new(FakeRuntime::default());
        let manager = BridgeManager::new(
            MemoryBackend::new(),
            Arc::new(InMemoryAppserviceDirectory::new()),
            Some(runtime),
            "example.org",
            "https://example.org",
        )
        .unwrap();
        manager.attach(&fake_matrix(fake.clone()).await);
        manager
            .put(
                "mautrix-whatsapp",
                BridgeOfferingRequest {
                    runtime: Some("cluster".into()),
                    options: Some(hs_admin::model::BridgeOfferingOptions {
                        double_puppeting,
                        ..Default::default()
                    }),
                    ..BridgeOfferingRequest::default()
                },
            )
            .await
            .unwrap();
        manager
            .put_instance("mautrix-whatsapp", OWNER)
            .await
            .unwrap();
        manager.tick().await; // requested -> registered: tokens and a name
        manager.tick().await; // registered -> deploying: its deployment applied
        manager
            .store
            .update_instance("mautrix-whatsapp", OWNER, |r| {
                assert_eq!(r.state, InstanceState::Deploying);
                assert!(r.applied_fingerprint.is_some());
                r.enter(InstanceState::Ready, 2);
                r.reason = None;
                r.dm_room = Some(OLD_CHAT.into());
                r.dm_started_by = None;
                // The bot's identity settled already, and looked at just now, so that the
                // chat stories below see the chat's calls only.
                let seeds = Seeds::generate();
                r.cross_signing_master_seed = Some(seeds.master);
                r.cross_signing_self_signing_seed = Some(seeds.self_signing);
                r.signed_bot_device = Some("DEVICE".into());
                true
            })
            .unwrap()
            .unwrap();
        manager
            .identity_checked_ms
            .lock()
            .unwrap()
            .insert(("mautrix-whatsapp".into(), OWNER.into()), now_ms());
        fake.take_calls(); // the front doors' registration and names
        manager
    }

    fn row_of(manager: &BridgeManager<MemoryBackend>) -> InstanceRow {
        manager
            .store
            .instance("mautrix-whatsapp", OWNER)
            .unwrap()
            .unwrap()
    }

    /// Brandon's instance ready with its chat settled and its bot's identity not: the row a
    /// server from before 2026-10-03 has, or a fresh instance the moment it became ready.
    async fn ready_with_an_unsigned_bot(
        fake: &Arc<FakeMatrix>,
    ) -> Arc<BridgeManager<MemoryBackend>> {
        let manager = ready_with_an_unsettled_chat(fake, None).await;
        manager
            .store
            .update_instance("mautrix-whatsapp", OWNER, |r| {
                r.dm_started_by = Some(CHAT_BY_OWNER.into());
                r.cross_signing_master_seed = None;
                r.cross_signing_self_signing_seed = None;
                r.signed_bot_device = None;
                true
            })
            .unwrap()
            .unwrap();
        manager.identity_checked_ms.lock().unwrap().clear();
        manager
    }

    #[tokio::test]
    async fn a_ready_instances_bot_gets_a_cross_signing_identity_and_its_device_signed() {
        let fake = Arc::new(FakeMatrix::default());
        fake.add_device(BOT, "IEXNEKZESJ");
        let manager = ready_with_an_unsigned_bot(&fake).await;

        manager.tick().await;
        assert_eq!(
            fake.take_calls(),
            vec![
                format!("POST keys/query as={BOT}"),
                format!("POST keys/device_signing/upload as={BOT}"),
                format!("POST keys/signatures/upload as={BOT}"),
                format!("GET devices as={BOT}"),
            ]
        );
        let row = row_of(&manager);
        let seeds = Seeds {
            master: row
                .cross_signing_master_seed
                .clone()
                .expect("a master seed"),
            self_signing: row
                .cross_signing_self_signing_seed
                .clone()
                .expect("a self-signing seed"),
        };
        let identity = BotIdentity::from_seeds(BOT, &seeds).unwrap();
        let keys = fake.keys();
        assert!(identity.is_published_master(keys["master_keys"].get(BOT)));
        assert_eq!(
            keys["self_signing_keys"][BOT]["usage"],
            json!(["self_signing"])
        );
        assert!(
            keys["self_signing_keys"][BOT]["signatures"][BOT][identity.master_key_id()].is_string(),
            "the self-signing key is signed by the master key"
        );
        assert!(identity.has_signed_device(&keys["device_keys"][BOT]["IEXNEKZESJ"]));
        assert_eq!(
            keys["device_keys"][BOT]["IEXNEKZESJ"]["signatures"][BOT]["ed25519:IEXNEKZESJ"], "own",
            "the device's own signature is kept"
        );
        assert_eq!(row.signed_bot_device.as_deref(), Some("IEXNEKZESJ"));
        assert!(row.reason.is_none(), "{:?}", row.reason);
        let view = manager
            .instance("mautrix-whatsapp", OWNER)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(view.signed_bot_device.as_deref(), Some("IEXNEKZESJ"));

        // Settled: the next steps do not ask again (for a minute).
        manager.tick().await;
        manager.tick().await;
        assert!(fake.calls().is_empty(), "{:?}", fake.calls());

        // A device the bridge makes later (a reset database) is signed on the next look,
        // with the same identity: no second upload of the keys.
        fake.add_device(BOT, "NEWDEVICE1");
        manager.identity_checked_ms.lock().unwrap().clear();
        manager.tick().await;
        assert_eq!(
            fake.take_calls(),
            vec![
                format!("POST keys/query as={BOT}"),
                format!("POST keys/signatures/upload as={BOT}"),
                format!("GET devices as={BOT}"),
            ]
        );
        let keys = fake.keys();
        assert!(identity.has_signed_device(&keys["device_keys"][BOT]["NEWDEVICE1"]));
        assert!(identity.has_signed_device(&keys["device_keys"][BOT]["IEXNEKZESJ"]));
    }

    #[tokio::test]
    async fn an_instance_recreated_for_the_same_owner_replaces_the_bots_old_keys() {
        let fake = Arc::new(FakeMatrix::default());
        // The server still has the previous instance's identity for the same bot, and its
        // device signed by it.
        let old = BotIdentity::from_seeds(BOT, &Seeds::generate()).unwrap();
        let old_body = old.upload_body().unwrap();
        fake.keys.lock().unwrap()["master_keys"][BOT] = old_body["master_key"].clone();
        fake.keys.lock().unwrap()["self_signing_keys"][BOT] = old_body["self_signing_key"].clone();
        fake.add_device(BOT, "OLDDEVICE1");
        let old_device = fake.keys()["device_keys"][BOT]["OLDDEVICE1"].clone();
        let signed = old.sign_device("OLDDEVICE1", &old_device).unwrap();
        fake.keys.lock().unwrap()["device_keys"][BOT]["OLDDEVICE1"] =
            signed[BOT]["OLDDEVICE1"].clone();
        let manager = ready_with_an_unsigned_bot(&fake).await;

        manager.tick().await;
        assert_eq!(
            fake.take_calls(),
            vec![
                format!("POST keys/query as={BOT}"),
                format!("POST keys/device_signing/upload as={BOT}"),
                format!("POST keys/signatures/upload as={BOT}"),
                format!("GET devices as={BOT}"),
            ],
            "the new identity replaces the old one (no user-interactive auth for an appservice), and the device is signed by it"
        );
        let row = row_of(&manager);
        let seeds = Seeds {
            master: row.cross_signing_master_seed.clone().unwrap(),
            self_signing: row.cross_signing_self_signing_seed.clone().unwrap(),
        };
        let new = BotIdentity::from_seeds(BOT, &seeds).unwrap();
        let keys = fake.keys();
        assert!(new.is_published_master(keys["master_keys"].get(BOT)));
        assert!(!old.is_published_master(keys["master_keys"].get(BOT)));
        assert!(new.has_signed_device(&keys["device_keys"][BOT]["OLDDEVICE1"]));
        assert_eq!(row.signed_bot_device.as_deref(), Some("OLDDEVICE1"));
    }

    #[tokio::test]
    async fn a_bot_without_a_device_yet_gets_its_keys_and_is_looked_at_again_each_step() {
        let fake = Arc::new(FakeMatrix::default());
        let manager = ready_with_an_unsigned_bot(&fake).await;

        manager.tick().await;
        assert_eq!(
            fake.take_calls(),
            vec![
                format!("POST keys/query as={BOT}"),
                format!("POST keys/device_signing/upload as={BOT}"),
            ]
        );
        assert!(row_of(&manager).signed_bot_device.is_none());
        // Not settled: asked again next step, and the keys (already there) not re-uploaded.
        manager.tick().await;
        assert_eq!(fake.take_calls(), vec![format!("POST keys/query as={BOT}")]);
        // The bridge starts and makes its device: signed.
        fake.add_device(BOT, "LATEDEVICE");
        manager.tick().await;
        assert_eq!(
            fake.take_calls(),
            vec![
                format!("POST keys/query as={BOT}"),
                format!("POST keys/signatures/upload as={BOT}"),
                format!("GET devices as={BOT}"),
            ]
        );
        assert_eq!(
            row_of(&manager).signed_bot_device.as_deref(),
            Some("LATEDEVICE")
        );
    }

    const DAY_MS: u64 = 24 * 60 * 60 * 1000;

    /// The demo on 2026-10-10: the WhatsApp bridge's crypto reset of 2026-10-09 made the bot a
    /// new device `BQBMQVR81T`, and its device from before, `BSLXZIVKIV`, stayed registered;
    /// clients went on encrypting room keys to it and withholding keys from it. The manager
    /// removes it, says so in the log and on the instance, and names the device in use.
    #[tokio::test]
    async fn a_device_the_bridge_left_behind_after_a_reset_is_removed_and_the_one_in_use_kept() {
        let fake = Arc::new(FakeMatrix::default());
        let now = now_ms();
        fake.add_device(BOT, "BSLXZIVKIV");
        fake.seen_at(BOT, "BSLXZIVKIV", Some(now - 8 * DAY_MS));
        fake.add_device(BOT, "BQBMQVR81T");
        fake.seen_at(BOT, "BQBMQVR81T", Some(now - DAY_MS));
        let manager = ready_with_an_unsigned_bot(&fake).await;
        let appservice_id = row_of(&manager).appservice_id.expect("an appservice id");
        let log = LogSink::capture();

        manager.tick().await;
        let calls = fake.take_calls();
        assert_eq!(
            calls
                .iter()
                .filter(|c| c.starts_with("POST keys/signatures/upload"))
                .count(),
            2,
            "both devices signed first: {calls:?}"
        );
        assert_eq!(
            calls[calls.len() - 2..],
            [
                format!("GET devices as={BOT}"),
                format!("DELETE devices/BSLXZIVKIV as={BOT}"),
            ],
            "the old device removed as the bot, through the instance's token"
        );
        assert_eq!(fake.device_ids(BOT), vec!["BQBMQVR81T".to_owned()]);
        assert!(fake.keys()["device_keys"][BOT].get("BSLXZIVKIV").is_none());

        let row = row_of(&manager);
        assert_eq!(row.signed_bot_device.as_deref(), Some("BQBMQVR81T"));
        assert!(row.reason.is_none(), "{:?}", row.reason);
        assert_eq!(row.removed_bot_devices.len(), 1);
        assert_eq!(row.removed_bot_devices[0].device_id, "BSLXZIVKIV");
        assert_eq!(row.removed_bot_devices[0].kept_device, "BQBMQVR81T");
        assert_eq!(
            row.removed_bot_devices[0].last_seen_ms,
            Some(now - 8 * DAY_MS)
        );
        let view = manager
            .instance("mautrix-whatsapp", OWNER)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(view.signed_bot_device.as_deref(), Some("BQBMQVR81T"));
        assert_eq!(view.removed_bot_devices.len(), 1);
        assert_eq!(view.removed_bot_devices[0].device_id, "BSLXZIVKIV");
        assert_eq!(view.removed_bot_devices[0].kept_device, "BQBMQVR81T");
        assert!(view.removed_bot_devices[0].last_seen_at.is_some());

        let removed = log.lines_with("removed a device the bridge bot no longer uses");
        assert_eq!(removed.len(), 1, "{removed:?}");
        for expected in [
            "INFO".to_owned(),
            format!("appservice={appservice_id}"),
            format!("bot={BOT}"),
            "device=BSLXZIVKIV".to_owned(),
            "kept_device=BQBMQVR81T".to_owned(),
            "bridge_type=mautrix-whatsapp".to_owned(),
            format!("owner={OWNER}"),
        ] {
            assert!(
                removed[0].contains(&expected),
                "{expected} in {}",
                removed[0]
            );
        }

        // The next look finds nothing left behind: no removal, no line.
        manager.identity_checked_ms.lock().unwrap().clear();
        manager.tick().await;
        assert_eq!(
            fake.take_calls(),
            vec![
                format!("POST keys/query as={BOT}"),
                format!("GET devices as={BOT}"),
            ]
        );
        assert_eq!(
            log.lines_with("removed a device the bridge bot no longer uses")
                .len(),
            1
        );
        assert_eq!(row_of(&manager).removed_bot_devices.len(), 1);
    }

    /// A device seen within the day is kept even when a newer one is in use (the bridge may
    /// still be on it), and removed once it has gone a day unseen; a device the server has no
    /// time for is never removed, nor the device in use however long the bridge was quiet.
    #[tokio::test]
    async fn a_bots_older_device_is_kept_until_a_day_unseen_and_the_device_in_use_never_removed() {
        let fake = Arc::new(FakeMatrix::default());
        let now = now_ms();
        // The device in use, quiet for a month.
        fake.add_device(BOT, "INUSE00001");
        fake.seen_at(BOT, "INUSE00001", Some(now - 30 * DAY_MS));
        // An older one, seen nearly a day before.
        fake.add_device(BOT, "OLDER00001");
        fake.seen_at(BOT, "OLDER00001", Some(now - 30 * DAY_MS - DAY_MS + 60_000));
        // One the server has no time for.
        fake.add_device(BOT, "NOTIME0001");
        fake.seen_at(BOT, "NOTIME0001", None);
        let manager = ready_with_an_unsigned_bot(&fake).await;

        manager.tick().await;
        // Both older ones are more than a day old: OLDER00001 goes, NOTIME0001 stays.
        let calls = fake.take_calls();
        assert!(
            calls.contains(&format!("DELETE devices/OLDER00001 as={BOT}")),
            "{calls:?}"
        );
        assert!(
            !calls
                .iter()
                .any(|c| c.contains("NOTIME0001") && c.starts_with("DELETE"))
        );
        assert_eq!(
            fake.device_ids(BOT),
            vec!["INUSE00001".to_owned(), "NOTIME0001".to_owned()]
        );
        assert_eq!(
            row_of(&manager).signed_bot_device.as_deref(),
            Some("INUSE00001")
        );

        // A reset now: the new device is in use, the old one was seen just now (within the
        // day), so it stays.
        fake.seen_at(BOT, "INUSE00001", Some(now - 2 * 60 * 60 * 1000));
        fake.add_device(BOT, "NEWDEVICE1");
        manager.identity_checked_ms.lock().unwrap().clear();
        manager.tick().await;
        assert!(
            !fake.take_calls().iter().any(|c| c.starts_with("DELETE")),
            "a device seen within the day is kept"
        );
        assert_eq!(
            row_of(&manager).signed_bot_device.as_deref(),
            Some("NEWDEVICE1")
        );
        // A day later (as the server's times say): removed.
        fake.seen_at(BOT, "INUSE00001", Some(now - DAY_MS - 1));
        manager.identity_checked_ms.lock().unwrap().clear();
        manager.tick().await;
        assert!(
            fake.take_calls()
                .contains(&format!("DELETE devices/INUSE00001 as={BOT}"))
        );
        assert_eq!(
            fake.device_ids(BOT),
            vec!["NEWDEVICE1".to_owned(), "NOTIME0001".to_owned()]
        );
        let removed: Vec<String> = row_of(&manager)
            .removed_bot_devices
            .iter()
            .map(|d| d.device_id.clone())
            .collect();
        assert_eq!(removed, vec!["OLDER00001", "INUSE00001"]);
    }

    #[test]
    fn the_device_in_use_is_the_one_with_keys_seen_last_and_the_rest_wait_a_day() {
        let now = 100 * DAY_MS;
        let dev = |id: &str, seen: Option<u64>| (id.to_owned(), seen);

        // One device, however old: in use, nothing removed.
        assert_eq!(
            bot_devices_plan(&[dev("A", Some(1))], &["A"], now),
            Some(("A".to_owned(), vec![]))
        );
        // No device with keys, or none with a time: nothing in use, nothing removed.
        assert_eq!(bot_devices_plan(&[dev("A", Some(1))], &[], now), None);
        assert_eq!(bot_devices_plan(&[dev("A", None)], &["A"], now), None);
        // Two seen at the same moment: cannot be told apart.
        assert_eq!(
            bot_devices_plan(&[dev("A", Some(5)), dev("B", Some(5))], &["A", "B"], now),
            None
        );
        // A newer device without keys does not count as in use, and is not removed.
        assert_eq!(
            bot_devices_plan(
                &[dev("OLD", Some(now - 9 * DAY_MS)), dev("NEW", Some(now))],
                &["OLD"],
                now
            ),
            Some(("OLD".to_owned(), vec![]))
        );
        // An older device without keys (made and abandoned) is removed once a day unseen.
        assert_eq!(
            bot_devices_plan(
                &[dev("BARE", Some(now - 2 * DAY_MS)), dev("NEW", Some(now))],
                &["NEW"],
                now
            ),
            Some((
                "NEW".to_owned(),
                vec![("BARE".to_owned(), now - 2 * DAY_MS)]
            ))
        );
        // Exactly a day unseen is enough; a millisecond less is not.
        assert_eq!(
            bot_devices_plan(
                &[
                    dev("A", Some(now - DAY_MS)),
                    dev("B", Some(now - DAY_MS + 1)),
                    dev("C", Some(now))
                ],
                &["A", "B", "C"],
                now
            ),
            Some(("C".to_owned(), vec![("A".to_owned(), now - DAY_MS)]))
        );
    }

    thread_local! {
        /// The buffer the test on this thread captures the log into, if one does.
        static LOG_BUFFER: std::cell::RefCell<Option<Arc<Mutex<Vec<u8>>>>> =
            const { std::cell::RefCell::new(None) };
    }

    /// The writer of this binary's one global `tracing` subscriber: a line goes to the buffer
    /// of the test on the thread that logged it (`LogSink::capture`), and nowhere otherwise.
    /// One global subscriber rather than one per test: `tracing` caches each callsite's
    /// interest from whichever thread hits it first, so a subscriber set on a test's thread
    /// alone sees nothing once a parallel test has hit the callsite without one.
    #[derive(Clone, Copy)]
    struct ThreadLog;

    impl std::io::Write for ThreadLog {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            LOG_BUFFER.with(|b| {
                if let Some(buffer) = b.borrow().as_ref() {
                    buffer.lock().unwrap().extend_from_slice(buf);
                }
            });
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for ThreadLog {
        type Writer = ThreadLog;

        fn make_writer(&'a self) -> ThreadLog {
            *self
        }
    }

    /// What the manager logged at `INFO` and above on this thread (a `#[tokio::test]` runs its
    /// steps on it) while the test holds this, one line per event, without time or target.
    struct LogSink(Arc<Mutex<Vec<u8>>>);

    impl LogSink {
        fn capture() -> LogSink {
            static INSTALLED: std::sync::OnceLock<()> = std::sync::OnceLock::new();
            INSTALLED.get_or_init(|| {
                let subscriber = tracing_subscriber::fmt()
                    .with_writer(ThreadLog)
                    .with_ansi(false)
                    .with_target(false)
                    .without_time()
                    .with_max_level(tracing::Level::INFO)
                    .finish();
                // Refused only if a test set another global subscriber first: none does.
                let _ = tracing::subscriber::set_global_default(subscriber);
            });
            let buffer = Arc::new(Mutex::new(Vec::new()));
            LOG_BUFFER.with(|b| *b.borrow_mut() = Some(buffer.clone()));
            LogSink(buffer)
        }

        /// The lines logged so far that contain `needle`.
        fn lines_with(&self, needle: &str) -> Vec<String> {
            String::from_utf8_lossy(&self.0.lock().unwrap())
                .lines()
                .filter(|l| l.contains(needle))
                .map(str::to_owned)
                .collect()
        }
    }

    impl Drop for LogSink {
        fn drop(&mut self) {
            LOG_BUFFER.with(|b| *b.borrow_mut() = None);
        }
    }

    /// The owner recovered a bridge on 2026-10-09 by resetting its crypto store, which made a
    /// new bot device, and had no log line saying the manager had signed it: there is one now,
    /// naming the appservice, the bot and the device, once per signature and never for a look
    /// that signs nothing; and the instance names the device signed just now, not the old one
    /// the server still lists.
    #[tokio::test]
    async fn signing_a_bot_device_is_logged_once_with_the_appservice_bot_and_device() {
        let fake = Arc::new(FakeMatrix::default());
        fake.add_device(BOT, "IEXNEKZESJ");
        let manager = ready_with_an_unsigned_bot(&fake).await;
        let appservice_id = row_of(&manager).appservice_id.expect("an appservice id");
        let log = LogSink::capture();

        manager.tick().await;
        let signed = log.lines_with("cross-signed the bridge bot's device");
        assert_eq!(signed.len(), 1, "{signed:?}");
        for expected in [
            format!("appservice={appservice_id}"),
            format!("bot={BOT}"),
            "device=IEXNEKZESJ".to_owned(),
            "first_signing=true".to_owned(),
            "bridge_type=mautrix-whatsapp".to_owned(),
            format!("owner={OWNER}"),
        ] {
            assert!(signed[0].contains(&expected), "{expected} in {}", signed[0]);
        }
        assert!(!signed[0].contains("previous_device"), "{}", signed[0]);

        // Looks that sign nothing log nothing, whether skipped (settled) or made (a recheck).
        manager.tick().await;
        manager.identity_checked_ms.lock().unwrap().clear();
        manager.tick().await;
        assert_eq!(
            fake.take_calls(),
            vec![
                format!("POST keys/query as={BOT}"),
                format!("POST keys/device_signing/upload as={BOT}"),
                format!("POST keys/signatures/upload as={BOT}"),
                format!("GET devices as={BOT}"),
                format!("POST keys/query as={BOT}"),
                format!("GET devices as={BOT}"),
            ]
        );
        assert_eq!(
            log.lines_with("cross-signed the bridge bot's device").len(),
            1,
            "a look that signs nothing logs nothing"
        );

        // The bridge's crypto store is reset: a new device, which the server happens to list
        // before the old (still signed) one.
        {
            let mut keys = fake.keys.lock().unwrap();
            let devices = keys["device_keys"][BOT].as_object_mut().unwrap();
            let old = devices.remove("IEXNEKZESJ").unwrap();
            drop(keys);
            fake.add_device(BOT, "BQBMQVR81T");
            fake.keys.lock().unwrap()["device_keys"][BOT]["IEXNEKZESJ"] = old;
        }
        manager.identity_checked_ms.lock().unwrap().clear();
        manager.tick().await;
        let signed = log.lines_with("cross-signed the bridge bot's device");
        assert_eq!(signed.len(), 2, "{signed:?}");
        for expected in [
            format!("appservice={appservice_id}"),
            "device=BQBMQVR81T".to_owned(),
            "first_signing=false".to_owned(),
            "previous_device=IEXNEKZESJ".to_owned(),
        ] {
            assert!(signed[1].contains(&expected), "{expected} in {}", signed[1]);
        }
        assert_eq!(
            row_of(&manager).signed_bot_device.as_deref(),
            Some("BQBMQVR81T"),
            "the instance names the device signed just now, not the old one listed after it"
        );
        let view = manager
            .instance("mautrix-whatsapp", OWNER)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(view.signed_bot_device.as_deref(), Some("BQBMQVR81T"));
    }

    #[tokio::test]
    async fn a_chat_the_bot_started_is_repaired_by_a_leave_and_the_owners_invitation() {
        let fake = Arc::new(FakeMatrix::default());
        *fake.creator.lock().unwrap() = BOT.into();
        *fake.members.lock().unwrap() = vec![BOT.into(), OWNER.into()];
        *fake.bot_in_room.lock().unwrap() = true;
        let manager = ready_with_an_unsettled_chat(&fake, None).await;

        manager.tick().await;
        assert_eq!(
            fake.take_calls(),
            vec![
                format!("GET joined_members as={BOT}"),
                format!("GET state as={BOT}"),
                format!("POST leave as={BOT}"),
                format!("POST invite as={OWNER}"),
                format!("POST join as={BOT}"),
                format!("PUT send as={BOT}"),
            ]
        );
        let said = fake.notices().join("\n");
        assert!(said.contains("I started this chat myself"), "{said}");
        assert!(said.contains("come back on your invitation"), "{said}");
        assert!(said.contains("login qr"), "{said}");
        let row = row_of(&manager);
        assert_eq!(row.dm_room.as_deref(), Some(OLD_CHAT), "the same chat");
        assert_eq!(row.dm_started_by.as_deref(), Some(CHAT_BY_OWNER));
        assert!(row.reason.is_none(), "{:?}", row.reason);
        let view = manager
            .instance("mautrix-whatsapp", OWNER)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(view.chat_room.as_deref(), Some(OLD_CHAT));
        assert_eq!(view.chat_started_by.as_deref(), Some("owner"));

        // Settled: later steps leave the chat alone.
        manager.tick().await;
        manager.tick().await;
        assert!(fake.calls().is_empty(), "{:?}", fake.calls());
    }

    #[tokio::test]
    async fn a_chat_the_owner_started_is_recorded_as_theirs_and_left_alone() {
        let fake = Arc::new(FakeMatrix::default());
        *fake.creator.lock().unwrap() = OWNER.into();
        *fake.members.lock().unwrap() = vec![BOT.into(), OWNER.into()];
        *fake.bot_in_room.lock().unwrap() = true;
        let manager = ready_with_an_unsettled_chat(&fake, None).await;

        manager.tick().await;
        assert_eq!(
            fake.take_calls(),
            vec![
                format!("GET joined_members as={BOT}"),
                format!("GET state as={BOT}"),
            ]
        );
        assert!(fake.notices().is_empty());
        assert_eq!(
            row_of(&manager).dm_started_by.as_deref(),
            Some(CHAT_BY_OWNER)
        );
        manager.tick().await;
        assert!(fake.calls().is_empty());
    }

    #[tokio::test]
    async fn without_double_puppeting_the_bot_says_commands_need_its_prefix_once() {
        let fake = Arc::new(FakeMatrix::default());
        *fake.creator.lock().unwrap() = BOT.into();
        *fake.members.lock().unwrap() = vec![BOT.into(), OWNER.into()];
        *fake.bot_in_room.lock().unwrap() = true;
        let manager = ready_with_an_unsettled_chat(&fake, Some(false)).await;

        manager.tick().await;
        assert_eq!(
            fake.take_calls(),
            vec![
                format!("GET joined_members as={BOT}"),
                format!("GET state as={BOT}"),
                format!("PUT send as={BOT}"),
            ],
            "no leave, no invite: the instance cannot act as the owner"
        );
        let said = fake.notices().join("\n");
        assert!(
            said.contains("`!wa login qr` rather than `login qr`"),
            "{said}"
        );
        assert!(said.contains(BOT), "{said}");
        let row = row_of(&manager);
        assert_eq!(row.dm_started_by.as_deref(), Some(CHAT_BY_BOT));
        let view = manager
            .instance("mautrix-whatsapp", OWNER)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(view.chat_started_by.as_deref(), Some("bot"));
        // Said once.
        manager.tick().await;
        manager.tick().await;
        assert!(fake.calls().is_empty(), "{:?}", fake.calls());
        assert_eq!(fake.notices().len(), 1);
    }

    #[tokio::test]
    async fn a_chat_the_bot_is_no_longer_in_is_forgotten_and_a_new_one_started_as_the_owner() {
        let fake = Arc::new(FakeMatrix::default());
        *fake.creator.lock().unwrap() = BOT.into();
        *fake.bot_in_room.lock().unwrap() = false;
        let manager = ready_with_an_unsettled_chat(&fake, None).await;

        manager.tick().await;
        assert_eq!(
            fake.take_calls(),
            vec![format!("GET joined_members as={BOT}")]
        );
        let row = row_of(&manager);
        assert!(row.dm_room.is_none(), "forgotten");
        assert!(row.reason.is_none(), "{:?}", row.reason);

        // The next step starts a chat the way a new instance gets one: as the owner.
        manager.tick().await;
        let calls = fake.take_calls();
        assert_eq!(calls[0], "POST register as=", "{calls:?}");
        assert!(
            calls.contains(&format!("POST createRoom as={OWNER}")),
            "{calls:?}"
        );
        let row = row_of(&manager);
        assert_eq!(row.dm_room.as_deref(), Some("!new:example.org"));
        assert_eq!(row.dm_started_by.as_deref(), Some(CHAT_BY_OWNER));
    }

    #[tokio::test]
    async fn when_the_bot_cannot_get_back_in_the_chat_is_forgotten_and_the_reason_says_so() {
        let fake = Arc::new(FakeMatrix::default());
        *fake.creator.lock().unwrap() = BOT.into();
        *fake.members.lock().unwrap() = vec![BOT.into(), OWNER.into()];
        *fake.bot_in_room.lock().unwrap() = true;
        *fake.join_fails.lock().unwrap() = true;
        let manager = ready_with_an_unsettled_chat(&fake, None).await;

        manager.tick().await;
        let calls = fake.take_calls();
        assert!(calls.contains(&format!("POST leave as={BOT}")), "{calls:?}");
        assert!(calls.contains(&format!("POST join as={BOT}")), "{calls:?}");
        assert!(fake.notices().is_empty(), "nothing promised");
        let row = row_of(&manager);
        assert!(row.dm_room.is_none(), "forgotten");
        let reason = row.reason.clone().unwrap_or_default();
        assert!(reason.contains("could not rejoin"), "{reason}");
        assert!(reason.contains("a new chat will be started"), "{reason}");
    }

    #[tokio::test]
    async fn a_bots_chat_the_owner_has_not_joined_waits_for_them() {
        let fake = Arc::new(FakeMatrix::default());
        *fake.creator.lock().unwrap() = BOT.into();
        *fake.members.lock().unwrap() = vec![BOT.into()];
        *fake.bot_in_room.lock().unwrap() = true;
        let manager = ready_with_an_unsettled_chat(&fake, None).await;

        manager.tick().await;
        let calls = fake.take_calls();
        assert!(!calls.iter().any(|c| c.contains("leave")), "{calls:?}");
        assert!(fake.notices().is_empty());
        assert_eq!(row_of(&manager).dm_started_by.as_deref(), Some(CHAT_BY_BOT));

        // They accept the old invitation: the next step repairs the chat.
        *fake.members.lock().unwrap() = vec![BOT.into(), OWNER.into()];
        manager.tick().await;
        let calls = fake.take_calls();
        assert_eq!(
            calls,
            vec![
                format!("GET joined_members as={BOT}"),
                format!("POST leave as={BOT}"),
                format!("POST invite as={OWNER}"),
                format!("POST join as={BOT}"),
                format!("PUT send as={BOT}"),
            ],
            "the creator is known by then: no second look at the state"
        );
        assert_eq!(
            row_of(&manager).dm_started_by.as_deref(),
            Some(CHAT_BY_OWNER)
        );
    }

    #[tokio::test]
    async fn a_new_chat_records_who_started_it() {
        let fake = Arc::new(FakeMatrix::default());
        let manager = ready_with_an_unsettled_chat(&fake, None).await;
        manager
            .store
            .update_instance("mautrix-whatsapp", OWNER, |r| {
                r.dm_room = None;
                true
            })
            .unwrap();
        manager.tick().await;
        let row = row_of(&manager);
        assert_eq!(row.dm_room.as_deref(), Some("!new:example.org"));
        assert_eq!(row.dm_started_by.as_deref(), Some(CHAT_BY_OWNER));
        let calls = fake.take_calls();
        assert!(
            calls.contains(&format!("POST createRoom as={OWNER}")),
            "{calls:?}"
        );
        // Settled from the start: no further look.
        manager.tick().await;
        assert!(fake.calls().is_empty(), "{:?}", fake.calls());
    }

    #[test]
    fn the_notices_say_what_to_type() {
        let (text, html) = prefixed_commands_notice("WhatsApp", BOT, Some("!wa"));
        assert!(
            text.contains("`!wa login qr` rather than `login qr`"),
            "{text}"
        );
        assert!(html.contains("<code>!wa login qr</code>"), "{html}");
        let (text, _) = prefixed_commands_notice("Thing", BOT, None);
        assert!(text.contains("command prefix"), "{text}");
        let (text, html) = repaired_chat_notice("WhatsApp", &["Send `login qr`".into()]);
        assert!(text.ends_with("- Send `login qr`\n"), "{text}");
        assert!(
            html.ends_with("<ol><li>Send <code>login qr</code></li></ol>"),
            "{html}"
        );
    }
}
