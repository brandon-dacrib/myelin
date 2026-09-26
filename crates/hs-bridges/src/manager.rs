//! The manager (RFC 0017 section 4.1): offerings, instances, and the state machine that takes
//! an instance from `requested` to `ready` -- registered, deployed, answering this server, and
//! its owner invited to it.

use std::collections::BTreeMap;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use async_trait::async_trait;
use hs_admin::bridge_offerings::{BridgeOfferingSource, SHARED_INSTANCE};
use hs_admin::bridge_types::{self, BRIDGE_INSTANCE_KEY, InstanceRender, InstanceSpec};
use hs_admin::model::{
    AdminAppserviceCreate, BridgeDeploymentTarget, BridgeInstance, BridgeInstanceFiles,
    BridgeOffering, BridgeOfferingRequest,
};
use hs_admin::sources::{AppserviceDirectory, SourceError};
use hs_kv::KvBackend;
use serde_json::json;
use sha2::{Digest, Sha256};

use crate::matrix::MatrixClient;
use crate::runtime::{DeploySpec, Runtime, manifest_yaml};
use crate::store::{BridgeStore, InstanceRow, InstanceState, ManagerRow, OfferingRow, StoreError};

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

/// The Kubernetes name for an instance's objects: short, DNS-safe, and the same every time for
/// the same appservice id.
#[must_use]
pub fn deploy_name(appservice_id: &str) -> String {
    let digest = Sha256::digest(appservice_id.as_bytes());
    format!("bridge-{}", &hex::encode(digest)[..8])
}

