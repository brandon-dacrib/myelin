//! Single sign-on through CAS ([`crate::cas`] has the flow):
//!
//! - `GET /login/sso/redirect`, `GET /login/sso/redirect/{idpId}` (`idpId` `cas`) and the older
//!   `GET /login/cas/redirect`: `302` to the CAS server's sign-in page.
//! - `GET /login/cas/ticket`: where CAS sends the person back. With `redirectUrl` it signs them
//!   in (creating the account at a first sign-in) and shows a page linking back to the client
//!   with a login token; with `session` it passes user-interactive auth's `m.login.sso` stage.
//! - `GET /auth/m.login.sso/fallback/web?session=`: the spec's fallback for that stage, a
//!   `302` to the CAS sign-in page whose return address carries the session.
//!
//! The ticket endpoint answers HTML, never JSON: it is a page in the person's browser. Its
//! success page is `200` (Synapse's `sso_redirect_confirm.html` and `sso_auth_success.html`
//! are too; Sytest reads the login token out of the body) and asks the person to continue,
//! rather than sending them on with a `302`, so that a link somebody else crafted cannot sign a
//! person in to an application they never meant to use without them seeing where they are
//! going.

use std::collections::HashMap;

use axum::extract::{Path, Query, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use quick_xml::escape::escape;
use ruma::{OwnedUserId, UserId};
use serde_json::json;

use crate::cas::{self, CasError, CasResponse};
use crate::config::CasSettings;
use crate::error::{ErrCode, MatrixError};
use crate::state::AuthState;
use crate::store::{ExternalIdRecord, LoginTokenRecord, StoreError, UserRecord};
use crate::token::{TokenHash, generate_login_token};
use crate::uia;

fn not_configured() -> MatrixError {
    MatrixError::new(
        StatusCode::BAD_REQUEST,
        ErrCode::Unrecognized,
        "This server has no single sign-on provider configured",
    )
}

fn cas_settings(state: &AuthState) -> Result<CasSettings, MatrixError> {
    state.config.get().cas.clone().ok_or_else(not_configured)
}

fn redirect(location: &str) -> Response {
    match HeaderValue::from_str(location) {
        Ok(value) => (StatusCode::FOUND, [(header::LOCATION, value)]).into_response(),
        Err(_) => MatrixError::internal().into_response(),
    }
}

fn no_service_base(error: &CasError) -> MatrixError {
    tracing::warn!(%error, "cannot start a CAS sign-in");
    MatrixError::new(
        StatusCode::INTERNAL_SERVER_ERROR,
        ErrCode::Unknown,
        error.to_string(),
    )
}

/// `GET /login/sso/redirect?redirectUrl=`: sends the person to the CAS server to sign in.
///
/// # Errors
/// `400 M_UNRECOGNIZED` without CAS configured, `400 M_MISSING_PARAM` without `redirectUrl`,
/// `500` without an address CAS could send the person back to.
pub async fn get_sso_redirect(
    State(state): State<AuthState>,
    Query(query): Query<HashMap<String, String>>,
) -> Result<Response, MatrixError> {
    let cas = cas_settings(&state)?;
    let redirect_url = query
        .get("redirectUrl")
        .filter(|u| !u.is_empty())
        .ok_or_else(|| MatrixError::missing_param("Missing redirectUrl"))?;
    let service = cas::service_url(&state.config.get(), &cas, ("redirectUrl", redirect_url))
        .map_err(|e| no_service_base(&e))?;
    Ok(redirect(&cas::login_url(&cas, &service)))
}

/// `GET /login/sso/redirect/{idpId}`: [`get_sso_redirect`] for a named provider; only
/// [`cas::PROVIDER`] exists.
///
/// # Errors
/// `404 M_NOT_FOUND` for another provider; otherwise as [`get_sso_redirect`].
pub async fn get_sso_redirect_idp(
    State(state): State<AuthState>,
    Path(idp_id): Path<String>,
    query: Query<HashMap<String, String>>,
) -> Result<Response, MatrixError> {
    if state.config.get().cas.is_none() {
        return Err(not_configured());
    }
    if idp_id != cas::PROVIDER {
        return Err(MatrixError::not_found(format!(
            "No identity provider called {idp_id}"
        )));
    }
    get_sso_redirect(State(state), query).await
}

/// `GET /auth/m.login.sso/fallback/web?session=`: the user-interactive auth fallback for the
/// `m.login.sso` stage. Sends the person to CAS with a return address that names the session.
///
/// # Errors
/// As [`get_sso_redirect`], and `400` for a missing or unknown session.
pub async fn get_sso_fallback(
    State(state): State<AuthState>,
    Query(query): Query<HashMap<String, String>>,
) -> Result<Response, MatrixError> {
    let cas = cas_settings(&state)?;
    let session = query
        .get("session")
        .ok_or_else(|| MatrixError::missing_param("Missing session"))?;
    let config = state.config.get();
    uia::session_id_for(
        state.store.as_ref(),
        Some(session),
        state.now_ms(),
        config.uia_session_timeout_ms,
    )
    .await?;
    let service =
        cas::service_url(&config, &cas, ("session", session)).map_err(|e| no_service_base(&e))?;
    Ok(redirect(&cas::login_url(&cas, &service)))
}

/// An HTML page with `status`.
fn page(status: StatusCode, title: &str, body_html: &str) -> Response {
    let html = format!(
        "<!DOCTYPE html>\n<html lang=\"en\">\n<head>\n<meta charset=\"utf-8\">\n\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n\
         <title>{title}</title>\n<style>body{{font-family:system-ui,sans-serif;max-width:32rem;\
         margin:3rem auto;padding:0 1rem;line-height:1.5}}a.button{{display:inline-block;\
         padding:.6rem 1.2rem;border-radius:.4rem;background:#0b6bcb;color:#fff;\
         text-decoration:none}}</style>\n</head>\n<body>\n<h1>{title}</h1>\n{body_html}\n\
         </body>\n</html>\n",
        title = escape(title),
    );
    (
        status,
        [
            (header::CONTENT_TYPE, "text/html; charset=utf-8"),
            (header::CACHE_CONTROL, "no-store"),
            (header::X_FRAME_OPTIONS, "DENY"),
            (
                header::CONTENT_SECURITY_POLICY,
                "default-src 'none'; style-src 'unsafe-inline'; frame-ancestors 'none'",
            ),
        ],
        html,
    )
        .into_response()
}

fn error_page(status: StatusCode, message: &str) -> Response {
    page(
        status,
        "Sign-in failed",
        &format!("<p>{}</p>", escape(message)),
    )
}

/// `client` with `loginToken=<token>` added to its query, before any fragment.
fn with_login_token(client: &str, token: &str) -> String {
    let (base, fragment) = match client.split_once('#') {
        Some((base, fragment)) => (base, Some(fragment)),
        None => (client, None),
    };
    let separator = if base.contains('?') { '&' } else { '?' };
    let mut out = format!(
        "{base}{separator}loginToken={}",
        cas::encode_component(token)
    );
    if let Some(fragment) = fragment {
        out.push('#');
        out.push_str(fragment);
    }
    out
}

/// The host part of a client address, to show the person where they are going.
fn host_of(url: &str) -> &str {
    let rest = url.split_once("://").map_or(url, |(_, rest)| rest);
    rest.split(['/', '?', '#']).next().unwrap_or(rest)
}

/// `GET /login/cas/ticket?ticket=..&redirectUrl=..|session=..`: where CAS sends the person back.
/// See the module docs.
pub async fn get_cas_ticket(
    State(state): State<AuthState>,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    let Some(cas) = state.config.get().cas.clone() else {
        return not_configured().into_response();
    };
    let Some(ticket) = query.get("ticket").filter(|t| !t.is_empty()) else {
        return error_page(
            StatusCode::BAD_REQUEST,
            "The sign-in service sent you back without a ticket.",
        );
    };
    if let Some(session) = query.get("session") {
        return ui_auth_ticket(&state, &cas, ticket, session).await;
    }
    let Some(client) = query.get("redirectUrl").filter(|u| !u.is_empty()) else {
        return error_page(
            StatusCode::BAD_REQUEST,
            "The sign-in service sent you back without saying where to go next.",
        );
    };
    login_ticket(&state, &cas, ticket, client).await
}

/// Checks `ticket` against the CAS server for the service address `arg` names.
async fn validate(
    state: &AuthState,
    cas: &CasSettings,
    ticket: &str,
    arg: (&str, &str),
) -> Result<CasResponse, Box<Response>> {
    let service = cas::service_url(&state.config.get(), cas, arg).map_err(|e| {
        tracing::warn!(error = %e, "cannot finish a CAS sign-in");
        Box::new(error_page(
            StatusCode::INTERNAL_SERVER_ERROR,
            &e.to_string(),
        ))
    })?;
    let checked = match state
        .cas_validator
        .validate(&cas::validate_url(cas), ticket, &service)
        .await
    {
        Ok(body) => cas::parse_response(&body).map(|mut response| {
            cas::prefix_numeric_user(&mut response, cas);
            response
        }),
        Err(e) => Err(e),
    };
    let response = checked.map_err(|e| {
        cas::count("failed");
        tracing::warn!(error = %e, server = %cas.server_url, "a CAS ticket was not accepted");
        let status = match e {
            CasError::Rejected(_) => StatusCode::FORBIDDEN,
            _ => StatusCode::BAD_GATEWAY,
        };
        Box::new(error_page(status, &e.to_string()))
    })?;
    if !cas::meets_requirements(&response, &cas.required_attributes) {
        cas::count("failed");
        tracing::info!(
            cas_user = %response.user,
            "refused a CAS sign-in: the account lacks an attribute auth.cas.required_attributes asks for"
        );
        return Err(Box::new(error_page(
            StatusCode::FORBIDDEN,
            "Your account with the sign-in service is not allowed to use this server.",
        )));
    }
    Ok(response)
}

/// Which local account a CAS sign-in is for.
enum CasAccount {
    /// The account linked to this CAS user at an earlier sign-in.
    Linked(OwnedUserId),
    /// An account that existed before CAS was turned on, whose localpart the CAS user name maps
    /// onto (Synapse's grandfathering of existing users).
    Existing(OwnedUserId),
    /// Nobody yet: a first sign-in makes this account.
    New(OwnedUserId),
    /// The CAS user name maps onto no valid user ID.
    Unusable,
}

/// The local account `response` names: see [`CasAccount`].
async fn existing_account(
    state: &AuthState,
    response: &CasResponse,
) -> Result<CasAccount, StoreError> {
    if let Some(user_id) = state
        .store
        .get_user_by_external_id(cas::PROVIDER, &response.user)
        .await?
    {
        return Ok(CasAccount::Linked(user_id));
    }
    let localpart = cas::map_username_to_localpart(&response.user);
    let Ok(user_id) = UserId::parse_with_server_name(localpart.as_str(), state.server_name())
    else {
        return Ok(CasAccount::Unusable);
    };
    if user_id.validate_strict().is_err() {
        return Ok(CasAccount::Unusable);
    }
    if state.store.get_user(&user_id).await?.is_some() {
        Ok(CasAccount::Existing(user_id))
    } else {
        Ok(CasAccount::New(user_id))
    }
}

/// Refuses a CAS sign-in that would make, or take over, an account in an application service's
/// exclusive user namespace (a bridge's `@irc_.*`): the same `M_EXCLUSIVE` rule `/register`
/// applies ([`crate::routes::register::refuse_exclusive`], which logs which appservice holds the
/// name), as Synapse's SSO registration checks it through `check_username`.
async fn refuse_exclusive(state: &AuthState, user_id: &UserId) -> Result<(), Box<Response>> {
    match crate::routes::register::refuse_exclusive(state, user_id.localpart(), None).await {
        Ok(()) => Ok(()),
        Err(error) if error.status() == StatusCode::BAD_REQUEST => {
            cas::count("failed");
            Err(Box::new(error_page(
                StatusCode::FORBIDDEN,
                "Your user name at the sign-in service is reserved here for a bridge or another \
                 application service, so it cannot be used to sign in. Ask this server's \
                 administrator.",
            )))
        }
        Err(error) => {
            tracing::warn!(%error, %user_id, "could not check a CAS sign-in against the appservice namespaces");
            Err(Box::new(error_page(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Something went wrong.",
            )))
        }
    }
}

async fn link(state: &AuthState, user_id: &UserId, response: &CasResponse) {
    let record = ExternalIdRecord {
        user_id: user_id.to_owned(),
        provider: cas::PROVIDER.to_owned(),
        external_id: response.user.clone(),
        added_at_ms: state.now_ms(),
    };
    if let Err(error) = state.store.add_external_id(record).await {
        tracing::warn!(%error, %user_id, "could not link a CAS account to its local account");
    }
}

async fn login_ticket(
    state: &AuthState,
    cas: &CasSettings,
    ticket: &str,
    client: &str,
) -> Response {
    let response = match validate(state, cas, ticket, ("redirectUrl", client)).await {
        Ok(response) => response,
        Err(page) => return *page,
    };
    let internal = || error_page(StatusCode::INTERNAL_SERVER_ERROR, "Something went wrong.");
    let account = match existing_account(state, &response).await {
        Ok(found) => found,
        Err(error) => {
            tracing::warn!(%error, "could not look up a CAS sign-in's account");
            return internal();
        }
    };
    if let CasAccount::Existing(user_id) | CasAccount::New(user_id) = &account
        && let Err(page) = refuse_exclusive(state, user_id).await
    {
        return *page;
    }
    let display_name = cas
        .displayname_attribute
        .as_ref()
        .and_then(|attr| response.attributes.get(attr))
        .and_then(|values| values.first().cloned())
        .filter(|name| !name.is_empty());
    let (user_id, created) = match account {
        CasAccount::Linked(user_id) | CasAccount::Existing(user_id) => {
            match state.store.get_user(&user_id).await {
                Ok(Some(user)) if user.deactivated => {
                    cas::count("failed");
                    return error_page(StatusCode::FORBIDDEN, "This account has been deactivated.");
                }
                Ok(Some(user)) => {
                    // `auth.sso.update_profile_information`: the name CAS gives now replaces
                    // the account's, and the change is carried into the user's rooms, as
                    // Synapse's `complete_sso_login_request` does with `_sso_update_profile_information`.
                    if state.config.get().sso_update_profile_information
                        && let Some(name) = &display_name
                        && user.display_name.as_deref() != Some(name.as_str())
                    {
                        match state
                            .store
                            .set_profile_display_name(&user_id, Some(name.clone()))
                            .await
                        {
                            Ok(()) => {
                                tracing::info!(%user_id, "a display name followed the sign-on provider's at sign-in");
                                if let Some(refresh) = state.profile_refresh() {
                                    refresh.profile_changed(state, &user_id);
                                }
                            }
                            Err(error) => {
                                tracing::warn!(%error, %user_id, "could not update a display name from the sign-on provider");
                            }
                        }
                    }
                }
                Ok(None) => {}
                Err(error) => {
                    tracing::warn!(%error, "could not read a CAS sign-in's account");
                    return internal();
                }
            }
            link(state, &user_id, &response).await;
            (user_id, false)
        }
        CasAccount::New(user_id) => {
            if !cas.enable_registration {
                // `auth.cas.enable_registration: false`: sign-in only, as Synapse's
                // `registration_enabled=False` aborts the flow for an unknown user.
                cas::count("failed");
                tracing::info!(cas_user = %response.user, %user_id, "refused a CAS sign-in: no account here and auth.cas.enable_registration is off");
                return error_page(
                    StatusCode::FORBIDDEN,
                    "You have no account on this server, and signing in through the sign-in \
                     service does not create one here. Ask this server's administrator.",
                );
            }
            match state
                .store
                .is_localpart_available(user_id.localpart())
                .await
            {
                Ok(true) => {}
                Ok(false) => {
                    cas::count("failed");
                    return error_page(
                        StatusCode::CONFLICT,
                        "An account with this name already exists here, under another spelling.",
                    );
                }
                Err(error) => {
                    tracing::warn!(%error, "could not check a CAS sign-in's user name");
                    return internal();
                }
            }
            let mut record = UserRecord::new(user_id.clone(), state.now_ms());
            record.display_name = display_name;
            if let Err(error) = state.store.create_user(record).await {
                tracing::warn!(%error, %user_id, "could not create an account at a CAS sign-in");
                return internal();
            }
            link(state, &user_id, &response).await;
            (user_id, true)
        }
        CasAccount::Unusable => {
            cas::count("failed");
            tracing::info!(cas_user = %response.user, "refused a CAS sign-in: the user name makes no valid user ID");
            return error_page(
                StatusCode::FORBIDDEN,
                "Your user name at the sign-in service cannot be used as a user name here.",
            );
        }
    };

    let token = generate_login_token();
    let record = LoginTokenRecord {
        hash: TokenHash::of(&token),
        user_id: user_id.clone(),
        expires_at_ms: state.now_ms() + state.config.get().login_token_ttl_ms,
        used: false,
    };
    if let Err(error) = state.store.put_login_token(record).await {
        tracing::warn!(%error, %user_id, "could not store a CAS sign-in's login token");
        return internal();
    }
    cas::count(if created { "registered" } else { "login" });
    tracing::info!(%user_id, cas_user = %response.user, new_account = created, "signed in through CAS");

    let continue_to = with_login_token(client, &token);
    // An application the operator trusts (`auth.sso.client_whitelist`) gets the person back at
    // once, without the confirmation page, as Synapse's `complete_sso_login` does.
    if state.config.get().sso_client_is_trusted(client) {
        tracing::debug!(%user_id, "sent a CAS sign-in straight back to a trusted application");
        return redirect(&continue_to);
    }
    page(
        StatusCode::OK,
        "Continue to your account",
        &format!(
            "<p>You signed in through {idp} as <strong>{user}</strong>.</p>\n\
             <p>Continue to <strong>{host}</strong>, the application that asked you to sign \
             in. If you did not just try to sign in to it, close this page.</p>\n\
             <p><a class=\"button\" href=\"{href}\">Continue</a></p>",
            idp = escape(cas.idp_name.as_str()),
            user = escape(user_id.as_str()),
            host = escape(host_of(client)),
            href = escape(continue_to.as_str()),
        ),
    )
}

async fn ui_auth_ticket(
    state: &AuthState,
    cas: &CasSettings,
    ticket: &str,
    session: &str,
) -> Response {
    let timeout = state.config.get().uia_session_timeout_ms;
    if uia::session_id_for(state.store.as_ref(), Some(session), state.now_ms(), timeout)
        .await
        .is_err()
    {
        return error_page(
            StatusCode::BAD_REQUEST,
            "This confirmation has expired. Start again from your application.",
        );
    }
    let response = match validate(state, cas, ticket, ("session", session)).await {
        Ok(response) => response,
        Err(page) => return *page,
    };
    let internal = || error_page(StatusCode::INTERNAL_SERVER_ERROR, "Something went wrong.");
    let user_id = match existing_account(state, &response).await {
        Ok(CasAccount::Linked(user_id) | CasAccount::Existing(user_id)) => Some(user_id),
        Ok(CasAccount::New(_) | CasAccount::Unusable) => None,
        Err(error) => {
            tracing::warn!(%error, "could not look up a CAS confirmation's account");
            return internal();
        }
    };
    if let Err(error) = state
        .store
        .mark_stage_complete(session, "m.login.sso")
        .await
    {
        tracing::warn!(%error, "could not record a CAS confirmation");
        return internal();
    }
    // The account CAS vouched for, or none at all: `reauth` compares it with the requester and
    // refuses the operation unless they are the same, as Synapse's `complete_sso_ui_auth_request`
    // marks the stage done for the empty user on a mismatch.
    let recorded = match &user_id {
        Some(user_id) => uia::record_authenticated_user(state.store.as_ref(), session, user_id)
            .await
            .map_err(|_| ()),
        None => state
            .store
            .set_session_data(session, uia::AUTHENTICATED_USER_KEY, json!(""))
            .await
            .map_err(|_| ()),
    };
    if recorded.is_err() {
        return internal();
    }
    match user_id {
        Some(user_id) => {
            cas::count("ui_auth");
            tracing::info!(%user_id, cas_user = %response.user, "confirmed a user-interactive auth through CAS");
            page(
                StatusCode::OK,
                "Confirmed",
                "<p>You have confirmed your identity. Close this page and go back to your \
                 application to finish.</p>",
            )
        }
        None => {
            cas::count("ui_auth_mismatch");
            tracing::info!(cas_user = %response.user, "a CAS confirmation named no account of this server");
            page(
                StatusCode::OK,
                "Not confirmed",
                "<p>The account you signed in to at the sign-in service is not linked to an \
                 account here, so this could not be confirmed. Close this page and try again \
                 with the right account.</p>",
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::{Arc, Mutex};

    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    use super::*;
    use crate::config::AuthConfig;

    /// Answers every ticket check with the configured body and remembers what it was asked.
    struct FakeCas {
        body: Mutex<String>,
        asked: Mutex<Vec<(String, String, String)>>,
    }

    #[async_trait::async_trait]
    impl cas::CasValidator for FakeCas {
        async fn validate(
            &self,
            server_url: &str,
            ticket: &str,
            service: &str,
        ) -> Result<String, CasError> {
            self.asked.lock().unwrap().push((
                server_url.to_owned(),
                ticket.to_owned(),
                service.to_owned(),
            ));
            Ok(self.body.lock().unwrap().clone())
        }
    }

    fn success(user: &str) -> String {
        format!(
            "<cas:serviceResponse xmlns:cas='http://www.yale.edu/tp/cas'><cas:authenticationSuccess>\
             <cas:user>{user}</cas:user><cas:attributes><cas:name>Casey</cas:name></cas:attributes>\
             </cas:authenticationSuccess></cas:serviceResponse>"
        )
    }

    fn state_with_cas(body: String) -> (AuthState, Arc<FakeCas>) {
        let fake = Arc::new(FakeCas {
            body: Mutex::new(body),
            asked: Mutex::new(Vec::new()),
        });
        let config = AuthConfig {
            public_baseurl: Some("https://hs.example.org".to_owned()),
            cas: Some(CasSettings {
                server_url: "https://cas.example.edu/cas".to_owned(),
                service_base: None,
                displayname_attribute: Some("name".to_owned()),
                required_attributes: BTreeMap::new(),
                idp_name: "Campus CAS".to_owned(),
                protocol_version: None,
                enable_registration: true,
                numeric_ids_prefix: None,
            }),
            ..AuthConfig::default()
        };
        let state = AuthState::in_memory_with_config(config).with_cas_validator(fake.clone());
        (state, fake)
    }

    /// Replaces the CAS settings in `state`'s live configuration.
    fn set_cas(state: &AuthState, change: impl FnOnce(&mut CasSettings, &mut AuthConfig)) {
        let mut config = state.config.get().as_ref().clone();
        let mut cas = config.cas.take().unwrap();
        change(&mut cas, &mut config);
        config.cas = Some(cas);
        state.set_config(config);
    }

    async fn get(state: &AuthState, uri: &str) -> (StatusCode, Option<String>, String) {
        let app = crate::routes::router().with_state(state.clone());
        let response = app
            .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let location = response
            .headers()
            .get(header::LOCATION)
            .map(|v| v.to_str().unwrap().to_owned());
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        (status, location, String::from_utf8(bytes.to_vec()).unwrap())
    }

    const CLIENT: &str = "https://client?p=http%3A%2F%2Fserver";

    fn service_for_client() -> String {
        format!(
            "https://hs.example.org/_matrix/client/r0/login/cas/ticket?redirectUrl={}",
            cas::encode_component(CLIENT)
        )
    }

    #[tokio::test]
    async fn the_redirect_goes_to_the_cas_login_page_with_the_r0_service() {
        let (state, _) = state_with_cas(success("x"));
        for path in [
            "/login/sso/redirect",
            "/login/cas/redirect",
            "/login/sso/redirect/cas",
        ] {
            let uri = format!("{path}?redirectUrl={}", cas::encode_component(CLIENT));
            let (status, location, _) = get(&state, &uri).await;
            assert_eq!(status, StatusCode::FOUND, "{path}");
            assert_eq!(
                location.unwrap(),
                format!(
                    "https://cas.example.edu/cas/login?service={}",
                    cas::encode_component(&service_for_client())
                )
            );
        }
        let (status, _, _) = get(&state, "/login/sso/redirect/google?redirectUrl=x").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        let (status, _, _) = get(&state, "/login/sso/redirect").await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn without_cas_there_is_no_sso() {
        let state = AuthState::in_memory();
        let (status, _, body) = get(&state, "/login/sso/redirect?redirectUrl=x").await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(body.contains("M_UNRECOGNIZED"), "{body}");
        let (_, _, body) = get(&state, "/login").await;
        assert!(!body.contains("m.login.sso"), "{body}");
    }

    #[tokio::test]
    async fn login_lists_sso_and_cas_when_cas_is_configured() {
        let (state, _) = state_with_cas(success("x"));
        let (_, _, body) = get(&state, "/login").await;
        let body: serde_json::Value = serde_json::from_str(&body).unwrap();
        let flows = body["flows"].as_array().unwrap();
        let sso = flows.iter().find(|f| f["type"] == "m.login.sso").unwrap();
        assert_eq!(sso["identity_providers"][0]["id"], "cas");
        assert_eq!(sso["identity_providers"][0]["name"], "Campus CAS");
        assert!(flows.iter().any(|f| f["type"] == "m.login.cas"));
    }

    /// `auth.sso.client_whitelist`: a trusted application gets the person back with a `302`,
    /// any other still gets the confirmation page.
    #[tokio::test]
    async fn a_trusted_client_is_sent_straight_back_without_the_confirmation_page() {
        let (state, _) = state_with_cas(success("trusted"));
        let mut config = (*state.config.get()).clone();
        config.sso_client_whitelist = vec!["https://client/".to_owned()];
        state.set_config(config);
        let uri = format!(
            "/login/cas/ticket?redirectUrl={}&ticket=t",
            cas::encode_component("https://client/app#/home")
        );
        let (status, location, _) = get(&state, &uri).await;
        assert_eq!(status, StatusCode::FOUND);
        let location = location.unwrap();
        assert!(
            location.starts_with("https://client/app?loginToken="),
            "{location}"
        );
        assert!(location.ends_with("#/home"), "{location}");

        // Not on the list: the page, as before.
        let uri = format!(
            "/login/cas/ticket?redirectUrl={}&ticket=t",
            cas::encode_component("https://client.evil.example/")
        );
        let (status, location, body) = get(&state, &uri).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(location, None);
        assert!(body.contains("Continue"), "{body}");
    }

    /// A CAS user name inside a bridge's exclusive namespace neither makes an account nor signs
    /// in to the bridge's existing one, as `/register` refuses it with `M_EXCLUSIVE`.
    #[tokio::test]
    async fn a_cas_user_cannot_take_a_name_in_an_appservices_exclusive_namespace() {
        let (state, _) = state_with_cas(success("irc_bob"));
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
        let state = AuthState {
            appservices: Arc::new(registry),
            ..state
        };
        let uri = format!(
            "/login/cas/ticket?redirectUrl={}&ticket=t",
            cas::encode_component(CLIENT)
        );
        let (status, _, body) = get(&state, &uri).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
        assert!(!body.contains("loginToken"));
        let ghost = ruma::user_id!("@irc_bob:example.org");
        assert!(state.store.get_user(ghost).await.unwrap().is_none());

        // The bridge's own ghost, already registered, is not signed in to either.
        state
            .store
            .create_user(UserRecord::new(ghost.to_owned(), 0))
            .await
            .unwrap();
        let (status, _, body) = get(&state, &uri).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
        assert!(!body.contains("loginToken"));
    }

    #[tokio::test]
    async fn a_new_cas_user_gets_an_account_and_a_login_token() {
        let (state, fake) = state_with_cas(success("cas_user!"));
        let uri = format!(
            "/login/cas/ticket?redirectUrl={}&ticket=goldenticket",
            cas::encode_component(CLIENT)
        );
        let (status, _, body) = get(&state, &uri).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let asked = fake.asked.lock().unwrap().clone();
        assert_eq!(
            asked,
            vec![(
                "https://cas.example.edu/cas/proxyValidate".to_owned(),
                "goldenticket".to_owned(),
                service_for_client()
            )]
        );
        // What Sytest greps the page for.
        let token = body
            .split("loginToken=")
            .nth(1)
            .and_then(|rest| rest.split(['"', '&']).next())
            .unwrap()
            .to_owned();
        assert!(body.contains("https://client?p=http%3A%2F%2Fserver&amp;loginToken="));

        let user_id = ruma::user_id!("@cas_user=21:example.org");
        let user = state.store.get_user(user_id).await.unwrap().unwrap();
        assert_eq!(user.display_name.as_deref(), Some("Casey"));
        assert_eq!(
            state
                .store
                .get_user_by_external_id("cas", "cas_user!")
                .await
                .unwrap()
                .as_deref(),
            Some(user_id)
        );
        let consumed = state
            .store
            .consume_login_token(&TokenHash::of(&token), state.now_ms())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(consumed.user_id, user_id);
    }

    #[tokio::test]
    async fn protocol_version_3_checks_the_ticket_at_the_p3_endpoint() {
        let (state, fake) = state_with_cas(success("cas_user!"));
        set_cas(&state, |cas, _| cas.protocol_version = Some(3));
        let uri = format!(
            "/login/cas/ticket?redirectUrl={}&ticket=goldenticket",
            cas::encode_component(CLIENT)
        );
        let (status, _, body) = get(&state, &uri).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(
            fake.asked.lock().unwrap()[0].0,
            "https://cas.example.edu/cas/p3/proxyValidate"
        );
    }

    #[tokio::test]
    async fn a_digits_only_cas_user_is_prefixed_when_numeric_ids_are_allowed() {
        let (state, _) = state_with_cas(success("1234"));
        set_cas(&state, |cas, _| {
            cas.numeric_ids_prefix = Some("u".to_owned())
        });
        let uri = format!(
            "/login/cas/ticket?redirectUrl={}&ticket=goldenticket",
            cas::encode_component(CLIENT)
        );
        let (status, _, body) = get(&state, &uri).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let user_id = ruma::user_id!("@u1234:example.org");
        assert!(state.store.get_user(user_id).await.unwrap().is_some());
        // Linked under the prefixed name, so the next sign-in finds the same account.
        assert_eq!(
            state
                .store
                .get_user_by_external_id("cas", "u1234")
                .await
                .unwrap()
                .as_deref(),
            Some(user_id)
        );
        assert!(
            state
                .store
                .get_user(ruma::user_id!("@1234:example.org"))
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn with_registration_off_only_people_with_an_account_sign_in() {
        let (state, _) = state_with_cas(success("Newcomer"));
        set_cas(&state, |cas, _| cas.enable_registration = false);
        let uri = format!(
            "/login/cas/ticket?redirectUrl={}&ticket=goldenticket",
            cas::encode_component(CLIENT)
        );
        let (status, _, body) = get(&state, &uri).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
        assert!(body.contains("does not create one here"), "{body}");
        assert!(
            state
                .store
                .get_user(ruma::user_id!("@newcomer:example.org"))
                .await
                .unwrap()
                .is_none(),
            "no account is made"
        );

        // Somebody with an account under the mapped name still signs in.
        let alice = ruma::user_id!("@alice:example.org").to_owned();
        state
            .store
            .create_user(UserRecord::new(alice.clone(), 0))
            .await
            .unwrap();
        let (state2, _) = state_with_cas(success("Alice"));
        set_cas(&state2, |cas, _| cas.enable_registration = false);
        state2
            .store
            .create_user(UserRecord::new(alice.clone(), 0))
            .await
            .unwrap();
        let (status, _, body) = get(&state2, &uri).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(body.contains("loginToken="), "{body}");
    }

    #[tokio::test]
    async fn the_display_name_follows_the_provider_only_when_asked() {
        let alice = ruma::user_id!("@alice:example.org").to_owned();
        let uri = format!(
            "/login/cas/ticket?redirectUrl={}&ticket=goldenticket",
            cas::encode_component(CLIENT)
        );
        for update in [false, true] {
            let (state, _) = state_with_cas(success("Alice"));
            set_cas(&state, |_, config| {
                config.sso_update_profile_information = update
            });
            let mut record = UserRecord::new(alice.clone(), 0);
            record.display_name = Some("Old Name".to_owned());
            state.store.create_user(record).await.unwrap();
            let (status, _, body) = get(&state, &uri).await;
            assert_eq!(status, StatusCode::OK, "{body}");
            let user = state.store.get_user(&alice).await.unwrap().unwrap();
            let expected = if update { "Casey" } else { "Old Name" };
            assert_eq!(
                user.display_name.as_deref(),
                Some(expected),
                "update={update}"
            );
        }
    }

    #[tokio::test]
    async fn an_existing_account_with_the_mapped_localpart_signs_in_as_itself() {
        let (state, _) = state_with_cas(success("Alice"));
        let alice = ruma::user_id!("@alice:example.org").to_owned();
        state
            .store
            .create_user(UserRecord::new(alice.clone(), 0))
            .await
            .unwrap();
        let (status, _, body) = get(
            &state,
            "/login/cas/ticket?redirectUrl=https%3A%2F%2Fc&ticket=t",
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(body.contains("@alice:example.org"));
        assert_eq!(state.store.list_users().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn a_refused_ticket_or_missing_attribute_is_an_error_page() {
        let (state, fake) = state_with_cas(
            "<cas:serviceResponse xmlns:cas='x'><cas:authenticationFailure code='INVALID_TICKET'>bad</cas:authenticationFailure></cas:serviceResponse>".to_owned(),
        );
        let (status, _, body) = get(
            &state,
            "/login/cas/ticket?redirectUrl=https%3A%2F%2Fc&ticket=t",
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert!(!body.contains("loginToken"));

        *fake.body.lock().unwrap() = success("bob");
        let mut config = (*state.config.get()).clone();
        if let Some(cas) = config.cas.as_mut() {
            cas.required_attributes = BTreeMap::from([("group".to_owned(), None)]);
        }
        state.set_config(config);
        let (status, _, body) = get(
            &state,
            "/login/cas/ticket?redirectUrl=https%3A%2F%2Fc&ticket=t",
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert!(!body.contains("loginToken"));
        assert!(state.store.list_users().await.unwrap().is_empty());
    }

    #[test]
    fn the_login_token_is_added_before_any_fragment() {
        assert_eq!(
            with_login_token("https://c/#/x", "t"),
            "https://c/?loginToken=t#/x"
        );
        assert_eq!(
            with_login_token("https://c/?a=b", "t"),
            "https://c/?a=b&loginToken=t"
        );
        assert_eq!(host_of("https://client?p=1"), "client");
    }

    /// The three SSO tests of Sytest's `10apidoc/13ui-auth.pl`, against the router: the flow is
    /// offered, a CAS confirmation as the requester completes the deletion, and one as somebody
    /// else is `403`.
    #[tokio::test]
    async fn the_sso_stage_of_user_interactive_auth() {
        let (state, fake) = state_with_cas(success("alice"));
        let alice = ruma::user_id!("@alice:example.org").to_owned();
        let mut record = UserRecord::new(alice.clone(), 0);
        record.password_hash = Some(crate::password::hash_password("sekrit").unwrap());
        state.store.create_user(record).await.unwrap();
        let session =
            crate::session::create_session(&state, &alice, Some("DEV1".into()), None, false)
                .await
                .unwrap();

        let delete = |token: String, body: serde_json::Value| {
            let state = state.clone();
            async move {
                let app = crate::routes::router().with_state(state);
                app.oneshot(
                    Request::builder()
                        .method("DELETE")
                        .uri("/devices/DEV1")
                        .header("authorization", format!("Bearer {token}"))
                        .header("content-type", "application/json")
                        .body(Body::from(body.to_string()))
                        .unwrap(),
                )
                .await
                .unwrap()
            }
        };

        for (cas_user, expected) in [
            ("somebody_else", StatusCode::FORBIDDEN),
            ("alice", StatusCode::OK),
        ] {
            *fake.body.lock().unwrap() = success(cas_user);
            let response = delete(session.access_token.clone(), json!({})).await;
            assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
            let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap();
            let challenge: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
            assert!(
                challenge["flows"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|f| f["stages"][0] == "m.login.sso"),
                "{challenge}"
            );
            let id = challenge["session"].as_str().unwrap().to_owned();

            let (status, _, _) =
                get(&state, &format!("/login/cas/ticket?session={id}&ticket=t")).await;
            assert_eq!(status, StatusCode::OK);
            let response = delete(
                session.access_token.clone(),
                json!({"auth": {"session": id}}),
            )
            .await;
            assert_eq!(response.status(), expected, "CAS user {cas_user}");
        }
        assert!(
            state
                .store
                .get_device(&alice, "DEV1".into())
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn the_fallback_sends_the_person_to_cas_with_the_session() {
        let (state, _) = state_with_cas(success("x"));
        let id = uia::session_id_for(state.store.as_ref(), None, state.now_ms(), 60_000)
            .await
            .unwrap();
        let (status, location, _) = get(
            &state,
            &format!("/auth/m.login.sso/fallback/web?session={id}"),
        )
        .await;
        assert_eq!(status, StatusCode::FOUND);
        let service =
            format!("https://hs.example.org/_matrix/client/r0/login/cas/ticket?session={id}");
        assert_eq!(
            location.unwrap(),
            format!(
                "https://cas.example.edu/cas/login?service={}",
                cas::encode_component(&service)
            )
        );
        let (status, _, _) = get(&state, "/auth/m.login.sso/fallback/web?session=nope").await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }
}
