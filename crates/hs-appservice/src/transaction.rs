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

/// One to-device message queued for an appservice (MSC4203). Distinct from an ordinary to-device
/// event because the appservice needs to know *which* masqueradable user/device pair it was
/// addressed to — a plain to-device event has no room to carry that.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToDeviceEntry {
    /// The recipient's full user ID.
    pub to_user_id: String,
    /// The recipient's device ID.
    pub to_device_id: String,
    /// The event itself (`{"type": ..., "sender": ..., "content": {...}}`).
    pub event: Value,
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

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
    fn is_empty_reflects_every_field() {
        assert!(Transaction::default().is_empty());
        let with_event = Transaction {
            events: sample_events(),
            ..Default::default()
        };
        assert!(!with_event.is_empty());
    }
}
