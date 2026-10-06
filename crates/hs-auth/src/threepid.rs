//! Self-service third-party identifiers: this server proving that a person controls an email
//! address, adding and removing the identifiers bound to an account, and binding them at an
//! identity server (so other people can find the account by them) and unbinding them again.
//!
//! # Validation
//!
//! A client asks for a validation email (`POST /register/email/requestToken`,
//! `/account/3pid/email/requestToken`, `/account/password/email/requestToken`: [`Purpose`]),
//! this server sends one with a one-time link, and following the link
//! (`GET /_matrix/client/unstable/<purpose>/email/submit_token`) marks the session
//! (`sid`, kept by [`crate::store::ThreepidStore`]) validated. The client then presents
//! `{sid, client_secret}` where it wants the address used: the `m.login.email.identity`
//! registration stage, `POST /account/3pid/add`, or (signed out) `POST /account/password`,
//! which resets the password of the account the address belongs to ([`password_reset_owner`]).
//!
//! A client may name a `next_link`, where the person's browser goes once the link is followed
//! (its own "you can go back now" page); [`check_next_link`] allows `http(s)` addresses only, and
//! only on the hosts `auth.next_link_domain_whitelist` lists when it is set, as Synapse's
//! `assert_valid_next_link`. A password reset link first shows a page asking the person to
//! confirm (a `POST` back to the same address), so that a mail scanner fetching every link in
//! an email cannot validate the session on its own, as Synapse's `password_reset_confirmation`
//! page does. This is Synapse's "local" email behaviour
//! (`threepid_behaviour_email = LOCAL`); this server never delegates email validation to an
//! identity server. Validation is available while the server can send email and knows the
//! address its links point back to: `email.smtp.host`, `email.from` and
//! `server.public_baseurl` ([`email_available`]). Otherwise the `requestToken` routes answer
//! `400 M_THREEPID_MEDIUM_NOT_SUPPORTED`, the spec's code for "the homeserver does not support
//! adding a third-party identifier of the given medium", which is exactly the situation; phone
//! numbers always get it, since this server sends no text messages.
//!
//! # Identity servers
//!
//! Binding (`POST /account/3pid/bind`) and unbinding (`/account/3pid/unbind`,
//! `/account/3pid/delete`, account deactivation) talk to an identity server through
//! [`IdentityServerClient`], answered by `hs serve` from the same client third-party invites
//! use, so only the identity servers `auth.identity_servers` names are ever contacted. Where an
//! identifier was bound is remembered ([`crate::store::ThreepidBindingRecord`]), so it can be
//! unbound there later without the client naming the server, as Synapse's
//! `user_threepid_id_server` table does.

use std::sync::LazyLock;

use async_trait::async_trait;
use axum::http::StatusCode;
use prometheus_client::encoding::EncodeLabelSet;
use prometheus_client::metrics::counter::Counter;
use prometheus_client::metrics::family::Family;
use rand::Rng;
use rand::distr::Alphanumeric;
use ruma::UserId;
use serde_json::{Value, json};
use sha2::Digest;

use crate::error::{ErrCode, MatrixError};
use crate::state::AuthState;
use crate::store::{ThreepidBindingRecord, ThreepidValidationRecord};

/// How long the link in a validation email works: an hour, as Synapse's
/// `email.validation_token_lifetime` defaults to.
pub const TOKEN_LIFETIME_MS: u64 = 60 * 60 * 1000;

/// How long a validated session may be used (to register, or to add the address) after it was
/// started: a day.
pub const VALIDATED_SESSION_LIFETIME_MS: u64 = 24 * 60 * 60 * 1000;

/// One email for [`EmailSender`] to send.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutgoingEmail {
    /// The recipient's address.
    pub to: String,
    /// The subject line.
    pub subject: String,
    /// The plain-text part.
    pub text: String,
    /// The HTML part.
    pub html: String,
}

