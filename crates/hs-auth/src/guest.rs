//! Guest access: which client-server endpoints a guest account may call, and the counters an
//! operator watches guest use with.
//!
//! A guest account (`POST /register?kind=guest`, allowed only while `auth.allow_guest_access` is
//! on) is a temporary account with no password. The spec's "Guest Access" module
//! (`refs/matrix-spec/content/client-server-api/modules/guest_access.md`) lists the endpoints a
//! guest may use; everything else answers `403 M_GUEST_ACCESS_FORBIDDEN`. Rather than every
//! handler in every crate choosing between [`crate::requester::Requester`] and
//! [`crate::middleware::AllowGuest`], the one [`GUEST_ENDPOINTS`] table below decides, by method
//! and path, inside the `Requester` extractor itself: a handler written for full accounts stays
//! closed to guests unless its endpoint is listed here, and the whole guest surface can be
//! audited in one place.
//!
//! The table is the spec's list, plus what Synapse also lets a guest do (its servlets with
//! `allow_guest=True`, read for behavior only) where a guest in a room needs it: joining by
//! alias, typing, read receipts and markers, presence, the TURN server, capabilities, the joined
//! rooms list, the public room directory, relations and `/keys/changes`.
//!
//! What a guest may *do* once an endpoint admits it -- join only a room whose
//! `m.room.guest_access` is `can_join`, and be made to leave when that changes -- is the room
//! layer's (`hs_room::routes::membership`, `hs_room::actor`).

use std::sync::LazyLock;

use axum::http::Method;
use prometheus_client::encoding::EncodeLabelSet;
use prometheus_client::metrics::counter::Counter;
use prometheus_client::metrics::family::Family;

/// One endpoint a guest may call: an HTTP method and a path relative to the client-server API
/// version (`rooms/*/messages` for `/_matrix/client/v3/rooms/{roomId}/messages`), where `*`
/// stands for exactly one path segment and a trailing `**` for any number of them.
#[derive(Debug, Clone, Copy)]
pub struct GuestEndpoint {
    /// The HTTP method.
    pub method: &'static str,
    /// The path pattern after `/_matrix/client/<version>/`.
    pub path: &'static str,
}

const fn ep(method: &'static str, path: &'static str) -> GuestEndpoint {
    GuestEndpoint { method, path }
}

/// Every endpoint a guest may call besides `GET /account/whoami` and `POST /logout[/all]`, which
/// take [`crate::middleware::AllowGuest`] and admit guests themselves.
pub const GUEST_ENDPOINTS: &[GuestEndpoint] = &[
    // The spec: retrieving events and media.
    ep("GET", "rooms/*/state"),
    ep("GET", "rooms/*/state/*"),
    ep("GET", "rooms/*/state/*/*"),
    ep("GET", "rooms/*/context/*"),
    ep("GET", "rooms/*/event/*"),
    ep("GET", "rooms/*/messages"),
    ep("GET", "rooms/*/members"),
    ep("GET", "rooms/*/initialSync"),
    ep("GET", "sync"),
    ep("GET", "events"),
    ep("GET", "media/download/**"),
    ep("GET", "media/thumbnail/**"),
    // The spec: sending events.
    ep("POST", "rooms/*/join"),
    ep("POST", "rooms/*/leave"),
    ep("PUT", "rooms/*/send/*/*"),
    ep("PUT", "rooms/*/state/*"),
    ep("PUT", "rooms/*/state/*/*"),
    ep("PUT", "sendToDevice/*/*"),
    // The spec: account maintenance (only the display name of the profile).
    ep("GET", "profile/**"),
    ep("PUT", "profile/*/displayname"),
    ep("DELETE", "profile/*/displayname"),
    ep("GET", "devices"),
    ep("GET", "devices/*"),
    ep("PUT", "devices/*"),
    // The spec: end-to-end encryption.
    ep("POST", "keys/upload"),
    ep("POST", "keys/query"),
    ep("POST", "keys/claim"),
    // Synapse's `allow_guest=True` servlets beyond the spec's list.
    ep("POST", "join/*"),
    ep("GET", "keys/changes"),
    ep("PUT", "rooms/*/typing/*"),
    ep("POST", "rooms/*/receipt/*/*"),
    ep("POST", "rooms/*/read_markers"),
    ep("GET", "presence/*/status"),
    ep("PUT", "presence/*/status"),
    ep("GET", "voip/turnServer"),
    ep("GET", "capabilities"),
    ep("GET", "joined_rooms"),
    ep("GET", "publicRooms"),
    ep("POST", "publicRooms"),
    ep("GET", "rooms/*/relations/**"),
    ep("GET", "rooms/*/hierarchy"),
];

