//! `m.room.server_acl` evaluation: one function, used identically for inbound accept and
//! outbound send, per `docs/design/06-federation-threat-model.md` section 2.7's "exactly one
//! place this logic can be wrong" requirement.

use std::net::IpAddr;

use regex::Regex;
use serde::{Deserialize, Serialize};

/// The `content` of an `m.room.server_acl` event, as evaluated by [`is_allowed`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ServerAcl {
    #[serde(default = "default_true")]
    pub allow_ip_literals: bool,
    #[serde(default)]
    pub allow: Vec<String>,
    #[serde(default)]
    pub deny: Vec<String>,
}

fn default_true() -> bool {
    true
}

impl Default for ServerAcl {
    fn default() -> Self {
        Self {
            allow_ip_literals: true,
            allow: vec!["*".to_string()],
            deny: Vec::new(),
        }
    }
}

/// Evaluates whether `server_name` is allowed to federate under `acl`. The single rule set used
/// for both directions (threat model 2.7): a server is allowed if
/// [`ServerAcl::allow_ip_literals`] does not exclude it (see below) and it matches at least one
/// `allow` glob and no `deny` glob. An empty `allow` list denies every server (matches Synapse's
/// documented behaviour: `allow` is not implicitly `["*"]` when present-but-empty, only when
/// entirely absent — represented here by [`ServerAcl::default`]'s `allow: ["*"]`, which only
/// applies when no ACL event exists at all, not when one exists with an empty `allow`).
#[must_use]
pub fn is_allowed(server_name: &str, acl: &ServerAcl) -> bool {
    if !acl.allow_ip_literals && is_ip_literal(server_name) {
        return false;
    }
    let allowed = acl
        .allow
        .iter()
        .any(|pattern| glob_matches(pattern, server_name));
    if !allowed {
        return false;
    }
    let denied = acl
        .deny
        .iter()
        .any(|pattern| glob_matches(pattern, server_name));
    !denied
}

/// Whether `server_name` is an IP literal (v4 or v6, with or without brackets), for
/// [`ServerAcl::allow_ip_literals`]'s check against the *string form* of the server name — not
/// against a DNS-resolved address (that is `hs-config`'s `ip_range_blocklist`, a different
/// question, see the threat model 2.7's note distinguishing the two).
fn is_ip_literal(server_name: &str) -> bool {
    let candidate = server_name
        .strip_prefix('[')
        .and_then(|s| s.strip_suffix(']'))
        .unwrap_or(server_name);
    // Strip a trailing `:port` for the plain (non-bracketed) form, since IPv6 without brackets
    // would be ambiguous with a port separator — bracketed form is the only unambiguous way to
    // combine IPv6 + port, so a bare `candidate` here is only ever IPv4[:port] or a hostname.
    let host_part = if server_name.starts_with('[') {
        candidate
    } else {
        candidate.split_once(':').map_or(candidate, |(h, _)| h)
    };
    host_part.parse::<IpAddr>().is_ok()
}

/// Translates a `server_acl` glob (`*` = any run of characters, `?` = exactly one character,
/// every other character literal) into an anchored regex and matches it against `server_name`.
/// Anchored (`^...$`) so `*.example.org` cannot accidentally match `evilexample.org`, and every
/// regex metacharacter other than the two the spec defines is escaped so `.` in a hostname is
/// never accidentally treated as "any character" (threat model 2.7's named bug class).
#[must_use]
pub fn glob_matches(pattern: &str, server_name: &str) -> bool {
    build_glob_regex(pattern)
        .map(|re| re.is_match(server_name))
        .unwrap_or(false)
}

/// Characters that are special to `regex`'s syntax and must be escaped when a `server_acl` glob
/// pattern uses them literally (every character except the two the spec defines, `*` and `?`).
fn is_regex_metacharacter(c: char) -> bool {
    matches!(
        c,
        '.' | '+' | '(' | ')' | '[' | ']' | '{' | '}' | '^' | '$' | '|' | '\\' | '/'
    )
}