/// Where validation emails are sent from: `hs serve`'s SMTP mailer (the one email pushers use),
/// installed through [`AuthState::install_email_sender`]. This crate cannot reach it itself
/// (`hs-push` depends on this crate), so, like [`crate::state::DeviceListChangeNotifier`], it is a
/// trait defined here and answered from outside. Unset, no email can be sent and validation is
/// unavailable.
#[async_trait]
pub trait EmailSender: Send + Sync {
    /// Whether email can be sent right now (`email.smtp.host` and `email.from` are set).
    fn can_send(&self) -> bool;

    /// What this service is called in emails (`email.app_name`), for the subject line.
    fn app_name(&self) -> String {
        "Matrix".to_owned()
    }

    /// Sends `email`, with this server's configured sender.
    ///
    /// # Errors
    /// Why the email could not be sent.
    async fn send(&self, email: OutgoingEmail) -> Result<(), String>;
}

/// Why an identity server call failed.
#[derive(Debug, Clone, PartialEq)]
pub enum IdentityServerError {
    /// The identity server answered with an error status.
    Refused {
        /// The HTTP status.
        status: u16,
        /// Its body, `null` when it was not JSON.
        body: Value,
    },
    /// The identity server could not be reached, or its answer could not be read.
    Unreachable(String),
}

/// What unbinding at one identity server came to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnbindOutcome {
    /// The identity server removed the binding.
    Unbound,
    /// The identity server does not support unbinding, or did not have the binding (it answered
    /// `400`, `404` or `501`, which Synapse treats the same way).
    NotSupported,
}

/// The identity-server calls binding and unbinding make, answered by `hs serve`
/// (`crate::identity_service` there) and installed through
/// [`AuthState::install_identity_server_client`]. Unset, binding is refused and unbinding
/// reports `no-support`.
#[async_trait]
pub trait IdentityServerClient: Send + Sync {
    /// Whether this server will contact `id_server` (`host[:port]`) at all
    /// (`auth.identity_servers`).
    fn allows(&self, id_server: &str) -> bool;

    /// `POST /_matrix/identity/v2/3pid/bind` with `Authorization: Bearer <id_access_token>` and
    /// `{sid, client_secret, mxid}`: the identity server's answer (`medium`, `address`, `mxid`,
    /// signatures).
    ///
    /// # Errors
    /// The identity server's refusal, or why it could not be reached.
    async fn bind(
        &self,
        id_server: &str,
        id_access_token: &str,
        sid: &str,
        client_secret: &str,
        mxid: &UserId,
    ) -> Result<Value, IdentityServerError>;

    /// `POST /_matrix/identity/v2/3pid/unbind` with `{mxid, threepid: {medium, address}}`,
    /// signed by this server as Synapse signs it.
    ///
    /// # Errors
    /// Why the identity server could not be reached, or a refusal other than "not supported".
    async fn unbind(
        &self,
        id_server: &str,
        mxid: &UserId,
        medium: &str,
        address: &str,
    ) -> Result<UnbindOutcome, IdentityServerError>;
}

/// What a validation session is for, which is also the path segment of its link.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Purpose {
    /// `POST /register/email/requestToken`: registering with the address.
    Registration,
    /// `POST /account/3pid/email/requestToken`: adding the address to an account.
    AddThreepid,
    /// `POST /account/password/email/requestToken`: resetting a password by the address.
    PasswordReset,
}

impl Purpose {
    /// The stored and path form: `registration`, `add_threepid`, `password_reset`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Registration => "registration",
            Self::AddThreepid => "add_threepid",
            Self::PasswordReset => "password_reset",
        }
    }

    fn subject(self, app_name: &str) -> String {
        match self {
            Self::Registration => format!("[{app_name}] Validate your email"),
            Self::AddThreepid => format!("[{app_name}] Validate your email"),
            Self::PasswordReset => format!("[{app_name}] Password reset"),
        }
    }

    fn explanation(self) -> &'static str {
        match self {
            Self::Registration => "to finish signing up",
            Self::AddThreepid => "to add this address to your account",
            Self::PasswordReset => "to reset your password",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, EncodeLabelSet)]
