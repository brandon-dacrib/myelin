//! Connects the bridge manager's runtime to the operator's Kubernetes client (RFC 0017).

use std::sync::Arc;

use async_trait::async_trait;
use hs_admin::model::{BridgeDeployment, BridgeDeploymentTarget};
use hs_bridges::runtime::{DeploySpec, Runtime};
use hs_operator::crds::{ImageSpec, Phase};
use hs_operator::deploy::{BridgeInstanceSpec, BridgeInstanceStatus, KubeBridgeClient};

struct KubernetesRuntime {
    client: KubeBridgeClient,
    homeserver_url: String,
}

/// Builds the runtime when both chart-provided environment variables are present.
///
/// # Errors
/// If deployment was configured incompletely, its URL is invalid, or Kubernetes configuration
/// cannot be loaded. An explicitly configured runtime must not silently fall back to manual setup.
pub async fn runtime() -> Result<Option<Arc<dyn Runtime>>, String> {
    let namespace = std::env::var("MYELIN_BRIDGES_NAMESPACE").ok();
    let homeserver_url = std::env::var("MYELIN_BRIDGES_HOMESERVER_URL").ok();
    match (namespace, homeserver_url) {
        (None, None) => Ok(None),
        (Some(namespace), Some(homeserver_url)) if !namespace.trim().is_empty() => {
            let url = reqwest::Url::parse(&homeserver_url).map_err(|e| e.to_string())?;
            if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
                return Err("MYELIN_BRIDGES_HOMESERVER_URL must be an HTTP(S) URL".into());
            }
            let client = KubeBridgeClient::in_cluster(namespace)
                .await
                .map_err(|e| e.to_string())?;
            Ok(Some(Arc::new(KubernetesRuntime {
                client,
                homeserver_url,
            })))
        }
        _ => Err("set both MYELIN_BRIDGES_NAMESPACE and MYELIN_BRIDGES_HOMESERVER_URL".into()),
    }
}

fn deployment(status: BridgeInstanceStatus) -> BridgeDeployment {
    BridgeDeployment {
        namespace: status.namespace,
        name: status.name,
        image: status.image,
        service_url: status.service_url,
        phase: match status.phase {
            Phase::Pending => "Pending",
            Phase::Ready => "Ready",
            Phase::Degraded => "Degraded",
        }
        .into(),
        ready: status.ready,
        message: status.message,
    }
}

#[async_trait]
impl Runtime for KubernetesRuntime {
    fn target(&self) -> BridgeDeploymentTarget {
        BridgeDeploymentTarget {
            available: true,
            namespace: Some(self.client.namespace().into()),
            homeserver_url: Some(self.homeserver_url.clone()),
            reason: None,
        }
    }

    fn service_url(&self, name: &str, port: u16) -> String {
        self.client.service_url(name, i32::from(port))
    }

    async fn apply(&self, spec: &DeploySpec) -> Result<BridgeDeployment, String> {
        self.client
            .apply(&BridgeInstanceSpec {
                name: spec.name.clone(),
                labels: spec.labels.clone(),
                bridge_type: spec.bridge_type.clone(),
                appservice_id: spec.appservice_id.clone(),
                image: ImageSpec {
                    repository: spec.image_repository.clone(),
                    tag: Some(spec.image_tag.clone()),
                    digest: None,
                    pull_policy: None,
                },
                port: i32::from(spec.port),
                args: spec.args.clone(),
                files: spec.files.clone(),
                storage_size: spec.storage_size.clone(),
            })
            .await
            .map(deployment)
            .map_err(|e| e.to_string())
    }

    async fn status(&self, name: &str) -> Result<Option<BridgeDeployment>, String> {
        self.client
            .status(name)
            .await
            .map(|s| s.map(deployment))
            .map_err(|e| e.to_string())
    }

    async fn delete(&self, name: &str) -> Result<(), String> {
        self.client.delete(name).await.map_err(|e| e.to_string())
    }
}
