//! What one configuration change did, setting by setting, and how to undo it.
//!
//! [`crate::store::ConfigStore`] records every write as a merge patch
//! ([`crate::store::ChangeRecord::patch`]) plus, since the per-setting history, what the database
//! held at each setting the patch touched just before it was applied
//! ([`crate::store::ChangeRecord::before`]). This module is the pure half of reading that back:
//!
//! - [`before_values`] captures the prior values, inside the write's own transaction;
//! - [`setting_changes`] flattens a record into one row per setting, which is what the admin API
//!   serves and the web interface shows ("Login rate limit: 5 → 10");
//! - [`revert_target`] and [`diff_merge_patch`] turn a record back into the merge patch that
//!   undoes it against what is stored *now*, so a revert is an ordinary write with an ordinary
//!   history entry of its own;
//! - [`pointers_overlap`] is how a revert notices that a later change touched the same settings.
//!
//! Every pointer here is an RFC 6901 JSON Pointer relative to the section
//! (`/login/per_second` in `rate_limits`), the same vocabulary as
//! [`crate::document::leaf_pointers`]. An array is one setting, as it is everywhere else.
//!
//! Everything here is about the *database layer*. A setting the database held nothing for took
//! its value from the bootstrap file or the schema default, and this module says so
//! ([`Prior::Unset`]) rather than guessing which: the file may have changed since.

use std::collections::BTreeMap;

use serde_json::{Map, Value};

use crate::document::leaf_pointers;
use crate::store::{ChangeRecord, LaterChange};

/// What the database held at each setting `patch` touches, before it is applied to `current`
/// (the section's stored document; an empty object when it stores nothing).
///
/// Keyed by section-relative JSON Pointer; `null` where the database held nothing, so the setting
/// read from the file or the schema default. Where the patch writes *beneath* a value that is not
/// an object (`{"a": {"b": 1}}` over a stored `"a": 5`), the merge replaces that whole value, so
/// the value is recorded at its own pointer (`/a`), not at the leaf -- which is what makes
/// [`revert_target`] give `5` back rather than an empty object.
///
/// An empty patch touches nothing and records nothing.
#[must_use]
pub fn before_values(current: &Value, patch: &Value) -> BTreeMap<String, Value> {
    let mut out = BTreeMap::new();
    if patch.as_object().is_some_and(Map::is_empty) {
        return out;
    }
    for pointer in leaf_pointers(patch) {
        let tokens = pointer_tokens(&pointer);
        let mut cursor = current;
        let mut recorded = false;
        for (depth, token) in tokens.iter().enumerate() {
            match cursor {
                Value::Object(map) => match map.get(token) {
                    Some(child) => cursor = child,
                    None => {
                        out.insert(pointer.clone(), Value::Null);
                        recorded = true;
                        break;
                    }
                },
                other => {
                    out.insert(join_pointer(&tokens[..depth]), other.clone());
                    recorded = true;
                    break;
                }
            }
        }
        if !recorded {
            out.insert(pointer, cursor.clone());
        }
    }
    out
}

/// What a setting was before a change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Prior {
    /// The change was recorded before prior values were kept, so nobody can say.
    Unknown,
    /// The database held nothing: the setting read from the bootstrap file or the schema default.
    Unset,
    /// The database held this value.
    Set(Value),
}

/// One setting one change touched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SettingChange {
    /// Section-relative JSON Pointer (`/login/per_second`).
    pub pointer: String,
    /// What the database held before the change.
    pub before: Prior,
    /// What the change wrote, or `None` when it removed the setting from the database (a reset,
    /// which leaves it to the file or the schema default).
    pub after: Option<Value>,
}