struct ValidationLabels {
    medium: &'static str,
    outcome: &'static str,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, EncodeLabelSet)]
struct ChangeLabels {
    action: &'static str,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, EncodeLabelSet)]
struct ResetLabels {
    outcome: &'static str,
}

static VALIDATIONS: LazyLock<Family<ValidationLabels, Counter>> = LazyLock::new(Family::default);
static CHANGES: LazyLock<Family<ChangeLabels, Counter>> = LazyLock::new(Family::default);
static PASSWORD_RESETS: LazyLock<Family<ResetLabels, Counter>> = LazyLock::new(Family::default);

/// Counts one signed-out password reset by email: `reset`, or `unknown_address` (the validated
/// address belongs to no account any more).
pub(crate) fn count_password_reset(outcome: &'static str) {
    PASSWORD_RESETS
        .get_or_create(&ResetLabels { outcome })
        .inc();
}

fn count_validation(outcome: &'static str) {
    VALIDATIONS
        .get_or_create(&ValidationLabels {
            medium: "email",
            outcome,
        })
        .inc();
}

/// Counts one change to a user's third-party identifiers: `added`, `deleted`, `bound`,
/// `unbound`.
pub(crate) fn count_change(action: &'static str) {
    CHANGES.get_or_create(&ChangeLabels { action }).inc();
}

/// Registers this module's counters into `registry`:
/// `hs_auth_threepid_validations_total{medium,outcome}` (`sent`, `send_failed`, `validated`,
/// `rejected`: a link with a wrong or expired token),
/// `hs_auth_threepid_changes_total{action}` (`added`, `deleted`, `bound`, `unbound`) and
/// `hs_auth_password_resets_total{outcome}` (`reset`, `unknown_address`).
pub fn register_metrics(registry: &mut prometheus_client::registry::Registry) {
    registry.register(
        "hs_auth_threepid_validations",
        "Third-party identifier validations, by medium and outcome: an email sent, an email \
         that could not be sent, a link followed, or a link refused",
        VALIDATIONS.clone(),
    );
    registry.register(
        "hs_auth_threepid_changes",
        "Changes to users' third-party identifiers, by action: added to or deleted from an \
         account, bound or unbound at an identity server",
        CHANGES.clone(),
    );
    registry.register(
        "hs_auth_password_resets",
        "Passwords reset by somebody signed out who proved they control the account's email \
         address, by outcome",
        PASSWORD_RESETS.clone(),
    );
}

/// Whether this server can validate email addresses now: it can send email and knows its own
/// public address. See the module docs.
#[must_use]
pub fn email_available(state: &AuthState) -> bool {
    state.config.get().public_baseurl.is_some()
        && state.email_sender().is_some_and(|s| s.can_send())
}

/// The spec's grammar for a `client_secret`: 1 to 255 characters of `[0-9a-zA-Z.=_-]`.
///
/// # Errors
/// `400 M_INVALID_PARAM` otherwise.
pub fn validate_client_secret(secret: &str) -> Result<(), MatrixError> {
    let ok = !secret.is_empty()
        && secret.len() <= 255
        && secret
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '=' | '_' | '-'));
    if ok {
        Ok(())
    } else {
        Err(MatrixError::invalid_param(
            "Invalid client_secret parameter",
        ))
    }
}

/// An email address as this server stores it: trimmed and lower-cased (as Synapse's
/// `canonicalise_email`), with one `@` between a non-empty local part and domain.
///
/// # Errors
/// `400 M_INVALID_PARAM` for something that is not an email address.
pub fn canonical_email(address: &str) -> Result<String, MatrixError> {
    let address = address.trim();
    let valid = match address.split_once('@') {
        Some((local, domain)) => {
            !local.is_empty()
                && !domain.is_empty()
                && !domain.contains('@')
                && !address.chars().any(char::is_whitespace)
        }
        None => false,
    };
    if !valid {
        return Err(MatrixError::invalid_param("Unable to parse email address"));
    }
    Ok(address.to_lowercase())
}

fn random_string(len: usize) -> String {
    std::iter::repeat_with(|| rand::rng().sample(Alphanumeric) as char)
        .take(len)
        .collect()
}

fn token_hash(token: &str) -> String {
    hex::encode(sha2::Sha256::digest(token.as_bytes()))
}

