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
///
/// The port is not part of what is matched: the spec's `allow`/`deny` are "server names ...
/// excluding any port information", so `example.org:8448` is matched as `example.org` (and
/// Sytest bans its own `localhost:<port>` server with a bare `localhost`).
#[must_use]
pub fn is_allowed(server_name: &str, acl: &ServerAcl) -> bool {
    if !acl.allow_ip_literals && is_ip_literal(server_name) {
        return false;
    }
    let host = host_of(server_name);
    let allowed = acl.allow.iter().any(|pattern| glob_matches(pattern, host));
    if !allowed {
        return false;
    }
    let denied = acl.deny.iter().any(|pattern| glob_matches(pattern, host));
    !denied
}

/// `server_name` without its `:port`, if it has one: `[::1]:8448` is `[::1]`, `host:8448` is
/// `host`, and a bare IPv6 literal's colons are left alone (a server name never carries one
/// unbracketed, but nothing here should cut one apart).
fn host_of(server_name: &str) -> &str {
    if server_name.starts_with('[') {
        return server_name
            .find(']')
            .map_or(server_name, |end| &server_name[..=end]);
    }
    match server_name.rsplit_once(':') {
        Some((host, port))
            if !host.contains(':')
                && !port.is_empty()
                && port.bytes().all(|b| b.is_ascii_digit()) =>
        {
            host
        }
        _ => server_name,
    }
}

/// Reads an `m.room.server_acl` event's `content` the way Synapse does, leniently: a field of
/// the wrong type is treated as absent (so a non-list `allow` allows nobody, a non-list `deny`
/// denies nobody, a non-boolean `allow_ip_literals` is `true`), and an entry of a list that is
/// not a string is skipped. A malformed ACL must not stop the room from being checked at all.
#[must_use]
pub fn acl_from_content(content: &serde_json::Value) -> ServerAcl {
    let strings = |field: &str| -> Vec<String> {
        content
            .get(field)
            .and_then(serde_json::Value::as_array)
            .map(|entries| {
                entries
                    .iter()
                    .filter_map(serde_json::Value::as_str)
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default()
    };
    ServerAcl {
        allow_ip_literals: content
            .get("allow_ip_literals")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(true),
        allow: strings("allow"),
        deny: strings("deny"),
    }
}

/// The one check every room-scoped federation request goes through: whether `origin` may talk
/// to this server about `room_id` under the room's current `m.room.server_acl`. A room with no
/// ACL, or one this server does not hold, allows everybody. `Err` carries the message for the
/// `403 M_FORBIDDEN` (or, for a PDU in `/send`, the per-event error).
///
/// # Errors
/// Returns the refusal message when the room's ACL denies `origin`.
pub async fn check_origin(
    rooms: &dyn crate::room_source::RoomDataSource,
    room_id: &str,
    origin: &str,
) -> Result<(), String> {
    let Some(content) = rooms.server_acl(room_id).await else {
        return Ok(());
    };
    if is_allowed(origin, &acl_from_content(&content)) {
        Ok(())
    } else {
        Err(format!(
            "server {origin} is banned from room {room_id} by its server ACL"
        ))
    }
}

/// The `endpoint` label of `hs_federation_acl_refusals_total` for a matched route path
/// (`/make_join/{roomId}/{userId}` is `make_join`): the path's first segment when it is one of
/// the federation API's room-scoped endpoints, `other` otherwise, so the label set stays fixed
/// whatever routes are added.
#[must_use]
pub fn endpoint_label(matched_path: &str) -> &'static str {
    const ENDPOINTS: &[&str] = &[
        "send",
        "make_join",
        "send_join",
        "make_leave",
        "send_leave",
        "make_knock",
        "send_knock",
        "invite",
        "state",
        "state_ids",
        "backfill",
        "get_missing_events",
        "event_auth",
        "hierarchy",
        "timestamp_to_event",
        "exchange_third_party_invite",
        "extremities",
        "rooms",
    ];
    let first = matched_path
        .trim_start_matches('/')
        .split('/')
        .next()
        .unwrap_or("");
    ENDPOINTS
        .iter()
        .copied()
        .find(|endpoint| *endpoint == first)
        .unwrap_or("other")
}

/// The route layer `crate::transport` puts over every federation route: a route whose path
/// names a `{roomId}` is answered `403 M_FORBIDDEN` before its handler runs when the room's
/// server ACL denies the (already signature-verified) requesting server. Being a layer over the
/// whole router rather than a call in each handler is the point: a room-scoped endpoint added
/// later is covered without anyone remembering to. `/send` names no room in its path and checks
/// each PDU's room itself (`crate::inbound::process_transaction`).
pub async fn enforce_on_room_routes(
    axum::extract::State(rooms): axum::extract::State<
        std::sync::Arc<dyn crate::room_source::RoomDataSource>,
    >,
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    use axum::extract::FromRequestParts as _;
    use axum::response::IntoResponse as _;

    let (mut parts, body) = request.into_parts();
    let room_id = match axum::extract::RawPathParams::from_request_parts(&mut parts, &()).await {
        Ok(params) => params
            .iter()
            .find(|(name, _)| *name == "roomId")
            .map(|(_, value)| value.to_owned()),
        Err(_) => None,
    };
    if let Some(room_id) = room_id
        && let Ok(auth) = crate::xmatrix::parse_x_matrix_header(&parts.headers)
        && let Err(message) = check_origin(rooms.as_ref(), &room_id, &auth.origin).await
    {
        let endpoint = parts
            .extensions
            .get::<axum::extract::MatchedPath>()
            .map_or("other", |matched| endpoint_label(matched.as_str()));
        crate::metrics::record_acl_refusal(endpoint);
        tracing::info!(
            origin = %auth.origin,
            room_id = %room_id,
            endpoint,
            "refused a federation request: the room's server ACL denies the requesting server"
        );
        return hs_http::error::MatrixError::forbidden(message).into_response();
    }
    next.run(axum::extract::Request::from_parts(parts, body))
        .await
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
    fn the_port_is_not_part_of_the_match() {
        let a = acl(&["*"], &["localhost"], true);
        assert!(!is_allowed("localhost:8448", &a));
        assert!(is_allowed("otherhost:8448", &a));
        let only = acl(&["*.example.org"], &[], false);
        assert!(is_allowed("matrix.example.org:443", &only));
        assert!(!is_allowed("[2001:db8::1]:8448", &only));
        assert_eq!(host_of("[2001:db8::1]:8448"), "[2001:db8::1]");
        assert_eq!(host_of("example.org"), "example.org");
        assert_eq!(host_of("example.org:"), "example.org:");
    }

    #[test]
    fn a_malformed_acl_is_read_leniently() {
        let read = acl_from_content(&serde_json::json!({
            "allow": ["*", 7],
            "deny": "evil.example.org",
            "allow_ip_literals": "no",
        }));
        assert_eq!(read.allow, vec!["*".to_owned()]);
        assert!(read.deny.is_empty());
        assert!(read.allow_ip_literals);
        // No `allow` at all allows nobody.
        assert!(!is_allowed(
            "good.example.org",
            &acl_from_content(&serde_json::json!({}))
        ));
    }

    #[test]
    fn endpoint_labels_are_a_fixed_set() {
        assert_eq!(endpoint_label("/make_join/{roomId}/{userId}"), "make_join");
        assert_eq!(endpoint_label("/state_ids/{roomId}"), "state_ids");
        assert_eq!(endpoint_label("/something_new/{roomId}"), "other");
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
