//! `GET`/`POST /login`: legacy password, short-term-token and application-service login.
//!
//! Wire shapes for the login mechanism (`LoginInfo`, `UserIdentifier` and their variants) are
//! `ruma::api::client::{session::login::v3, uiaa}` types reused as plain `serde` types (see the
//! module doc on [`crate::uia`] for why that is safe to do without pulling in ruma's endpoint
//! machinery). Everything else about the request (`device_id`, `initial_device_display_name`,
//! `refresh_token`) is read directly off the parsed JSON body.

use std::collections::HashMap;

use axum::extract::{Query, State};
use axum::http::HeaderMap;
use axum::http::header::AUTHORIZATION;
use axum::response::{IntoResponse, Response};
use axum::{Json, http::StatusCode};
use ruma::api::client::session::login::v3::LoginInfo;
use ruma::api::client::uiaa::UserIdentifier;
use ruma::{OwnedDeviceId, OwnedUserId, UserId};
use serde_json::{Value, json};

use crate::appservice::AppserviceRecord;
use crate::error::{ErrCode, MatrixError};
use crate::password;
use crate::session::{self, NewSession};
use crate::shared_secret_auth;
use crate::state::AuthState;
use crate::token::TokenHash;
use hs_http::body::PermissiveJson;

/// Synapse's exact wording for "no such user" and "wrong password" alike, deliberately identical
/// so a failed login never tells an attacker which half was wrong.
const INVALID_USERNAME_OR_PASSWORD: &str = "Invalid username or password";

/// The spec's login type for an appservice logging in as one of its users, with its own token.
const APPSERVICE_LOGIN_TYPE: &str = "m.login.application_service";

/// The wire login type for `crate::shared_secret_auth` (legacy mautrix bridge double puppeting).
const SHARED_SECRET_AUTH_LOGIN_TYPE: &str = "com.devture.shared_secret_auth";

/// `GET /login`: the login flows this server offers. `m.login.application_service` is always
/// listed, as Synapse lists it (`LoginRestServlet.on_GET` appends `APPSERVICE_TYPE`):
/// mautrix-go's older bridge framework looks for it before it logs its bot's device in for
/// end-to-bridge encryption, and refuses to start without it ("homeserver does not support
/// appservice login", mautrix-imessage on 2026-10-12). Only an appservice's token can use it.
/// `com.devture.shared_secret_auth` is advertised only when a secret is configured for it
/// (`crate::shared_secret_auth`), matching how mautrix bridges probe for it
/// (`refs/mautrix-python/mautrix/bridge/custom_puppet.py`'s
/// `flows.get_first_of_type(LoginType.DEVTURE_SHARED_SECRET, LoginType.PASSWORD)`, read for
/// behavior only): a bridge falls back to `m.login.password` when the flow isn't offered, so a
/// server that never enabled the feature should not pretend to.
pub async fn get_login_types(State(state): State<AuthState>) -> Json<Value> {
    let mut flows = Vec::new();
    // `auth.password.enabled` (Synapse's `password_config.enabled`): not offered when off.
    if state.config.get().password_login_enabled {
        flows.push(json!({"type": "m.login.password"}));
    }
    flows.push(json!({"type": "m.login.token"}));
    if state.config.get().shared_secret_auth_secret.is_some() {
        flows.push(json!({"type": SHARED_SECRET_AUTH_LOGIN_TYPE}));
    }
    // Single sign-on (`crate::cas`): the spec's `m.login.sso` with its one provider, and the
    // older `m.login.cas` that clients from before `m.login.sso` (and Sytest) look for.
    if let Some(cas) = &state.config.get().cas {
        flows.push(json!({
            "type": "m.login.sso",
            "identity_providers": [{"id": crate::cas::PROVIDER, "name": cas.idp_name}],
        }));
        flows.push(json!({"type": "m.login.cas"}));
    }
    flows.push(json!({"type": APPSERVICE_LOGIN_TYPE}));
    Json(json!({ "flows": flows }))
}