/// Percent-encodes a query value (the token, secret and session id are already from a safe
/// alphabet, but a `client_secret` may contain `=`).
#[must_use]
pub fn query_escape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for b in value.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

fn medium_not_supported(message: &str) -> MatrixError {
    MatrixError::new(
        StatusCode::BAD_REQUEST,
        ErrCode::ThreepidMediumNotSupported,
        message,
    )
}

/// Checks a client's `next_link`, as Synapse's `assert_valid_next_link`: an `http` or `https`
/// address (never `file:`, which would point at the person's own disk), on a host
/// `auth.next_link_domain_whitelist` lists when that is set. Returns the address to store.
///
/// # Errors
/// `400 M_INVALID_PARAM` otherwise.
pub fn check_next_link(state: &AuthState, next_link: &str) -> Result<String, MatrixError> {
    let refused = || {
        tracing::info!(next_link, "refused a validation email's next_link");
        MatrixError::invalid_param("'next_link' domain not included in whitelist, or not http(s)")
    };
    let parsed = reqwest::Url::parse(next_link).map_err(|_| refused())?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err(refused());
    }
    if let Some(allowed) = &state.config.get().next_link_domain_whitelist {
        let host = parsed.host_str().unwrap_or_default();
        if !allowed
            .iter()
            .any(|domain| domain.eq_ignore_ascii_case(host))
        {
            return Err(refused());
        }
    }
    Ok(next_link.to_owned())
}

/// `POST .../email/requestToken` for `purpose`: checks the request, and sends a validation
/// email unless this `(address, client_secret)` already has a session at this `send_attempt`.
/// Returns `{"sid": ...}`. `client_key` is the client's address, for the rate limit.
///
/// # Errors
/// `400 M_THREEPID_MEDIUM_NOT_SUPPORTED` while email cannot be sent; `400` for a missing or
/// malformed parameter; `400 M_THREEPID_IN_USE` (registering or adding an address already
/// bound) or `M_THREEPID_NOT_FOUND` (resetting a password by an address nobody has);
/// `403` registering while registration is closed; `429` over
/// `rate_limits.third_party_id_validation`; `500` if the email could not be sent.
pub async fn request_email_token(
    state: &AuthState,
    purpose: Purpose,
    body: &Value,
    client_key: Option<String>,
) -> Result<Value, MatrixError> {
    if !email_available(state) {
        return Err(medium_not_supported(
            "This server does not send email, so it cannot validate an email address",
        ));
    }
    if purpose == Purpose::Registration && !state.config.get().registration_enabled {
        return Err(MatrixError::forbidden(
            "Registration is disabled on this homeserver",
        ));
    }
    let client_secret = body
        .get("client_secret")
        .and_then(Value::as_str)
        .ok_or_else(|| MatrixError::missing_param("Missing client_secret"))?;
    validate_client_secret(client_secret)?;
    let address = canonical_email(
        body.get("email")
            .and_then(Value::as_str)
            .ok_or_else(|| MatrixError::missing_param("Missing email"))?,
    )?;
    let send_attempt = body
        .get("send_attempt")
        .and_then(|v| {
            v.as_u64()
                .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
        })
        .ok_or_else(|| MatrixError::missing_param("Missing send_attempt"))?;
    let next_link = match body.get("next_link") {
        None | Some(Value::Null) => None,
        Some(Value::String(link)) => Some(check_next_link(state, link)?),
        Some(_) => return Err(MatrixError::invalid_param("next_link must be a string")),
    };

    let owner = state.store.get_user_by_threepid("email", &address).await?;
    match purpose {
        Purpose::Registration | Purpose::AddThreepid if owner.is_some() => {
            return Err(MatrixError::new(
                StatusCode::BAD_REQUEST,
                ErrCode::ThreepidInUse,
                "Email is already in use",
            ));
        }
        Purpose::PasswordReset if owner.is_none() => {
            return Err(MatrixError::new(
                StatusCode::BAD_REQUEST,
                ErrCode::ThreepidNotFound,
                "Email not found",
            ));
        }
        _ => {}
    }

    let existing = state
        .store
        .find_unvalidated_session("email", &address, client_secret)
        .await?;
    if let Some(existing) = &existing
        && send_attempt <= existing.send_attempt
        && existing.purpose == purpose.as_str()
    {
        return Ok(json!({ "sid": existing.sid }));
    }

    let limits = &state.limits.third_party_id_validation;
    for key in client_key.iter().chain(std::iter::once(&address)) {
        limits.take_now(key).map_err(MatrixError::limit_exceeded)?;
    }

    let now = state.now_ms();
    let (sid, created_at_ms) = match existing {
        Some(existing) if existing.purpose == purpose.as_str() => {
            (existing.sid, existing.created_at_ms)
        }
        _ => (random_string(16), now),
    };
    let token = random_string(32);
    let base = state
        .config
        .get()
        .public_baseurl
        .clone()
        .unwrap_or_default();
    let link = format!(
        "{base}/_matrix/client/unstable/{}/email/submit_token?token={}&client_secret={}&sid={}",
        purpose.as_str(),
        query_escape(&token),
        query_escape(client_secret),
        query_escape(&sid),
    );
    let Some(sender) = state.email_sender() else {
        return Err(medium_not_supported(
            "This server does not send email, so it cannot validate an email address",
        ));
    };
    let email = OutgoingEmail {
        to: address.clone(),
        subject: purpose.subject(&sender.app_name()),
        text: format!(
            "Hello,\n\nFollow this link {}:\n\n{link}\n\nIf you did not ask for this, you can \
             ignore this email.\n",
            purpose.explanation()
        ),
        html: format!(
            "<!doctype html><html><body><p>Hello,</p><p>Follow <a href=\"{0}\">this link</a> \
             {1}:</p><p><a href=\"{0}\">{0}</a></p><p>If you did not ask for this, you can \
             ignore this email.</p></body></html>",
            html_escape(&link),
            purpose.explanation()
        ),
    };
    if let Err(error) = sender.send(email).await {
        count_validation("send_failed");
        tracing::warn!(%error, purpose = purpose.as_str(), "could not send a validation email");
        return Err(MatrixError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            ErrCode::Unknown,
            "An error was encountered when sending the email",
        ));
    }
    state
        .store
        .put_validation_session(ThreepidValidationRecord {
            sid: sid.clone(),
            medium: "email".to_owned(),
            address,
            client_secret: client_secret.to_owned(),
            purpose: purpose.as_str().to_owned(),
            token_hash: token_hash(&token),
            send_attempt,
            created_at_ms,
            token_expires_at_ms: now + TOKEN_LIFETIME_MS,
            validated_at_ms: None,
            next_link,
        })
        .await?;
    count_validation("sent");
    tracing::info!(
        sid,
        purpose = purpose.as_str(),
        send_attempt,
        "sent an email address validation"
    );
    Ok(json!({ "sid": sid }))
}

