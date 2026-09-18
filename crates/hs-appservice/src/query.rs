//! The appservice user/room-alias query protocol and third-party lookups (`PLAN.md` section 8.1
//! point 5, Appendix B's "Federation client endpoints" note and the spec's
//! `application-service-api`). All six calls here are **outbound**: the homeserver asks the
//! appservice, over HTTP with the appservice's `hs_token`, GET `{url}/_matrix/app/v1/...`. There
//! is nothing to serve on our own client-server API for these — unlike [`crate::ping`], which has
//! both an inbound and an outbound leg, querying is purely something the homeserver *does*, when
//! it needs to know whether an appservice can provision a not-yet-known user ID or room alias
//! (`GET /users/{userId}`, `GET /rooms/{roomAlias}`) before treating a namespaced-but-unseen ID as
//! nonexistent, and when serving `/_matrix/client/v1/thirdparty/*` itself needs to fan out to
//! every appservice that declared the relevant `protocols` entry.
//!
//! Response shapes (`ruma_appservice_api::{query, thirdparty}`, MIT) are reused directly for the
//! third-party lookups; the two existence queries collapse to `bool` (2xx = yes, anything else =
//! no), matching the spec's "any successful response ... is a nonexistence check" framing for
//! those two endpoints (empty `{}` body either way).

use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use hs_kv::KvBackend;
use ruma::thirdparty::{Location, Protocol, User};
use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::error::AppserviceError;
use crate::registry::Registry;

/// Performs one outbound `GET` call to an appservice. A trait for the same reason
/// [`crate::scheduler::TransactionSender`] and [`crate::ping::PingTransport`] are: tests and
/// `hs-bridge-conformance` substitute an in-process double.
#[async_trait]
pub trait AppserviceQueryTransport: Send + Sync {
    /// `path_and_query` is appended directly to `{url}/_matrix/app/v1` (already including a
    /// leading `/` and any `?query`). Returns `Ok(Some(body))` on 2xx (with `body` defaulting to
    /// `{}` if the appservice sent no content), `Ok(None)` on 404 (a spec-legal "no" for the
    /// existence queries and thirdparty lookups alike), and `Err` for anything else.
    async fn get(
        &self,
        url: &str,
        hs_token: &str,
        path_and_query: &str,
    ) -> Result<Option<Value>, String>;
}

/// The real HTTP query transport.
pub struct HttpQueryTransport {
    client: reqwest::Client,
}

impl Default for HttpQueryTransport {
    fn default() -> Self {
        Self::new()
    }
}

impl HttpQueryTransport {
    /// A transport with a 10-second timeout.
    #[must_use]
    pub fn new() -> Self {
        Self {
            client: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(10))
                .build()
                .unwrap_or_default(),
        }
    }
}

#[async_trait]
impl AppserviceQueryTransport for HttpQueryTransport {
    async fn get(
        &self,
        url: &str,
        hs_token: &str,
        path_and_query: &str,
    ) -> Result<Option<Value>, String> {
        let base = url.trim_end_matches('/');
        let full_url = format!("{base}/_matrix/app/v1{path_and_query}");
        let response = self
            .client
            .get(&full_url)
            .bearer_auth(hs_token)
            .send()
            .await
            .map_err(|e| e.to_string())?;
        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if !response.status().is_success() {
            let status = response.status();
            let text = response.text().await.unwrap_or_default();
            return Err(format!("{status}: {text}"));
        }
        let bytes = response.bytes().await.map_err(|e| e.to_string())?;
        if bytes.is_empty() {
            return Ok(Some(Value::Object(serde_json::Map::new())));
        }
        serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|e| e.to_string())
    }
}

