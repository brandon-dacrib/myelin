//! `POST /register` and `GET /register/available`.
//!
//! Registration drives the same [`crate::uia`] state machine as password change and device
//! deletion, and remembers what a dance needs across its rounds in the UIA session, as Synapse's
//! `RegisterRestServlet` does:
//!
//! - **The parameters.** The request body (less `auth` and `password`) is stored on the session;
//!   a later round that sends only `auth` gets the stored parameters back (Sytest's "registration
//!   remembers parameters": `device_id` and `initial_device_display_name` from the first call,
//!   `inhibit_login` likewise). A round that sends parameters replaces them.
//! - **The password**, as its hash only, never in plain text, so a later round need not send it.
//! - **The account made.** Once a session has registered an account, the same session finishing
//!   again logs that account in again rather than making a second one or answering
//!   `M_USER_IN_USE` (Sytest's "registration is idempotent, with/without username specified").
//! - **A session, once issued, is required** for an `m.login.dummy` stage for that username:
//!   one submitted without `session` while a session handed out for the same username is still
//!   open gets the challenge again (`401`) instead of registering around it (Complement's
//!   "Registration without a session fails"). A one-shot registration that never asked for a
//!   session is unaffected, as are a guest upgrading itself (Sytest's "Guest user can upgrade to
//!   fully featured user" sends its stage without the session it was given) and a stage carrying
//!   its own proof, such as an invite link's registration token.
//!
//! Supported UIA stages: `m.login.dummy` (the flow when nothing else is required),
//! `m.login.registration_token` (checked against
//! [`crate::config::AuthConfig::valid_registration_tokens`] and the token store),
//! `m.login.terms` (acknowledgement only), and `m.login.recaptcha` when `auth.recaptcha` has a
//! secret key ([`crate::recaptcha`]; required in every flow only with `auth.recaptcha.required`,
//! but a client may complete it either way, as Synapse allows), and `m.login.email.identity` (an
//! address this server validated, [`crate::threepid`]) as a flow of its own while email can be
//! sent. `m.login.msisdn` fails cleanly with `M_UNRECOGNIZED`, as does a stage this server
//! cannot check.

use std::collections::HashMap;

use axum::Json;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use rand::Rng;
use rand::distr::Alphanumeric;
use ruma::api::client::uiaa::{AuthData, AuthFlow, AuthType};
use ruma::{OwnedDeviceId, OwnedUserId, UserId};
use serde_json::{Value, json};

use crate::error::{ErrCode, MatrixError};
use crate::password;
use crate::session;
use crate::state::AuthState;
use crate::store::UserRecord;
use crate::uia;
use hs_http::body::PermissiveJson;

/// The flows `/register` offers. `token_only` is a server with open registration off, where a
/// registration token is the one way in: that is what makes an invite link work on a server that
/// is otherwise closed.
fn registration_flows(state: &AuthState, token_only: bool) -> Vec<AuthFlow> {
    let mut required = Vec::new();
    if state.config.get().registration_requires_token || token_only {
        required.push(AuthType::RegistrationToken);
    }
    if state
        .config
        .get()
        .recaptcha
        .as_ref()
        .is_some_and(|r| r.required)
    {
        required.push(AuthType::ReCaptcha);
    }
    if state.config.get().terms_enabled {
        required.push(AuthType::Terms);
    }
    let mut flows = if required.is_empty() {
        vec![AuthFlow::new(vec![AuthType::Dummy])]
    } else {
        vec![AuthFlow::new(required.clone())]
    };
    // Registering with an email address this server validates (`crate::threepid`), offered as
    // its own flow while the server can send email, as Synapse offers `[m.login.email.identity]`.
    if crate::threepid::email_available(state) {
        required.push(AuthType::EmailIdentity);
        flows.push(AuthFlow::new(required));
    }
    flows
}

/// Whether `auth_type` is a stage this server can satisfy now: the CAPTCHA once its key is
/// configured, an email address once this server can send email. Anything else (`m.login.msisdn`,
/// an unknown stage) gets the clean `M_UNRECOGNIZED` failure rather than "invalid auth".
fn stage_is_supported(state: &AuthState, auth_type: &AuthType) -> bool {
    match auth_type {
        AuthType::Dummy | AuthType::RegistrationToken | AuthType::Terms => true,
        AuthType::ReCaptcha => state.config.get().recaptcha.is_some(),
        AuthType::EmailIdentity => crate::threepid::email_available(state),
        _ => false,
    }
}

/// The challenge's `params`: the site key a client shows the CAPTCHA with, when the CAPTCHA is
/// configured.
fn challenge_params(state: &AuthState) -> Value {
    match state
        .config
        .get()
        .recaptcha
        .as_ref()
        .and_then(|r| r.public_key.clone())
    {
        Some(public_key) => json!({"m.login.recaptcha": {"public_key": public_key}}),
        None => json!({}),
    }
}

/// Checks a CAPTCHA answer with the CAPTCHA service. A service that cannot be asked fails the
/// stage (logged), and the client may try again.
async fn verify_recaptcha(
    state: &AuthState,
    response: &str,
    remote_ip: Option<&str>,
) -> Result<bool, MatrixError> {
    let Some(settings) = state.config.get().recaptcha.clone() else {
        return Ok(false);
    };
    let verifier = state.recaptcha_verifier().map_err(|error| {
        tracing::warn!(%error, "could not build the CAPTCHA verifier");
        MatrixError::internal()
    })?;
    let check = crate::recaptcha::RecaptchaCheck {
        siteverify_api: &settings.siteverify_api,
        private_key: &settings.private_key,
        response,
        remote_ip,
    };
    match verifier.verify(&check).await {
        Ok(true) => {
            crate::recaptcha::count("passed");
            Ok(true)
        }
        Ok(false) => {
            crate::recaptcha::count("failed");
            tracing::info!("a registration's CAPTCHA answer was refused by the CAPTCHA service");
            Ok(false)
        }
        Err(error) => {
            crate::recaptcha::count("error");
            tracing::warn!(%error, api = %settings.siteverify_api, "could not check a CAPTCHA answer");
            Ok(false)
        }
    }
}

/// Whether a submitted stage succeeded. A registration token is checked against the tokens in
/// the configuration file (which have no limits) and then the token store, where passing the
/// stage takes one of the token's places for `session_id` until the registration finishes or
/// the session expires -- see [`crate::registration_tokens`].
async fn verify_stage(
    state: &AuthState,
    data: &AuthData,
    session_id: Option<&str>,
    remote_ip: Option<&str>,
) -> Result<bool, MatrixError> {
    Ok(match data {
        AuthData::Dummy(_) => true,
        AuthData::ReCaptcha(r) => verify_recaptcha(state, &r.response, remote_ip).await?,
        AuthData::Terms(_) => true,
        AuthData::RegistrationToken(t) => {
            if state
                .config
                .get()
                .valid_registration_tokens
                .contains(&t.token)
            {
                true
            } else if let Some(session_id) = session_id {
                let reserved = state
                    .registration_tokens
                    .reserve(
                        &t.token,
                        session_id,
                        state.now_ms(),
                        state.config.get().uia_session_timeout_ms,
                    )
                    .await?;
                if reserved {
                    state
                        .store
                        .set_session_data(session_id, REGISTRATION_TOKEN_KEY, json!(t.token))
                        .await?;
                }
                reserved
            } else {
                false
            }
        }
        AuthData::EmailIdentity(e) => {
            // An address this server validated (`crate::threepid`), named by the session the
            // validation email started; remembered on the UIA session so that creating the
            // account can add it.
            let creds = &e.thirdparty_id_creds;
            match crate::threepid::validated_session(
                state,
                creds.sid.as_str(),
                creds.client_secret.as_str(),
            )
            .await?
            {
                Some(record)
                    if record.medium == "email"
                        && state
                            .store
                            .get_user_by_threepid("email", &record.address)
                            .await?
                            .is_none() =>
                {
                    if let Some(session_id) = session_id {
                        state
                            .store
                            .set_session_data(
                                session_id,
                                REGISTRATION_EMAIL_KEY,
                                serde_json::to_value(&record).unwrap_or_default(),
                            )
                            .await?;
                    }
                    true
                }
                _ => false,
            }
        }
        _ => false,
    })
}

/// Where a registration's UIA session remembers the token it presented, so that creating the
/// account can count the use.
const REGISTRATION_TOKEN_KEY: &str = "registration_token";

/// Where a registration's UIA session keeps the request's parameters (less `auth` and
/// `password`), for a later round that sends only `auth`. See the module docs.
const PARAMS_KEY: &str = "registration_params";

/// Where a registration's UIA session keeps the password's hash, for a later round that does
/// not send the password again.
const PASSWORD_HASH_KEY: &str = "registration_password_hash";