/// The manager.
pub struct BridgeManager<B: KvBackend> {
    pub(crate) store: BridgeStore<B>,
    directory: Arc<dyn AppserviceDirectory>,
    runtime: Option<Arc<dyn Runtime>>,
    pub(crate) server_name: String,
    /// How a bridge that runs elsewhere reaches this server: its public base URL.
    public_base_url: String,
    loopback: OnceLock<String>,
    pub(crate) client: OnceLock<MatrixClient>,
    wake: tokio::sync::Notify,
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
            public_base_url: public_base_url.trim_end_matches('/').to_owned(),
            loopback: OnceLock::new(),
            client: OnceLock::new(),
            wake: tokio::sync::Notify::new(),
        }))
    }

    /// Tells the manager where this server's client listener is, now that it is bound, and
    /// starts it: registers its own appservice there, then runs the state machine until the
    /// process ends.
    pub fn start(self: &Arc<Self>, loopback: &str) {
        let _ = self.loopback.set(loopback.trim_end_matches('/').to_owned());
        let _ = self.client.set(MatrixClient::new(loopback));
        let manager = self.clone();
        tokio::spawn(async move {
            if let Err(e) = manager.sync_registration().await {
                tracing::warn!(error = %e, "the bridge manager could not register its front doors");
            }
            loop {
                manager.tick().await;
                let _ = tokio::time::timeout(TICK, manager.wake.notified()).await;
            }
        });
    }

    /// The manager's own tokens.
    ///
    /// # Errors
    /// On a store failure.
    pub fn tokens(&self) -> Result<ManagerRow, StoreError> {
        self.store.manager(|| ManagerRow {
            as_token: crate::random_hex(32),
            hs_token: crate::random_hex(32),
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
        if let Some(client) = self.client.get() {
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

    fn offering_view(&self, row: &OfferingRow) -> Result<BridgeOffering, SourceError> {
        let kind =
            bridge_types::get(&row.bridge_type, &self.server_name).ok_or(SourceError::NotFound)?;
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
        Ok(BridgeOffering {
            bridge_type: row.bridge_type.clone(),
            name: kind.name.clone(),
            mode: kind.mode.clone(),
            enabled: row.enabled,
            runtime: row.runtime.clone(),
            image: format!("{repository}:{}", row.image_tag),
            front_door: (kind.mode == "per_user")
                .then(|| bridge_types::front_door_localpart(&row.bridge_type).map(|l| self.mxid(l)))
                .flatten(),
            access: row.access.clone(),
            options: row.options.clone(),
            instances: counts,
            created_at: rfc3339(row.created_at_ms),
        })
    }

    async fn instance_view(&self, row: &InstanceRow) -> BridgeInstance {
        let deployment = match (&self.runtime, &row.deploy_name) {
            (Some(runtime), Some(name)) if row.state != InstanceState::Requested => {
                runtime.status(name).await.ok().flatten()
            }
            _ => None,
        };
        let health = match &row.appservice_id {
            Some(id) => self.directory.health(id).await.ok().map(|h| h.status),
            None => None,
        };
        let bot = row.appservice_id.as_ref().and_then(|_| {
            bridge_types::instance_names(&row.bridge_type, owner_of(row))
                .map(|(bot, _)| self.mxid(&bot))
        });
        BridgeInstance {
            bridge_type: row.bridge_type.clone(),
            user_id: owner_of(row).map(str::to_owned),
            state: row.state.as_str().to_owned(),
            reason: row.reason.clone(),
            appservice_id: row.appservice_id.clone(),
            bot,
            deployment,
            health,
            created_at: rfc3339(row.created_at_ms),
            ready_at: row.ready_at_ms.map(rfc3339),
        }
    }

    // ---- rendering --------------------------------------------------------------------------

    /// How a bridge of `offering` reaches this server.
    fn homeserver_address(&self, offering: &OfferingRow) -> String {
        match (&self.runtime, offering.runtime.as_str()) {
            (Some(runtime), "cluster") => runtime
                .target()
                .homeserver_url
                .unwrap_or_else(|| self.public_base_url.clone()),
            _ => self.public_base_url.clone(),
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
        })
    }

    fn deploy_spec(row: &InstanceRow, render: &InstanceRender) -> Option<DeploySpec> {
        let appservice_id = row.appservice_id.clone()?;
        Some(DeploySpec {
            name: row.deploy_name.clone()?,
            labels: BTreeMap::from([
                ("myelin.dev/bridge-type".to_owned(), row.bridge_type.clone()),
                (
                    "myelin.dev/appservice-id".to_owned(),
                    label_safe(&appservice_id),
                ),
            ]),
            bridge_type: row.bridge_type.clone(),
            appservice_id,
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
        let rows = match self.store.instances(None) {
            Ok(rows) => rows,
            Err(e) => {
                tracing::warn!(error = %e, "the bridge manager could not read its instances");
                return;
            }
        };
        for row in rows {
            if let Err(e) = self.step(&row).await {
                tracing::warn!(bridge_type = %row.bridge_type, owner = %row.owner, error = %e, "bridge instance step failed");
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
            }
        }
    }

    async fn step(&self, row: &InstanceRow) -> Result<(), String> {
        let Some(offering) = self
            .store
            .offering(&row.bridge_type)
            .map_err(|e| e.to_string())?
        else {
            // Its offering is gone: it goes too.
            if row.state != InstanceState::Removing {
                self.set_state(row, InstanceState::Removing, None);
            }
            return self.remove(row).await;
        };
        let now = now_ms();
        match row.state {
            InstanceState::Requested => self.allocate_and_register(row, &offering).await,
            InstanceState::Registered => {
                if offering.runtime == "cluster" {
                    let runtime = self
                        .runtime
                        .clone()
                        .ok_or("this server cannot deploy bridges any more")?;
                    let render = self
                        .render(row, &offering)
                        .ok_or("the instance has no registration")?;
                    let spec = Self::deploy_spec(row, &render).ok_or("the instance has no name")?;
                    runtime.apply(&spec).await?;
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
                                    d.message
                                        .unwrap_or_else(|| "the pod never became ready".into()),
                                ),
                            );
                        } else {
                            self.set_reason(row, d.message);
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
                // An instance run by hand may take days; ask it every half minute, not every tick.
                if elsewhere
                    && now.saturating_sub(row.state_since_ms) % 30_000 > TICK.as_millis() as u64
                {
                    return Ok(());
                }
                let health = self.directory.ping(&id).await.map_err(|e| e.to_string())?;
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
                if row.dm_room.is_none() && owner_of(row).is_some() {
                    self.invite_owner(row, &offering).await?;
                }
                Ok(())
            }
            InstanceState::Failed => Ok(()),
            InstanceState::Removing => self.remove(row).await,
        }
    }

    fn set_state(&self, row: &InstanceRow, state: InstanceState, reason: Option<String>) {
        let from = row.state;
        let now = now_ms();
        let _ = self
            .store
            .update_instance(&row.bridge_type, &row.owner, |r| {
                if r.state != from {
                    return false; // someone else moved it
                }
                r.enter(state, now);
                r.reason = reason.clone();
                true
            });
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
        self.set_state(&row, InstanceState::Registered, None);
        self.wake();
        Ok(())
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
        let encrypted = offering.options.encryption.unwrap_or(true);
        let room = client
            .create_dm(&token, &bot, owner, encrypted)
            .await
            .map_err(|e| e.to_string())?;
        let recorded = self
            .store
            .update_instance(&row.bridge_type, &row.owner, |r| {
                if r.dm_room.is_some() {
                    return false;
                }
                r.dm_room = Some(room.clone());
                true
            })
            .map_err(|e| e.to_string())?;
        if recorded.is_none() {
            return Ok(()); // another replica got there first
        }
        let name = bridge_types::display_name(&row.bridge_type).unwrap_or(&row.bridge_type);
        let kind = bridge_types::get(&row.bridge_type, &self.server_name);
        let steps: Vec<String> = kind
            .map(|k| {
                k.sign_in
                    .steps
                    .iter()
                    .map(|s| s.replace("{bot}", "me"))
                    .collect()
            })
            .unwrap_or_default();
        let mut text = format!("This is your own {name} bridge. To sign in:\n");
        let mut html = format!("<p>This is your own {name} bridge. To sign in:</p><ol>");
        for step in steps.iter() {
            // "Start a direct chat with me and send `login qr`" reads oddly in the chat itself.
            let step = step.replace("Start a direct chat with me and send", "Send");
            text.push_str(&format!("- {step}\n"));
            html.push_str(&format!(
                "<li>{}</li>",
                crate::front_door::inline_html(&step)
            ));
        }
        html.push_str("</ol>");
        let _ = client.notice(&token, &bot, &room, &text, &html).await;
        // And where they asked for it, say it is done.
        if let (Some(door_room), Some(door)) = (
            &row.front_door_room,
            bridge_types::front_door_localpart(&row.bridge_type),
        ) {
            if let Ok(tokens) = self.tokens() {
                let text = format!(
                    "Your {name} bridge is ready. I've invited you to a chat with {bot}: accept it and follow the steps there to sign in."
                );
                let html = format!(
                    "Your {name} bridge is ready. I've invited you to a chat with <a href=\"https://matrix.to/#/{bot}\">{bot}</a>: accept it and follow the steps there to sign in."
                );
                let _ = client
                    .notice(&tokens.as_token, &self.mxid(door), door_room, &text, &html)
                    .await;
            }
        }
        Ok(())
    }

    async fn remove(&self, row: &InstanceRow) -> Result<(), String> {
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
        self.store
            .offerings()
            .map_err(store_err)?
            .iter()
            .map(|row| self.offering_view(row))
            .collect()
    }

    async fn get(&self, bridge_type: &str) -> Result<Option<BridgeOffering>, SourceError> {
        match self.store.offering(bridge_type).map_err(store_err)? {
            Some(row) => Ok(Some(self.offering_view(&row)?)),
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
        let default_tag = kind
            .image
            .rsplit_once(':')
            .map_or("latest", |(_, t)| t)
            .to_owned();
        let row = OfferingRow {
            bridge_type: bridge_type.to_owned(),
            enabled: request
                .enabled
                .or(current.as_ref().map(|c| c.enabled))
                .unwrap_or(true),
            runtime,
            image_tag: request
                .image_tag
                .clone()
                .filter(|t| !t.trim().is_empty())
                .or(current.as_ref().map(|c| c.image_tag.clone()))
                .unwrap_or(default_tag),
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
        if kind.mode == "shared" {
            self.request(bridge_type, SHARED_INSTANCE, None)
                .map_err(store_err)?;
        }
        self.offering_view(&row)
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