/// Escapes text for an HTML page.
#[must_use]
pub fn html_escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
    out
}

/// Follows a validation link: marks `sid` validated if `client_secret` and `token` are its and
/// the link has not expired. Following a link twice is not an error.
///
/// # Errors
/// `400 M_THREEPID_AUTH_FAILED` for an unknown session, a wrong secret, token or purpose, or an
/// expired link.
pub async fn submit_token(
    state: &AuthState,
    purpose: Purpose,
    sid: &str,
    client_secret: &str,
    token: &str,
) -> Result<ThreepidValidationRecord, MatrixError> {
    let refused = || {
        count_validation("rejected");
        MatrixError::new(
            StatusCode::BAD_REQUEST,
            ErrCode::ThreepidAuthFailed,
            "This validation link is not valid, or has expired",
        )
    };
    let Some(mut record) = state.store.get_validation_session(sid).await? else {
        return Err(refused());
    };
    if record.client_secret != client_secret
        || record.purpose != purpose.as_str()
        || !crate::store::tokens_match(&record.token_hash, &token_hash(token))
    {
        return Err(refused());
    }
    if record.validated_at_ms.is_some() {
        return Ok(record);
    }
    let now = state.now_ms();
    if now > record.token_expires_at_ms {
        return Err(refused());
    }
    record.validated_at_ms = Some(now);
    state.store.put_validation_session(record.clone()).await?;
    count_validation("validated");
    tracing::info!(
        sid,
        purpose = purpose.as_str(),
        "an email address was validated"
    );
    Ok(record)
}

