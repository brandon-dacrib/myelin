//! The push rules engine: evaluates a `ruma::push::Ruleset` against one event for one recipient.
//!
//! Per `docs/decisions/0007-build-less-reuse-more.md`, this is built on Ruma's push types rather
//! than reimplementing them: `ruma::push::Ruleset::iter()` already yields rules in exactly the
//! spec's priority order (override, content, room, sender, underride —
//! `refs/matrix-spec/content/client-server-api/modules/push.md` "Push Rules"), and
//! `ruma::push::AnyPushRuleRef::applies` already implements every condition kind (`event_match`,
//! `contains_display_name`, `room_member_count`, `sender_notification_permission`,
//! `event_property_is`, `event_property_contains`), the self-sender exclusion, the `enabled`
//! check, and the "old mention rules disabled when `m.mentions` is present" MSC3952 carve-out.
//! What is genuinely ours to build is: picking the first applicable rule (the spec's tie-break),
//! and turning it into the shape the rest of this crate and `crate::pushers` need.
//!
//! # Test provenance
//!
//! Test names and fixtures beginning `spec_` are transcribed from the specification's own push
//! rules examples and the predefined rule table
//! (`refs/matrix-spec/content/client-server-api/modules/push.md`, Apache-2.0). Test names
//! beginning `synapse_observed_` describe behavior read from Synapse's push evaluator
//! (`refs/synapse/rust/src/push/mod.rs`, AGPL-3.0) and re-expressed here as fresh assertions
//! against this crate's own API — no Synapse source is copied, per
//! `docs/decisions/0007-build-less-reuse-more.md` and this track's brief.

use ruma::push::{Action, FlattenedJson, PushConditionRoomCtx, Ruleset};
use ruma::serde::Raw;

/// The result of evaluating a ruleset against one event for one recipient: which rule matched
/// (if any) and what it says to do.
///
/// Not `PartialEq`/`Eq`: `ruma::push::Action` implements neither (its `Tweak` variant carries a
/// `serde_json::Value`), so tests assert on the individual fields — `rule_id`, `notify`,
/// `highlight`, `sound` — rather than on a whole outcome at once.
#[derive(Debug, Clone)]
pub struct EvaluationOutcome {
    /// The `rule_id` of the first rule (in spec priority order) whose conditions all held.
    pub rule_id: String,
    /// That rule's actions, verbatim.
    pub actions: Vec<Action>,
    /// `true` if any action is `notify` (or, with MSC3768, `notify_in_app`).
    pub notify: bool,
    /// `true` if any action is `set_tweak: highlight` with a truthy value.
    pub highlight: bool,
    /// The `sound` tweak's value, if any action sets one.
    pub sound: Option<String>,
}

/// Parses an event's canonical JSON into the flattened form push conditions match against.
///
/// # Errors
/// Returns the `serde_json` error if `event_json` is not valid JSON (should not happen for an
/// event that has already passed `hs-model`'s own parsing).
pub fn flatten_event(event_json: &str) -> serde_json::Result<FlattenedJson> {
    let raw = Raw::<serde_json::Value>::from_json_string(event_json.to_owned())?;
    Ok(FlattenedJson::from_raw(&raw))
}

