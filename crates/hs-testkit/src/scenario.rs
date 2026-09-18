//! A scenario DSL for scripted multi-user HTTP flows against an axum `Router`.
//!
//! [`Scenario`] drives a router the way Synapse's `HomeserverTestCase` drives an in-process
//! reactor, but through real HTTP request/response objects and `tower::ServiceExt::oneshot`
//! rather than any framework-internal shortcut — the same router a listener would serve is
//! exercised here. It tracks named users' sessions (`user_id`, `access_token`, `device_id`,
//! `refresh_token`) across calls so a scenario reads as a script ("alice registers, bob logs in,
//! alice sends a message, bob syncs") instead of hand-threading tokens through every request.
//!
//! [`Scenario::register`], [`Scenario::login`], [`Scenario::whoami`], [`Scenario::refresh`] and
//! [`Scenario::logout`] wrap the legacy Matrix client-server auth endpoints this workspace has
//! today (`hs-auth`'s router, see `crates/hs-testkit/tests/hs_auth_round_trip.rs`).
//! [`Scenario::send`] is the generic escalation hatch for any other endpoint (room creation,
//! messaging, sync, ...) as other tracks' routers land, and [`Scenario::sync`] is a thin
//! convenience over it for the spec's `/sync` shape once something serves it.

use std::collections::HashMap;

