//! The registry façade: the operations `hs appservice add|show|list|update|pause|resume|remove|
//! rotate-tokens`, the admin API and the console all funnel through (`PLAN.md` section 8.2).
//!
//! Wraps [`crate::store::AppserviceStore`] with the business logic a bare CRUD layer should not
//! own: namespace conflict detection, token generation and rotation, health status computation,
//! and registration-file import semantics.

use std::sync::Arc;

use hs_auth::clock::{Clock, SystemClock};
use hs_kv::KvBackend;
use rand::RngCore;
use ruma::{OwnedServerName, ServerName};
use serde_json::Value;

use crate::error::AppserviceError;
use crate::namespace::NamespaceKind;
use crate::registration::Registration;
use crate::store::{AppserviceRow, AppserviceStore, HealthRow, QueueStatus, QueuedTransaction};

/// A hook for checking whether a literal ID is already claimed by something outside the
/// appservice registry — principally, an already-registered human user. `PLAN.md` Appendix B and
/// section 8.2 both call for "namespace conflict detection against existing registrations and
/// users", but the user table is track 04's (`hs-user`), which does not exist yet; this trait is
/// the seam. [`NoExternalUsers`] is the default, honest no-op until track 04 lands one (see
/// `docs/status/11-appservices-and-bridges.md`).
pub trait ExternalIdentityChecker: Send + Sync {
    /// True if `user_id` (a full Matrix user ID) is already a real, registered account.
    fn user_exists(&self, user_id: &str) -> bool;
}

/// The default [`ExternalIdentityChecker`]: no external users are ever known, so this check
/// contributes nothing (registry-vs-registry conflict detection still applies).
#[derive(Debug, Clone, Copy, Default)]
pub struct NoExternalUsers;

impl ExternalIdentityChecker for NoExternalUsers {
    fn user_exists(&self, _user_id: &str) -> bool {
        false
    }
}

/// Health as the admin API reports it (`AppServiceHealth.status` /
/// `AppService.health` in `crates/hs-admin/openapi/openapi.yaml`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HealthStatus {
    /// Delivering normally.
    Healthy,
    /// Recent failures, below the threshold that marks it down.
    Degraded,
    /// At or past the consecutive-failure threshold; delivery is not making progress.
    Down,
    /// Paused by an operator; no delivery is attempted at all.
    Paused,
    /// No delivery has ever been attempted, so there is nothing to judge yet.
    Unknown,
}

/// [`HealthStatus`] plus the raw numbers the admin API's `AppServiceHealth` schema wants.
#[derive(Debug, Clone)]
pub struct Health {
    /// The computed status.
    pub status: HealthStatus,
    /// `AppServiceHealth.last_ping_at`.
    pub last_ping_at_ms: Option<u64>,
    /// The most recent delivery or ping error.
    pub last_error: Option<String>,
    /// `AppServiceHealth`'s backing consecutive-failure count.
    pub consecutive_failures: u32,
    /// When a transaction was last delivered successfully.
    pub last_success_at_ms: Option<u64>,
}

fn compute_status(paused: bool, health: &HealthRow, threshold: u32) -> HealthStatus {
    if paused {
        return HealthStatus::Paused;
    }
    if health.last_ping_at_ms.is_none() && health.last_success_at_ms.is_none() {
        return HealthStatus::Unknown;
    }
    // A ping that failed, with nothing delivered since, is the freshest thing known about this
    // bridge: it is not healthy, whatever the delivery counter says. Before this an operator
    // could press "ping", watch it fail, and read "healthy" beside the error.
    let last_ping_failed = health.last_ping_success == Some(false)
        && health.last_ping_at_ms >= health.last_success_at_ms;
    if health.consecutive_failures == 0 && !last_ping_failed {
        return HealthStatus::Healthy;
    }
    if health.consecutive_failures >= threshold {
        return HealthStatus::Down;
    }
    HealthStatus::Degraded
}

/// One backlog entry as the admin API reports it (`AppServiceBacklogEntry`).
#[derive(Debug, Clone)]
pub struct BacklogEntry {
    /// The transaction (queue sequence) id.
    pub transaction_id: String,
    /// How long this entry has been waiting, in milliseconds.
    pub age_ms: u64,
    /// Delivery attempts made so far.
    pub attempts: u32,
    /// The most recent delivery error, if any.
    pub last_error: Option<String>,
    /// Whether this entry has exhausted its retry budget.
    pub dead_lettered: bool,
}

