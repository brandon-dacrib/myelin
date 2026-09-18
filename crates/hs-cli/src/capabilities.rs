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
//! storage-trait level (nothing exposes it over HTTP). `m.room_versions` is omitted entirely
//! rather than naming a default room version this server cannot actually create a room of: no
//! room-creation route exists in what `hs serve` mounts. Update this alongside
//! `crate::versions`'s `unstable_features` as more routers get mounted here, and note the
//! addition in `docs/status/12-platform-and-kubernetes.md`.

use axum::Json;
use serde_json::{Value, json};

/// `GET /_matrix/client/v3/capabilities` handler.
pub async fn get_capabilities() -> Json<Value> {
    Json(json!({
        "capabilities": {
            "m.change_password": {"enabled": true},
            "m.set_displayname": {"enabled": false},
            "m.set_avatar_url": {"enabled": false},
            "m.3pid_changes": {"enabled": false}
        }
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn reports_change_password_enabled_and_no_room_versions_claim() {
        let Json(body) = get_capabilities().await;
        assert_eq!(body["capabilities"]["m.change_password"]["enabled"], true);
        assert_eq!(body["capabilities"]["m.set_displayname"]["enabled"], false);
        assert!(body["capabilities"].get("m.room_versions").is_none());
    }
}
