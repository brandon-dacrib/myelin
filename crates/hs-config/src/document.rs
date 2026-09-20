//! Sparse configuration documents: what each layer contributes, and the rule that combines them.
//!
//! A *document* is a JSON object shaped like [`Config`](crate::Config) but with every key
//! optional — the same shape a hand-written `homeserver.yaml` has, where anything absent means
//! "use the default". The effective configuration is built by merging several such documents in
//! precedence order (see [`Origin`]) and deserializing the result.
//!
//! The merge rule is RFC 7396 JSON Merge Patch: an object merges key by key, recursively;
//! anything else (a scalar, an array) replaces wholesale; and an explicit `null` *removes* the
//! key rather than setting it to null.
//!
//! That last case is what the admin API's reset-to-default does, and it is worth being exact
//! about what it resets *to*. A patch's `null` removes the key from the layer it is applied to --
//! [`crate::store::ConfigStore::patch_section`] merges it into the stored section rather than
//! storing the null itself -- so the database stops saying anything about that setting, and
//! whichever layer is underneath speaks again: the bootstrap file if it sets it, the schema
//! default otherwise. No layer can suppress a layer below it, because a layer is a document, not
//! a patch, and "reset" means "this layer stops setting it" rather than "nobody may set it". The
//! alternative -- storing a tombstone that shadows the file -- would make the database's
//! contribution to a setting have three states instead of two, and the file's value permanently
//! unreachable once anybody had touched that key.

use std::collections::BTreeMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// Where one setting's effective value came from, lowest precedence first. Each variant wins over
/// every variant above it.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum Origin {
    /// Nothing set it: the value is the schema's own default.
    Default,
    /// The bootstrap file (`-c homeserver.yaml`). Below the database so that editing a setting in
    /// the web interface is not silently undone by a file nobody remembers is mounted.
    File,
    /// The database — what the admin API writes, and the normal home for configuration.
    Database,
    /// An `HS__` environment variable. Above the database on purpose: a deployment that pins a
    /// setting in its environment (a Kubernetes manifest, a systemd unit) has said something the
    /// server must not quietly override. The admin API reports these as read-only rather than
    /// accepting a write it knows will have no effect.
    Environment,
}

impl Origin {
    /// The name this origin is reported under in the admin API.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Origin::Default => "default",
            Origin::File => "file",
            Origin::Database => "database",
            Origin::Environment => "environment",
        }
    }
}

/// Applies `patch` onto `target` in place, by RFC 7396 JSON Merge Patch.
///
/// Objects merge recursively; every other value replaces what was there; `null` in the patch
/// removes the key entirely rather than setting it to null.
pub fn merge_patch(target: &mut Value, patch: &Value) {
    let Value::Object(patch_map) = patch else {
        *target = patch.clone();
        return;
    };
    if !target.is_object() {
        *target = Value::Object(Map::new());
    }
    let Value::Object(target_map) = target else {
        unreachable!("forced to an object directly above");
    };
    for (key, value) in patch_map {
        if value.is_null() {
            target_map.remove(key);
        } else {
            merge_patch(target_map.entry(key.clone()).or_insert(Value::Null), value);
        }
    }
}

/// Merges `layers` in order (later wins) into one document.
#[must_use]
pub fn merge_all<'a, I: IntoIterator<Item = &'a Value>>(layers: I) -> Value {
    let mut merged = Value::Object(Map::new());
    for layer in layers {
        merge_patch(&mut merged, layer);
    }
    merged
}

/// Every leaf in `document`, as a JSON Pointer (`/auth/enable_registration`).
///
/// An array is one leaf, not one per element: `rate_limits` buckets and `media.thumbnail_sizes`
/// are edited and reported whole, and a per-element origin would be meaningless the moment a
/// higher layer replaced the array with one of a different length.
#[must_use]
pub fn leaf_pointers(document: &Value) -> Vec<String> {
    let mut out = Vec::new();
    collect_leaves(document, &mut String::new(), &mut out);
    out
}

fn collect_leaves(value: &Value, prefix: &mut String, out: &mut Vec<String>) {
    match value {
        Value::Object(map) if !map.is_empty() => {
            for (key, child) in map {
                let mark = prefix.len();
                prefix.push('/');
                prefix.push_str(&escape_pointer_token(key));
                collect_leaves(child, prefix, out);
                prefix.truncate(mark);
            }
        }
        _ => out.push(prefix.clone()),
    }
}

/// Escapes one JSON Pointer token (RFC 6901: `~` becomes `~0`, `/` becomes `~1`). Config keys are
/// `snake_case` field names today, so this never fires — but a map-valued section keyed by
/// something user-supplied (an appservice id, a rate-limit bucket name) could carry either.
fn escape_pointer_token(token: &str) -> String {
    token.replace('~', "~0").replace('/', "~1")
}