/// `POST /login`.
pub async fn post_login(
    State(state): State<AuthState>,
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
    client: hs_http::buckets::ClientIp,
    PermissiveJson(body): PermissiveJson<Value>,
) -> Result<Response, MatrixError> {
    let device_id: Option<OwnedDeviceId> = body
        .get("device_id")
        .and_then(Value::as_str)
        .map(Into::into);
    let initial_device_display_name = body
        .get("initial_device_display_name")
        .and_then(Value::as_str)
        .map(String::from);
    let refresh = body
        .get("refresh_token")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    // The pre-identifier way of naming an account by its email address or phone number:
    // top-level `medium` and `address`, which Synapse still reads (Sytest's "Can login with 3pid
    // and password using m.login.password" sends it).
    let legacy_threepid = match (
        body.get("medium").and_then(Value::as_str),
        body.get("address").and_then(Value::as_str),
    ) {
        (Some(medium), Some(address)) => Some((medium.to_owned(), address.to_owned())),
        _ => None,
    };
    let login_info: LoginInfo = serde_json::from_value(body).map_err(|_| {
        MatrixError::new(
            StatusCode::BAD_REQUEST,
            ErrCode::BadJson,
            "Invalid or missing login type",
        )
    })?;

    // `rate_limits.login`, per client address, as Synapse's `rc_login.address`: every attempt
    // takes from the bucket, except an appservice logging in one of its own users.
    if !matches!(login_info, LoginInfo::ApplicationService(_))
        && let Some(address) = client.key()
    {
        state
            .limits
            .login
            .take_now(&address)
            .map_err(MatrixError::limit_exceeded)?;
    }

    // The appservice an `m.login.application_service` login issues the session to (decision
    // 0053); `None` for every other login type.
    let mut issued_by_appservice = None;
    let user_id = if login_info.login_type() == SHARED_SECRET_AUTH_LOGIN_TYPE {
        resolve_shared_secret_auth_login(&state, &login_info.data()).await?
    } else {
        match login_info {
            LoginInfo::Password(p) => {
                if !state.config.get().password_login_enabled {
                    tracing::info!(
                        "refused a password login: auth.password.enabled is off on this server"
                    );
                    return Err(MatrixError::new(
                        StatusCode::FORBIDDEN,
                        ErrCode::Forbidden,
                        "Password login has been disabled on this server",
                    ));
                }
                resolve_password_login(&state, &p, legacy_threepid.as_ref()).await?
            }
            LoginInfo::Token(t) => resolve_token_login(&state, &t.token).await?,
            LoginInfo::ApplicationService(as_info) => {
                let (user_id, appservice_id) =
                    resolve_appservice_login(&state, &headers, &query, as_info.identifier.as_ref())
                        .await?;
                issued_by_appservice = Some(appservice_id);
                user_id
            }
            _ => {
                return Err(MatrixError::new(
                    StatusCode::BAD_REQUEST,
                    ErrCode::Unrecognized,
                    "Unsupported login type",
                ));
            }
        }
    };

    let user = state
        .store
        .get_user(&user_id)
        .await?
        .ok_or_else(|| MatrixError::forbidden(INVALID_USERNAME_OR_PASSWORD))?;
    if user.deactivated {
        return Err(MatrixError::forbidden(INVALID_USERNAME_OR_PASSWORD));
    }
    if user.locked {
        return Err(MatrixError::user_locked());
    }

    let session = session::create_session_issued_by(
        &state,
        &user_id,
        device_id,
        initial_device_display_name,
        refresh,
        issued_by_appservice,
    )
    .await?;

    Ok(login_response(&user_id, session).into_response())
}

/// ASCII-lowercases just the localpart portion of a user identifier a client typed for login,
/// leaving any `@`/`:server` structure and the server name's own case untouched.
///
/// Mirrors `routes::register::register_user`'s username lowercasing (see that function's doc
/// comment for the spec citation, `appendices.md`'s "User Identifiers": homeservers are expected
/// to downcase usernames so no two accounts differ only by case) on the *login* side too, not just
/// registration. Without this, a login attempt fails with the generic "Invalid username or
/// password" whenever a user (or a browser's `autocapitalize` on a mobile keyboard, or simply a
/// user who typed their name the way they'd write it) sends the localpart in a different case than
/// the lower-cased form it was actually registered under -- exactly the class of "looks fine,
/// subtly wrong" gap this endpoint is otherwise fully tested against. Confirmed this is real
/// Synapse behavior, not just a registration-time nicety: `refs/synapse/synapse/handlers/
/// auth.py::AuthHandler._find_user_id_and_pwd_hash` explicitly calls
/// `get_users_by_id_case_insensitive` and accepts the sole case-insensitive match. This ASCII-only
/// version needs none of that function's ambiguity resolution (multiple case-variant matches,
/// preferring an exact one) because this crate already forbids case-variant duplicates at
/// registration time (`register.rs::register_user` lower-cases before its availability check) --
/// there is at most one account for any given case-insensitive localpart, so lower-casing the
/// login attempt can only ever land on that one account or none.
fn lowercase_localpart_for_login(raw: &str) -> String {
    match raw.strip_prefix('@') {
        Some(rest) => match rest.split_once(':') {
            Some((localpart, server)) => format!("@{}:{server}", localpart.to_ascii_lowercase()),
            None => format!("@{}", rest.to_ascii_lowercase()),
        },
        None => raw.to_ascii_lowercase(),
    }
}