/// The appservice registry: registration storage, namespace conflict detection, health, and the
/// transaction-queue views the admin API's backlog endpoint needs.
pub struct Registry<B: KvBackend> {
    store: AppserviceStore<B>,
    clock: Arc<dyn Clock>,
    server_name: OwnedServerName,
    identity_checker: Arc<dyn ExternalIdentityChecker>,
    /// Consecutive failures before an appservice is reported `down` rather than `degraded`
    /// (`hs-config`'s `AppservicesConfig::tracking_failure_threshold`).
    pub failure_threshold: u32,
}

impl<B: KvBackend> Registry<B> {
    /// Opens a registry over `backend` for `server_name`, with the real system clock and no
    /// external identity checker.
    ///
    /// # Errors
    /// Returns [`AppserviceError::Store`] if the underlying keyspaces could not be opened.
    pub fn open(backend: B, server_name: &ServerName) -> Result<Self, AppserviceError> {
        Ok(Self {
            store: AppserviceStore::open(backend)?,
            clock: Arc::new(SystemClock),
            server_name: server_name.to_owned(),
            identity_checker: Arc::new(NoExternalUsers),
            failure_threshold: 50,
        })
    }

    /// Overrides the clock (tests) and/or the failure threshold and identity checker, builder
    /// style.
    #[must_use]
    pub fn with_clock(mut self, clock: Arc<dyn Clock>) -> Self {
        self.clock = clock;
        self
    }

    /// See [`ExternalIdentityChecker`].
    #[must_use]
    pub fn with_identity_checker(mut self, checker: Arc<dyn ExternalIdentityChecker>) -> Self {
        self.identity_checker = checker;
        self
    }

    /// Sets the consecutive-failure threshold used by [`Registry::health`].
    #[must_use]
    pub fn with_failure_threshold(mut self, threshold: u32) -> Self {
        self.failure_threshold = threshold;
        self
    }

    /// The underlying store, for the scheduler and other crate-internal callers.
    #[must_use]
    pub(crate) fn store(&self) -> &AppserviceStore<B> {
        &self.store
    }

    /// This homeserver's server name, for building full sender user IDs from a row's
    /// `sender_localpart` (`crate::auth_registry`).
    #[must_use]
    pub fn server_name(&self) -> &ServerName {
        &self.server_name
    }

    pub(crate) fn now_ms(&self) -> u64 {
        self.clock.now_ms()
    }

    fn sender_user_id(&self, localpart: &str) -> String {
        format!("@{localpart}:{}", self.server_name)
    }

    /// Registers a brand-new appservice from a parsed [`Registration`]. Hot: live immediately,
    /// per `PLAN.md` section 8.2.
    ///
    /// # Errors
    /// Returns [`AppserviceError::AlreadyExists`], [`AppserviceError::TokenConflict`] or
    /// [`AppserviceError::NamespaceConflict`] if the registration collides with an existing one.
    pub fn add(&self, reg: &Registration) -> Result<AppserviceRow, AppserviceError> {
        self.check_namespace_conflicts(reg, None)?;
        let row = AppserviceRow::from_registration(reg, self.now_ms());
        self.store.insert(&row)?;
        Ok(row)
    }

    /// Imports a registration file's contents (`app_service_config_files` at startup, or a
    /// reload): upserts by id, so re-importing the same static file after an edit updates the
    /// existing row rather than erroring with "already exists" — this is what makes static
    /// registration files hot-reloadable the same way registry rows are.
    ///
    /// # Errors
    /// Returns [`AppserviceError::Registration`] if `yaml` does not parse, or
    /// [`AppserviceError::TokenConflict`]/[`AppserviceError::NamespaceConflict`] if it collides
    /// with a *different* appservice.
    pub fn import_yaml(&self, yaml: &str) -> Result<AppserviceRow, AppserviceError> {
        let reg = Registration::parse_yaml(yaml)?;
        self.import(&reg)
    }

