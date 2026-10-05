//! Sign-in through a CAS server (Apereo CAS, CAS protocol 2 and 3): `auth.cas`, Synapse's
//! `cas_config`.
//!
//! The flow, as Synapse runs it (`synapse/handlers/cas.py`, read for behaviour only):
//!
//! 1. The client opens `GET /login/sso/redirect?redirectUrl=<client>` (or the older
//!    `/login/cas/redirect`); this server answers `302` to `<server_url>/login?service=<service>`,
//!    where the *service* is this server's own ticket endpoint with the client's address in it:
//!    `<public_baseurl>/_matrix/client/r0/login/cas/ticket?redirectUrl=<client>` ([`service_url`]).
//!    The `r0` is Synapse's spelling, kept because a CAS administrator registers the exact
//!    service address with the CAS server, and Sytest asserts it.
//! 2. The person signs in at CAS, which sends them back to the service address with
//!    `&ticket=<ticket>` added.
//! 3. This server checks the ticket at `<server_url>/proxyValidate?ticket=..&service=..` (the
//!    service exactly as it was sent in step 1, which CAS compares), and reads the CAS user name
//!    and attributes out of the XML answer ([`parse_response`]).
//! 4. The CAS user name becomes a localpart ([`map_username_to_localpart`], the spec's "mapping
//!    from other character sets"): an account linked to it before (external id `cas`), or an
//!    existing account with that localpart, signs in; otherwise the account is created. A
//!    short-lived login token is minted and the person is shown a page linking back to the
//!    client with `loginToken=<token>`, which the client trades at `POST /login`
//!    (`m.login.token`).
//!
//! The same ticket check also answers user-interactive auth's `m.login.sso` stage: the service
//! then carries `session=<uia session>` instead of a client address, and the account CAS vouched
//! for is recorded on the session ([`crate::uia::record_authenticated_user`]) for
//! [`crate::reauth`] to compare with the requester.
//!
//! The ticket check is behind [`CasValidator`] so tests can answer it; [`HttpCasValidator`] is
//! the real one. The routes are in [`crate::routes::sso`].

use std::collections::BTreeMap;
use std::sync::{LazyLock, OnceLock};
use std::time::Duration;

use prometheus_client::encoding::EncodeLabelSet;
use prometheus_client::metrics::counter::Counter;
use prometheus_client::metrics::family::Family;
use quick_xml::events::Event;

use crate::config::{AuthConfig, CasSettings};

/// The external-id provider name an account signed in through CAS is linked under, and the id
/// of the identity provider `GET /login` lists.
pub const PROVIDER: &str = "cas";

/// This server's ticket endpoint, relative to the service base: the address CAS sends people
/// back to.
pub const TICKET_PATH: &str = "/_matrix/client/r0/login/cas/ticket";

/// How long a ticket check may take.
const VALIDATE_TIMEOUT: Duration = Duration::from_secs(15);

/// Why a CAS sign-in did not complete.
#[derive(Debug, thiserror::Error)]
pub enum CasError {
    /// Neither `auth.cas.service_url` nor `server.public_baseurl` is set, so there is no address
    /// CAS could send people back to.
    #[error("auth.cas needs server.public_baseurl (or auth.cas.service_url) to be set")]
    NoServiceBase,
    /// The CAS server could not be reached, or answered with something other than `200`.
    #[error("the CAS server could not check the ticket: {0}")]
    Transport(String),
    /// The CAS server refused the ticket (`cas:authenticationFailure`).
    #[error("the CAS server refused the ticket: {0}")]
    Rejected(String),
    /// The CAS server's answer was not a CAS service response.
    #[error("the CAS server's answer could not be read: {0}")]
    Malformed(String),
}

/// What a successful ticket check says about the person.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CasResponse {
    /// The CAS user name (`cas:user`).
    pub user: String,
    /// The attributes CAS released (`cas:attributes`), each with every value it had.
    pub attributes: BTreeMap<String, Vec<String>>,
}