/// The validated session `{sid, client_secret}` names, if there is one still usable
/// ([`VALIDATED_SESSION_LIFETIME_MS`]). What the `m.login.email.identity` stage and
/// `POST /account/3pid/add` accept.
///
/// # Errors
/// A storage failure.
pub async fn validated_session(
    state: &AuthState,
    sid: &str,
    client_secret: &str,
) -> Result<Option<ThreepidValidationRecord>, MatrixError> {
    let Some(record) = state.store.get_validation_session(sid).await? else {
        return Ok(None);
    };
    let usable = record.client_secret == client_secret
        && record.validated_at_ms.is_some()
        && state.now_ms() <= record.created_at_ms + VALIDATED_SESSION_LIFETIME_MS;
    Ok(usable.then_some(record))
}

/// What a signed-out `POST /account/password`'s `m.login.email.identity` stage proves: the
/// address of the validated password-reset session `{sid, client_secret}` names (a session
/// started by `/account/password/email/requestToken`, whose link was followed and confirmed).
/// `None` for an unknown, unvalidated, expired or other-purpose session.
///
/// # Errors
/// A storage failure.
pub async fn password_reset_address(
    state: &AuthState,
    sid: &str,
    client_secret: &str,
) -> Result<Option<String>, MatrixError> {
    Ok(validated_session(state, sid, client_secret)
        .await?
        .filter(|r| r.medium == "email" && r.purpose == Purpose::PasswordReset.as_str())
        .map(|r| r.address))
}

/// The account whose password a reset by `address` changes: the one the address is bound to.
///
/// # Errors
/// `404 M_NOT_FOUND` when no account has it (any more), as Synapse answers.
pub async fn password_reset_owner(
    state: &AuthState,
    address: &str,
) -> Result<ruma::OwnedUserId, MatrixError> {
    match state.store.get_user_by_threepid("email", address).await? {
        Some(user_id) => Ok(user_id),
        None => {
            count_password_reset("unknown_address");
            Err(MatrixError::new(
                StatusCode::NOT_FOUND,
                ErrCode::NotFound,
                "Email address not found",
            ))
        }
    }
}

/// Adds a validated address to `user_id`'s account.
///
/// # Errors
/// `400 M_THREEPID_IN_USE` if another account has it.
pub async fn add_validated(
    state: &AuthState,
    user_id: &UserId,
    record: &ThreepidValidationRecord,
) -> Result<(), MatrixError> {
    let now = state.now_ms();
    match state
        .store
        .add_threepid(crate::store::ThreepidRecord {
            user_id: user_id.to_owned(),
            medium: record.medium.clone(),
            address: record.address.clone(),
            added_at_ms: now,
            validated_at_ms: record.validated_at_ms.unwrap_or(now),
        })
        .await
    {
        Ok(()) => {
            count_change("added");
            tracing::info!(user = %user_id, medium = record.medium, "added a third-party identifier");
            Ok(())
        }
        Err(crate::store::StoreError::Conflict(_)) => Err(MatrixError::new(
            StatusCode::BAD_REQUEST,
            ErrCode::ThreepidInUse,
            "This third-party identifier is already in use",
        )),
        Err(e) => Err(e.into()),
    }
}

fn not_trusted(id_server: &str) -> MatrixError {
    MatrixError::new(
        StatusCode::BAD_REQUEST,
        ErrCode::ServerNotTrusted,
        format!("{id_server} is not an identity server this server uses (auth.identity_servers)"),
    )
}

