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
use std::sync::Arc;

use async_trait::async_trait;
use hs_admin::model::{ConfigChange, ConfigReloadReport, ConfigSection, ConfigValidateReport};
use hs_admin::sources::{
    ConfigHistoryPage, ConfigPatch, ConfigRevert, ConfigRevertOutcome, ConfigSource, SourceError,
    config_change, config_section, revert_conflicts,
};
use hs_config::layered::Layers;
use hs_config::store::StoreError;
use hs_config::{Config, ConfigMeta};
use serde_json::Value;
use tokio::sync::RwLock;

use crate::bootstrap::OpenedConfigStore;
use crate::live_config::{Applied, LiveConfig};

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
    /// When each section was last hot-applied to the running server, RFC 3339. Written by
    /// [`StoreConfigSource::refresh`], which every write goes through.
    reloaded_at: BTreeMap<String, String>,
    /// What takes a change on in the running server (`crate::live_config`). `None` in a process
    /// that serves nothing, where there is nothing to apply a change to.
    live: Option<Arc<LiveConfig>>,
}

/// A [`ConfigSource`] backed by the running server's [`OpenedConfigStore`].
pub struct StoreConfigSource {
    store: OpenedConfigStore,
    state: RwLock<State>,
    /// The configuration this process actually booted on, frozen. `validate` and `reload`
    /// compare against it to report which changes wait for a restart.
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
                live: None,
            }),
            booted_config,
        }
    }

    /// Hot-applies every change this source writes, or reads back, to `live` -- the running
    /// server's side of the configuration (`crate::live_config`).
    #[must_use]
    pub fn with_live(self, live: Arc<LiveConfig>) -> Self {
        let mut state = self.state.into_inner();
        state.live = Some(live);
        Self {
            store: self.store,
            state: RwLock::new(state),
            booted_config: self.booted_config,
        }
    }

    /// The configuration as it resolves now -- file, database and environment, secrets
    /// included -- for what reads a setting when it is used rather than at boot (the migration
    /// from Synapse reads its source when it starts).
    ///
    /// # Errors
    /// The configuration does not resolve.
    pub async fn current_config(&self) -> Result<Config, hs_config::ConfigError> {
        Ok(self.state.read().await.layers.resolve()?.config)
    }

    /// Re-reads the database layer from the store, so the next resolve sees what was just
    /// written (see the module docs for what happens without it), and hot-applies what changed
    /// to the running server. Every write path calls this, which is what makes a change take
    /// effect however it was made. `None` when there is no running server to apply to, or the
    /// configuration does not resolve (the caller reports that itself).
    fn refresh(
        state: &mut State,
        store: &OpenedConfigStore,
    ) -> Result<Option<Applied>, StoreError> {
        let stored = store.load()?;
        state.layers.database = stored.document;
        state.meta = stored.meta;
        let Some(live) = state.live.clone() else {
            return Ok(None);
        };
        let Ok(resolved) = state.layers.resolve() else {
            return Ok(None);
        };
        let applied = live.apply(&resolved.config);
        if !applied.reloaded.is_empty() {
            let now = hs_http::time::now_rfc3339();
            for section in &applied.reloaded {
                state.reloaded_at.insert(section.clone(), now.clone());
            }
        }
        Ok(Some(applied))
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
            .map(|change| config_change(&change))
            .collect())
    }

    /// `section` as it resolves now, with its recent history -- what every write returns.
    fn section_now(&self, state: &State, section: &str) -> Result<ConfigSection, SourceError> {
        let resolved = state
            .layers
            .resolve()
            .map_err(|e| SourceError::Unavailable(e.to_string()))?;
        let mut out = config_section(
            &resolved,
            section,
            state.meta.revision,
            state.reloaded_at.get(section).cloned(),
        );
        out.history = self
            .section_history(section, SECTION_HISTORY_LIMIT)
            .map_err(|e| store_error(&e))?;
        Ok(out)
    }
}

