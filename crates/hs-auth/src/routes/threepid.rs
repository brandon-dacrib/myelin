//! The self-service third-party identifier routes: validation emails and their links, adding
//! and deleting an account's identifiers, and binding and unbinding them at an identity server.
//! The logic is [`crate::threepid`]'s; these are the HTTP shapes.

use std::collections::HashMap;

use axum::Json;
use axum::extract::{Query, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde_json::{Value, json};

use crate::error::{ErrCode, MatrixError};
use crate::reauth;
use crate::requester::Requester;
use crate::state::AuthState;
use crate::threepid::{self, Purpose};
use hs_http::body::PermissiveJson;

/// `POST /register/email/requestToken`: a validation email for registering with an address
/// nobody has.
pub async fn post_register_email_request_token(
    State(state): State<AuthState>,
    client: hs_http::buckets::ClientIp,
    PermissiveJson(body): PermissiveJson<Value>,
) -> Result<Json<Value>, MatrixError> {
    threepid::request_email_token(&state, Purpose::Registration, &body, client.key())
        .await
        .map(Json)
}

/// `POST /account/3pid/email/requestToken`: a validation email for adding an address nobody
/// has to an account.
pub async fn post_account_3pid_email_request_token(
    State(state): State<AuthState>,
    client: hs_http::buckets::ClientIp,
    PermissiveJson(body): PermissiveJson<Value>,
) -> Result<Json<Value>, MatrixError> {
    threepid::request_email_token(&state, Purpose::AddThreepid, &body, client.key())
        .await
        .map(Json)
}

/// `POST /account/password/email/requestToken`: a validation email to an address an account
/// has, for resetting its password.
pub async fn post_password_email_request_token(
    State(state): State<AuthState>,
    client: hs_http::buckets::ClientIp,
    PermissiveJson(body): PermissiveJson<Value>,
) -> Result<Json<Value>, MatrixError> {
    threepid::request_email_token(&state, Purpose::PasswordReset, &body, client.key())
        .await
        .map(Json)
}

/// `POST /register/msisdn/requestToken`, `/account/3pid/msisdn/requestToken` and
/// `/account/password/msisdn/requestToken`: `400 M_THREEPID_MEDIUM_NOT_SUPPORTED`, since this
/// server sends no text messages.
pub async fn post_msisdn_request_token() -> MatrixError {
    MatrixError::new(
        StatusCode::BAD_REQUEST,
        ErrCode::ThreepidMediumNotSupported,
        "This server does not send text messages, so it cannot validate a phone number",
    )
}

fn page(status: StatusCode, title: &str, message: &str) -> Response {
    let body = format!(
        "<!doctype html><html><head><meta charset=\"utf-8\"><meta name=\"viewport\" \
         content=\"width=device-width, initial-scale=1\"><title>{title}</title></head><body \
         style=\"font-family: sans-serif; max-width: 32em; margin: 3em auto; padding: 0 1em\">\
         <h1>{title}</h1><p>{message}</p></body></html>",
        title = threepid::html_escape(title),
        message = threepid::html_escape(message),
    );
    (
        status,
        [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
        body,
    )
        .into_response()
}

async fn submit(state: &AuthState, purpose: Purpose, query: &HashMap<String, String>) -> Response {
    let (Some(sid), Some(client_secret), Some(token)) = (
        query.get("sid"),
        query.get("client_secret"),
        query.get("token"),
    ) else {
        return page(
            StatusCode::BAD_REQUEST,
            "Link incomplete",
            "This validation link is missing part of its address. Copy the whole link from \
             the email.",
        );
    };
    match threepid::submit_token(state, purpose, sid, client_secret, token).await {
        Ok(_) => page(
            StatusCode::OK,
            "Email address validated",
            "Your email address has been validated. Go back to your app to continue.",
        ),
        Err(error) if error.status() == StatusCode::BAD_REQUEST => page(
            StatusCode::BAD_REQUEST,
            "Link not valid",
            "This validation link is not valid, or has expired. Ask your app to send a new \
             email.",
        ),
        Err(error) => error.into_response(),
    }
}

/// `GET /_matrix/client/unstable/registration/email/submit_token`: the link in a
/// registration's validation email.
pub async fn get_registration_submit_token(
    State(state): State<AuthState>,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    submit(&state, Purpose::Registration, &query).await
}

/// `GET /_matrix/client/unstable/add_threepid/email/submit_token`: the link in the validation
/// email for adding an address to an account.
pub async fn get_add_threepid_submit_token(
    State(state): State<AuthState>,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    submit(&state, Purpose::AddThreepid, &query).await
}

/// `GET /_matrix/client/unstable/password_reset/email/submit_token`: the link in a password
/// reset email.
pub async fn get_password_reset_submit_token(
    State(state): State<AuthState>,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    submit(&state, Purpose::PasswordReset, &query).await
}

fn required<'a>(body: &'a Value, key: &str) -> Result<&'a str, MatrixError> {
    body.get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| MatrixError::missing_param(format!("Missing {key}")))
}

