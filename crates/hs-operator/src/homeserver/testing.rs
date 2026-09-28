//! Test fixtures: `Homeserver` resources in the shapes the tests need.

use kube::Resource as _;

use crate::crds::{Homeserver, HomeserverSpec};

fn homeserver(name: &str, spec: serde_json::Value) -> Homeserver {
    let spec: HomeserverSpec = serde_json::from_value(spec).expect("fixture spec");
    let mut hs = Homeserver::new(name, spec);
    let meta = hs.meta_mut();
    meta.namespace = Some("matrix".to_owned());
    meta.uid = Some(format!("uid-{name}"));
    meta.generation = Some(1);
    hs
}

/// Embedded storage, the chart's `singleNode` mode.
pub(crate) fn single_node_homeserver(name: &str) -> Homeserver {
    homeserver(
        name,
        serde_json::json!({
            "serverName": "example.org",
            "image": {"repository": "ghcr.io/brandon-dacrib/myelin", "tag": "sha-abc"},
            "storage": {"backend": "embedded", "embedded": {"size": "10Gi"}},
            "signingKeySecretRef": {"name": "hs-signing-key", "key": "signing.key"},
        }),
    )
}

/// PostgreSQL on an external host, mesh TLS, S3 media, an admin API token: the cluster mode
/// the drain tests run.
pub(crate) fn cluster_homeserver(name: &str, replicas: i32) -> Homeserver {
    homeserver(
        name,
        serde_json::json!({
            "serverName": "example.org",
            "publicBaseUrl": "https://matrix.example.org",
            "replicas": replicas,
            "image": {"repository": "ghcr.io/brandon-dacrib/myelin", "tag": "sha-abc"},
            "storage": {"backend": "postgres", "postgres": {
                "host": "db.matrix.svc", "database": "myelin", "user": "myelin",
                "passwordSecretRef": {"name": "db", "key": "password"},
            }},
            "signingKeySecretRef": {"name": "hs-signing-key", "key": "signing.key"},
            "registrationSharedSecretRef": {"name": "hs-registration", "key": "registration-shared-secret"},
            "cluster": {"meshTlsSecret": "hs-mesh-tls"},
            "media": {"backend": "s3", "s3": {
                "bucket": "media", "endpoint": "http://s3:9000", "region": "us-east-1",
                "accessKeyId": "myelin",
                "secretAccessKeyRef": {"name": "hs-media-s3", "key": "secret-access-key"},
            }},
            "adminApi": {"tokenSecretRef": {"name": "hs-admin", "key": "token"}},
        }),
    )
}

/// CloudNativePG, a shared mesh secret, a local ReadWriteMany media claim, a session secret,
/// hard anti-affinity, custom resources and extra configuration.
pub(crate) fn cnpg_homeserver(name: &str) -> Homeserver {
    homeserver(
        name,
        serde_json::json!({
            "serverName": "chat.example.org",
            "replicas": 2,
            "image": {"repository": "ghcr.io/brandon-dacrib/myelin", "digest": "sha256:0123456789abcdef"},
            "storage": {"backend": "postgres", "postgres": {"cloudNativePgCluster": "pg"}},
            "signingKeySecretRef": {"name": "hs-signing-key", "key": "signing.key"},
            "sessionSecretRef": {"name": "hs-session", "key": "session-secret"},
            "cluster": {
                "meshSharedSecretRef": {"name": "hs-mesh", "key": "mesh-shared-secret"},
                "antiAffinity": "Hard",
                "roomShards": 64,
                "leaseTtl": "15s",
            },
            "media": {"backend": "local", "localClaim": "media-rwx"},
            "resources": {"requests": {"cpu": "250m", "memory": "512Mi"}, "limits": {"memory": "2Gi"}},
            "extraConfig": {"federation": {"enabled": false}},
        }),
    )
}
