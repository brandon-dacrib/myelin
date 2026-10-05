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

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex, OnceLock};

use async_trait::async_trait;
use hs_kv::KvBackend;
use ruma::thirdparty::{Location, Protocol, User};
use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::error::AppserviceError;
use crate::metrics::AppserviceMetrics;
use crate::namespace::NamespaceKind;
use crate::registry::{Registry, instance_id};
use crate::store::AppserviceRow;

/// How long an appservice's answer to `GET /thirdparty/protocol/{protocol}` is reused. Synapse
/// keeps it for an hour (`ApplicationServiceApi.protocol_meta_cache`); a bridge's protocol
/// metadata changes with its configuration, so five minutes here, which still answers a
/// client's protocol list, then each protocol, from one round of questions (Sytest's "HS can
/// provide query metadata on a single protocol" relies on that).
pub const PROTOCOL_CACHE_MS: u64 = 5 * 60 * 1000;

/// The two kinds of third-party lookup (`GET /_matrix/client/v3/thirdparty/{user,location}`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThirdPartyKind {
    /// `thirdparty/user`: Matrix users for third-party identities.
    User,
    /// `thirdparty/location`: Matrix room aliases for third-party locations.
    Location,
}

impl ThirdPartyKind {
    /// The path segment, `user` or `location`.
    #[must_use]
    pub fn segment(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Location => "location",
        }
    }

    /// The field every result must carry as a string, beside `protocol`: `userid` or `alias`
    /// (Synapse's `_is_valid_3pe_result`).
    #[must_use]
    pub fn id_field(self) -> &'static str {
        match self {
            Self::User => "userid",
            Self::Location => "alias",
        }
    }

    fn metric(self) -> &'static str {
        match self {
            Self::User => "thirdparty_user",
            Self::Location => "thirdparty_location",
        }
    }
}

/// A third-party lookup result is a JSON object with string `protocol` and the kind's id field,
/// and an object `fields`; anything else an appservice sends is dropped.
fn valid_3pe_result(value: &Value, kind: ThirdPartyKind) -> bool {
    let Some(object) = value.as_object() else {
        return false;
    };
    [kind.id_field(), "protocol"]
        .iter()
        .all(|key| object.get(*key).is_some_and(Value::is_string))
        && object.get("fields").is_some_and(Value::is_object)
}

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
            client: hs_http::client::builder()
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

fn encode_pairs(pairs: &[(String, String)]) -> String {
    if pairs.is_empty() {
        return String::new();
    }
    let parts: Vec<String> = pairs
        .iter()
        .map(|(k, v)| format!("{}={}", encode_path_segment(k), encode_path_segment(v)))
        .collect();
    format!("?{}", parts.join("&"))
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
    metrics: OnceLock<AppserviceMetrics>,
    /// `(appservice id, protocol) -> (when, metadata)`: see [`PROTOCOL_CACHE_MS`].
    protocol_cache: Mutex<HashMap<(String, String), (u64, Value)>>,
}

impl<B: KvBackend> QueryService<B> {
    /// Builds a query service over `registry`, calling out with `transport`.
    #[must_use]
    pub fn new(registry: Arc<Registry<B>>, transport: Arc<dyn AppserviceQueryTransport>) -> Self {
        Self {
            registry,
            transport,
            metrics: OnceLock::new(),
            protocol_cache: Mutex::new(HashMap::new()),
        }
    }

    /// The registry this service asks the appservices of.
    #[must_use]
    pub fn registry(&self) -> &Arc<Registry<B>> {
        &self.registry
    }

    /// Counts every question from now on in `hs_appservice_queries_total`. Set once; later
    /// calls are ignored. (The metrics registry exists after the appservices are loaded, so this
    /// is not a constructor argument.)
    pub fn set_metrics(&self, metrics: AppserviceMetrics) {
        let _ = self.metrics.set(metrics);
    }

    fn record(&self, appservice: &str, kind: &str, outcome: &str) {
        if let Some(metrics) = self.metrics.get() {
            metrics.record_query(appservice, kind, outcome);
        }
    }

