//! Who has signed in to a bridge, and as what: `GET /appservices/{id}/logins`.
//!
//! The bridges keep that state themselves. A mautrix `bridgev2` bridge exposes it through its
//! provisioning API, `GET /_matrix/provision/v3/whoami?user_id=...` on its appservice listener,
//! authenticated by the `provisioning.shared_secret` in its `config.yaml`; a render from the
//! catalogue mints that secret and keeps it in the registration
//! ([`crate::bridge_types::PROVISIONING_SECRET_KEY`]). Other bridge types have no API that
//! reports sign-ins ([`crate::bridge_types::ProvisioningApi`]), and the answer says so.
//!
//! This module is the part that needs no network: deciding from a registration whether and how
//! to ask ([`plan`]), and turning the bridge's answer, or its failure, into the admin API's
//! shape ([`answered`], [`failed`], [`refused`]). The asking itself, with its 30-second cache
//! and its counter, is `hs-appservice`'s (`hs_appservice::provisioning`), behind
//! [`crate::sources::AppserviceDirectory::logins`].

use std::time::Duration;

use serde_json::Value;

use crate::bridge_offerings::SHARED_INSTANCE;
use crate::bridge_types::{self, BRIDGE_INSTANCE_KEY, BRIDGE_TYPE_KEY, PROVISIONING_SECRET_KEY};
use crate::model::{AdminBridgeLogin, AdminBridgeLogins, AdminBridgeLoginsError};
use crate::sources::SourceError;

/// How long an answer is kept per (bridge, user) before the bridge is asked again.
pub const CACHE_TTL: Duration = Duration::from_secs(30);

/// The mautrix `bridgev2` provisioning endpoint that lists a user's logins, under the
/// registration's `url`.
pub const WHOAMI_PATH: &str = "/_matrix/provision/v3/whoami";

/// What to ask a bridge: everything [`plan`] found in its registration.
#[derive(Clone, PartialEq)]
pub struct WhoamiRequest {
    pub appservice_id: String,
    /// The catalogue entry, which every [`LoginsPlan::Ask`] has.
    pub bridge_type: String,
    /// The Matrix user asked about.
    pub user_id: String,
    /// The full whoami URL, without the query.
    pub url: String,
    /// The provisioning API's shared secret, sent as a bearer token.
    pub secret: String,
}

impl std::fmt::Debug for WhoamiRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WhoamiRequest")
            .field("appservice_id", &self.appservice_id)
            .field("bridge_type", &self.bridge_type)
            .field("user_id", &self.user_id)
            .field("url", &self.url)
            .finish_non_exhaustive()
    }
}

/// What [`plan`] decided.
#[derive(Debug, Clone, PartialEq)]
pub enum LoginsPlan {
    /// The bridge cannot be asked: the answer, with `supported: false` and the reason.
    Unsupported(AdminBridgeLogins),
    /// Ask it.
    Ask(WhoamiRequest),
}