/// [`Applied`] on the wire: a section that failed to apply is an error at its pointer.
fn reload_report(applied: Applied, revision: u64) -> ConfigReloadReport {
    ConfigReloadReport {
        reloaded_sections: applied.reloaded,
        errors: applied
            .failed
            .into_iter()
            .map(|(section, reason)| {
                hs_http::ValidationError::new(
                    format!("/{section}"),
                    format!(
                        "not applied to the running server, which keeps the old value: {reason}"
                    ),
                )
            })
            .collect(),
        requires_restart: applied.requires_restart,
        revision,
    }
}


/// A store failure is not the caller's fault and not something they can fix by changing the
/// request: it is this server being unable to answer.
fn store_error(e: &StoreError) -> SourceError {
    match e {
        StoreError::RevisionMismatch { .. } => SourceError::PreconditionFailed(e.to_string()),
        StoreError::UnknownSection { .. } | StoreError::NoSuchChange { .. } => {
            SourceError::NotFound
        }
        StoreError::NotRevertible { .. } => SourceError::Conflict(e.to_string()),
        StoreError::BootstrapSection { .. } | StoreError::BootstrapSetting { .. } => {
            SourceError::Conflict(e.to_string())
        }
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
        let bootstrap = hs_admin::sources::bootstrap_validation_errors(candidate);
        if !bootstrap.is_empty() {
            return Ok(ConfigValidateReport::invalid(bootstrap));
        }
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
        let bootstrap = Layers::bootstrap_in_patch(&request.section, &request.patch);
        if hs_config::store::is_bootstrap_section(&request.section) || !bootstrap.is_empty() {
            return Err(store_error(&StoreError::bootstrap(
                &request.section,
                bootstrap,
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
        // Validate and replace one database revision. A different replica may update settings
        // outside this patch, including ones that constrain whether the patch is valid.
        let mut attempts = 0;
        loop {
            attempts += 1;
            Self::refresh(&mut state, &self.store).map_err(|e| store_error(&e))?;
            let revision = state.meta.revision;
            if let Some(expected) = request.expected_revision
                && expected != revision
            {
                return Err(store_error(&StoreError::RevisionMismatch {
                    expected,
                    actual: revision,
                }));
            }
            state
                .layers
                .resolve_with_patch(&request.section, &request.patch)
                .map_err(|e| SourceError::Invalid(e.to_string()))?;

            match self.store.patch_section_expecting(
                &request.section,
                &request.patch,
                request.actor.as_deref(),
                now_ms(),
                Some(revision),
            ) {
                Ok(_) => break,
                Err(StoreError::RevisionMismatch { .. })
                    if request.expected_revision.is_none() && attempts < 3 => {}
                Err(e) => return Err(store_error(&e)),
            }
        }

        let applied = Self::refresh(&mut state, &self.store).map_err(|e| store_error(&e))?;

        let resolved = state
            .layers
            .resolve()
            .map_err(|e| SourceError::Unavailable(e.to_string()))?;
        let revision = state.meta.revision;
        let mut section = config_section(
            &resolved,
            &request.section,
            state.meta.revision,
            state.reloaded_at.get(&request.section).cloned(),
        );
        section.history = self
            .section_history(&request.section, SECTION_HISTORY_LIMIT)
            .map_err(|e| store_error(&e))?;
        section.applied = applied.map(|applied| reload_report(applied, revision));
        Ok(section)
    }

    async fn reload(&self) -> Result<ConfigReloadReport, SourceError> {
        let mut state = self.state.write().await;
        let applied = Self::refresh(&mut state, &self.store).map_err(|e| store_error(&e))?;
        let resolved = state
            .layers
            .resolve()
            .map_err(|e| SourceError::Invalid(e.to_string()))?;

        // What was hot-applied is what `crate::live_config` says it applied, and nothing else;
        // every section that has drifted from what the process booted on in a setting only a
        // restart reads is reported, so an operator is told a restart is pending rather than
        // left believing their change is in force.
        let applied = applied.unwrap_or_else(|| Applied {
            requires_restart: hs_config::reload::sections_requiring_restart(
                &self.booted_config,
                &resolved.config,
            )
            .into_iter()
            .map(str::to_owned)
            .collect(),
            ..Applied::default()
        });
        Ok(reload_report(applied, state.meta.revision))
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
                .iter()
                .map(config_change)
                .collect()),
        }
    }

    async fn history_page(
        &self,
        section: &str,
        before: Option<u64>,
        limit: usize,
    ) -> Result<ConfigHistoryPage, SourceError> {
        if !hs_config::reload::SECTION_NAMES.contains(&section) {
            return Err(SourceError::NotFound);
        }
        let page = self
            .store
            .history_page(Some(section), before, limit)
            .map_err(|e| store_error(&e))?;
        Ok(ConfigHistoryPage {
            changes: page.records.iter().map(config_change).collect(),
            older: page.older,
            newer: page.newer,
        })
    }

    async fn revert(&self, request: ConfigRevert) -> Result<ConfigRevertOutcome, SourceError> {
        let mut state = self.state.write().await;
        if !hs_config::reload::SECTION_NAMES.contains(&request.section.as_str()) {
            return Err(SourceError::NotFound);
        }
        // The store only writes a plan over the revision it was computed at. Without an
        // `If-Match` the caller accepts whatever is current, so a write from another replica in
        // between is planned again rather than reported as a precondition nobody set.
        let mut attempts = 0;
        let patch = loop {
            attempts += 1;
            let plan = self
                .store
                .revert_plan(&request.section, request.revision)
                .map_err(|e| store_error(&e))?;
            if let Some(expected) = request.expected_revision
                && expected != plan.revision
            {
                return Err(store_error(&StoreError::RevisionMismatch {
                    expected,
                    actual: plan.revision,
                }));
            }
            // Another replica may have written since this source last refreshed. Validate
            // against the same database revision the plan will atomically replace, including
            // settings outside the reverted patch that constrain whether it is valid.
            Self::refresh(&mut state, &self.store).map_err(|e| store_error(&e))?;
            if state.meta.revision != plan.revision {
                if request.expected_revision.is_none() && attempts < 3 {
                    continue;
                }
                return Err(store_error(&StoreError::RevisionMismatch {
                    expected: plan.revision,
                    actual: state.meta.revision,
                }));
            }
            if !plan.conflicts.is_empty() && !request.force {
                return Ok(ConfigRevertOutcome::Conflicts(revert_conflicts(
                    &request.section,
                    plan.conflicts,
                )));
            }
            if plan
                .patch
                .as_object()
                .is_some_and(serde_json::Map::is_empty)
            {
                Self::refresh(&mut state, &self.store).map_err(|e| store_error(&e))?;
                return Ok(ConfigRevertOutcome::Unchanged(
                    self.section_now(&state, &request.section)?,
                ));
            }
            // The same four checks as `patch_section`, against the patch the revert would write:
            // a revert is an ordinary write and must not be a way around any of them.
            let bootstrap = Layers::bootstrap_in_patch(&request.section, &plan.patch);
            if hs_config::store::is_bootstrap_section(&request.section) || !bootstrap.is_empty() {
                return Err(store_error(&StoreError::bootstrap(
                    &request.section,
                    bootstrap,
                )));
            }
            let pinned = state
                .layers
                .pinned_by_environment(&request.section, &plan.patch);
            if !pinned.is_empty() {
                return Err(SourceError::Conflict(format!(
                    "pinned by an HS__ environment variable, which outranks the database: {}",
                    pinned.join(", ")
                )));
            }
            state
                .layers
                .resolve_with_patch(&request.section, &plan.patch)
                .map_err(|e| SourceError::Invalid(e.to_string()))?;
            match self
                .store
                .apply_revert(&plan, request.actor.as_deref(), now_ms())
            {
                Ok(_) => break plan.patch,
                Err(StoreError::RevisionMismatch { .. })
                    if request.expected_revision.is_none() && attempts < 3 =>
                {
                    Self::refresh(&mut state, &self.store).map_err(|e| store_error(&e))?;
                }
                Err(e) => return Err(store_error(&e)),
            }
        };
        Self::refresh(&mut state, &self.store).map_err(|e| store_error(&e))?;
        Ok(ConfigRevertOutcome::Reverted {
            section: self.section_now(&state, &request.section)?,
            patch,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hs_config::FileLayer;
    use serde_json::json;

    #[tokio::test]
    async fn a_revert_validates_against_changes_from_another_source() {
        let dir = tempfile::tempdir().unwrap();
        let backend = hs_kv::fjall_backend::FjallBackend::open(dir.path()).unwrap();
        let storage = crate::storage::OpenedStorage::Embedded(backend);
        let writer = OpenedConfigStore::open(&storage).unwrap();
        writer
            .patch_section(
                "auth",
                &json!({"oidc_providers": [{
                    "idp_id": "example", "issuer": "https://issuer.example",
                    "client_id": "myelin"
                }]}),
                None,
                1,
            )
            .unwrap();
        let removed = writer
            .patch_section("auth", &json!({"oidc_providers": []}), None, 2)
            .unwrap();
        let layers = Layers {
            file: Some(FileLayer {
                path: "hs.yaml".into(),
                document: json!({"server": {"server_name": "example.org"}}),
            }),
            database: removed.document,
            ..Layers::default()
        };
        let booted = layers.resolve().unwrap().config;
        let source = StoreConfigSource::new(
            layers,
            OpenedConfigStore::open(&storage).unwrap(),
            removed.meta.clone(),
            booted,
        );
        // The second source enables MAS after the first source cached its layers. Restoring
        // OIDC providers must now fail: both auth modes cannot be configured together.
        let latest = writer
            .patch_section(
                "auth",
                &json!({"mas_delegation": {"endpoint": "https://mas.example"}}),
                None,
                3,
            )
            .unwrap();
        let result = source
            .revert(ConfigRevert {
                section: "auth".into(),
                revision: removed.meta.revision,
                actor: None,
                expected_revision: Some(latest.meta.revision),
                force: false,
            })
            .await;
        assert!(matches!(result, Err(SourceError::Invalid(_))), "{result:?}");
        let stored = writer.load().unwrap();
        assert_eq!(stored.meta.revision, latest.meta.revision);
        assert_eq!(stored.document["auth"]["oidc_providers"], json!([]));
    }

    #[tokio::test]
    async fn a_patch_validates_against_changes_from_another_source() {
        for conditional in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let backend = hs_kv::fjall_backend::FjallBackend::open(dir.path()).unwrap();
            let storage = crate::storage::OpenedStorage::Embedded(backend);
            let writer = OpenedConfigStore::open(&storage).unwrap();
            let initial = writer.load().unwrap();
            let layers = Layers {
                file: Some(FileLayer {
                    path: "hs.yaml".into(),
                    document: json!({"server": {"server_name": "example.org"}}),
                }),
                database: initial.document,
                ..Layers::default()
            };
            let booted = layers.resolve().unwrap().config;
            let source = StoreConfigSource::new(
                layers,
                OpenedConfigStore::open(&storage).unwrap(),
                initial.meta,
                booted,
            );
            let latest = writer
                .patch_section(
                    "auth",
                    &json!({"mas_delegation": {"endpoint": "https://mas.example"}}),
                    None,
                    1,
                )
                .unwrap();
            let result = source
                .patch_section(ConfigPatch {
                    section: "auth".into(),
                    patch: json!({"oidc_providers": [{
                        "idp_id": "example", "issuer": "https://issuer.example",
                        "client_id": "myelin"
                    }]}),
                    actor: None,
                    expected_revision: conditional.then_some(latest.meta.revision),
                })
                .await;
            assert!(matches!(result, Err(SourceError::Invalid(_))), "{result:?}");
            let stored = writer.load().unwrap();
            assert_eq!(stored.meta.revision, latest.meta.revision);
            assert_eq!(stored.document, latest.document);
        }
    }
}
