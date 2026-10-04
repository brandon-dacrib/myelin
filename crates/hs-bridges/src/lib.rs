//! `hs-bridges`: bridge offerings and per-user bridge instances (RFC 0017,
//! `docs/rfcs/0017-the-server-deploys-its-own-bridges.md`).
//!
//! - [`store`]: offerings, instances and the manager's rooms, in `hs-kv`.
//! - [`runtime`]: the seam to wherever instances run, and the manifest for running one on
//!   another cluster.
//! - [`matrix`]: the client API over loopback, as the manager's bots and an instance's bot.
//! - [`manager`]: [`manager::BridgeManager`], the admin API's data source for offerings and
//!   instances, and the state machine that takes an instance from requested to ready.
//! - [`front_door`]: the manager's appservice API, and what `@whatsappbot` and `@bridges` say.
//! - [`cross_signing`]: the cross-signing identity the manager keeps for each instance's bot,
//!   so that a client which excludes insecure devices still shares keys with the bridge.
//! - [`overlap`] and [`directory`]: a bridge registered by hand for a network this server now
//!   offers is named on its health and on the offering, with what to do (RFC 0017 section 6).

#![forbid(unsafe_code)]

pub mod cross_signing;
pub mod directory;
pub mod front_door;
pub mod manager;
pub mod matrix;
pub mod overlap;
pub mod runtime;
pub mod store;

/// `bytes` random bytes, hex-encoded.
#[must_use]
pub fn random_hex(bytes: usize) -> String {
    use rand::RngCore;
    let mut buf = vec![0u8; bytes];
    rand::rng().fill_bytes(&mut buf);
    hex::encode(buf)
}