fn identity_server_failed(error: &IdentityServerError) -> MatrixError {
    match error {
        IdentityServerError::Refused { status, body } => {
            // The identity server's own refusal, passed on with its status when that is a
            // client error (a session it never validated, a bad access token), as Synapse does.
            let errcode = body["errcode"].as_str().unwrap_or("M_UNKNOWN");
            let message = body["error"]
                .as_str()
                .unwrap_or("The identity server refused the request");
            let status = StatusCode::from_u16(*status)
                .ok()
                .filter(|s| s.is_client_error())
                .unwrap_or(StatusCode::BAD_GATEWAY);
            MatrixError::new(
                status,
                ErrCode::Unknown,
                format!("The identity server refused the request ({errcode}): {message}"),
            )
        }
        IdentityServerError::Unreachable(reason) => MatrixError::new(
            StatusCode::BAD_GATEWAY,
            ErrCode::Unknown,
            format!("Could not contact the identity server: {reason}"),
        ),
    }
}

/// Binds an identifier the identity server validated (`sid`, `client_secret` are the identity
/// server's) to `user_id` there, and remembers where. Returns the identity server's answer.
///
/// # Errors
/// `400 M_SERVER_NOT_TRUSTED` for an identity server this server does not use (or none
/// installed); the identity server's refusal; `502` if it could not be reached or answered
/// without saying what it bound.
pub async fn bind(
    state: &AuthState,
    user_id: &UserId,
    id_server: &str,
    id_access_token: &str,
    sid: &str,
    client_secret: &str,
) -> Result<Value, MatrixError> {
    let Some(client) = state
        .identity_server_client()
        .filter(|c| c.allows(id_server))
    else {
        return Err(not_trusted(id_server));
    };
    let answer = client
        .bind(id_server, id_access_token, sid, client_secret, user_id)
        .await
        .map_err(|e| {
            tracing::info!(user = %user_id, id_server, error = ?e, "binding a third-party identifier failed");
            identity_server_failed(&e)
        })?;
    let (Some(medium), Some(address)) = (answer["medium"].as_str(), answer["address"].as_str())
    else {
        return Err(MatrixError::new(
            StatusCode::BAD_GATEWAY,
            ErrCode::Unknown,
            "The identity server did not say what it bound",
        ));
    };
    state
        .store
        .add_threepid_binding(ThreepidBindingRecord {
            user_id: user_id.to_owned(),
            medium: medium.to_owned(),
            address: address.to_owned(),
            id_server: id_server.to_owned(),
            bound_at_ms: state.now_ms(),
        })
        .await?;
    count_change("bound");
    tracing::info!(user = %user_id, medium, id_server, "bound a third-party identifier at an identity server");
    Ok(answer)
}

/// Unbinds `(medium, address)` from `user_id` at `id_server`, or, with none named, at every
/// identity server this server bound it at. `true` when every identity server tried removed
/// it; `false` when there was nowhere to try, or one does not support unbinding -- the spec's
/// `id_server_unbind_result` of `success` and `no-support`. Each binding tried is forgotten.
///
/// # Errors
/// `400 M_SERVER_NOT_TRUSTED` for a named identity server this server does not use; `502`
/// when an identity server could not be reached or failed otherwise.
pub async fn unbind(
    state: &AuthState,
    user_id: &UserId,
    medium: &str,
    address: &str,
    id_server: Option<&str>,
) -> Result<bool, MatrixError> {
    let id_servers: Vec<String> = match id_server {
        Some(id_server) => vec![id_server.to_owned()],
        None => state
            .store
            .list_threepid_bindings(user_id)
            .await?
            .into_iter()
            .filter(|b| b.medium == medium && b.address.eq_ignore_ascii_case(address))
            .map(|b| b.id_server)
            .collect(),
    };
    if id_servers.is_empty() {
        return Ok(false);
    }
    let Some(client) = state.identity_server_client() else {
        return Ok(false);
    };
    let mut all_unbound = true;
    for id_server in id_servers {
        if !client.allows(&id_server) {
            return Err(not_trusted(&id_server));
        }
        let outcome = client
            .unbind(&id_server, user_id, medium, address)
            .await
            .map_err(|e| {
                tracing::warn!(user = %user_id, id_server, error = ?e, "unbinding a third-party identifier failed");
                identity_server_failed(&e)
            })?;
        state
            .store
            .remove_threepid_binding(user_id, medium, address, &id_server)
            .await?;
        match outcome {
            UnbindOutcome::Unbound => {
                count_change("unbound");
                tracing::info!(user = %user_id, medium, id_server, "unbound a third-party identifier at an identity server");
            }
            UnbindOutcome::NotSupported => {
                tracing::info!(user = %user_id, medium, id_server, "the identity server does not support unbinding");
                all_unbound = false;
            }
        }
    }
    Ok(all_unbound)
}

