//! The data source behind the `bridge_deployments.*`, `bridge_offerings.*` and
//! `bridge_instances.*` operations (RFC 0017): bridge types switched on for this server, and the
//! per-user instances of each. Implemented for real by `hs-bridges`' manager; by
//! [`InMemoryBridgeOfferings`] for this crate's tests and `hs-admin-mock`.

use std::collections::BTreeMap;
use std::sync::RwLock;

use async_trait::async_trait;

use crate::model::{
    BridgeDeploymentTarget, BridgeInstance, BridgeInstanceFiles, BridgeOffering,
    BridgeOfferingRequest,
};
use crate::sources::SourceError;

/// The user id a shared type's one instance is addressed by in paths.
pub const SHARED_INSTANCE: &str = "_";

/// Offerings and their instances. Every method naming a type that is not offered answers
/// [`SourceError::NotFound`], except [`BridgeOfferingSource::put`], which creates it (and
/// answers [`SourceError::NotFound`] only for a type the catalogue does not have).
#[async_trait]
pub trait BridgeOfferingSource: Send + Sync + 'static {
    /// Whether this server can deploy instances itself, and where.
    async fn target(&self) -> BridgeDeploymentTarget;
    async fn list(&self) -> Result<Vec<BridgeOffering>, SourceError>;
    async fn get(&self, bridge_type: &str) -> Result<Option<BridgeOffering>, SourceError>;
    /// Creates or changes an offering, registering its front door.
    async fn put(
        &self,
        bridge_type: &str,
        request: BridgeOfferingRequest,
    ) -> Result<BridgeOffering, SourceError>;
    /// [`SourceError::Conflict`] while instances remain, unless `remove_instances`.
    async fn delete(&self, bridge_type: &str, remove_instances: bool) -> Result<(), SourceError>;
    async fn instances(&self, bridge_type: &str) -> Result<Vec<BridgeInstance>, SourceError>;
    async fn instance(
        &self,
        bridge_type: &str,
        user_id: &str,
    ) -> Result<Option<BridgeInstance>, SourceError>;
    /// Starts provisioning an instance for `user_id`; idempotent, and retries a failed one.
    async fn put_instance(
        &self,
        bridge_type: &str,
        user_id: &str,
    ) -> Result<BridgeInstance, SourceError>;
    async fn delete_instance(&self, bridge_type: &str, user_id: &str) -> Result<(), SourceError>;
    /// The files to run an instance elsewhere, with its own tokens.
    async fn instance_files(
        &self,
        bridge_type: &str,
        user_id: &str,
    ) -> Result<BridgeInstanceFiles, SourceError>;
}

/// A [`BridgeOfferingSource`] held in memory: offerings are stored as given, instances are
/// created `requested` and stay there. For tests and the mock server.
#[derive(Debug, Default)]
pub struct InMemoryBridgeOfferings {
    target: BridgeDeploymentTarget,
    offerings: RwLock<BTreeMap<String, BridgeOffering>>,
    instances: RwLock<BTreeMap<(String, String), BridgeInstance>>,
}

impl InMemoryBridgeOfferings {
    /// With deployment available into `namespace`.
    #[must_use]
    pub fn in_cluster(namespace: &str) -> Self {
        Self {
            target: BridgeDeploymentTarget {
                available: true,
                namespace: Some(namespace.to_owned()),
                homeserver_url: Some(format!("http://myelin.{namespace}.svc:8008")),
                reason: None,
            },
            ..Self::default()
        }
    }

    fn counts(&self, bridge_type: &str) -> BTreeMap<String, u64> {
        let mut out = BTreeMap::new();
        for ((t, _), i) in self.instances.read().expect("lock").iter() {
            if t == bridge_type {
                *out.entry(i.state.clone()).or_insert(0) += 1;
            }
        }
        out
    }

    fn with_counts(&self, mut offering: BridgeOffering) -> BridgeOffering {
        offering.instances = self.counts(&offering.bridge_type);
        offering
    }
}

#[async_trait]
impl BridgeOfferingSource for InMemoryBridgeOfferings {
    async fn target(&self) -> BridgeDeploymentTarget {
        self.target.clone()
    }

    async fn list(&self) -> Result<Vec<BridgeOffering>, SourceError> {
        let offerings: Vec<_> = self
            .offerings
            .read()
            .expect("lock")
            .values()
            .cloned()
            .collect();
        Ok(offerings.into_iter().map(|o| self.with_counts(o)).collect())
    }

    async fn get(&self, bridge_type: &str) -> Result<Option<BridgeOffering>, SourceError> {
        let offering = self
            .offerings
            .read()
            .expect("lock")
            .get(bridge_type)
            .cloned();
        Ok(offering.map(|o| self.with_counts(o)))
    }