/// One row per setting `patch` touched, with the value before (from `before`, the record's
/// [`before_values`], or `None` for a record older than them) and after.
#[must_use]
pub fn setting_changes(
    patch: &Value,
    before: Option<&BTreeMap<String, Value>>,
) -> Vec<SettingChange> {
    if patch.as_object().is_some_and(Map::is_empty) {
        return Vec::new();
    }
    leaf_pointers(patch)
        .into_iter()
        .map(|pointer| {
            let after = patch.pointer(&pointer).filter(|v| !v.is_null()).cloned();
            let before = match before {
                None => Prior::Unknown,
                Some(map) => match map.get(&pointer) {
                    Some(Value::Null) => Prior::Unset,
                    Some(value) => Prior::Set(value.clone()),
                    // Recorded at an ancestor: the patch replaced a non-object value above this
                    // setting, so this setting itself did not exist before.
                    None if map.keys().any(|k| is_proper_prefix(k, &pointer)) => Prior::Unset,
                    None => Prior::Unknown,
                },
            };
            SettingChange {
                pointer,
                before,
                after,
            }
        })
        .collect()
}

/// The section document that undoes a change: `current` with every pointer in `before` put back
/// to what it was -- set to the recorded value, or removed where the database held nothing.
///
/// Only the settings the change touched are put back. A setting a later change wrote elsewhere
/// in the section is left alone; one it wrote at the *same* pointer is overwritten, which is why
/// a revert checks [`pointers_overlap`] against later changes first.
#[must_use]
pub fn revert_target(current: &Value, before: &BTreeMap<String, Value>) -> Value {
    let mut target = if current.is_object() {
        current.clone()
    } else {
        Value::Object(Map::new())
    };
    for (pointer, value) in before {
        if value.is_null() {
            remove_at(&mut target, pointer);
        } else {
            set_at(&mut target, pointer, value.clone());
        }
    }
    target
}

/// The RFC 7396 merge patch that turns `from` into `to`: what [`crate::document::merge_patch`]
/// applied to `from` yields `to` (for documents without `null` members, which a stored section
/// never has -- merge patching removes them).
///
/// Unchanged members are left out, so an empty object means there is nothing to do.
#[must_use]
pub fn diff_merge_patch(from: &Value, to: &Value) -> Value {
    let (Value::Object(from_map), Value::Object(to_map)) = (from, to) else {
        return to.clone();
    };
    let mut patch = Map::new();
    for key in from_map.keys() {
        if !to_map.contains_key(key) {
            patch.insert(key.clone(), Value::Null);
        }
    }
    for (key, to_value) in to_map {
        match from_map.get(key) {
            Some(from_value) if from_value == to_value => {}
            Some(from_value @ Value::Object(_)) if to_value.is_object() => {
                patch.insert(key.clone(), diff_merge_patch(from_value, to_value));
            }
            _ => {
                patch.insert(key.clone(), to_value.clone());
            }
        }
    }
    Value::Object(patch)
}

/// The changes in `later` -- newer changes to the same section, oldest first -- that wrote any of
/// the settings a change's `before` names, each with the pointers it shares. Reverting that
/// change would undo those writes too.
#[must_use]
pub fn later_conflicts<'a>(
    before: &BTreeMap<String, Value>,
    later: impl IntoIterator<Item = &'a ChangeRecord>,
) -> Vec<LaterChange> {
    later
        .into_iter()
        .filter_map(|record| {
            let shared: Vec<String> = leaf_pointers(&record.patch)
                .into_iter()
                .filter(|p| before.keys().any(|t| pointers_overlap(t, p)))
                .collect();
            (!shared.is_empty()).then(|| LaterChange {
                revision: record.revision,
                actor: record.actor.clone(),
                at_ms: record.at_ms,
                pointers: shared,
            })
        })
        .collect()
}

/// Whether two section-relative pointers name the same setting or one contains the other.
#[must_use]
pub fn pointers_overlap(a: &str, b: &str) -> bool {
    a == b || is_proper_prefix(a, b) || is_proper_prefix(b, a)
}

/// `prefix` names an object that contains `pointer` (token-wise: `/a` contains `/a/b`, not
/// `/ab`).
fn is_proper_prefix(prefix: &str, pointer: &str) -> bool {
    pointer.len() > prefix.len()
        && pointer.starts_with(prefix)
        && pointer.as_bytes().get(prefix.len()) == Some(&b'/')
}

