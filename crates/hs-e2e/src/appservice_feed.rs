//! Turns this crate's storage into the `hs_appservice::transaction::Transaction` fields track
//! 11's scheduler already has fields for (MSC3202): one-time-key counts, unused fallback key
//! types, and device-list changes.
//!
//! This module is deliberately *pull*-shaped, matching how `hs_appservice::scheduler::Scheduler`
//! itself is used: `Scheduler::enqueue` takes an already-built `Transaction` (see
//! `crates/hs-appservice/src/scheduler.rs`'s `enqueue`, read before writing this); nothing in
//! that crate calls back into a key-count source. Whatever crate wires an appservice's outbound
//! transaction together each poll cycle (today no crate does this end to end — see
//! `docs/status/11-appservices-and-bridges.md`) calls
//! [`one_time_key_counts_for`]/[`unused_fallback_key_types_for`]/[`device_list_update_for`] with
//! the set of masqueradable (user, device) pairs and users the appservice's registration
//! namespaces cover, and assigns the results onto a
//! [`hs_appservice::transaction::Transaction`]'s corresponding fields before calling
//! `Scheduler::enqueue`. This crate does not itself decide *which* devices an appservice cares
//! about — that is `hs-appservice`'s namespace-matching job (`hs_appservice::namespace`) — so
//! every function here takes the already-resolved list as input rather than a registration.
//!
//! The MSC3983 (`POST .../org.matrix.msc3983/keys/claim`) and MSC3984
//! (`POST .../org.matrix.msc3984/keys/query`) appservice-facing HTTP proxies themselves are
//! [`crate::routes::appservice_proxy`], not this module: those are inbound requests *from* an
//! appservice asking this server to claim/query keys, which reuse the same
//! [`crate::store::OneTimeKeyStore`]/[`crate::store::DeviceKeyStore`] methods the ordinary
//! `/keys/claim`/`/keys/query` handlers use. This module is the other direction: outbound
//! transaction fields this server pushes to the appservice unprompted.

use std::collections::BTreeMap;

use hs_appservice::transaction::{DeviceListsUpdate, OneTimeKeysCount, UnusedFallbackKeyTypes};
use ruma::{OwnedDeviceId, OwnedUserId, UserId};

use crate::store::{DeviceKeyStore, FallbackKeyStore, OneTimeKeyStore};

/// Builds MSC3202's `device_one_time_keys_count` field for the given `(user, device)` pairs
/// (typically every masqueradable device an appservice's registration namespaces cover).
///
/// # Errors
/// Returns the first storage error encountered.
pub async fn one_time_key_counts_for<S: OneTimeKeyStore + ?Sized>(
    store: &S,
    devices: &[(OwnedUserId, OwnedDeviceId)],
) -> Result<OneTimeKeysCount, crate::store::StoreError> {
    let mut out = OneTimeKeysCount::new();
    for (user, device) in devices {
        let counts = store.count_one_time_keys(user, device).await?;
        if counts.is_empty() {
            continue;
        }
        let counts_u64: BTreeMap<String, u64> = counts.into_iter().collect();
        out.entry(user.to_string())
            .or_default()
            .insert(device.to_string(), counts_u64);
    }
    Ok(out)
}

/// Builds MSC3202's `device_unused_fallback_key_types` field for the given `(user, device)`
/// pairs.
///
/// # Errors
/// Returns the first storage error encountered.
pub async fn unused_fallback_key_types_for<S: FallbackKeyStore + ?Sized>(
    store: &S,
    devices: &[(OwnedUserId, OwnedDeviceId)],
) -> Result<UnusedFallbackKeyTypes, crate::store::StoreError> {
    let mut out = UnusedFallbackKeyTypes::new();
    for (user, device) in devices {
        let algorithms = store.unused_fallback_key_algorithms(user, device).await?;
        if algorithms.is_empty() {
            continue;
        }
        out.entry(user.to_string())
            .or_default()
            .insert(device.to_string(), algorithms);
    }
    Ok(out)
}

/// Builds MSC3202's `device_lists` field: users covered by the appservice (`interested`) whose
/// device list changed with a stream position greater than `since`.
///
/// This crate has no notion of room membership, so it cannot itself compute the `left` half of
/// [`DeviceListsUpdate`] (users who stopped sharing an encrypted room with a masqueradable user)
/// — that is inherently a room-membership fact `hs-room`/`hs-user` own. Callers that can resolve
/// membership pass it via `left`; callers that cannot (there is no such integration yet) pass an
/// empty slice, which is a safe under-report, not a wrong one: the appservice simply resyncs a
/// device list it did not strictly need to. Documented as a seam in `docs/status/08-e2ee.md`.
///
/// # Errors
/// Returns the first storage error encountered.
pub async fn device_list_update_for<S: DeviceKeyStore + ?Sized>(
    store: &S,
    since: u64,
    interested: impl Fn(&UserId) -> bool,
    left: &[OwnedUserId],
) -> Result<DeviceListsUpdate, crate::store::StoreError> {
    let changed = store.changed_users_since(since, None).await?;
    Ok(DeviceListsUpdate {
        changed: changed
            .into_iter()
            .filter(|u| interested(u))
            .map(|u| u.to_string())
            .collect(),
        left: left.iter().map(ToString::to_string).collect(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::tables::TablesE2eStore;
    use hs_kv::memory::MemoryBackend;

    fn uid(s: &str) -> ruma::OwnedUserId {
        ruma::UserId::parse(s).unwrap().to_owned()
    }

    fn did(s: &str) -> ruma::OwnedDeviceId {
        ruma::OwnedDeviceId::from(s)
    }

    #[tokio::test]
    async fn feeds_match_the_underlying_store() {
        let store = TablesE2eStore::open(MemoryBackend::new()).unwrap();
        let user = uid("@bridge_ghost:example.org");
        let device = did("BRIDGE1");
        store
            .upload_device_keys(&user, &device, serde_json::json!({}))
            .await
            .unwrap();
        let mut otk = std::collections::BTreeMap::new();
        otk.insert(
            "signed_curve25519:K1".to_string(),
            serde_json::json!({"key": "x"}),
        );
        store.upload_one_time_keys(&user, &device, otk).await.unwrap();
        let mut fb = std::collections::BTreeMap::new();
        fb.insert(
            "signed_curve25519:FB1".to_string(),
            serde_json::json!({"key": "x", "fallback": true}),
        );
        store.upload_fallback_keys(&user, &device, fb).await.unwrap();

        let devices = vec![(user.clone(), device.clone())];
        let counts = one_time_key_counts_for(&store, &devices).await.unwrap();
        assert_eq!(counts[user.as_str()][device.as_str()]["signed_curve25519"], 1);

        let fallback = unused_fallback_key_types_for(&store, &devices).await.unwrap();
        assert_eq!(
            fallback[user.as_str()][device.as_str()],
            vec!["signed_curve25519".to_string()]
        );

        let interested = |_: &UserId| true;
        let left: Vec<ruma::OwnedUserId> = Vec::new();
        let dl = device_list_update_for(&store, 0, interested, &left)
            .await
            .unwrap();
        assert!(dl.changed.contains(&user.to_string()));
    }
}
