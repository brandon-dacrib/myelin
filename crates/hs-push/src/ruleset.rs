//! A user's push ruleset: the spec's five rule kinds and MSC4306's `postcontent`, in evaluation
//! order, with the CRUD the `/pushrules` surface needs.
//!
//! # `postcontent` (MSC4306)
//!
//! MSC4306 (thread subscriptions) adds a sixth kind, `postcontent`, evaluated after `content`
//! and before `room`, as Synapse does (`rust/src/push/mod.rs`'s `PushRules::iter`). Its only
//! rules are server defaults (`.io.element.msc4306.rule.subscribed_thread` and
//! `.unsubscribed_thread`) whose condition is the user's subscription to the event's thread;
//! clients may not add their own (`PUT` answers `400 M_INVALID_PARAM`, as Synapse's does). This
//! server does not implement thread subscriptions, so it ships no `postcontent` rules, exactly
//! what Synapse does with `msc4306_enabled` off (its default): the kind is accepted on every
//! `/pushrules` path, its list is empty, and asking for one of those rules is `404`. A
//! `postcontent` rule that reaches a ruleset some other way (an imported one) is evaluated in
//! its place like any other conditional rule.
//!
//! This is this crate's own container rather than `ruma::push::Ruleset`, for one reason: Ruma
//! types a room rule's `rule_id` as `OwnedRoomId` and a sender rule's as `OwnedUserId`, and
//! rejects anything else at parse time. The spec says those IDs *should* be a room or user ID,
//! but every client-facing server treats them as opaque strings (Synapse stores them verbatim,
//! Sytest's own `61push/02add_rules.pl` adds a room rule named `#spam:example.com`), and a
//! server that 400s what every other server accepts breaks the clients that relied on it. The
//! individual rules are still Ruma's (`ConditionalPushRule`, `PatternedPushRule`, and their
//! condition and action types), so condition evaluation stays Ruma's too (`crate::engine`).
//!
//! The JSON shape is the spec's `Ruleset`, field for field, which is also what
//! `ruma::push::Ruleset` serializes to: a ruleset stored by an older build of this crate reads
//! back unchanged.

use ruma::UserId;
use ruma::push::{
    Action, ConditionalPushRule, ConditionalPushRuleInit, PatternedPushRule, PatternedPushRuleInit,
    PushCondition,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The kinds of push rule, in evaluation order: the spec's five, with MSC4306's `postcontent`
/// between `content` and `room` (see the module docs).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RuleKind {
    /// Highest priority: user-set and server-default override rules.
    Override,
    /// Glob matches on `content.body`.
    Content,
    /// MSC4306's conditional rules after `content`: server defaults only.
    PostContent,
    /// One room, by its `rule_id`.
    Room,
    /// One sender, by its `rule_id`.
    Sender,
    /// Lowest priority: the catch-alls.
    Underride,
}

impl RuleKind {
    /// Every kind, in evaluation order.
    pub const ALL: [RuleKind; 6] = [
        RuleKind::Override,
        RuleKind::Content,
        RuleKind::PostContent,
        RuleKind::Room,
        RuleKind::Sender,
        RuleKind::Underride,
    ];

    /// The kind a `/pushrules/global/{kind}/...` path names, or `None` for an unknown one.
    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        Some(match raw {
            "override" => Self::Override,
            "content" => Self::Content,
            "postcontent" => Self::PostContent,
            "room" => Self::Room,
            "sender" => Self::Sender,
            "underride" => Self::Underride,
            _ => return None,
        })
    }

    /// The wire name (`override`, `content`, `postcontent`, `room`, `sender`, `underride`).
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Override => "override",
            Self::Content => "content",
            Self::PostContent => "postcontent",
            Self::Room => "room",
            Self::Sender => "sender",
            Self::Underride => "underride",
        }
    }
}

impl std::fmt::Display for RuleKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A room or sender rule: actions for one room or one sender, whose `rule_id` is that room or
/// sender. Kept as a plain string, see the module docs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SimpleRule {
    /// What to do when the rule matches.
    pub actions: Vec<Action>,
    /// Whether this is a server-default rule (never, for these kinds, but the wire shape has it).
    pub default: bool,
    /// Whether the rule is enabled.
    pub enabled: bool,
    /// The room ID or user ID the rule is about.
    pub rule_id: String,
}

