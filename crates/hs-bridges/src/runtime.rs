//! Where instances run (RFC 0017 section 4.3). [`Runtime`] is the seam: the Kubernetes
//! implementation lives with the binary (it needs `hs-operator`'s client), and without one an
//! offering's instances run elsewhere, from their files.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use async_trait::async_trait;
use hs_admin::model::{BridgeDeployment, BridgeDeploymentTarget};

/// One instance's deployment, as the runtime is asked for it.
#[derive(Clone)]
pub struct DeploySpec {
    /// The `Bridge`, Deployment and Service name.
    pub name: String,
    pub labels: BTreeMap<String, String>,
    pub bridge_type: String,
    pub appservice_id: String,
    /// The owner's Matrix ID; `None` for a shared instance. The `Bridge` carries it so that the
    /// operator labels every object with `myelin.dev/owner`.
    pub owner: Option<String>,
    pub image_repository: String,
    pub image_tag: String,
    pub port: u16,
    pub args: Vec<String>,
    /// File name in `/data` -> contents.
    pub files: BTreeMap<String, String>,
    pub storage_size: Option<String>,
}

impl std::fmt::Debug for DeploySpec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DeploySpec")
            .field("name", &self.name)
            .field("bridge_type", &self.bridge_type)
            .field("appservice_id", &self.appservice_id)
            .finish_non_exhaustive()
    }
}

/// Runs instances somewhere.
#[async_trait]
pub trait Runtime: Send + Sync + 'static {
    /// Where, as the admin API reports it.
    fn target(&self) -> BridgeDeploymentTarget;
    /// Where this server will reach a deployment named `name` listening on `port`.
    fn service_url(&self, name: &str, port: u16) -> String;
    /// Creates or updates the deployment; idempotent.
    async fn apply(&self, spec: &DeploySpec) -> Result<BridgeDeployment, String>;
    /// Its state, or `None` if there is no such deployment.
    async fn status(&self, name: &str) -> Result<Option<BridgeDeployment>, String>;
    /// Removes it, and everything it owns; a missing one is not an error.
    async fn delete(&self, name: &str) -> Result<(), String>;
}

/// A Secret holding the files and a `Bridge` resource running them, for `kubectl apply` on a
/// cluster that runs the Myelin operator.
#[must_use]
pub fn manifest_yaml(spec: &DeploySpec, namespace: &str) -> String {
    let mut out = String::new();
    let _ = writeln!(
        out,
        "# {} for {}, for a cluster running the Myelin operator (RFC 0017).",
        spec.bridge_type, spec.appservice_id
    );
    out.push_str("# The Secret carries the bridge's tokens: keep this file private.\n");
    out.push_str("apiVersion: v1\nkind: Secret\nmetadata:\n");
    let _ = writeln!(out, "  name: {}-files\n  namespace: {namespace}", spec.name);
    out.push_str("type: Opaque\nstringData:\n");
    for (file, contents) in &spec.files {
        let _ = writeln!(out, "  {file}: |");
        for line in contents.lines() {
            let _ = writeln!(out, "    {line}");
        }
    }
    out.push_str("---\napiVersion: hs.matrix.org/v1alpha1\nkind: Bridge\nmetadata:\n");
    let _ = writeln!(out, "  name: {}\n  namespace: {namespace}", spec.name);
    if !spec.labels.is_empty() {
        out.push_str("  labels:\n");
        for (k, v) in &spec.labels {
            let _ = writeln!(out, "    {k}: {}", yaml_str(v));
        }
    }
    out.push_str("spec:\n");
    let _ = writeln!(out, "  bridgeType: {}", spec.bridge_type);
    let _ = writeln!(out, "  appserviceId: {}", spec.appservice_id);
    if let Some(owner) = &spec.owner {
        let _ = writeln!(out, "  owner: {}", yaml_str(owner));
    }
    let _ = writeln!(
        out,
        "  image:\n    repository: {}\n    tag: {}",
        spec.image_repository,
        yaml_str(&spec.image_tag)
    );
    let _ = writeln!(out, "  port: {}", spec.port);
    let _ = writeln!(out, "  filesSecret: {}-files", spec.name);
    if !spec.args.is_empty() {
        out.push_str("  args:\n");
        for a in &spec.args {
            let _ = writeln!(out, "    - {}", yaml_str(a));
        }
    }
    if let Some(size) = &spec.storage_size {
        let _ = writeln!(out, "  storage:\n    size: {size}");
    }
    out
}

fn yaml_str(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_manifest_is_a_secret_and_a_bridge() {
        let spec = DeploySpec {
            name: "bridge-1a2b3c4d".into(),
            labels: BTreeMap::from([("myelin.dev/appservice-id".into(), "whatsapp-alice".into())]),
            bridge_type: "mautrix-whatsapp".into(),
            appservice_id: "whatsapp-alice".into(),
            owner: Some("@alice:example.org".into()),
            image_repository: "dock.mau.dev/mautrix/whatsapp".into(),
            image_tag: "latest".into(),
            port: 29318,
            args: vec!["-o".into(), "@a:x".into()],
            files: BTreeMap::from([
                ("config.yaml".into(), "a: 1\nb:\n  c: \"d\"\n".into()),
                ("registration.yaml".into(), "id: whatsapp-alice\n".into()),
            ]),
            storage_size: Some("1Gi".into()),
        };
        let yaml = manifest_yaml(&spec, "myelin");
        let docs: Vec<serde_json::Value> = yaml
            .split("\n---\n")
            .map(|d| serde_yaml_ng::from_str(d).unwrap())
            .collect();
        assert_eq!(docs.len(), 2);
        assert_eq!(docs[0]["kind"], "Secret");
        assert_eq!(
            docs[0]["stringData"]["config.yaml"],
            "a: 1\nb:\n  c: \"d\"\n"
        );
        assert_eq!(docs[1]["spec"]["filesSecret"], docs[0]["metadata"]["name"]);
        assert_eq!(docs[1]["spec"]["args"][1], "@a:x");
        assert_eq!(docs[1]["spec"]["port"], 29318);
        assert_eq!(docs[1]["spec"]["owner"], "@alice:example.org");

        let shared = DeploySpec {
            owner: None,
            ..spec
        };
        let yaml = manifest_yaml(&shared, "myelin");
        assert!(!yaml.contains("owner:"), "{yaml}");
    }
}
