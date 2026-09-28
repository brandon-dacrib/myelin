//! [`HomeserverKube`] over a real `kube::Client`: server-side apply under the operator's field
//! manager, the status sub-resource, the finalizer, and Kubernetes Events on the `Homeserver`.

use k8s_openapi::api::apps::v1::StatefulSet;
use k8s_openapi::api::core::v1::{ConfigMap, Pod, Secret, Service, ServiceAccount};
use k8s_openapi::api::policy::v1::PodDisruptionBudget;
use kube::api::{DeleteParams, ListParams, Patch, PatchParams};
use kube::runtime::events::{Event, EventType, Recorder, Reporter};
use kube::{Api, Client, Resource as _, ResourceExt as _};
use serde::Serialize;
use serde::de::DeserializeOwned;

use super::objects::DesiredObjects;
use super::reconciler::{FINALIZER, HomeserverKube, Note};
use crate::bridge::MANAGER;
use crate::crds::{Homeserver, HomeserverStatus, SecretKeyRef};

/// [`HomeserverKube`] over a `kube::Client`.
#[derive(Clone)]
pub struct KubeHomeserverOps {
    client: Client,
    recorder: Recorder,
}

impl std::fmt::Debug for KubeHomeserverOps {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KubeHomeserverOps").finish_non_exhaustive()
    }
}

impl KubeHomeserverOps {
    /// Events are reported as controller [`MANAGER`], instance `POD_NAME` when set.
    #[must_use]
    pub fn new(client: Client) -> Self {
        let reporter = Reporter {
            controller: MANAGER.to_owned(),
            instance: std::env::var("POD_NAME").ok(),
        };
        Self {
            recorder: Recorder::new(client.clone(), reporter),
            client,
        }
    }

    async fn apply_one<K>(&self, namespace: &str, object: &K) -> Result<(), kube::Error>
    where
        K: kube::Resource<Scope = k8s_openapi::NamespaceResourceScope>
            + Clone
            + DeserializeOwned
            + Serialize
            + std::fmt::Debug,
        K::DynamicType: Default,
    {
        let api: Api<K> = Api::namespaced(self.client.clone(), namespace);
        let name = object.meta().name.clone().unwrap_or_default();
        api.patch(
            &name,
            &PatchParams::apply(MANAGER).force(),
            &Patch::Apply(object),
        )
        .await?;
        Ok(())
    }
}

/// The status as a merge patch that also clears what is gone: `drain` as `null` and
/// `pendingUndrains` as `[]` when empty, which plain serialization would omit (and a merge
/// patch would then leave as it was).
#[must_use]
pub fn status_patch(status: &HomeserverStatus) -> serde_json::Value {
    let mut value = serde_json::to_value(status).unwrap_or_default();
    if let Some(map) = value.as_object_mut() {
        map.entry("drain").or_insert(serde_json::Value::Null);
        map.entry("pendingUndrains")
            .or_insert_with(|| serde_json::Value::Array(Vec::new()));
    }
    serde_json::json!({ "status": value })
}

impl HomeserverKube for KubeHomeserverOps {
    async fn get_stateful_set(
        &self,
        namespace: &str,
        name: &str,
    ) -> Result<Option<StatefulSet>, kube::Error> {
        Api::<StatefulSet>::namespaced(self.client.clone(), namespace)
            .get_opt(name)
            .await
    }

    async fn list_pods(&self, namespace: &str, selector: &str) -> Result<Vec<Pod>, kube::Error> {
        Ok(Api::<Pod>::namespaced(self.client.clone(), namespace)
            .list(&ListParams::default().labels(selector))
            .await?
            .items)
    }

    async fn secret_value(
        &self,
        namespace: &str,
        secret: &SecretKeyRef,
    ) -> Result<Option<String>, kube::Error> {
        let found = Api::<Secret>::namespaced(self.client.clone(), namespace)
            .get_opt(&secret.name)
            .await?;
        Ok(found.and_then(|s| {
            s.data
                .and_then(|d| d.get(&secret.key).cloned())
                .and_then(|b| String::from_utf8(b.0).ok())
                .or_else(|| s.string_data.and_then(|d| d.get(&secret.key).cloned()))
        }))
    }

    async fn apply(&self, namespace: &str, objects: &DesiredObjects) -> Result<(), kube::Error> {
        self.apply_one::<ServiceAccount>(namespace, &objects.service_account)
            .await?;
        self.apply_one::<ConfigMap>(namespace, &objects.config_map)
            .await?;
        self.apply_one::<Service>(namespace, &objects.headless_service)
            .await?;
        self.apply_one::<Service>(namespace, &objects.service)
            .await?;
        self.apply_one::<StatefulSet>(namespace, &objects.stateful_set)
            .await?;
        match &objects.pod_disruption_budget {
            Some(pdb) => {
                self.apply_one::<PodDisruptionBudget>(namespace, pdb)
                    .await?
            }
            None => {
                let name = objects.stateful_set.name_any();
                let api: Api<PodDisruptionBudget> = Api::namespaced(self.client.clone(), namespace);
                match api.delete(&name, &DeleteParams::default()).await {
                    Ok(_) => {}
                    Err(kube::Error::Api(e)) if e.code == 404 => {}
                    Err(e) => return Err(e),
                }
            }
        }
        Ok(())
    }

    async fn patch_status(
        &self,
        namespace: &str,
        name: &str,
        status: &HomeserverStatus,
    ) -> Result<(), kube::Error> {
        Api::<Homeserver>::namespaced(self.client.clone(), namespace)
            .patch_status(
                name,
                &PatchParams::default(),
                &Patch::Merge(status_patch(status)),
            )
            .await?;
        Ok(())
    }

    async fn set_finalizer(&self, hs: &Homeserver, present: bool) -> Result<(), kube::Error> {
        let mut finalizers: Vec<String> = hs
            .finalizers()
            .iter()
            .filter(|f| *f != FINALIZER)
            .cloned()
            .collect();
        if present {
            finalizers.push(FINALIZER.to_owned());
        }
        let namespace = hs.namespace().unwrap_or_default();
        Api::<Homeserver>::namespaced(self.client.clone(), &namespace)
            .patch(
                &hs.name_any(),
                &PatchParams::default(),
                &Patch::Merge(serde_json::json!({
                    "metadata": {
                        "finalizers": finalizers,
                        "resourceVersion": hs.resource_version(),
                    }
                })),
            )
            .await?;
        Ok(())
    }

    async fn publish(&self, hs: &Homeserver, note: &Note) {
        let event = Event {
            type_: if note.warning {
                EventType::Warning
            } else {
                EventType::Normal
            },
            reason: note.reason.to_owned(),
            note: Some(note.message.clone()),
            action: "Reconcile".to_owned(),
            secondary: None,
        };
        if let Err(e) = self.recorder.publish(&event, &hs.object_ref(&())).await {
            tracing::warn!(homeserver = %hs.name_any(), error = %e, reason = note.reason, "could not publish an event");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_status_patch_clears_a_finished_drain() {
        let patch = status_patch(&HomeserverStatus::default());
        assert!(patch["status"]["drain"].is_null());
        assert_eq!(patch["status"]["pendingUndrains"], serde_json::json!([]));
        assert!(patch["status"].get("drain").is_some());
    }
}