    async fn put(
        &self,
        bridge_type: &str,
        request: BridgeOfferingRequest,
    ) -> Result<BridgeOffering, SourceError> {
        let Some(kind) = crate::bridge_types::get(bridge_type, "example.org") else {
            return Err(SourceError::NotFound);
        };
        let runtime = request.runtime.clone().unwrap_or_else(|| "cluster".into());
        if runtime == "cluster" && !self.target.available {
            return Err(SourceError::InvalidField {
                pointer: "/runtime",
                detail: "this server cannot deploy bridges".into(),
            });
        }
        let mut offerings = self.offerings.write().expect("lock");
        let current = offerings.get(bridge_type).cloned();
        let offering = BridgeOffering {
            bridge_type: bridge_type.to_owned(),
            name: kind.name.clone(),
            mode: kind.mode.clone(),
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
                .unwrap_or_else(|| {
                    kind.image
                        .rsplit_once(':')
                        .map_or("latest", |(_, t)| t)
                        .to_owned()
                }),
            image: kind.image.clone(),
            front_door: (kind.mode == "per_user").then(|| {
                kind.default_namespaces["users"][1]["regex"]
                    .as_str()
                    .unwrap_or_default()
                    .replace('\\', "")
            }),
            access: request
                .access
                .or(current.as_ref().map(|c| c.access.clone()))
                .unwrap_or_default(),
            options: request
                .options
                .or(current.as_ref().map(|c| c.options.clone()))
                .unwrap_or_default(),
            instances: BTreeMap::new(),
            created_at: current
                .map(|c| c.created_at)
                .unwrap_or_else(|| "2026-09-26T00:00:00.000Z".into()),
            overlapping_appservices: Vec::new(),
        };
        offerings.insert(bridge_type.to_owned(), offering.clone());
        drop(offerings);
        Ok(self.with_counts(offering))
    }

    async fn delete(&self, bridge_type: &str, remove_instances: bool) -> Result<(), SourceError> {
        if self.get(bridge_type).await?.is_none() {
            return Err(SourceError::NotFound);
        }
        let mut instances = self.instances.write().expect("lock");
        let has = instances.keys().any(|(t, _)| t == bridge_type);
        if has && !remove_instances {
            return Err(SourceError::Conflict(format!(
                "{bridge_type} still has instances"
            )));
        }
        instances.retain(|(t, _), _| t != bridge_type);
        drop(instances);
        self.offerings.write().expect("lock").remove(bridge_type);
        Ok(())
    }

    async fn instances(&self, bridge_type: &str) -> Result<Vec<BridgeInstance>, SourceError> {
        if self.get(bridge_type).await?.is_none() {
            return Err(SourceError::NotFound);
        }
        Ok(self
            .instances
            .read()
            .expect("lock")
            .iter()
            .filter(|((t, _), _)| t == bridge_type)
            .map(|(_, i)| i.clone())
            .collect())
    }

    async fn instance(
        &self,
        bridge_type: &str,
        user_id: &str,
    ) -> Result<Option<BridgeInstance>, SourceError> {
        Ok(self
            .instances
            .read()
            .expect("lock")
            .get(&(bridge_type.to_owned(), user_id.to_owned()))
            .cloned())
    }

    async fn put_instance(
        &self,
        bridge_type: &str,
        user_id: &str,
    ) -> Result<BridgeInstance, SourceError> {
        if self.get(bridge_type).await?.is_none() {
            return Err(SourceError::NotFound);
        }
        let mut instances = self.instances.write().expect("lock");
        let instance = instances
            .entry((bridge_type.to_owned(), user_id.to_owned()))
            .or_insert_with(|| BridgeInstance {
                bridge_type: bridge_type.to_owned(),
                user_id: (user_id != SHARED_INSTANCE).then(|| user_id.to_owned()),
                state: "requested".into(),
                created_at: "2026-09-26T00:00:00.000Z".into(),
                ..BridgeInstance::default()
            });
        Ok(instance.clone())
    }

    async fn delete_instance(&self, bridge_type: &str, user_id: &str) -> Result<(), SourceError> {
        self.instances
            .write()
            .expect("lock")
            .remove(&(bridge_type.to_owned(), user_id.to_owned()))
            .map(|_| ())
            .ok_or(SourceError::NotFound)
    }

    async fn instance_files(
        &self,
        bridge_type: &str,
        user_id: &str,
    ) -> Result<BridgeInstanceFiles, SourceError> {
        if self.instance(bridge_type, user_id).await?.is_none() {
            return Err(SourceError::NotFound);
        }
        Ok(BridgeInstanceFiles {
            config_yaml: Some("homeserver:\n  domain: example.org\n".into()),
            registration_yaml: format!("id: {bridge_type}\n"),
            compose_yaml: "services: {}\n".into(),
            manifest_yaml: "kind: Bridge\n".into(),
        })
    }
}
