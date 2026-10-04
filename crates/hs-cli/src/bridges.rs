//! Connects the bridge manager's runtime to the operator's Kubernetes client (RFC 0017), and
//! reads the offerings the deployment declares.

use std::sync::Arc;

use async_trait::async_trait;
use hs_admin::model::{BridgeDeployment, BridgeDeploymentTarget, BridgeOfferingRequest};
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

/// The environment variable the chart's `bridges.offerings` arrives in: a JSON list of
/// `{"type": <catalogue id>, ...}` where the rest is a `BridgeOfferingRequest` (`runtime`,
/// `image_tag`, `access`, `options`; absent fields take the defaults a `PUT` would).
pub const DECLARED_OFFERINGS_ENV: &str = "MYELIN_BRIDGES_OFFERINGS";

/// The offerings the deployment declares ([`DECLARED_OFFERINGS_ENV`]), for
/// `hs_bridges::manager::BridgeManager::set_declared`: each is created the first time the
/// server runs with it declared, then left to the admin API. Unset or blank: none.
///
/// # Errors
/// When the value is not a JSON list of objects naming a `type` the catalogue has: a
/// deployment that declares a bridge it misspells should not boot as if it declared nothing.
pub fn declared_offerings() -> Result<Vec<(String, BridgeOfferingRequest)>, String> {
    parse_declared_offerings(std::env::var(DECLARED_OFFERINGS_ENV).ok().as_deref())
}

fn parse_declared_offerings(
    value: Option<&str>,
) -> Result<Vec<(String, BridgeOfferingRequest)>, String> {
    let Some(value) = value.map(str::trim).filter(|v| !v.is_empty()) else {
        return Ok(Vec::new());
    };
    let items: Vec<serde_json::Map<String, serde_json::Value>> = serde_json::from_str(value)
        .map_err(|e| format!("{DECLARED_OFFERINGS_ENV} is not a JSON list of objects: {e}"))?;
    let mut declared = Vec::new();
    for mut item in items {
        let bridge_type = item
            .remove("type")
            .and_then(|t| t.as_str().map(str::to_owned))
            .ok_or_else(|| format!("{DECLARED_OFFERINGS_ENV}: every entry needs a \"type\""))?;
        if hs_admin::bridge_types::get(&bridge_type, "example.org").is_none() {
            return Err(format!(
                "{DECLARED_OFFERINGS_ENV}: {bridge_type} is not a bridge type in the catalogue"
            ));
        }
        let request: BridgeOfferingRequest =
            serde_json::from_value(serde_json::Value::Object(item)).map_err(|e| {
                format!("{DECLARED_OFFERINGS_ENV}: {bridge_type}'s settings do not parse: {e}")
            })?;
        if declared.iter().any(|(t, _)| t == &bridge_type) {
            return Err(format!("{DECLARED_OFFERINGS_ENV}: {bridge_type} is declared twice"));
        }
        declared.push((bridge_type, request));
    }
    Ok(declared)
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
                owner: spec.owner.clone(),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn declared_offerings_parse_as_the_put_body_with_a_type() {
        assert!(parse_declared_offerings(None).unwrap().is_empty());
        assert!(parse_declared_offerings(Some("  ")).unwrap().is_empty());
        let declared = parse_declared_offerings(Some(
            r#"[{"type": "mautrix-whatsapp", "runtime": "cluster", "options": {"double_puppeting": true}},
                {"type": "heisenbridge"}]"#,
        ))
        .unwrap();
        assert_eq!(declared.len(), 2);
        assert_eq!(declared[0].0, "mautrix-whatsapp");
        assert_eq!(declared[0].1.runtime.as_deref(), Some("cluster"));
        assert_eq!(
            declared[0].1.options.as_ref().and_then(|o| o.double_puppeting),
            Some(true)
        );
        assert_eq!(declared[1].0, "heisenbridge");
        assert_eq!(declared[1].1, BridgeOfferingRequest::default());
    }

    #[test]
    fn a_misspelt_or_malformed_declaration_is_refused() {
        let unknown = parse_declared_offerings(Some(r#"[{"type": "mautrix-whatsap"}]"#)).unwrap_err();
        assert!(unknown.contains("not a bridge type in the catalogue"), "{unknown}");
        let untyped = parse_declared_offerings(Some(r#"[{"runtime": "cluster"}]"#)).unwrap_err();
        assert!(untyped.contains("needs a \"type\""), "{untyped}");
        let twice = parse_declared_offerings(Some(
            r#"[{"type": "heisenbridge"}, {"type": "heisenbridge"}]"#,
        ))
        .unwrap_err();
        assert!(twice.contains("declared twice"), "{twice}");
        let not_a_list = parse_declared_offerings(Some(r#"{"type": "heisenbridge"}"#)).unwrap_err();
        assert!(not_a_list.contains("not a JSON list"), "{not_a_list}");
        let bad_field = parse_declared_offerings(Some(r#"[{"type": "heisenbridge", "enabled": "yes"}]"#))
            .unwrap_err();
        assert!(bad_field.contains("do not parse"), "{bad_field}");
    }
}