    /// As [`Registry::import_yaml`], for an already-parsed [`Registration`].
    ///
    /// # Errors
    /// See [`Registry::import_yaml`].
    pub fn import(&self, reg: &Registration) -> Result<AppserviceRow, AppserviceError> {
        self.check_namespace_conflicts(reg, Some(&reg.id))?;
        let now = self.now_ms();
        let row = match self.store.get(&reg.id)? {
            Some(existing) => {
                let mut updated = AppserviceRow::from_registration(reg, existing.created_at_ms);
                updated.paused = existing.paused;
                updated.updated_at_ms = now;
                self.store.replace(&updated)?;
                updated
            }
            None => {
                let row = AppserviceRow::from_registration(reg, now);
                self.store.insert(&row)?;
                row
            }
        };
        Ok(row)
    }

    /// One appservice by id.
    ///
    /// # Errors
    /// Returns [`AppserviceError::Store`] on backend failure.
    pub fn get(&self, id: &str) -> Result<Option<AppserviceRow>, AppserviceError> {
        self.store.get(id)
    }

    /// Every registered appservice.
    ///
    /// # Errors
    /// Returns [`AppserviceError::Store`] on backend failure.
    pub fn list(&self) -> Result<Vec<AppserviceRow>, AppserviceError> {
        self.store.list()
    }

    /// Applies an RFC 7396 JSON Merge Patch to an appservice's registration fields (everything
    /// but `id`), matching the admin API's `PATCH /appservices/{id}` semantics
    /// (`AppServiceUpdate` in `crates/hs-admin/openapi/openapi.yaml`). `sender_localpart` is
    /// immutable through this path (the OpenAPI doc says "everything but id and
    /// sender_localpart"); a patch that includes it is ignored for that field, not rejected,
    /// since a merge patch's whole point is "apply what you understand".
    ///
    /// # Errors
    /// Returns [`AppserviceError::NotFound`] if `id` is not registered, or
    /// [`AppserviceError::NamespaceConflict`]/[`AppserviceError::TokenConflict`] if the patched
    /// result collides with a different appservice.
    pub fn update(&self, id: &str, patch: &Value) -> Result<AppserviceRow, AppserviceError> {
        let existing = self
            .store
            .get(id)?
            .ok_or_else(|| AppserviceError::NotFound(id.to_string()))?;
        let reg = existing.to_registration()?;
        let mut as_json = serde_json::to_value(RegistrationJson::from(&reg))
            .map_err(|e| AppserviceError::Decode(e.to_string()))?;
        json_merge_patch(&mut as_json, patch);
        // sender_localpart and id are not part of a merge-patchable registration surface.
        as_json["id"] = Value::String(id.to_string());
        as_json["sender_localpart"] = Value::String(existing.sender_localpart.clone());
        let patched: RegistrationJson =
            serde_json::from_value(as_json).map_err(|e| AppserviceError::Decode(e.to_string()))?;
        let patched_reg = patched.into_registration()?;

        self.check_namespace_conflicts(&patched_reg, Some(id))?;
        let mut row = AppserviceRow::from_registration(&patched_reg, existing.created_at_ms);
        row.paused = existing.paused;
        row.updated_at_ms = self.now_ms();
        self.store.replace(&row)?;
        Ok(row)
    }

    /// Pauses delivery: the scheduler keeps enqueueing but stops dequeuing, so nothing sent while
    /// paused is lost, only delayed.
    ///
    /// # Errors
    /// Returns [`AppserviceError::NotFound`] if `id` is not registered.
    pub fn pause(&self, id: &str) -> Result<AppserviceRow, AppserviceError> {
        self.set_paused(id, true)
    }

    /// Resumes a paused appservice.
    ///
    /// # Errors
    /// Returns [`AppserviceError::NotFound`] if `id` is not registered.
    pub fn resume(&self, id: &str) -> Result<AppserviceRow, AppserviceError> {
        self.set_paused(id, false)
    }

    fn set_paused(&self, id: &str, paused: bool) -> Result<AppserviceRow, AppserviceError> {
        let mut row = self
            .store
            .get(id)?
            .ok_or_else(|| AppserviceError::NotFound(id.to_string()))?;
        row.paused = paused;
        row.updated_at_ms = self.now_ms();
        self.store.replace(&row)?;
        Ok(row)
    }

