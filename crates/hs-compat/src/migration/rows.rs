//! Synapse's rows, read into the importer's model: the parts of reading Synapse that are a
//! matter of knowing how Synapse lays a thing out rather than of querying it, kept apart from
//! [`crate::migration::source`] so that they are tested against rows as a real Synapse wrote
//! them (`tests/fixtures/synapse-small/data.sql`) without a database.
//!
//! Each function takes rows as `to_jsonb(row)` gives them (numbers as numbers, text as
//! strings, JSON columns still as text), and is written from the column names and the
//! behavior of Synapse's client-server API, not from Synapse's code.

use serde_json::{Map, Value, json};

use super::model::{
    SynapseBackupVersion, SynapseCrossSigning, SynapseDeviceKeys, SynapseFilter, SynapsePushRules,
    SynapsePusher, SynapseReceipt, SynapseRoomKey,
};

fn text(row: &Value, key: &str) -> Option<String> {
    row.get(key).and_then(Value::as_str).map(str::to_owned)
}

fn number(row: &Value, key: &str) -> Option<i64> {
    row.get(key).and_then(Value::as_i64)
}

fn flag(row: &Value, key: &str) -> Option<bool> {
    match row.get(key) {
        Some(Value::Bool(b)) => Some(*b),
        Some(Value::Number(n)) => n.as_i64().map(|n| n != 0),
        _ => None,
    }
}

/// A JSON column held as text, parsed; `what` names it in the error.
fn json_text(row: &Value, key: &str, what: &str) -> Result<Value, String> {
    let raw = text(row, key).ok_or_else(|| format!("{what} has no {key}"))?;
    serde_json::from_str(&raw).map_err(|e| format!("{what}'s {key} is unreadable JSON: {e}"))
}

/// One signature Synapse holds apart from the key it is on (`e2e_cross_signing_signatures`):
/// `(signer, signing key id, signature)`.
pub type HeldSignature = (String, String, String);

/// Reads an `e2e_cross_signing_signatures` row: `(target user, target device or key, held)`.
///
/// # Errors
/// A column is missing.
pub fn signature(row: &Value) -> Result<(String, String, HeldSignature), String> {
    let get = |key: &str| text(row, key).ok_or_else(|| format!("a signature row has no {key}"));
    Ok((
        get("target_user_id")?,
        get("target_device_id")?,
        (get("user_id")?, get("key_id")?, get("signature")?),
    ))
}

/// Merges `signatures` into `object`'s `signatures` map, as `/keys/query` shows them: Synapse
/// keeps a signature uploaded with `/keys/signatures/upload` in a table of its own, and adds it
/// to the key it signs when the key is queried.
pub fn merge_signatures(object: &mut Value, signatures: &[HeldSignature]) {
    let Some(map) = object.as_object_mut() else {
        return;
    };
    for (signer, key_id, signature) in signatures {
        let by_signer = map
            .entry("signatures")
            .or_insert_with(|| Value::Object(Map::new()));
        if !by_signer.is_object() {
            *by_signer = Value::Object(Map::new());
        }
        if let Some(by_signer) = by_signer.as_object_mut() {
            let keys = by_signer
                .entry(signer.clone())
                .or_insert_with(|| Value::Object(Map::new()));
            if let Some(keys) = keys.as_object_mut() {
                keys.insert(key_id.clone(), Value::String(signature.clone()));
            }
        }
    }
}

/// The public key a cross-signing key is named by: what follows `ed25519:` in its one `keys`
/// entry. A signature on it names it as its `target_device_id`.
#[must_use]
pub fn cross_signing_public_key(key: &Value) -> Option<String> {
    key.get("keys")?
        .as_object()?
        .keys()
        .find_map(|id| id.strip_prefix("ed25519:").map(str::to_owned))
}

