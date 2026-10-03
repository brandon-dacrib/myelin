//! The appservice transaction body: `PUT /_matrix/app/v1/transactions/{txnId}`'s payload, built
//! with every key spelling `PLAN.md` Appendix B lists, gated exactly the way Synapse's own sender
//! gates them (`refs/synapse/synapse/appservice/api.py`'s `ApplicationServiceApi.push_bulk`,
//! behavioral reference only, no code copied) — see [`Transaction::to_wire_json`] for the full
//! gating table and the one place this crate deliberately goes further than Synapse's current
//! behavior.
//!
//! Generic over `serde_json::Value` for event payloads rather than `ruma`'s event types: the room
//! actor (track 04, `hs-room`) that will eventually feed this scheduler does not exist yet, and a
//! transaction's job here is just to carry whatever canonical JSON it is given, unchanged, to the
//! appservice — re-typing it through `ruma`'s event enums would only be able to reject shapes this
//! crate has no way to construct anyway.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// One to-device message queued for an appservice (MSC2409, and MSC4203 for `m.room.encrypted`).
/// Distinct from an ordinary to-device event because the appservice needs to know *which*
/// masqueradable user/device pair it was addressed to — a plain to-device event has no room to
/// carry that.
///
/// On the wire it is the to-device event itself with two more keys, exactly as Synapse sends it
/// and `mautrix-go` parses it (`event.Event`'s `to_user_id`/`to_device_id`):
/// `{"type": ..., "sender": ..., "content": {...}, "to_user_id": ..., "to_device_id": ...}`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToDeviceEntry {
    /// The recipient's full user ID.
    pub to_user_id: String,
    /// The recipient's device ID.
    pub to_device_id: String,
    /// The event itself (`{"type": ..., "sender": ..., "content": {...}}`), flattened into the
    /// same object as the two addressing keys.
    #[serde(flatten)]
    pub event: Value,
}

/// The `m.typing` ephemeral event for `room_id` as an appservice is sent it (Synapse's shape:
/// no `sender`, every currently typing user, whether or not the appservice's own).
#[must_use]
pub fn typing_event(room_id: &str, user_ids: &[String]) -> Value {
    serde_json::json!({
        "type": "m.typing",
        "room_id": room_id,
        "content": {"user_ids": user_ids},
    })
}

/// The `m.receipt` ephemeral event for `room_id` with `content` in the spec's own shape,
/// `{event_id: {receipt_type: {user_id: {"ts": ...}}}}` ([`receipt_content`]).
#[must_use]
pub fn receipt_event(room_id: &str, content: Value) -> Value {
    serde_json::json!({
        "type": "m.receipt",
        "room_id": room_id,
        "content": content,
    })
}

/// Builds `m.receipt` content from `(user_id, receipt type, event_id, ts)` rows: the spec's
/// `{event_id: {receipt_type: {user_id: {"ts": ts}}}}`.
#[must_use]
pub fn receipt_content<'a>(
    rows: impl IntoIterator<Item = (&'a str, &'a str, &'a str, u64)>,
) -> Value {
    let mut by_event: BTreeMap<&str, BTreeMap<&str, Map<String, Value>>> = BTreeMap::new();
    for (user_id, kind, event_id, ts) in rows {
        by_event
            .entry(event_id)
            .or_default()
            .entry(kind)
            .or_default()
            .insert(user_id.to_owned(), serde_json::json!({"ts": ts}));
    }
    Value::Object(
        by_event
            .into_iter()
            .map(|(event_id, kinds)| {
                let kinds: Map<String, Value> = kinds
                    .into_iter()
                    .map(|(kind, users)| (kind.to_owned(), Value::Object(users)))
                    .collect();
                (event_id.to_owned(), Value::Object(kinds))
            })
            .collect(),
    )
}