use axum::Router;
use axum::body::Body;
use axum::http::header::{AUTHORIZATION, CONTENT_TYPE};
use axum::http::{HeaderMap, Method, Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;

use crate::matrix_error::MatrixErrorExpectation;

/// A decoded HTTP response from a [`Scenario`] step: status, headers, and the body parsed as
/// JSON when possible (falling back to a JSON string of the raw bytes for non-JSON bodies, so a
/// scenario can still inspect it without panicking on a body it didn't expect).
#[derive(Debug, Clone)]
pub struct ScenarioResponse {
    /// The HTTP status code.
    pub status: StatusCode,
    /// The response headers.
    pub headers: HeaderMap,
    /// The response body, parsed as JSON (or wrapped as a JSON string if it was not valid JSON;
    /// an empty body decodes as [`Value::Null`]).
    pub json: Value,
}

impl ScenarioResponse {
    /// Panics with a diagnostic if the status does not match.
    pub fn assert_status(&self, expected: StatusCode) -> &Self {
        assert_eq!(
            self.status, expected,
            "expected HTTP status {expected}, got {} (body: {})",
            self.status, self.json
        );
        self
    }

    /// Shorthand for `assert_status(StatusCode::OK)`.
    pub fn assert_ok(&self) -> &Self {
        self.assert_status(StatusCode::OK)
    }

    /// Panics with a diagnostic unless this response is the spec's standard error shape with the
    /// given status and `errcode`.
    pub fn assert_matrix_error(&self, status: StatusCode, errcode: &'static str) -> &Self {
        MatrixErrorExpectation::new(status, errcode).check(self.status, &self.json);
        self
    }

    /// `body[key]` as a string, panicking with the whole body if the key is absent or not a
    /// string.
    #[must_use]
    pub fn str_field(&self, key: &str) -> &str {
        self.json
            .get(key)
            .and_then(Value::as_str)
            .unwrap_or_else(|| {
                panic!(
                    "expected response body to have string field {key:?}, got {}",
                    self.json
                )
            })
    }
}

/// One named user's accumulated session state across a [`Scenario`].
#[derive(Debug, Default, Clone)]
pub struct UserSession {
    /// The full Matrix user ID, once known (set by [`Scenario::register`] / [`Scenario::login`]).
    pub user_id: Option<String>,
    /// The current access token, if the user is logged in.
    pub access_token: Option<String>,
    /// The device ID associated with the current access token.
    pub device_id: Option<String>,
    /// The current refresh token, if one was issued.
    pub refresh_token: Option<String>,
}

/// A scripted, multi-user scenario against one axum router.
///
/// `router` must already have its state applied (`Router<()>`, what `axum::Router::with_state`
/// produces): [`Scenario`] calls it directly, the same way a real listener would, through
/// `tower::ServiceExt::oneshot`.
pub struct Scenario {
    router: Router,
    prefix: String,
    users: HashMap<String, UserSession>,
}

impl Scenario {
    /// A scenario against `router`, with paths passed to its methods used verbatim.
    #[must_use]
    pub fn new(router: Router) -> Self {
        Self::with_prefix(router, "")
    }

    /// A scenario against `router`, where every path given to a DSL method is prepended with
    /// `prefix` first (for a router mounted under `/_matrix/client/v3`, for example).
    #[must_use]
    pub fn with_prefix(router: Router, prefix: impl Into<String>) -> Self {
        Self {
            router,
            prefix: prefix.into(),
            users: HashMap::new(),
        }
    }

    /// Read-only access to a named user's accumulated session state.
    #[must_use]
    pub fn session(&self, name: &str) -> Option<&UserSession> {
        self.users.get(name)
    }

    /// Overrides `name`'s stored access token, creating the named session if it does not exist
    /// yet. Use this to test a server's handling of a token it never issued (`M_UNKNOWN_TOKEN`)
    /// or a deliberately malformed one, which no DSL verb produces on its own since they only
    /// ever store tokens the server actually returned.
    pub fn set_access_token(&mut self, name: &str, access_token: impl Into<String>) {
        self.session_mut(name).access_token = Some(access_token.into());
    }

    fn session_mut(&mut self, name: &str) -> &mut UserSession {
        self.users.entry(name.to_string()).or_default()
    }

    fn full_path(&self, path: &str) -> String {
        format!("{}{path}", self.prefix)
    }

    /// Sends one HTTP request through the router, optionally as `name` (attaching that user's
    /// stored access token as a bearer `Authorization` header if one is set), optionally with a
    /// JSON body. This is the generic building block every other DSL method is written in terms
    /// of; use it directly for endpoints this crate has no dedicated helper for yet.
    ///
    /// # Panics
    /// Panics if `path` cannot be built into a valid request, or if the router itself panics
    /// (axum routers are otherwise infallible `Service`s: they turn handler errors into HTTP
    /// error responses, not `Err`).
    pub async fn send(
        &mut self,
        name: Option<&str>,
        method: Method,
        path: &str,
        body: Option<Value>,
    ) -> ScenarioResponse {
        let mut builder = Request::builder().method(method).uri(self.full_path(path));

        if let Some(name) = name
            && let Some(session) = self.users.get(name)
            && let Some(token) = &session.access_token
        {
            builder = builder.header(AUTHORIZATION, format!("Bearer {token}"));
        }

        let request = match body {
            Some(value) => builder
                .header(CONTENT_TYPE, "application/json")
                .body(Body::from(value.to_string()))
                .expect("valid HTTP request"),
            None => builder.body(Body::empty()).expect("valid HTTP request"),
        };

        let response = self
            .router
            .clone()
            .oneshot(request)
            .await
            .expect("axum routers are an infallible Service");
        let status = response.status();
        let headers = response.headers().clone();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("collecting the response body");
        let json = if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes)
                .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into_owned()))
        };
        ScenarioResponse {
            status,
            headers,
            json,
        }
    }

    /// `POST /register` with a `m.login.dummy` auth stage (the always-available fallback flow),
    /// via a bare username/password. On success, records `user_id`, `access_token` and
    /// `device_id` for `name`.
    pub async fn register(
        &mut self,
        name: &str,
        username: &str,
        password: &str,
    ) -> ScenarioResponse {
        let body = json!({
            "username": username,
            "password": password,
            "auth": {"type": "m.login.dummy"},
        });
        let response = self.send(None, Method::POST, "/register", Some(body)).await;
        if response.status == StatusCode::OK {
            let session = self.session_mut(name);
            session.user_id = str_opt(&response.json, "user_id");
            session.access_token = str_opt(&response.json, "access_token");
            session.device_id = str_opt(&response.json, "device_id");
        }
        response
    }

    /// `POST /login` with `m.login.password`, requesting a refresh token. On success, records
    /// `user_id`, `access_token`, `device_id` and `refresh_token` for `name`.
    pub async fn login(&mut self, name: &str, username: &str, password: &str) -> ScenarioResponse {
        let body = json!({
            "type": "m.login.password",
            "identifier": {"type": "m.id.user", "user": username},
            "password": password,
            "refresh_token": true,
        });
        let response = self.send(None, Method::POST, "/login", Some(body)).await;
        if response.status == StatusCode::OK {
            let session = self.session_mut(name);
            session.user_id = str_opt(&response.json, "user_id");
            session.access_token = str_opt(&response.json, "access_token");
            session.device_id = str_opt(&response.json, "device_id");
            session.refresh_token = str_opt(&response.json, "refresh_token");
        }
        response
    }

    /// `GET /account/whoami` as `name`, using its stored access token.
    pub async fn whoami(&mut self, name: &str) -> ScenarioResponse {
        self.send(Some(name), Method::GET, "/account/whoami", None)
            .await
    }

    /// `POST /refresh` using `name`'s stored refresh token. On success, replaces the stored
    /// access token (and refresh token, if a new one was issued).
    pub async fn refresh(&mut self, name: &str) -> ScenarioResponse {
        let refresh_token = self
            .users
            .get(name)
            .and_then(|s| s.refresh_token.clone())
            .unwrap_or_default();
        let body = json!({"refresh_token": refresh_token});
        let response = self.send(None, Method::POST, "/refresh", Some(body)).await;
        if response.status == StatusCode::OK {
            let session = self.session_mut(name);
            session.access_token = str_opt(&response.json, "access_token");
            if let Some(rt) = str_opt(&response.json, "refresh_token") {
                session.refresh_token = Some(rt);
            }
        }
        response
    }

    /// `POST /logout` as `name`. On success, clears its stored access and refresh tokens (the
    /// device ID and user ID are left in place: they are still meaningful, only the credentials
    /// are gone).
    pub async fn logout(&mut self, name: &str) -> ScenarioResponse {
        let response = self.send(Some(name), Method::POST, "/logout", None).await;
        if response.status == StatusCode::OK {
            let session = self.session_mut(name);
            session.access_token = None;
            session.refresh_token = None;
        }
        response
    }

    /// `GET /sync` as `name`. A thin convenience over [`Scenario::send`]; no crate in this
    /// workspace serves `/sync` yet, so today this reliably returns a 404 — it exists so
    /// scenarios written against `hs-sync` once it lands do not need a new verb.
    pub async fn sync(&mut self, name: &str) -> ScenarioResponse {
        self.send(Some(name), Method::GET, "/sync", None).await
    }
}

fn str_opt(value: &Value, key: &str) -> Option<String> {
    value.get(key).and_then(Value::as_str).map(String::from)
}