async fn resolve_password_login(
    state: &AuthState,
    p: &ruma::api::client::session::login::v3::Password,
    legacy_threepid: Option<&(String, String)>,
) -> Result<OwnedUserId, MatrixError> {
    #[allow(deprecated)]
    let user_id = if let Some(identifier) = &p.identifier {
        identifier_to_user_id(state, identifier).await?
    } else if let Some((medium, address)) = legacy_threepid {
        let address = if medium == "email" {
            address.trim().to_lowercase()
        } else {
            address.clone()
        };
        threepid_user_id(state, medium, &address).await?
    } else if let Some(user) = &p.user {
        UserId::parse_with_server_name(
            lowercase_localpart_for_login(user.as_str()),
            state.server_name(),
        )
        .map_err(|_| MatrixError::forbidden(INVALID_USERNAME_OR_PASSWORD))?
    } else {
        return Err(MatrixError::missing_param("Missing user identifier"));
    };

    let record = state.store.get_user(&user_id).await?;
    let Some(record) = record else {
        return Err(MatrixError::forbidden(INVALID_USERNAME_OR_PASSWORD));
    };
    let Some(hash) = &record.password_hash else {
        return Err(MatrixError::forbidden(INVALID_USERNAME_OR_PASSWORD));
    };
    let ok = password::verify_password(&p.password, hash, &state.config.get().bcrypt_pepper)
        .map_err(|_| MatrixError::forbidden(INVALID_USERNAME_OR_PASSWORD))?;
    if !ok {
        return Err(MatrixError::forbidden(INVALID_USERNAME_OR_PASSWORD));
    }
    Ok(user_id)
}

/// Resolves a `UserIdentifier` to a full user ID. `m.id.user` parses/validates directly;
/// `m.id.thirdparty` (email) and `m.id.phone` (MSISDN, or a country+number pair) go through
/// [`crate::store::UserStore::get_user_by_threepid`]. The phone-number variant's `country` +
/// `phone` are concatenated digits-only rather than canonicalized through a full E.164
/// country-calling-code table — a day-one simplification noted in
/// `docs/rfcs/0002-auth-tokens-and-requester.md` section 8; clients that already send a bare
/// MSISDN as `phone` with `country` empty are unaffected.
pub(crate) async fn identifier_to_user_id(
    state: &AuthState,
    identifier: &UserIdentifier,
) -> Result<OwnedUserId, MatrixError> {
    match identifier {
        UserIdentifier::Matrix(m) => UserId::parse_with_server_name(
            lowercase_localpart_for_login(m.user.as_str()),
            state.server_name(),
        )
        .map_err(|_| MatrixError::forbidden(INVALID_USERNAME_OR_PASSWORD)),
        UserIdentifier::Email(e) => threepid_user_id(state, "email", &e.address).await,
        UserIdentifier::Msisdn(m) => threepid_user_id(state, "msisdn", &m.number).await,
        UserIdentifier::PhoneNumber(p) => {
            let digits: String = p
                .country
                .chars()
                .chain(p.phone.chars())
                .filter(char::is_ascii_digit)
                .collect();
            threepid_user_id(state, "msisdn", &digits).await
        }
        _ => Err(MatrixError::forbidden(INVALID_USERNAME_OR_PASSWORD)),
    }
}

async fn threepid_user_id(
    state: &AuthState,
    medium: &str,
    address: &str,
) -> Result<OwnedUserId, MatrixError> {
    state
        .store
        .get_user_by_threepid(medium, address)
        .await?
        .ok_or_else(|| MatrixError::forbidden(INVALID_USERNAME_OR_PASSWORD))
}

async fn resolve_token_login(state: &AuthState, token: &str) -> Result<OwnedUserId, MatrixError> {
    let hash = TokenHash::of(token);
    let record = state
        .store
        .consume_login_token(&hash, state.now_ms())
        .await?;
    record
        .map(|r| r.user_id)
        .ok_or_else(|| MatrixError::forbidden("Invalid login token"))
}

/// Resolves a `com.devture.shared_secret_auth` login (`crate::shared_secret_auth`). `data` is
/// `LoginInfo::data()`'s view of the request body with `type` removed: `{"identifier": {"type":
/// "m.id.user", "user": "..."}, "token": "<hex hmac-sha512>"}`. Returns
/// [`MatrixError::forbidden`] with the same generic wording as a failed password login on any
/// failure (unknown/malformed token, wrong secret, non-`m.id.user` identifier), so a probing
/// client cannot distinguish "this feature is disabled" from "you got the token wrong" — the only
/// case advertised differently is the feature being entirely unconfigured, which
/// [`get_login_types`] simply does not list.
async fn resolve_shared_secret_auth_login(
    state: &AuthState,
    data: &serde_json::Map<String, Value>,
) -> Result<OwnedUserId, MatrixError> {
    let config = state.config.get();
    let Some(secret) = config.shared_secret_auth_secret.as_deref() else {
        return Err(MatrixError::new(
            StatusCode::BAD_REQUEST,
            ErrCode::Unrecognized,
            "com.devture.shared_secret_auth is not enabled on this server",
        ));
    };

    let identifier: UserIdentifier = data
        .get("identifier")
        .cloned()
        .ok_or_else(|| MatrixError::missing_param("Missing identifier"))
        .and_then(|v| {
            serde_json::from_value(v).map_err(|_| MatrixError::invalid_param("invalid identifier"))
        })?;
    let UserIdentifier::Matrix(m) = identifier else {
        return Err(MatrixError::invalid_param(
            "com.devture.shared_secret_auth only supports m.id.user identifiers",
        ));
    };
    let user_id = UserId::parse_with_server_name(m.user.as_str(), state.server_name())
        .map_err(|_| MatrixError::invalid_param("invalid user identifier"))?;

    let token = data
        .get("token")
        .and_then(Value::as_str)
        .ok_or_else(|| MatrixError::missing_param("Missing token"))?;

    shared_secret_auth::verify_token(secret.as_bytes(), &user_id, token)
        .map_err(|_| MatrixError::forbidden(INVALID_USERNAME_OR_PASSWORD))?;

    Ok(user_id)
}

