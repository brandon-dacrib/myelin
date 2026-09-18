//! A no-op [`ModuleHooks`] implementation: every check allows, every notification is ignored,
//! every override defers to the caller's default. This is what a server with no modules
//! configured runs against, and it is what `hs-testkit` (or any track's own tests) can build on
//! for a "modules exist but do nothing" baseline.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;

use crate::hooks::{
    AuthResult, BackgroundUpdateGuidance, CheckResult, EventForCheck, MediaDescriptor, ModuleHooks,
    PresenceInterest, RatelimitOverride, RuleResult, UserProfile,
};

/// See the module doc comment.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoopHooks;

#[async_trait]
impl ModuleHooks for NoopHooks {
    async fn check_event_for_spam(&self, _event: &EventForCheck) -> CheckResult {
        CheckResult::Allow
    }

    async fn user_may_invite(&self, _inviter: &str, _invitee: &str, _room_id: &str) -> CheckResult {
        CheckResult::Allow
    }

    async fn check_username_for_spam(&self, _profile: &UserProfile) -> CheckResult {
        CheckResult::Allow
    }

    async fn check_event_allowed(&self, _event: &EventForCheck) -> RuleResult {
        RuleResult::Allow
    }

    async fn get_interested_users(&self, _presence_user_id: &str) -> PresenceInterest {
        PresenceInterest::Users(Vec::new())
    }

    async fn is_user_expired(&self, _user_id: &str) -> Option<i64> {
        None
    }

    async fn check_password(&self, _user_id: &str, _password: &str) -> Option<AuthResult> {
        None
    }

    async fn background_update_guidance(&self, _update_name: &str) -> BackgroundUpdateGuidance {
        BackgroundUpdateGuidance::default()
    }

    async fn on_account_data_updated(
        &self,
        _user_id: &str,
        _room_id: Option<&str>,
        _account_data_type: &str,
        _content: &serde_json::Value,
    ) {
    }

    async fn check_media_for_spam(&self, _media: &MediaDescriptor) -> CheckResult {
        CheckResult::Allow
    }

    async fn ratelimit_override(
        &self,
        _user_id: &str,
        _limiter: &str,
    ) -> Option<RatelimitOverride> {
        None
    }

    async fn should_federate_room(&self, _room_id: &str) -> bool {
        true
    }

    async fn extra_unsigned_fields(
        &self,
        _event: &EventForCheck,
    ) -> HashMap<String, serde_json::Value> {
        HashMap::new()
    }
}

/// Runs several modules in a fixed order, short-circuiting the checking hooks on the first
/// `Deny`/non-`Allow` result (matching Synapse's own module chain semantics: any module may
/// veto). Notification hooks (`on_account_data_updated`) run on every module regardless of what
/// earlier ones returned, since there is nothing to veto.
pub struct ModuleChain {
    modules: Vec<Arc<dyn ModuleHooks>>,
}

impl ModuleChain {
    pub fn new(modules: Vec<Arc<dyn ModuleHooks>>) -> Self {
        Self { modules }
    }
}

#[async_trait]
impl ModuleHooks for ModuleChain {
    async fn check_event_for_spam(&self, event: &EventForCheck) -> CheckResult {
        for m in &self.modules {
            let result = m.check_event_for_spam(event).await;
            if !result.is_allowed() {
                return result;
            }
        }
        CheckResult::Allow
    }

    async fn user_may_invite(&self, inviter: &str, invitee: &str, room_id: &str) -> CheckResult {
        for m in &self.modules {
            let result = m.user_may_invite(inviter, invitee, room_id).await;
            if !result.is_allowed() {
                return result;
            }
        }
        CheckResult::Allow
    }

    async fn check_username_for_spam(&self, profile: &UserProfile) -> CheckResult {
        for m in &self.modules {
            let result = m.check_username_for_spam(profile).await;
            if !result.is_allowed() {
                return result;
            }
        }
        CheckResult::Allow
    }

    async fn check_event_allowed(&self, event: &EventForCheck) -> RuleResult {
        let mut current = event.clone();
        for m in &self.modules {
            match m.check_event_allowed(&current).await {
                RuleResult::Allow => continue,
                RuleResult::Replace { content } => current.content = content,
                deny @ RuleResult::Deny { .. } => return deny,
            }
        }
        if current.content == event.content {
            RuleResult::Allow
        } else {
            RuleResult::Replace {
                content: current.content,
            }
        }
    }

    async fn get_interested_users(&self, presence_user_id: &str) -> PresenceInterest {
        let mut union = std::collections::HashSet::new();
        for m in &self.modules {
            match m.get_interested_users(presence_user_id).await {
                PresenceInterest::AllUsers => return PresenceInterest::AllUsers,
                PresenceInterest::Users(users) => union.extend(users),
            }
        }
        PresenceInterest::Users(union.into_iter().collect())
    }

    async fn is_user_expired(&self, user_id: &str) -> Option<i64> {
        for m in &self.modules {
            if let Some(expiry) = m.is_user_expired(user_id).await {
                return Some(expiry);
            }
        }
        None
    }

    async fn check_password(&self, user_id: &str, password: &str) -> Option<AuthResult> {
        for m in &self.modules {
            if let Some(result) = m.check_password(user_id, password).await {
                return Some(result);
            }
        }
        None
    }

