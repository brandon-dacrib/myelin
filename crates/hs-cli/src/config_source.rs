//! The real [`ConfigSource`]: the admin API, and so the management web interface, writing to this
//! server's own configuration store.
//!
//! `hs-admin` declares the seam and answers `503 unavailable` until something fills it
//! (`hs_admin::sources::ConfigSource`); `hs-config` owns the store and the layering; this is the
//! one place that knows about both. Without it every `/config*` operation is honest and useless.
//!
//! # Why the layers are behind a lock
//!
//! [`Layers::database`](hs_config::Layers) is a snapshot taken when the store was opened, and a
//! write does not update it. In `hs config`, each invocation is its own process and it never
//! showed; in a long-lived `hs serve` it would mean an operator saves a setting, the API reports
//! success, and every subsequent read answers with the configuration from before their write --
//! indistinguishable from the write having been lost. Every write path here re-reads the store
//! before it returns.

use std::collections::BTreeMap;

use async_trait::async_trait;
use hs_admin::model::{ConfigChange, ConfigReloadReport, ConfigSection, ConfigValidateReport};
use hs_admin::sources::{ConfigPatch, ConfigSource, SourceError, config_section};
use hs_config::layered::Layers;
use hs_config::store::StoreError;
use hs_config::{Config, ConfigMeta};
use serde_json::Value;
use tokio::sync::RwLock;

use crate::bootstrap::OpenedConfigStore;

/// How many changes `GET /config/{section}` carries with a section.
const SECTION_HISTORY_LIMIT: usize = 20;

/// How far back to look for a section's own changes before giving up. A section's history is
/// filtered out of the store's global one, so a busy neighbour must not be able to crowd a quiet
/// section's history out of the answer -- see [`ConfigSource::history`]'s contract.
const HISTORY_SCAN: usize = 500;

/// The mutable half: the layers the server resolves through, and the store's metadata.
struct State {
    layers: Layers,
    meta: ConfigMeta,
    /// When each section was last reloaded, RFC 3339. Only ever written by [`StoreConfigSource::reload`].
    reloaded_at: BTreeMap<String, String>,
}

/// A [`ConfigSource`] backed by the running server's [`OpenedConfigStore`].
pub struct StoreConfigSource {
    store: OpenedConfigStore,
    state: RwLock<State>,
    /// The configuration this process actually booted on, frozen. `reload` compares against it to
    /// report what has changed since -- which is the only honest thing it can say, because
    /// nothing in this server re-reads its configuration while running yet.
    booted_config: Config,
}

impl std::fmt::Debug for StoreConfigSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("StoreConfigSource")
    }
}

impl StoreConfigSource {
    /// Builds the source from a booted server's layers and store.
    #[must_use]
    pub fn new(
        layers: Layers,
        store: OpenedConfigStore,
        meta: ConfigMeta,
        booted_config: Config,
    ) -> Self {
        Self {
            store,
            state: RwLock::new(State {
                layers,
                meta,
                reloaded_at: BTreeMap::new(),
            }),
            booted_config,
        }
    }

    /// Re-reads the database layer from the store, so the next resolve sees what was just
    /// written. See the module docs for what happens without it.
    fn refresh(state: &mut State, store: &OpenedConfigStore) -> Result<(), StoreError> {
        let stored = store.load()?;
        state.layers.database = stored.document;
        state.meta = stored.meta;
        Ok(())
    }

    /// This section's own changes, newest first.
    fn section_history(
        &self,
        section: &str,
        limit: usize,
    ) -> Result<Vec<ConfigChange>, StoreError> {
        Ok(self
            .store
            .history(HISTORY_SCAN)?
            .into_iter()
            .filter(|change| change.section == section)
            .take(limit)
            .map(change_to_model)
            .collect())
    }
}

fn change_to_model(change: hs_config::store::ChangeRecord) -> ConfigChange {
    ConfigChange {
        revision: change.revision,
        section: change.section,
        patch: change.patch,
        actor: change.actor,
        at: hs_http::time::rfc3339_from_millis(change.at_ms),
    }
}

/// A store failure is not the caller's fault and not something they can fix by changing the
/// request: it is this server being unable to answer.
fn store_error(e: &StoreError) -> SourceError {
    match e {
        StoreError::RevisionMismatch { .. } => SourceError::PreconditionFailed(e.to_string()),
        StoreError::UnknownSection { .. } => SourceError::NotFound,
        StoreError::BootstrapSection { .. } => SourceError::Conflict(e.to_string()),
        StoreError::Kv(_) | StoreError::Corrupt { .. } => SourceError::Unavailable(e.to_string()),
    }
}

fn now_ms() -> i64 {
    i64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis(),
    )
    .unwrap_or(i64::MAX)
}

#[async_trait]
impl ConfigSource for StoreConfigSource {
    async fn list_sections(&self) -> Result<Vec<ConfigSection>, SourceError> {
        let state = self.state.read().await;
        let resolved = state
            .layers
            .resolve()
            .map_err(|e| SourceError::Unavailable(e.to_string()))?;
        Ok(hs_config::reload::SECTION_NAMES
            .iter()
            .map(|name| {
                config_section(
                    &resolved,
                    name,
                    state.meta.revision,
                    state.reloaded_at.get(*name).cloned(),
                )
            })
            .collect())
    }