/// Where a registration's UIA session records the account it made, so finishing the same
/// session again logs that account in instead of making another.
const REGISTERED_USER_KEY: &str = "registered_user_id";

/// The registrations that were handed a UIA session, by the username they asked for, so that a
/// stage submitted for that username without the session can be sent back to it (see the module
/// docs). Kept in this process's memory and bounded: it is a strictness check, not state a
/// registration depends on -- behind a load balancer, a request that reaches another replica
/// simply is not checked.
#[derive(Debug, Default)]
pub struct PendingRegistrations {
    by_username: std::sync::Mutex<HashMap<String, (String, u64)>>,
}

impl PendingRegistrations {
    /// The most usernames remembered at once; the oldest is forgotten first.
    const CAPACITY: usize = 10_000;

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, (String, u64)>> {
        self.by_username
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn remember(&self, username: &str, session_id: &str, now_ms: u64, timeout_ms: u64) {
        let mut map = self.lock();
        map.retain(|_, (_, at)| at.saturating_add(timeout_ms) >= now_ms);
        if map.len() >= Self::CAPACITY
            && !map.contains_key(username)
            && let Some(oldest) = map
                .iter()
                .min_by_key(|(_, (_, at))| *at)
                .map(|(name, _)| name.clone())
        {
            map.remove(&oldest);
        }
        map.entry(username.to_owned())
            .or_insert_with(|| (session_id.to_owned(), now_ms));
    }

    fn session_for(&self, username: &str, now_ms: u64, timeout_ms: u64) -> Option<String> {
        self.lock()
            .get(username)
            .filter(|(_, at)| at.saturating_add(timeout_ms) >= now_ms)
            .map(|(session, _)| session.clone())
    }

    fn forget(&self, username: &str) {
        self.lock().remove(username);
    }
}

/// Where a registration's UIA session remembers the email address its `m.login.email.identity`
/// stage validated (the [`crate::store::ThreepidValidationRecord`]), so that creating the
/// account can add it.
const REGISTRATION_EMAIL_KEY: &str = "registration_email";

/// Whether any stored token would admit a registration now. On a server with open registration
/// off, a registration that presents nothing is refused outright unless this holds, so a closed
/// server with no invitations out looks exactly as closed as it did before tokens existed.
async fn any_token_usable(state: &AuthState) -> Result<bool, MatrixError> {
    if !state.config.get().valid_registration_tokens.is_empty() {
        return Ok(true);
    }
    let now = state.now_ms();
    let timeout = state.config.get().uia_session_timeout_ms;
    Ok(state
        .registration_tokens
        .list()
        .await?
        .iter()
        .any(|t| t.usable(now, timeout)))
}

/// `GET /_matrix/client/v1/register/m.login.registration_token/validity?token=...`: whether the
/// token would let someone register right now. Checking does not use it. Answers the same with
/// open registration on or off, since a token is exactly what works while it is off.
pub async fn get_registration_token_validity(
    State(state): State<AuthState>,
    Query(query): Query<HashMap<String, String>>,
) -> Result<Json<Value>, MatrixError> {
    let token = query
        .get("token")
        .ok_or_else(|| MatrixError::missing_param("Missing token"))?;
    let valid = if state.config.get().valid_registration_tokens.contains(token) {
        true
    } else {
        state
            .registration_tokens
            .get(token)
            .await?
            .is_some_and(|t| t.usable(state.now_ms(), state.config.get().uia_session_timeout_ms))
    };
    Ok(Json(json!({ "valid": valid })))
}

/// `GET /register/available?username=...`.
///
/// Deliberately does **not** lower-case `username` the way [`register_user`] does before
/// checking it. Read `refs/synapse/synapse/rest/client/register.py`'s
/// `UsernameAvailabilityRestServlet.on_GET`: it passes the raw query parameter straight to
/// `check_username` with no `.lower()` call (unlike both of `RegisterRestServlet`'s call sites),
/// so an upper-case query on real Synapse gets `M_INVALID_USERNAME` even though `/register` would
/// happily downcase and accept the same string. That is a genuine asymmetry between the two
/// endpoints, not an oversight this crate is inventing — Complement's own
/// `apidoc_register_test.go` never exercises an upper-case `/register/available` query, so there
/// is no conformance test pulling either way; matching Synapse's actual behavior here rather than
/// guessing was the tie-breaker.
pub async fn get_register_available(
    State(state): State<AuthState>,
    Query(query): Query<HashMap<String, String>>,
) -> Result<Json<Value>, MatrixError> {
    let username = query
        .get("username")
        .ok_or_else(|| MatrixError::missing_param("Missing username"))?;
    validate_localpart(&state, username)?;
    refuse_exclusive(&state, username, None).await?;
    if !state.store.is_localpart_available(username).await? {
        return Err(MatrixError::user_in_use());
    }
    Ok(Json(json!({"available": true})))
}

/// Rejects any localpart outside the spec's *strict* user ID grammar.
///
/// `UserId::parse_with_server_name`'s own validation (`ruma_identifiers_validation::user_id::
/// validate`, used by `UserId::parse`/`TryFrom<&str>`) only rejects a literal `:` or NUL byte —
/// that is the *historical* grammar, kept permissive so a server can still parse old user IDs
/// already sitting in room state (refs/matrix-spec/content/appendices.md, "Historical User IDs":
/// "clients and servers MUST accept user IDs with localparts consisting of any legal
/// non-surrogate Unicode code points except for `:` and `NUL`"). A server *minting a new* user ID
/// must be stricter: the same file's "User Identifiers" section says a localpart "MUST contain
/// only the characters a-z, 0-9, `.`, `_`, `=`, `-`, `/`, and `+`", and the `/register`
/// `operationId`'s own description adds "the server MUST either map the provided `username` onto
/// a `user_id` in a logical manner, or reject any `username` which does not comply to the
/// grammar with `M_INVALID_USERNAME`" (refs/matrix-spec/data/api/client-server/registration.yaml).
/// `UserId::validate_strict` (`ruma_identifiers_validation::user_id::localpart_is_fully_
/// conforming`) is exactly that stricter check.
///
/// Confirmed against Complement's `refs/complement/tests/csapi/apidoc_register_test.go`:
/// "POST /register rejects usernames with special characters" submits localparts containing
/// `!"\:?\\@[]{}|£é\n'` and expects `400 M_INVALID_USERNAME` for every one of them (before UIA is
/// even attempted — Complement's own comment there: "servers are expected to validate request
/// bodies before handling UIA, so 400 is expected here, not 401"), and "GET /register/available
/// returns M_INVALID_USERNAME for invalid user name" does the same for a bare comma. Before this
/// fix, `validate_localpart` accepted all of the above (none of them are `:` or NUL), so
/// `/register/available` reported `available: true` for shapes `/register` would then also
/// silently accept instead of rejecting — the exact gap
/// `docs/status/14-test-and-conformance.md` recorded for this track.
pub(crate) fn validate_localpart(state: &AuthState, username: &str) -> Result<(), MatrixError> {
    let user_id = UserId::parse_with_server_name(username, state.server_name()).map_err(|_| {
        MatrixError::invalid_username(format!("'{username}' is not a valid user ID localpart"))
    })?;
    user_id.validate_strict().map_err(|_| {
        MatrixError::invalid_username(format!("'{username}' is not a valid user ID localpart"))
    })
}

/// Refuses `username` when an appservice holds it in an exclusive user namespace (`400
/// M_EXCLUSIVE`, the spec's `/register` error for "the desired user ID is in the exclusive
/// namespace claimed by an application service"), unless `appservice_id` is that appservice.
/// Logged at `INFO`: an operator who wonders why a name cannot be had sees which bridge has it.
async fn refuse_exclusive(
    state: &AuthState,
    username: &str,
    appservice_id: Option<&str>,
) -> Result<(), MatrixError> {
    let Ok(user_id) = UserId::parse_with_server_name(username, state.server_name()) else {
        return Ok(());
    };
    match state.appservices.exclusive_user_owner(&user_id).await {
        Some(owner) if Some(owner.as_str()) != appservice_id => {
            tracing::info!(%user_id, appservice = %owner, "refused a registration in an appservice's exclusive namespace");
            Err(MatrixError::new(
                StatusCode::BAD_REQUEST,
                ErrCode::Exclusive,
                format!("{user_id} is reserved by an application service"),
            ))
        }
        _ => Ok(()),
    }
}

fn random_localpart() -> String {
    std::iter::repeat_with(|| rand::rng().sample(Alphanumeric) as char)
        .filter(char::is_ascii_lowercase)
        .take(12)
        .collect()
}

/// `POST /register?kind=user|guest`.
pub async fn post_register(
    State(state): State<AuthState>,
    Query(query): Query<HashMap<String, String>>,
    headers: axum::http::HeaderMap,
    client: hs_http::buckets::ClientIp,
    PermissiveJson(body): PermissiveJson<Value>,
) -> Result<Response, MatrixError> {
    // An appservice's own registration flow, before anything else: it has nothing in common
    // with a person's. Per the application service API's "Registration" section it is
    // authenticated by the `as_token`, takes no user-interactive auth, and is not subject to
    // `enable_registration`, which is about people. The type is a *top-level* `type`, not an
    // `auth` stage, as every bridge sends it; `/register` used to know nothing of it, so a
    // bridge on a server with registration closed -- the default -- could not create its own
    // bot and stopped right there. Found by starting heisenbridge.
    if body.get("type").and_then(Value::as_str) == Some("m.login.application_service") {
        return register_appservice_user(&state, &query, &headers, &body).await;
    }
    let kind = query.get("kind").map(String::as_str).unwrap_or("user");
    if kind != "guest" && kind != "user" {
        return Err(MatrixError::invalid_param(format!(
            "Unknown registration kind '{kind}'"
        )));
    }
    // `rate_limits.registration`, per client address, as Synapse's `rc_registration`: every
    // request is refused while the address's bucket is empty, but only a request that makes an
    // account takes from it -- a person's user-interactive auth takes two or three rounds.
    let address = client.key();
    if let Some(address) = &address {
        state
            .limits
            .registration
            .check_now(address)
            .map_err(MatrixError::limit_exceeded)?;
    }
    let response = if kind == "guest" {
        register_guest(&state, &body).await?
    } else {
        register_user(&state, &body, address.as_deref()).await?
    };
    if response.status() == StatusCode::OK
        && let Some(address) = &address
    {
        // Already checked above; a race with another request from the same address can only
        // have emptied the bucket, which the next request will be refused for.
        let _ = state.limits.registration.take_now(address);
    }
    Ok(response)
}

/// `POST /register` with `type: m.login.application_service`: an appservice creating one of its
/// own users -- its bot, or a ghost in its `users` namespace. See `post_register`.
async fn register_appservice_user(
    state: &AuthState,
    query: &HashMap<String, String>,
    headers: &axum::http::HeaderMap,
    body: &Value,
) -> Result<Response, MatrixError> {
    let bearer = crate::middleware::bearer_token(headers)?;
    let token = match (bearer, query.get("access_token")) {
        (Some(token), _) => token,
        (None, Some(token)) if state.config.get().accept_legacy_query_param_token => token.clone(),
        _ => return Err(MatrixError::missing_token()),
    };
    let Some(appservice) = state.appservices.lookup_by_token(&token).await else {
        return Err(MatrixError::unknown_token(false));
    };

    // The spec's field is `username`; the application service API's older text called it
    // `user`, and Synapse still reads that first, so some bridges still send it.
    let username = body
        .get("username")
        .or_else(|| body.get("user"))
        .and_then(Value::as_str)
        .map(str::to_ascii_lowercase)
        .ok_or_else(|| MatrixError::invalid_param("username is required"))?;
    validate_localpart(state, &username)?;
    let user_id =
        UserId::parse_with_server_name(username.as_str(), state.server_name()).map_err(|_| {
            MatrixError::invalid_username(format!("'{username}' is not a valid user ID localpart"))
        })?;
    if !appservice.can_control(&user_id) {
        return Err(MatrixError::new(
            StatusCode::BAD_REQUEST,
            ErrCode::Exclusive,
            format!(
                "{user_id} is neither appservice {}'s own user nor in its user namespace",
                appservice.appservice_id
            ),
        ));
    }
    if user_id != appservice.sender {
        refuse_exclusive(state, &username, Some(&appservice.appservice_id)).await?;
    }
    if !state.store.is_localpart_available(&username).await? {
        return Err(MatrixError::user_in_use());
    }
    state
        .store
        .create_user({
            let mut r = UserRecord::new(user_id.clone(), state.now_ms());
            r.appservice_id = Some(appservice.appservice_id.clone());
            r
        })
        .await?;

    let inhibit_login = body
        .get("inhibit_login")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    finish_registration(state, &user_id, body, inhibit_login).await
}

/// `POST /register?kind=guest`: a guest account (no password, [`UserRecord::is_guest`]) with a
/// device and an access token, while `auth.allow_guest_access` is on. Refused with
/// `403 M_GUEST_ACCESS_FORBIDDEN` while it is off. What a guest may then call is
/// [`crate::guest`]'s.
async fn register_guest(state: &AuthState, body: &Value) -> Result<Response, MatrixError> {
    if !state.config.get().guest_registration_enabled {
        crate::guest::count_registration(false);
        tracing::debug!("refused a guest registration: auth.allow_guest_access is off");
        return Err(MatrixError::new(
            StatusCode::FORBIDDEN,
            ErrCode::GuestAccessForbidden,
            "Guest access is disabled on this server",
        ));
    }
    let user_id = fresh_user_id(state).await?;
    state
        .store
        .create_user({
            let mut r = UserRecord::new(user_id.clone(), state.now_ms());
            r.is_guest = true;
            r
        })
        .await?;
    crate::guest::count_registration(true);
    tracing::info!(%user_id, "registered a guest account");

    let inhibit_login = body
        .get("inhibit_login")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    finish_registration(state, &user_id, body, inhibit_login).await
}

/// The guest account a registration with `guest_access_token` upgrades: the spec's "Guest
/// Access" module has a guest become a full account by registering as usual with its access
/// token and its own localpart as `username`. Refused unless the token is a live guest's and
/// `username` is that guest's localpart, so one guest cannot take over another's name.
async fn guest_to_upgrade(
    state: &AuthState,
    token: &str,
    username: Option<&str>,
) -> Result<OwnedUserId, MatrixError> {
    let Some(username) = username else {
        return Err(MatrixError::missing_param(
            "username is required to upgrade a guest account: the guest's own localpart",
        ));
    };
    let record = state
        .store
        .get_access_token(&crate::token::TokenHash::of(token))
        .await?
        .filter(|t| t.expires_at_ms.is_none_or(|at| at >= state.now_ms()))
        .ok_or_else(|| MatrixError::unknown_token(false))?;
    let user = state
        .store
        .get_user(&record.user_id)
        .await?
        .filter(|u| !u.deactivated)
        .ok_or_else(|| MatrixError::unknown_token(false))?;
    if !user.is_guest {
        return Err(MatrixError::forbidden(
            "guest_access_token does not belong to a guest account",
        ));
    }
    if user.user_id.localpart() != username {
        return Err(MatrixError::forbidden(
            "A guest account can only be upgraded to its own user ID",
        ));
    }
    Ok(user.user_id)
}

async fn register_user(
    state: &AuthState,
    body: &Value,
    remote_ip: Option<&str>,
) -> Result<Response, MatrixError> {
    let auth: Option<AuthData> = match body.get("auth") {
        Some(v) if !v.is_null() => Some(
            serde_json::from_value(v.clone())
                .map_err(|_| MatrixError::invalid_param("invalid auth data"))?,
        ),
        _ => None,
    };
    let session_id_param = auth.as_ref().and_then(AuthData::session);
    let submitted_type = auth.as_ref().and_then(AuthData::auth_type);
    let now = state.now_ms();
    let timeout = state.config.get().uia_session_timeout_ms;

    // What this session remembers from earlier rounds (see the module docs). An unknown or
    // expired session remembers nothing; `uia::advance` refuses it below.
    let live_session = match session_id_param {
        Some(id) if state.store.session_exists(id, now, timeout).await? => Some(id),
        _ => None,
    };
    let remembered = |key: &'static str| async move {
        match live_session {
            Some(id) => state.store.get_session_data(id, key).await,
            None => Ok(None),
        }
    };
    let stored_params = remembered(PARAMS_KEY).await?;
    let stored_password_hash = remembered(PASSWORD_HASH_KEY)
        .await?
        .and_then(|v| v.as_str().map(str::to_owned));
    let registered_user: Option<OwnedUserId> = remembered(REGISTERED_USER_KEY)
        .await?
        .and_then(|v| v.as_str().and_then(|s| UserId::parse(s).ok()));

