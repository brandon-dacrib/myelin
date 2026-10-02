//! The admin API's own metrics, registered by whoever serves it (`hs serve` calls
//! [`register_metrics`] on its registry). Per-route request and status counts are `hs-http`'s
//! `hs_http_requests_total{route,status}`; what is here is what a route counter cannot say.

use std::sync::LazyLock;

use prometheus_client::metrics::counter::Counter;
use prometheus_client::metrics::family::Family;

/// The labels of `hs_admin_scope_refusals_total`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, prometheus_client::encoding::EncodeLabelSet)]
pub struct ScopeRefusalLabels {
    /// The scope the operation required and the token lacked.
    pub required_scope: &'static str,
}

/// Process-wide: a counter is an atomic, and scope checks run far from any registry.
static SCOPE_REFUSALS: LazyLock<Family<ScopeRefusalLabels, Counter>> =
    LazyLock::new(Family::default);

/// Counts one `403 insufficient-scope` for `required_scope`.
pub fn count_scope_refusal(required_scope: &'static str) {
    SCOPE_REFUSALS
        .get_or_create(&ScopeRefusalLabels { required_scope })
        .inc();
}

/// How many refusals have been counted for `required_scope` so far in this process.
#[must_use]
pub fn scope_refusals(required_scope: &'static str) -> u64 {
    SCOPE_REFUSALS
        .get_or_create(&ScopeRefusalLabels { required_scope })
        .get()
}

/// Registers `hs_admin_scope_refusals_total{required_scope}` -- admin API requests refused
/// with `403 insufficient-scope`, by the scope they lacked -- into `registry`. A token that
/// keeps being refused is either minted too narrow or being used for something it was not
/// meant for; either way an operator wants to see it climb.
pub fn register_metrics(registry: &mut prometheus_client::registry::Registry) {
    // Registered without `_total`: the text encoder appends it.
    registry.register(
        "hs_admin_scope_refusals",
        "Admin API requests refused with 403 insufficient-scope, by the scope the token lacked",
        SCOPE_REFUSALS.clone(),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refusals_are_counted_by_required_scope() {
        let before = scope_refusals("bridges:write");
        count_scope_refusal("bridges:write");
        count_scope_refusal("bridges:write");
        assert_eq!(scope_refusals("bridges:write"), before + 2);

        let mut registry = prometheus_client::registry::Registry::default();
        register_metrics(&mut registry);
        let mut out = String::new();
        prometheus_client::encoding::text::encode(&mut out, &registry).expect("encodes");
        assert!(
            out.contains("hs_admin_scope_refusals_total{required_scope=\"bridges:write\"}"),
            "{out}"
        );
    }
}