/// The unescaped tokens of a JSON Pointer (`/a~1b/c` is `["a/b", "c"]`).
#[must_use]
pub fn pointer_tokens(pointer: &str) -> Vec<String> {
    pointer
        .split('/')
        .skip(1)
        .map(|t| t.replace("~1", "/").replace("~0", "~"))
        .collect()
}

fn join_pointer(tokens: &[String]) -> String {
    let mut out = String::new();
    for token in tokens {
        out.push('/');
        out.push_str(&token.replace('~', "~0").replace('/', "~1"));
    }
    out
}

/// Sets `pointer` in `document` to `value`, creating (or replacing non-object values with) the
/// objects above it. The empty pointer replaces the whole document.
fn set_at(document: &mut Value, pointer: &str, value: Value) {
    let tokens = pointer_tokens(pointer);
    let Some((last, parents)) = tokens.split_last() else {
        *document = value;
        return;
    };
    let mut cursor = document;
    for token in parents {
        if !cursor.is_object() {
            *cursor = Value::Object(Map::new());
        }
        let Value::Object(map) = cursor else {
            return;
        };
        cursor = map
            .entry(token.clone())
            .or_insert_with(|| Value::Object(Map::new()));
    }
    if !cursor.is_object() {
        *cursor = Value::Object(Map::new());
    }
    if let Value::Object(map) = cursor {
        map.insert(last.clone(), value);
    }
}

/// Removes `pointer` from `document`, then every object above it that the removal left empty,
/// so a revert of "set `/a/b`" over a section that had no `a` leaves no `a: {}` behind. The empty
/// pointer empties the document.
fn remove_at(document: &mut Value, pointer: &str) {
    let tokens = pointer_tokens(pointer);
    if tokens.is_empty() {
        *document = Value::Object(Map::new());
        return;
    }
    remove_tokens(document, &tokens);
}

