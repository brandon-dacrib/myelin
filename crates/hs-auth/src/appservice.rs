//! Application service token lookup: a stub registry against which appservice requests are
//! authenticated until track 11 (`hs-appservice`) provides the real one.
//!
//! `docs/workstreams/07-auth-and-identity.md`'s Phase 0 deliverable is "appservice tokens and
//! identity assertion against 11's registry stub" — this module is that stub. It is intentionally
//! the smallest thing that lets [`crate::middleware`] exercise the full appservice authentication
//! path (token lookup, `user_id` masquerade namespace check, `device_id` masquerade device
//! existence check) end to end in tests. When track 11 lands its own registry, it either
//! implements [`AppserviceRegistry`] directly or this trait moves to `hs-appservice` behind an
//! RFC; either way [`crate::middleware`] does not change.

use async_trait::async_trait;
use regex::Regex;
use ruma::{OwnedUserId, UserId};

/// One namespace rule from an appservice's registration (`namespaces.users` in the registration
/// YAML): a regular expression over full user IDs, and whether it is exclusive (no other
/// appservice or human may claim a matching ID).
#[derive(Debug, Clone)]
pub struct NamespaceRule {
    /// The compiled regular expression matched against a full user ID.
    pub regex: Regex,
    /// Exclusive namespaces are reserved to this appservice; non-exclusive ones just grant
    /// masquerading rights without reserving registration.
    pub exclusive: bool,
}

/// One application service's identity, as far as this crate's authentication path needs to know.
/// A real registry (track 11) carries much more (transaction URLs, protocols); this is the
/// projection [`crate::middleware`] actually consumes.
///
/// `rate_limited` and `msc4190_enabled` were added per
/// `docs/rfcs/0009-appservice-identity-capability-flags.md` (authored by track 11, applied by
/// track 07): they let a real registry's per-registration capability flags reach
/// [`crate::requester::AppserviceIdentity`] and, from there, rate-limiting code and
/// `crate::routes::devices`'s MSC4190 branch. Both default to the RFC's "assume nothing extra is
/// granted" values (`rate_limited: true` meaning *not* exempt, `msc4190_enabled: false`) via
/// [`AppserviceRecord::new`], so existing callers that only set `appservice_id`/`sender`/
/// `user_namespaces` and don't know about these fields keep their prior behavior unchanged.
#[derive(Debug, Clone)]
pub struct AppserviceRecord {
    /// The registration's `id` field.
    pub appservice_id: String,
    /// The registration's `sender_localpart`, resolved to a full user ID on this homeserver.
    pub sender: OwnedUserId,
    /// The `namespaces.users` rules, used to validate a `user_id` masquerade parameter.
    pub user_namespaces: Vec<NamespaceRule>,
    /// `false` if this registration's `rate_limited: false` was set, meaning every request from
    /// this appservice (its own sender and every masqueraded user) is exempt from rate limiting.
    /// `true` (the default) means ordinary rate limiting still applies.
    /// `docs/rfcs/0009-appservice-identity-capability-flags.md` point 1.
    pub rate_limited: bool,
    /// `true` if this registration set `io.element.msc4190: true`: `PUT /devices/{deviceId}`
    /// creates the device instead of 404ing, and `DELETE /devices/{deviceId}` skips UIA
    /// re-authentication, for this appservice's requests.
    /// `docs/rfcs/0009-appservice-identity-capability-flags.md` point 2.
    pub msc4190_enabled: bool,
}

impl AppserviceRecord {
    /// Convenience constructor for the common case (no extra capability flags), matching the
    /// three-field shape every fixture in this workspace used before RFC 0009. Prefer this over a
    /// bare struct literal so a future capability flag addition doesn't require touching every
    /// call site again.
    #[must_use]
    pub fn new(
        appservice_id: impl Into<String>,
        sender: OwnedUserId,
        user_namespaces: Vec<NamespaceRule>,
    ) -> Self {
        Self {
            appservice_id: appservice_id.into(),
            sender,
            user_namespaces,
            rate_limited: true,
            msc4190_enabled: false,
        }
    }

    /// True if this appservice is allowed to masquerade as `user_id` — either it *is* the sender,
    /// or `user_id` matches one of its registered user namespaces. Matches Synapse's
    /// `ApplicationService.is_interested_in_user`/`is_exclusive_user` used from
    /// `validate_appservice_can_control_user_id` (behavioral reference only).
    #[must_use]
    pub fn can_control(&self, user_id: &UserId) -> bool {
        if user_id == self.sender {
            return true;
        }
        self.user_namespaces
            .iter()
            .any(|ns| ns.regex.is_match(user_id.as_str()))
    }
}

/// Looks up an application service by the bearer token it presented.
#[async_trait]
pub trait AppserviceRegistry: Send + Sync {
    /// Returns the appservice this token belongs to, or `None` if it is not an appservice token
    /// at all (the caller then falls through to ordinary user-token authentication).
    async fn lookup_by_token(&self, token: &str) -> Option<AppserviceRecord>;
}

/// An in-memory registry for tests and for running this crate standalone before track 11's real
/// registry exists.
#[derive(Default)]
pub struct InMemoryAppserviceRegistry {
    by_token: std::sync::Mutex<std::collections::HashMap<String, AppserviceRecord>>,
}

impl InMemoryAppserviceRegistry {
    /// An empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers an appservice under a token, for tests to set up fixtures with.
    pub fn insert(&self, token: impl Into<String>, record: AppserviceRecord) {
        self.by_token
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(token.into(), record);
    }
}

#[async_trait]
impl AppserviceRegistry for InMemoryAppserviceRegistry {
    async fn lookup_by_token(&self, token: &str) -> Option<AppserviceRecord> {
        self.by_token
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(token)
            .cloned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ruma::user_id;

    fn bridge() -> AppserviceRecord {
        AppserviceRecord::new(
            "irc-bridge",
            user_id!("@irc-bridge:example.org").to_owned(),
            vec![NamespaceRule {
                regex: Regex::new(r"^@irc_.*:example\.org$").unwrap(),
                exclusive: true,
            }],
        )
    }

    #[test]
    fn sender_can_always_control_itself() {
        let record = bridge();
        assert!(record.can_control(user_id!("@irc-bridge:example.org")));
    }

    #[test]
    fn namespace_match_can_control() {
        let record = bridge();
        assert!(record.can_control(user_id!("@irc_alice:example.org")));
    }

    #[test]
    fn outside_namespace_cannot_control() {
        let record = bridge();
        assert!(!record.can_control(user_id!("@alice:example.org")));
    }

    #[tokio::test]
    async fn registry_lookup_round_trips() {
        let registry = InMemoryAppserviceRegistry::new();
        registry.insert("as_token_123", bridge());
        let found = registry.lookup_by_token("as_token_123").await;
        assert_eq!(found.unwrap().appservice_id, "irc-bridge");
        assert!(registry.lookup_by_token("nope").await.is_none());
    }
}