/// What every rule kind has in common, so the list operations below are written once.
pub trait Rule {
    /// The rule's ID.
    fn rule_id(&self) -> &str;
    /// The rule's actions.
    fn actions(&self) -> &[Action];
    /// The rule's actions, for editing.
    fn actions_mut(&mut self) -> &mut Vec<Action>;
    /// Whether the rule is enabled.
    fn enabled(&self) -> bool;
    /// Turns the rule on or off.
    fn set_enabled(&mut self, enabled: bool);
    /// Whether the rule is a server default.
    fn is_default(&self) -> bool;
}

macro_rules! impl_rule {
    ($t:ty) => {
        impl Rule for $t {
            fn rule_id(&self) -> &str {
                &self.rule_id
            }
            fn actions(&self) -> &[Action] {
                &self.actions
            }
            fn actions_mut(&mut self) -> &mut Vec<Action> {
                &mut self.actions
            }
            fn enabled(&self) -> bool {
                self.enabled
            }
            fn set_enabled(&mut self, enabled: bool) {
                self.enabled = enabled;
            }
            fn is_default(&self) -> bool {
                self.default
            }
        }
    };
}
impl_rule!(ConditionalPushRule);
impl_rule!(PatternedPushRule);
impl_rule!(SimpleRule);

/// A borrowed rule of any kind.
#[derive(Debug, Clone, Copy)]
pub enum RuleRef<'a> {
    /// An override or underride rule.
    Conditional(&'a ConditionalPushRule),
    /// A content rule.
    Patterned(&'a PatternedPushRule),
    /// A room or sender rule.
    Simple(&'a SimpleRule),
}

impl RuleRef<'_> {
    /// The rule's ID.
    #[must_use]
    pub fn rule_id(&self) -> &str {
        match self {
            Self::Conditional(r) => r.rule_id(),
            Self::Patterned(r) => r.rule_id(),
            Self::Simple(r) => r.rule_id(),
        }
    }

    /// The rule's actions.
    #[must_use]
    pub fn actions(&self) -> &[Action] {
        match self {
            Self::Conditional(r) => r.actions(),
            Self::Patterned(r) => r.actions(),
            Self::Simple(r) => r.actions(),
        }
    }

    /// Whether the rule is enabled.
    #[must_use]
    pub fn enabled(&self) -> bool {
        match self {
            Self::Conditional(r) => r.enabled(),
            Self::Patterned(r) => r.enabled(),
            Self::Simple(r) => r.enabled(),
        }
    }

    /// Whether the rule is a server default.
    #[must_use]
    pub fn is_default(&self) -> bool {
        match self {
            Self::Conditional(r) => r.is_default(),
            Self::Patterned(r) => r.is_default(),
            Self::Simple(r) => r.is_default(),
        }
    }

    /// The rule as the spec's `PushRule` JSON object (what `GET /pushrules/global/{kind}/{id}`
    /// returns).
    #[must_use]
    pub fn to_json(&self) -> Value {
        let value = match self {
            Self::Conditional(r) => serde_json::to_value(r),
            Self::Patterned(r) => serde_json::to_value(r),
            Self::Simple(r) => serde_json::to_value(r),
        };
        // Every rule type here is a plain struct of serializable fields; serialization cannot
        // fail. The fallback keeps this infallible without an `unwrap`.
        value.unwrap_or(Value::Null)
    }
}

/// A rule as `PUT /pushrules/global/{kind}/{ruleId}` describes it, before it is placed in a
/// ruleset.
#[derive(Debug, Clone)]
pub struct NewRule {
    /// Which list it goes in.
    pub kind: RuleKind,
    /// Its ID.
    pub rule_id: String,
    /// Its actions.
    pub actions: Vec<Action>,
    /// Its conditions (override and underride rules only; ignored for the other kinds).
    pub conditions: Vec<PushCondition>,
    /// Its `content.body` glob (content rules only; required for them).
    pub pattern: Option<String>,
}

/// Why a ruleset edit was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RuleError {
    /// IDs starting with `.` are reserved for server-default rules.
    #[error("rule IDs starting with '.' are reserved for server-default rules")]
    ServerDefaultRuleId,
    /// The ID is empty or contains a `/` or `\`.
    #[error("invalid rule ID")]
    InvalidRuleId,
    /// A content rule was given without a pattern.
    #[error("content rules require a pattern")]
    MissingPattern,
    /// `before`/`after` named a server-default rule, which user rules cannot be placed around.
    #[error("rules cannot be placed relative to a server-default rule")]
    RelativeToServerDefaultRule,
    /// `before`/`after` named a rule that does not exist in that kind.
    #[error("no such rule to place this one relative to")]
    UnknownRuleId,
    /// `before` named a rule above `after`.
    #[error("'before' names a rule above 'after'")]
    BeforeHigherThanAfter,
    /// The rule to edit or remove does not exist.
    #[error("no such rule")]
    NotFound,
    /// Server-default rules cannot be removed (only disabled or re-actioned).
    #[error("server-default rules cannot be removed")]
    ServerDefault,
    /// `postcontent` rules are server defaults only (MSC4306).
    #[error("user-defined rules using `postcontent` are not accepted")]
    UserPostContent,
}