/// One device's end-to-end keys from its rows: `device_keys` (the `e2e_device_keys_json` row,
/// if it has one), its one-time keys (`e2e_one_time_keys_json`, in any order), its fallback
/// keys (`e2e_fallback_keys_json`) and the signatures held on its identity keys.
///
/// # Errors
/// A row's JSON is unreadable.
pub fn device_keys(
    user_id: &str,
    device_id: &str,
    device_keys: Option<&Value>,
    one_time_keys: &[Value],
    fallback_keys: &[Value],
    signatures: &[HeldSignature],
) -> Result<SynapseDeviceKeys, String> {
    let what = format!("{user_id}'s device {device_id}");
    let keys = match device_keys {
        Some(row) => {
            let mut keys = json_text(row, "key_json", &what)?;
            merge_signatures(&mut keys, signatures);
            Some(keys)
        }
        None => None,
    };
    let mut otks: Vec<(i64, String, Value)> = Vec::with_capacity(one_time_keys.len());
    for row in one_time_keys {
        let id = format!(
            "{}:{}",
            text(row, "algorithm").unwrap_or_default(),
            text(row, "key_id").unwrap_or_default()
        );
        otks.push((
            number(row, "ts_added_ms").unwrap_or(0),
            id,
            json_text(row, "key_json", &what)?,
        ));
    }
    // The order they were uploaded in, which is the order they are handed out in.
    otks.sort_by(|a, b| (a.0, &a.1).cmp(&(b.0, &b.1)));
    let mut fallback = Vec::with_capacity(fallback_keys.len());
    for row in fallback_keys {
        let id = format!(
            "{}:{}",
            text(row, "algorithm").unwrap_or_default(),
            text(row, "key_id").unwrap_or_default()
        );
        fallback.push((
            id,
            json_text(row, "key_json", &what)?,
            flag(row, "used").unwrap_or(false),
        ));
    }
    fallback.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(SynapseDeviceKeys {
        user_id: user_id.to_owned(),
        device_id: device_id.to_owned(),
        keys,
        one_time_keys: otks.into_iter().map(|(_, id, key)| (id, key)).collect(),
        fallback_keys: fallback,
    })
}

/// One account's cross-signing keys from its `e2e_cross_signing_keys` rows (every key it ever
/// uploaded: the newest of each type is the one in force) and the signatures held on them,
/// keyed by the signed key's public key.
///
/// # Errors
/// A key's JSON is unreadable.
pub fn cross_signing(
    user_id: &str,
    key_rows: &[Value],
    signatures: &[(String, HeldSignature)],
) -> Result<SynapseCrossSigning, String> {
    let mut newest: [Option<(i64, Value)>; 3] = [None, None, None];
    for row in key_rows {
        let slot = match text(row, "keytype").as_deref() {
            Some("master") => 0,
            Some("self_signing") => 1,
            Some("user_signing") => 2,
            _ => continue,
        };
        let stream = number(row, "stream_id").unwrap_or(0);
        if newest[slot].as_ref().is_some_and(|(s, _)| *s >= stream) {
            continue;
        }
        let key = json_text(row, "keydata", &format!("{user_id}'s cross-signing key"))?;
        newest[slot] = Some((stream, key));
    }
    let [master, self_signing, user_signing] = newest.map(|slot| {
        slot.map(|(_, mut key)| {
            if let Some(public) = cross_signing_public_key(&key) {
                let on_it: Vec<HeldSignature> = signatures
                    .iter()
                    .filter(|(target, _)| *target == public)
                    .map(|(_, held)| held.clone())
                    .collect();
                merge_signatures(&mut key, &on_it);
            }
            key
        })
    });
    Ok(SynapseCrossSigning {
        user_id: user_id.to_owned(),
        master,
        self_signing,
        user_signing,
    })
}

/// A backup version from its `e2e_room_keys_versions` row.
///
/// # Errors
/// A column is missing, or `auth_data` is unreadable.
pub fn backup_version(row: &Value) -> Result<SynapseBackupVersion, String> {
    let user_id = text(row, "user_id").ok_or("a backup version has no user_id")?;
    let version = number(row, "version")
        .and_then(|v| u64::try_from(v).ok())
        .ok_or_else(|| format!("{user_id}'s backup has no usable version number"))?;
    Ok(SynapseBackupVersion {
        auth_data: json_text(row, "auth_data", &format!("{user_id}'s backup {version}"))?,
        user_id,
        version,
        algorithm: text(row, "algorithm").unwrap_or_default(),
        deleted: flag(row, "deleted").unwrap_or(false),
    })
}