/// The user an `m.login.application_service` login logs in, and the id of the appservice whose
/// token asked (the one the session is issued to, decision 0053).
async fn resolve_appservice_login(
    state: &AuthState,
    headers: &HeaderMap,
    query: &HashMap<String, String>,
    identifier: Option<&UserIdentifier>,
) -> Result<(OwnedUserId, String), MatrixError> {
    let token = bearer_or_query_token(headers, query).ok_or_else(MatrixError::missing_token)?;
    let record: AppserviceRecord = state
        .appservices
        .lookup_by_token(&token)
        .await
        .ok_or_else(|| MatrixError::unknown_token(false))?;

    let Some(identifier) = identifier else {
        return Ok((record.sender, record.appservice_id));
    };
    let UserIdentifier::Matrix(m) = identifier else {
        return Err(MatrixError::invalid_param(
            "Application service login only supports m.id.user identifiers",
        ));
    };
    let user_id = UserId::parse_with_server_name(m.user.as_str(), state.server_name())
        .map_err(|_| MatrixError::invalid_param("invalid user identifier"))?;
    if !record.can_control(&user_id) {
        // Its double-puppeting registration may claim them: a bridge that knows only this login
        // form (mautrix-imessage) double puppets with its own token, while its own registration
        // does not claim its person (decision 0046).
        let through_double_puppet = state
            .appservices
            .double_puppet_record_for(&record.appservice_id)
            .await
            .filter(|dp| dp.can_control(&user_id));
        let Some(double_puppet) = through_double_puppet else {
            return Err(MatrixError::forbidden(
                "Application service cannot masquerade as this user",
            ));
        };
        tracing::info!(
            appservice = %record.appservice_id,
            double_puppet = %double_puppet.appservice_id,
            user_id = %user_id,
            "an appservice logged in as a user its double-puppeting registration claims"
        );
    }
    // Appservice-driven logins auto-provision the user if it does not exist yet, matching
    // Synapse's behavior for `m.login.application_service` (the AS namespace already proved the
    // right to this user ID; there is no separate registration step for it).
    if state.store.get_user(&user_id).await?.is_none() {
        state
            .store
            .create_user(crate::store::UserRecord::new(
                user_id.clone(),
                state.now_ms(),
            ))
            .await?;
    }
    Ok((user_id, record.appservice_id))
}

fn bearer_or_query_token(headers: &HeaderMap, query: &HashMap<String, String>) -> Option<String> {
    if let Some(value) = headers.get(AUTHORIZATION)
        && let Ok(raw) = value.to_str()
        && let Some(token) = raw.strip_prefix("Bearer ")
    {
        return Some(token.to_string());
    }
    query.get("access_token").cloned()
}