    /// Asks `row` at `path_and_query`, counting the outcome under `kind`. `None` for "no" (404,
    /// or no `url` to ask) and for an error, which is logged.
    async fn ask(&self, row: &AppserviceRow, kind: &str, path_and_query: &str) -> Option<Value> {
        let url = row.url.as_deref()?;
        match self.transport.get(url, &row.hs_token, path_and_query).await {
            Ok(Some(value)) => {
                self.record(&row.id, kind, "yes");
                Some(value)
            }
            Ok(None) => {
                self.record(&row.id, kind, "no");
                None
            }
            Err(error) => {
                self.record(&row.id, kind, "error");
                tracing::warn!(appservice = %row.id, kind, %error, "an appservice did not answer the homeserver's question");
                None
            }
        }
    }

    /// Asks each appservice whose user namespace covers `user_id` (`GET
    /// /_matrix/app/v1/users/{userId}`) whether it has that user, as Synapse does for an
    /// unknown local user before it delivers an event naming them
    /// (`ApplicationServicesHandler.query_user_exists`). `true` at the first that says yes: by
    /// the spec it has created the user by then.
    pub async fn user_exists(&self, user_id: &str) -> bool {
        let rows = match self.registry.interested(NamespaceKind::Users, user_id) {
            Ok(rows) => rows,
            Err(error) => {
                tracing::warn!(%error, user_id, "could not read the appservice registry to ask about a user");
                return false;
            }
        };
        let path = format!("/users/{}", encode_path_segment(user_id));
        for row in rows {
            if self.ask(&row, "user", &path).await.is_some() {
                tracing::info!(appservice = %row.id, user_id, "an appservice provided a user the homeserver asked about");
                return true;
            }
        }
        false
    }

    /// Asks each appservice whose alias namespace covers `alias` (`GET
    /// /_matrix/app/v1/rooms/{roomAlias}`) whether it can provide that room, for a local alias
    /// the directory does not hold (`DirectoryHandler.get_association`'s
    /// `query_room_alias_exists`). `true` at the first that says yes: by the spec it has created
    /// the room and the alias by then, so the caller looks the alias up again.
    pub async fn room_alias_exists(&self, alias: &str) -> bool {
        let rows = match self.registry.interested(NamespaceKind::Aliases, alias) {
            Ok(rows) => rows,
            Err(error) => {
                tracing::warn!(%error, alias, "could not read the appservice registry to ask about an alias");
                return false;
            }
        };
        let path = format!("/rooms/{}", encode_path_segment(alias));
        for row in rows {
            if self.ask(&row, "room_alias", &path).await.is_some() {
                tracing::info!(appservice = %row.id, alias, "an appservice provided a room alias the homeserver asked about");
                return true;
            }
        }
        false
    }