/// The client-server API path `path` names, after `/_matrix/client/<version>/` (`v3`, `r0`,
/// `v1`, or `unstable` and one namespace segment such as `org.matrix.msc3575`), without a
/// trailing slash. A path outside `/_matrix/client` is returned as it is, minus its leading
/// slash, so a router mounted without the prefix (a crate's own tests) is matched the same way.
#[must_use]
pub fn client_api_relative(path: &str) -> &str {
    let trimmed = path.trim_end_matches('/');
    let Some(rest) = trimmed.strip_prefix("/_matrix/client/") else {
        return trimmed.trim_start_matches('/');
    };
    let Some((version, rest)) = rest.split_once('/') else {
        return "";
    };
    if version == "unstable" {
        // `unstable/<namespace>/...` for MSC routes; a bare `unstable/...` keeps its path.
        return match rest.split_once('/') {
            Some((namespace, tail)) if namespace.contains('.') => tail,
            _ => rest,
        };
    }
    rest
}

fn pattern_matches(pattern: &str, path: &str) -> bool {
    let mut pattern_segments = pattern.split('/');
    let mut path_segments = path.split('/');
    loop {
        match (pattern_segments.next(), path_segments.next()) {
            (Some("**"), Some(_)) => return true,
            (Some("*"), Some(segment)) if !segment.is_empty() => {}
            (Some(expected), Some(segment)) if expected == segment => {}
            (None, None) => return true,
            _ => return false,
        }
    }
}

/// Whether a guest may call `method` on `path` (a full request path, such as
/// `/_matrix/client/v3/rooms/!a:example.org/messages`): whether [`GUEST_ENDPOINTS`] lists it.
#[must_use]
pub fn guest_may_use(method: &Method, path: &str) -> bool {
    let relative = client_api_relative(path);
    GUEST_ENDPOINTS
        .iter()
        .any(|e| e.method == method.as_str() && pattern_matches(e.path, relative))
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, EncodeLabelSet)]
struct OutcomeLabels {
    outcome: &'static str,
}

/// Process-wide, like the other request-path counters: guest registration and the extractor are
/// far from any registry, and a counter is an atomic.
static REGISTRATIONS: LazyLock<Family<OutcomeLabels, Counter>> = LazyLock::new(Family::default);
static REFUSED_REQUESTS: LazyLock<Counter> = LazyLock::new(Counter::default);
static UPGRADES: LazyLock<Counter> = LazyLock::new(Counter::default);

/// Counts one `POST /register?kind=guest`: `created` or `refused` (guest access is off).
pub(crate) fn count_registration(created: bool) {
    let outcome = if created { "created" } else { "refused" };
    REGISTRATIONS
        .get_or_create(&OutcomeLabels { outcome })
        .inc();
}

/// Counts one request a guest made to an endpoint guests may not use.
pub(crate) fn count_refused_request() {
    REFUSED_REQUESTS.inc();
}

/// Counts one guest account made into a full account.
pub(crate) fn count_upgrade() {
    UPGRADES.inc();
}