fn login_response(user_id: &UserId, session: NewSession) -> Json<Value> {
    let mut body = json!({
        "user_id": user_id,
        "access_token": session.access_token,
        "device_id": session.device_id,
    });
    if let Some(rt) = session.refresh_token {
        body["refresh_token"] = json!(rt);
    }
    if let Some(ms) = session.expires_in_ms {
        body["expires_in_ms"] = json!(ms);
    }
    Json(body)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::password::hash_password;
    use crate::store::{LoginTokenRecord, UserRecord};
    use ruma::user_id;

    async fn state_with_password_user(user_id: &ruma::UserId, password: &str) -> AuthState {
        let state = AuthState::in_memory();
        let mut record = UserRecord::new(user_id.to_owned(), 0);
        record.password_hash = Some(hash_password(password).unwrap());
        state.store.create_user(record).await.unwrap();
        state
    }

    /// mautrix-imessage double puppets only through an appservice login with its own token.
    /// Its own registration does not claim its person (decision 0046), so the login is allowed
    /// for whoever its double-puppeting registration claims, and nobody else.
    #[tokio::test]
    async fn an_appservice_logs_in_as_a_user_its_double_puppeting_registration_claims() {
        use crate::appservice::{AppserviceRecord, InMemoryAppserviceRegistry, NamespaceRule};
        let rule = |pattern: &str, exclusive| NamespaceRule {
            regex: regex::Regex::new(pattern).unwrap(),
            exclusive,
        };
        let registry = InMemoryAppserviceRegistry::new();
        registry.insert(
            "imessage_token",
            AppserviceRecord::new(
                "imessage-brandon",
                user_id!("@imessagebot_brandon:example.org").to_owned(),
                vec![rule(r"@imessage_brandon_.*:example\.org", true)],
            ),
        );
        let state = AuthState::in_memory().with_appservices(std::sync::Arc::new(registry));
        let login_as = |state: AuthState, user: &str| {
            let mut headers = HeaderMap::new();
            headers.insert(AUTHORIZATION, "Bearer imessage_token".parse().unwrap());
            let body = json!({
                "type": "m.login.application_service",
                "identifier": {"type": "m.id.user", "user": user},
            });
            async move {
                post_login(
                    State(state),
                    headers,
                    Query(HashMap::new()),
                    hs_http::buckets::ClientIp(None),
                    PermissiveJson(body),
                )
                .await
                .map(|r| r.status())
                .map_err(|e| e.status())
            }
        };

        // No double-puppeting registration: its person is not its own registration's to claim.
        assert_eq!(
            login_as(state.clone(), "@brandon:example.org").await,
            Err(StatusCode::FORBIDDEN)
        );

        let registry = InMemoryAppserviceRegistry::new();
        registry.insert(
            "imessage_token",
            AppserviceRecord::new(
                "imessage-brandon",
                user_id!("@imessagebot_brandon:example.org").to_owned(),
                vec![rule(r"@imessage_brandon_.*:example\.org", true)],
            ),
        );
        registry.insert_double_puppet(
            "imessage-brandon",
            AppserviceRecord::new(
                "imessage-brandon-doublepuppet",
                user_id!("@imessage-brandon-doublepuppet:example.org").to_owned(),
                vec![rule(r"@brandon:example\.org", false)],
            ),
        );
        let state = AuthState::in_memory().with_appservices(std::sync::Arc::new(registry));
        assert_eq!(
            login_as(state.clone(), "@brandon:example.org").await,
            Ok(StatusCode::OK)
        );
        // Only whom the double-puppeting registration claims.
        assert_eq!(
            login_as(state, "@alice:example.org").await,
            Err(StatusCode::FORBIDDEN)
        );
    }

    #[tokio::test]
    async fn password_login_by_localpart_succeeds() {
        let state = state_with_password_user(user_id!("@alice:example.org"), "hunter2").await;
        let body = json!({"type": "m.login.password", "identifier": {"type": "m.id.user", "user": "alice"}, "password": "hunter2"});
        let response = post_login(
            State(state),
            HeaderMap::new(),
            Query(HashMap::new()),
            hs_http::buckets::ClientIp(None),
            PermissiveJson(body),
        )
        .await
        .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    /// Logs in with `body` (and `as_token` as the bearer, if given); the response's JSON.
    async fn login_json(state: &AuthState, as_token: Option<&str>, body: Value) -> Value {
        let mut headers = HeaderMap::new();
        if let Some(token) = as_token {
            headers.insert(AUTHORIZATION, format!("Bearer {token}").parse().unwrap());
        }
        let response = post_login(
            State(state.clone()),
            headers,
            Query(HashMap::new()),
            hs_http::buckets::ClientIp(None),
            PermissiveJson(body),
        )
        .await
        .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    /// The requester the middleware makes of a request bearing `token`.
    async fn requester_for(state: &AuthState, token: &str) -> crate::requester::Requester {
        use axum::extract::FromRequestParts;
        let (mut parts, ()) = axum::http::Request::builder()
            .uri("/x")
            .header(AUTHORIZATION, format!("Bearer {token}"))
            .body(())
            .unwrap()
            .into_parts();
        crate::requester::Requester::from_request_parts(&mut parts, state)
            .await
            .unwrap()
    }

    /// Decision 0053: the session an `m.login.application_service` login makes (a bridge's
    /// double puppet) is attributed to the appservice that logged in, on the token record and,
    /// resolved against the registration as it stands, on every request's requester -- without
    /// making the request an appservice's. Its `/refresh` keeps the attribution. The same
    /// user's password session is not attributed. An edited registration applies to the next
    /// request; a removed one leaves the session an ordinary one.
    #[tokio::test]
    async fn an_appservice_login_issues_a_session_attributed_to_the_appservice() {
        use crate::appservice::{AppserviceRecord, InMemoryAppserviceRegistry, NamespaceRule};
        use crate::requester::{IssuingAppservice, RateLimitExemption};
        let brandon = user_id!("@brandon:example.org");
        let mut record = AppserviceRecord::new(
            "imessage",
            user_id!("@imessagebot:example.org").to_owned(),
            vec![NamespaceRule {
                regex: regex::Regex::new(r"@brandon:example\.org").unwrap(),
                exclusive: false,
            }],
        );
        record.rate_limited = false;
        let registry = std::sync::Arc::new(InMemoryAppserviceRegistry::new());
        registry.insert("as_token_imessage", record.clone());
        let state = state_with_password_user(brandon, "hunter2")
            .await
            .with_appservices(registry.clone());

        let puppet = login_json(
            &state,
            Some("as_token_imessage"),
            json!({
                "type": "m.login.application_service",
                "identifier": {"type": "m.id.user", "user": "brandon"},
                "refresh_token": true,
            }),
        )
        .await;
        let own = login_json(
            &state,
            None,
            json!({"type": "m.login.password", "identifier": {"type": "m.id.user", "user": "brandon"}, "password": "hunter2"}),
        )
        .await;
        let puppet_token = puppet["access_token"].as_str().unwrap();
        let own_token = own["access_token"].as_str().unwrap();

        let stored = state
            .store
            .get_access_token(&TokenHash::of(puppet_token))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(stored.issued_by_appservice.as_deref(), Some("imessage"));
        let stored_own = state
            .store
            .get_access_token(&TokenHash::of(own_token))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(stored_own.issued_by_appservice, None);

        let requester = requester_for(&state, puppet_token).await;
        assert_eq!(requester.user_id, brandon);
        assert_eq!(requester.appservice, None, "not an appservice request");
        assert_eq!(
            requester.issued_by_appservice,
            Some(IssuingAppservice {
                appservice_id: "imessage".to_owned(),
                rate_limited: false
            })
        );
        assert_eq!(
            requester.rate_limit_exemption(),
            Some(RateLimitExemption::IssuedToAppservice)
        );
        assert!(requester.exempt_from_rate_limits(false));
        assert!(!requester.exempt_from_rate_limits(true), "an override wins");
        let own_requester = requester_for(&state, own_token).await;
        assert_eq!(own_requester.issued_by_appservice, None);
        assert_eq!(own_requester.rate_limit_exemption(), None);

        // A refreshed session is still the appservice's.
        let refreshed = crate::routes::refresh::post_refresh(
            State(state.clone()),
            PermissiveJson(json!({"refresh_token": puppet["refresh_token"]})),
        )
        .await
        .unwrap()
        .0;
        let refreshed_token = refreshed["access_token"].as_str().unwrap();
        assert_eq!(
            requester_for(&state, refreshed_token)
                .await
                .rate_limit_exemption(),
            Some(RateLimitExemption::IssuedToAppservice)
        );

        // The registration now says `rate_limited: true`: the next request is limited.
        record.rate_limited = true;
        registry.insert("as_token_imessage", record);
        let requester = requester_for(&state, refreshed_token).await;
        assert_eq!(
            requester
                .issued_by_appservice
                .as_ref()
                .map(|a| a.rate_limited),
            Some(true)
        );
        assert_eq!(requester.rate_limit_exemption(), None);

        // Removed: an ordinary session.
        registry.remove("as_token_imessage");
        let requester = requester_for(&state, refreshed_token).await;
        assert_eq!(requester.issued_by_appservice, None);
        assert_eq!(requester.rate_limit_exemption(), None);
    }

    /// `auth.password.enabled` off: `GET /login` does not offer `m.login.password` and a
    /// password login is refused `403 M_FORBIDDEN`, read from the live configuration so turning
    /// it back on applies to the next request.
    #[tokio::test]
    async fn a_password_login_is_neither_offered_nor_accepted_when_turned_off() {
        let state = state_with_password_user(user_id!("@alice:example.org"), "hunter2").await;
        let types = |state: AuthState| async move {
            let Json(body) = get_login_types(State(state)).await;
            body["flows"]
                .as_array()
                .unwrap()
                .iter()
                .map(|f| f["type"].as_str().unwrap().to_owned())
                .collect::<Vec<_>>()
        };
        let login = |state: AuthState| async move {
            let body = json!({"type": "m.login.password", "identifier": {"type": "m.id.user", "user": "alice"}, "password": "hunter2"});
            post_login(
                State(state),
                HeaderMap::new(),
                Query(HashMap::new()),
                hs_http::buckets::ClientIp(None),
                PermissiveJson(body),
            )
            .await
        };
        assert!(
            types(state.clone())
                .await
                .contains(&"m.login.password".to_owned())
        );

        let mut config = state.config.get().as_ref().clone();
        config.password_login_enabled = false;
        state.set_config(config.clone());
        let flows = types(state.clone()).await;
        assert!(!flows.contains(&"m.login.password".to_owned()), "{flows:?}");
        assert!(flows.contains(&"m.login.token".to_owned()));
        let err = login(state.clone()).await.unwrap_err();
        assert_eq!(err.status(), StatusCode::FORBIDDEN);
        assert_eq!(err.errcode(), ErrCode::Forbidden);

        config.password_login_enabled = true;
        state.set_config(config);
        assert_eq!(login(state).await.unwrap().status(), StatusCode::OK);
    }

    /// `rate_limits.login` is per client address, and a request with no address (this host's own
    /// tooling) is not limited.
    #[tokio::test]
    async fn the_login_limit_is_per_client_address() {
        let state = state_with_password_user(user_id!("@alice:example.org"), "hunter2").await;
        state
            .limits
            .login
            .set_limit(Some(hs_http::buckets::BucketLimit {
                per_second: 0.001,
                burst_count: 1,
            }));
        let login = |address: Option<&str>| {
            let state = state.clone();
            let address = address.map(|a| a.parse().unwrap());
            async move {
                let body = json!({"type": "m.login.password", "identifier": {"type": "m.id.user", "user": "alice"}, "password": "hunter2"});
                post_login(
                    State(state),
                    HeaderMap::new(),
                    Query(HashMap::new()),
                    hs_http::buckets::ClientIp(address),
                    PermissiveJson(body),
                )
                .await
            }
        };
        assert!(login(Some("203.0.113.1")).await.is_ok());
        let refused = login(Some("203.0.113.1")).await.unwrap_err();
        assert_eq!(refused.status(), StatusCode::TOO_MANY_REQUESTS);
        assert!(login(Some("203.0.113.2")).await.is_ok());
        assert!(login(None).await.is_ok());
        assert!(login(None).await.is_ok());
    }

    #[tokio::test]
    async fn password_login_with_deprecated_user_field_succeeds() {
        let state = state_with_password_user(user_id!("@bob:example.org"), "hunter2").await;
        let body = json!({"type": "m.login.password", "user": "bob", "password": "hunter2"});
        let response = post_login(
            State(state),
            HeaderMap::new(),
            Query(HashMap::new()),
            hs_http::buckets::ClientIp(None),
            PermissiveJson(body),
        )
        .await
        .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn password_login_with_different_case_localpart_succeeds() {
        // The account is stored (as it would be after `register_user`'s lower-casing) under the
        // lower-case localpart; the client logs in typing a capitalized version, as a browser
        // autocapitalize or a user typing their name naturally might.
        let state = state_with_password_user(user_id!("@alice:example.org"), "hunter2").await;
        let body = json!({"type": "m.login.password", "identifier": {"type": "m.id.user", "user": "Alice"}, "password": "hunter2"});
        let response = post_login(
            State(state),
            HeaderMap::new(),
            Query(HashMap::new()),
            hs_http::buckets::ClientIp(None),
            PermissiveJson(body),
        )
        .await
        .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn password_login_with_deprecated_user_field_and_different_case_succeeds() {
        let state = state_with_password_user(user_id!("@bob:example.org"), "hunter2").await;
        let body = json!({"type": "m.login.password", "user": "BOB", "password": "hunter2"});
        let response = post_login(
            State(state),
            HeaderMap::new(),
            Query(HashMap::new()),
            hs_http::buckets::ClientIp(None),
            PermissiveJson(body),
        )
        .await
        .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn password_login_with_a_full_mxid_in_a_different_case_succeeds() {
        let state = state_with_password_user(user_id!("@carol:example.org"), "hunter2").await;
        let body = json!({"type": "m.login.password", "identifier": {"type": "m.id.user", "user": "@Carol:example.org"}, "password": "hunter2"});
        let response = post_login(
            State(state),
            HeaderMap::new(),
            Query(HashMap::new()),
            hs_http::buckets::ClientIp(None),
            PermissiveJson(body),
        )
        .await
        .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn wrong_password_is_forbidden_with_generic_message() {
        let state = state_with_password_user(user_id!("@carol:example.org"), "hunter2").await;
        let body = json!({"type": "m.login.password", "identifier": {"type": "m.id.user", "user": "carol"}, "password": "wrong"});
        let err = post_login(
            State(state),
            HeaderMap::new(),
            Query(HashMap::new()),
            hs_http::buckets::ClientIp(None),
            PermissiveJson(body),
        )
        .await
        .unwrap_err();
        assert_eq!(err.status(), StatusCode::FORBIDDEN);
        assert_eq!(err.errcode().as_str(), "M_FORBIDDEN");
    }

    #[tokio::test]
    async fn unknown_user_gives_the_same_generic_message_as_wrong_password() {
        let state = AuthState::in_memory();
        let body = json!({"type": "m.login.password", "identifier": {"type": "m.id.user", "user": "nobody"}, "password": "x"});
        let err = post_login(
            State(state),
            HeaderMap::new(),
            Query(HashMap::new()),
            hs_http::buckets::ClientIp(None),
            PermissiveJson(body),
        )
        .await
        .unwrap_err();
        assert_eq!(err.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn password_login_by_email_identifier_succeeds() {
        let state = state_with_password_user(user_id!("@dave:example.org"), "hunter2").await;
        state
            .store
            .add_threepid(crate::store::ThreepidRecord {
                user_id: user_id!("@dave:example.org").to_owned(),
                medium: "email".to_owned(),
                address: "dave@example.org".to_owned(),
                added_at_ms: 1,
                validated_at_ms: 1,
            })
            .await
            .unwrap();
        let body = json!({
            "type": "m.login.password",
            "identifier": {"type": "m.id.thirdparty", "medium": "email", "address": "dave@example.org"},
            "password": "hunter2"
        });
        let response = post_login(
            State(state),
            HeaderMap::new(),
            Query(HashMap::new()),
            hs_http::buckets::ClientIp(None),
            PermissiveJson(body),
        )
        .await
        .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn login_token_is_consumed_once() {
        let state = AuthState::in_memory();
        let uid = user_id!("@erin:example.org").to_owned();
        state
            .store
            .create_user(UserRecord::new(uid.clone(), 0))
            .await
            .unwrap();
        let raw_token = "syl_testtoken";
        state
            .store
            .put_login_token(LoginTokenRecord {
                hash: TokenHash::of(raw_token),
                user_id: uid,
                expires_at_ms: state.now_ms() + 60_000,
                used: false,
            })
            .await
            .unwrap();

        let body = json!({"type": "m.login.token", "token": raw_token});
        let response = post_login(
            State(state.clone()),
            HeaderMap::new(),
            Query(HashMap::new()),
            hs_http::buckets::ClientIp(None),
            PermissiveJson(body.clone()),
        )
        .await
        .unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let err = post_login(
            State(state),
            HeaderMap::new(),
            Query(HashMap::new()),
            hs_http::buckets::ClientIp(None),
            PermissiveJson(body),
        )
        .await
        .unwrap_err();
        assert_eq!(err.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn get_login_types_lists_password_and_token() {
        let Json(body) = get_login_types(State(AuthState::in_memory())).await;
        let types: Vec<String> = body["flows"]
            .as_array()
            .unwrap()
            .iter()
            .map(|f| f["type"].as_str().unwrap().to_string())
            .collect();
        assert!(types.contains(&"m.login.password".to_string()));
        assert!(types.contains(&"m.login.token".to_string()));
        // As Synapse lists it: a bridge on mautrix-go's older framework will not start without.
        assert!(types.contains(&APPSERVICE_LOGIN_TYPE.to_string()));
    }

    #[tokio::test]
    async fn shared_secret_auth_is_not_advertised_when_unconfigured() {
        let Json(body) = get_login_types(State(AuthState::in_memory())).await;
        let types: Vec<String> = body["flows"]
            .as_array()
            .unwrap()
            .iter()
            .map(|f| f["type"].as_str().unwrap().to_string())
            .collect();
        assert!(!types.contains(&SHARED_SECRET_AUTH_LOGIN_TYPE.to_string()));
    }

    #[tokio::test]
    async fn shared_secret_auth_is_advertised_when_configured() {
        let config = crate::config::AuthConfig {
            shared_secret_auth_secret: Some("sekrit".to_string()),
            ..crate::config::AuthConfig::default()
        };
        let Json(body) = get_login_types(State(AuthState::in_memory_with_config(config))).await;
        let types: Vec<String> = body["flows"]
            .as_array()
            .unwrap()
            .iter()
            .map(|f| f["type"].as_str().unwrap().to_string())
            .collect();
        assert!(types.contains(&SHARED_SECRET_AUTH_LOGIN_TYPE.to_string()));
    }

    #[tokio::test]
    async fn shared_secret_auth_login_succeeds_with_a_valid_token() {
        let config = crate::config::AuthConfig {
            shared_secret_auth_secret: Some("sekrit".to_string()),
            ..crate::config::AuthConfig::default()
        };
        let state = AuthState::in_memory_with_config(config);
        let uid = user_id!("@puppet:example.org").to_owned();
        state
            .store
            .create_user(UserRecord::new(uid.clone(), 0))
            .await
            .unwrap();
        let token = shared_secret_auth::compute_token(b"sekrit", &uid);
        let body = json!({
            "type": SHARED_SECRET_AUTH_LOGIN_TYPE,
            "identifier": {"type": "m.id.user", "user": "puppet"},
            "token": token,
        });
        let response = post_login(
            State(state),
            HeaderMap::new(),
            Query(HashMap::new()),
            hs_http::buckets::ClientIp(None),
            PermissiveJson(body),
        )
        .await
        .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn shared_secret_auth_login_rejects_a_wrong_token() {
        let config = crate::config::AuthConfig {
            shared_secret_auth_secret: Some("sekrit".to_string()),
            ..crate::config::AuthConfig::default()
        };
        let state = AuthState::in_memory_with_config(config);
        let uid = user_id!("@puppet2:example.org").to_owned();
        state
            .store
            .create_user(UserRecord::new(uid.clone(), 0))
            .await
            .unwrap();
        let body = json!({
            "type": SHARED_SECRET_AUTH_LOGIN_TYPE,
            "identifier": {"type": "m.id.user", "user": "puppet2"},
            "token": "not the right token",
        });
        let err = post_login(
            State(state),
            HeaderMap::new(),
            Query(HashMap::new()),
            hs_http::buckets::ClientIp(None),
            PermissiveJson(body),
        )
        .await
        .unwrap_err();
        assert_eq!(err.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn shared_secret_auth_login_fails_cleanly_when_disabled() {
        let state = AuthState::in_memory();
        let uid = user_id!("@puppet3:example.org").to_owned();
        state
            .store
            .create_user(UserRecord::new(uid.clone(), 0))
            .await
            .unwrap();
        let body = json!({
            "type": SHARED_SECRET_AUTH_LOGIN_TYPE,
            "identifier": {"type": "m.id.user", "user": "puppet3"},
            "token": "whatever",
        });
        let err = post_login(
            State(state),
            HeaderMap::new(),
            Query(HashMap::new()),
            hs_http::buckets::ClientIp(None),
            PermissiveJson(body),
        )
        .await
        .unwrap_err();
        assert_eq!(err.errcode().as_str(), "M_UNRECOGNIZED");
    }
}
