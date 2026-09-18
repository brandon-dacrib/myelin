//! Python-`re`-compatible regular expressions for appservice registration namespaces.
//!
//! Synapse loads a registration's `namespaces.{users,aliases,rooms}[].regex` with Python's `re`
//! module (`synapse/appservice/__init__.py`'s `_Namespace`, behavioral reference only) and matches
//! it with `re.match` semantics: the pattern is implicitly anchored at the start of the string but
//! not at the end, i.e. it matches a *prefix*. mautrix bridges nearly always write their own
//! patterns fully anchored (`^@irc_.*:example\.org$`), but a registration authored for Synapse is
//! entitled to omit the trailing `$` and still have it behave as Synapse would.
//!
//! Most registration patterns mautrix bridges emit (character classes, `.*`, alternation, escaped
//! dots) are within the subset the linear-time `regex` crate already supports, so this module
//! tries `regex` first — it is faster and has no catastrophic-backtracking risk, which matters
//! because these patterns run on every request that touches a namespaced ID. Patterns that need
//! Python-only features `regex` cannot express (lookahead/lookbehind `(?=...)`, `(?!...)`,
//! `(?<=...)`, `(?<!...)`, and backreferences `\1`) fall back to [`fancy_regex`], which supports
//! them at the cost of potential exponential backtracking on adversarial input — acceptable here
//! because registration files are operator-supplied, not attacker-supplied.
//!
//! # Known divergences from Python's `re`, documented rather than silently accepted
//!
//! - Python's `re` is Unicode-aware by default for `\w`, `\d`, `\s` etc. against `str` patterns;
//!   both `regex` and `fancy_regex` are Unicode-aware by default too, so this generally matches.
//! - Python's `re.match` anchors only the start; this module's [`NamespacePattern::is_match`]
//!   reproduces that by prefixing every pattern with `\A` (not `^`, so multi-line flags some
//!   registration author might set cannot change the anchoring) rather than requiring the pattern
//!   to already start with `^`. A pattern that already starts with `^` or `\A` is used as-is: a
//!   redundant anchor is harmless.
//! - Named groups (`(?P<name>...)`), possessive quantifiers and atomic groups are accepted by
//!   `fancy_regex` but their Python-specific semantics around `\g<name>` backreferences are not
//!   exercised by any mautrix or Synapse registration we have seen; if one shows up, treat it as a
//!   fresh divergence to document here, not as pre-covered.

use std::fmt;

/// A compiled namespace regular expression, matched the way Synapse matches
/// `namespaces.*[].regex`: anchored at the start, not required to be anchored at the end.
#[derive(Clone)]
pub struct NamespacePattern {
    source: String,
    engine: Engine,
}

#[derive(Clone)]
enum Engine {
    /// The common case: linear-time, no backtracking blowup.
    Fast(Box<regex::Regex>),
    /// Fallback for patterns using lookaround or backreferences.
    Fancy(Box<fancy_regex::Regex>),
}

/// A pattern that failed to compile under both engines.
#[derive(Debug, Clone)]
pub struct NamespacePatternError {
    /// The offending pattern source.
    pub pattern: String,
    /// The `fancy_regex` compiler's error message (the more permissive engine, so its error is
    /// the more informative one to surface).
    pub message: String,
}

impl fmt::Display for NamespacePatternError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "invalid namespace regex {:?}: {}",
            self.pattern, self.message
        )
    }
}

impl std::error::Error for NamespacePatternError {}

/// Rewrites `pattern` so it is anchored at the start the way Python's `re.match` (and therefore
/// Synapse) anchors, unless it already starts with an explicit `^` or `\A`.
fn anchor_at_start(pattern: &str) -> String {
    if pattern.starts_with('^') || pattern.starts_with("\\A") {
        pattern.to_string()
    } else {
        format!("\\A(?:{pattern})")
    }
}

impl NamespacePattern {
    /// Compiles `pattern`, trying the linear-time `regex` engine first and falling back to
    /// `fancy_regex` for lookaround or backreferences. See the module docs for the anchoring rule.
    ///
    /// # Errors
    /// Returns [`NamespacePatternError`] if neither engine can compile `pattern` at all (a
    /// genuinely malformed pattern, not just one using fancy features).
    pub fn compile(pattern: &str) -> Result<Self, NamespacePatternError> {
        let anchored = anchor_at_start(pattern);
        if let Ok(re) = regex::Regex::new(&anchored) {
            return Ok(Self {
                source: pattern.to_string(),
                engine: Engine::Fast(Box::new(re)),
            });
        }
        match fancy_regex::Regex::new(&anchored) {
            Ok(re) => Ok(Self {
                source: pattern.to_string(),
                engine: Engine::Fancy(Box::new(re)),
            }),
            Err(e) => Err(NamespacePatternError {
                pattern: pattern.to_string(),
                message: e.to_string(),
            }),
        }
    }

