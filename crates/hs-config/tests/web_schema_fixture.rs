//! The management interface's copy of the configuration schema is the schema.
//!
//! `web/src/test/fixtures/hs-config-schema.json` is what the web's "every setting has a real
//! control" test walks (`web/src/lib/config-model.real-schema.test.ts`). It is meant to be
//! `hs_config::schema::json_schema()` verbatim (the derived schema plus each setting's
//! `x-applies`) -- exactly what `GET /api/v1/config/schema`
//! serves as its `schema` member -- and it drifted once already: a setting added here
//! (`media.scanning.icap.preview`) reached the server without the web's test ever seeing it, so a
//! shape the interface could not render went unnoticed. This test fails the moment the two
//! differ.
//!
//! To regenerate the fixture after changing a configuration type:
//!
//! ```sh
//! HS_UPDATE_WEB_SCHEMA_FIXTURE=1 cargo test -p hs-config --test web_schema_fixture
//! ```
//!
//! and then run `npm run check` in `web/`, which tells you whether the interface can render
//! whatever changed.

use std::path::PathBuf;

fn fixture_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../web/src/test/fixtures/hs-config-schema.json")
}

fn current_schema() -> serde_json::Value {
    // The derived schema with each setting's `x-applies` (`hs_config::schema`), which is what the
    // admin API serves.
    hs_config::schema::json_schema().clone()
}

#[test]
fn the_web_fixture_is_the_configuration_schema() {
    let path = fixture_path();
    let schema = current_schema();
    if std::env::var_os("HS_UPDATE_WEB_SCHEMA_FIXTURE").is_some() {
        let mut text = serde_json::to_string_pretty(&schema).expect("serializable");
        text.push('\n');
        std::fs::write(&path, text).expect("the fixture should be writable");
        return;
    }
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| {
        panic!(
            "{} should exist (the web's schema fixture): {e}",
            path.display()
        )
    });
    let fixture: serde_json::Value =
        serde_json::from_str(&text).expect("the fixture should be JSON");
    if fixture != schema {
        let differing = differing_pointers(&fixture, &schema, String::new());
        panic!(
            "web/src/test/fixtures/hs-config-schema.json differs from hs_config::schema::json_schema() at {} \
             place(s), first: {:?}.\nRegenerate it with \
             `HS_UPDATE_WEB_SCHEMA_FIXTURE=1 cargo test -p hs-config --test web_schema_fixture`, \
             then run `npm run check` in web/ to see whether the interface renders the change.",
            differing.len(),
            differing.iter().take(10).collect::<Vec<_>>()
        );
    }
}

/// The JSON Pointers at which `a` and `b` differ, for a failure message a person can act on.
fn differing_pointers(a: &serde_json::Value, b: &serde_json::Value, at: String) -> Vec<String> {
    use serde_json::Value;
    match (a, b) {
        (Value::Object(x), Value::Object(y)) => {
            let mut keys: Vec<&String> = x.keys().chain(y.keys()).collect();
            keys.sort();
            keys.dedup();
            keys.into_iter()
                .flat_map(|k| {
                    let pointer = format!("{at}/{}", k.replace('~', "~0").replace('/', "~1"));
                    match (x.get(k), y.get(k)) {
                        (Some(l), Some(r)) => differing_pointers(l, r, pointer),
                        _ => vec![pointer],
                    }
                })
                .collect()
        }
        (Value::Array(x), Value::Array(y)) if x.len() == y.len() => x
            .iter()
            .zip(y)
            .enumerate()
            .flat_map(|(i, (l, r))| differing_pointers(l, r, format!("{at}/{i}")))
            .collect(),
        _ if a == b => Vec::new(),
        _ => vec![if at.is_empty() { "/".to_owned() } else { at }],
    }
}