/// A user's complete global ruleset. See the module docs for why this is not
/// `ruma::push::Ruleset`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Ruleset {
    /// Override rules, highest priority first.
    #[serde(rename = "override", default)]
    pub override_: Vec<ConditionalPushRule>,
    /// Content rules.
    #[serde(default)]
    pub content: Vec<PatternedPushRule>,
    /// MSC4306's `postcontent` rules (see the module docs). Left out of the JSON when empty,
    /// which it always is here, so a client that predates the kind sees the spec's shape.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub postcontent: Vec<ConditionalPushRule>,
    /// Room rules.
    #[serde(default)]
    pub room: Vec<SimpleRule>,
    /// Sender rules.
    #[serde(default)]
    pub sender: Vec<SimpleRule>,
    /// Underride rules, lowest priority.
    #[serde(default)]
    pub underride: Vec<ConditionalPushRule>,
}

fn validate_new_rule_id(rule_id: &str) -> Result<(), RuleError> {
    if rule_id.starts_with('.') {
        return Err(RuleError::ServerDefaultRuleId);
    }
    if rule_id.is_empty() || rule_id.contains('/') || rule_id.contains('\\') {
        return Err(RuleError::InvalidRuleId);
    }
    Ok(())
}

/// Places `rule` in `list`: replacing a rule with the same ID in place (keeping its `enabled`
/// flag, as the spec says a re-`PUT` must), or at `default_position` for a new one, then moving
/// it next to `after`/`before` if given. The same algorithm as `ruma::push::insert_and_move_rule`,
/// over a `Vec`.
fn insert_rule<T: Rule>(
    list: &mut Vec<T>,
    mut rule: T,
    default_position: usize,
    after: Option<&str>,
    before: Option<&str>,
) -> Result<(), RuleError> {
    let existing = list.iter().position(|r| r.rule_id() == rule.rule_id());
    let from = match existing {
        Some(i) => {
            rule.set_enabled(list[i].enabled());
            list[i] = rule;
            i
        }
        None => {
            list.push(rule);
            list.len() - 1
        }
    };
    let mut to = default_position;
    if let Some(id) = after {
        let idx = list
            .iter()
            .position(|r| r.rule_id() == id)
            .ok_or(RuleError::UnknownRuleId)?;
        to = idx + 1;
    }
    if let Some(id) = before {
        let idx = list
            .iter()
            .position(|r| r.rule_id() == id)
            .ok_or(RuleError::UnknownRuleId)?;
        if idx < to {
            return Err(RuleError::BeforeHigherThanAfter);
        }
        to = idx;
    }
    if existing.is_none() || after.is_some() || before.is_some() {
        let item = list.remove(from);
        let to = to.min(list.len());
        list.insert(to, item);
    }
    Ok(())
}

fn find<T: Rule>(list: &[T], rule_id: &str) -> Option<usize> {
    list.iter().position(|r| r.rule_id() == rule_id)
}

fn remove_rule<T: Rule>(list: &mut Vec<T>, rule_id: &str) -> Result<(), RuleError> {
    let idx = find(list, rule_id).ok_or(RuleError::NotFound)?;
    if list[idx].is_default() {
        return Err(RuleError::ServerDefault);
    }
    list.remove(idx);
    Ok(())
}