fn encode_path_segment(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

fn encode_query(fields: &BTreeMap<String, String>) -> String {
    if fields.is_empty() {
        return String::new();
    }
    let parts: Vec<String> = fields
        .iter()
        .map(|(k, v)| format!("{}={}", encode_path_segment(k), encode_path_segment(v)))
        .collect();
    format!("?{}", parts.join("&"))
}

/// Queries the appservice user/room-alias existence protocol and third-party lookups.
pub struct QueryService<B: KvBackend> {
    registry: Arc<Registry<B>>,
    transport: Arc<dyn AppserviceQueryTransport>,
}

impl<B: KvBackend> QueryService<B> {
    /// Builds a query service over `registry`, calling out with `transport`.
    #[must_use]
    pub fn new(registry: Arc<Registry<B>>, transport: Arc<dyn AppserviceQueryTransport>) -> Self {
        Self {
            registry,
            transport,
        }
    }

    async fn row_and_get(
        &self,
        appservice_id: &str,
        path_and_query: &str,
    ) -> Result<Option<Value>, AppserviceError> {
        let row = self
            .registry
            .get(appservice_id)?
            .ok_or_else(|| AppserviceError::NotFound(appservice_id.to_string()))?;
        let Some(url) = row.url else {
            return Err(AppserviceError::NoUrl(appservice_id.to_string()));
        };
        self.transport
            .get(&url, &row.hs_token, path_and_query)
            .await
            .map_err(AppserviceError::Store)
    }

    /// `GET /users/{userId}`: does this appservice claim it can provision `user_id`.
    ///
    /// # Errors
    /// [`AppserviceError::NotFound`] if unregistered, [`AppserviceError::NoUrl`] for a
    /// double-puppet registration (nothing to ask), or [`AppserviceError::Store`] on transport
    /// failure.
    pub async fn query_user(
        &self,
        appservice_id: &str,
        user_id: &str,
    ) -> Result<bool, AppserviceError> {
        let path = format!("/users/{}", encode_path_segment(user_id));
        Ok(self.row_and_get(appservice_id, &path).await?.is_some())
    }

    /// `GET /rooms/{roomAlias}`: does this appservice claim it can provision `room_alias`.
    ///
    /// # Errors
    /// See [`QueryService::query_user`].
    pub async fn query_room_alias(
        &self,
        appservice_id: &str,
        room_alias: &str,
    ) -> Result<bool, AppserviceError> {
        let path = format!("/rooms/{}", encode_path_segment(room_alias));
        Ok(self.row_and_get(appservice_id, &path).await?.is_some())
    }

    async fn get_typed<T: DeserializeOwned + Default>(
        &self,
        appservice_id: &str,
        path_and_query: &str,
    ) -> Result<T, AppserviceError> {
        match self.row_and_get(appservice_id, path_and_query).await? {
            Some(value) => {
                serde_json::from_value(value).map_err(|e| AppserviceError::Decode(e.to_string()))
            }
            None => Ok(T::default()),
        }
    }

    /// `GET /thirdparty/protocol/{protocol}`.
    ///
    /// # Errors
    /// See [`QueryService::query_user`], plus [`AppserviceError::Decode`] on a malformed response.
    pub async fn thirdparty_protocol(
        &self,
        appservice_id: &str,
        protocol: &str,
    ) -> Result<Option<Protocol>, AppserviceError> {
        let path = format!("/thirdparty/protocol/{}", encode_path_segment(protocol));
        self.row_and_get(appservice_id, &path)
            .await?
            .map(serde_json::from_value)
            .transpose()
            .map_err(|e| AppserviceError::Decode(e.to_string()))
    }

    /// `GET /thirdparty/location/{protocol}?field=value...`.
    ///
    /// # Errors
    /// See [`QueryService::thirdparty_protocol`].
    pub async fn thirdparty_location_for_protocol(
        &self,
        appservice_id: &str,
        protocol: &str,
        fields: &BTreeMap<String, String>,
    ) -> Result<Vec<Location>, AppserviceError> {
        let path = format!(
            "/thirdparty/location/{}{}",
            encode_path_segment(protocol),
            encode_query(fields)
        );
        self.get_typed(appservice_id, &path).await
    }

    /// `GET /thirdparty/location?alias={roomAlias}`.
    ///
    /// # Errors
    /// See [`QueryService::thirdparty_protocol`].
    pub async fn thirdparty_location_for_alias(
        &self,
        appservice_id: &str,
        room_alias: &str,
    ) -> Result<Vec<Location>, AppserviceError> {
        let mut fields = BTreeMap::new();
        fields.insert("alias".to_string(), room_alias.to_string());
        let path = format!("/thirdparty/location{}", encode_query(&fields));
        self.get_typed(appservice_id, &path).await
    }

    /// `GET /thirdparty/user/{protocol}?field=value...`.
    ///
    /// # Errors
    /// See [`QueryService::thirdparty_protocol`].
    pub async fn thirdparty_user_for_protocol(
        &self,
        appservice_id: &str,
        protocol: &str,
        fields: &BTreeMap<String, String>,
    ) -> Result<Vec<User>, AppserviceError> {
        let path = format!(
            "/thirdparty/user/{}{}",
            encode_path_segment(protocol),
            encode_query(fields)
        );
        self.get_typed(appservice_id, &path).await
    }

    /// `GET /thirdparty/user?userid={userId}`.
    ///
    /// # Errors
    /// See [`QueryService::thirdparty_protocol`].
    pub async fn thirdparty_user_for_user_id(
        &self,
        appservice_id: &str,
        user_id: &str,
    ) -> Result<Vec<User>, AppserviceError> {
        let mut fields = BTreeMap::new();
        fields.insert("userid".to_string(), user_id.to_string());
        let path = format!("/thirdparty/user{}", encode_query(&fields));
        self.get_typed(appservice_id, &path).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::namespace::Namespaces;
    use crate::registration::Registration;
    use ruma::server_name;
    use serde_json::json;
    use std::sync::Mutex;

    struct MockTransport {
        responses: Mutex<std::collections::HashMap<String, Result<Option<Value>, String>>>,
        calls: Mutex<Vec<String>>,
    }

    impl MockTransport {
        fn new() -> Self {
            Self {
                responses: Mutex::new(std::collections::HashMap::new()),
                calls: Mutex::new(Vec::new()),
            }
        }
        fn set(&self, path: &str, response: Result<Option<Value>, String>) {
            self.responses
                .lock()
                .unwrap()
                .insert(path.to_string(), response);
        }
    }

    #[async_trait]
    impl AppserviceQueryTransport for MockTransport {
        async fn get(
            &self,
            _url: &str,
            _hs_token: &str,
            path_and_query: &str,
        ) -> Result<Option<Value>, String> {
            self.calls.lock().unwrap().push(path_and_query.to_string());
            self.responses
                .lock()
                .unwrap()
                .get(path_and_query)
                .cloned()
                .unwrap_or(Ok(None))
        }
    }

    fn service_with(
        id: &str,
        url: Option<&str>,
        transport: Arc<MockTransport>,
    ) -> QueryService<hs_kv::memory::MemoryBackend> {
        let registry = Arc::new(
            Registry::open(
                hs_kv::memory::MemoryBackend::new(),
                server_name!("example.org"),
            )
            .unwrap(),
        );
        registry
            .add(&Registration {
                id: id.to_string(),
                url: url.map(str::to_string),
                as_token: format!("as_{id}"),
                hs_token: format!("hs_{id}"),
                sender_localpart: format!("{id}bot"),
                rate_limited: true,
                namespaces: Namespaces::default(),
                protocols: vec!["irc".to_string()],
                receive_ephemeral: false,
                push_ephemeral_legacy: false,
                msc3202: false,
                msc4190: false,
                extra: Default::default(),
            })
            .unwrap();
        QueryService::new(registry, transport)
    }

    #[tokio::test]
    async fn user_query_true_on_2xx_false_on_404() {
        let transport = Arc::new(MockTransport::new());
        transport.set("/users/%40irc_bob%3Aexample.org", Ok(Some(json!({}))));
        let service = service_with("irc", Some("http://bridge.local"), transport);
        assert!(
            service
                .query_user("irc", "@irc_bob:example.org")
                .await
                .unwrap()
        );
        assert!(
            !service
                .query_user("irc", "@nope:example.org")
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn room_alias_query_round_trips() {
        let transport = Arc::new(MockTransport::new());
        transport.set("/rooms/%23irc_%23foo%3Aexample.org", Ok(Some(json!({}))));
        let service = service_with("irc", Some("http://bridge.local"), transport);
        assert!(
            service
                .query_room_alias("irc", "#irc_#foo:example.org")
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn null_url_registration_cannot_be_queried() {
        let transport = Arc::new(MockTransport::new());
        let service = service_with("dp", None, transport);
        assert!(matches!(
            service.query_user("dp", "@x:example.org").await.unwrap_err(),
            AppserviceError::NoUrl(id) if id == "dp"
        ));
    }

    #[tokio::test]
    async fn thirdparty_location_for_alias_decodes_the_response() {
        let transport = Arc::new(MockTransport::new());
        transport.set(
            "/thirdparty/location?alias=%23irc_%23foo%3Aexample.org",
            Ok(Some(json!([{
                "alias": "#irc_#foo:example.org",
                "protocol": "irc",
                "fields": {"channel": "#foo"}
            }]))),
        );
        let service = service_with("irc", Some("http://bridge.local"), transport);
        let locations = service
            .thirdparty_location_for_alias("irc", "#irc_#foo:example.org")
            .await
            .unwrap();
        assert_eq!(locations.len(), 1);
    }

    #[tokio::test]
    async fn thirdparty_query_defaults_to_empty_on_404() {
        let transport = Arc::new(MockTransport::new());
        let service = service_with("irc", Some("http://bridge.local"), transport);
        let users = service
            .thirdparty_user_for_user_id("irc", "@nope:example.org")
            .await
            .unwrap();
        assert!(users.is_empty());
    }
}