/// Percent-encodes everything but RFC 3986's unreserved characters, as Python's
/// `urllib.parse.urlencode` and Perl's `uri_escape` do: the service address is compared as a
/// string by CAS servers (and by Sytest), so it must be spelt the same way every time.
#[must_use]
pub fn encode_component(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for byte in raw.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            out.push(char::from(byte));
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// The service address for one sign-in: [`TICKET_PATH`] on `auth.cas.service_url` (or
/// `server.public_baseurl`), with `arg` (`("redirectUrl", <client>)` or `("session", <id>)`) as
/// its query.
///
/// # Errors
/// [`CasError::NoServiceBase`] when neither base is configured.
pub fn service_url(
    config: &AuthConfig,
    cas: &CasSettings,
    arg: (&str, &str),
) -> Result<String, CasError> {
    let base = cas
        .service_base
        .as_deref()
        .or(config.public_baseurl.as_deref())
        .ok_or(CasError::NoServiceBase)?;
    Ok(format!(
        "{base}{TICKET_PATH}?{}={}",
        arg.0,
        encode_component(arg.1)
    ))
}

/// Where a person is sent to sign in: `<server_url>/login?service=<service>`.
#[must_use]
pub fn login_url(cas: &CasSettings, service: &str) -> String {
    format!(
        "{}/login?service={}",
        cas.server_url,
        encode_component(service)
    )
}

/// Reads a CAS 2/3 service response (`<cas:serviceResponse>`). Namespace prefixes are ignored:
/// the elements are matched by local name, as CAS servers differ in the prefix they use.
///
/// # Errors
/// [`CasError::Rejected`] for a `cas:authenticationFailure` (with its text), and
/// [`CasError::Malformed`] for anything that is not a success with a non-empty `cas:user`.
pub fn parse_response(body: &str) -> Result<CasResponse, CasError> {
    let mut reader = quick_xml::Reader::from_str(body);
    let mut stack: Vec<String> = Vec::new();
    let mut text = String::new();
    let mut user: Option<String> = None;
    let mut attributes: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut failure: Option<String> = None;
    let mut success = false;

    loop {
        let event = reader
            .read_event()
            .map_err(|e| CasError::Malformed(e.to_string()))?;
        match event {
            Event::Start(e) => {
                let name = String::from_utf8_lossy(e.local_name().as_ref()).into_owned();
                if name == "authenticationSuccess" {
                    success = true;
                }
                if name == "authenticationFailure" {
                    failure = Some(String::new());
                }
                stack.push(name);
                text.clear();
            }
            Event::Empty(e) => {
                let name = String::from_utf8_lossy(e.local_name().as_ref()).into_owned();
                if name == "authenticationFailure" {
                    failure = Some(String::new());
                }
            }
            Event::Text(t) => {
                let decoded = t.decode().map_err(|e| CasError::Malformed(e.to_string()))?;
                text.push_str(&decoded);
            }
            Event::CData(t) => {
                let decoded = t.decode().map_err(|e| CasError::Malformed(e.to_string()))?;
                text.push_str(&decoded);
            }
            Event::GeneralRef(r) => {
                if let Some(ch) = r
                    .resolve_char_ref()
                    .map_err(|e| CasError::Malformed(e.to_string()))?
                {
                    text.push(ch);
                } else {
                    let name = r.decode().map_err(|e| CasError::Malformed(e.to_string()))?;
                    match quick_xml::escape::resolve_predefined_entity(&name) {
                        Some(value) => text.push_str(value),
                        None => {
                            return Err(CasError::Malformed(format!("unknown entity &{name};")));
                        }
                    }
                }
            }
            Event::End(_) => {
                let value = text.trim().to_owned();
                text.clear();
                let Some(name) = stack.pop() else {
                    return Err(CasError::Malformed("unbalanced element".to_owned()));
                };
                let parent = stack.last().map(String::as_str);
                let in_success = stack.iter().any(|s| s == "authenticationSuccess");
                if name == "user" && parent == Some("authenticationSuccess") {
                    user = Some(value);
                } else if parent == Some("attributes") && in_success {
                    attributes.entry(name).or_default().push(value);
                } else if name == "authenticationFailure" {
                    failure = Some(value);
                }
            }
            Event::Eof => break,
            _ => {}
        }
    }

    if let Some(reason) = failure {
        return Err(CasError::Rejected(if reason.is_empty() {
            "authentication failed".to_owned()
        } else {
            reason
        }));
    }
    if !success {
        return Err(CasError::Malformed(
            "no cas:authenticationSuccess element".to_owned(),
        ));
    }
    match user {
        Some(user) if !user.is_empty() => Ok(CasResponse { user, attributes }),
        _ => Err(CasError::Malformed(
            "no cas:user in the response".to_owned(),
        )),
    }
}

/// Whether `response` has every attribute `required` names: with the value given, or with any
/// value for `None`. Synapse's `SsoAttributeRequirement`.
#[must_use]
pub fn meets_requirements(
    response: &CasResponse,
    required: &BTreeMap<String, Option<String>>,
) -> bool {
    required
        .iter()
        .all(|(name, wanted)| match response.attributes.get(name) {
            None => false,
            Some(values) => wanted
                .as_ref()
                .is_none_or(|wanted| values.iter().any(|v| v == wanted)),
        })
}

/// Maps a user name from another system onto a localpart, as the spec's appendix "Mapping from
/// other character sets" describes and Synapse's `map_username_to_mxid_localpart` does: lower
/// case, every byte outside `a-z 0-9 _ - . / +` written `=xx` in hex (UTF-8 bytes for non-ASCII
/// characters), and a leading `_` written `=5f`. `cas_user!` becomes `cas_user=21`.
#[must_use]
pub fn map_username_to_localpart(username: &str) -> String {
    let lower = username.to_lowercase();
    let mut out = String::with_capacity(lower.len());
    for byte in lower.bytes() {
        if byte.is_ascii_lowercase()
            || byte.is_ascii_digit()
            || matches!(byte, b'_' | b'-' | b'.' | b'/' | b'+')
        {
            out.push(char::from(byte));
        } else {
            out.push_str(&format!("={byte:02x}"));
        }
    }
    if let Some(rest) = out.strip_prefix('_') {
        out = format!("=5f{rest}");
    }
    out
}

/// Checks a ticket with the CAS server: `GET <server_url>/proxyValidate?ticket=..&service=..`,
/// answering the response body.
#[async_trait::async_trait]
pub trait CasValidator: Send + Sync {
    /// The CAS server's answer to the ticket check.
    ///
    /// # Errors
    /// [`CasError::Transport`] when the server cannot be reached or answers other than `200`.
    async fn validate(
        &self,
        server_url: &str,
        ticket: &str,
        service: &str,
    ) -> Result<String, CasError>;
}

/// The [`CasValidator`] over HTTP, with the workspace's shared trust roots. Plain `http://` and
/// loopback addresses are allowed: the operator chose the address in `auth.cas.server_url`.
#[derive(Default)]
pub struct HttpCasValidator {
    client: OnceLock<reqwest::Client>,
}

impl HttpCasValidator {
    fn client(&self) -> Result<&reqwest::Client, CasError> {
        if let Some(client) = self.client.get() {
            return Ok(client);
        }
        let client = hs_http::client::builder()
            .timeout(VALIDATE_TIMEOUT)
            .build()
            .map_err(|e| CasError::Transport(e.to_string()))?;
        Ok(self.client.get_or_init(|| client))
    }
}

#[async_trait::async_trait]
impl CasValidator for HttpCasValidator {
    async fn validate(
        &self,
        server_url: &str,
        ticket: &str,
        service: &str,
    ) -> Result<String, CasError> {
        let url = format!(
            "{server_url}/proxyValidate?ticket={}&service={}",
            encode_component(ticket),
            encode_component(service)
        );
        let response = self
            .client()?
            .get(&url)
            .send()
            .await
            .map_err(|e| CasError::Transport(e.to_string()))?;
        let status = response.status();
        let body = response
            .text()
            .await
            .map_err(|e| CasError::Transport(e.to_string()))?;
        if !status.is_success() {
            return Err(CasError::Transport(format!("HTTP {status}")));
        }
        Ok(body)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, EncodeLabelSet)]
struct SsoLabels {
    provider: &'static str,
    outcome: &'static str,
}

static SSO_LOGINS: LazyLock<Family<SsoLabels, Counter>> = LazyLock::new(Family::default);

/// Counts one single-sign-on completion: `outcome` is `login` (an existing account),
/// `registered` (an account created at its first sign-in), `ui_auth` (a user-interactive auth
/// stage passed), `ui_auth_mismatch` (the provider vouched for another account) or `failed`.
pub(crate) fn count(outcome: &'static str) {
    SSO_LOGINS
        .get_or_create(&SsoLabels {
            provider: PROVIDER,
            outcome,
        })
        .inc();
}

/// Registers `hs_auth_sso_logins_total{provider,outcome}` (see [`count`]'s outcomes) into
/// `registry`.
pub fn register_metrics(registry: &mut prometheus_client::registry::Registry) {
    // Registered without `_total`: the text encoder appends it.
    registry.register(
        "hs_auth_sso_logins",
        "Single sign-on completions, by provider and outcome: login, registered, ui_auth, \
         ui_auth_mismatch, failed",
        SSO_LOGINS.clone(),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    const SUCCESS: &str = "<cas:serviceResponse xmlns:cas='http://www.yale.edu/tp/cas'>
    <cas:authenticationSuccess>
         <cas:user>cas_user!</cas:user>
         <cas:attributes></cas:attributes>
    </cas:authenticationSuccess>
</cas:serviceResponse>";

    fn settings() -> CasSettings {
        CasSettings {
            server_url: "https://cas.example.edu/cas".to_owned(),
            service_base: None,
            displayname_attribute: None,
            required_attributes: BTreeMap::new(),
            idp_name: "CAS".to_owned(),
        }
    }

    #[test]
    fn reads_the_user_from_a_success() {
        let response = parse_response(SUCCESS).unwrap();
        assert_eq!(response.user, "cas_user!");
        assert!(response.attributes.is_empty());
    }

    #[test]
    fn reads_attributes_with_every_value_and_entities() {
        let body = r#"<cas:serviceResponse xmlns:cas="http://www.yale.edu/tp/cas">
  <cas:authenticationSuccess>
    <cas:user>jdoe</cas:user>
    <cas:attributes>
      <cas:displayName>Jane &amp; Doe</cas:displayName>
      <cas:memberOf>staff</cas:memberOf>
      <cas:memberOf>admins</cas:memberOf>
      <cas:note><![CDATA[a <b>]]></cas:note>
    </cas:attributes>
  </cas:authenticationSuccess>
</cas:serviceResponse>"#;
        let response = parse_response(body).unwrap();
        assert_eq!(response.user, "jdoe");
        assert_eq!(response.attributes["displayName"], vec!["Jane & Doe"]);
        assert_eq!(response.attributes["memberOf"], vec!["staff", "admins"]);
        assert_eq!(response.attributes["note"], vec!["a <b>"]);
    }

    #[test]
    fn a_failure_is_a_rejection_with_its_text() {
        let body = r#"<cas:serviceResponse xmlns:cas="http://www.yale.edu/tp/cas">
  <cas:authenticationFailure code="INVALID_TICKET">Ticket ST-1 not recognized</cas:authenticationFailure>
</cas:serviceResponse>"#;
        match parse_response(body) {
            Err(CasError::Rejected(reason)) => assert_eq!(reason, "Ticket ST-1 not recognized"),
            other => panic!("expected a rejection, got {other:?}"),
        }
    }

    #[test]
    fn something_else_is_malformed() {
        assert!(matches!(
            parse_response("<html>nope</html>"),
            Err(CasError::Malformed(_))
        ));
        assert!(matches!(parse_response(""), Err(CasError::Malformed(_))));
        assert!(matches!(
            parse_response(
                "<cas:serviceResponse xmlns:cas='x'><cas:authenticationSuccess><cas:user></cas:user></cas:authenticationSuccess></cas:serviceResponse>"
            ),
            Err(CasError::Malformed(_))
        ));
    }

    #[test]
    fn maps_user_names_onto_localparts_as_synapse_does() {
        assert_eq!(map_username_to_localpart("cas_user!"), "cas_user=21");
        assert_eq!(map_username_to_localpart("Jane.Doe"), "jane.doe");
        assert_eq!(map_username_to_localpart("_hidden"), "=5fhidden");
        assert_eq!(map_username_to_localpart("a=b"), "a=3db");
        assert_eq!(map_username_to_localpart("é"), "=c3=a9");
        assert_eq!(map_username_to_localpart("x y@z"), "x=20y=40z");
    }

    #[test]
    fn the_service_address_is_the_r0_ticket_endpoint_with_the_client_escaped() {
        let config = AuthConfig {
            public_baseurl: Some("https://localhost:8800".to_owned()),
            ..AuthConfig::default()
        };
        let service = service_url(
            &config,
            &settings(),
            ("redirectUrl", "https://client?p=http%3A%2F%2Fserver"),
        )
        .unwrap();
        // What Sytest's `matrix_login_with_cas` builds with Perl's `uri_escape`.
        assert_eq!(
            service,
            "https://localhost:8800/_matrix/client/r0/login/cas/ticket?redirectUrl=https%3A%2F%2Fclient%3Fp%3Dhttp%253A%252F%252Fserver"
        );
        assert_eq!(
            login_url(&settings(), "https://h/x?a=b"),
            "https://cas.example.edu/cas/login?service=https%3A%2F%2Fh%2Fx%3Fa%3Db"
        );
        let mut overridden = settings();
        overridden.service_base = Some("https://other.example".to_owned());
        assert!(
            service_url(&config, &overridden, ("session", "abc"))
                .unwrap()
                .starts_with(
                    "https://other.example/_matrix/client/r0/login/cas/ticket?session=abc"
                )
        );
        assert!(matches!(
            service_url(&AuthConfig::default(), &settings(), ("session", "abc")),
            Err(CasError::NoServiceBase)
        ));
    }

    #[test]
    fn required_attributes_need_the_value_or_presence() {
        let response = CasResponse {
            user: "u".to_owned(),
            attributes: BTreeMap::from([("group".to_owned(), vec!["staff".to_owned()])]),
        };
        assert!(meets_requirements(&response, &BTreeMap::new()));
        assert!(meets_requirements(
            &response,
            &BTreeMap::from([("group".to_owned(), Some("staff".to_owned()))])
        ));
        assert!(meets_requirements(
            &response,
            &BTreeMap::from([("group".to_owned(), None)])
        ));
        assert!(!meets_requirements(
            &response,
            &BTreeMap::from([("group".to_owned(), Some("admins".to_owned()))])
        ));
        assert!(!meets_requirements(
            &response,
            &BTreeMap::from([("dept".to_owned(), None)])
        ));
    }
}