/// A backed-up room key from its `e2e_room_keys` row.
///
/// # Errors
/// A column is missing, or `session_data` is unreadable.
pub fn room_key(row: &Value) -> Result<SynapseRoomKey, String> {
    let room_id = text(row, "room_id").ok_or("a backed-up key has no room_id")?;
    let session_id = text(row, "session_id").ok_or("a backed-up key has no session_id")?;
    let count = |key: &str| {
        number(row, key)
            .and_then(|n| u64::try_from(n).ok())
            .unwrap_or(0)
    };
    Ok(SynapseRoomKey {
        session_data: json_text(row, "session_data", &format!("backed-up key {session_id}"))?,
        first_message_index: count("first_message_index"),
        forwarded_count: count("forwarded_count"),
        is_verified: flag(row, "is_verified").unwrap_or(false),
        room_id,
        session_id,
    })
}

/// The kind a Synapse push rule's `priority_class` stands for. `postcontent` (MSC4306) has no
/// counterpart here.
fn kind_of_class(class: i64) -> Option<&'static str> {
    match class {
        5 => Some("override"),
        4 => Some("content"),
        3 => Some("room"),
        2 => Some("sender"),
        1 => Some("underride"),
        _ => None,
    }
}

/// `global/<kind>/<id>`, Synapse's name for a rule, split.
fn split_rule_id(rule_id: &str) -> Option<(&str, &str)> {
    let mut parts = rule_id.splitn(3, '/');
    let (Some("global"), Some(kind), Some(id)) = (parts.next(), parts.next(), parts.next()) else {
        return None;
    };
    (!id.is_empty()).then_some((kind, id))
}

/// One account's push rules from its `push_rules` and `push_rules_enable` rows.
///
/// Synapse keeps only what the account changed: a rule it made is a `push_rules` row with the
/// kind's `priority_class`; a server-default rule whose actions it changed is a row with the
/// default's id (`global/underride/.m.rule.message`) and class `-1`; a rule turned on or off,
/// its own or a default, is a `push_rules_enable` row. Within a kind, Synapse orders an
/// account's rules by `priority`, highest first.
#[must_use]
pub fn push_rules(user_id: &str, rule_rows: &[Value], enable_rows: &[Value]) -> SynapsePushRules {
    let mut out = SynapsePushRules {
        user_id: user_id.to_owned(),
        ..SynapsePushRules::default()
    };
    let mut custom: Vec<(i64, i64, String, Value)> = Vec::new();
    for row in rule_rows {
        let rule_id = text(row, "rule_id").unwrap_or_default();
        let Some((kind, id)) = split_rule_id(&rule_id) else {
            out.unreadable
                .push(format!("rule {rule_id:?}: not a rule id Synapse makes"));
            continue;
        };
        let actions = match json_text(row, "actions", &format!("rule {rule_id}")) {
            Ok(actions) => actions,
            Err(why) => {
                out.unreadable.push(why);
                continue;
            }
        };
        if id.starts_with('.') {
            out.default_actions
                .push((kind.to_owned(), id.to_owned(), actions));
            continue;
        }
        let class = number(row, "priority_class").unwrap_or(0);
        let Some(class_kind) = kind_of_class(class) else {
            out.unreadable.push(format!(
                "rule {rule_id}: priority class {class} has no counterpart here"
            ));
            continue;
        };
        let conditions = match json_text(row, "conditions", &format!("rule {rule_id}")) {
            Ok(conditions) => conditions,
            Err(why) => {
                out.unreadable.push(why);
                continue;
            }
        };
        let rule = match class_kind {
            "override" | "underride" => {
                json!({"rule_id": id, "conditions": conditions, "actions": actions})
            }
            "content" => {
                let Some(pattern) = conditions
                    .as_array()
                    .and_then(|c| c.first())
                    .and_then(|c| c.get("pattern"))
                    .and_then(Value::as_str)
                else {
                    out.unreadable
                        .push(format!("rule {rule_id}: a content rule without a pattern"));
                    continue;
                };
                json!({"rule_id": id, "pattern": pattern, "actions": actions})
            }
            _ => json!({"rule_id": id, "actions": actions}),
        };
        custom.push((
            class,
            number(row, "priority").unwrap_or(0),
            class_kind.to_owned(),
            rule,
        ));
    }
    custom.sort_by(|a, b| (b.0, b.1).cmp(&(a.0, a.1)));
    out.custom = custom
        .into_iter()
        .map(|(_, _, kind, rule)| (kind, rule))
        .collect();
    for row in enable_rows {
        let rule_id = text(row, "rule_id").unwrap_or_default();
        let Some(enabled) = flag(row, "enabled") else {
            continue;
        };
        match split_rule_id(&rule_id) {
            Some((kind, id)) => out.enabled.push((kind.to_owned(), id.to_owned(), enabled)),
            None => out
                .unreadable
                .push(format!("enabled flag of {rule_id:?}: not a rule id")),
        }
    }
    out
}

