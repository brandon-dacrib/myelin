//! The operator's objects against `helm template` of `deploy/helm/hs` with
//! [`chart_values`]: every object the chart renders must be one the operator builds, equal field
//! by field once the differences [`super::objects::chart_differences`] lists are normalised
//! away.
//!
//! Needs `helm` on `PATH`. Without it the test says so and passes, unless `HS_REQUIRE_HELM` is
//! set (CI sets it, `.github/workflows/ci.yml`), in which case it fails.

use std::collections::BTreeMap;
use std::io::Write as _;
use std::process::{Command, Stdio};

use serde_json::Value;

use super::objects::{ANNOTATION_CONFIG_CHECKSUM, ANNOTATION_TEMPLATE_HASH, build, chart_values};
use super::testing::{cluster_homeserver, cnpg_homeserver, single_node_homeserver};
use crate::crds::Homeserver;

fn helm_available() -> bool {
    Command::new("helm")
        .arg("version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

fn chart_dir() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../deploy/helm/hs")
}

/// `helm template` of the chart for `hs`, keyed by `Kind/name`.
fn render_chart(hs: &Homeserver) -> BTreeMap<String, Value> {
    let values = serde_json::to_string(&chart_values(hs)).unwrap();
    let mut child = Command::new("helm")
        .args(["template", hs.metadata.name.as_deref().unwrap()])
        .arg(chart_dir())
        .args([
            "--namespace",
            hs.metadata.namespace.as_deref().unwrap(),
            "-f",
            "-",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn helm");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(values.as_bytes())
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "helm template failed: {}\nvalues: {values}",
        String::from_utf8_lossy(&output.stderr)
    );
    let text = String::from_utf8(output.stdout).unwrap();
    let mut objects = BTreeMap::new();
    for document in serde_yaml_ng::Deserializer::from_str(&text) {
        let value: Value = serde::Deserialize::deserialize(document).unwrap();
        if value.is_null() {
            continue;
        }
        objects.insert(key(&value), value);
    }
    objects
}

fn key(v: &Value) -> String {
    format!(
        "{}/{}",
        v["kind"].as_str().unwrap_or("?"),
        v["metadata"]["name"].as_str().unwrap_or("?")
    )
}

/// The operator's objects for `hs`, keyed like [`render_chart`].
fn operator_objects(hs: &Homeserver) -> BTreeMap<String, Value> {
    let objects = build(hs).unwrap();
    let mut values = vec![
        serde_json::to_value(&objects.service_account).unwrap(),
        serde_json::to_value(&objects.config_map).unwrap(),
        serde_json::to_value(&objects.service).unwrap(),
        serde_json::to_value(&objects.headless_service).unwrap(),
        serde_json::to_value(&objects.stateful_set).unwrap(),
    ];
    if let Some(pdb) = &objects.pod_disruption_budget {
        values.push(serde_json::to_value(pdb).unwrap());
    }
    values.into_iter().map(|v| (key(&v), v)).collect()
}

fn remove(map: &mut Value, pointer: &str, field: &str) {
    if let Some(Value::Object(m)) = map.pointer_mut(pointer) {
        m.remove(field);
    }
}

fn drop_if_empty(v: &mut Value, pointer: &str, field: &str) {
    let empty = v
        .pointer(&format!("{pointer}/{field}"))
        .is_some_and(|x| x.as_object().is_some_and(serde_json::Map::is_empty));
    if empty {
        remove(v, pointer, field);
    }
}

/// Normalises exactly the documented differences away.
fn normalise(mut v: Value) -> Value {
    for labels in ["/metadata/labels", "/spec/template/metadata/labels"] {
        for label in [
            "helm.sh/chart",
            "app.kubernetes.io/version",
            "app.kubernetes.io/managed-by",
        ] {
            remove(&mut v, labels, label);
        }
    }
    remove(&mut v, "/metadata", "ownerReferences");
    remove(&mut v, "/metadata", "namespace");
    remove(&mut v, "/metadata/annotations", ANNOTATION_TEMPLATE_HASH);
    drop_if_empty(&mut v, "/metadata", "annotations");
    remove(
        &mut v,
        "/spec/template/metadata/annotations",
        ANNOTATION_CONFIG_CHECKSUM,
    );
    drop_if_empty(&mut v, "/spec/template/metadata", "annotations");
    if v["kind"] == "StatefulSet" {
        remove(&mut v, "/spec", "updateStrategy");
        if let Some(Value::Array(templates)) = v.pointer_mut("/spec/volumeClaimTemplates") {
            for t in templates {
                remove(t, "/metadata", "annotations");
                // `k8s-openapi` writes a claim template's `apiVersion` and `kind`, which the
                // chart leaves implicit; the API server reads both the same.
                remove(t, "", "apiVersion");
                remove(t, "", "kind");
            }
        }
    }
    if v["kind"] == "ConfigMap"
        && let Some(Value::String(text)) = v.pointer("/data/homeserver.yaml")
    {
        let parsed: Value = serde_yaml_ng::from_str(text).unwrap();
        v["data"]["homeserver.yaml"] = parsed;
    }
    v
}

/// The first path at which two values differ, for a readable failure.
fn first_difference(a: &Value, b: &Value, path: &str) -> Option<String> {
    match (a, b) {
        (Value::Object(x), Value::Object(y)) => {
            for k in x.keys().chain(y.keys()) {
                let p = format!("{path}/{k}");
                match (x.get(k), y.get(k)) {
                    (Some(l), Some(r)) => {
                        if let Some(d) = first_difference(l, r, &p) {
                            return Some(d);
                        }
                    }
                    (l, r) => return Some(format!("{p}: chart {l:?} vs operator {r:?}")),
                }
            }
            None
        }
        (Value::Array(x), Value::Array(y)) => {
            if x.len() != y.len() {
                return Some(format!(
                    "{path}: chart has {} items, operator {}: chart {a} vs operator {b}",
                    x.len(),
                    y.len()
                ));
            }
            x.iter()
                .zip(y)
                .enumerate()
                .find_map(|(i, (l, r))| first_difference(l, r, &format!("{path}/{i}")))
        }
        (Value::Number(x), Value::Number(y)) if x.as_f64() == y.as_f64() => None,
        _ if a == b => None,
        _ => Some(format!("{path}: chart {a} vs operator {b}")),
    }
}

fn assert_equivalent(hs: &Homeserver) {
    let chart = render_chart(hs);
    let operator = operator_objects(hs);
    assert_eq!(
        chart.keys().collect::<Vec<_>>(),
        operator.keys().collect::<Vec<_>>(),
        "the chart and the operator render different objects for {}",
        hs.metadata.name.as_deref().unwrap_or("?")
    );
    for (k, chart_object) in chart {
        let chart_object = normalise(chart_object);
        let operator_object = normalise(operator[&k].clone());
        if let Some(difference) = first_difference(&chart_object, &operator_object, "") {
            panic!(
                "{k} differs at {difference}\n--- chart\n{}\n--- operator\n{}",
                serde_json::to_string_pretty(&chart_object).unwrap(),
                serde_json::to_string_pretty(&operator_object).unwrap()
            );
        }
    }
}

fn helm_or_skip() -> bool {
    if helm_available() {
        return true;
    }
    assert!(
        std::env::var_os("HS_REQUIRE_HELM").is_none(),
        "HS_REQUIRE_HELM is set but `helm` is not on PATH"
    );
    eprintln!("helm is not on PATH: skipping the chart comparison");
    false
}

#[test]
fn single_node_matches_the_chart() {
    if helm_or_skip() {
        assert_equivalent(&single_node_homeserver("hs"));
    }
}

#[test]
fn cluster_on_postgres_with_mesh_tls_and_s3_matches_the_chart() {
    if helm_or_skip() {
        assert_equivalent(&cluster_homeserver("chat", 3));
    }
}

#[test]
fn cluster_on_cloudnativepg_with_a_shared_mesh_secret_matches_the_chart() {
    if helm_or_skip() {
        assert_equivalent(&cnpg_homeserver("myelin"));
    }
}

#[test]
fn a_one_replica_cluster_without_anti_affinity_matches_the_chart() {
    if helm_or_skip() {
        let mut hs = cluster_homeserver("solo", 1);
        hs.spec.cluster.anti_affinity = crate::crds::AntiAffinity::None;
        hs.spec.storage.backend = crate::crds::StorageBackend::Slatedb;
        hs.spec.storage.slatedb = Some(crate::crds::SlatedbStorageSpec {
            bucket_url: "s3://data/myelin".to_owned(),
            shard_count: 128,
        });
        assert_equivalent(&hs);
    }
}

#[test]
fn normalising_keeps_real_differences() {
    let a = serde_json::json!({"kind": "Service", "metadata": {"name": "x", "labels": {"helm.sh/chart": "hs-0.1.0", "a": "1"}}});
    let b = serde_json::json!({"kind": "Service", "metadata": {"name": "x", "labels": {"a": "2"}}});
    let d = first_difference(&normalise(a), &normalise(b), "").unwrap();
    assert!(d.starts_with("/metadata/labels/a"), "{d}");
}
