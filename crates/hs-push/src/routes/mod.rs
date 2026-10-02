//! The client-server HTTP endpoints this crate owns, as a router fragment. Mount [`router`] under
//! `/_matrix/client/v3` (and the historical `r0` alias), the way `hs-auth`, `hs-room` and `hs-e2e`
//! do. See `docs/status/10-push.md`'s "Interfaces provided" for the exact
//! `crates/hs-cli/src/serve.rs` wiring this expects.
//!
//! See [`pushrules`]' module docs for the `/pushrules` path grammar and which paths answer
//! `400`.

pub mod notifications;
pub mod pushers;
pub mod pushrules;
#[cfg(test)]
mod tests;

use hs_http::router::{AuthKind, Builder, RouteManifest, RouteMeta, Surface};
use hs_kv::KvBackend;

use crate::state::PushState;

fn matrix_client(operation_id: &str) -> RouteMeta {
    RouteMeta::new(Surface::MatrixClient, AuthKind::Matrix).with_operation_id(operation_id)
}

/// The spec-relative (no version prefix) client-server router fragment: push rules in every form,
/// pusher registration, and the notification log.
pub fn router<B: KvBackend + 'static>() -> (axum::Router<PushState<B>>, RouteManifest) {
    // Paths that are not in the spec answer `400 M_UNRECOGNIZED` (see `pushrules`' module docs);
    // they are registered under the admin surface so the spec coverage tool does not count them
    // as client-server routes the spec lacks.
    let malformed = || RouteMeta::new(Surface::Admin, AuthKind::None);
    Builder::new()
        .get(
            "/pushrules/",
            pushrules::get_pushrules_all::<B>,
            matrix_client("getPushRules"),
        )
        .put("/pushrules/", pushrules::malformed, malformed())
        .delete("/pushrules/", pushrules::malformed, malformed())
        .get("/pushrules/{scope}", pushrules::malformed, malformed())
        .put("/pushrules/{scope}", pushrules::malformed, malformed())
        .delete("/pushrules/{scope}", pushrules::malformed, malformed())
        .get(
            "/pushrules/global/",
            pushrules::get_pushrules_global::<B>,
            matrix_client("getPushRulesGlobal"),
        )
        .get(
            "/pushrules/{scope}/",
            pushrules::get_pushrules_scope::<B>,
            malformed(),
        )
        .put("/pushrules/{scope}/", pushrules::malformed, malformed())
        .delete("/pushrules/{scope}/", pushrules::malformed, malformed())
        .get(
            "/pushrules/{scope}/{kind}",
            pushrules::malformed,
            malformed(),
        )
        .put(
            "/pushrules/{scope}/{kind}",
            pushrules::malformed,
            malformed(),
        )
        .delete(
            "/pushrules/{scope}/{kind}",
            pushrules::malformed,
            malformed(),
        )
        .get(
            "/pushrules/{scope}/{kind}/",
            pushrules::get_pushrules_kind::<B>,
            malformed(),
        )
        .put(
            "/pushrules/{scope}/{kind}/",
            pushrules::malformed,
            malformed(),
        )
        .delete(
            "/pushrules/{scope}/{kind}/",
            pushrules::malformed,
            malformed(),
        )
        .get(
            "/pushrules/global/{kind}/{ruleId}",
            pushrules::global::get_pushrule::<B>,
            matrix_client("getPushRule"),
        )
        .put(
            "/pushrules/global/{kind}/{ruleId}",
            pushrules::global::put_pushrule::<B>,
            matrix_client("setPushRule"),
        )
        .delete(
            "/pushrules/global/{kind}/{ruleId}",
            pushrules::global::delete_pushrule::<B>,
            matrix_client("deletePushRule"),
        )
        .get(
            "/pushrules/{scope}/{kind}/{ruleId}",
            pushrules::get_pushrule::<B>,
            malformed(),
        )
        .put(
            "/pushrules/{scope}/{kind}/{ruleId}",
            pushrules::put_pushrule::<B>,
            malformed(),
        )
        .delete(
            "/pushrules/{scope}/{kind}/{ruleId}",
            pushrules::delete_pushrule::<B>,
            malformed(),
        )
        .get(
            "/pushrules/global/{kind}/{ruleId}/actions",
            pushrules::global::get_actions::<B>,
            matrix_client("getPushRuleActions"),
        )
        .put(
            "/pushrules/global/{kind}/{ruleId}/actions",
            pushrules::global::put_actions::<B>,
            matrix_client("setPushRuleActions"),
        )
        .get(
            "/pushrules/global/{kind}/{ruleId}/enabled",
            pushrules::global::get_enabled::<B>,
            matrix_client("isPushRuleEnabled"),
        )
        .put(
            "/pushrules/global/{kind}/{ruleId}/enabled",
            pushrules::global::put_enabled::<B>,
            matrix_client("setPushRuleEnabled"),
        )
        .get(
            "/pushrules/{scope}/{kind}/{ruleId}/{attr}",
            pushrules::get_pushrule_attr::<B>,
            malformed(),
        )
        .put(
            "/pushrules/{scope}/{kind}/{ruleId}/{attr}",
            pushrules::put_pushrule_attr::<B>,
            malformed(),
        )
        .delete(
            "/pushrules/{scope}/{kind}/{ruleId}/{attr}",
            pushrules::malformed,
            malformed(),
        )
        .get(
            "/pushers",
            pushers::get_pushers::<B>,
            matrix_client("getPushers"),
        )
        .post(
            "/pushers/set",
            pushers::post_pushers_set::<B>,
            matrix_client("postPusher"),
        )
        .get(
            "/notifications",
            notifications::get_notifications::<B>,
            matrix_client("getNotifications"),
        )
        .build()
}