fn no_validated_session() -> MatrixError {
    MatrixError::new(
        StatusCode::BAD_REQUEST,
        ErrCode::ThreepidAuthFailed,
        "No validated 3pid session found",
    )
}

/// The address as stored: an email address canonicalised, anything else as given.
fn stored_address(medium: &str, address: &str) -> Result<String, MatrixError> {
    if medium == "email" {
        threepid::canonical_email(address)
    } else {
        Ok(address.to_owned())
    }
}

/// `POST /account/3pid/add`: adds an address this server validated (`sid`, `client_secret`) to
/// the account, after user-interactive auth (Synapse requires it).
pub async fn post_account_3pid_add(
    State(state): State<AuthState>,
    requester: Requester,
    PermissiveJson(body): PermissiveJson<Value>,
) -> Result<Response, MatrixError> {
    requester.require_not_suspended()?;
    let sid = required(&body, "sid")?;
    let client_secret = required(&body, "client_secret")?;
    threepid::validate_client_secret(client_secret)?;
    if let Some(response) =
        reauth::run_for_operation(&state, &requester, &body, "POST /account/3pid/add").await?
    {
        return Ok(response);
    }
    let record = threepid::validated_session(&state, sid, client_secret)
        .await?
        .ok_or_else(no_validated_session)?;
    threepid::add_validated(&state, &requester.user_id, &record).await?;
    Ok(Json(json!({})).into_response())
}

/// `POST /account/3pid` (deprecated): adds an address this server validated, named by
/// `three_pid_creds` (or `threePidCreds`), with no user-interactive auth, as Synapse still
/// accepts it; with `bind: true` the address is also bound at the identity server the
/// credentials name.
pub async fn post_account_3pid(
    State(state): State<AuthState>,
    requester: Requester,
    PermissiveJson(body): PermissiveJson<Value>,
) -> Result<Json<Value>, MatrixError> {
    requester.require_not_suspended()?;
    let creds = body
        .get("three_pid_creds")
        .or_else(|| body.get("threePidCreds"))
        .ok_or_else(|| MatrixError::missing_param("Missing param three_pid_creds"))?;
    let sid = required(creds, "sid")?;
    let client_secret = required(creds, "client_secret")?;
    threepid::validate_client_secret(client_secret)?;
    let record = threepid::validated_session(&state, sid, client_secret)
        .await?
        .ok_or_else(no_validated_session)?;
    threepid::add_validated(&state, &requester.user_id, &record).await?;
    if body.get("bind").and_then(Value::as_bool) == Some(true)
        && let (Some(id_server), Some(id_access_token)) = (
            creds.get("id_server").and_then(Value::as_str),
            creds.get("id_access_token").and_then(Value::as_str),
        )
    {
        threepid::bind(
            &state,
            &requester.user_id,
            id_server,
            id_access_token,
            sid,
            client_secret,
        )
        .await?;
    }
    Ok(Json(json!({})))
}