fn set_actions_on<T: Rule>(
    list: &mut [T],
    rule_id: &str,
    actions: Vec<Action>,
) -> Result<(), RuleError> {
    let idx = find(list, rule_id).ok_or(RuleError::NotFound)?;
    *list[idx].actions_mut() = actions;
    Ok(())
}

fn set_enabled_on<T: Rule>(list: &mut [T], rule_id: &str, enabled: bool) -> Result<(), RuleError> {
    let idx = find(list, rule_id).ok_or(RuleError::NotFound)?;
    list[idx].set_enabled(enabled);
    Ok(())
}

impl Ruleset {
    /// The spec's predefined rules for `user_id` (`ruma::push::Ruleset::server_default`, whose
    /// contents `crate::rulesets`' tests pin against the spec's own list).
    #[must_use]
    pub fn server_default(user_id: &UserId) -> Self {
        Self::from(ruma::push::Ruleset::server_default(user_id))
    }

    /// Every rule, in evaluation order: override, content, postcontent, room, sender,
    /// underride, each list in its own priority order.
    pub fn iter(&self) -> impl Iterator<Item = (RuleKind, RuleRef<'_>)> {
        let o = self
            .override_
            .iter()
            .map(|r| (RuleKind::Override, RuleRef::Conditional(r)));
        let c = self
            .content
            .iter()
            .map(|r| (RuleKind::Content, RuleRef::Patterned(r)));
        let p = self
            .postcontent
            .iter()
            .map(|r| (RuleKind::PostContent, RuleRef::Conditional(r)));
        let r = self
            .room
            .iter()
            .map(|r| (RuleKind::Room, RuleRef::Simple(r)));
        let s = self
            .sender
            .iter()
            .map(|r| (RuleKind::Sender, RuleRef::Simple(r)));
        let u = self
            .underride
            .iter()
            .map(|r| (RuleKind::Underride, RuleRef::Conditional(r)));
        o.chain(c).chain(p).chain(r).chain(s).chain(u)
    }