/// The `m.presence` ephemeral event for `user_id` with `content` in the spec's shape
/// (`presence`, `last_active_ago`, `currently_active`, `status_msg`; no `user_id` inside, as
/// Synapse's `format_user_presence_state(include_user_id=False)`).
#[must_use]
pub fn presence_event(user_id: &str, content: Value) -> Value {
    serde_json::json!({
        "type": "m.presence",
        "sender": user_id,
        "content": content,
    })
}

/// MSC3202's device-list change summary for one transaction.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct DeviceListsUpdate {
    /// Users whose device identity keys changed, or who now share an encrypted room with a
    /// masqueradable user, since the last transaction.
    pub changed: Vec<String>,
    /// Users who no longer share an encrypted room since the last transaction.
    pub left: Vec<String>,
}

impl DeviceListsUpdate {
    /// True if there is nothing to report.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.changed.is_empty() && self.left.is_empty()
    }
}

/// `user_id -> device_id -> algorithm -> count`.
pub type OneTimeKeysCount = BTreeMap<String, BTreeMap<String, BTreeMap<String, u64>>>;
/// `user_id -> device_id -> [algorithm]`.
pub type UnusedFallbackKeyTypes = BTreeMap<String, BTreeMap<String, Vec<String>>>;

/// One appservice transaction body, before it is addressed to a specific appservice (spelling
/// gating depends on that appservice's registration flags, applied in
/// [`Transaction::to_wire_json`]).
#[derive(Debug, Clone, Default)]
pub struct Transaction {
    /// Persistent timeline events, already-canonical JSON.
    pub events: Vec<Value>,
    /// Ephemeral data: typing, receipts, presence (`m.typing`/`m.receipt`/`m.presence` shaped
    /// objects).
    pub ephemeral: Vec<Value>,
    /// To-device messages (MSC4203).
    pub to_device: Vec<ToDeviceEntry>,
    /// MSC3202 device-list changes.
    pub device_lists: DeviceListsUpdate,
    /// MSC3202 one-time-key counts.
    pub one_time_keys_count: OneTimeKeysCount,
    /// MSC3202 unused fallback key types.
    pub unused_fallback_key_types: UnusedFallbackKeyTypes,
}

