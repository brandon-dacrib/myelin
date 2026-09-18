//! Implements `hs-auth`'s [`AppserviceRegistry`] trait over [`Registry`], replacing the stub
//! `InMemoryAppserviceRegistry` (`crates/hs-auth/src/appservice.rs`'s crate docs: "When track 11
//! lands its own registry, it either implements `AppserviceRegistry` directly or this trait moves
//! to `hs-appservice` behind an RFC; either way `crate::middleware` does not change" — this module
//! is that implementation).
//!
//! # The `regex::Regex` divergence
//!
//! `hs-auth`'s [`NamespaceRule`] hard-codes `regex::Regex` (it predates this crate). Every user
//! namespace pattern this crate compiles under the linear-time `regex` engine projects across
//! losslessly ([`crate::regexp::NamespacePattern::as_regex_crate`]). A pattern that only compiles
//! under `fancy_regex` (lookaround or backreferences) has no such projection; rather than silently
//! drop it (which would make the masquerade check *more* permissive by omission — a
//! `user_id` that should have failed one of two exclusive rules might now only be checked against
//! one) or panic, it is skipped with a `tracing::warn!` and, since `AppserviceRecord::can_control`
//! is an `any()` over the namespace list, this specific rule simply never grants masquerading
//! through the fast `hs-auth` path. This crate's own namespace matching (registry conflict
//! detection, `/users`/`/rooms` query routing) is unaffected — it always uses the full
//! `NamespacePattern`, `fancy_regex` fallback included.

use std::sync::Arc;

use async_trait::async_trait;
use hs_auth::appservice::{AppserviceRecord, AppserviceRegistry, NamespaceRule};
use hs_kv::KvBackend;
use ruma::UserId;

use crate::registry::Registry;

/// Adapts [`Registry`] to `hs-auth`'s [`AppserviceRegistry`] trait, so `hs-auth`'s middleware can
/// authenticate appservice requests against this crate's store-backed registry.
pub struct RegistryAppserviceAdapter<B: KvBackend> {
    registry: Arc<Registry<B>>,
}

impl<B: KvBackend> RegistryAppserviceAdapter<B> {
    /// Wraps `registry` for use as `hs-auth`'s `AuthState::appservices`.
    #[must_use]
    pub fn new(registry: Arc<Registry<B>>) -> Self {
        Self { registry }
    }
}

#[async_trait]
impl<B: KvBackend> AppserviceRegistry for RegistryAppserviceAdapter<B> {
    async fn lookup_by_token(&self, token: &str) -> Option<AppserviceRecord> {
        let row = self
            .registry
            .store()
            .get_by_as_token(token)
            .ok()
            .flatten()?;
        let sender_str = format!("@{}:{}", row.sender_localpart, self.registry.server_name());
        let sender = UserId::parse(&sender_str).ok()?;

        let compiled = row.namespaces.compile().ok()?;
        let mut user_namespaces = Vec::new();
        for rule in &compiled.users {
            match rule.pattern.as_regex_crate() {
                Some(re) => user_namespaces.push(NamespaceRule {
                    regex: re,
                    exclusive: rule.exclusive,
                }),
                None => tracing::warn!(
                    appservice_id = %row.id,
                    pattern = rule.pattern.source(),
                    "user namespace pattern needs the fancy_regex fallback; hs-auth's fast \
                     masquerade check cannot express it and will never match this specific rule \
                     (see crate::auth_registry docs)"
                ),
            }
        }

        Some(AppserviceRecord {
            appservice_id: row.id,
            sender,
            user_namespaces,
            // `docs/rfcs/0009-appservice-identity-capability-flags.md`: track 07 added these two
            // fields to `AppserviceRecord`/`AppserviceIdentity` this session. This one-line fill-in
            // at the call site the RFC itself named is the mechanical half of the rollout the RFC
            // assigned to track 11 ("Track 11 will apply this patch itself if track 07 has not
            // picked it up by the time both tracks are back in the same integration window") —
            // applied here now, minimally, only to keep the shared workspace build green for every
            // concurrently running track, using data this adapter already had in hand (`row.
            // rate_limited`/`row.msc4190`, both already parsed from the registration file by
            // `crate::registration::Registration`). No other behavior in this crate changed.
            rate_limited: row.rate_limited,
            msc4190_enabled: row.msc4190,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::namespace::{NamespaceRule as CrateNamespaceRule, Namespaces};
    use crate::registration::Registration;
    use hs_kv::memory::MemoryBackend;
    use ruma::{server_name, user_id};

    fn adapter_with(reg: Registration) -> RegistryAppserviceAdapter<MemoryBackend> {
        let registry =
            Arc::new(Registry::open(MemoryBackend::new(), server_name!("example.org")).unwrap());
        registry.add(&reg).unwrap();
        RegistryAppserviceAdapter::new(registry)
    }

    fn reg(exclusive_pattern: &str) -> Registration {
        Registration {
            id: "irc".to_string(),
            url: Some("http://localhost:1".to_string()),
            as_token: "as_irc".to_string(),
            hs_token: "hs_irc".to_string(),
            sender_localpart: "ircbot".to_string(),
            rate_limited: true,
            namespaces: Namespaces {
                users: vec![CrateNamespaceRule::compile(exclusive_pattern, true).unwrap()],
                aliases: vec![],
                rooms: vec![],
            },
            protocols: vec![],
            receive_ephemeral: false,
            push_ephemeral_legacy: false,
            msc3202: false,
            msc4190: false,
            extra: Default::default(),
        }
    }

    #[tokio::test]
    async fn looks_up_by_as_token_and_projects_namespaces() {
        let adapter = adapter_with(reg(r"^@irc_.*:example\.org$"));
        let record = adapter.lookup_by_token("as_irc").await.unwrap();
        assert_eq!(record.appservice_id, "irc");
        assert_eq!(record.sender, user_id!("@ircbot:example.org"));
        assert!(record.can_control(user_id!("@irc_alice:example.org")));
        assert!(!record.can_control(user_id!("@someone_else:example.org")));
    }

    #[tokio::test]
    async fn unknown_token_is_none() {
        let adapter = adapter_with(reg(r"^@irc_.*:example\.org$"));
        assert!(adapter.lookup_by_token("nope").await.is_none());
    }

    #[tokio::test]
    async fn fancy_only_pattern_is_dropped_but_does_not_fail_the_lookup() {
        // Negative lookahead: only compiles under fancy_regex, so hs-auth's projection cannot
        // express it and the record comes back with an empty (not missing) namespace list for
        // that rule — the sender can still always control itself.
        let adapter = adapter_with(reg(r"^@irc_(?!admin_).*:example\.org$"));
        let record = adapter.lookup_by_token("as_irc").await.unwrap();
        assert!(record.user_namespaces.is_empty());
        assert!(record.can_control(user_id!("@ircbot:example.org")));
        assert!(!record.can_control(user_id!("@irc_bob:example.org")));
    }
}