    // The parameters in force: this request's, or -- for a round that sends only `auth` -- the
    // ones the session remembers.
    let mut params = body.as_object().cloned().unwrap_or_default();
    params.remove("auth");
    let params_from_request = !params.is_empty();
    if !params_from_request && let Some(Value::Object(stored)) = stored_params {
        params = stored;
    }
    let password_raw = params
        .remove("password")
        .and_then(|v| v.as_str().map(str::to_owned));
    if params_from_request
        && password_raw.is_none()
        && params.remove("initial_device_display_name").is_some()
    {
        // Synapse's workaround for a client that sent `initial_device_display_name` alone on a
        // later round, which would otherwise replace the remembered parameters with nothing but
        // a display name.
        tracing::debug!("ignored initial_device_display_name sent without a password");
    }
    let params_value = Value::Object(params.clone());

    // With open registration off, a registration token is the only way in. A request that
    // presents one, or continues a session that may already hold one, goes on to the token
    // flow; anything else is refused as before, unless a token is out there to be used, in
    // which case the flows are offered so that a client can ask its user for it.
    let token_only = !state.config.get().registration_enabled;
    if token_only {
        let presenting =
            submitted_type == Some(AuthType::RegistrationToken) || session_id_param.is_some();
        if !presenting && !any_token_usable(state).await? {
            return Err(MatrixError::forbidden("Registration is disabled"));
        }
    }