/// What deactivating `user_id` does to its third-party identifiers, as Synapse's
/// `DeactivateAccountHandler`: unbinds every one this server bound at an identity server, then
/// removes every one bound to the account here. `true` when every unbind succeeded (and so when
/// there was nothing to unbind).
///
/// # Errors
/// As [`unbind`], and storage failures.
pub async fn on_deactivation(state: &AuthState, user_id: &UserId) -> Result<bool, MatrixError> {
    let mut all_unbound = true;
    let bindings = state.store.list_threepid_bindings(user_id).await?;
    let mut seen = std::collections::BTreeSet::new();
    for binding in bindings {
        if seen.insert((binding.medium.clone(), binding.address.to_ascii_lowercase())) {
            all_unbound &= unbind(state, user_id, &binding.medium, &binding.address, None).await?;
        }
    }
    for threepid in state.store.list_threepids(user_id).await? {
        match state
            .store
            .remove_threepid(user_id, &threepid.medium, &threepid.address)
            .await
        {
            Ok(()) | Err(crate::store::StoreError::NotFound(_)) => count_change("deleted"),
            Err(e) => return Err(e.into()),
        }
    }
    Ok(all_unbound)
}

/// What an administrator deactivating `user_id` does to its third-party identifiers
/// (`users.deactivate`, Synapse's admin deactivation): the same as [`on_deactivation`] --
/// every binding this server made is unbound at its identity server, then every identifier is
/// removed from the account -- except that an identity server which cannot be asked does not
/// undo the administrator's decision. Its failure is logged at `WARN` and its binding kept (so
/// the account's owner, or a later attempt, still knows where it was), and the rest go on.
/// Returns how many bindings could not be unbound.
///
/// # Errors
/// Storage failures.
pub async fn on_admin_deactivation(
    state: &AuthState,
    user_id: &UserId,
) -> Result<usize, MatrixError> {
    let mut failed = 0;
    let bindings = state.store.list_threepid_bindings(user_id).await?;
    let mut seen = std::collections::BTreeSet::new();
    for binding in bindings {
        if !seen.insert((binding.medium.clone(), binding.address.to_ascii_lowercase())) {
            continue;
        }
        if let Err(error) = unbind(state, user_id, &binding.medium, &binding.address, None).await {
            failed += 1;
            tracing::warn!(
                user = %user_id,
                medium = binding.medium,
                id_server = binding.id_server,
                %error,
                "could not unbind a deactivated account's third-party identifier"
            );
        }
    }
    for threepid in state.store.list_threepids(user_id).await? {
        match state
            .store
            .remove_threepid(user_id, &threepid.medium, &threepid.address)
            .await
        {
            Ok(()) | Err(crate::store::StoreError::NotFound(_)) => count_change("deleted"),
            Err(e) => return Err(e.into()),
        }
    }
    Ok(failed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_secrets_follow_the_spec_grammar() {
        assert!(validate_client_secret("abcDEF123._=-").is_ok());
        assert!(validate_client_secret("12345678901234567890Az.=_-").is_ok());
        assert!(validate_client_secret("").is_err());
        assert!(validate_client_secret("has space").is_err());
        assert!(validate_client_secret("slash/").is_err());
        assert!(validate_client_secret(&"a".repeat(256)).is_err());
    }

    #[test]
    fn email_addresses_are_canonicalised() {
        assert_eq!(
            canonical_email(" Bob@Example.COM ").unwrap(),
            "bob@example.com"
        );
        assert!(canonical_email("nobody").is_err());
        assert!(canonical_email("@example.com").is_err());
        assert!(canonical_email("a@").is_err());
        assert!(canonical_email("a@b@c").is_err());
        assert!(canonical_email("a b@c").is_err());
    }

    #[test]
    fn query_values_are_escaped() {
        assert_eq!(query_escape("ab.c=_-"), "ab.c%3D_-");
    }
}
