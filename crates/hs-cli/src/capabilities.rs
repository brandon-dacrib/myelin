//! `GET /_matrix/client/v3/capabilities`: what this server's authenticated account-management
//! surface actually supports. Every `mautrix-go`/`bridgev2` bridge calls this immediately after
//! `login`/`register`/`whoami` (`PLAN.md` Appendix B, "Client endpoints called"), and it was
//! 404ing before this change.
//!
//! Reported honestly against what `hs serve` actually mounts today
//! (`crates/hs-cli/src/serve.rs::build_router`): `m.change_password` is `true` (`POST
//! /account/password` is `hs-auth`'s own route and is mounted). `m.set_displayname`,
//! `m.set_avatar_url` and `m.3pid_changes` are `false` — no profile or 3PID-management HTTP
//! routes are mounted yet, even though `hs-auth::store::UserStore` has `bind_threepid` at the
//! storage-trait level (nothing exposes it over HTTP). Update this alongside `crate::versions`'s
//! `unstable_features` as more routers get mounted here, and note the addition in
//! `docs/status/12-platform-and-kubernetes.md`.
//!
//! # `m.room_versions`
//!
//! This was omitted entirely until `hs-room` was mounted, on the reasoning that a server with no
//! room-creation route must not name a room version it cannot create a room of. `POST /createRoom`
//! is mounted now, so the claim is reported — and generated from the same table the room actor
//! validates against (`hs_model::room_version::known_room_version_ids`), so a version this server
//! genuinely supports can never drift out of what it advertises.
//!
//! The **default** is the one value with two sources of truth: `hs-room`'s actor picks `"11"` for
//! a `/createRoom` request that names no version
//! (`crates/hs-room/src/actor.rs`, `request.room_version.unwrap_or_else(...)`), and that crate
//! exports no constant to read it from. [`DEFAULT_ROOM_VERSION`] below mirrors it, and
//! [`tests::advertised_default_is_a_room_version_this_server_knows`] fails if it is ever set to
//! something the rules table does not know. Changing the server's default means changing both;
//! the right fix is a `pub const` in `hs-room` that both read, which belongs to that track.
//!
//! Every known version is advertised as `"stable"`. That is what "this server will create one if
//! asked" means here; none of the versions in the table are experimental drafts.

use axum::Json;
use serde_json::{Map, Value, json};

/// The room version `hs-room` creates a room with when the request names none. Mirrors
/// `crates/hs-room/src/actor.rs`; see this module's doc comment for why it is duplicated and what
/// would remove the duplication.
pub const DEFAULT_ROOM_VERSION: &str = "11";

/// `{"<version>": "stable", ...}` for every room version this server's rules table knows, which
/// is exactly the set `hs_room::actor` will accept in a `/createRoom` request.
fn available_room_versions() -> Map<String, Value> {
    hs_model::room_version::known_room_version_ids()
        .map(|id| (id.to_owned(), Value::from("stable")))
        .collect()
}

/// `GET /_matrix/client/v3/capabilities` handler.
pub async fn get_capabilities() -> Json<Value> {
    Json(json!({
        "capabilities": {
            "m.change_password": {"enabled": true},
            "m.set_displayname": {"enabled": false},
            "m.set_avatar_url": {"enabled": false},
            "m.3pid_changes": {"enabled": false},
            "m.room_versions": {
                "default": DEFAULT_ROOM_VERSION,
                "available": available_room_versions()
            }
        }
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn reports_change_password_enabled_and_profile_routes_absent() {
        let Json(body) = get_capabilities().await;
        assert_eq!(body["capabilities"]["m.change_password"]["enabled"], true);
        assert_eq!(body["capabilities"]["m.set_displayname"]["enabled"], false);
        assert_eq!(body["capabilities"]["m.3pid_changes"]["enabled"], false);
    }

    #[tokio::test]
    async fn advertises_every_room_version_the_rules_table_knows() {
        let Json(body) = get_capabilities().await;
        let available = body["capabilities"]["m.room_versions"]["available"]
            .as_object()
            .expect("available is an object");
        for id in hs_model::room_version::known_room_version_ids() {
            assert_eq!(
                available.get(id).and_then(Value::as_str),
                Some("stable"),
                "room version {id} is supported but not advertised"
            );
        }
        assert_eq!(available.len(), available_room_versions().len());
    }

    #[test]
    fn advertised_default_is_a_room_version_this_server_knows() {
        // Guards the duplication this module's doc comment describes: if `hs-room`'s default
        // moves and `DEFAULT_ROOM_VERSION` is updated to something the rules table does not
        // implement, a client would be told to create rooms this server would then refuse.
        let id = ruma::RoomVersionId::try_from(DEFAULT_ROOM_VERSION)
            .expect("the advertised default parses as a room version");
        assert!(
            hs_model::room_version::rules_for(&id).is_some(),
            "the advertised default room version {DEFAULT_ROOM_VERSION} has no rules"
        );
    }

    #[tokio::test]
    async fn default_is_advertised_among_the_available_versions() {
        let Json(body) = get_capabilities().await;
        let versions = &body["capabilities"]["m.room_versions"];
        let default = versions["default"].as_str().unwrap();
        assert!(
            versions["available"].get(default).is_some(),
            "the default {default} is not in the available map"
        );
    }
}