    if let Some(pw) = &password_raw {
        state.config.get().password_policy.validate(pw)?;
    }

    // Per the spec's rationale for excluding upper-case from the user ID grammar
    // (appendices.md, "User Identifiers": "we chose to disallow upper-case characters because we
    // do not consider it valid to have two user IDs which differ only in case... [this] requir[es]
    // homeservers to downcase usernames when creating user IDs for new users"), lower-case the
    // client-supplied `username` *before* validating or checking availability, so
    // "User-UPPER" and "user-upper" register the same account instead of two. Matches Synapse's
    // `RegisterRestServlet.on_POST` (`refs/synapse/synapse/rest/client/register.py`:
    // `desired_username = desired_username.lower()`, applied before `check_username`), and
    // Complement's `apidoc_register_test.go` "POST /register downcases capitals in usernames"
    // (registers `user-UPPER`, expects `user_id: "@user-upper:hs1"`). ASCII-only: the grammar
    // itself is ASCII (`validate_localpart` rejects anything else), so a locale-aware
    // `str::to_lowercase` would only risk surprising non-ASCII casing rules for no benefit.
    let username = params
        .get("username")
        .and_then(Value::as_str)
        .map(str::to_ascii_lowercase);
    if let Some(username) = &username {
        validate_localpart(state, username)?;
        refuse_exclusive(state, username, None).await?;
    }
    // A guest becoming a full account keeps its user ID, which is taken -- by the guest.
    let upgrading = match params.get("guest_access_token").and_then(Value::as_str) {
        Some(token) => Some(guest_to_upgrade(state, token, username.as_deref()).await?),
        None => None,
    };
    // The account this session already made has the name, which is not "in use" to it.
    let already_ours = |name: &str| {
        registered_user
            .as_ref()
            .is_some_and(|u| u.localpart() == name)
    };
    if let Some(username) = &username
        && upgrading.is_none()
        && !already_ours(username)
        && !state.store.is_localpart_available(username).await?
    {
        return Err(MatrixError::user_in_use());
    }

    let flows = registration_flows(state, token_only);

    if let Some(auth_type) = &submitted_type
        && !stage_is_supported(state, auth_type)
    {
        return Err(MatrixError::new(
            StatusCode::BAD_REQUEST,
            ErrCode::Unrecognized,
            format!(
                "The '{}' authentication stage is not supported by this server",
                auth_type.as_ref()
            ),
        ));
    }

    // A dummy stage submitted without its session while one handed out for this username is
    // still open goes back to that session (see the module docs). Only the dummy stage: it
    // proves nothing, so it can only be acknowledging a challenge, and has to name it. A stage
    // that carries its own proof (a registration token, a CAPTCHA answer, a validated email)
    // may still start a session of its own, as Synapse allows -- an invite link's sign-up page
    // asks for the flows and then sends the token without the session.
    if session_id_param.is_none()
        && submitted_type == Some(AuthType::Dummy)
        && upgrading.is_none()
        && let Some(username) = &username
        && let Some(pending) = state
            .pending_registrations
            .session_for(username, now, timeout)
        && state.store.session_exists(&pending, now, timeout).await?
    {
        tracing::info!(
            username = %username,
            "refused a registration stage sent without the session its username was given"
        );
        let completed = state
            .store
            .completed_stages(&pending)
            .await?
            .into_iter()
            .map(AuthType::from)
            .collect();
        let body =
            uia::incomplete_body_with_params(flows, completed, pending, challenge_params(state));
        return Ok((StatusCode::UNAUTHORIZED, Json(body)).into_response());
    }

    // A registration token takes a place on the token for this session, so the session has to
    // exist before the stage is checked: resolve (or create) it first, exactly as `advance`
    // would, and hand `advance` the id.
    let session_id: Option<String> = if matches!(
        submitted_type,
        Some(AuthType::RegistrationToken | AuthType::EmailIdentity)
    ) {
        Some(uia::session_id_for(state.store.as_ref(), session_id_param, now, timeout).await?)
    } else {
        session_id_param.map(str::to_owned)
    };

    let stage_ok = match &auth {
        Some(data) if submitted_type.is_some() => {
            verify_stage(state, data, session_id.as_deref(), remote_ip).await?
        }
        _ => true,
    };

    let outcome = uia::advance(
        state.store.as_ref(),
        &flows,
        session_id.as_deref(),
        submitted_type,
        stage_ok,
        now,
        timeout,
    )
    .await?;
    let session_id = outcome.session_id.clone();
    uia::bind_operation(state.store.as_ref(), &session_id, "POST /register").await?;
    if params_from_request {
        state
            .store
            .set_session_data(&session_id, PARAMS_KEY, params_value.clone())
            .await?;
    }

    if !outcome.complete {
        // The password is hashed once and kept with the session, so a later round need not send
        // it again (and the plain text is never stored).
        if let Some(pw) = &password_raw
            && stored_password_hash.is_none()
        {
            let hash = password::hash_password(pw).map_err(|_| MatrixError::internal())?;
            state
                .store
                .set_session_data(&session_id, PASSWORD_HASH_KEY, json!(hash))
                .await?;
        }
        if let Some(username) = &username
            && upgrading.is_none()
        {
            state
                .pending_registrations
                .remember(username, &session_id, now, timeout);
        }
        let body = uia::incomplete_body_with_params(
            flows,
            outcome.completed,
            session_id,
            challenge_params(state),
        );
        return Ok((StatusCode::UNAUTHORIZED, Json(body)).into_response());
    }
    if let Some(username) = &username {
        state.pending_registrations.forget(username);
    }

    let inhibit_login = params
        .get("inhibit_login")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    if let Some(user_id) = registered_user {
        // This session already made its account: log it in again (Synapse: "Already registered
        // user ID for this session").
        tracing::info!(%user_id, "a registration session finished again; logging its account in");
        return finish_registration(state, &user_id, &params_value, inhibit_login).await;
    }

    let password_hash = match (&password_raw, stored_password_hash) {
        (Some(pw), _) => Some(password::hash_password(pw).map_err(|_| MatrixError::internal())?),
        (None, stored) => stored,
    };

    let user_id = if let Some(user_id) = upgrading {
        state.store.upgrade_guest(&user_id, password_hash).await?;
        crate::guest::count_upgrade();
        tracing::info!(%user_id, "a guest account became a full account");
        user_id
    } else {
        // Re-check availability defensively (closes the TOCTOU window between the early check
        // above and account creation, for two concurrent registrations of the same name).
        let user_id = match &username {
            Some(name) => {
                if !state.store.is_localpart_available(name).await? {
                    return Err(MatrixError::user_in_use());
                }
                UserId::parse_with_server_name(name, state.server_name()).map_err(|_| {
                    MatrixError::invalid_username(format!(
                        "'{name}' is not a valid user ID localpart"
                    ))
                })?
            }
            None => fresh_user_id(state).await?,
        };
        state
            .store
            .create_user({
                let mut r = UserRecord::new(user_id.clone(), state.now_ms());
                r.password_hash = password_hash;
                r
            })
            .await?;
        user_id
    };
    state
        .store
        .set_session_data(&session_id, REGISTERED_USER_KEY, json!(user_id))
        .await?;

    // The account exists: the token this registration presented has been used. A failure to
    // count it is logged rather than failing a registration that has already succeeded.
    if let Ok(Some(Value::String(token))) = state
        .store
        .get_session_data(&session_id, REGISTRATION_TOKEN_KEY)
        .await
        && let Err(error) = state
            .registration_tokens
            .complete(&token, &session_id)
            .await
    {
        tracing::warn!(%error, %user_id, "could not count a registration token's use");
    }

    // The email address the `m.login.email.identity` stage validated is the account's now. The
    // account exists already, so a failure here (somebody bound the address in between) is
    // logged rather than failing the registration.
    if let Ok(Some(record)) = state
        .store
        .get_session_data(&session_id, REGISTRATION_EMAIL_KEY)
        .await
        && let Ok(record) = serde_json::from_value::<crate::store::ThreepidValidationRecord>(record)
        && let Err(error) = crate::threepid::add_validated(state, &user_id, &record).await
    {
        tracing::warn!(%error, %user_id, "could not add the email address a registration validated");
    }

    finish_registration(state, &user_id, &params_value, inhibit_login).await
}