    /// The rules of one kind, in priority order.
    #[must_use]
    pub fn rules(&self, kind: RuleKind) -> Vec<RuleRef<'_>> {
        match kind {
            RuleKind::Override => self.override_.iter().map(RuleRef::Conditional).collect(),
            RuleKind::Content => self.content.iter().map(RuleRef::Patterned).collect(),
            RuleKind::PostContent => self.postcontent.iter().map(RuleRef::Conditional).collect(),
            RuleKind::Room => self.room.iter().map(RuleRef::Simple).collect(),
            RuleKind::Sender => self.sender.iter().map(RuleRef::Simple).collect(),
            RuleKind::Underride => self.underride.iter().map(RuleRef::Conditional).collect(),
        }
    }

    /// One rule by kind and ID.
    #[must_use]
    pub fn get(&self, kind: RuleKind, rule_id: &str) -> Option<RuleRef<'_>> {
        self.rules(kind)
            .into_iter()
            .find(|r| r.rule_id() == rule_id)
    }

    /// Adds `rule`, or replaces the rule with the same ID (keeping whether it was enabled). A new
    /// rule becomes the highest-priority rule of its kind, except that an override rule goes
    /// after `.m.rule.master`, which must stay first; `after`/`before` place it next to an
    /// existing user rule instead.
    ///
    /// # Errors
    /// See [`RuleError`]: a reserved or malformed ID, a content rule without a pattern, a
    /// `postcontent` rule, or a bad `after`/`before`.
    pub fn insert(
        &mut self,
        rule: NewRule,
        after: Option<&str>,
        before: Option<&str>,
    ) -> Result<(), RuleError> {
        if rule.kind == RuleKind::PostContent {
            return Err(RuleError::UserPostContent);
        }
        validate_new_rule_id(&rule.rule_id)?;
        if after.is_some_and(|s| s.starts_with('.')) || before.is_some_and(|s| s.starts_with('.')) {
            return Err(RuleError::RelativeToServerDefaultRule);
        }
        let NewRule {
            kind,
            rule_id,
            actions,
            conditions,
            pattern,
        } = rule;
        match kind {
            RuleKind::Override | RuleKind::Underride => {
                let new = ConditionalPushRule::from(ConditionalPushRuleInit {
                    actions,
                    default: false,
                    enabled: true,
                    rule_id,
                    conditions,
                });
                if kind == RuleKind::Override {
                    // `.m.rule.master` stays the highest-priority rule.
                    let position = usize::from(
                        self.override_
                            .first()
                            .is_some_and(|r| r.rule_id == ".m.rule.master"),
                    );
                    insert_rule(&mut self.override_, new, position, after, before)
                } else {
                    insert_rule(&mut self.underride, new, 0, after, before)
                }
            }
            RuleKind::PostContent => Err(RuleError::UserPostContent),
            RuleKind::Content => {
                let pattern = pattern.ok_or(RuleError::MissingPattern)?;
                let new = PatternedPushRule::from(PatternedPushRuleInit {
                    actions,
                    default: false,
                    enabled: true,
                    rule_id,
                    pattern,
                });
                insert_rule(&mut self.content, new, 0, after, before)
            }
            RuleKind::Room | RuleKind::Sender => {
                let new = SimpleRule {
                    actions,
                    default: false,
                    enabled: true,
                    rule_id,
                };
                let list = if kind == RuleKind::Room {
                    &mut self.room
                } else {
                    &mut self.sender
                };
                insert_rule(list, new, 0, after, before)
            }
        }
    }

    /// Removes a user rule.
    ///
    /// # Errors
    /// [`RuleError::NotFound`] if there is no such rule, [`RuleError::ServerDefault`] if it is a
    /// server default.
    pub fn remove(&mut self, kind: RuleKind, rule_id: &str) -> Result<(), RuleError> {
        match kind {
            RuleKind::Override => remove_rule(&mut self.override_, rule_id),
            RuleKind::Content => remove_rule(&mut self.content, rule_id),
            RuleKind::PostContent => remove_rule(&mut self.postcontent, rule_id),
            RuleKind::Room => remove_rule(&mut self.room, rule_id),
            RuleKind::Sender => remove_rule(&mut self.sender, rule_id),
            RuleKind::Underride => remove_rule(&mut self.underride, rule_id),
        }
    }

    /// Replaces a rule's actions (server-default rules included).
    ///
    /// # Errors
    /// [`RuleError::NotFound`] if there is no such rule.
    pub fn set_actions(
        &mut self,
        kind: RuleKind,
        rule_id: &str,
        actions: Vec<Action>,
    ) -> Result<(), RuleError> {
        match kind {
            RuleKind::Override => set_actions_on(&mut self.override_, rule_id, actions),
            RuleKind::Content => set_actions_on(&mut self.content, rule_id, actions),
            RuleKind::PostContent => set_actions_on(&mut self.postcontent, rule_id, actions),
            RuleKind::Room => set_actions_on(&mut self.room, rule_id, actions),
            RuleKind::Sender => set_actions_on(&mut self.sender, rule_id, actions),
            RuleKind::Underride => set_actions_on(&mut self.underride, rule_id, actions),
        }
    }

    /// Turns a rule on or off (server-default rules included).
    ///
    /// # Errors
    /// [`RuleError::NotFound`] if there is no such rule.
    pub fn set_enabled(
        &mut self,
        kind: RuleKind,
        rule_id: &str,
        enabled: bool,
    ) -> Result<(), RuleError> {
        match kind {
            RuleKind::Override => set_enabled_on(&mut self.override_, rule_id, enabled),
            RuleKind::Content => set_enabled_on(&mut self.content, rule_id, enabled),
            RuleKind::PostContent => set_enabled_on(&mut self.postcontent, rule_id, enabled),
            RuleKind::Room => set_enabled_on(&mut self.room, rule_id, enabled),
            RuleKind::Sender => set_enabled_on(&mut self.sender, rule_id, enabled),
            RuleKind::Underride => set_enabled_on(&mut self.underride, rule_id, enabled),
        }
    }

    /// The rules of one kind as the JSON list `GET /pushrules/global/{kind}/` returns.
    #[must_use]
    pub fn kind_to_json(&self, kind: RuleKind) -> Value {
        Value::Array(self.rules(kind).iter().map(RuleRef::to_json).collect())
    }
}