/// Decides from a registration (its JSON form, unrecognised keys included) whether the bridge
/// `appservice_id` can be asked who has signed in, and if so, how.
///
/// `user_id` is the user asked about; absent, a per-user instance's owner (RFC 0017) is asked
/// about. A type that reports no sign-ins answers without one.
///
/// # Errors
/// [`SourceError::InvalidField`] for a `user_id` that is not a Matrix user ID, or for none at
/// all on a bridge that serves many users.
pub fn plan(
    appservice_id: &str,
    registration: &Value,
    user_id: Option<&str>,
) -> Result<LoginsPlan, SourceError> {
    let user_id = match user_id.map(str::trim).filter(|u| !u.is_empty()) {
        Some(user) => {
            if !(user.starts_with('@') && user.len() > 3 && user[1..].contains(':')) {
                return Err(SourceError::InvalidField {
                    pointer: "/user_id",
                    detail: format!("not a Matrix user ID: {user}"),
                });
            }
            Some(user.to_owned())
        }
        None => registration
            .get(BRIDGE_INSTANCE_KEY)
            .and_then(Value::as_str)
            .filter(|owner| *owner != SHARED_INSTANCE && owner.starts_with('@'))
            .map(str::to_owned),
    };
    let bridge_type = registration
        .get(BRIDGE_TYPE_KEY)
        .and_then(Value::as_str)
        .map(str::to_owned);
    let unsupported = |api: &str, reason: String| {
        Ok(LoginsPlan::Unsupported(AdminBridgeLogins {
            appservice_id: appservice_id.to_owned(),
            bridge_type: bridge_type.clone(),
            provisioning_api: api.to_owned(),
            supported: false,
            reason: Some(reason),
            user_id: user_id.clone(),
            ..AdminBridgeLogins::default()
        }))
    };
    let Some(type_id) = bridge_type.as_deref() else {
        return unsupported(
            "none",
            "This appservice was not added from the bridge catalogue, so the server does not know \
             whether it has a provisioning API; the bridge keeps who has signed in itself."
                .to_owned(),
        );
    };
    let Some((api, note)) = bridge_types::provisioning(type_id) else {
        return unsupported(
            "none",
            format!(
                "{type_id} is not in the bridge catalogue, so the server does not know its \
                 provisioning API; the bridge keeps who has signed in itself."
            ),
        );
    };
    if !api.reports_logins() {
        return unsupported(api.as_str(), note.to_owned());
    }
    let Some(url) = registration
        .get("url")
        .and_then(Value::as_str)
        .map(|u| u.trim().trim_end_matches('/'))
        .filter(|u| !u.is_empty())
    else {
        return unsupported(
            api.as_str(),
            "The registration has no url, so the server cannot reach the bridge to ask it."
                .to_owned(),
        );
    };
    let Some(secret) = registration
        .get(PROVISIONING_SECRET_KEY)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
    else {
        return unsupported(
            api.as_str(),
            format!(
                "The registration carries no provisioning secret: it was made before the server \
                 kept one. Put the bridge's provisioning.shared_secret (from its config.yaml) in \
                 the registration's {PROVISIONING_SECRET_KEY} key with a merge patch, and the \
                 server can ask it."
            ),
        );
    };
    let Some(user_id) = user_id else {
        return Err(SourceError::InvalidField {
            pointer: "/user_id",
            detail: "user_id is required: this bridge is shared, so name the Matrix user to ask \
                     about"
                .to_owned(),
        });
    };
    Ok(LoginsPlan::Ask(WhoamiRequest {
        appservice_id: appservice_id.to_owned(),
        bridge_type: type_id.to_owned(),
        user_id,
        url: format!("{url}{WHOAMI_PATH}"),
        secret: secret.to_owned(),
    }))
}

fn base(request: &WhoamiRequest) -> AdminBridgeLogins {
    AdminBridgeLogins {
        appservice_id: request.appservice_id.clone(),
        bridge_type: Some(request.bridge_type.clone()),
        provisioning_api: bridge_types::ProvisioningApi::MautrixV3.as_str().to_owned(),
        supported: true,
        user_id: Some(request.user_id.clone()),
        ..AdminBridgeLogins::default()
    }
}

fn non_empty(value: &Value) -> Option<String> {
    value
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

/// A bridge timestamp as RFC 3339. mautrix sends seconds (`jsontime.Unix`); a value too large
/// to be seconds is taken as milliseconds.
fn timestamp(value: &Value) -> Option<String> {
    let n = value
        .as_i64()
        .or_else(|| value.as_f64().map(|f| f as i64))?;
    if n <= 0 {
        return None;
    }
    let ms = if n > 100_000_000_000 {
        n
    } else {
        n.saturating_mul(1000)
    };
    Some(hs_http::time::rfc3339_from_millis(ms))
}

/// The bridge's `whoami` answer in the admin API's shape, checked at `now_ms`.
///
/// # Errors
/// An `invalid_answer` error for a body that is not a whoami answer: not an object, or
/// `logins` neither a list nor null.
pub fn answered(
    request: &WhoamiRequest,
    body: &Value,
    now_ms: i64,
) -> Result<AdminBridgeLogins, AdminBridgeLoginsError> {
    let invalid = |what: &str| AdminBridgeLoginsError {
        status: 502,
        reason: "invalid_answer".to_owned(),
        detail: format!(
            "the bridge's {WHOAMI_PATH} answered something that is not a bridgev2 whoami: {what}"
        ),
    };
    let Some(object) = body.as_object() else {
        return Err(invalid("not a JSON object"));
    };
    let entries = match object.get("logins") {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Array(entries)) => entries.clone(),
        Some(_) => return Err(invalid("logins is not a list")),
    };
    let logins: Vec<AdminBridgeLogin> = entries
        .iter()
        .filter_map(|login| {
            let remote_id = non_empty(&login["id"])?;
            let profile = &login["profile"];
            let remote_name = non_empty(&login["name"])
                .or_else(|| non_empty(&profile["name"]))
                .or_else(|| non_empty(&profile["phone"]))
                .or_else(|| non_empty(&profile["username"]))
                .or_else(|| non_empty(&profile["email"]));
            Some(AdminBridgeLogin {
                user_id: request.user_id.clone(),
                remote_id,
                remote_name,
                state: non_empty(&login["state_event"])
                    .map_or_else(|| "unknown".to_owned(), |s| s.to_lowercase()),
                state_reason: non_empty(&login["state_reason"]),
                since: timestamp(&login["state_ts"]),
            })
        })
        .collect();
    Ok(AdminBridgeLogins {
        signed_in: Some(!logins.is_empty()),
        logins,
        checked_at: Some(hs_http::time::rfc3339_from_millis(now_ms)),
        ..base(request)
    })
}