impl Transaction {
    /// True if this transaction carries nothing at all — the scheduler does not enqueue empty
    /// transactions (there is never a reason to spend a round trip on one).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.events.is_empty()
            && self.ephemeral.is_empty()
            && self.to_device.is_empty()
            && self.device_lists.is_empty()
            && self.one_time_keys_count.is_empty()
            && self.unused_fallback_key_types.is_empty()
    }

    /// Builds the wire JSON body for this transaction as it should be sent to an appservice whose
    /// registration requested `receive_ephemeral`/`push_ephemeral_legacy`/`msc3202` as given.
    ///
    /// Gating, matching Synapse's `push_bulk` exactly except where noted:
    ///
    /// - `events`: always present (Synapse always includes it too).
    /// - `ephemeral`: present iff `receive_ephemeral`, regardless of whether it is empty (matches
    ///   Synapse: the key's *presence* is the capability signal some bridge code checks for).
    /// - `de.sorunome.msc2409.ephemeral`: present iff `push_ephemeral_legacy`, same rule.
    /// - `to_device` **and** `de.sorunome.msc2409.to_device`: present iff `receive_ephemeral ||
    ///   push_ephemeral_legacy` (to-device piggybacks on ephemeral support in Synapse, since
    ///   MSC4203 was folded into MSC2409's rollout). **Divergence from Synapse's current
    ///   behavior, deliberately**: Synapse today sends only the legacy `de.sorunome.msc2409.
    ///   to_device` spelling (its source comments this is pending MSC4203's stable merge); this
    ///   crate sends both, since `PLAN.md` Appendix B records that `mautrix-go`'s parser already
    ///   accepts the stable name, and sending both costs nothing a bridge parser does not already
    ///   tolerate. Recorded in `docs/status/11-appservices-and-bridges.md`.
    /// - MSC3202 fields (`device_lists`, one-time-key counts, fallback key types): present only
    ///   when `msc3202` is set **and** the corresponding field is non-empty (matches Synapse:
    ///   these are conditional on content, unlike the ephemeral/to-device keys above). Each is
    ///   written under **both** the bare stable name and the `org.matrix.msc3202.`-prefixed
    ///   legacy name; the one-time-key count is additionally written under
    ///   `org.matrix.msc3202.device_one_time_key_counts` (note: `key_counts`, not
    ///   `keys_count`) — the older, still-sent spelling `refs/synapse/synapse/appservice/api.py`
    ///   emits alongside the current one, exactly as `PLAN.md` Appendix B documents ("Synapse also
    ///   sends the older ...").
    #[must_use]
    pub fn to_wire_json(
        &self,
        receive_ephemeral: bool,
        push_ephemeral_legacy: bool,
        msc3202: bool,
    ) -> Value {
        let mut body = Map::new();
        body.insert("events".to_string(), Value::Array(self.events.clone()));

        if receive_ephemeral {
            body.insert(
                "ephemeral".to_string(),
                Value::Array(self.ephemeral.clone()),
            );
        }
        if push_ephemeral_legacy {
            body.insert(
                "de.sorunome.msc2409.ephemeral".to_string(),
                Value::Array(self.ephemeral.clone()),
            );
        }

        if receive_ephemeral || push_ephemeral_legacy {
            let to_device: Vec<Value> = self
                .to_device
                .iter()
                .map(|e| serde_json::to_value(e).expect("ToDeviceEntry always serializes"))
                .collect();
            body.insert("to_device".to_string(), Value::Array(to_device.clone()));
            body.insert(
                "de.sorunome.msc2409.to_device".to_string(),
                Value::Array(to_device),
            );
        }

        if msc3202 {
            if !self.device_lists.is_empty() {
                let value = serde_json::to_value(&self.device_lists)
                    .expect("DeviceListsUpdate always serializes");
                body.insert("device_lists".to_string(), value.clone());
                body.insert("org.matrix.msc3202.device_lists".to_string(), value);
            }
            if !self.one_time_keys_count.is_empty() {
                let value = serde_json::to_value(&self.one_time_keys_count)
                    .expect("OneTimeKeysCount always serializes");
                body.insert("device_one_time_keys_count".to_string(), value.clone());
                body.insert(
                    "org.matrix.msc3202.device_one_time_keys_count".to_string(),
                    value.clone(),
                );
                body.insert(
                    "org.matrix.msc3202.device_one_time_key_counts".to_string(),
                    value,
                );
            }
            if !self.unused_fallback_key_types.is_empty() {
                let value = serde_json::to_value(&self.unused_fallback_key_types)
                    .expect("UnusedFallbackKeyTypes always serializes");
                body.insert(
                    "device_unused_fallback_key_types".to_string(),
                    value.clone(),
                );
                body.insert(
                    "org.matrix.msc3202.device_unused_fallback_key_types".to_string(),
                    value,
                );
            }
        }

        Value::Object(body)
    }
}

/// What one wire body carries, by kind, for the delivery log line and the
/// `hs_appservice_delivered_items` counter ([`crate::metrics`]). Counted from the body as sent
/// ([`body_counts`]), so a batch of several queue entries is counted once, as one transaction.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BodyCounts {
    /// Timeline events.
    pub events: usize,
    /// `m.typing` ephemeral events.
    pub typing: usize,
    /// `m.receipt` ephemeral events.
    pub receipts: usize,
    /// `m.presence` ephemeral events.
    pub presence: usize,
    /// To-device messages.
    pub to_device: usize,
    /// Users in `device_lists.changed` plus `device_lists.left`.
    pub device_list_changes: usize,
    /// Devices with a one-time-key count.
    pub one_time_key_counts: usize,
    /// Devices with an unused fallback key type list.
    pub fallback_key_types: usize,
}