    /// Removes an appservice and its transaction queue and health rows.
    ///
    /// # Errors
    /// Returns [`AppserviceError::NotFound`] if `id` is not registered.
    pub fn remove(&self, id: &str) -> Result<(), AppserviceError> {
        self.store.remove(id)?;
        self.store.purge_queue(id)?;
        self.store.delete_health(id)?;
        Ok(())
    }

    /// Generates fresh `as_token`/`hs_token` values (32 random bytes, hex-encoded — the shape
    /// `mautrix`'s own registration generators and `openssl rand -hex 32` both produce) and
    /// stores them, invalidating the old ones immediately.
    ///
    /// # Errors
    /// Returns [`AppserviceError::NotFound`] if `id` is not registered.
    pub fn rotate_tokens(&self, id: &str) -> Result<AppserviceRow, AppserviceError> {
        let mut row = self
            .store
            .get(id)?
            .ok_or_else(|| AppserviceError::NotFound(id.to_string()))?;
        row.as_token = generate_token();
        row.hs_token = generate_token();
        row.updated_at_ms = self.now_ms();
        self.store.replace(&row)?;
        Ok(row)
    }

    /// Renders an appservice's registration as YAML, for the admin API's
    /// `GET /appservices/{id}/registration` (includes tokens, matching the OpenAPI doc's warning
    /// that this endpoint requires `bridges:write`, not just `:read`).
    ///
    /// # Errors
    /// Returns [`AppserviceError::NotFound`] if `id` is not registered.
    pub fn export_registration_yaml(&self, id: &str) -> Result<String, AppserviceError> {
        let row = self
            .store
            .get(id)?
            .ok_or_else(|| AppserviceError::NotFound(id.to_string()))?;
        Ok(row.to_registration()?.to_yaml())
    }

    /// Computed health for one appservice.
    ///
    /// # Errors
    /// Returns [`AppserviceError::NotFound`] if `id` is not registered.
    pub fn health(&self, id: &str) -> Result<Health, AppserviceError> {
        let row = self
            .store
            .get(id)?
            .ok_or_else(|| AppserviceError::NotFound(id.to_string()))?;
        let health_row = self.store.health(id)?;
        Ok(Health {
            status: compute_status(row.paused, &health_row, self.failure_threshold),
            last_ping_at_ms: health_row.last_ping_at_ms,
            last_error: health_row.last_error,
            consecutive_failures: health_row.consecutive_failures,
            last_success_at_ms: health_row.last_success_at_ms,
        })
    }

    /// The pending and dead-lettered backlog for one appservice, oldest first — the admin API's
    /// `GET /appservices/{id}/backlog`. Delivered entries (kept briefly for backlog-age
    /// reporting by the scheduler) are excluded, since they are not backlog.
    ///
    /// # Errors
    /// Returns [`AppserviceError::Store`] on backend failure.
    pub fn backlog(&self, id: &str) -> Result<Vec<BacklogEntry>, AppserviceError> {
        let now = self.now_ms();
        let mut entries: Vec<QueuedTransaction> = self
            .store
            .queue_for(id)?
            .into_iter()
            .filter(|e| e.status != QueueStatus::Delivered)
            .collect();
        entries.sort_by_key(|e| e.seq);
        Ok(entries
            .into_iter()
            .map(|e| BacklogEntry {
                transaction_id: e.seq.to_string(),
                age_ms: now.saturating_sub(e.enqueued_at_ms),
                attempts: e.attempts,
                last_error: e.last_error,
                dead_lettered: e.status == QueueStatus::DeadLettered,
            })
            .collect())
    }

