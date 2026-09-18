//! `hs-e2e`: server-side end-to-end encryption support (track 08,
//! `docs/workstreams/08-e2ee.md`).
//!
//! This crate is key distribution and device tracking only — it never inspects or interprets
//! encrypted message content, only the metadata clients need to establish and maintain encrypted
//! sessions: device identity keys, one-time and fallback keys (with an atomic claim that makes a
//! double claim impossible by construction — see [`store`]'s module docs), cross-signing keys,
//! the device-list change stream, key backups, and to-device messaging.
//!
//! - [`store`]: the storage traits ([`store::DeviceKeyStore`], [`store::OneTimeKeyStore`],
//!   [`store::FallbackKeyStore`], [`store::CrossSigningStore`], [`store::BackupStore`],
//!   [`store::ToDeviceStore`]) and their `hs-kv`/`hs-tables`-backed implementation
//!   ([`store::tables::TablesE2eStore`]).
//! - [`state`]: [`state::E2eState`], this crate's axum shared state, and [`state::E2eRequester`],
//!   the bridge that lets a handler mounted on `Router<E2eState<B>>` take
//!   [`hs_auth::requester::Requester`] as a parameter (same pattern as `hs-room`'s
//!   `RoomState`/`RoomRequester`).
//! - [`error`]: [`error::E2eError`], mapped to `hs-http`'s Matrix error shape.
//! - [`routes`]: the client-server HTTP endpoints, as a router fragment.
//! - [`appservice_feed`]: functions that turn this crate's storage into the
//!   `hs_appservice::transaction::Transaction` fields (MSC3202 one-time-key counts, unused
//!   fallback key types, device-list changes) track 11's scheduler already has fields for.

pub mod appservice_feed;
pub mod error;
pub mod routes;
pub mod state;
pub mod store;