/// Returns whether `value` is now an empty object because of this removal.
fn remove_tokens(value: &mut Value, tokens: &[String]) -> bool {
    let Value::Object(map) = value else {
        return false;
    };
    let Some((first, rest)) = tokens.split_first() else {
        return false;
    };
    if rest.is_empty() {
        if map.remove(first).is_none() {
            return false;
        }
    } else {
        let Some(child) = map.get_mut(first) else {
            return false;
        };
        if remove_tokens(child, rest) {
            map.remove(first);
        } else {
            return false;
        }
    }
    map.is_empty()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::document::merge_patch;
    use serde_json::json;

    fn map(pairs: &[(&str, Value)]) -> BTreeMap<String, Value> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), v.clone()))
            .collect()
    }

    #[test]
    fn before_values_record_the_stored_value_or_null_per_setting() {
        let current = json!({"login": {"per_second": 5, "burst_count": 10}});
        let patch = json!({"login": {"per_second": 10}, "message": {"per_second": 1}});
        assert_eq!(
            before_values(&current, &patch),
            map(&[
                ("/login/per_second", json!(5)),
                ("/message/per_second", Value::Null),
            ])
        );
    }

    #[test]
    fn a_patch_writing_beneath_a_scalar_records_the_scalar_where_it_was() {
        let current = json!({"a": 5});
        let patch = json!({"a": {"b": 1, "c": 2}});
        assert_eq!(before_values(&current, &patch), map(&[("/a", json!(5))]));
        let mut after = current.clone();
        merge_patch(&mut after, &patch);
        let target = revert_target(&after, &before_values(&current, &patch));
        assert_eq!(target, current);
    }

    #[test]
    fn an_empty_patch_touches_nothing() {
        assert!(before_values(&json!({"a": 1}), &json!({})).is_empty());
        assert!(setting_changes(&json!({}), Some(&BTreeMap::new())).is_empty());
    }

    #[test]
    fn setting_changes_say_what_was_there_and_what_was_written() {
        let patch =
            json!({"login": {"per_second": 10, "burst_count": null}, "message": {"per_second": 1}});
        let before = map(&[
            ("/login/per_second", json!(5)),
            ("/login/burst_count", json!(20)),
            ("/message/per_second", Value::Null),
        ]);
        let rows = setting_changes(&patch, Some(&before));
        assert_eq!(
            rows,
            vec![
                // In the patch's own order.
                SettingChange {
                    pointer: "/login/per_second".into(),
                    before: Prior::Set(json!(5)),
                    after: Some(json!(10)),
                },
                SettingChange {
                    pointer: "/login/burst_count".into(),
                    before: Prior::Set(json!(20)),
                    after: None,
                },
                SettingChange {
                    pointer: "/message/per_second".into(),
                    before: Prior::Unset,
                    after: Some(json!(1)),
                },
            ]
        );
        // A record from before prior values were kept.
        assert!(
            setting_changes(&patch, None)
                .iter()
                .all(|row| row.before == Prior::Unknown)
        );
    }

    #[test]
    fn a_change_is_undone_exactly_and_leaves_no_empty_objects() {
        let current = json!({"login": {"burst_count": 10}});
        let patch =
            json!({"login": {"per_second": 10}, "message": {"per_second": 1, "burst_count": 3}});
        let before = before_values(&current, &patch);
        let mut after = current.clone();
        merge_patch(&mut after, &patch);

        let target = revert_target(&after, &before);
        assert_eq!(target, current);
        let undo = diff_merge_patch(&after, &target);
        assert_eq!(
            undo,
            json!({"login": {"per_second": null}, "message": null})
        );
        let mut reverted = after.clone();
        merge_patch(&mut reverted, &undo);
        assert_eq!(reverted, current);
    }

    #[test]
    fn a_revert_leaves_settings_the_change_did_not_touch_alone() {
        let before = map(&[("/login/per_second", json!(5))]);
        // A later change set burst_count; the revert of per_second must not undo it.
        let now = json!({"login": {"per_second": 10, "burst_count": 99}});
        assert_eq!(
            revert_target(&now, &before),
            json!({"login": {"per_second": 5, "burst_count": 99}})
        );
    }

    #[test]
    fn a_reset_is_undone_by_setting_the_value_back() {
        let current = json!({"enable_registration": true});
        let patch = json!({"enable_registration": null});
        let before = before_values(&current, &patch);
        let mut after = current.clone();
        merge_patch(&mut after, &patch);
        assert_eq!(after, json!({}));
        let undo = diff_merge_patch(&after, &revert_target(&after, &before));
        assert_eq!(undo, json!({"enable_registration": true}));
    }

    #[test]
    fn diffing_equal_documents_is_an_empty_patch() {
        let doc = json!({"a": {"b": [1, 2]}, "c": "x"});
        assert_eq!(diff_merge_patch(&doc, &doc), json!({}));
    }

    #[test]
    fn diff_replaces_arrays_whole_and_objects_by_member() {
        let from = json!({"sizes": [1, 2], "o": {"x": 1, "y": 2}});
        let to = json!({"sizes": [1], "o": {"x": 1, "z": 3}});
        let patch = diff_merge_patch(&from, &to);
        assert_eq!(patch, json!({"sizes": [1], "o": {"y": null, "z": 3}}));
        let mut applied = from.clone();
        merge_patch(&mut applied, &patch);
        assert_eq!(applied, to);
    }

    #[test]
    fn overlap_is_token_wise() {
        assert!(pointers_overlap("/login/per_second", "/login/per_second"));
        assert!(pointers_overlap("/login", "/login/per_second"));
        assert!(pointers_overlap("/login/per_second", "/login"));
        assert!(!pointers_overlap("/login", "/login_extra"));
        assert!(!pointers_overlap("/login/per_second", "/login/burst_count"));
    }

    #[test]
    fn escaped_tokens_round_trip() {
        let before = map(&[("/a~1b/c~0d", json!(1))]);
        let target = revert_target(&json!({}), &before);
        assert_eq!(target, json!({"a/b": {"c~d": 1}}));
        assert_eq!(pointer_tokens("/a~1b/c~0d"), vec!["a/b", "c~d"]);
    }
}