    /// Namespace conflict detection (`PLAN.md` section 8.2's "namespace conflict checks against
    /// existing registrations and existing users"). Checks, against every *other* registered
    /// appservice (and, via [`ExternalIdentityChecker`], any external identity source track 04
    /// wires in):
    ///
    /// 1. No other appservice already has this exact `sender_localpart`.
    /// 2. This appservice's own sender is not already claimed by another appservice's exclusive
    ///    `users` namespace.
    /// 3. None of this appservice's *exclusive* namespace rules match another appservice's
    ///    sender (a literal-against-regex check — always decidable).
    /// 4. None of this appservice's exclusive namespace rules is *textually identical* to another
    ///    appservice's exclusive rule of the same category (regex-against-regex equivalence is
    ///    undecidable in general — Synapse itself does not attempt it either — so this is the
    ///    decidable subset: catching the operationally common case of the same pattern
    ///    registered twice, by accident or by a bridge being re-added).
    /// 5. The [`ExternalIdentityChecker`] is asked whether this appservice's own sender already
    ///    exists as a non-appservice account.
    ///
    /// # Errors
    /// Returns [`AppserviceError::NamespaceConflict`] on the first conflict found.
    fn check_namespace_conflicts(
        &self,
        reg: &Registration,
        self_id: Option<&str>,
    ) -> Result<(), AppserviceError> {
        let sender = self.sender_user_id(&reg.sender_localpart);

        if self.identity_checker.user_exists(&sender) {
            return Err(AppserviceError::NamespaceConflict {
                new_id: reg.id.clone(),
                kind: "sender",
                value: sender,
                existing_id: "<external user>".to_string(),
            });
        }

        for other in self.store.list()? {
            if Some(other.id.as_str()) == self_id {
                continue;
            }

            if other.sender_localpart == reg.sender_localpart {
                return Err(AppserviceError::NamespaceConflict {
                    new_id: reg.id.clone(),
                    kind: "sender",
                    value: sender,
                    existing_id: other.id,
                });
            }

            let other_reg = other.to_registration()?;
            let other_sender = self.sender_user_id(&other.sender_localpart);

            if reg
                .namespaces
                .exclusive_match(NamespaceKind::Users, &other_sender)
            {
                return Err(AppserviceError::NamespaceConflict {
                    new_id: reg.id.clone(),
                    kind: "users",
                    value: other_sender,
                    existing_id: other.id.clone(),
                });
            }
            if other_reg
                .namespaces
                .exclusive_match(NamespaceKind::Users, &sender)
            {
                return Err(AppserviceError::NamespaceConflict {
                    new_id: reg.id.clone(),
                    kind: "users",
                    value: sender,
                    existing_id: other.id.clone(),
                });
            }

            for kind in [
                NamespaceKind::Users,
                NamespaceKind::Aliases,
                NamespaceKind::Rooms,
            ] {
                for rule in reg.namespaces.category(kind) {
                    if !rule.exclusive {
                        continue;
                    }
                    for other_rule in other_reg.namespaces.category(kind) {
                        if other_rule.exclusive
                            && other_rule.pattern.source() == rule.pattern.source()
                        {
                            return Err(AppserviceError::NamespaceConflict {
                                new_id: reg.id.clone(),
                                kind: kind.key(),
                                value: rule.pattern.source().to_string(),
                                existing_id: other.id.clone(),
                            });
                        }
                    }
                }
            }
        }

        Ok(())
    }
}

fn generate_token() -> String {
    let mut bytes = [0u8; 32];
    rand::rng().fill_bytes(&mut bytes);
    hex::encode(bytes)
}

/// The JSON Merge Patch (RFC 7396) target shape for [`Registry::update`]: exactly the fields of
/// [`Registration`] that a merge patch can touch, using plain JSON types (so `namespaces` is the
/// spec's own object shape, not the compiled form) with defaults so a patch omitting a whole
/// section (say, `protocols`) leaves it untouched via the merge-patch algorithm rather than this
/// struct's `Deserialize` failing on a missing field.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct RegistrationJson {
    id: String,
    url: Option<String>,
    as_token: String,
    hs_token: String,
    sender_localpart: String,
    rate_limited: bool,
    namespaces: crate::namespace::NamespacesSpec,
    protocols: Vec<String>,
    receive_ephemeral: bool,
    #[serde(default)]
    push_ephemeral_legacy: bool,
    #[serde(rename = "org.matrix.msc3202")]
    msc3202: bool,
    #[serde(rename = "io.element.msc4190")]
    msc4190: bool,
    #[serde(default)]
    extra: serde_json::Map<String, Value>,
}