    /// The original, unanchored pattern text as it appeared in the registration file.
    #[must_use]
    pub fn source(&self) -> &str {
        &self.source
    }

    /// True if `pattern` matches a prefix of `text` (Python `re.match` semantics).
    #[must_use]
    pub fn is_match(&self, text: &str) -> bool {
        match &self.engine {
            Engine::Fast(re) => re.is_match(text),
            // fancy_regex's `is_match` returns `Result` because catastrophic backtracking can, in
            // principle, exceed its internal backtrack budget; treat that as "did not match"
            // rather than panicking or propagating, since a namespace check must never crash a
            // request handler.
            Engine::Fancy(re) => re.is_match(text).unwrap_or(false),
        }
    }

    /// True if this pattern needed the `fancy_regex` fallback (lookaround or backreferences) —
    /// exposed so the registration importer can warn operators that this namespace runs on the
    /// slower, backtracking engine.
    #[must_use]
    pub fn uses_fancy_engine(&self) -> bool {
        matches!(self.engine, Engine::Fancy(_))
    }

    /// Compiles this same pattern as a plain [`regex::Regex`] with the same start-anchoring, for
    /// interop with [`hs_auth::appservice::NamespaceRule`], whose `regex` field is hard-typed to
    /// `regex::Regex` (it predates this crate; see `crates/hs-auth/src/appservice.rs`). Returns
    /// `None` when this pattern only compiles under `fancy_regex` — in that case the fast
    /// masquerade check `hs-auth`'s middleware performs cannot express this namespace, which is
    /// the documented divergence: patterns needing lookaround or backreferences are honored by
    /// this crate's own namespace matching (registry conflict checks, `/users`/`/rooms` query
    /// routing) but not by `hs-auth`'s `user_id` masquerade check, which falls back to denying the
    /// masquerade for that specific namespace rather than guessing.
    #[must_use]
    pub fn as_regex_crate(&self) -> Option<regex::Regex> {
        match &self.engine {
            Engine::Fast(re) => Some((**re).clone()),
            Engine::Fancy(_) => None,
        }
    }
}

impl fmt::Debug for NamespacePattern {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("NamespacePattern")
            .field("source", &self.source)
            .field("fancy", &self.uses_fancy_engine())
            .finish()
    }
}

impl PartialEq for NamespacePattern {
    fn eq(&self, other: &Self) -> bool {
        self.source == other.source
    }
}
impl Eq for NamespacePattern {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_a_prefix_like_python_re_match() {
        let p = NamespacePattern::compile(r"@irc_.*:example\.org").unwrap();
        assert!(p.is_match("@irc_bob:example.org"));
        // Python `re.match` allows trailing garbage since the pattern is not end-anchored.
        assert!(p.is_match("@irc_bob:example.orgTRAILING"));
        assert!(!p.is_match("prefix@irc_bob:example.org"));
    }

    #[test]
    fn explicit_end_anchor_is_respected() {
        let p = NamespacePattern::compile(r"^@irc_.*:example\.org$").unwrap();
        assert!(p.is_match("@irc_bob:example.org"));
        assert!(!p.is_match("@irc_bob:example.orgTRAILING"));
    }

    #[test]
    fn already_anchored_pattern_is_not_double_wrapped() {
        let p = NamespacePattern::compile(r"^@irc_.*:example\.org$").unwrap();
        assert!(!p.uses_fancy_engine());
    }

    #[test]
    fn falls_back_to_fancy_regex_for_lookahead() {
        // A pattern using negative lookahead, which the `regex` crate cannot compile.
        let p = NamespacePattern::compile(r"^@irc_(?!admin_).*:example\.org$").unwrap();
        assert!(p.uses_fancy_engine());
        assert!(p.is_match("@irc_bob:example.org"));
        assert!(!p.is_match("@irc_admin_bob:example.org"));
    }

    #[test]
    fn fancy_pattern_has_no_regex_crate_projection() {
        let p = NamespacePattern::compile(r"^@irc_(?!admin_).*:example\.org$").unwrap();
        assert!(p.as_regex_crate().is_none());
    }

    #[test]
    fn fast_pattern_has_a_regex_crate_projection() {
        let p = NamespacePattern::compile(r"^@irc_.*:example\.org$").unwrap();
        let re = p.as_regex_crate().unwrap();
        assert!(re.is_match("@irc_bob:example.org"));
    }

    #[test]
    fn malformed_pattern_is_an_error() {
        assert!(NamespacePattern::compile(r"@irc_(unterminated").is_err());
    }
}
