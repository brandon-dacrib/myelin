//! The `m.login.recaptcha` registration stage: checking a CAPTCHA answer against the CAPTCHA
//! service's verification API (`auth.recaptcha` in the configuration, Synapse's `recaptcha_*`).
//!
//! The check is Google reCAPTCHA's `siteverify` protocol, which hCaptcha and others also speak:
//! `POST` a form with `secret` (this server's private key), `response` (what the client's widget
//! produced) and `remoteip`, and read `success` from the JSON answer. Behaviour read from
//! Synapse's `RecaptchaAuthChecker` (`synapse/handlers/ui_auth/checkers.py`): a transport failure
//! or an answer without `success: true` fails the stage, and the client may try again.
//!
//! The HTTP call sits behind [`RecaptchaVerifier`] so tests can stand in for the service.

use std::sync::LazyLock;
use std::time::Duration;

use prometheus_client::encoding::EncodeLabelSet;
use prometheus_client::metrics::counter::Counter;
use prometheus_client::metrics::family::Family;

/// One CAPTCHA answer to check.
#[derive(Debug, Clone)]
pub struct RecaptchaCheck<'a> {
    /// The verification endpoint (`auth.recaptcha.siteverify_api`).
    pub siteverify_api: &'a str,
    /// This server's secret key.
    pub private_key: &'a str,
    /// The answer the client submitted (`auth.response`).
    pub response: &'a str,
    /// The client's address, when known.
    pub remote_ip: Option<&'a str>,
}

/// Asks the CAPTCHA service whether an answer is right.
#[async_trait::async_trait]
pub trait RecaptchaVerifier: Send + Sync {
    /// `Ok(true)` when the service says the answer is right.
    ///
    /// # Errors
    /// Why the service could not be asked or answered something unreadable; the stage fails.
    async fn verify(&self, check: &RecaptchaCheck<'_>) -> Result<bool, String>;
}

/// The [`RecaptchaVerifier`] over HTTPS (or HTTP, for a service the operator runs locally).
pub struct HttpRecaptchaVerifier {
    client: reqwest::Client,
}

impl HttpRecaptchaVerifier {
    /// A verifier with the workspace's shared roots and a ten-second limit per check.
    ///
    /// # Errors
    /// If the HTTP client cannot be built.
    pub fn new() -> Result<Self, reqwest::Error> {
        Ok(Self {
            client: hs_http::client::builder()
                .timeout(Duration::from_secs(10))
                .build()?,
        })
    }
}

#[async_trait::async_trait]
impl RecaptchaVerifier for HttpRecaptchaVerifier {
    async fn verify(&self, check: &RecaptchaCheck<'_>) -> Result<bool, String> {
        let mut form = vec![("secret", check.private_key), ("response", check.response)];
        if let Some(ip) = check.remote_ip {
            form.push(("remoteip", ip));
        }
        let answer = self
            .client
            .post(check.siteverify_api)
            .form(&form)
            .send()
            .await
            .map_err(|e| format!("could not reach the CAPTCHA service: {e}"))?;
        if !answer.status().is_success() {
            return Err(format!("the CAPTCHA service answered {}", answer.status()));
        }
        let body: serde_json::Value = answer
            .json()
            .await
            .map_err(|e| format!("the CAPTCHA service's answer is not JSON: {e}"))?;
        Ok(body.get("success").and_then(serde_json::Value::as_bool) == Some(true))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, EncodeLabelSet)]
struct OutcomeLabels {
    outcome: &'static str,
}

/// Process-wide, like [`crate::guest`]'s counters.
static CHECKS: LazyLock<Family<OutcomeLabels, Counter>> = LazyLock::new(Family::default);

/// Counts one check by outcome: `passed`, `failed` (the service said no) or `error` (it could
/// not be asked).
pub(crate) fn count(outcome: &'static str) {
    CHECKS.get_or_create(&OutcomeLabels { outcome }).inc();
}

/// Registers `hs_auth_recaptcha_checks_total{outcome="passed"|"failed"|"error"}` into
/// `registry`.
pub fn register_metrics(registry: &mut prometheus_client::registry::Registry) {
    // Registered without `_total`: the text encoder appends it.
    registry.register(
        "hs_auth_recaptcha_checks",
        "CAPTCHA answers checked at registration, by outcome: passed, failed (the CAPTCHA \
         service said no), or error (it could not be asked)",
        CHECKS.clone(),
    );
}