/// The answer for a bridge that could not be asked: `supported`, with `error` set.
#[must_use]
pub fn failed(request: &WhoamiRequest, error: AdminBridgeLoginsError) -> AdminBridgeLogins {
    AdminBridgeLogins {
        error: Some(error),
        ..base(request)
    }
}

/// The error for a bridge that answered `status` with `body`: its own `errcode` and `error`
/// when it gave them, and what the status most likely means.
#[must_use]
pub fn refused(status: u16, body: &str) -> AdminBridgeLoginsError {
    let parsed: Value = serde_json::from_str(body).unwrap_or(Value::Null);
    let errcode = non_empty(&parsed["errcode"]);
    let message = non_empty(&parsed["error"]);
    let said = match (errcode, message) {
        (Some(code), Some(message)) => format!("{code}: {message}"),
        (Some(code), None) => code,
        (None, Some(message)) => message,
        (None, None) => {
            let text: String = body.trim().chars().take(200).collect();
            if text.is_empty() {
                "no body".to_owned()
            } else {
                text
            }
        }
    };
    let meaning = match status {
        401 => "the bridge did not accept the provisioning secret",
        403 => "the bridge refused: the secret, or the user's permission to sign in",
        404 => "the bridge does not serve the bridgev2 provisioning API at this path",
        _ => "the bridge answered with an error",
    };
    AdminBridgeLoginsError {
        status,
        reason: "refused".to_owned(),
        detail: format!("{meaning} ({status}, {said})"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn whatsapp(extra: Value) -> Value {
        let mut registration = json!({
            "id": "whatsapp",
            "url": "http://127.0.0.1:29318/",
            "as_token": "a",
            "hs_token": "h",
            "sender_localpart": "whatsappbot",
            BRIDGE_TYPE_KEY: "mautrix-whatsapp",
            PROVISIONING_SECRET_KEY: "s3cret",
        });
        for (k, v) in extra.as_object().unwrap() {
            registration[k] = v.clone();
        }
        registration
    }

    fn ask(plan: LoginsPlan) -> WhoamiRequest {
        match plan {
            LoginsPlan::Ask(request) => request,
            LoginsPlan::Unsupported(answer) => panic!("expected to ask, got {answer:?}"),
        }
    }

    fn unsupported(plan: LoginsPlan) -> AdminBridgeLogins {
        match plan {
            LoginsPlan::Unsupported(answer) => answer,
            LoginsPlan::Ask(request) => panic!("expected no ask, got {request:?}"),
        }
    }

    #[test]
    fn a_mautrix_bridge_with_a_secret_is_asked_at_its_whoami() {
        let request = ask(plan("whatsapp", &whatsapp(json!({})), Some("@alice:x.org")).unwrap());
        assert_eq!(
            request.url,
            "http://127.0.0.1:29318/_matrix/provision/v3/whoami"
        );
        assert_eq!(request.secret, "s3cret");
        assert_eq!(request.user_id, "@alice:x.org");
        assert!(!format!("{request:?}").contains("s3cret"), "Debug hides it");
    }

    #[test]
    fn an_instance_is_asked_about_its_owner_and_a_shared_bridge_needs_a_user() {
        let instance = whatsapp(json!({BRIDGE_INSTANCE_KEY: "@alice:x.org"}));
        assert_eq!(
            ask(plan("whatsapp-alice", &instance, None).unwrap()).user_id,
            "@alice:x.org"
        );
        let shared = whatsapp(json!({BRIDGE_INSTANCE_KEY: SHARED_INSTANCE}));
        assert!(matches!(
            plan("whatsapp", &shared, None),
            Err(SourceError::InvalidField {
                pointer: "/user_id",
                ..
            })
        ));
        assert!(matches!(
            plan("whatsapp", &whatsapp(json!({})), Some("alice")),
            Err(SourceError::InvalidField {
                pointer: "/user_id",
                ..
            })
        ));
    }

    #[test]
    fn a_type_without_an_api_or_a_registration_without_a_secret_says_why() {
        let heisen = unsupported(
            plan(
                "irc",
                &json!({"id": "irc", "url": "http://b:9898", BRIDGE_TYPE_KEY: "heisenbridge"}),
                None,
            )
            .unwrap(),
        );
        assert!(!heisen.supported);
        assert_eq!(heisen.provisioning_api, "none");
        assert!(heisen.reason.unwrap().contains("control room"));

        let irc = unsupported(
            plan(
                "irc",
                &json!({"id": "irc", "url": "http://b:9999", BRIDGE_TYPE_KEY: "matrix-appservice-irc"}),
                Some("@a:x.org"),
            )
            .unwrap(),
        );
        assert_eq!(irc.provisioning_api, "irc_v1");
        assert_eq!(irc.user_id.as_deref(), Some("@a:x.org"));

        let custom = unsupported(plan("x", &json!({"id": "x", "url": "http://b"}), None).unwrap());
        assert_eq!(custom.provisioning_api, "none");
        assert!(custom.reason.unwrap().contains("catalogue"));

        let mut old = whatsapp(json!({}));
        old.as_object_mut().unwrap().remove(PROVISIONING_SECRET_KEY);
        let old = unsupported(plan("whatsapp", &old, Some("@a:x.org")).unwrap());
        assert_eq!(old.provisioning_api, "mautrix_v3");
        assert!(old.reason.unwrap().contains(PROVISIONING_SECRET_KEY));

        let no_url =
            unsupported(plan("w", &whatsapp(json!({"url": null})), Some("@a:x.org")).unwrap());
        assert!(no_url.reason.unwrap().contains("no url"));
    }

    #[test]
    fn a_whoami_answer_is_normalised() {
        let request = ask(plan("whatsapp", &whatsapp(json!({})), Some("@alice:x.org")).unwrap());
        // The shape mautrix-go's bridgev2 provisioning API answers (`RespWhoami`).
        let body = json!({
            "network": {"displayname": "WhatsApp", "network_id": "whatsapp"},
            "login_flows": [{"id": "qr"}],
            "homeserver": "x.org",
            "bridge_bot": "@whatsappbot:x.org",
            "command_prefix": "!wa",
            "management_room": "!dm:x.org",
            "logins": [
                {
                    "id": "15551234567",
                    "name": "+1 555-123-4567",
                    "state_event": "CONNECTED",
                    "state_ts": 1_727_000_000,
                    "profile": {"phone": "+15551234567"},
                    "space_room": "!space:x.org"
                },
                {
                    "id": "15550000000",
                    "name": "",
                    "state_event": "BAD_CREDENTIALS",
                    "state_reason": "wa-logged-out",
                    "state_ts": 1_727_000_000_123i64,
                    "profile": {"phone": "+15550000000"}
                },
                {"name": "no id, skipped"}
            ]
        });
        let answer = answered(&request, &body, 1_727_000_100_000).unwrap();
        assert!(answer.supported);
        assert_eq!(answer.signed_in, Some(true));
        assert_eq!(answer.logins.len(), 2);
        let first = &answer.logins[0];
        assert_eq!(first.user_id, "@alice:x.org");
        assert_eq!(first.remote_id, "15551234567");
        assert_eq!(first.remote_name.as_deref(), Some("+1 555-123-4567"));
        assert_eq!(first.state, "connected");
        assert_eq!(first.since.as_deref(), Some("2024-09-22T10:13:20.000Z"));
        let second = &answer.logins[1];
        assert_eq!(second.remote_name.as_deref(), Some("+15550000000"));
        assert_eq!(second.state, "bad_credentials");
        assert_eq!(second.state_reason.as_deref(), Some("wa-logged-out"));
        assert_eq!(second.since.as_deref(), Some("2024-09-22T10:13:20.123Z"));
        assert_eq!(
            answer.checked_at.as_deref(),
            Some("2024-09-22T10:15:00.000Z")
        );

        // Not signed in: no logins, or a null list (a nil slice in Go).
        for empty in [json!({"logins": []}), json!({"logins": null}), json!({})] {
            let answer = answered(&request, &empty, 0).unwrap();
            assert_eq!(answer.signed_in, Some(false), "{empty}");
            assert!(answer.logins.is_empty());
        }
        let nonsense = answered(&request, &json!({"logins": "yes"}), 0).unwrap_err();
        assert_eq!(
            (nonsense.status, nonsense.reason.as_str()),
            (502, "invalid_answer")
        );
        assert!(answered(&request, &json!([1]), 0).is_err());
    }

    #[test]
    fn a_refusal_carries_the_bridges_own_words() {
        let e = refused(
            401,
            r#"{"errcode":"M_UNKNOWN_TOKEN","error":"Invalid auth token"}"#,
        );
        assert_eq!((e.status, e.reason.as_str()), (401, "refused"));
        assert!(e.detail.contains("M_UNKNOWN_TOKEN: Invalid auth token"));
        assert!(e.detail.contains("secret"));
        assert!(
            refused(404, "404 page not found")
                .detail
                .contains("bridgev2")
        );
        assert!(refused(500, "").detail.contains("no body"));
    }
}