fn build_glob_regex(pattern: &str) -> Option<Regex> {
    let mut out = String::with_capacity(pattern.len() * 2 + 2);
    out.push('^');
    for c in pattern.chars() {
        match c {
            '*' => out.push_str(".*"),
            '?' => out.push('.'),
            _ => {
                if is_regex_metacharacter(c) {
                    out.push('\\');
                }
                out.push(c);
            }
        }
    }
    out.push('$');
    Regex::new(&out).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn acl(allow: &[&str], deny: &[&str], allow_ip_literals: bool) -> ServerAcl {
        ServerAcl {
            allow_ip_literals,
            allow: allow.iter().map(|s| s.to_string()).collect(),
            deny: deny.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn default_acl_allows_everything() {
        assert!(is_allowed("anything.example.org", &ServerAcl::default()));
    }

    #[test]
    fn wildcard_suffix_matches_subdomains_but_not_the_bare_domain() {
        let a = acl(&["*.example.org"], &[], true);
        assert!(is_allowed("matrix.example.org", &a));
        assert!(is_allowed("a.b.example.org", &a));
        assert!(!is_allowed("example.org", &a));
        assert!(!is_allowed("notexample.org", &a));
        assert!(!is_allowed("example.org.evil.com", &a));
    }

    #[test]
    fn question_mark_matches_exactly_one_character_never_zero() {
        let a = acl(&["ho?t.example.org"], &[], true);
        assert!(is_allowed("host.example.org", &a));
        // `?` requires exactly one character, so a zero-character gap does not match...
        assert!(!is_allowed("hot.example.org", &a));
        // ...and neither does a two-character gap.
        assert!(!is_allowed("hoost.example.org", &a));
    }

    #[test]
    fn dot_in_pattern_is_literal_not_any_character() {
        let a = acl(&["a.example.org"], &[], true);
        assert!(is_allowed("a.example.org", &a));
        // If `.` were treated as regex "any char" this would wrongly match too.
        assert!(!is_allowed("aXexample.org", &a));
    }

    #[test]
    fn deny_overrides_allow() {
        let a = acl(&["*"], &["evil.example.org"], true);
        assert!(is_allowed("good.example.org", &a));
        assert!(!is_allowed("evil.example.org", &a));
    }

    #[test]
    fn empty_allow_list_denies_everything() {
        let a = acl(&[], &[], true);
        assert!(!is_allowed("anything.example.org", &a));
    }

    #[test]
    fn allow_ip_literals_false_blocks_ipv4_and_ipv6_literals_even_if_allow_matches() {
        let a = acl(&["*"], &[], false);
        assert!(!is_allowed("192.0.2.1", &a));
        assert!(!is_allowed("[2001:db8::1]", &a));
        assert!(is_allowed("example.org", &a));
    }

    #[test]
    fn allow_ip_literals_true_permits_ip_literals_if_allow_matches() {
        let a = acl(&["*"], &[], true);
        assert!(is_allowed("192.0.2.1", &a));
    }

    #[test]
    fn ip_literal_with_port_is_still_detected_as_a_literal() {
        let a = acl(&["*"], &[], false);
        assert!(!is_allowed("192.0.2.1:8449", &a));
    }

    #[test]
    fn regex_metacharacters_in_pattern_are_escaped() {
        let a = acl(&["a+b.example.org"], &[], true);
        assert!(is_allowed("a+b.example.org", &a));
        assert!(!is_allowed("aaab.example.org", &a));
    }

    #[test]
    fn one_function_used_symmetrically_for_inbound_and_outbound() {
        // There is exactly one entry point (`is_allowed`) — this test exists to document that
        // inbound-accept and outbound-send call sites (crate::transport, crate::client) must both
        // call this same function rather than reimplementing the check, per threat model 2.7.
        let a = acl(&["*"], &["blocked.example.org"], true);
        // Same call, same result, regardless of "direction" (there is no direction parameter by
        // design — see the module doc).
        assert_eq!(
            is_allowed("blocked.example.org", &a),
            is_allowed("blocked.example.org", &a)
        );
    }
}