async fn fresh_user_id(state: &AuthState) -> Result<OwnedUserId, MatrixError> {
    for _ in 0..10 {
        let candidate = random_localpart();
        if state.store.is_localpart_available(&candidate).await? {
            return UserId::parse_with_server_name(candidate, state.server_name())
                .map_err(|_| MatrixError::internal());
        }
    }
    Err(MatrixError::internal())
}

async fn finish_registration(
    state: &AuthState,
    user_id: &UserId,
    body: &Value,
    inhibit_login: bool,
) -> Result<Response, MatrixError> {
    if inhibit_login {
        return Ok(Json(json!({"user_id": user_id})).into_response());
    }

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

    let session = session::create_session(
        state,
        user_id,
        device_id,
        initial_device_display_name,
        refresh,
    )
    .await?;

    let mut response = json!({
        "user_id": user_id,
        "access_token": session.access_token,
        "device_id": session.device_id,
    });
    if let Some(rt) = session.refresh_token {
        response["refresh_token"] = json!(rt);
    }
    if let Some(ms) = session.expires_in_ms {
        response["expires_in_ms"] = json!(ms);
    }
    Ok(Json(response).into_response())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::AuthConfig;

    #[tokio::test]
    async fn registration_completes_with_dummy_stage_by_default() {
        let state = AuthState::in_memory();
        let body = json!({"username": "newuser", "password": "hunter22", "auth": {"type": "m.login.dummy"}});
        let response = post_register(
            State(state),
            Query(HashMap::new()),
            axum::http::HeaderMap::new(),
            hs_http::buckets::ClientIp(None),
            PermissiveJson(body),
        )
        .await
        .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn first_call_without_auth_returns_401_with_flows() {
        let state = AuthState::in_memory();
        let body = json!({"username": "newuser2", "password": "hunter22"});
        let response = post_register(
            State(state),
            Query(HashMap::new()),
            axum::http::HeaderMap::new(),
            hs_http::buckets::ClientIp(None),
            PermissiveJson(body),
        )
        .await
        .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: Value = serde_json::from_slice(&bytes).unwrap();
        assert!(json["session"].is_string());
        assert!(json["flows"].is_array());
    }

    #[tokio::test]
    async fn weak_password_is_rejected_before_uia() {
        let mut config = AuthConfig::default();
        config.password_policy.minimum_length = Some(10);
        let state = AuthState::in_memory_with_config(config);
        let body =
            json!({"username": "shortpw", "password": "short", "auth": {"type": "m.login.dummy"}});
        let err = post_register(
            State(state),
            Query(HashMap::new()),
            axum::http::HeaderMap::new(),
            hs_http::buckets::ClientIp(None),
            PermissiveJson(body),
        )
        .await
        .unwrap_err();
        assert_eq!(err.errcode().as_str(), "M_WEAK_PASSWORD");
    }

    #[tokio::test]
    async fn duplicate_username_is_rejected() {
        let state = AuthState::in_memory();
        let body =
            json!({"username": "dupe", "password": "hunter22", "auth": {"type": "m.login.dummy"}});
        let response = post_register(
            State(state.clone()),
            Query(HashMap::new()),
            axum::http::HeaderMap::new(),
            hs_http::buckets::ClientIp(None),
            PermissiveJson(body.clone()),
        )
        .await
        .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let err = post_register(
            State(state),
            Query(HashMap::new()),
            axum::http::HeaderMap::new(),
            hs_http::buckets::ClientIp(None),
            PermissiveJson(body),
        )
        .await
        .unwrap_err();
        assert_eq!(err.errcode().as_str(), "M_USER_IN_USE");
    }

    #[tokio::test]
    async fn registration_token_stage_requires_a_valid_token() {
        let mut config = AuthConfig {
            registration_requires_token: true,
            ..AuthConfig::default()
        };
        config
            .valid_registration_tokens
            .insert("good-token".to_string());
        let state = AuthState::in_memory_with_config(config);

        let body = json!({"username": "tokenuser", "auth": {"type": "m.login.registration_token", "token": "bad-token"}});
        let err = post_register(
            State(state.clone()),
            Query(HashMap::new()),
            axum::http::HeaderMap::new(),
            hs_http::buckets::ClientIp(None),
            PermissiveJson(body),
        )
        .await
        .unwrap_err();
        // Refused, and free to try again with a token that works: see `uia::advance`.
        assert_eq!(err.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(err.errcode(), crate::error::ErrCode::Forbidden);

        let body = json!({"username": "tokenuser", "auth": {"type": "m.login.registration_token", "token": "good-token"}});
        let response = post_register(
            State(state),
            Query(HashMap::new()),
            axum::http::HeaderMap::new(),
            hs_http::buckets::ClientIp(None),
            PermissiveJson(body),
        )
        .await
        .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    fn closed_server() -> AuthState {
        AuthState::in_memory_with_config(AuthConfig {
            registration_enabled: false,
            ..AuthConfig::default()
        })
    }

    async fn register(state: &AuthState, body: Value) -> Result<Response, MatrixError> {
        post_register(
            State(state.clone()),
            Query(HashMap::new()),
            axum::http::HeaderMap::new(),
            hs_http::buckets::ClientIp(None),
            PermissiveJson(body),
        )
        .await
    }

    async fn add_token(state: &AuthState, token: &str, uses: Option<u64>, expires: Option<i64>) {
        state
            .registration_tokens
            .create(crate::registration_tokens::RegistrationTokenRecord::new(
                token.into(),
                uses,
                expires,
                0,
            ))
            .await
            .unwrap();
    }

    async fn validity(state: &AuthState, token: &str) -> bool {
        let mut q = HashMap::new();
        q.insert("token".to_owned(), token.to_owned());
        let Json(v) = get_registration_token_validity(State(state.clone()), Query(q))
            .await
            .unwrap();
        v["valid"].as_bool().unwrap()
    }

    async fn json_body(response: Response) -> Value {
        serde_json::from_slice(
            &axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap(),
        )
        .unwrap()
    }

    async fn challenge(state: &AuthState, body: Value) -> (String, Value) {
        let response = register(state, body).await.unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        let body = json_body(response).await;
        (body["session"].as_str().unwrap().to_owned(), body)
    }

    /// Sytest's "registration remembers parameters" and "registration with inhibit_login
    /// inhibits login": a round that sends only `auth` gets the first round's parameters.
    #[tokio::test]
    async fn a_round_with_only_auth_uses_the_remembered_parameters() {
        let state = AuthState::in_memory();
        let (session, first) = challenge(
            &state,
            json!({
                "username": "remembered",
                "password": "sUp3rs3kr1t",
                "device_id": "xyzzy",
                "initial_device_display_name": "display_name",
            }),
        )
        .await;
        assert_eq!(first["completed"], json!([]));
        let response = register(
            &state,
            json!({"auth": {"type": "m.login.dummy", "session": session}}),
        )
        .await
        .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = json_body(response).await;
        assert_eq!(body["user_id"], "@remembered:example.org");
        assert_eq!(body["device_id"], "xyzzy");
        let user_id = ruma::user_id!("@remembered:example.org");
        let device = state
            .store
            .get_device(user_id, ruma::device_id!("xyzzy"))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(device.display_name.as_deref(), Some("display_name"));
        // The password came from the first round, as a hash.
        let record = state.store.get_user(user_id).await.unwrap().unwrap();
        assert!(
            crate::password::verify_password(
                "sUp3rs3kr1t",
                record.password_hash.as_ref().unwrap(),
                ""
            )
            .unwrap()
        );

        let (session, _) = challenge(
            &state,
            json!({"username": "inhibited", "password": "sUp3rs3kr1t", "inhibit_login": true}),
        )
        .await;
        let body = json_body(
            register(
                &state,
                json!({"auth": {"type": "m.login.dummy", "session": session}}),
            )
            .await
            .unwrap(),
        )
        .await;
        assert_eq!(body["user_id"], "@inhibited:example.org");
        assert!(body.get("access_token").is_none(), "{body}");
        assert!(body.get("device_id").is_none(), "{body}");
    }

    /// Sytest's "registration is idempotent, with/without username specified": the same
    /// session finishing twice logs the one account in twice.
    #[tokio::test]
    async fn finishing_a_session_again_logs_its_account_in_again() {
        let state = AuthState::in_memory();
        for username in [None, Some("idempotent")] {
            let mut first = json!({"password": "sUp3rs3kr1t"});
            if let Some(name) = username {
                first["username"] = json!(name);
            }
            let (session, _) = challenge(&state, first.clone()).await;
            let mut finish = first.clone();
            finish["auth"] = json!({"type": "m.login.dummy", "session": session});
            let one = json_body(register(&state, finish.clone()).await.unwrap()).await;
            let two = json_body(register(&state, finish).await.unwrap()).await;
            assert_eq!(one["user_id"], two["user_id"], "{one} {two}");
            assert!(two["access_token"].is_string());
            assert_ne!(one["access_token"], two["access_token"]);
        }
    }

    /// Complement's "Registration without a session fails": once a username was handed a
    /// session, a stage sent for it without that session is challenged again. A one-shot
    /// registration (no session ever handed out) still completes.
    #[tokio::test]
    async fn a_stage_without_the_session_its_username_was_given_is_challenged() {
        let state = AuthState::in_memory();
        let (session, _) = challenge(
            &state,
            json!({"username": "needs-session", "password": "sUp3rs3kr1t"}),
        )
        .await;
        let response = register(
            &state,
            json!({
                "username": "needs-session",
                "password": "sUp3rs3kr1t",
                "auth": {"type": "m.login.dummy"}
            }),
        )
        .await
        .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(json_body(response).await["session"], session.as_str());
        assert!(
            state
                .store
                .is_localpart_available("needs-session")
                .await
                .unwrap()
        );
        // With the session it goes through.
        let response = register(
            &state,
            json!({
                "username": "needs-session",
                "password": "sUp3rs3kr1t",
                "auth": {"type": "m.login.dummy", "session": session}
            }),
        )
        .await
        .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    /// An invite link's sign-up page asks for the flows, then sends the token without the
    /// session it was handed: the token proves itself, so that registers
    /// (`crates/hs-cli/tests/invites_and_notices.rs`).
    #[tokio::test]
    async fn a_token_stage_without_the_session_still_registers() {
        let state = closed_server();
        add_token(&state, "invite", Some(1), None).await;
        let _ = challenge(
            &state,
            json!({"username": "dana", "password": "hunter2-dana"}),
        )
        .await;
        let response = register(
            &state,
            json!({
                "username": "dana",
                "password": "hunter2-dana",
                "auth": {"type": "m.login.registration_token", "token": "invite"}
            }),
        )
        .await
        .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    /// Stands in for the CAPTCHA service: right when the answer is "right".
    struct FakeCaptcha;

    #[async_trait::async_trait]
    impl crate::recaptcha::RecaptchaVerifier for FakeCaptcha {
        async fn verify(
            &self,
            check: &crate::recaptcha::RecaptchaCheck<'_>,
        ) -> Result<bool, String> {
            assert_eq!(check.private_key, "captcha-secret");
            Ok(check.response == "right")
        }
    }

    fn captcha_server(required: bool) -> AuthState {
        let state = AuthState::in_memory_with_config(AuthConfig {
            recaptcha: Some(crate::config::RecaptchaSettings {
                required,
                public_key: Some("captcha-site".into()),
                private_key: "captcha-secret".into(),
                siteverify_api: "https://captcha.invalid/siteverify".into(),
            }),
            ..AuthConfig::default()
        });
        state.install_recaptcha_verifier(std::sync::Arc::new(FakeCaptcha));
        state
    }

    /// Sytest's "Register with a recaptcha": with keys configured and the CAPTCHA not
    /// required, a client may still complete the stage, and the challenge says so.
    #[tokio::test]
    async fn a_configured_captcha_can_be_completed_even_when_not_required() {
        let state = captcha_server(false);
        let response = register(
            &state,
            json!({
                "username": "captcha",
                "password": "sUp3rs3kr1t",
                "auth": {"type": "m.login.recaptcha", "response": "right"}
            }),
        )
        .await
        .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        let body = json_body(response).await;
        assert_eq!(body["completed"], json!(["m.login.recaptcha"]));
        assert_eq!(body["flows"], json!([{"stages": ["m.login.dummy"]}]));
        assert_eq!(
            body["params"]["m.login.recaptcha"]["public_key"],
            "captcha-site"
        );
    }

    #[tokio::test]
    async fn a_required_captcha_is_in_the_flow_and_a_wrong_answer_fails_it() {
        let state = captcha_server(true);
        let (session, body) = challenge(
            &state,
            json!({"username": "captcha2", "password": "sUp3rs3kr1t"}),
        )
        .await;
        assert_eq!(body["flows"], json!([{"stages": ["m.login.recaptcha"]}]));
        let err = register(
            &state,
            json!({"auth": {"type": "m.login.recaptcha", "response": "wrong", "session": session}}),
        )
        .await
        .unwrap_err();
        assert_eq!(err.status(), StatusCode::UNAUTHORIZED);
        let response = register(
            &state,
            json!({"auth": {"type": "m.login.recaptcha", "response": "right", "session": session}}),
        )
        .await
        .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            json_body(response).await["user_id"],
            "@captcha2:example.org"
        );
    }

    #[tokio::test]
    async fn a_closed_server_with_no_tokens_stays_closed() {
        let state = closed_server();
        let err = register(&state, json!({"username": "x", "password": "hunter22"}))
            .await
            .unwrap_err();
        assert_eq!(err.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn a_token_registers_on_a_closed_server_once_and_is_then_used_up() {
        let state = closed_server();
        add_token(&state, "invite", Some(1), None).await;
        assert!(validity(&state, "invite").await);
        assert!(!validity(&state, "unknown").await);

        // Presenting nothing is offered the token flow, since a token is out there.
        let response = register(&state, json!({"username": "first", "password": "hunter22"}))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        let body = json_body(response).await;
        assert_eq!(
            body["flows"][0]["stages"],
            json!(["m.login.registration_token"])
        );
        let session = body["session"].as_str().unwrap().to_owned();
        // A dummy stage does not satisfy it.
        let response = register(
            &state,
            json!({
                "username": "first",
                "password": "hunter22",
                "auth": {"type": "m.login.dummy", "session": session}
            }),
        )
        .await
        .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

        // The token does.
        let response = register(
            &state,
            json!({
                "username": "first",
                "password": "hunter22",
                "auth": {"type": "m.login.registration_token", "token": "invite", "session": session}
            }),
        )
        .await
        .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let row = state
            .registration_tokens
            .get("invite")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.completed, 1);
        assert!(row.pending.is_empty());
        assert!(!validity(&state, "invite").await);

        // Used up: a second person is refused at the stage.
        let err = register(
            &state,
            json!({
                "username": "second",
                "password": "hunter22",
                "auth": {"type": "m.login.registration_token", "token": "invite"}
            }),
        )
        .await
        .unwrap_err();
        assert_eq!(err.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(err.errcode(), crate::error::ErrCode::Forbidden);
        // And with no usable token left, the server is closed again.
        let err = register(&state, json!({"username": "third", "password": "hunter22"}))
            .await
            .unwrap_err();
        assert_eq!(err.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn a_presented_token_holds_its_place_until_the_registration_finishes() {
        let state = AuthState::in_memory_with_config(AuthConfig {
            registration_enabled: false,
            terms_enabled: true,
            ..AuthConfig::default()
        });
        add_token(&state, "one-place", Some(1), None).await;

        // Stage one: the token. Terms are still owed, so the account does not exist yet, but the
        // token's one place is taken.
        let response = register(
            &state,
            json!({
                "username": "alice",
                "password": "hunter22",
                "auth": {"type": "m.login.registration_token", "token": "one-place"}
            }),
        )
        .await
        .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        let body = json_body(response).await;
        let session = body["session"].as_str().unwrap().to_owned();
        assert!(!validity(&state, "one-place").await);
        let err = register(
            &state,
            json!({
                "username": "mallory",
                "password": "hunter22",
                "auth": {"type": "m.login.registration_token", "token": "one-place"}
            }),
        )
        .await
        .unwrap_err();
        assert_eq!(err.status(), StatusCode::UNAUTHORIZED);

        // Stage two finishes the registration and counts the use.
        let response = register(
            &state,
            json!({
                "username": "alice",
                "password": "hunter22",
                "auth": {"type": "m.login.terms", "session": session}
            }),
        )
        .await
        .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let row = state
            .registration_tokens
            .get("one-place")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.completed, 1);
        assert!(row.pending.is_empty());
    }

    #[tokio::test]
    async fn an_expired_token_is_invalid_and_refused() {
        let state = closed_server();
        add_token(&state, "old", None, Some(1)).await;
        assert!(!validity(&state, "old").await);
        let err = register(
            &state,
            json!({
                "username": "late",
                "password": "hunter22",
                "auth": {"type": "m.login.registration_token", "token": "old"}
            }),
        )
        .await
        .unwrap_err();
        assert_eq!(err.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn validity_needs_the_token_parameter() {
        let err = get_registration_token_validity(State(closed_server()), Query(HashMap::new()))
            .await
            .unwrap_err();
        assert_eq!(err.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn recaptcha_stage_fails_cleanly_when_submitted() {
        let state = AuthState::in_memory();
        let body = json!({"username": "recaptchauser", "auth": {"type": "m.login.recaptcha", "response": "x"}});
        let err = post_register(
            State(state),
            Query(HashMap::new()),
            axum::http::HeaderMap::new(),
            hs_http::buckets::ClientIp(None),
            PermissiveJson(body),
        )
        .await
        .unwrap_err();
        assert_eq!(err.errcode().as_str(), "M_UNRECOGNIZED");
    }

    #[tokio::test]
    async fn inhibit_login_skips_token_issuance() {
        let state = AuthState::in_memory();
        let body = json!({
            "username": "noauto",
            "password": "hunter22",
            "inhibit_login": true,
            "auth": {"type": "m.login.dummy"}
        });
        let response = post_register(
            State(state),
            Query(HashMap::new()),
            axum::http::HeaderMap::new(),
            hs_http::buckets::ClientIp(None),
            PermissiveJson(body),
        )
        .await
        .unwrap();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: Value = serde_json::from_slice(&bytes).unwrap();
        assert!(json.get("access_token").is_none());
        assert_eq!(json["user_id"], "@noauto:example.org");
    }

    #[tokio::test]
    async fn guest_registration_respects_config_flag() {
        let state = AuthState::in_memory(); // guest_registration_enabled: false by default
        let mut query = HashMap::new();
        query.insert("kind".to_string(), "guest".to_string());
        let err = post_register(
            State(state),
            Query(query),
            axum::http::HeaderMap::new(),
            hs_http::buckets::ClientIp(None),
            PermissiveJson(json!({})),
        )
        .await
        .unwrap_err();
        assert_eq!(err.status(), StatusCode::FORBIDDEN);
        assert_eq!(err.errcode().as_str(), "M_GUEST_ACCESS_FORBIDDEN");
    }

    async fn register_with(state: &AuthState, kind: Option<&str>, body: Value) -> Response {
        let mut query = HashMap::new();
        if let Some(kind) = kind {
            query.insert("kind".to_string(), kind.to_string());
        }
        match post_register(
            State(state.clone()),
            Query(query),
            axum::http::HeaderMap::new(),
            hs_http::buckets::ClientIp(None),
            PermissiveJson(body),
        )
        .await
        {
            Ok(response) => response,
            Err(error) => error.into_response(),
        }
    }

    fn guest_state() -> AuthState {
        AuthState::in_memory_with_config(AuthConfig {
            guest_registration_enabled: true,
            ..AuthConfig::default()
        })
    }

    #[tokio::test]
    async fn guest_registration_succeeds_when_enabled() {
        let state = guest_state();
        let response = register_with(&state, Some("guest"), json!({})).await;
        assert_eq!(response.status(), StatusCode::OK);
        let body = body_json(response).await;
        let user_id = ruma::UserId::parse(body["user_id"].as_str().unwrap()).unwrap();
        assert_eq!(user_id.server_name(), "example.org");
        assert!(body["access_token"].is_string());
        assert!(body["device_id"].is_string());
        let record = state.store.get_user(&user_id).await.unwrap().unwrap();
        assert!(record.is_guest);
        assert!(record.password_hash.is_none());
    }

    #[tokio::test]
    async fn a_guest_upgrades_to_a_full_account_with_its_own_localpart() {
        let state = guest_state();
        let guest = body_json(register_with(&state, Some("guest"), json!({})).await).await;
        let user_id = ruma::UserId::parse(guest["user_id"].as_str().unwrap()).unwrap();
        let body = json!({
            "username": user_id.localpart(),
            "password": "SIR_Arthur_David",
            "guest_access_token": guest["access_token"],
            "auth": {"type": "m.login.dummy"},
        });
        let response = register_with(&state, None, body).await;
        assert_eq!(response.status(), StatusCode::OK);
        let upgraded = body_json(response).await;
        assert_eq!(upgraded["user_id"], guest["user_id"]);
        assert!(upgraded["access_token"].is_string());
        let record = state.store.get_user(&user_id).await.unwrap().unwrap();
        assert!(!record.is_guest);
        assert!(record.password_hash.is_some());
    }

    #[tokio::test]
    async fn a_guest_cannot_upgrade_another_guest_or_a_full_account() {
        let state = guest_state();
        let first = body_json(register_with(&state, Some("guest"), json!({})).await).await;
        let second = body_json(register_with(&state, Some("guest"), json!({})).await).await;
        let first_id = ruma::UserId::parse(first["user_id"].as_str().unwrap()).unwrap();
        let body = json!({
            "username": first_id.localpart(),
            "password": "SIR_Arthur_David",
            "guest_access_token": second["access_token"],
            "auth": {"type": "m.login.dummy"},
        });
        let response = register_with(&state, None, body).await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        assert!(
            state
                .store
                .get_user(&first_id)
                .await
                .unwrap()
                .unwrap()
                .is_guest
        );

        // Without the token, the guest's name is taken like any other.
        let body = json!({
            "username": first_id.localpart(),
            "password": "SIR_Arthur_David",
            "auth": {"type": "m.login.dummy"},
        });
        let response = register_with(&state, None, body).await;
        assert_eq!(body_json(response).await["errcode"], "M_USER_IN_USE");
    }

    #[tokio::test]
    async fn register_available_reports_taken_and_free_names() {
        let state = AuthState::in_memory();
        let body =
            json!({"username": "taken", "password": "hunter22", "auth": {"type": "m.login.dummy"}});
        post_register(
            State(state.clone()),
            Query(HashMap::new()),
            axum::http::HeaderMap::new(),
            hs_http::buckets::ClientIp(None),
            PermissiveJson(body),
        )
        .await
        .unwrap();

        let mut query = HashMap::new();
        query.insert("username".to_string(), "taken".to_string());
        let err = get_register_available(State(state.clone()), Query(query))
            .await
            .unwrap_err();
        assert_eq!(err.errcode().as_str(), "M_USER_IN_USE");

        let mut query = HashMap::new();
        query.insert("username".to_string(), "free".to_string());
        let Json(body) = get_register_available(State(state), Query(query))
            .await
            .unwrap();
        assert_eq!(body["available"], true);
    }

    /// `refs/complement/tests/csapi/apidoc_register_test.go`: "GET /register/available returns
    /// M_INVALID_USERNAME for invalid user name" uses `"username,should_not_be_valid"` (a comma is
    /// legal ASCII, not `:` or NUL, so the old lenient check accepted it and reported
    /// `available: true`; want `400 M_INVALID_USERNAME`).
    #[tokio::test]
    async fn register_available_rejects_an_invalid_username_shape() {
        let state = AuthState::in_memory();
        let mut query = HashMap::new();
        query.insert(
            "username".to_string(),
            "username,should_not_be_valid".to_string(),
        );
        let err = get_register_available(State(state), Query(query))
            .await
            .unwrap_err();
        assert_eq!(err.status(), StatusCode::BAD_REQUEST);
        assert_eq!(err.errcode().as_str(), "M_INVALID_USERNAME");
    }

    /// `refs/complement/tests/csapi/apidoc_register_test.go`: "POST /register rejects usernames
    /// with special characters" — every one of these localparts must 400 with `M_INVALID_USERNAME`
    /// *before* UIA runs (Complement's own comment: "servers are expected to validate request
    /// bodies before handling UIA, so 400 is expected here, not 401"). None of these characters are
    /// `:` or NUL, so the old `UserId::parse_with_server_name`-only check accepted all of them.
    #[tokio::test]
    async fn register_rejects_usernames_with_special_characters() {
        let state = AuthState::in_memory();
        for ch in [
            "!", "\"", ":", "?", "\\", "@", "[", "]", "{", "|", "}", "£", "é", "\n", "'",
        ] {
            let body = json!({
                "username": format!("user-{ch}-reject-please"),
                "password": "sUp3rs3kr1t",
            });
            let err = post_register(
                State(state.clone()),
                Query(HashMap::new()),
                axum::http::HeaderMap::new(),
                hs_http::buckets::ClientIp(None),
                PermissiveJson(body),
            )
            .await
            .unwrap_err();
            assert_eq!(err.status(), StatusCode::BAD_REQUEST, "char {ch:?}");
            assert_eq!(err.errcode().as_str(), "M_INVALID_USERNAME", "char {ch:?}");
        }
    }

    /// `refs/complement/tests/csapi/apidoc_register_test.go`: "POST /register downcases capitals
    /// in usernames" — registering `user-UPPER` must succeed with `user_id: "@user-upper:..."`,
    /// not store the localpart verbatim (which would let `@user-UPPER:...` and a later
    /// `@user-upper:...` registration coexist as two accounts a client can't tell apart, per the
    /// spec's user-ID-grammar rationale).
    #[tokio::test]
    async fn register_downcases_uppercase_usernames() {
        let state = AuthState::in_memory();
        let body = json!({
            "username": "user-UPPER",
            "password": "sUp3rs3kr1t",
            "auth": {"type": "m.login.dummy"}
        });
        let response = post_register(
            State(state),
            Query(HashMap::new()),
            axum::http::HeaderMap::new(),
            hs_http::buckets::ClientIp(None),
            PermissiveJson(body),
        )
        .await
        .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["user_id"], "@user-upper:example.org");
    }

    /// The reverse direction of the above: once `user-upper` is lower-cased and stored, a second
    /// registration attempt spelled with different capitalization must collide with it rather than
    /// creating a second account — the exact "two accounts a client considers the same" failure
    /// mode this fix closes.
    #[tokio::test]
    async fn register_treats_different_capitalizations_as_the_same_username() {
        let state = AuthState::in_memory();
        let first = json!({
            "username": "CaseCollide",
            "password": "sUp3rs3kr1t",
            "auth": {"type": "m.login.dummy"}
        });
        let response = post_register(
            State(state.clone()),
            Query(HashMap::new()),
            axum::http::HeaderMap::new(),
            hs_http::buckets::ClientIp(None),
            PermissiveJson(first),
        )
        .await
        .unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let second = json!({
            "username": "casecollide",
            "password": "sUp3rs3kr1t",
            "auth": {"type": "m.login.dummy"}
        });
        let err = post_register(
            State(state),
            Query(HashMap::new()),
            axum::http::HeaderMap::new(),
            hs_http::buckets::ClientIp(None),
            PermissiveJson(second),
        )
        .await
        .unwrap_err();
        assert_eq!(err.errcode().as_str(), "M_USER_IN_USE");
    }

    /// Registration in a single round trip -- `username`/`password`/`auth: {"type":
    /// "m.login.dummy"}` sent all at once, no prior call to fetch the session -- must keep
    /// working. Complement's `apidoc_register_test.go` "Registration without a session fails"
    /// wants a stricter rule that would break exactly this pattern; `refs/synapse/synapse/
    /// handlers/auth.py::AuthHandler.check_ui_auth` confirms real Synapse allows it (mints a
    /// fresh session and completes the stage on it in the same call whenever `session` is
    /// absent), and that Complement test itself skips Synapse, Dendrite and Conduit for the same
    /// reason. See `uia::advance`'s doc comment and `docs/status/07-auth-and-identity.md` for the
    /// full read.
    #[tokio::test]
    async fn registration_completes_in_a_single_round_trip_with_no_prior_session() {
        let state = AuthState::in_memory();
        let body = json!({
            "username": "single-round-trip",
            "password": "sUp3rs3kr1t",
            "auth": {"type": "m.login.dummy"}
        });
        let response = post_register(
            State(state),
            Query(HashMap::new()),
            axum::http::HeaderMap::new(),
            hs_http::buckets::ClientIp(None),
            PermissiveJson(body),
        )
        .await
        .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    // ---------------------------------------------------------------------------------------
    // An appservice registering its own users.
    // ---------------------------------------------------------------------------------------

    /// A server with registration closed -- the default -- and one bridge registered.
    fn closed_server_with_a_bridge() -> AuthState {
        let state = AuthState::in_memory();
        let registry = crate::appservice::InMemoryAppserviceRegistry::new();
        registry.insert(
            "as_secret",
            crate::appservice::AppserviceRecord::new(
                "irc",
                ruma::user_id!("@ircbot:example.org").to_owned(),
                vec![crate::appservice::NamespaceRule {
                    regex: regex::Regex::new(r"^@irc_.*:example\.org$").unwrap(),
                    exclusive: true,
                }],
            ),
        );
        AuthState {
            appservices: std::sync::Arc::new(registry),
            config: hs_config::Live::new(AuthConfig {
                registration_enabled: false,
                ..AuthConfig::default()
            }),
            ..state
        }
    }

    async fn body_json(response: Response) -> Value {
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    fn bearer(token: &str) -> axum::http::HeaderMap {
        let mut headers = axum::http::HeaderMap::new();
        headers.insert(
            axum::http::header::AUTHORIZATION,
            format!("Bearer {token}").parse().unwrap(),
        );
        headers
    }

    /// Sytest's "Regular users cannot register within the AS namespace": a person may not take
    /// a user ID a bridge holds exclusively, from `/register` or `/register/available`
    /// (`400 M_EXCLUSIVE`); the bridge itself still may.
    #[tokio::test]
    async fn a_person_cannot_register_in_an_appservices_exclusive_namespace() {
        let state = closed_server_with_a_bridge();
        let state = AuthState {
            config: hs_config::Live::new(AuthConfig::default()),
            ..state
        };
        let err = register(
            &state,
            json!({"username": "irc_carol", "password": "a-long-enough-password", "auth": {"type": "m.login.dummy"}}),
        )
        .await
        .unwrap_err();
        assert_eq!(err.errcode(), ErrCode::Exclusive, "{err:?}");
        assert_eq!(err.status(), StatusCode::BAD_REQUEST);

        let mut query = HashMap::new();
        query.insert("username".to_owned(), "irc_carol".to_owned());
        let err = get_register_available(State(state.clone()), Query(query))
            .await
            .unwrap_err();
        assert_eq!(err.errcode(), ErrCode::Exclusive, "{err:?}");

        // Outside the namespace nothing changes.
        register(
            &state,
            json!({"username": "carol", "password": "a-long-enough-password", "auth": {"type": "m.login.dummy"}}),
        )
        .await
        .unwrap();

        // The bridge registers its own ghost there.
        let body = json!({"type": "m.login.application_service", "username": "irc_carol"});
        let response = post_register(
            State(state),
            Query(HashMap::new()),
            bearer("as_secret"),
            hs_http::buckets::ClientIp(None),
            PermissiveJson(body),
        )
        .await
        .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    /// What heisenbridge sends first, byte for byte, and got "Registration is disabled" for:
    /// the bot registering itself, with `inhibit_login`, on a server whose registration is
    /// closed. It is the appservice's own flow, and that setting is about people.
    #[tokio::test]
    async fn an_appservice_registers_its_bot_with_registration_closed_and_no_uia() {
        let state = closed_server_with_a_bridge();
        let body = json!({"type": "m.login.application_service", "username": "heisenbridge", "inhibit_login": true});
        // Not its bot and not in its namespace.
        let err = post_register(
            State(state.clone()),
            Query(HashMap::new()),
            bearer("as_secret"),
            hs_http::buckets::ClientIp(None),
            PermissiveJson(body),
        )
        .await
        .unwrap_err();
        assert_eq!(err.errcode(), ErrCode::Exclusive, "{err:?}");

        let body = json!({"type": "m.login.application_service", "username": "ircbot", "inhibit_login": true});
        let response = post_register(
            State(state.clone()),
            Query(HashMap::new()),
            bearer("as_secret"),
            hs_http::buckets::ClientIp(None),
            PermissiveJson(body),
        )
        .await
        .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let json = body_json(response).await;
        assert_eq!(json["user_id"], "@ircbot:example.org");
        assert!(json.get("access_token").is_none(), "inhibit_login: {json}");
        let record = state
            .store
            .get_user(ruma::user_id!("@ircbot:example.org"))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(record.appservice_id.as_deref(), Some("irc"));

        // A ghost, with a session this time.
        let body = json!({"type": "m.login.application_service", "username": "irc_alice"});
        let response = post_register(
            State(state.clone()),
            Query(HashMap::new()),
            bearer("as_secret"),
            hs_http::buckets::ClientIp(None),
            PermissiveJson(body),
        )
        .await
        .unwrap();
        let json = body_json(response).await;
        assert_eq!(json["user_id"], "@irc_alice:example.org");
        assert!(json["access_token"].is_string(), "{json}");

        // Twice is a conflict, which every bridge expects and treats as "already done".
        let body = json!({"type": "m.login.application_service", "username": "ircbot"});
        let err = post_register(
            State(state.clone()),
            Query(HashMap::new()),
            bearer("as_secret"),
            hs_http::buckets::ClientIp(None),
            PermissiveJson(body),
        )
        .await
        .unwrap_err();
        assert_eq!(err.errcode(), ErrCode::UserInUse, "{err:?}");

        // Without a token it is nobody's flow; with somebody else's token, no better.
        let body = json!({"type": "m.login.application_service", "username": "irc_bob"});
        let err = post_register(
            State(state.clone()),
            Query(HashMap::new()),
            axum::http::HeaderMap::new(),
            hs_http::buckets::ClientIp(None),
            PermissiveJson(body.clone()),
        )
        .await
        .unwrap_err();
        assert_eq!(err.errcode(), ErrCode::MissingToken, "{err:?}");
        let err = post_register(
            State(state),
            Query(HashMap::new()),
            bearer("not_an_as_token"),
            hs_http::buckets::ClientIp(None),
            PermissiveJson(body),
        )
        .await
        .unwrap_err();
        assert_eq!(err.errcode(), ErrCode::UnknownToken, "{err:?}");
    }
}