impl From<&Registration> for RegistrationJson {
    fn from(reg: &Registration) -> Self {
        Self {
            id: reg.id.clone(),
            url: reg.url.clone(),
            as_token: reg.as_token.clone(),
            hs_token: reg.hs_token.clone(),
            sender_localpart: reg.sender_localpart.clone(),
            rate_limited: reg.rate_limited,
            namespaces: crate::namespace::NamespacesSpec::from(&reg.namespaces),
            protocols: reg.protocols.clone(),
            receive_ephemeral: reg.receive_ephemeral,
            push_ephemeral_legacy: reg.push_ephemeral_legacy,
            msc3202: reg.msc3202,
            msc4190: reg.msc4190,
            extra: reg.extra.clone(),
        }
    }
}

impl RegistrationJson {
    fn into_registration(self) -> Result<Registration, AppserviceError> {
        Ok(Registration {
            id: self.id,
            url: self.url,
            as_token: self.as_token,
            hs_token: self.hs_token,
            sender_localpart: self.sender_localpart,
            rate_limited: self.rate_limited,
            namespaces: self.namespaces.compile()?,
            protocols: self.protocols,
            receive_ephemeral: self.receive_ephemeral,
            push_ephemeral_legacy: self.push_ephemeral_legacy,
            msc3202: self.msc3202,
            msc4190: self.msc4190,
            extra: self.extra,
        })
    }
}