    async fn background_update_guidance(&self, update_name: &str) -> BackgroundUpdateGuidance {
        // The most conservative (smallest batch) guidance any module offers wins.
        let mut guidance = BackgroundUpdateGuidance::default();
        for m in &self.modules {
            let g = m.background_update_guidance(update_name).await;
            if g.batch_size < guidance.batch_size {
                guidance = g;
            }
        }
        guidance
    }

    async fn on_account_data_updated(
        &self,
        user_id: &str,
        room_id: Option<&str>,
        account_data_type: &str,
        content: &serde_json::Value,
    ) {
        for m in &self.modules {
            m.on_account_data_updated(user_id, room_id, account_data_type, content)
                .await;
        }
    }

    async fn check_media_for_spam(&self, media: &MediaDescriptor) -> CheckResult {
        for m in &self.modules {
            let result = m.check_media_for_spam(media).await;
            if !result.is_allowed() {
                return result;
            }
        }
        CheckResult::Allow
    }

    async fn ratelimit_override(&self, user_id: &str, limiter: &str) -> Option<RatelimitOverride> {
        for m in &self.modules {
            if let Some(o) = m.ratelimit_override(user_id, limiter).await {
                return Some(o);
            }
        }
        None
    }

    async fn should_federate_room(&self, room_id: &str) -> bool {
        for m in &self.modules {
            if !m.should_federate_room(room_id).await {
                return false;
            }
        }
        true
    }

    async fn extra_unsigned_fields(
        &self,
        event: &EventForCheck,
    ) -> HashMap<String, serde_json::Value> {
        let mut merged = HashMap::new();
        for m in &self.modules {
            merged.extend(m.extra_unsigned_fields(event).await);
        }
        merged
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_event() -> EventForCheck {
        EventForCheck {
            event_id: "$1".into(),
            room_id: "!r:example.org".into(),
            sender: "@a:example.org".into(),
            event_type: "m.room.message".into(),
            state_key: None,
            content: serde_json::json!({"body": "hi"}),
        }
    }

    #[tokio::test]
    async fn noop_allows_everything() {
        let hooks = NoopHooks;
        assert!(
            hooks
                .check_event_for_spam(&sample_event())
                .await
                .is_allowed()
        );
        assert!(
            hooks
                .user_may_invite("@a:x", "@b:x", "!r:x")
                .await
                .is_allowed()
        );
        assert_eq!(
            hooks.check_event_allowed(&sample_event()).await,
            RuleResult::Allow
        );
        assert_eq!(hooks.is_user_expired("@a:x").await, None);
        assert_eq!(hooks.check_password("@a:x", "hunter2").await, None);
        assert!(hooks.should_federate_room("!r:x").await);
        assert!(
            hooks
                .extra_unsigned_fields(&sample_event())
                .await
                .is_empty()
        );
    }

    struct DenyAll;
    #[async_trait]
    impl ModuleHooks for DenyAll {
        async fn check_event_for_spam(&self, _event: &EventForCheck) -> CheckResult {
            CheckResult::deny("no")
        }
        async fn user_may_invite(&self, _i: &str, _v: &str, _r: &str) -> CheckResult {
            CheckResult::Allow
        }
        async fn check_username_for_spam(&self, _p: &UserProfile) -> CheckResult {
            CheckResult::Allow
        }
        async fn check_event_allowed(&self, _event: &EventForCheck) -> RuleResult {
            RuleResult::Allow
        }
        async fn get_interested_users(&self, _p: &str) -> PresenceInterest {
            PresenceInterest::Users(vec![])
        }
        async fn is_user_expired(&self, _u: &str) -> Option<i64> {
            None
        }
        async fn check_password(&self, _u: &str, _p: &str) -> Option<AuthResult> {
            None
        }
        async fn background_update_guidance(&self, _u: &str) -> BackgroundUpdateGuidance {
            BackgroundUpdateGuidance::default()
        }
        async fn on_account_data_updated(
            &self,
            _u: &str,
            _r: Option<&str>,
            _t: &str,
            _c: &serde_json::Value,
        ) {
        }
        async fn check_media_for_spam(&self, _m: &MediaDescriptor) -> CheckResult {
            CheckResult::Allow
        }
        async fn ratelimit_override(&self, _u: &str, _l: &str) -> Option<RatelimitOverride> {
            None
        }
        async fn should_federate_room(&self, _r: &str) -> bool {
            true
        }
        async fn extra_unsigned_fields(
            &self,
            _event: &EventForCheck,
        ) -> HashMap<String, serde_json::Value> {
            HashMap::new()
        }
    }

    #[tokio::test]
    async fn chain_short_circuits_on_first_deny() {
        let chain = ModuleChain::new(vec![Arc::new(DenyAll), Arc::new(NoopHooks)]);
        let result = chain.check_event_for_spam(&sample_event()).await;
        assert!(!result.is_allowed());
    }

    #[tokio::test]
    async fn chain_allows_when_every_module_allows() {
        let chain = ModuleChain::new(vec![Arc::new(NoopHooks), Arc::new(NoopHooks)]);
        assert!(
            chain
                .check_event_for_spam(&sample_event())
                .await
                .is_allowed()
        );
    }
}