/// The origin of every setting any layer sets: the highest-precedence layer that set it wins.
///
/// `layers` must be in precedence order, lowest first. Settings nothing sets are absent from the
/// result — their origin is [`Origin::Default`], which a caller resolves by looking up a pointer
/// that is not here.
#[must_use]
pub fn origins<'a, I: IntoIterator<Item = (Origin, &'a Value)>>(
    layers: I,
) -> BTreeMap<String, Origin> {
    let mut out = BTreeMap::new();
    for (origin, document) in layers {
        for pointer in leaf_pointers(document) {
            // A `null` leaf is a removal, not a value: `merge_patch` drops the key, so this
            // layer does not own the setting -- whatever is underneath does. No layer this
            // server assembles actually carries one today (the store merges patches into the
            // stored document rather than keeping their nulls), but an `HS__FOO__BAR=null`
            // variable produces one, so the rule is stated rather than assumed.
            if document.pointer(&pointer).is_some_and(Value::is_null) {
                out.remove(&pointer);
                continue;
            }
            out.insert(pointer, origin);
        }
    }
    out
}

/// The top-level section a JSON Pointer falls in (`/auth/enable_registration` → `auth`), or
/// `None` for the empty pointer.
#[must_use]
pub fn section_of(pointer: &str) -> Option<&str> {
    pointer
        .strip_prefix('/')?
        .split('/')
        .next()
        .filter(|s| !s.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn objects_merge_recursively_and_scalars_replace() {
        let mut target = json!({"auth": {"enable_registration": false, "guest_access": true}});
        merge_patch(&mut target, &json!({"auth": {"enable_registration": true}}));
        assert_eq!(
            target,
            json!({"auth": {"enable_registration": true, "guest_access": true}})
        );
    }

    #[test]
    fn an_array_replaces_rather_than_merging_elementwise() {
        let mut target = json!({"media": {"thumbnail_sizes": [1, 2, 3]}});
        merge_patch(&mut target, &json!({"media": {"thumbnail_sizes": [9]}}));
        assert_eq!(target, json!({"media": {"thumbnail_sizes": [9]}}));
    }

    /// The reset-to-default path: the web interface clears a setting by patching `null` over it,
    /// and the key disappears rather than becoming a null the schema would reject.
    #[test]
    fn null_removes_the_key_instead_of_setting_it() {
        let mut target = json!({"auth": {"enable_registration": true, "guest_access": true}});
        merge_patch(&mut target, &json!({"auth": {"enable_registration": null}}));
        assert_eq!(target, json!({"auth": {"guest_access": true}}));
    }

    #[test]
    fn leaves_are_pointers_and_an_array_is_one_leaf() {
        let doc = json!({"server": {"server_name": "a.example"}, "media": {"sizes": [1, 2]}});
        let mut leaves = leaf_pointers(&doc);
        leaves.sort();
        assert_eq!(leaves, vec!["/media/sizes", "/server/server_name"]);
    }

    #[test]
    fn the_highest_layer_that_sets_a_value_owns_it() {
        let file = json!({"auth": {"enable_registration": false}, "server": {"server_name": "a"}});
        let database = json!({"auth": {"enable_registration": true}});
        let environment = json!({"server": {"server_name": "b"}});
        let origins = origins([
            (Origin::File, &file),
            (Origin::Database, &database),
            (Origin::Environment, &environment),
        ]);
        assert_eq!(origins["/auth/enable_registration"], Origin::Database);
        assert_eq!(origins["/server/server_name"], Origin::Environment);
        assert_eq!(origins.get("/federation/client_timeout"), None);
    }

    /// A layer that carries an explicit null does not own the setting it nulls: the key is gone
    /// from the merged document, so reporting that layer as its source would be a lie. What the
    /// *server* does with a reset is one level up, in `layered` -- the store never keeps the null,
    /// so the layer below gets the setting back.
    #[test]
    fn a_layer_that_nulls_a_setting_does_not_own_it() {
        let file = json!({"auth": {"enable_registration": false}});
        let database = json!({"auth": {"enable_registration": null}});
        let origins = origins([(Origin::File, &file), (Origin::Database, &database)]);
        assert_eq!(origins.get("/auth/enable_registration"), None);

        let merged = merge_all([&file, &database]);
        assert_eq!(merged.pointer("/auth/enable_registration"), None);
    }

    #[test]
    fn section_of_a_pointer_is_its_first_token() {
        assert_eq!(section_of("/auth/enable_registration"), Some("auth"));
        assert_eq!(section_of("/auth"), Some("auth"));
        assert_eq!(section_of(""), None);
    }
}