/// A minimal RFC 7396 JSON Merge Patch applier: merges `patch` into `target` in place. An object
/// value in `patch` merges key by key (recursing); any other value (including `null`, which
/// deletes the key) replaces the target key outright.
fn json_merge_patch(target: &mut Value, patch: &Value) {
    let Value::Object(patch_obj) = patch else {
        *target = patch.clone();
        return;
    };
    if !target.is_object() {
        *target = Value::Object(serde_json::Map::new());
    }
    let target_obj = target.as_object_mut().expect("just ensured object above");
    for (key, patch_value) in patch_obj {
        if patch_value.is_null() {
            target_obj.remove(key);
            continue;
        }
        let entry = target_obj
            .entry(key.clone())
            .or_insert(Value::Object(serde_json::Map::new()));
        json_merge_patch(entry, patch_value);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hs_auth::clock::FixedClock;
    use hs_kv::memory::MemoryBackend;
    use ruma::server_name;

    fn registry() -> Registry<MemoryBackend> {
        Registry::open(MemoryBackend::new(), server_name!("example.org"))
            .unwrap()
            .with_clock(Arc::new(FixedClock::new(1000)))
    }

    fn reg(id: &str, sender: &str, user_pattern: &str, exclusive: bool) -> Registration {
        Registration {
            id: id.to_string(),
            url: Some("http://localhost:1234".to_string()),
            as_token: format!("as_{id}"),
            hs_token: format!("hs_{id}"),
            sender_localpart: sender.to_string(),
            rate_limited: true,
            namespaces: crate::namespace::Namespaces {
                users: vec![
                    crate::namespace::NamespaceRule::compile(user_pattern, exclusive).unwrap(),
                ],
                aliases: vec![],
                rooms: vec![],
            },
            protocols: vec![],
            receive_ephemeral: false,
            push_ephemeral_legacy: false,
            msc3202: false,
            msc4190: false,
            extra: Default::default(),
        }
    }

    #[test]
    fn add_and_get_round_trip() {
        let r = registry();
        let added = r
            .add(&reg("a", "abot", r"^@a_.*:example\.org$", true))
            .unwrap();
        assert_eq!(added.id, "a");
        assert_eq!(r.get("a").unwrap().unwrap().sender_localpart, "abot");
    }

    #[test]
    fn duplicate_sender_localpart_is_a_conflict() {
        let r = registry();
        r.add(&reg("a", "bot", r"^@a_.*:example\.org$", true))
            .unwrap();
        let err = r
            .add(&reg("b", "bot", r"^@b_.*:example\.org$", true))
            .unwrap_err();
        assert!(matches!(
            err,
            AppserviceError::NamespaceConflict { kind: "sender", .. }
        ));
    }

    #[test]
    fn new_appservice_cannot_exclusively_claim_an_existing_sender() {
        let r = registry();
        r.add(&reg("a", "abot", r"^@zzz_never:example\.org$", true))
            .unwrap();
        // b's exclusive namespace happens to match a's sender.
        let err = r
            .add(&reg("b", "bbot", r"^@a.*:example\.org$", true))
            .unwrap_err();
        assert!(matches!(
            err,
            AppserviceError::NamespaceConflict { kind: "users", .. }
        ));
    }

    #[test]
    fn identical_exclusive_pattern_twice_is_a_conflict() {
        let r = registry();
        r.add(&reg("a", "abot", r"^@dup_.*:example\.org$", true))
            .unwrap();
        let err = r
            .add(&reg("b", "bbot", r"^@dup_.*:example\.org$", true))
            .unwrap_err();
        assert!(matches!(
            err,
            AppserviceError::NamespaceConflict { kind: "users", .. }
        ));
    }

    #[test]
    fn non_exclusive_namespace_never_conflicts_double_puppet_case() {
        let r = registry();
        r.add(&reg("a", "abot", r"^@a_.*:example\.org$", true))
            .unwrap();
        // A double-puppeting registration with a broad, non-exclusive namespace covering every
        // user must not conflict with anything.
        let dp = reg("a_double_puppet", "abot2", r"@.*:example\.org", false);
        r.add(&dp).unwrap();
    }

    #[test]
    fn pause_and_resume_round_trip() {
        let r = registry();
        r.add(&reg("a", "abot", r"^@a_.*:example\.org$", true))
            .unwrap();
        assert!(!r.get("a").unwrap().unwrap().paused);
        r.pause("a").unwrap();
        assert!(r.get("a").unwrap().unwrap().paused);
        assert_eq!(r.health("a").unwrap().status, HealthStatus::Paused);
        r.resume("a").unwrap();
        assert!(!r.get("a").unwrap().unwrap().paused);
    }

    #[test]
    fn rotate_tokens_changes_both_tokens() {
        let r = registry();
        let before = r
            .add(&reg("a", "abot", r"^@a_.*:example\.org$", true))
            .unwrap();
        let after = r.rotate_tokens("a").unwrap();
        assert_ne!(before.as_token, after.as_token);
        assert_ne!(before.hs_token, after.hs_token);
        assert_eq!(after.as_token.len(), 64);
    }

    #[test]
    fn import_is_idempotent_upsert_for_static_registration_files() {
        let r = registry();
        let reg1 = reg("a", "abot", r"^@a_.*:example\.org$", true);
        r.import(&reg1).unwrap();
        let mut reg2 = reg1;
        reg2.protocols = vec!["irc".to_string()];
        let row = r.import(&reg2).unwrap();
        assert_eq!(row.protocols, vec!["irc".to_string()]);
        assert_eq!(r.list().unwrap().len(), 1);
    }

    #[test]
    fn update_applies_merge_patch_and_preserves_id_and_sender() {
        let r = registry();
        r.add(&reg("a", "abot", r"^@a_.*:example\.org$", true))
            .unwrap();
        let patch = serde_json::json!({"rate_limited": false, "protocols": ["irc"]});
        let updated = r.update("a", &patch).unwrap();
        assert!(!updated.rate_limited);
        assert_eq!(updated.protocols, vec!["irc".to_string()]);
        assert_eq!(updated.sender_localpart, "abot");
    }

    #[test]
    fn remove_purges_queue_and_health() {
        let r = registry();
        r.add(&reg("a", "abot", r"^@a_.*:example\.org$", true))
            .unwrap();
        r.store().enqueue("a", serde_json::json!({}), 0).unwrap();
        r.remove("a").unwrap();
        assert!(r.get("a").unwrap().is_none());
        assert!(r.store().queue_for("a").unwrap().is_empty());
    }

    #[test]
    fn external_identity_checker_blocks_sender_collision() {
        struct AlwaysExists;
        impl ExternalIdentityChecker for AlwaysExists {
            fn user_exists(&self, _user_id: &str) -> bool {
                true
            }
        }
        let r = registry().with_identity_checker(Arc::new(AlwaysExists));
        let err = r
            .add(&reg("a", "abot", r"^@a_.*:example\.org$", true))
            .unwrap_err();
        assert!(matches!(
            err,
            AppserviceError::NamespaceConflict { kind: "sender", .. }
        ));
    }

    #[test]
    fn health_is_unknown_before_any_activity() {
        let r = registry();
        r.add(&reg("a", "abot", r"^@a_.*:example\.org$", true))
            .unwrap();
        assert_eq!(r.health("a").unwrap().status, HealthStatus::Unknown);
    }
}
