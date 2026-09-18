//! The client-server HTTP endpoints this crate owns, as a router fragment. Mount [`router`] under
//! `/_matrix/client/v3` (and the historical `r0` alias), the way `hs-auth`, `hs-room` and `hs-e2e`
//! do. See `docs/status/10-push.md`'s "Interfaces provided" for the exact
//! `crates/hs-cli/src/serve.rs` wiring this expects.
//!
//! The spec spells the whole-ruleset path with a trailing slash (`GET /pushrules/`) and every
//! other `/pushrules` path without one, which is not a typo on our side: clients send both, so
//! both are registered against the same handler.

pub mod pushers;
pub mod pushrules;

use hs_http::router::{AuthKind, Builder, RouteManifest, RouteMeta, Surface};
use hs_kv::KvBackend;

use crate::state::PushState;

fn matrix_client(operation_id: &str) -> RouteMeta {
    RouteMeta::new(Surface::MatrixClient, AuthKind::Matrix).with_operation_id(operation_id)
}

/// The spec-relative (no version prefix) client-server router fragment: push rules in every form,
/// and pusher registration.
pub fn router<B: KvBackend + 'static>() -> (axum::Router<PushState<B>>, RouteManifest) {
    Builder::new()
        .get(
            "/pushrules/",
            pushrules::get_pushrules_all::<B>,
            matrix_client("getPushRules"),
        )
        .get(
            "/pushrules/global/{kind}/{ruleId}",
            pushrules::get_pushrule::<B>,
            matrix_client("getPushRule"),
        )
        .put(
            "/pushrules/global/{kind}/{ruleId}",
            pushrules::put_pushrule::<B>,
            matrix_client("setPushRule"),
        )
        .delete(
            "/pushrules/global/{kind}/{ruleId}",
            pushrules::delete_pushrule::<B>,
            matrix_client("deletePushRule"),
        )
        .get(
            "/pushrules/global/{kind}/{ruleId}/actions",
            pushrules::get_pushrule_actions::<B>,
            matrix_client("getPushRuleActions"),
        )
        .put(
            "/pushrules/global/{kind}/{ruleId}/actions",
            pushrules::put_pushrule_actions::<B>,
            matrix_client("setPushRuleActions"),
        )
        .get(
            "/pushrules/global/{kind}/{ruleId}/enabled",
            pushrules::get_pushrule_enabled::<B>,
            matrix_client("isPushRuleEnabled"),
        )
        .put(
            "/pushrules/global/{kind}/{ruleId}/enabled",
            pushrules::put_pushrule_enabled::<B>,
            matrix_client("setPushRuleEnabled"),
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
        .build()
}