fn unbind_result(unbound: bool) -> Value {
    json!({ "id_server_unbind_result": if unbound { "success" } else { "no-support" } })
}

/// `POST /account/3pid/delete`: removes an identifier from the account, unbinding it first at
/// the identity server named (`id_server`) or at every identity server this server bound it at.
pub async fn post_account_3pid_delete(
    State(state): State<AuthState>,
    requester: Requester,
    PermissiveJson(body): PermissiveJson<Value>,
) -> Result<Json<Value>, MatrixError> {
    requester.require_not_suspended()?;
    let medium = required(&body, "medium")?;
    let address = stored_address(medium, required(&body, "address")?)?;
    let id_server = body.get("id_server").and_then(Value::as_str);
    let unbound = threepid::unbind(&state, &requester.user_id, medium, &address, id_server).await?;
    match state
        .store
        .remove_threepid(&requester.user_id, medium, &address)
        .await
    {
        Ok(()) => {
            threepid::count_change("deleted");
            tracing::info!(user = %requester.user_id, medium, "deleted a third-party identifier");
        }
        // Deleting one the account does not have (only bound at an identity server, out of
        // band) is not an error: the unbind above was the point.
        Err(crate::store::StoreError::NotFound(_)) => {}
        Err(e) => return Err(e.into()),
    }
    Ok(Json(unbind_result(unbound)))
}

/// `POST /account/3pid/bind`: binds an identifier the identity server validated to this
/// account there.
pub async fn post_account_3pid_bind(
    State(state): State<AuthState>,
    requester: Requester,
    PermissiveJson(body): PermissiveJson<Value>,
) -> Result<Json<Value>, MatrixError> {
    requester.require_not_suspended()?;
    let id_server = required(&body, "id_server")?;
    let id_access_token = required(&body, "id_access_token")?;
    let sid = required(&body, "sid")?;
    let client_secret = required(&body, "client_secret")?;
    threepid::validate_client_secret(client_secret)?;
    threepid::bind(
        &state,
        &requester.user_id,
        id_server,
        id_access_token,
        sid,
        client_secret,
    )
    .await?;
    Ok(Json(json!({})))
}