/// A pusher from its `pushers` row.
///
/// # Errors
/// A column is missing, or `data` is unreadable.
pub fn pusher(row: &Value) -> Result<SynapsePusher, String> {
    let id = number(row, "id").ok_or("a pusher has no id")?;
    let get = |key: &str| text(row, key).ok_or_else(|| format!("pusher {id} has no {key}"));
    let data = match text(row, "data") {
        Some(_) => json_text(row, "data", &format!("pusher {id}"))?,
        None => json!({}),
    };
    Ok(SynapsePusher {
        id,
        user_id: get("user_name")?,
        kind: get("kind")?,
        app_id: get("app_id")?,
        app_display_name: get("app_display_name")?,
        device_display_name: get("device_display_name")?,
        pushkey: get("pushkey")?,
        profile_tag: text(row, "profile_tag").filter(|t| !t.is_empty()),
        lang: text(row, "lang"),
        data,
        enabled: flag(row, "enabled").unwrap_or(true),
    })
}

/// A read receipt from its `receipts_linearized` row (`data` holds `{"ts": ...}`).
///
/// # Errors
/// A column is missing.
pub fn receipt(row: &Value) -> Result<SynapseReceipt, String> {
    let stream_id = number(row, "stream_id").ok_or("a receipt has no stream_id")?;
    let get = |key: &str| text(row, key).ok_or_else(|| format!("receipt {stream_id} has no {key}"));
    let ts = text(row, "data")
        .and_then(|d| serde_json::from_str::<Value>(&d).ok())
        .and_then(|d| d.get("ts").and_then(Value::as_u64))
        .unwrap_or(0);
    Ok(SynapseReceipt {
        stream_id,
        room_id: get("room_id")?,
        receipt_type: get("receipt_type")?,
        user_id: get("user_id")?,
        event_id: get("event_id")?,
        thread_id: text(row, "thread_id"),
        ts,
    })
}

/// A filter from its `user_filters` row, without `filter_json`, and that column as text.
/// Older Synapses name the account by its localpart only (`user_id`); newer ones also keep
/// `full_user_id`.
///
/// # Errors
/// A column is missing, or the filter is unreadable.
pub fn filter(row: &Value, filter_json: &str, server_name: &str) -> Result<SynapseFilter, String> {
    let localpart = text(row, "user_id").ok_or("a filter has no user_id")?;
    let user_id =
        text(row, "full_user_id").unwrap_or_else(|| format!("@{localpart}:{server_name}"));
    let filter_id = number(row, "filter_id")
        .map(|n| n.to_string())
        .ok_or_else(|| format!("a filter of {user_id} has no filter_id"))?;
    let filter = serde_json::from_str(filter_json)
        .map_err(|e| format!("{user_id}'s filter {filter_id} is unreadable JSON: {e}"))?;
    Ok(SynapseFilter {
        user_id,
        filter_id,
        filter,
    })
}

#[cfg(test)]
mod tests {
    //! Rows as the real Synapse 1.161 of `tests/fixtures/synapse-small` wrote them.

    use super::*;