impl BodyCounts {
    /// Every kind with its count, in a fixed order, for iterating the metric labels.
    #[must_use]
    pub fn by_kind(&self) -> [(&'static str, usize); 8] {
        [
            ("events", self.events),
            ("typing", self.typing),
            ("receipts", self.receipts),
            ("presence", self.presence),
            ("to_device", self.to_device),
            ("device_list_changes", self.device_list_changes),
            ("one_time_key_counts", self.one_time_key_counts),
            ("fallback_key_types", self.fallback_key_types),
        ]
    }
}

/// One `m.room_key.withheld` found in a wire body by [`key_withheld_in`]: a client telling one
/// of the appservice's devices that it will not get a room's keys, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyWithheld {
    /// The client's user.
    pub sender: String,
    /// The event's `code` (`m.unverified`, `m.blacklisted`, `m.unauthorised`, `m.unavailable`,
    /// `m.no_olm`), or `unknown` when it carries none.
    pub code: String,
    /// The event's `reason`, when it carried one.
    pub reason: Option<String>,
    /// The event's `room_id`, when it carried one.
    pub room_id: Option<String>,
    /// The appservice user it was addressed to (the MSC4203 addressing key).
    pub to_user_id: String,
    /// The device it was addressed to.
    pub to_device_id: String,
}

/// The `m.room_key.withheld` to-device events `body` (a wire body, possibly several merged)
/// carries, in order: the stable `to_device` key where present, the legacy one otherwise, as
/// [`body_counts`] reads them. A bridge that is sent one answers the person that their message
/// was not bridged; the scheduler keeps the last one in the appservice's health so that an
/// operator sees the same without the bridge's log.
#[must_use]
pub fn key_withheld_in(body: &Value) -> Vec<KeyWithheld> {
    body.get("to_device")
        .or_else(|| body.get("de.sorunome.msc2409.to_device"))
        .and_then(Value::as_array)
        .map_or(&[][..], Vec::as_slice)
        .iter()
        .filter(|e| e.get("type").and_then(Value::as_str) == Some("m.room_key.withheld"))
        .map(|e| {
            let content = e.get("content").cloned().unwrap_or(Value::Null);
            let text = |v: &Value, key: &str| v.get(key).and_then(Value::as_str).map(str::to_owned);
            KeyWithheld {
                sender: text(e, "sender").unwrap_or_default(),
                code: text(&content, "code").unwrap_or_else(|| "unknown".to_owned()),
                reason: text(&content, "reason"),
                room_id: text(&content, "room_id"),
                to_user_id: text(e, "to_user_id").unwrap_or_default(),
                to_device_id: text(e, "to_device_id").unwrap_or_default(),
            }
        })
        .collect()
}

