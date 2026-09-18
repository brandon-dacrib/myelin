//! One namespace declaration (`namespaces.{users,aliases,rooms}[]`) from an appservice
//! registration, and the three-category [`Namespaces`] container.

use crate::regexp::{NamespacePattern, NamespacePatternError};

/// Which of the three namespace categories a registration declares. Mirrors the spec's
/// `application-service/definitions/namespace.yaml` and its three call sites (`users`, `aliases`,
/// `rooms`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NamespaceKind {
    /// Full user IDs (`@_ircbridge_.*:example.org`).
    Users,
    /// Room aliases (`#_ircbridge_.*:example.org`).
    Aliases,
    /// Room IDs. Unlike users and aliases this is rarely used by mautrix bridges (rooms are
    /// usually created by the bridge itself, not matched by pattern) but is part of the spec and
    /// some legacy `mautrix-python` bridges and `matrix-appservice-irc` declare it.
    Rooms,
}

impl NamespaceKind {
    /// The registration YAML key for this category.
    #[must_use]
    pub fn key(self) -> &'static str {
        match self {
            Self::Users => "users",
            Self::Aliases => "aliases",
            Self::Rooms => "rooms",
        }
    }
}

/// One compiled namespace rule: a pattern and whether it is exclusive.
///
/// Exclusive namespaces (`exclusive: true`) reserve every matching ID to this appservice alone —
/// no other appservice may register an overlapping exclusive namespace, and (for the `users`
/// category) no human may register a matching localpart. Non-exclusive namespaces just grant
/// masquerading rights (`user_id`) without reserving anything, which is exactly the mechanism
/// double puppeting relies on: a bridge's `users` namespace is non-exclusive so it can masquerade
/// as a real, already-registered human account without claiming to own that account's ID.
#[derive(Debug, Clone)]
pub struct NamespaceRule {
    /// The compiled pattern, matched with Python `re.match` (prefix) semantics — see
    /// [`crate::regexp`].
    pub pattern: NamespacePattern,
    /// Whether this namespace is exclusive. Defaults to `false` when the registration omits the
    /// key, matching the spec's `namespace.yaml` default.
    pub exclusive: bool,
}

impl NamespaceRule {
    /// Compiles a rule from its registration-file fields.
    ///
    /// # Errors
    /// Returns [`NamespacePatternError`] if `regex` does not compile under either regex engine.
    pub fn compile(regex: &str, exclusive: bool) -> Result<Self, NamespacePatternError> {
        Ok(Self {
            pattern: NamespacePattern::compile(regex)?,
            exclusive,
        })
    }

    /// True if `text` matches this rule's pattern.
    #[must_use]
    pub fn is_match(&self, text: &str) -> bool {
        self.pattern.is_match(text)
    }
}

/// The three namespace categories a registration declares, each a list of rules matched in
/// declaration order (the first match wins for "is this exclusive", matching Synapse's behavior
/// of checking every declared namespace and treating any exclusive match as exclusive regardless
/// of order — see [`Namespaces::exclusive_match`]).
#[derive(Debug, Clone, Default)]
pub struct Namespaces {
    /// `namespaces.users`.
    pub users: Vec<NamespaceRule>,
    /// `namespaces.aliases`.
    pub aliases: Vec<NamespaceRule>,
    /// `namespaces.rooms`.
    pub rooms: Vec<NamespaceRule>,
}

impl Namespaces {
    /// The rules for one category.
    #[must_use]
    pub fn category(&self, kind: NamespaceKind) -> &[NamespaceRule] {
        match kind {
            NamespaceKind::Users => &self.users,
            NamespaceKind::Aliases => &self.aliases,
            NamespaceKind::Rooms => &self.rooms,
        }
    }

    /// True if any rule in `kind` matches `text`, regardless of exclusivity — "is this appservice
    /// interested in this ID at all" (Synapse's `is_interested_in_user`/`is_interested_in_alias`).
    #[must_use]
    pub fn is_interested(&self, kind: NamespaceKind, text: &str) -> bool {
        self.category(kind).iter().any(|r| r.is_match(text))
    }

    /// True if any *exclusive* rule in `kind` matches `text` — "does this appservice exclusively
    /// own this ID" (Synapse's `is_exclusive_user`/`is_exclusive_alias`), which is what namespace
    /// conflict detection and human-registration blocking key on.
    #[must_use]
    pub fn exclusive_match(&self, kind: NamespaceKind, text: &str) -> bool {
        self.category(kind)
            .iter()
            .any(|r| r.exclusive && r.is_match(text))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rule(pattern: &str, exclusive: bool) -> NamespaceRule {
        NamespaceRule::compile(pattern, exclusive).unwrap()
    }

    #[test]
    fn is_interested_ignores_exclusivity() {
        let ns = Namespaces {
            users: vec![rule(r"^@irc_.*:example\.org$", false)],
            ..Default::default()
        };
        assert!(ns.is_interested(NamespaceKind::Users, "@irc_bob:example.org"));
        assert!(!ns.exclusive_match(NamespaceKind::Users, "@irc_bob:example.org"));
    }

    #[test]
    fn exclusive_match_requires_exclusive_flag() {
        let ns = Namespaces {
            users: vec![rule(r"^@irc_.*:example\.org$", true)],
            ..Default::default()
        };
        assert!(ns.exclusive_match(NamespaceKind::Users, "@irc_bob:example.org"));
        assert!(!ns.exclusive_match(NamespaceKind::Users, "@other:example.org"));
    }
}