impl From<ruma::push::Ruleset> for Ruleset {
    fn from(r: ruma::push::Ruleset) -> Self {
        let simple =
            |actions: Vec<Action>, default: bool, enabled: bool, rule_id: String| SimpleRule {
                actions,
                default,
                enabled,
                rule_id,
            };
        Self {
            override_: r.override_.into_iter().collect(),
            content: r.content.into_iter().collect(),
            postcontent: Vec::new(),
            room: r
                .room
                .into_iter()
                .map(|s| simple(s.actions, s.default, s.enabled, s.rule_id.to_string()))
                .collect(),
            sender: r
                .sender
                .into_iter()
                .map(|s| simple(s.actions, s.default, s.enabled, s.rule_id.to_string()))
                .collect(),
            underride: r.underride.into_iter().collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ruma::user_id;

    fn room_rule(id: &str) -> NewRule {
        NewRule {
            kind: RuleKind::Room,
            rule_id: id.to_owned(),
            actions: vec![Action::Notify],
            conditions: vec![],
            pattern: None,
        }
    }

    #[test]
    fn a_room_rule_id_need_not_be_a_room_id() {
        let mut rules = Ruleset::server_default(user_id!("@alice:example.org"));
        rules
            .insert(room_rule("#spam:example.com"), None, None)
            .unwrap();
        let rule = rules.get(RuleKind::Room, "#spam:example.com").unwrap();
        assert!(rule.enabled());
        assert!(!rule.is_default());
        let json = rule.to_json();
        assert_eq!(json["rule_id"], "#spam:example.com");
        assert_eq!(json["default"], false);
        assert_eq!(json["enabled"], true);
        assert_eq!(json["actions"], serde_json::json!(["notify"]));
    }

    #[test]
    fn new_rules_go_first_and_before_after_place_them() {
        let mut rules = Ruleset::default();
        rules.insert(room_rule("#a"), None, None).unwrap();
        rules.insert(room_rule("#b"), None, None).unwrap();
        let ids = |r: &Ruleset| r.room.iter().map(|x| x.rule_id.clone()).collect::<Vec<_>>();
        assert_eq!(ids(&rules), ["#b", "#a"]);
        rules.insert(room_rule("#c"), None, Some("#a")).unwrap();
        assert_eq!(ids(&rules), ["#b", "#c", "#a"]);
        rules.insert(room_rule("#d"), Some("#b"), None).unwrap();
        assert_eq!(ids(&rules), ["#b", "#d", "#c", "#a"]);
        assert_eq!(
            rules.insert(room_rule("#e"), Some("#a"), Some("#b")),
            Err(RuleError::BeforeHigherThanAfter)
        );
        assert_eq!(
            rules.insert(room_rule("#e"), Some("#zzz"), None),
            Err(RuleError::UnknownRuleId)
        );
        assert_eq!(
            rules.insert(room_rule("#e"), None, Some(".m.rule.master")),
            Err(RuleError::RelativeToServerDefaultRule)
        );
    }

    #[test]
    fn re_putting_a_rule_keeps_its_place_and_enabled_flag() {
        let mut rules = Ruleset::default();
        rules.insert(room_rule("#a"), None, None).unwrap();
        rules.insert(room_rule("#b"), None, None).unwrap();
        rules.set_enabled(RuleKind::Room, "#a", false).unwrap();
        let mut again = room_rule("#a");
        again.actions = vec![];
        rules.insert(again, None, None).unwrap();
        assert_eq!(rules.room.len(), 2, "idempotent");
        assert_eq!(rules.room[1].rule_id, "#a");
        assert!(!rules.room[1].enabled, "enabled survives a re-PUT");
        assert!(rules.room[1].actions.is_empty(), "actions are replaced");
    }

    #[test]
    fn override_rules_slot_in_after_master() {
        let mut rules = Ruleset::server_default(user_id!("@alice:example.org"));
        rules
            .insert(
                NewRule {
                    kind: RuleKind::Override,
                    rule_id: "mine".to_owned(),
                    actions: vec![Action::Notify],
                    conditions: vec![],
                    pattern: None,
                },
                None,
                None,
            )
            .unwrap();
        assert_eq!(rules.override_[0].rule_id, ".m.rule.master");
        assert_eq!(rules.override_[1].rule_id, "mine");
    }

    #[test]
    fn reserved_and_malformed_ids_are_refused() {
        let mut rules = Ruleset::default();
        assert_eq!(
            rules.insert(room_rule(".mine"), None, None),
            Err(RuleError::ServerDefaultRuleId)
        );
        assert_eq!(
            rules.insert(room_rule("a/b"), None, None),
            Err(RuleError::InvalidRuleId)
        );
        assert_eq!(
            rules.insert(room_rule("a\\b"), None, None),
            Err(RuleError::InvalidRuleId)
        );
        assert_eq!(
            rules.insert(room_rule(""), None, None),
            Err(RuleError::InvalidRuleId)
        );
        let mut content = room_rule("x");
        content.kind = RuleKind::Content;
        assert_eq!(
            rules.insert(content, None, None),
            Err(RuleError::MissingPattern)
        );
    }

    #[test]
    fn defaults_can_be_edited_but_not_removed() {
        let mut rules = Ruleset::server_default(user_id!("@alice:example.org"));
        assert_eq!(
            rules.remove(RuleKind::Override, ".m.rule.master"),
            Err(RuleError::ServerDefault)
        );
        assert_eq!(
            rules.remove(RuleKind::Override, "nope"),
            Err(RuleError::NotFound)
        );
        rules
            .set_actions(RuleKind::Underride, ".m.rule.message", vec![])
            .unwrap();
        assert!(
            rules
                .get(RuleKind::Underride, ".m.rule.message")
                .unwrap()
                .actions()
                .is_empty()
        );
        assert!(
            rules
                .get(RuleKind::Underride, ".m.rule.message")
                .unwrap()
                .is_default()
        );
        rules
            .set_enabled(RuleKind::Override, ".m.rule.master", true)
            .unwrap();
        assert!(
            rules
                .get(RuleKind::Override, ".m.rule.master")
                .unwrap()
                .enabled()
        );
        rules.insert(room_rule("#a"), None, None).unwrap();
        rules.remove(RuleKind::Room, "#a").unwrap();
        assert!(rules.room.is_empty());
    }

    #[test]
    fn postcontent_rules_are_evaluated_in_their_place_and_never_added_by_a_client() {
        let mut rules = Ruleset::default();
        let mut rule = room_rule("mine");
        rule.kind = RuleKind::PostContent;
        assert_eq!(
            rules.insert(rule, None, None),
            Err(RuleError::UserPostContent)
        );
        assert_eq!(RuleKind::parse("postcontent"), Some(RuleKind::PostContent));
        let order: Vec<&str> = RuleKind::ALL.iter().map(|k| k.as_str()).collect();
        assert_eq!(
            order,
            [
                "override",
                "content",
                "postcontent",
                "room",
                "sender",
                "underride"
            ]
        );
        let with_one: Ruleset = serde_json::from_value(serde_json::json!({
            "postcontent": [{"rule_id": ".x", "default": true, "enabled": true,
                             "conditions": [], "actions": ["notify"]}],
            "room": [{"rule_id": "!r:x", "default": false, "enabled": true, "actions": []}],
        }))
        .unwrap();
        let kinds: Vec<RuleKind> = with_one.iter().map(|(k, _)| k).collect();
        assert_eq!(kinds, [RuleKind::PostContent, RuleKind::Room]);
        assert_eq!(
            serde_json::to_value(&with_one).unwrap()["postcontent"][0]["rule_id"],
            ".x"
        );
    }

    #[test]
    fn json_round_trips_through_rumas_shape() {
        let alice = user_id!("@alice:example.org");
        let theirs = serde_json::to_value(ruma::push::Ruleset::server_default(alice)).unwrap();
        let ours: Ruleset = serde_json::from_value(theirs.clone()).unwrap();
        let mut ours_json = serde_json::to_value(&ours).unwrap();
        // Ruma omits empty lists; the spec's example shows every kind. Both read back the same.
        // `postcontent` (MSC4306) is not the spec's: left out when empty, as it is here.
        assert!(ours_json.get("postcontent").is_none());
        for kind in RuleKind::ALL {
            if kind != RuleKind::PostContent && theirs.get(kind.as_str()).is_none() {
                assert_eq!(ours_json[kind.as_str()], serde_json::json!([]));
                ours_json.as_object_mut().unwrap().remove(kind.as_str());
            }
        }
        assert_eq!(ours_json, theirs);
        let back: ruma::push::Ruleset = serde_json::from_value(ours_json).unwrap();
        assert_eq!(back.underride.len(), ours.underride.len());
    }
}