/// `POST /account/3pid/unbind`: unbinds an identifier from this account at the identity server
/// named, or at every one this server bound it at, keeping it on the account.
pub async fn post_account_3pid_unbind(
    State(state): State<AuthState>,
    requester: Requester,
    PermissiveJson(body): PermissiveJson<Value>,
) -> Result<Json<Value>, MatrixError> {
    let medium = required(&body, "medium")?;
    let address = stored_address(medium, required(&body, "address")?)?;
    let id_server = body.get("id_server").and_then(Value::as_str);
    let unbound = threepid::unbind(&state, &requester.user_id, medium, &address, id_server).await?;
    Ok(Json(unbind_result(unbound)))
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;
    use crate::config::AuthConfig;
    use crate::store::UserRecord;
    use crate::threepid::{
        EmailSender, IdentityServerClient, IdentityServerError, OutgoingEmail, UnbindOutcome,
    };
    use ruma::{UserId, user_id};

    #[derive(Default)]
    struct RecordingSender(Mutex<Vec<OutgoingEmail>>);

    #[async_trait::async_trait]
    impl EmailSender for RecordingSender {
        fn can_send(&self) -> bool {
            true
        }
        async fn send(&self, email: OutgoingEmail) -> Result<(), String> {
            self.0.lock().unwrap().push(email);
            Ok(())
        }
    }

    /// An identity server that binds whatever it is asked to and records unbinds; `id.example`
    /// is the only one allowed, and `old.example` does not support unbinding.
    #[derive(Default)]
    struct FakeIdentityServer {
        bindings: Mutex<Vec<(String, String, String)>>,
        unbinds: Mutex<Vec<(String, String, String)>>,
    }

    #[async_trait::async_trait]
    impl IdentityServerClient for FakeIdentityServer {
        fn allows(&self, id_server: &str) -> bool {
            id_server == "id.example" || id_server == "old.example"
        }
        async fn bind(
            &self,
            id_server: &str,
            id_access_token: &str,
            sid: &str,
            _client_secret: &str,
            mxid: &UserId,
        ) -> Result<Value, IdentityServerError> {
            if id_access_token != "idtoken" {
                return Err(IdentityServerError::Refused {
                    status: 403,
                    body: json!({"errcode": "M_UNAUTHORIZED", "error": "bad token"}),
                });
            }
            let address = format!("{sid}@example.com");
            self.bindings.lock().unwrap().push((
                id_server.to_owned(),
                mxid.to_string(),
                address.clone(),
            ));
            Ok(json!({"medium": "email", "address": address, "mxid": mxid}))
        }
        async fn unbind(
            &self,
            id_server: &str,
            mxid: &UserId,
            _medium: &str,
            address: &str,
        ) -> Result<UnbindOutcome, IdentityServerError> {
            self.unbinds.lock().unwrap().push((
                id_server.to_owned(),
                mxid.to_string(),
                address.to_owned(),
            ));
            Ok(if id_server == "old.example" {
                UnbindOutcome::NotSupported
            } else {
                UnbindOutcome::Unbound
            })
        }
    }

    fn email_state() -> (AuthState, Arc<RecordingSender>) {
        let state = AuthState::in_memory_with_config(AuthConfig {
            public_baseurl: Some("https://hs.example.org".into()),
            ..AuthConfig::default()
        });
        let sender = Arc::new(RecordingSender::default());
        state.install_email_sender(sender.clone());
        (state, sender)
    }

    fn query_of(link: &str) -> HashMap<String, String> {
        let query = link.split_once('?').unwrap().1;
        query
            .split('&')
            .map(|pair| {
                let (k, v) = pair.split_once('=').unwrap();
                (k.to_owned(), v.replace("%3D", "="))
            })
            .collect()
    }

    fn link_in(email: &OutgoingEmail) -> String {
        email
            .text
            .split_whitespace()
            .find(|w| w.starts_with("http"))
            .unwrap()
            .to_owned()
    }

    async fn request(state: &AuthState, purpose: Purpose, email: &str, attempt: u64) -> Value {
        threepid::request_email_token(
            state,
            purpose,
            &json!({"client_secret": "SECRET.=_-", "email": email, "send_attempt": attempt}),
            None,
        )
        .await
        .unwrap()
    }

    async fn follow(state: &AuthState, purpose: Purpose, email: &OutgoingEmail) -> Response {
        let link = link_in(email);
        assert!(
            link.starts_with(&format!(
                "https://hs.example.org/_matrix/client/unstable/{}/email/submit_token?",
                purpose.as_str()
            )),
            "{link}"
        );
        submit(state, purpose, &query_of(&link)).await
    }

    async fn body_json(response: Response) -> Value {
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    async fn user_with_password(state: &AuthState, user_id: &UserId) -> Requester {
        let mut record = UserRecord::new(user_id.to_owned(), 0);
        record.password_hash = Some(crate::password::hash_password("sekrit").unwrap());
        state.store.create_user(record).await.unwrap();
        Requester::for_user(user_id.to_owned())
    }

    #[tokio::test]
    async fn without_email_the_request_is_medium_not_supported() {
        let state = AuthState::in_memory();
        let err = threepid::request_email_token(
            &state,
            Purpose::Registration,
            &json!({"client_secret": "s", "email": "a@b.c", "send_attempt": 1}),
            None,
        )
        .await
        .unwrap_err();
        assert_eq!(err.status(), StatusCode::BAD_REQUEST);
        assert_eq!(err.errcode(), ErrCode::ThreepidMediumNotSupported);
        // An email sender but no public address to link back to is not enough either.
        let state = AuthState::in_memory();
        state.install_email_sender(Arc::new(RecordingSender::default()));
        assert!(!threepid::email_available(&state));
        assert_eq!(
            post_msisdn_request_token().await.errcode(),
            ErrCode::ThreepidMediumNotSupported
        );
    }

    #[tokio::test]
    async fn a_request_is_checked_before_any_email_goes() {
        let (state, sender) = email_state();
        let bad_secret = threepid::request_email_token(
            &state,
            Purpose::Registration,
            &json!({"client_secret": "no spaces", "email": "a@b.c", "send_attempt": 1}),
            None,
        )
        .await
        .unwrap_err();
        assert_eq!(bad_secret.errcode(), ErrCode::InvalidParam);
        let bad_email = threepid::request_email_token(
            &state,
            Purpose::Registration,
            &json!({"client_secret": "s", "email": "nobody", "send_attempt": 1}),
            None,
        )
        .await
        .unwrap_err();
        assert_eq!(bad_email.errcode(), ErrCode::InvalidParam);
        let missing = threepid::request_email_token(
            &state,
            Purpose::Registration,
            &json!({"client_secret": "s", "email": "a@b.c"}),
            None,
        )
        .await
        .unwrap_err();
        assert_eq!(missing.errcode(), ErrCode::MissingParam);
        // Resetting a password by an address nobody has.
        let reset = threepid::request_email_token(
            &state,
            Purpose::PasswordReset,
            &json!({"client_secret": "s", "email": "a@b.c", "send_attempt": 1}),
            None,
        )
        .await
        .unwrap_err();
        assert_eq!(reset.errcode(), ErrCode::ThreepidNotFound);
        assert!(sender.0.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_repeated_send_attempt_sends_nothing_and_a_higher_one_resends_on_the_same_session() {
        let (state, sender) = email_state();
        let first = request(&state, Purpose::Registration, "Bob@Example.com", 1).await;
        let again = request(&state, Purpose::Registration, "bob@example.com", 1).await;
        assert_eq!(first["sid"], again["sid"]);
        assert_eq!(sender.0.lock().unwrap().len(), 1);
        assert_eq!(sender.0.lock().unwrap()[0].to, "bob@example.com");
        let third = request(&state, Purpose::Registration, "bob@example.com", 2).await;
        assert_eq!(first["sid"], third["sid"]);
        assert_eq!(sender.0.lock().unwrap().len(), 2);
        // Only the newest link works.
        let emails = sender.0.lock().unwrap().clone();
        let stale = follow(&state, Purpose::Registration, &emails[0]).await;
        assert_eq!(stale.status(), StatusCode::BAD_REQUEST);
        let fresh = follow(&state, Purpose::Registration, &emails[1]).await;
        assert_eq!(fresh.status(), StatusCode::OK);
        // Following it twice is fine; following it for another purpose is not.
        let twice = follow(&state, Purpose::Registration, &emails[1]).await;
        assert_eq!(twice.status(), StatusCode::OK);
        let mut wrong = query_of(&link_in(&emails[1]));
        let elsewhere = submit(&state, Purpose::AddThreepid, &wrong).await;
        assert_eq!(elsewhere.status(), StatusCode::BAD_REQUEST);
        wrong.insert("client_secret".into(), "other".into());
        let wrong_secret = submit(&state, Purpose::Registration, &wrong).await;
        assert_eq!(wrong_secret.status(), StatusCode::BAD_REQUEST);
        let incomplete = submit(&state, Purpose::Registration, &HashMap::new()).await;
        assert_eq!(incomplete.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn an_expired_link_is_refused() {
        let (state, sender) = email_state();
        request(&state, Purpose::AddThreepid, "late@example.com", 1).await;
        let email = sender.0.lock().unwrap()[0].clone();
        let query = query_of(&link_in(&email));
        let mut record = state
            .store
            .get_validation_session(&query["sid"])
            .await
            .unwrap()
            .unwrap();
        record.token_expires_at_ms = 0;
        state.store.put_validation_session(record).await.unwrap();
        let response = submit(&state, Purpose::AddThreepid, &query).await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    /// Sytest's "Can register using an email address", then "Can login with 3pid and password
    /// using m.login.password" with the legacy top-level `medium`/`address`.
    #[tokio::test]
    async fn registering_with_a_validated_email_address_binds_it_and_it_signs_in() {
        let (state, sender) = email_state();
        let register = |body: Value| {
            crate::routes::register::post_register(
                State(state.clone()),
                Query(HashMap::new()),
                axum::http::HeaderMap::new(),
                hs_http::buckets::ClientIp(None),
                PermissiveJson(body),
            )
        };
        let challenge = register(json!({"username": "emailuser", "password": "sekrit"}))
            .await
            .unwrap();
        assert_eq!(challenge.status(), StatusCode::UNAUTHORIZED);
        let challenge = body_json(challenge).await;
        let flows = challenge["flows"].as_array().unwrap();
        assert!(
            flows
                .iter()
                .any(|f| f["stages"] == json!(["m.login.email.identity"])),
            "{challenge}"
        );
        let session = challenge["session"].as_str().unwrap().to_owned();

        let sid = request(&state, Purpose::Registration, "testemail@example.com", 1).await["sid"]
            .as_str()
            .unwrap()
            .to_owned();
        // Presented before the link is followed, the stage fails.
        let early = register(json!({
            "username": "emailuser", "password": "sekrit",
            "auth": {"type": "m.login.email.identity", "session": session,
                     "threepid_creds": {"sid": sid, "client_secret": "SECRET.=_-"}},
        }))
        .await
        .unwrap_err();
        assert_eq!(early.status(), StatusCode::UNAUTHORIZED);
        let email = sender.0.lock().unwrap()[0].clone();
        assert_eq!(
            follow(&state, Purpose::Registration, &email).await.status(),
            StatusCode::OK
        );
        let done = register(json!({
            "username": "emailuser", "password": "sekrit",
            "auth": {"type": "m.login.email.identity", "session": session,
                     "threepid_creds": {"sid": sid, "client_secret": "SECRET.=_-"}},
        }))
        .await
        .unwrap();
        assert_eq!(done.status(), StatusCode::OK);
        let user = user_id!("@emailuser:example.org");
        let threepids = state.store.list_threepids(user).await.unwrap();
        assert_eq!(threepids.len(), 1);
        assert_eq!(threepids[0].address, "testemail@example.com");

        let login = crate::routes::login::post_login(
            State(state.clone()),
            axum::http::HeaderMap::new(),
            Query(HashMap::new()),
            hs_http::buckets::ClientIp(None),
            PermissiveJson(json!({
                "type": "m.login.password", "medium": "email",
                "address": "TestEmail@example.com", "password": "sekrit",
            })),
        )
        .await
        .unwrap();
        assert_eq!(login.status(), StatusCode::OK);
        assert_eq!(body_json(login).await["user_id"], user.as_str());

        // The address is taken now.
        let taken = threepid::request_email_token(
            &state,
            Purpose::Registration,
            &json!({"client_secret": "x", "email": "testemail@example.com", "send_attempt": 1}),
            None,
        )
        .await
        .unwrap_err();
        assert_eq!(taken.errcode(), ErrCode::ThreepidInUse);
    }

    #[tokio::test]
    async fn without_email_registration_offers_no_email_flow() {
        let state = AuthState::in_memory();
        let challenge = crate::routes::register::post_register(
            State(state),
            Query(HashMap::new()),
            axum::http::HeaderMap::new(),
            hs_http::buckets::ClientIp(None),
            PermissiveJson(json!({"username": "plain"})),
        )
        .await
        .unwrap();
        let challenge = body_json(challenge).await;
        assert_eq!(challenge["flows"], json!([{"stages": ["m.login.dummy"]}]));
    }

    /// Sytest's `add_email_for_user`: validate, then the deprecated `POST /account/3pid`.
    #[tokio::test]
    async fn an_address_is_added_with_the_deprecated_route_and_with_add_after_uia() {
        let (state, sender) = email_state();
        let alice = user_with_password(&state, user_id!("@alice:example.org")).await;
        let sid = request(&state, Purpose::AddThreepid, "bob@example.com", 1).await["sid"]
            .as_str()
            .unwrap()
            .to_owned();
        let creds = json!({"three_pid_creds": {"sid": sid, "client_secret": "SECRET.=_-",
                                               "id_server": "id.example"}});
        // Not yet validated.
        let err = post_account_3pid(
            State(state.clone()),
            alice.clone(),
            PermissiveJson(creds.clone()),
        )
        .await
        .unwrap_err();
        assert_eq!(err.errcode(), ErrCode::ThreepidAuthFailed);
        let email = sender.0.lock().unwrap()[0].clone();
        follow(&state, Purpose::AddThreepid, &email).await;
        let Json(_) = post_account_3pid(State(state.clone()), alice.clone(), PermissiveJson(creds))
            .await
            .unwrap();
        assert_eq!(
            state.store.list_threepids(&alice.user_id).await.unwrap()[0].address,
            "bob@example.com"
        );

        // `/add` asks for user-interactive auth first.
        request(&state, Purpose::AddThreepid, "carol@example.com", 1).await;
        let email = sender.0.lock().unwrap()[1].clone();
        follow(&state, Purpose::AddThreepid, &email).await;
        let sid = query_of(&link_in(&email))["sid"].clone();
        let body = json!({"sid": sid, "client_secret": "SECRET.=_-"});
        let challenge = post_account_3pid_add(
            State(state.clone()),
            alice.clone(),
            PermissiveJson(body.clone()),
        )
        .await
        .unwrap();
        assert_eq!(challenge.status(), StatusCode::UNAUTHORIZED);
        let mut with_auth = body;
        with_auth["auth"] = json!({"type": "m.login.password", "password": "sekrit",
            "identifier": {"type": "m.id.user", "user": "alice"}});
        let added = post_account_3pid_add(
            State(state.clone()),
            alice.clone(),
            PermissiveJson(with_auth),
        )
        .await
        .unwrap();
        assert_eq!(added.status(), StatusCode::OK);
        assert_eq!(
            state
                .store
                .list_threepids(&alice.user_id)
                .await
                .unwrap()
                .len(),
            2
        );
    }

    /// `54identity.pl`: bind through this server, then delete, unbind with and without naming
    /// the identity server, and unbind one bound out of band.
    #[tokio::test]
    async fn binding_and_unbinding_go_to_the_identity_server_and_are_remembered() {
        let state = AuthState::in_memory();
        let is = Arc::new(FakeIdentityServer::default());
        state.install_identity_server_client(is.clone());
        let alice = user_with_password(&state, user_id!("@alice:example.org")).await;
        let bind_body = |sid: &str, id_server: &str| {
            json!({"id_server": id_server, "id_access_token": "idtoken", "sid": sid,
                   "client_secret": "12345678901234567890Az.=_-"})
        };
        let Json(_) = post_account_3pid_bind(
            State(state.clone()),
            alice.clone(),
            PermissiveJson(bind_body("bob1", "id.example")),
        )
        .await
        .unwrap();
        assert_eq!(is.bindings.lock().unwrap().len(), 1);
        let bindings = state
            .store
            .list_threepid_bindings(&alice.user_id)
            .await
            .unwrap();
        assert_eq!(bindings[0].address, "bob1@example.com");
        assert_eq!(bindings[0].id_server, "id.example");

        // Delete without naming the identity server: unbound where it was bound.
        let Json(deleted) = post_account_3pid_delete(
            State(state.clone()),
            alice.clone(),
            PermissiveJson(json!({"medium": "email", "address": "bob1@example.com"})),
        )
        .await
        .unwrap();
        assert_eq!(deleted["id_server_unbind_result"], "success");
        assert_eq!(
            is.unbinds.lock().unwrap().last().unwrap().0,
            "id.example".to_owned()
        );
        assert!(
            state
                .store
                .list_threepid_bindings(&alice.user_id)
                .await
                .unwrap()
                .is_empty()
        );

        // Nothing recorded and no identity server named: nowhere to unbind.
        let Json(nowhere) = post_account_3pid_unbind(
            State(state.clone()),
            alice.clone(),
            PermissiveJson(json!({"medium": "email", "address": "x@example.com"})),
        )
        .await
        .unwrap();
        assert_eq!(nowhere["id_server_unbind_result"], "no-support");

        // Bound out of band: the identity server named is asked anyway.
        let Json(out_of_band) = post_account_3pid_delete(
            State(state.clone()),
            alice.clone(),
            PermissiveJson(json!({"medium": "email", "address": "bob3@example.com",
                                  "id_server": "id.example"})),
        )
        .await
        .unwrap();
        assert_eq!(out_of_band["id_server_unbind_result"], "success");
        assert_eq!(
            is.unbinds.lock().unwrap().last().unwrap().2,
            "bob3@example.com".to_owned()
        );

        // An identity server that does not support unbinding.
        let Json(_) = post_account_3pid_bind(
            State(state.clone()),
            alice.clone(),
            PermissiveJson(bind_body("old", "old.example")),
        )
        .await
        .unwrap();
        let Json(old) = post_account_3pid_unbind(
            State(state.clone()),
            alice.clone(),
            PermissiveJson(json!({"medium": "email", "address": "old@example.com"})),
        )
        .await
        .unwrap();
        assert_eq!(old["id_server_unbind_result"], "no-support");

        // An identity server this server does not use is refused, and so is a bad token.
        let untrusted = post_account_3pid_bind(
            State(state.clone()),
            alice.clone(),
            PermissiveJson(bind_body("x", "evil.example")),
        )
        .await
        .unwrap_err();
        assert_eq!(untrusted.errcode(), ErrCode::ServerNotTrusted);
        let mut bad_token = bind_body("x", "id.example");
        bad_token["id_access_token"] = json!("wrong");
        let refused = post_account_3pid_bind(
            State(state.clone()),
            alice.clone(),
            PermissiveJson(bad_token),
        )
        .await
        .unwrap_err();
        assert_eq!(refused.status(), StatusCode::FORBIDDEN);
    }

    /// `54identity.pl`'s "3PIDs are unbound after account deactivation".
    #[tokio::test]
    async fn deactivation_unbinds_and_removes_every_identifier() {
        let state = AuthState::in_memory();
        let is = Arc::new(FakeIdentityServer::default());
        state.install_identity_server_client(is.clone());
        let alice = user_with_password(&state, user_id!("@alice:example.org")).await;
        let Json(_) = post_account_3pid_bind(
            State(state.clone()),
            alice.clone(),
            PermissiveJson(
                json!({"id_server": "id.example", "id_access_token": "idtoken",
                                  "sid": "bob4", "client_secret": "s"}),
            ),
        )
        .await
        .unwrap();
        state
            .store
            .add_threepid(crate::store::ThreepidRecord {
                user_id: alice.user_id.clone(),
                medium: "email".into(),
                address: "local@example.com".into(),
                added_at_ms: 0,
                validated_at_ms: 0,
            })
            .await
            .unwrap();
        let response = crate::routes::account::post_account_deactivate(
            State(state.clone()),
            alice.clone(),
            PermissiveJson(
                json!({"auth": {"type": "m.login.password", "password": "sekrit",
                "identifier": {"type": "m.id.user", "user": "alice"}}}),
            ),
        )
        .await
        .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            body_json(response).await["id_server_unbind_result"],
            "success"
        );
        assert_eq!(is.unbinds.lock().unwrap()[0].2, "bob4@example.com");
        assert!(
            state
                .store
                .list_threepids(&alice.user_id)
                .await
                .unwrap()
                .is_empty()
        );
        assert!(
            state
                .store
                .list_threepid_bindings(&alice.user_id)
                .await
                .unwrap()
                .is_empty()
        );
    }
}