/// Counts what `body` (a wire body from [`Transaction::to_wire_json`], possibly several merged
/// by the scheduler) carries. The stable key is read where present, the legacy one otherwise,
/// so a body gated to either spelling counts the same.
#[must_use]
pub fn body_counts(body: &Value) -> BodyCounts {
    let array = |stable: &str, legacy: &str| -> &[Value] {
        body.get(stable)
            .or_else(|| body.get(legacy))
            .and_then(Value::as_array)
            .map_or(&[], Vec::as_slice)
    };
    let devices_in = |stable: &str, legacy: &str| -> usize {
        body.get(stable)
            .or_else(|| body.get(legacy))
            .and_then(Value::as_object)
            .map_or(0, |users| {
                users
                    .values()
                    .filter_map(Value::as_object)
                    .map(Map::len)
                    .sum()
            })
    };
    let ephemeral = array("ephemeral", "de.sorunome.msc2409.ephemeral");
    let of_type = |t: &str| {
        ephemeral
            .iter()
            .filter(|e| e.get("type").and_then(Value::as_str) == Some(t))
            .count()
    };
    let device_lists = body
        .get("device_lists")
        .or_else(|| body.get("org.matrix.msc3202.device_lists"));
    let device_list_changes = ["changed", "left"]
        .iter()
        .map(|key| {
            device_lists
                .and_then(|dl| dl.get(*key))
                .and_then(Value::as_array)
                .map_or(0, Vec::len)
        })
        .sum();
    BodyCounts {
        events: body
            .get("events")
            .and_then(Value::as_array)
            .map_or(0, Vec::len),
        typing: of_type("m.typing"),
        receipts: of_type("m.receipt"),
        presence: of_type("m.presence"),
        to_device: array("to_device", "de.sorunome.msc2409.to_device").len(),
        device_list_changes,
        one_time_key_counts: devices_in(
            "device_one_time_keys_count",
            "org.matrix.msc3202.device_one_time_keys_count",
        ),
        fallback_key_types: devices_in(
            "device_unused_fallback_key_types",
            "org.matrix.msc3202.device_unused_fallback_key_types",
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn body_counts_read_either_spelling_once() {
        let mut otk = OneTimeKeysCount::new();
        otk.entry("@bot:x".to_string()).or_default().insert(
            "DEV".to_string(),
            BTreeMap::from([("signed_curve25519".to_string(), 3)]),
        );
        let txn = Transaction {
            events: sample_events(),
            ephemeral: vec![
                typing_event("!r:x", &[]),
                receipt_event("!r:x", json!({})),
                receipt_event("!s:x", json!({})),
                presence_event("@a:x", json!({})),
            ],
            to_device: vec![ToDeviceEntry {
                to_user_id: "@bot:x".to_string(),
                to_device_id: "DEV".to_string(),
                event: json!({"type": "m.room_key"}),
            }],
            device_lists: DeviceListsUpdate {
                changed: vec!["@a:x".to_string(), "@b:x".to_string()],
                left: vec![],
            },
            one_time_keys_count: otk,
            ..Default::default()
        };
        let expected = BodyCounts {
            events: 1,
            typing: 1,
            receipts: 2,
            presence: 1,
            to_device: 1,
            device_list_changes: 2,
            one_time_key_counts: 1,
            fallback_key_types: 0,
        };
        assert_eq!(body_counts(&txn.to_wire_json(true, true, true)), expected);
        assert_eq!(body_counts(&txn.to_wire_json(false, true, true)), expected);
        // Gated off: not counted, because not sent.
        assert_eq!(
            body_counts(&txn.to_wire_json(false, false, false)),
            BodyCounts {
                events: 1,
                ..BodyCounts::default()
            }
        );
    }

    fn sample_events() -> Vec<Value> {
        vec![json!({"type": "m.room.message", "event_id": "$1", "sender": "@a:x", "content": {}})]
    }

    #[test]
    fn plain_transaction_with_no_flags_only_has_events() {
        let txn = Transaction {
            events: sample_events(),
            ..Default::default()
        };
        let body = txn.to_wire_json(false, false, false);
        let obj = body.as_object().unwrap();
        assert_eq!(obj.len(), 1);
        assert!(obj.contains_key("events"));
    }

    #[test]
    fn stable_ephemeral_flag_only_emits_stable_key() {
        let txn = Transaction {
            ephemeral: vec![json!({"type": "m.typing"})],
            ..Default::default()
        };
        let body = txn.to_wire_json(true, false, false);
        let obj = body.as_object().unwrap();
        assert!(obj.contains_key("ephemeral"));
        assert!(!obj.contains_key("de.sorunome.msc2409.ephemeral"));
        assert!(obj.contains_key("to_device"));
        assert!(obj.contains_key("de.sorunome.msc2409.to_device"));
    }

    #[test]
    fn legacy_ephemeral_flag_only_emits_legacy_key() {
        let txn = Transaction {
            ephemeral: vec![json!({"type": "m.typing"})],
            ..Default::default()
        };
        let body = txn.to_wire_json(false, true, false);
        let obj = body.as_object().unwrap();
        assert!(!obj.contains_key("ephemeral"));
        assert!(obj.contains_key("de.sorunome.msc2409.ephemeral"));
    }

    #[test]
    fn ephemeral_key_present_even_when_list_is_empty_matching_synapse() {
        let txn = Transaction::default();
        let body = txn.to_wire_json(true, false, false);
        let obj = body.as_object().unwrap();
        assert_eq!(obj.get("ephemeral").unwrap(), &Value::Array(vec![]));
    }

    #[test]
    fn no_ephemeral_flags_means_no_to_device_either() {
        let txn = Transaction {
            to_device: vec![ToDeviceEntry {
                to_user_id: "@bob:x".to_string(),
                to_device_id: "DEV".to_string(),
                event: json!({"type": "m.room_key"}),
            }],
            ..Default::default()
        };
        let body = txn.to_wire_json(false, false, false);
        let obj = body.as_object().unwrap();
        assert!(!obj.contains_key("to_device"));
        assert!(!obj.contains_key("de.sorunome.msc2409.to_device"));
    }

    #[test]
    fn msc3202_disabled_suppresses_device_fields_even_if_present() {
        let mut otk = OneTimeKeysCount::new();
        otk.entry("@bob:x".to_string())
            .or_default()
            .entry("DEV".to_string())
            .or_default()
            .insert("signed_curve25519".to_string(), 5);
        let txn = Transaction {
            one_time_keys_count: otk,
            ..Default::default()
        };
        let body = txn.to_wire_json(true, true, false);
        let obj = body.as_object().unwrap();
        assert!(!obj.contains_key("device_one_time_keys_count"));
    }

    #[test]
    fn msc3202_one_time_keys_count_uses_all_three_documented_spellings() {
        let mut otk = OneTimeKeysCount::new();
        otk.entry("@bob:x".to_string())
            .or_default()
            .entry("DEV".to_string())
            .or_default()
            .insert("signed_curve25519".to_string(), 5);
        let txn = Transaction {
            one_time_keys_count: otk,
            ..Default::default()
        };
        let body = txn.to_wire_json(false, false, true);
        let obj = body.as_object().unwrap();
        let expected = json!({"@bob:x": {"DEV": {"signed_curve25519": 5}}});
        assert_eq!(obj.get("device_one_time_keys_count"), Some(&expected));
        assert_eq!(
            obj.get("org.matrix.msc3202.device_one_time_keys_count"),
            Some(&expected)
        );
        assert_eq!(
            obj.get("org.matrix.msc3202.device_one_time_key_counts"),
            Some(&expected)
        );
    }

    #[test]
    fn msc3202_fields_absent_when_empty_even_if_enabled() {
        let txn = Transaction::default();
        let body = txn.to_wire_json(false, false, true);
        let obj = body.as_object().unwrap();
        assert!(!obj.contains_key("device_lists"));
        assert!(!obj.contains_key("device_one_time_keys_count"));
        assert!(!obj.contains_key("device_unused_fallback_key_types"));
    }

    #[test]
    fn msc3202_device_lists_and_fallback_keys_use_stable_and_legacy_spellings() {
        let mut fallback = UnusedFallbackKeyTypes::new();
        fallback
            .entry("@bob:x".to_string())
            .or_default()
            .insert("DEV".to_string(), vec!["signed_curve25519".to_string()]);
        let txn = Transaction {
            device_lists: DeviceListsUpdate {
                changed: vec!["@bob:x".to_string()],
                left: vec![],
            },
            unused_fallback_key_types: fallback,
            ..Default::default()
        };
        let body = txn.to_wire_json(false, false, true);
        let obj = body.as_object().unwrap();
        let dl_expected = json!({"changed": ["@bob:x"], "left": []});
        assert_eq!(obj.get("device_lists"), Some(&dl_expected));
        assert_eq!(
            obj.get("org.matrix.msc3202.device_lists"),
            Some(&dl_expected)
        );
        assert!(obj.contains_key("device_unused_fallback_key_types"));
        assert!(obj.contains_key("org.matrix.msc3202.device_unused_fallback_key_types"));
    }

    #[test]
    fn a_to_device_entry_is_the_event_with_its_addressing_keys_flattened_in() {
        let txn = Transaction {
            to_device: vec![ToDeviceEntry {
                to_user_id: "@ghost:x".to_string(),
                to_device_id: "DEV".to_string(),
                event: json!({"type": "m.room_key", "sender": "@alice:x", "content": {"k": 1}}),
            }],
            ..Default::default()
        };
        let body = txn.to_wire_json(true, false, false);
        assert_eq!(
            body["to_device"][0],
            json!({
                "type": "m.room_key",
                "sender": "@alice:x",
                "content": {"k": 1},
                "to_user_id": "@ghost:x",
                "to_device_id": "DEV",
            })
        );
    }

    #[test]
    fn the_ephemeral_shapes_are_synapses() {
        assert_eq!(
            typing_event("!r:x", &["@a:x".to_owned(), "@b:x".to_owned()]),
            json!({"type": "m.typing", "room_id": "!r:x", "content": {"user_ids": ["@a:x", "@b:x"]}})
        );
        let content = receipt_content([
            ("@a:x", "m.read", "$one", 5),
            ("@b:x", "m.read", "$one", 6),
            ("@a:x", "m.read.private", "$two", 7),
        ]);
        assert_eq!(
            content,
            json!({
                "$one": {"m.read": {"@a:x": {"ts": 5}, "@b:x": {"ts": 6}}},
                "$two": {"m.read.private": {"@a:x": {"ts": 7}}},
            })
        );
        assert_eq!(
            receipt_event("!r:x", content.clone()),
            json!({"type": "m.receipt", "room_id": "!r:x", "content": content})
        );
        assert_eq!(
            presence_event("@a:x", json!({"presence": "online"})),
            json!({"type": "m.presence", "sender": "@a:x", "content": {"presence": "online"}})
        );
    }

    #[test]
    fn is_empty_reflects_every_field() {
        assert!(Transaction::default().is_empty());
        let with_event = Transaction {
            events: sample_events(),
            ..Default::default()
        };
        assert!(!with_event.is_empty());
    }

    #[test]
    fn key_withheld_in_reads_the_withheld_events_under_either_spelling() {
        let withheld = json!({
            "type": "m.room_key.withheld",
            "sender": "@alice:x",
            "content": {
                "algorithm": "m.megolm.v1.aes-sha2",
                "code": "m.unverified",
                "reason": "The sender has disabled encrypting to unverified devices.",
                "room_id": "!chat:x",
                "sender_key": "k",
                "session_id": "s",
            },
            "to_user_id": "@whatsappbot_alice:x",
            "to_device_id": "IEXNEKZESJ",
        });
        let request = json!({
            "type": "m.room_key_request", "sender": "@alice:x", "content": {},
            "to_user_id": "@whatsappbot_alice:x", "to_device_id": "IEXNEKZESJ",
        });
        let found = key_withheld_in(&json!({"to_device": [request, withheld]}));
        assert_eq!(
            found,
            vec![KeyWithheld {
                sender: "@alice:x".into(),
                code: "m.unverified".into(),
                reason: Some("The sender has disabled encrypting to unverified devices.".into()),
                room_id: Some("!chat:x".into()),
                to_user_id: "@whatsappbot_alice:x".into(),
                to_device_id: "IEXNEKZESJ".into(),
            }]
        );
        // The legacy key alone, and an event with no code.
        let bare = json!({"type": "m.room_key.withheld", "sender": "@b:x", "content": {}});
        let found = key_withheld_in(&json!({"de.sorunome.msc2409.to_device": [bare]}));
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].code, "unknown");
        assert_eq!(found[0].to_device_id, "");
        assert!(key_withheld_in(&json!({"events": []})).is_empty());
    }
}