/// Registers this crate's guest counters into `registry`:
/// `hs_auth_guest_registrations_total{outcome="created"|"refused"}`,
/// `hs_auth_guest_requests_refused_total` (a guest calling an endpoint guests may not use) and
/// `hs_auth_guest_upgrades_total`.
pub fn register_metrics(registry: &mut prometheus_client::registry::Registry) {
    // Registered without `_total`: the text encoder appends it.
    registry.register(
        "hs_auth_guest_registrations",
        "POST /register?kind=guest requests, by outcome: created, or refused because \
         auth.allow_guest_access is off",
        REGISTRATIONS.clone(),
    );
    registry.register(
        "hs_auth_guest_requests_refused",
        "Requests from guest accounts refused 403 M_GUEST_ACCESS_FORBIDDEN because guests may \
         not use the endpoint",
        REFUSED_REQUESTS.clone(),
    );
    registry.register(
        "hs_auth_guest_upgrades",
        "Guest accounts made into full accounts through POST /register with guest_access_token",
        UPGRADES.clone(),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_version_prefix_is_removed_whatever_the_version() {
        assert_eq!(
            client_api_relative("/_matrix/client/v3/rooms/!a:b/messages"),
            "rooms/!a:b/messages"
        );
        assert_eq!(client_api_relative("/_matrix/client/r0/sync"), "sync");
        assert_eq!(
            client_api_relative("/_matrix/client/v1/media/download/a/b"),
            "media/download/a/b"
        );
        assert_eq!(
            client_api_relative("/_matrix/client/unstable/org.matrix.msc3575/sync"),
            "sync"
        );
        assert_eq!(
            client_api_relative("/rooms/!a:b/state/"),
            "rooms/!a:b/state"
        );
    }

    #[test]
    fn a_guest_may_read_join_and_talk_in_rooms() {
        for (method, path) in [
            (Method::GET, "/_matrix/client/v3/sync"),
            (Method::GET, "/_matrix/client/v3/rooms/!a:b/messages"),
            (Method::GET, "/_matrix/client/v3/rooms/!a:b/state"),
            (
                Method::GET,
                "/_matrix/client/v3/rooms/!a:b/state/m.room.member/@g:b",
            ),
            (Method::GET, "/_matrix/client/r0/rooms/!a:b/initialSync"),
            (Method::GET, "/_matrix/client/r0/events"),
            (Method::POST, "/_matrix/client/v3/join/!a:b"),
            (Method::POST, "/_matrix/client/v3/join/%23room:b"),
            (Method::POST, "/_matrix/client/v3/rooms/!a:b/leave"),
            (
                Method::PUT,
                "/_matrix/client/v3/rooms/!a:b/send/m.room.message/t1",
            ),
            (
                Method::PUT,
                "/_matrix/client/v3/rooms/!a:b/state/m.room.topic/",
            ),
            (Method::PUT, "/_matrix/client/v3/rooms/!a:b/typing/@g:b"),
            (
                Method::POST,
                "/_matrix/client/v3/rooms/!a:b/receipt/m.read/$e",
            ),
            (Method::PUT, "/_matrix/client/v3/profile/@g:b/displayname"),
            (Method::GET, "/_matrix/client/v3/profile/@g:b/displayname"),
            (Method::GET, "/_matrix/client/v1/media/download/b/abc"),
            (Method::POST, "/_matrix/client/v3/keys/upload"),
            (Method::GET, "/_matrix/client/v3/voip/turnServer"),
            (Method::PUT, "/_matrix/client/v3/presence/@g:b/status"),
        ] {
            assert!(guest_may_use(&method, path), "{method} {path}");
        }
    }

    #[test]
    fn a_guest_may_not_create_invite_upload_or_keep_account_data() {
        for (method, path) in [
            (Method::POST, "/_matrix/client/v3/createRoom"),
            (Method::POST, "/_matrix/client/v3/rooms/!a:b/invite"),
            (Method::POST, "/_matrix/client/v3/rooms/!a:b/kick"),
            (Method::POST, "/_matrix/client/v1/media/upload"),
            (Method::POST, "/_matrix/media/v3/upload"),
            (
                Method::PUT,
                "/_matrix/client/v3/user/@g:b/account_data/m.direct",
            ),
            (Method::PUT, "/_matrix/client/v3/profile/@g:b/avatar_url"),
            (Method::POST, "/_matrix/client/v3/account/password"),
            (Method::GET, "/_matrix/client/v3/rooms/!a:b/joined_members"),
            (Method::DELETE, "/_matrix/client/v3/devices/D"),
            (Method::POST, "/_matrix/client/v3/user/@g:b/filter"),
            // A method the table does not list for a path it does.
            (Method::DELETE, "/_matrix/client/v3/sync"),
            // A path one segment longer than the table's.
            (Method::GET, "/_matrix/client/v3/rooms/!a:b/messages/extra"),
        ] {
            assert!(!guest_may_use(&method, path), "{method} {path}");
        }
    }
}