    async fn get_section(&self, name: &str) -> Result<Option<ConfigSection>, SourceError> {
        if !hs_config::reload::SECTION_NAMES.contains(&name) {
            return Ok(None);
        }
        let state = self.state.read().await;
        let resolved = state
            .layers
            .resolve()
            .map_err(|e| SourceError::Unavailable(e.to_string()))?;
        let mut section = config_section(
            &resolved,
            name,
            state.meta.revision,
            state.reloaded_at.get(name).cloned(),
        );
        section.history = self
            .section_history(name, SECTION_HISTORY_LIMIT)
            .map_err(|e| store_error(&e))?;
        Ok(Some(section))
    }

    async fn environment_pinned(
        &self,
        section: &str,
        patch: &Value,
    ) -> Result<Vec<String>, SourceError> {
        let state = self.state.read().await;
        Ok(state.layers.pinned_by_environment(section, patch))
    }

    async fn validate(&self, candidate: &Value) -> Result<ConfigValidateReport, SourceError> {
        let state = self.state.read().await;
        let mut proposed = state.layers.clone();
        let mut database = proposed.database.clone();
        hs_config::merge_patch(&mut database, candidate);
        proposed.database = database;

        match proposed.resolve() {
            Ok(new) => {
                let requires_restart =
                    hs_config::reload::sections_requiring_restart(&self.booted_config, &new.config)
                        .into_iter()
                        .map(str::to_owned)
                        .collect();
                Ok(ConfigValidateReport::valid(requires_restart))
            }
            Err(e) => Ok(ConfigValidateReport::invalid(
                hs_admin::sources::config_validation_errors(&e),
            )),
        }
    }

    async fn patch_section(&self, request: ConfigPatch) -> Result<ConfigSection, SourceError> {
        let mut state = self.state.write().await;

        if !hs_config::reload::SECTION_NAMES.contains(&request.section.as_str()) {
            return Err(SourceError::NotFound);
        }
        if hs_config::store::is_bootstrap_section(&request.section) {
            return Err(SourceError::Conflict(format!(
                "{:?} says where this server's database is, so it cannot be stored in it",
                request.section
            )));
        }
        // The handler checked this too. It is checked again here because between the two there is
        // a window in which another operator can write, and a patch computed against the older
        // view would silently overwrite theirs.
        let pinned = state
            .layers
            .pinned_by_environment(&request.section, &request.patch);
        if !pinned.is_empty() {
            return Err(SourceError::Conflict(format!(
                "pinned by an HS__ environment variable, which outranks the database: {}",
                pinned.join(", ")
            )));
        }
        // Validate what the patch would produce, not the patch: a stored setting this server then
        // refuses to boot on is far worse than a rejected request.
        state
            .layers
            .resolve_with_patch(&request.section, &request.patch)
            .map_err(|e| SourceError::Invalid(e.to_string()))?;

        self.store
            .patch_section_expecting(
                &request.section,
                &request.patch,
                request.actor.as_deref(),
                now_ms(),
                request.expected_revision,
            )
            .map_err(|e| store_error(&e))?;

        Self::refresh(&mut state, &self.store).map_err(|e| store_error(&e))?;

        let resolved = state
            .layers
            .resolve()
            .map_err(|e| SourceError::Unavailable(e.to_string()))?;
        let mut section = config_section(
            &resolved,
            &request.section,
            state.meta.revision,
            state.reloaded_at.get(&request.section).cloned(),
        );
        section.history = self
            .section_history(&request.section, SECTION_HISTORY_LIMIT)
            .map_err(|e| store_error(&e))?;
        Ok(section)
    }

    async fn reload(&self) -> Result<ConfigReloadReport, SourceError> {
        let mut state = self.state.write().await;
        Self::refresh(&mut state, &self.store).map_err(|e| store_error(&e))?;
        let resolved = state
            .layers
            .resolve()
            .map_err(|e| SourceError::Invalid(e.to_string()))?;

        // Honest, and unflattering: nothing in this server re-reads its configuration while
        // running. The rate limiter, the federation policy and the telemetry layer are all built
        // once at startup, so there is no section this call can truthfully claim to have swapped
        // in -- `reloaded_sections` stays empty until one of them grows a live read. What it does
        // do is refresh what the API itself serves, and report every section that has drifted
        // from what the process booted on, so an operator is told a restart is pending rather
        // than left believing their change is in force.
        let requires_restart: Vec<String> =
            hs_config::reload::sections_requiring_restart(&self.booted_config, &resolved.config)
                .into_iter()
                .map(str::to_owned)
                .collect();

        Ok(ConfigReloadReport {
            reloaded_sections: Vec::new(),
            errors: Vec::new(),
            requires_restart,
            revision: state.meta.revision,
        })
    }

    async fn history(
        &self,
        section: Option<&str>,
        limit: usize,
    ) -> Result<Vec<ConfigChange>, SourceError> {
        match section {
            Some(name) => self
                .section_history(name, limit)
                .map_err(|e| store_error(&e)),
            None => Ok(self
                .store
                .history(limit)
                .map_err(|e| store_error(&e))?
                .into_iter()
                .map(change_to_model)
                .collect()),
        }
    }
}