    #[test]
    fn device_keys_carry_the_signatures_synapse_holds_apart_and_one_time_keys_keep_upload_order() {
        let keys_row = json!({
            "user_id": "@alice:fixture.test", "device_id": "ALICEPHONE", "ts_added_ms": 1,
            "key_json": r#"{"algorithms":["m.olm.v1.curve25519-aes-sha2"],"device_id":"ALICEPHONE","keys":{"ed25519:ALICEPHONE":"pub"},"signatures":{"@alice:fixture.test":{"ed25519:ALICEPHONE":"self"}},"user_id":"@alice:fixture.test"}"#,
        });
        let otk = |id: &str, ts: i64| {
            json!({"algorithm": "signed_curve25519", "key_id": id, "ts_added_ms": ts,
                   "key_json": format!(r#"{{"key":"{id}"}}"#)})
        };
        let fallback = json!({"algorithm": "signed_curve25519", "key_id": "AAAAFB",
                              "key_json": r#"{"fallback":true,"key":"fb"}"#, "used": false});
        let signature = signature(&json!({
            "user_id": "@alice:fixture.test", "key_id": "ed25519:SSK",
            "target_user_id": "@alice:fixture.test", "target_device_id": "ALICEPHONE",
            "signature": "by-ssk",
        }))
        .unwrap();
        assert_eq!(signature.1, "ALICEPHONE");
        let device = device_keys(
            "@alice:fixture.test",
            "ALICEPHONE",
            Some(&keys_row),
            &[otk("AAAAA1", 20), otk("AAAAA0", 10), otk("AAAAA2", 20)],
            &[fallback],
            &[signature.2],
        )
        .unwrap();
        let keys = device.keys.unwrap();
        assert_eq!(
            keys["signatures"]["@alice:fixture.test"],
            json!({"ed25519:ALICEPHONE": "self", "ed25519:SSK": "by-ssk"})
        );
        let ids: Vec<&str> = device
            .one_time_keys
            .iter()
            .map(|(id, _)| id.as_str())
            .collect();
        assert_eq!(
            ids,
            [
                "signed_curve25519:AAAAA0",
                "signed_curve25519:AAAAA1",
                "signed_curve25519:AAAAA2"
            ]
        );
        assert_eq!(
            device.fallback_keys,
            vec![(
                "signed_curve25519:AAAAFB".to_owned(),
                json!({"fallback": true, "key": "fb"}),
                false
            )]
        );
    }

    #[test]
    fn the_newest_cross_signing_key_of_each_type_is_the_one_in_force_with_its_signatures() {
        let key = |kind: &str, public: &str, stream: i64| {
            json!({"user_id": "@bob:fixture.test", "keytype": kind, "stream_id": stream,
                   "keydata": format!(r#"{{"keys":{{"ed25519:{public}":"{public}"}},"usage":["{kind}"],"user_id":"@bob:fixture.test"}}"#)})
        };
        let by_alice = (
            "BOBMASTER2".to_owned(),
            (
                "@alice:fixture.test".to_owned(),
                "ed25519:ALICEUSK".to_owned(),
                "alice-verified-bob".to_owned(),
            ),
        );
        let keys = cross_signing(
            "@bob:fixture.test",
            &[
                key("master", "BOBMASTER1", 5),
                key("master", "BOBMASTER2", 9),
                key("self_signing", "BOBSSK", 6),
            ],
            &[by_alice],
        )
        .unwrap();
        let master = keys.master.unwrap();
        assert_eq!(
            cross_signing_public_key(&master).as_deref(),
            Some("BOBMASTER2")
        );
        assert_eq!(
            master["signatures"]["@alice:fixture.test"]["ed25519:ALICEUSK"],
            "alice-verified-bob"
        );
        assert!(keys.self_signing.unwrap().get("signatures").is_none());
        assert!(keys.user_signing.is_none());
    }

    #[test]
    fn push_rules_read_as_the_client_server_api_shows_them() {
        let rule = |id: &str, class: i64, priority: i64, conditions: &str, actions: &str| {
            json!({"user_name": "@alice:fixture.test", "rule_id": id, "priority_class": class,
                   "priority": priority, "conditions": conditions, "actions": actions})
        };
        let enable = |id: &str, enabled: Option<i64>| json!({"user_name": "@alice:fixture.test", "rule_id": id, "enabled": enabled});
        let rules = push_rules(
            "@alice:fixture.test",
            &[
                rule(
                    "global/content/lobbyword",
                    4,
                    0,
                    r#"[{"kind":"event_match","key":"content.body","pattern":"lobby"}]"#,
                    r#"["notify",{"set_tweak":"highlight"}]"#,
                ),
                rule(
                    "global/room/!dm:fixture.test",
                    3,
                    0,
                    r#"[{"kind":"event_match","key":"room_id","pattern":"!dm:fixture.test"}]"#,
                    "[]",
                ),
                rule(
                    "global/override/fixture.quiet_bots",
                    5,
                    0,
                    r#"[{"kind":"event_match","key":"sender","pattern":"@bot*"}]"#,
                    "[]",
                ),
                rule(
                    "global/override/fixture.louder",
                    5,
                    1,
                    "[]",
                    r#"["notify"]"#,
                ),
                rule(
                    "global/underride/.m.rule.message",
                    -1,
                    1,
                    "[]",
                    r#"["notify",{"set_tweak":"sound","value":"default"}]"#,
                ),
                rule("global/postcontent/x", 6, 0, "[]", "[]"),
            ],
            &[
                enable("global/override/.m.rule.suppress_notices", Some(0)),
                enable("global/content/lobbyword", Some(1)),
                enable("global/override/fixture.gone", None),
            ],
        );
        let kinds: Vec<(&str, &str)> = rules
            .custom
            .iter()
            .map(|(k, r)| (k.as_str(), r["rule_id"].as_str().unwrap()))
            .collect();
        assert_eq!(
            kinds,
            [
                ("override", "fixture.louder"),
                ("override", "fixture.quiet_bots"),
                ("content", "lobbyword"),
                ("room", "!dm:fixture.test"),
            ]
        );
        assert_eq!(rules.custom[2].1["pattern"], "lobby");
        assert!(rules.custom[3].1.get("conditions").is_none());
        assert_eq!(
            rules.default_actions,
            vec![(
                "underride".to_owned(),
                ".m.rule.message".to_owned(),
                json!(["notify", {"set_tweak": "sound", "value": "default"}])
            )]
        );
        assert_eq!(
            rules.enabled,
            vec![
                (
                    "override".to_owned(),
                    ".m.rule.suppress_notices".to_owned(),
                    false
                ),
                ("content".to_owned(), "lobbyword".to_owned(), true),
            ]
        );
        assert_eq!(rules.unreadable.len(), 1, "{:?}", rules.unreadable);
    }

    #[test]
    fn pushers_receipts_filters_and_backups_read_as_synapse_wrote_them() {
        let pusher = pusher(&json!({
            "id": 2, "user_name": "@alice:fixture.test", "access_token": null, "profile_tag": "",
            "kind": "http", "app_id": "org.example.fixture", "app_display_name": "Fixture",
            "device_display_name": "Alice's phone", "pushkey": "alice-pushkey", "ts": 1,
            "lang": "en", "data": r#"{"url":"https://push.fixture.test/_matrix/push/v1/notify","format":"event_id_only"}"#,
            "enabled": true, "device_id": "ALICEPHONE",
        }))
        .unwrap();
        assert_eq!(pusher.profile_tag, None);
        assert_eq!(pusher.data["format"], "event_id_only");

        let receipt = receipt(&json!({
            "stream_id": 3, "room_id": "!dm:fixture.test", "receipt_type": "m.read.private",
            "user_id": "@alice:fixture.test", "event_id": "$e", "thread_id": null,
            "data": r#"{"ts":1790833103751}"#,
        }))
        .unwrap();
        assert_eq!(receipt.ts, 1_790_833_103_751);
        assert_eq!(receipt.thread_id, None);

        let old = filter(
            &json!({"user_id": "bob", "filter_id": 0}),
            r#"{"event_fields":["type"]}"#,
            "fixture.test",
        )
        .unwrap();
        assert_eq!(old.user_id, "@bob:fixture.test");
        assert_eq!(old.filter_id, "0");
        let new = filter(
            &json!({"user_id": "alice", "full_user_id": "@alice:fixture.test", "filter_id": 3}),
            "{}",
            "elsewhere.test",
        )
        .unwrap();
        assert_eq!(new.user_id, "@alice:fixture.test");

        let deleted = backup_version(&json!({
            "user_id": "@alice:fixture.test", "version": 1, "algorithm": "m.megolm_backup.v1.curve25519-aes-sha2",
            "auth_data": r#"{"public_key":"k"}"#, "deleted": 1, "etag": null,
        }))
        .unwrap();
        assert!(deleted.deleted);
        assert_eq!(deleted.version, 1);
        let key = room_key(&json!({
            "user_id": "@alice:fixture.test", "room_id": "!lobby:fixture.test",
            "session_id": "s", "version": 2, "first_message_index": 1, "forwarded_count": 0,
            "is_verified": false, "session_data": r#"{"ciphertext":"c"}"#,
        }))
        .unwrap();
        assert_eq!(key.first_message_index, 1);
        assert_eq!(key.session_data["ciphertext"], "c");
    }
}