    /// One appservice's metadata for `protocol` (`GET /thirdparty/protocol/{protocol}`), from
    /// the cache when it is under [`PROTOCOL_CACHE_MS`] old. An answer without an `instances`
    /// list is not metadata and is dropped. Each instance with a `network_id` is given its
    /// `instance_id` ([`instance_id`]), which `/publicRooms` takes back as
    /// `third_party_instance_id`.
    async fn protocol_of(&self, row: &AppserviceRow, protocol: &str) -> Option<Value> {
        let key = (row.id.clone(), protocol.to_owned());
        let now = self.registry.now_ms();
        if let Some((at, value)) = self
            .protocol_cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&key)
            && now.saturating_sub(*at) < PROTOCOL_CACHE_MS
        {
            self.record(&row.id, "protocol", "cached");
            return Some(value.clone());
        }
        let path = format!("/thirdparty/protocol/{}", encode_path_segment(protocol));
        let mut info = self.ask(row, "protocol", &path).await?;
        let Some(instances) = info.get_mut("instances").and_then(Value::as_array_mut) else {
            tracing::warn!(appservice = %row.id, protocol, "an appservice's protocol metadata has no instances list; ignoring it");
            return None;
        };
        for instance in instances.iter_mut() {
            let network = instance
                .get("network_id")
                .and_then(Value::as_str)
                .map(str::to_owned);
            if let (Some(network), Some(object)) = (network, instance.as_object_mut()) {
                object.insert(
                    "instance_id".to_owned(),
                    Value::String(instance_id(&row.id, &network)),
                );
            }
        }
        self.protocol_cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(key, (now, info.clone()));
        Some(info)
    }

    /// `GET /_matrix/client/v3/thirdparty/protocols` (and, with `only`, `.../protocol/{p}`):
    /// every protocol the registered appservices declare, each asked of every appservice that
    /// declares it, merged as Synapse merges them (`get_3pe_protocols`): the first answer's
    /// fields, every answer's `instances`. A protocol nobody answered for is left out.
    pub async fn protocols(&self, only: Option<&str>) -> BTreeMap<String, Value> {
        let rows = match self.registry.list() {
            Ok(rows) => rows,
            Err(error) => {
                tracing::warn!(%error, "could not read the appservice registry for third-party protocols");
                return BTreeMap::new();
            }
        };
        let mut merged: BTreeMap<String, Value> = BTreeMap::new();
        for row in &rows {
            for protocol in &row.protocols {
                if only.is_some_and(|only| only != protocol) {
                    continue;
                }
                let Some(info) = self.protocol_of(row, protocol).await else {
                    continue;
                };
                match merged.get_mut(protocol) {
                    None => {
                        merged.insert(protocol.clone(), info);
                    }
                    Some(first) => {
                        let more = info
                            .get("instances")
                            .and_then(Value::as_array)
                            .cloned()
                            .unwrap_or_default();
                        if let Some(instances) =
                            first.get_mut("instances").and_then(Value::as_array_mut)
                        {
                            instances.extend(more);
                        }
                    }
                }
            }
        }
        merged
    }

    /// `GET /_matrix/client/v3/thirdparty/{user,location}[/{protocol}]`: asks every appservice
    /// that declares `protocol` (with no protocol, the reverse lookups `?userid=`/`?alias=`,
    /// every appservice that declares any) with the client's query, and returns every valid
    /// result, in registry order. An appservice that does not answer contributes nothing.
    pub async fn thirdparty_lookup(
        &self,
        kind: ThirdPartyKind,
        protocol: Option<&str>,
        query: &[(String, String)],
    ) -> Vec<Value> {
        let rows = match self.registry.list() {
            Ok(rows) => rows,
            Err(error) => {
                tracing::warn!(%error, "could not read the appservice registry for a third-party lookup");
                return Vec::new();
            }
        };
        let path = match protocol {
            Some(protocol) => format!(
                "/thirdparty/{}/{}{}",
                kind.segment(),
                encode_path_segment(protocol),
                encode_pairs(query)
            ),
            None => format!("/thirdparty/{}{}", kind.segment(), encode_pairs(query)),
        };
        let mut out = Vec::new();
        for row in rows {
            let declares = match protocol {
                Some(protocol) => row.protocols.iter().any(|p| p == protocol),
                None => !row.protocols.is_empty(),
            };
            if !declares {
                continue;
            }
            let Some(Value::Array(results)) = self.ask(&row, kind.metric(), &path).await else {
                continue;
            };
            out.extend(
                results
                    .into_iter()
                    .filter(|result| valid_3pe_result(result, kind)),
            );
        }
        out
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

    /// Answers by `(url, path)`, so two appservices can answer the same question differently,
    /// and records every question.
    type Answers = std::collections::HashMap<(String, String), Result<Option<Value>, String>>;

    #[derive(Default)]
    struct ByUrl {
        answers: Mutex<Answers>,
        asked: Mutex<Vec<(String, String)>>,
    }

    impl ByUrl {
        fn answer(&self, url: &str, path: &str, response: Result<Option<Value>, String>) {
            self.answers
                .lock()
                .unwrap()
                .insert((url.to_owned(), path.to_owned()), response);
        }
        fn asked(&self) -> Vec<(String, String)> {
            self.asked.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl AppserviceQueryTransport for ByUrl {
        async fn get(
            &self,
            url: &str,
            _hs_token: &str,
            path_and_query: &str,
        ) -> Result<Option<Value>, String> {
            let key = (url.to_owned(), path_and_query.to_owned());
            self.asked.lock().unwrap().push(key.clone());
            self.answers
                .lock()
                .unwrap()
                .get(&key)
                .cloned()
                .unwrap_or(Ok(None))
        }
    }

    /// Two bridges of protocol `ymca` (Sytest's `AS_INFO` 0 and 1), one of `irc` users too, and
    /// a clock the test moves.
    fn two_bridges(
        transport: Arc<ByUrl>,
    ) -> (
        QueryService<hs_kv::memory::MemoryBackend>,
        Arc<hs_auth::clock::FixedClock>,
    ) {
        let clock = Arc::new(hs_auth::clock::FixedClock::new(1_000));
        let registry = Arc::new(
            Registry::open(
                hs_kv::memory::MemoryBackend::new(),
                server_name!("example.org"),
            )
            .unwrap()
            .with_clock(clock.clone()),
        );
        // `two` covers the same aliases without holding them: an exclusive claim each would
        // conflict.
        for (id, users, aliases_exclusive) in
            [("one", r"@astest-.*", true), ("two", r"@other-.*", false)]
        {
            registry
                .add(&Registration {
                    id: id.to_string(),
                    url: Some(format!("http://{id}.local")),
                    as_token: format!("as_{id}"),
                    hs_token: format!("hs_{id}"),
                    sender_localpart: format!("{id}bot"),
                    rate_limited: true,
                    namespaces: Namespaces {
                        users: vec![crate::namespace::NamespaceRule::compile(users, true).unwrap()],
                        aliases: vec![
                            crate::namespace::NamespaceRule::compile(
                                r"#astest-.*",
                                aliases_exclusive,
                            )
                            .unwrap(),
                        ],
                        rooms: vec![],
                    },
                    protocols: vec!["ymca".to_string()],
                    receive_ephemeral: false,
                    push_ephemeral_legacy: false,
                    msc3202: false,
                    msc4190: false,
                    extra: Default::default(),
                })
                .unwrap();
        }
        (QueryService::new(registry, transport), clock)
    }

    /// Sytest's "HS provides query metadata" and "HS can provide query metadata on a single
    /// protocol": each bridge of a protocol is asked, the answers are merged (the first's fields,
    /// everybody's instances), an instance with a `network_id` gets its `instance_id`, and the
    /// answers are reused for five minutes, which is what the second test relies on.
    #[tokio::test]
    async fn protocol_metadata_is_merged_across_appservices_and_kept_a_while() {
        let transport = Arc::new(ByUrl::default());
        let path = "/thirdparty/protocol/ymca";
        transport.answer(
            "http://one.local",
            path,
            Ok(Some(json!({
                "user_fields": ["field1", "field2"],
                "location_fields": ["field3"],
                "icon": "mxc://1234/56/7",
                "instances": [{"desc": "instance 1"}, {"desc": "instance 2", "network_id": "libera"}],
            }))),
        );
        transport.answer(
            "http://two.local",
            path,
            Ok(Some(json!({
                "user_fields": ["ignored"],
                "instances": [{"desc": "instance 3"}],
            }))),
        );
        let (service, clock) = two_bridges(transport.clone());
        let protocols = service.protocols(None).await;
        assert_eq!(
            protocols["ymca"],
            json!({
                "user_fields": ["field1", "field2"],
                "location_fields": ["field3"],
                "icon": "mxc://1234/56/7",
                "instances": [
                    {"desc": "instance 1"},
                    {"desc": "instance 2", "network_id": "libera", "instance_id": "one|libera"},
                    {"desc": "instance 3"},
                ],
            })
        );
        assert_eq!(transport.asked().len(), 2);

        // Now they answer nonsense; the single-protocol call is answered from what they said.
        transport.answer("http://one.local", path, Ok(Some(json!([]))));
        transport.answer("http://two.local", path, Ok(Some(json!([]))));
        let one = service.protocols(Some("ymca")).await;
        assert_eq!(one["ymca"], protocols["ymca"]);
        assert_eq!(transport.asked().len(), 2, "answered from the cache");
        assert!(service.protocols(Some("irc")).await.is_empty());

        // Five minutes on they are asked again, and an answer without instances is not metadata.
        clock.advance(PROTOCOL_CACHE_MS);
        assert!(service.protocols(None).await.is_empty());
        assert_eq!(transport.asked().len(), 4);
    }

    /// Sytest's "HS will proxy request for 3PU mapping" and "... 3PL mapping": the client's
    /// fields are passed to every bridge of the protocol, and only well-formed results come
    /// back.
    #[tokio::test]
    async fn thirdparty_lookups_go_to_the_protocols_appservices() {
        let transport = Arc::new(ByUrl::default());
        transport.answer(
            "http://one.local",
            "/thirdparty/user/ymca?field1=ONE&field2=TWO",
            Ok(Some(json!([
                {"protocol": "ymca", "fields": {"field1": "result"}, "userid": "@remote-user:bridged.example.com"},
                {"protocol": "ymca", "userid": "@no-fields:bridged.example.com"},
            ]))),
        );
        transport.answer(
            "http://two.local",
            "/thirdparty/user/ymca?field1=ONE&field2=TWO",
            Ok(Some(json!([]))),
        );
        transport.answer(
            "http://two.local",
            "/thirdparty/location/ymca?field3=THREE",
            Err("502: down".to_owned()),
        );
        transport.answer(
            "http://one.local",
            "/thirdparty/location/ymca?field3=THREE",
            Ok(Some(json!([
                {"protocol": "ymca", "fields": {"field3": "result"}, "alias": "#remote-room:bridged.example.com"},
            ]))),
        );
        let (service, _) = two_bridges(transport.clone());
        let fields = |pairs: &[(&str, &str)]| -> Vec<(String, String)> {
            pairs
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect()
        };
        let users = service
            .thirdparty_lookup(
                ThirdPartyKind::User,
                Some("ymca"),
                &fields(&[("field1", "ONE"), ("field2", "TWO")]),
            )
            .await;
        assert_eq!(
            users,
            vec![
                json!({"protocol": "ymca", "fields": {"field1": "result"}, "userid": "@remote-user:bridged.example.com"})
            ]
        );
        let locations = service
            .thirdparty_lookup(
                ThirdPartyKind::Location,
                Some("ymca"),
                &fields(&[("field3", "THREE")]),
            )
            .await;
        assert_eq!(
            locations.len(),
            1,
            "the one that is down contributes nothing"
        );
        assert!(
            service
                .thirdparty_lookup(ThirdPartyKind::User, Some("irc"), &[])
                .await
                .is_empty()
        );
        assert!(
            !transport
                .asked()
                .iter()
                .any(|(_, path)| path.starts_with("/thirdparty/user/irc")),
            "nobody declares irc"
        );
    }

    /// The existence questions go to the appservices whose namespace covers the ID, and stop at
    /// the first yes.
    #[tokio::test]
    async fn existence_questions_go_to_the_appservices_that_cover_the_id() {
        let transport = Arc::new(ByUrl::default());
        transport.answer(
            "http://one.local",
            "/users/%40astest-1%3Aexample.org",
            Ok(Some(json!({}))),
        );
        transport.answer(
            "http://two.local",
            "/rooms/%23astest-room%3Aexample.org",
            Ok(Some(json!({}))),
        );
        let (service, _) = two_bridges(transport.clone());
        assert!(service.user_exists("@astest-1:example.org").await);
        assert!(!service.user_exists("@astest-2:example.org").await);
        assert!(!service.user_exists("@nobody:example.org").await);
        assert!(service.room_alias_exists("#astest-room:example.org").await);
        assert!(!service.room_alias_exists("#tea:example.org").await);
        assert_eq!(
            transport.asked(),
            vec![
                (
                    "http://one.local".to_owned(),
                    "/users/%40astest-1%3Aexample.org".to_owned()
                ),
                (
                    "http://one.local".to_owned(),
                    "/users/%40astest-2%3Aexample.org".to_owned()
                ),
                (
                    "http://one.local".to_owned(),
                    "/rooms/%23astest-room%3Aexample.org".to_owned()
                ),
                (
                    "http://two.local".to_owned(),
                    "/rooms/%23astest-room%3Aexample.org".to_owned()
                ),
            ]
        );
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
