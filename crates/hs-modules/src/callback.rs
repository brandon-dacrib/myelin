//! The versioned JSON HTTP-callback module protocol: how an out-of-process module (any language,
//! not just Rust) implements [`crate::hooks::ModuleHooks`] over HTTP. This is Synapse's module
//! system's replacement for in-process Python: instead of loading a `.py` file into the server,
//! an operator points a hook at a URL, and `crate::client::HttpCallbackClient` calls it.
//!
//! ## Wire format
//!
//! `POST <base_url>/<hook>` with:
//!
//! ```json
//! {"protocol_version": "1", "payload": { ...hook-specific... }}
//! ```
//!
//! and the module responds `200 OK` with:
//!
//! ```json
//! {"protocol_version": "1", "result": { ...hook-specific... }}
//! ```
//!
//! `<hook>` is the method name in `snake_case` (`check_event_for_spam`,
//! `user_may_invite`, ...), matching [`crate::hooks::ModuleHooks`]'s method names exactly so the
//! mapping between the trait and the wire protocol needs no separate table.
//!
//! A module that does not implement a given hook returns `404`; the client treats that the same
//! as "no opinion" (`None`/`Allow`, per the hook's own default), not an error, so an operator can
//! run a module that only implements `check_event_for_spam` without also standing up every other
//! endpoint.
//!
//! ## Versioning
//!
//! `protocol_version` is a string so it can grow non-numerically if needed (`"1"`, `"1.1"`, ...
//! is not promised; only forward compatibility within major version `"1"` is). The client sends
//! the version it speaks; a module that cannot handle it answers `409` with
//! `{"error": "unsupported_protocol_version", "supported": ["1"]}`. Breaking wire changes bump
//! the version and are proposed as an RFC (this crate's brief, "Day-one work").

pub const PROTOCOL_VERSION: &str = "1";

use serde::{Deserialize, Serialize};

/// The envelope every callback request carries, generic over the hook-specific payload.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CallbackRequest<T> {
    pub protocol_version: String,
    pub payload: T,
}

impl<T> CallbackRequest<T> {
    pub fn new(payload: T) -> Self {
        Self {
            protocol_version: PROTOCOL_VERSION.to_string(),
            payload,
        }
    }
}

/// The envelope every callback response carries.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CallbackResponse<T> {
    pub protocol_version: String,
    pub result: T,
}

impl<T> CallbackResponse<T> {
    pub fn new(result: T) -> Self {
        Self {
            protocol_version: PROTOCOL_VERSION.to_string(),
            result,
        }
    }
}

/// The body of a `409` "I don't speak that version" response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UnsupportedVersion {
    pub error: String,
    pub supported: Vec<String>,
}

impl UnsupportedVersion {
    pub fn current() -> Self {
        Self {
            error: "unsupported_protocol_version".to_string(),
            supported: vec![PROTOCOL_VERSION.to_string()],
        }
    }
}

/// The path segment for one hook, matching [`crate::hooks::ModuleHooks`]'s method names.
/// Kept as plain `&'static str` constants (not an enum) so a module implementer can `match` on
/// the URL path directly without depending on this crate at all — the protocol is meant to be
/// implementable in any language.
pub mod hook_names {
    pub const CHECK_EVENT_FOR_SPAM: &str = "check_event_for_spam";
    pub const USER_MAY_INVITE: &str = "user_may_invite";
    pub const CHECK_USERNAME_FOR_SPAM: &str = "check_username_for_spam";
    pub const CHECK_EVENT_ALLOWED: &str = "check_event_allowed";
    pub const GET_INTERESTED_USERS: &str = "get_interested_users";
    pub const IS_USER_EXPIRED: &str = "is_user_expired";
    pub const CHECK_PASSWORD: &str = "check_password";
    pub const BACKGROUND_UPDATE_GUIDANCE: &str = "background_update_guidance";
    pub const ON_ACCOUNT_DATA_UPDATED: &str = "on_account_data_updated";
    pub const CHECK_MEDIA_FOR_SPAM: &str = "check_media_for_spam";
    pub const RATELIMIT_OVERRIDE: &str = "ratelimit_override";
    pub const SHOULD_FEDERATE_ROOM: &str = "should_federate_room";
    pub const EXTRA_UNSIGNED_FIELDS: &str = "extra_unsigned_fields";
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_envelope_round_trips() {
        let req = CallbackRequest::new(serde_json::json!({"a": 1}));
        let json = serde_json::to_string(&req).unwrap();
        let back: CallbackRequest<serde_json::Value> = serde_json::from_str(&json).unwrap();
        assert_eq!(back.protocol_version, PROTOCOL_VERSION);
        assert_eq!(back.payload, serde_json::json!({"a": 1}));
    }

    #[test]
    fn unsupported_version_lists_current() {
        let uv = UnsupportedVersion::current();
        assert_eq!(uv.supported, vec![PROTOCOL_VERSION.to_string()]);
    }
}