/// Evaluates `ruleset` against `event` for the recipient described by `ctx`
/// (`ctx.user_id`/`ctx.user_display_name`), returning the first matching rule's outcome, or
/// `None` if no rule applies (per spec: the homeserver then must not notify the push gateway at
/// all, not even with an empty action list).
///
/// Matches [`ruma::push::AnyPushRuleRef::applies`]'s own behavior of never matching the event's
/// own sender (`ctx.user_id == event["sender"]`): callers do not need to filter the sender out of
/// their recipient list themselves, though `crate::compiled`'s hot path still skips the sender up
/// front to avoid the (cheap but pointless) per-rule check.
pub async fn evaluate(
    ruleset: &Ruleset,
    event: &FlattenedJson,
    ctx: &PushConditionRoomCtx,
) -> Option<EvaluationOutcome> {
    for rule in ruleset.iter() {
        if !rule.applies(event, ctx).await {
            continue;
        }
        let actions: Vec<Action> = rule.actions().to_vec();
        return Some(EvaluationOutcome {
            rule_id: rule.rule_id().to_owned(),
            notify: rule.triggers_notification(),
            highlight: rule.triggers_highlight(),
            sound: rule.triggers_sound().map(|s| s.as_ref().to_owned()),
            actions,
        });
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use ruma::UInt;
    use ruma::push::PushConditionRoomCtx;
    use ruma::{room_id, user_id};

    fn ctx_for(user: &ruma::UserId, member_count: u32, display_name: &str) -> PushConditionRoomCtx {
        PushConditionRoomCtx::new(
            room_id!("!spec:example.org").to_owned(),
            UInt::from(member_count),
            user.to_owned(),
            display_name.to_owned(),
        )
    }

    fn event(json: serde_json::Value) -> FlattenedJson {
        flatten_event(&json.to_string()).unwrap()
    }

    // --- spec_: the specification's own examples and predefined-rule table -----------------

    #[tokio::test]
    async fn spec_default_ruleset_notifies_on_a_plain_message() {
        let alice = user_id!("@alice:example.org");
        let ruleset = Ruleset::server_default(alice);
        let ctx = ctx_for(alice, 2, "Alice");
        let ev = event(serde_json::json!({
            "type": "m.room.message",
            "sender": "@bob:example.org",
            "room_id": "!spec:example.org",
            "content": {"msgtype": "m.text", "body": "hello"},
        }));
        let outcome = evaluate(&ruleset, &ev, &ctx)
            .await
            .expect("a default rule should match");
        // `.m.rule.message` (an underride rule) is the lowest-priority catch-all that matches an
        // ordinary text message with no other condition triggered; it notifies without a
        // highlight.
        assert!(outcome.notify);
        assert!(!outcome.highlight);
    }

    #[tokio::test]
    async fn spec_a_users_own_events_never_match_any_rule() {
        let alice = user_id!("@alice:example.org");
        let ruleset = Ruleset::server_default(alice);
        let ctx = ctx_for(alice, 2, "Alice");
        let ev = event(serde_json::json!({
            "type": "m.room.message",
            "sender": "@alice:example.org",
            "room_id": "!spec:example.org",
            "content": {"msgtype": "m.text", "body": "hello, this mentions Alice"},
        }));
        assert!(evaluate(&ruleset, &ev, &ctx).await.is_none());
    }

    #[tokio::test]
    async fn spec_disabled_rule_never_matches() {
        let alice = user_id!("@alice:example.org");
        let mut ruleset = Ruleset::server_default(alice);
        ruleset
            .set_enabled(ruma::push::RuleKind::Underride, ".m.rule.message", false)
            .unwrap();
        // Three members, not two: in a two-member room `.m.rule.room_one_to_one` matches ahead of
        // `.m.rule.message`, so disabling `.m.rule.message` alone would not make the ruleset
        // silent and the test would prove nothing about `enabled` being honored.
        let ctx = ctx_for(alice, 3, "Alice");
        let ev = event(serde_json::json!({
            "type": "m.room.message",
            "sender": "@bob:example.org",
            "room_id": "!spec:example.org",
            "content": {"msgtype": "m.text", "body": "hello"},
        }));
        // With the only rule that would otherwise match disabled, nothing applies.
        assert!(evaluate(&ruleset, &ev, &ctx).await.is_none());
    }

    #[tokio::test]
    async fn spec_is_user_mention_highlights_and_notifies() {
        // `.m.rule.is_user_mention` (override, spec v1.7,
        // `refs/matrix-spec/content/client-server-api/modules/push.md`): an
        // `event_property_contains` on `content.m\.mentions.user_ids`, with actions `notify` +
        // `sound: default` + `highlight`. This replaced the old body-scanning
        // `.m.rule.contains_user_name`, which the spec's predefined table no longer lists at all
        // (and which `Ruleset::server_default` correspondingly does not create -- its content
        // ruleset is empty). Mentions are intentional now: naming someone in the body does not
        // notify them, listing them in `m.mentions` does.
        let alice = user_id!("@alice:example.org");
        let ruleset = Ruleset::server_default(alice);
        let ctx = ctx_for(alice, 3, "Alice");

        let mentioned = event(serde_json::json!({
            "type": "m.room.message",
            "sender": "@bob:example.org",
            "room_id": "!spec:example.org",
            "content": {
                "msgtype": "m.text",
                "body": "look at this",
                "m.mentions": {"user_ids": ["@alice:example.org"]},
            },
        }));
        let outcome = evaluate(&ruleset, &mentioned, &ctx)
            .await
            .expect("an intentional mention should trigger");
        assert_eq!(outcome.rule_id, ".m.rule.is_user_mention");
        assert!(outcome.highlight);
        assert!(outcome.notify);
        assert_eq!(outcome.sound.as_deref(), Some("default"));

        // The counterpart the same MSC created: the localpart in the body, with no `m.mentions`,
        // falls through to the plain-message rule and does not highlight.
        let named_in_body = event(serde_json::json!({
            "type": "m.room.message",
            "sender": "@bob:example.org",
            "room_id": "!spec:example.org",
            "content": {"msgtype": "m.text", "body": "hey alice, look at this"},
        }));
        let outcome = evaluate(&ruleset, &named_in_body, &ctx)
            .await
            .expect("a plain message still notifies");
        assert_eq!(outcome.rule_id, ".m.rule.message");
        assert!(!outcome.highlight);
    }

    #[tokio::test]
    async fn spec_room_member_count_is_condition_one_to_one_room() {
        // `.m.rule.room_one_to_one` (underride) requires exactly 2 members and a non-encrypted
        // room; with 3 members it must not match, falling through to `.m.rule.message` instead
        // (still notifies, since both are underride "notify" rules, but proves the
        // `room_member_count` condition actually gates the more specific rule).
        let alice = user_id!("@alice:example.org");
        let ruleset = Ruleset::server_default(alice);
        let ev = event(serde_json::json!({
            "type": "m.room.message",
            "sender": "@bob:example.org",
            "room_id": "!spec:example.org",
            "content": {"msgtype": "m.text", "body": "hi"},
        }));

        let two_member_ctx = ctx_for(alice, 2, "Alice");
        let outcome_2 = evaluate(&ruleset, &ev, &two_member_ctx).await.unwrap();
        assert_eq!(outcome_2.rule_id, ".m.rule.room_one_to_one");

        let three_member_ctx = ctx_for(alice, 3, "Alice");
        let outcome_3 = evaluate(&ruleset, &ev, &three_member_ctx).await.unwrap();
        assert_eq!(outcome_3.rule_id, ".m.rule.message");
    }

    #[tokio::test]
    async fn spec_at_room_notification_requires_room_notification_permission() {
        // `.m.rule.roomnotif` (override) matches `content.body` containing `@room` but only
        // applies when the sender has the room's `notifications.room` power level. Below it:
        // no override matches, so evaluation falls through to whatever underride applies (still
        // "notify", but crucially *not* the override rule, and not highlighted).
        let alice = user_id!("@alice:example.org");
        let ruleset = Ruleset::server_default(alice);
        let ev = event(serde_json::json!({
            "type": "m.room.message",
            "sender": "@bob:example.org",
            "room_id": "!spec:example.org",
            "content": {"msgtype": "m.text", "body": "@room please read this"},
        }));

        // No power_levels context at all: `sender_notification_permission` cannot match
        // (ruma's own contract: "If this is missing, push rules that require this will never
        // match").
        let ctx = ctx_for(alice, 5, "Alice");
        let outcome = evaluate(&ruleset, &ev, &ctx).await.unwrap();
        assert_ne!(outcome.rule_id, ".m.rule.roomnotif");
    }

    // --- synapse_observed_: behavior read from Synapse's evaluator, re-expressed as fresh
    // assertions (no Synapse source copied; see this module's doc comment). ------------------

    #[tokio::test]
    async fn synapse_observed_tombstone_event_is_an_override_highlight() {
        // Synapse's default rule table treats `m.room.tombstone` as an override rule that
        // notifies (room upgrades are always surfaced, regardless of content rules below it).
        let alice = user_id!("@alice:example.org");
        let ruleset = Ruleset::server_default(alice);
        let ctx = ctx_for(alice, 4, "Alice");
        let ev = event(serde_json::json!({
            "type": "m.room.tombstone",
            "sender": "@bob:example.org",
            "room_id": "!spec:example.org",
            "state_key": "",
            "content": {"body": "This room has been replaced"},
        }));
        let outcome = evaluate(&ruleset, &ev, &ctx)
            .await
            .expect("tombstone should always notify");
        assert_eq!(outcome.rule_id, ".m.rule.tombstone");
        assert!(outcome.notify);
    }

    #[tokio::test]
    async fn synapse_observed_own_membership_change_notifies_as_invite_for_me() {
        // Synapse's `.m.rule.invite_for_me` (override) fires only when the state_key names the
        // recipient and membership is "invite" -- an unrelated member's join must not trigger it.
        let alice = user_id!("@alice:example.org");
        let ruleset = Ruleset::server_default(alice);
        let ctx = ctx_for(alice, 4, "Alice");

        let invite_for_alice = event(serde_json::json!({
            "type": "m.room.member",
            "sender": "@bob:example.org",
            "room_id": "!spec:example.org",
            "state_key": "@alice:example.org",
            "content": {"membership": "invite"},
        }));
        let outcome = evaluate(&ruleset, &invite_for_alice, &ctx)
            .await
            .expect("own invite should notify");
        assert_eq!(outcome.rule_id, ".m.rule.invite_for_me");

        let someone_elses_join = event(serde_json::json!({
            "type": "m.room.member",
            "sender": "@bob:example.org",
            "room_id": "!spec:example.org",
            "state_key": "@carol:example.org",
            "content": {"membership": "join"},
        }));
        let outcome2 = evaluate(&ruleset, &someone_elses_join, &ctx).await;
        assert_ne!(
            outcome2.map(|o| o.rule_id),
            Some(".m.rule.invite_for_me".to_owned())
        );
    }

    #[tokio::test]
    async fn synapse_observed_reactions_do_not_notify_by_default() {
        // Synapse's default `.m.rule.reaction` (override) matches `m.reaction` events with an
        // empty action list, so a reaction is a *matching, non-notifying* rule -- distinct from
        // "no rule matched at all". This is what stops reactions from ever pushing by default
        // while still letting a user override that with their own rule.
        let alice = user_id!("@alice:example.org");
        let ruleset = Ruleset::server_default(alice);
        let ctx = ctx_for(alice, 3, "Alice");
        let ev = event(serde_json::json!({
            "type": "m.reaction",
            "sender": "@bob:example.org",
            "room_id": "!spec:example.org",
            "content": {
                "m.relates_to": {"rel_type": "m.annotation", "event_id": "$1", "key": "👍"}
            },
        }));
        let outcome = evaluate(&ruleset, &ev, &ctx)
            .await
            .expect("reaction rule should match");
        assert_eq!(outcome.rule_id, ".m.rule.reaction");
        assert!(!outcome.notify);
    }

    #[tokio::test]
    async fn synapse_observed_room_rule_overrides_default_for_muted_room() {
        // A user-created room rule with an empty action list (Synapse's "mute this room") must
        // outrank the default underride `.m.rule.message` for every event in that room, since
        // room rules are checked before underride rules.
        let alice = user_id!("@alice:example.org");
        let mut ruleset = Ruleset::server_default(alice);
        let room = ruma::room_id!("!spec:example.org");
        ruleset
            .insert(
                ruma::push::NewPushRule::Room(ruma::push::NewSimplePushRule::new(
                    room.to_owned(),
                    vec![],
                )),
                None,
                None,
            )
            .unwrap();
        let ctx = ctx_for(alice, 3, "Alice");
        let ev = event(serde_json::json!({
            "type": "m.room.message",
            "sender": "@bob:example.org",
            "room_id": "!spec:example.org",
            "content": {"msgtype": "m.text", "body": "hi"},
        }));
        let outcome = evaluate(&ruleset, &ev, &ctx)
            .await
            .expect("the room rule itself matches");
        assert_eq!(outcome.rule_id, room.as_str());
        assert!(!outcome.notify);
    }
}
