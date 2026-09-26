//! The common schemas of RFC 0004 section 5: [`Page`], [`Task`], [`Principal`], [`AuditEntry`],
//! [`Event`], [`ResourceRef`], [`Actor`], and the [`Scope`] enum (section 8.2). Resource-specific
//! bodies (`User`, `Room`, ...) are not modeled here yet; the full OpenAPI document
//! (`crates/hs-admin/openapi/openapi.yaml`) is the contract for those until Phase 1 gives them
//! typed Rust representations alongside real handlers.

use serde::{Deserialize, Serialize};

/// A new server-generated identifier: a ULID in canonical uppercase Crockford base32 (RFC 0004
/// decision D15.2).
pub fn new_id() -> String {
    ulid::Ulid::new().to_string()
}

/// One of the six OAuth scopes RFC 0004 section 8.2 defines. Implements the "implies" partial
/// order: `admin:write` implies everything, and each `*:write` implies its own `*:read`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Scope {
    #[serde(rename = "admin:read")]
    AdminRead,
    #[serde(rename = "admin:write")]
    AdminWrite,
    #[serde(rename = "bridges:read")]
    BridgesRead,
    #[serde(rename = "bridges:write")]
    BridgesWrite,
    #[serde(rename = "moderation:read")]
    ModerationRead,
    #[serde(rename = "moderation:write")]
    ModerationWrite,
}

impl Scope {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::AdminRead => "admin:read",
            Self::AdminWrite => "admin:write",
            Self::BridgesRead => "bridges:read",
            Self::BridgesWrite => "bridges:write",
            Self::ModerationRead => "moderation:read",
            Self::ModerationWrite => "moderation:write",
        }
    }

    /// Parses the wire form (`"admin:read"`, ...). Unknown strings are `None`, not an open enum:
    /// the scope catalog is closed (RFC 0004 D15.8 lists exactly six).
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "admin:read" => Some(Self::AdminRead),
            "admin:write" => Some(Self::AdminWrite),
            "bridges:read" => Some(Self::BridgesRead),
            "bridges:write" => Some(Self::BridgesWrite),
            "moderation:read" => Some(Self::ModerationRead),
            "moderation:write" => Some(Self::ModerationWrite),
            _ => None,
        }
    }

    /// Whether holding `self` satisfies a requirement for `required` (RFC 0004 D15.8:
    /// `admin:write` satisfies everything; each `*:write` satisfies its own `*:read`).
    pub fn satisfies(self, required: Scope) -> bool {
        if self == required || self == Scope::AdminWrite {
            return true;
        }
        matches!(
            (self, required),
            (Scope::BridgesWrite, Scope::BridgesRead)
                | (Scope::ModerationWrite, Scope::ModerationRead)
        )
    }
}

/// Whether any scope in `held` satisfies `required`.
pub fn any_satisfies(held: &[Scope], required: Scope) -> bool {
    held.iter().any(|s| s.satisfies(required))
}

/// A page of `T`, per RFC 0004 section 3.3: keyset-paginated, cursor opaque.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Page<T> {
    pub items: Vec<T>,
    pub next_cursor: Option<String>,
    pub prev_cursor: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total: Option<u64>,
}

impl<T> Page<T> {
    pub fn new(items: Vec<T>) -> Self {
        Self {
            items,
            next_cursor: None,
            prev_cursor: None,
            total: None,
        }
    }

    /// Slices `items` into one page using RFC 0004's offset-shaped cursor convention: cursors are
    /// plain decimal offsets, the same convention `hs-admin-mock`'s `page_json` documents, not yet
    /// the opaque keyset cursors a real backing store's own pagination would produce (that needs a
    /// sort-key fingerprint per resource, which is Phase 1 work once a real store exists to build
    /// one against). `limit` is clamped to `[1, 500]`; a `cursor` that fails to parse as a decimal
    /// offset is treated as the first page rather than an error, matching the mock's leniency.
    pub fn paginate(
        items: Vec<T>,
        cursor: Option<&str>,
        limit: Option<usize>,
        include_total: bool,
    ) -> Self {
        let limit = limit.unwrap_or(50).clamp(1, 500);
        let total_len = items.len();
        let offset = cursor
            .and_then(|c| c.parse::<usize>().ok())
            .unwrap_or(0)
            .min(total_len);
        let end = (offset + limit).min(total_len);
        let next_cursor = if end < total_len {
            Some(end.to_string())
        } else {
            None
        };
        let prev_cursor = if offset > 0 {
            Some(offset.saturating_sub(limit).to_string())
        } else {
            None
        };
        let page_items: Vec<T> = items.into_iter().skip(offset).take(end - offset).collect();
        Self {
            items: page_items,
            next_cursor,
            prev_cursor,
            total: include_total.then_some(total_len as u64),
        }
    }
}

/// `ResourceRef` (RFC 0004 section 5): an open-enum resource type plus its identifier.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourceRef {
    pub r#type: String,
    pub id: String,
}

impl ResourceRef {
    pub fn new(r#type: impl Into<String>, id: impl Into<String>) -> Self {
        Self {
            r#type: r#type.into(),
            id: id.into(),
        }
    }
}

/// `Actor` (RFC 0004 section 5): who did something, for the audit log and the event stream.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Actor {
    pub kind: ActorKind,
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub token_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ip: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_agent: Option<String>,
}

impl Actor {
    pub fn system() -> Self {
        Self {
            kind: ActorKind::System,
            id: "system".to_string(),
            display_name: None,
            token_id: None,
            ip: None,
            user_agent: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActorKind {
    User,
    Client,
    ServiceAccount,
    System,
}

/// `Principal` (RFC 0004 section 8.1): the authenticated caller.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Principal {
    pub kind: PrincipalKind,
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    pub scopes: Vec<Scope>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub token_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub issued_by: Option<String>,
}

impl Principal {
    pub fn has_scope(&self, required: Scope) -> bool {
        any_satisfies(&self.scopes, required)
    }

    /// Projects this principal onto an [`Actor`] for the audit log and the event stream (RFC 0004
    /// section 9/10: every recorded mutation names who did it). `ip`/`user_agent` are left unset
    /// here — no handler in this crate threads a real client IP through yet (see
    /// `docs/status/15-admin-api-and-modules.md`); a caller with that information can still set it
    /// with struct-update syntax.
    pub fn to_actor(&self) -> Actor {
        Actor {
            kind: match self.kind {
                PrincipalKind::User | PrincipalKind::Legacy => ActorKind::User,
                PrincipalKind::Client => ActorKind::Client,
                PrincipalKind::ServiceAccount => ActorKind::ServiceAccount,
            },
            id: self.id.clone(),
            display_name: self.display_name.clone(),
            token_id: self.token_id.clone(),
            ip: None,
            user_agent: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PrincipalKind {
    User,
    Client,
    ServiceAccount,
    Legacy,
}

/// The OpenAPI `StatisticsOverview` schema: the body of `GET /statistics/overview`.
///
/// Every field is optional, in the contract and here, and an absent one means "this server does
/// not know", not zero. A source fills in what it can count honestly and leaves the rest out, so
/// that the dashboard can show a dash for a number nobody has rather than a confident `0`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatisticsOverview {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub users_count: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rooms_count: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub media_count: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub media_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub daily_active_users: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub monthly_active_users: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub federation_destinations_failing_count: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pending_reports_count: Option<u64>,
}

/// The OpenAPI `ClusterStatus` schema: the body of `GET /cluster`. As with
/// [`StatisticsOverview`], only `mode` is always known.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClusterStatus {
    /// `single-node` or `cluster`.
    pub mode: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub epoch: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub replica_count: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shard_count: Option<u64>,
}

/// The OpenAPI `SetupStatus` schema: the body of `GET /setup`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SetupStatus {
    /// True while this server has no administrator and is offering to create one.
    pub needs_setup: bool,
}

/// The OpenAPI `SetupRequest` schema: the body of `POST /setup`.
///
/// `Debug` is written by hand so that neither the setup token nor the password can reach a log
/// line through a stray `{:?}`.
#[derive(Clone, Deserialize)]
pub struct SetupRequest {
    pub setup_token: String,
    pub username: String,
    pub password: String,
}

impl std::fmt::Debug for SetupRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SetupRequest")
            .field("setup_token", &"<redacted>")
            .field("username", &self.username)
            .field("password", &"<redacted>")
            .finish()
    }
}

/// The OpenAPI `SetupSession` schema: the body of a successful `POST /setup`, a signed-in
/// session for the administrator it just created.
#[derive(Clone, Serialize, Deserialize)]
pub struct SetupSession {
    pub user_id: String,
    pub access_token: String,
    pub device_id: String,
}

impl std::fmt::Debug for SetupSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SetupSession")
            .field("user_id", &self.user_id)
            .field("access_token", &"<redacted>")
            .field("device_id", &self.device_id)
            .finish()
    }
}

/// The OpenAPI `User` schema (`crates/hs-admin/openapi/openapi.yaml`): one row of `GET /users`
/// and the body of `GET /users/{user_id}`. Field-for-field match with that schema. Served by
/// whatever implements [`crate::sources::UserDirectory`] (track 07's real implementation, or
/// [`crate::sources::InMemoryUserDirectory`] for tests).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct AdminUser {
    pub user_id: String,
    pub display_name: Option<String>,
    pub avatar_url: Option<String>,
    pub admin: bool,
    pub deactivated: bool,
    pub erased: bool,
    pub locked: bool,
    pub suspended: bool,
    pub shadow_banned: bool,
    pub user_type: Option<String>,
    pub consent_version: Option<String>,
    pub appservice_id: Option<String>,
    /// RFC 3339 millisecond-precision UTC (RFC 0004 D15.2).
    pub created_at: String,
    pub last_seen_at: Option<String>,
    pub device_count: u64,
    pub room_count: u64,
    pub media_count: u64,
}

/// The OpenAPI `Device` schema: one of a user's devices, as `GET /users/{user_id}/devices`
/// lists them.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct AdminDevice {
    pub device_id: String,
    pub display_name: Option<String>,
    pub last_seen_ip: Option<String>,
    /// RFC 3339 millisecond-precision UTC.
    pub last_seen_at: Option<String>,
}

/// The body of `POST /users/{user_id}/reset-password`. Debug never shows the password.
#[derive(Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct AdminPasswordReset {
    pub password: String,
    /// Whether every one of the user's sessions is signed out too. The contract's default, and
    /// the right one: a password is reset because the old one is not trusted any more.
    #[serde(default = "default_true")]
    pub logout_devices: bool,
}

fn default_true() -> bool {
    true
}

impl std::fmt::Debug for AdminPasswordReset {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AdminPasswordReset")
            .field("password", &"<redacted>")
            .field("logout_devices", &self.logout_devices)
            .finish()
    }
}

/// The OpenAPI `BridgeType` schema: one entry of the catalogue `GET /bridge-types` offers
/// (`crate::bridge_types`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct BridgeType {
    pub id: String,
    pub name: String,
    /// One line on what it connects.
    pub description: String,
    /// `messaging`, `social`, `irc` or `integrations`: how the wizard groups the catalogue.
    pub category: String,
    pub upstream_project: String,
    /// The project's own documentation.
    pub docs_url: String,
    pub image: String,
    /// The port the bridge listens on for this server by default: what its own config generator
    /// writes, and what a render's `url` names.
    pub port: u16,
    /// `users`/`aliases`/`rooms`, each a list of `{regex, exclusive}`, written for this server.
    pub default_namespaces: serde_json::Value,
    pub config_keys: Vec<BridgeTypeConfigKey>,
    pub supports_double_puppeting: bool,
    pub required_features: Vec<String>,
    /// Whether a render of this type produces a `config_yaml` the bridge reads as it is
    /// (mautrix bridges), or only the registration and the notes to run it.
    pub renders_config: bool,
    /// How a person signs in to the bridge once it runs.
    pub sign_in: BridgeTypeSignIn,
}

/// [`BridgeType::sign_in`]: the bridge's own documented login flow, one step per line. `{bot}`
/// in a step stands for the bridge bot's Matrix ID, which the interface substitutes.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct BridgeTypeSignIn {
    pub steps: Vec<String>,
    pub notes: Option<String>,
}

/// One of a [`BridgeType`]'s `config_keys`: something the operator has to have.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct BridgeTypeConfigKey {
    pub key: String,
    pub description: String,
    pub required: bool,
}

/// The OpenAPI `BridgeTypeRenderResult` schema: what `POST /bridge-types/{type}/render` hands
/// back. `registration` carries freshly minted tokens; Debug shows none of it.
#[derive(Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct BridgeTypeRenderResult {
    pub registration: serde_json::Value,
    pub registration_yaml: String,
    /// The bridge's own `config.yaml`, with everything that ties it to this server filled in;
    /// `None` for a bridge whose configuration the render does not write (see
    /// [`BridgeType::renders_config`]).
    pub config_yaml: Option<String>,
    pub compose_yaml: String,
    pub bridge_resource_yaml: String,
}

impl std::fmt::Debug for BridgeTypeRenderResult {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("BridgeTypeRenderResult(<redacted>)")
    }
}

/// The OpenAPI `RoomMember` schema: one row of `GET /rooms/{room_id}/members`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct AdminRoomMember {
    pub user_id: String,
    /// `join`, `invite`, `leave`, `ban` or `knock`, as the member event says.
    pub membership: String,
    pub display_name: Option<String>,
    pub avatar_url: Option<String>,
}

/// The OpenAPI `Destination` schema: one remote server this one has tried to reach, and how
/// that is going. Served by whatever implements [`crate::sources::FederationSource`].
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct AdminDestination {
    pub server_name: String,
    pub last_successful_at: Option<String>,
    /// When the current run of failures began; `None` while it is not failing.
    pub failing_since: Option<String>,
    pub retry_last_at: Option<String>,
    pub retry_interval_ms: Option<u64>,
    pub pending_pdu_count: u64,
    pub pending_edu_count: u64,
}

/// The OpenAPI `AppService` schema: one row of `GET /appservices` and the body of every
/// per-appservice operation that answers with the appservice. Served by whatever implements
/// [`crate::sources::AppserviceDirectory`] (`hs-appservice`'s real one over its registry, or
/// [`crate::sources::InMemoryAppserviceDirectory`] for tests).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct AdminAppservice {
    pub id: String,
    pub sender_localpart: String,
    /// `None` for a registration with `url: null`, which is never pushed to.
    pub url: Option<String>,
    /// The registration's `namespaces` as written: `users`/`aliases`/`rooms`, each a list of
    /// `{regex, exclusive}`.
    pub namespaces: serde_json::Value,
    pub rate_limited: bool,
    pub protocols: Vec<String>,
    pub paused: bool,
    /// `healthy`, `degraded`, `down`, `paused` or `unknown` -- the same word
    /// [`AdminAppserviceHealth::status`] carries, so a list can be coloured without a request per
    /// row.
    pub health: String,
    /// RFC 3339 millisecond-precision UTC.
    pub created_at: String,
    /// The catalogue entry (`GET /bridge-types`) this appservice was created from, read from
    /// the registration's `io.myelin.bridge_type` key; `None` for a registration that did not
    /// come through the catalogue.
    pub bridge_type: Option<String>,
    /// Where an operator can go from here. `login_url` is where a bridge with its own login
    /// flow puts it; nothing sets it yet.
    pub links: AdminAppserviceLinks,
}

/// `AppService.links`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct AdminAppserviceLinks {
    pub login_url: Option<String>,
}

/// The OpenAPI `AppServiceHealth` schema.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct AdminAppserviceHealth {
    pub status: String,
    pub last_ping_at: Option<String>,
    pub last_error: Option<String>,
}

/// The OpenAPI `AppServiceBacklogEntry` schema: one queued or dead-lettered transaction.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct AdminAppserviceBacklogEntry {
    pub transaction_id: String,
    pub age_ms: u64,
    pub attempts: u32,
    pub last_error: Option<String>,
    pub dead_lettered: bool,
}

/// The OpenAPI `AppServiceTokens` schema: the body of `POST /appservices/{id}/rotate-tokens`,
/// and the one place the tokens are ever shown. Debug shows neither.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub struct AdminAppserviceTokens {
    pub as_token: String,
    pub hs_token: String,
}

impl std::fmt::Debug for AdminAppserviceTokens {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AdminAppserviceTokens")
            .field("as_token", &"<redacted>")
            .field("hs_token", &"<redacted>")
            .finish()
    }
}

/// The OpenAPI `AppServiceCreate` schema: a registration, as the JSON object a registration
/// file holds or as that file's text. One of the two.
#[derive(Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct AdminAppserviceCreate {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub registration: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub registration_yaml: Option<String>,
}

impl std::fmt::Debug for AdminAppserviceCreate {
    // A registration carries both tokens.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AdminAppserviceCreate")
            .field(
                "registration",
                &self.registration.as_ref().map(|_| "<redacted>"),
            )
            .field(
                "registration_yaml",
                &self.registration_yaml.as_ref().map(|_| "<redacted>"),
            )
            .finish()
    }
}

/// The OpenAPI `AppServiceReplayRequest` schema: which dead-lettered transactions to send again.
/// Both absent means all of them.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct AdminAppserviceReplay {
    #[serde(default)]
    pub transaction_ids: Vec<String>,
    /// RFC 3339; only entries queued at or after it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub since: Option<String>,
}

/// The OpenAPI `Room` schema: one row of `GET /rooms` and the body of `GET /rooms/{room_id}`,
/// `.../block`, `.../unblock`, `.../make-admin`. Field-for-field match with that schema. Served by
/// whatever implements [`crate::sources::RoomDirectory`] (track 04's real implementation, or
/// [`crate::sources::InMemoryRoomDirectory`] for tests).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct AdminRoom {
    pub room_id: String,
    pub name: Option<String>,
    pub topic: Option<String>,
    pub avatar_url: Option<String>,
    pub canonical_alias: Option<String>,
    pub joined_members_count: u64,
    pub local_members_count: u64,
    pub state_events_count: u64,
    pub version: String,
    pub creator: Option<String>,
    pub encrypted: bool,
    pub join_rule: String,
    pub guest_access: String,
    pub history_visibility: String,
    pub federatable: bool,
    pub public: bool,
    pub room_type: Option<String>,
    pub blocked: bool,
    pub blocked_reason: Option<String>,
    pub tombstoned: bool,
    pub replacement_room_id: Option<String>,
    pub forgotten: bool,
}

/// The OpenAPI `ThreePid` schema, used by `users.create`'s request body and `users.lookup`'s
/// match criteria.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ThreePid {
    pub medium: String,
    pub address: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub added_at: Option<String>,
}

/// The OpenAPI `ExternalId` schema, used by `users.create`'s request body and `users.lookup`'s
/// match criteria.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExternalId {
    pub provider: String,
    pub external_id: String,
}

/// The static, operator-configured parts of the OpenAPI `ServerInfo` schema: everything except
/// `uptime_ms`, which [`AdminState`](crate::router::AdminState) computes per-request from its
/// start time rather than storing. See [`ServerInfo::with_uptime_ms`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerInfo {
    pub name: String,
    pub version: String,
    pub build: String,
    pub supported_room_versions: Vec<String>,
    pub enabled_components: Vec<String>,
    pub contract_version: String,
}

impl Default for ServerInfo {
    fn default() -> Self {
        Self {
            name: "hs".to_string(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            build: "dev".to_string(),
            supported_room_versions: Vec::new(),
            enabled_components: Vec::new(),
            contract_version: "1.0".to_string(),
        }
    }
}

impl ServerInfo {
    /// The full `GET /server` response body: this value's static fields plus `uptime_ms`
    /// computed at request time.
    pub fn with_uptime_ms(&self, uptime_ms: u64) -> ServerInfoResponse {
        ServerInfoResponse {
            name: self.name.clone(),
            version: self.version.clone(),
            build: self.build.clone(),
            supported_room_versions: self.supported_room_versions.clone(),
            enabled_components: self.enabled_components.clone(),
            uptime_ms,
            contract_version: self.contract_version.clone(),
        }
    }
}

/// The OpenAPI `ServerInfo` schema as actually served (includes `uptime_ms`; see [`ServerInfo`]).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerInfoResponse {
    pub name: String,
    pub version: String,
    pub build: String,
    pub supported_room_versions: Vec<String>,
    pub enabled_components: Vec<String>,
    pub uptime_ms: u64,
    pub contract_version: String,
}

/// The OpenAPI `ServerHealth` schema. `status` is one of `ok`, `degraded`, `down`; `checks` maps
/// a check name to a free-text status (`"ok"`, `"unknown"`, or a failure description). A check
/// whose backing source is not wired is reported as `"unknown"`, never as `"ok"` (RFC 0004
/// doesn't specify this precisely; see `docs/status/15-admin-api-and-modules.md` "Decisions
/// made").
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerHealth {
    pub status: String,
    pub checks: std::collections::BTreeMap<String, String>,
}

impl ServerHealth {
    /// Builds a `ServerHealth` from a set of individual check results: `status` is `"ok"` when
    /// every check is `"ok"`, `"down"` when any check reports `"down"`, and `"degraded"`
    /// otherwise (for example, one or more checks are `"unknown"` because their source isn't
    /// wired yet).
    pub fn from_checks(checks: std::collections::BTreeMap<String, String>) -> Self {
        let status = if checks.values().any(|v| v == "down") {
            "down"
        } else if checks.values().all(|v| v == "ok") {
            "ok"
        } else {
            "degraded"
        };
        Self {
            status: status.to_string(),
            checks,
        }
    }
}

/// `Task` (RFC 0004 section 3.7): the admin face of a long-running operation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Task {
    pub id: String,
    pub action: String,
    pub status: TaskStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resource: Option<ResourceRef>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub progress: Option<TaskProgress>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<hs_http::Problem>,
    pub created_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub started_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scheduled_for: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_by: Option<Actor>,
}

impl Task {
    pub fn scheduled(
        action: impl Into<String>,
        resource: Option<ResourceRef>,
        created_by: Actor,
    ) -> Self {
        Self {
            id: new_id(),
            action: action.into(),
            status: TaskStatus::Scheduled,
            resource,
            progress: None,
            result: None,
            error: None,
            created_at: hs_http::time::now_rfc3339(),
            started_at: None,
            finished_at: None,
            scheduled_for: None,
            created_by: Some(created_by),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    Scheduled,
    Running,
    Succeeded,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskProgress {
    pub current: u64,
    pub total: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unit: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

/// `AuditEntry` (RFC 0004 section 9).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditEntry {
    pub id: String,
    pub recorded_at: String,
    pub action: String,
    pub actor: Actor,
    pub target: ResourceRef,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request: Option<AuditRequest>,
    pub outcome: AuditOutcome,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub changes: Vec<AuditChange>,
    pub replayed: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub replay_of: Option<String>,
}

impl AuditEntry {
    /// Builds a fresh entry for one mutation, stamping `id` and `recorded_at`.
    pub fn new(
        action: impl Into<String>,
        actor: Actor,
        target: ResourceRef,
        outcome: AuditOutcome,
    ) -> Self {
        Self {
            id: new_id(),
            recorded_at: hs_http::time::now_rfc3339(),
            action: action.into(),
            actor,
            target,
            request: None,
            outcome,
            changes: Vec::new(),
            replayed: false,
            replay_of: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditRequest {
    pub method: String,
    pub path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub idempotency_key: Option<String>,
    /// Secrets redacted, truncated at 64 KiB by the caller before storing (RFC 0004 section 9).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditOutcome {
    pub status: u16,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub problem: Option<hs_http::Problem>,
}

impl AuditOutcome {
    pub fn success(status: u16) -> Self {
        Self {
            status,
            problem: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditChange {
    pub pointer: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub to: Option<serde_json::Value>,
}

/// The JSON payload of one SSE frame's `data` field (RFC 0004 section 10).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Event {
    pub id: String,
    pub r#type: String,
    pub recorded_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resource: Option<ResourceRef>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub actor: Option<Actor>,
    pub data: serde_json::Value,
}

impl Event {
    pub fn new(r#type: impl Into<String>, data: serde_json::Value) -> Self {
        Self {
            id: new_id(),
            r#type: r#type.into(),
            recorded_at: hs_http::time::now_rfc3339(),
            resource: None,
            actor: None,
            data,
        }
    }

    pub fn with_resource(mut self, resource: ResourceRef) -> Self {
        self.resource = Some(resource);
        self
    }

    pub fn with_actor(mut self, actor: Actor) -> Self {
        self.actor = Some(actor);
        self
    }

    /// Renders this event as one SSE frame, including the trailing blank line.
    pub fn to_sse_frame(&self) -> String {
        let data = serde_json::to_string(self).unwrap_or_else(|_| "{}".to_string());
        format!(
            "id: {}\nevent: {}\ndata: {}\n\n",
            self.id, self.r#type, data
        )
    }
}

/// The OpenAPI `ConfigSection` schema: one row of `GET /config` and the body of `GET
/// /config/{section}` and `PATCH /config/{section}`. Served by whatever implements
/// [`crate::sources::ConfigSource`].
///
/// `values` is the section's *effective* configuration — what this server is actually running
/// on, after the bootstrap file, the database and the environment have been merged — with every
/// secret already replaced by `{"$secret": true}` (see [`crate::config_schema`]). Its keys are
/// relative to the section, because that is the object a form edits and a merge patch is written
/// against.
///
/// `origins` is keyed by *whole-configuration* JSON Pointer (`/auth/enable_registration`)
/// instead, matching `hs_config::Resolved::origins` and the pointers `GET /config/schema`
/// reports, so the management interface has one pointer vocabulary across both endpoints rather
/// than two that differ by a prefix.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConfigSection {
    pub name: String,
    /// Whether this section can be swapped into a running server (`hs_config::reload`).
    pub reloadable: bool,
    /// Whether this section is read before the database is open and so can never be stored in
    /// it (`storage` today). The management interface renders these read-only; `config.update`
    /// refuses them.
    pub bootstrap: bool,
    /// The highest-precedence layer that sets anything in this section (`default`, `file`,
    /// `database` or `environment`) — what an operator sees at a glance in the section list.
    pub source: String,
    /// RFC 3339 UTC, or `None` when this section has never been reloaded on a running server.
    pub last_reloaded_at: Option<String>,
    pub values: serde_json::Value,
    pub origins: std::collections::BTreeMap<String, String>,
    /// The configuration's revision at the time of this read, which is also this section's ETag
    /// for `If-Match`. It counts writes to the configuration as a whole, not to this section:
    /// any concurrent change is a reason to re-read before patching.
    pub revision: u64,
    /// The most recent changes to this section, newest first. Populated by `GET
    /// /config/{section}`; left empty by the section list, which would otherwise pay for ten
    /// histories nobody asked for.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub history: Vec<ConfigChange>,
}

/// One recorded configuration change (`hs_config::store::ChangeRecord` on the wire). `patch` is
/// redacted exactly as [`ConfigSection::values`] is: a secret that was set through this API must
/// not be readable back out of its own history.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConfigChange {
    pub revision: u64,
    pub section: String,
    pub patch: serde_json::Value,
    pub actor: Option<String>,
    /// RFC 3339 millisecond-precision UTC (RFC 0004 D15.2).
    pub at: String,
}

/// The OpenAPI `ConfigValidateReport` schema: the answer to "would this configuration be
/// accepted?", asked without applying it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConfigValidateReport {
    pub valid: bool,
    #[serde(default)]
    pub errors: Vec<hs_http::ValidationError>,
    /// Sections whose new value could not be applied without restarting the process
    /// (`hs_config::reload::sections_requiring_restart`). Empty does not mean "no change"; it
    /// means every change is one a running server can take.
    #[serde(default)]
    pub requires_restart: Vec<String>,
}

impl ConfigValidateReport {
    /// A report saying the candidate configuration is fine.
    #[must_use]
    pub fn valid(requires_restart: Vec<String>) -> Self {
        Self {
            valid: true,
            errors: Vec::new(),
            requires_restart,
        }
    }

    /// A report carrying every problem found, rather than the first.
    #[must_use]
    pub fn invalid(errors: Vec<hs_http::ValidationError>) -> Self {
        Self {
            valid: false,
            errors,
            requires_restart: Vec::new(),
        }
    }
}

/// The OpenAPI `ConfigReloadReport` schema: what a re-read of the configuration layers actually
/// changed on the running server.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ConfigReloadReport {
    pub reloaded_sections: Vec<String>,
    #[serde(default)]
    pub errors: Vec<hs_http::ValidationError>,
    /// Sections that changed but could not be hot-applied. Reported rather than swallowed: an
    /// operator who is told "reloaded" and then finds the old value still in force has been
    /// lied to.
    #[serde(default)]
    pub requires_restart: Vec<String>,
    pub revision: u64,
}

/// The OpenAPI `ConfigSchema` schema, served by `GET /config/schema`: everything the management
/// interface needs to render configuration forms without knowing a single field name.
///
/// `schema` is the JSON Schema `schemars` derives from `hs_config::Config` — types, defaults,
/// enums, descriptions and the shape of every nested object. `settings` is the live half: one
/// row per setting this server actually has, saying where its value came from and whether the
/// interface may offer to change it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConfigSchema {
    pub schema: serde_json::Value,
    pub sections: Vec<ConfigSectionInfo>,
    pub settings: Vec<ConfigSettingInfo>,
    pub revision: u64,
}

/// One section's metadata in [`ConfigSchema`], without its values.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConfigSectionInfo {
    pub name: String,
    pub reloadable: bool,
    pub bootstrap: bool,
    pub source: String,
}

/// One setting's metadata in [`ConfigSchema`], keyed by whole-configuration JSON Pointer so it
/// lines up with both [`ConfigSection::origins`] and the `schema` member's own structure.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConfigSettingInfo {
    pub pointer: String,
    pub section: String,
    /// `default`, `file`, `database` or `environment`.
    pub origin: String,
    /// Whether this setting's value is a secret and is therefore served redacted.
    pub secret: bool,
    /// Whether changing it takes effect without a restart.
    pub reloadable: bool,
    /// Whether `config.update` would accept a change to it. False for a bootstrap section and
    /// for anything an `HS__` environment variable pins, so the interface can show the field
    /// read-only with a reason instead of offering an edit that would be refused.
    pub editable: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn admin_write_satisfies_everything() {
        for scope in [
            Scope::AdminRead,
            Scope::BridgesRead,
            Scope::BridgesWrite,
            Scope::ModerationRead,
            Scope::ModerationWrite,
        ] {
            assert!(Scope::AdminWrite.satisfies(scope));
        }
    }

    #[test]
    fn write_satisfies_its_own_read() {
        assert!(Scope::BridgesWrite.satisfies(Scope::BridgesRead));
        assert!(Scope::ModerationWrite.satisfies(Scope::ModerationRead));
        assert!(!Scope::BridgesRead.satisfies(Scope::BridgesWrite));
    }

    #[test]
    fn admin_read_does_not_satisfy_bridges_read() {
        assert!(!Scope::AdminRead.satisfies(Scope::BridgesRead));
    }

    #[test]
    fn scope_round_trips_through_wire_form() {
        for s in [
            Scope::AdminRead,
            Scope::AdminWrite,
            Scope::BridgesRead,
            Scope::BridgesWrite,
            Scope::ModerationRead,
            Scope::ModerationWrite,
        ] {
            assert_eq!(Scope::parse(s.as_str()), Some(s));
        }
        assert_eq!(Scope::parse("bogus"), None);
    }

    #[test]
    fn event_sse_frame_has_id_event_and_data_lines() {
        let e = Event::new("user.suspended", serde_json::json!({"reason": "spam"}));
        let frame = e.to_sse_frame();
        assert!(frame.starts_with("id: "));
        assert!(frame.contains("event: user.suspended\n"));
        assert!(frame.ends_with("\n\n"));
    }

    #[test]
    fn new_id_is_ulid_shaped() {
        let id = new_id();
        assert_eq!(id.len(), 26);
    }

    #[test]
    fn paginate_first_page_has_next_but_no_prev() {
        let items: Vec<u32> = (0..10).collect();
        let page = Page::paginate(items, None, Some(4), false);
        assert_eq!(page.items, vec![0, 1, 2, 3]);
        assert_eq!(page.next_cursor, Some("4".to_string()));
        assert_eq!(page.prev_cursor, None);
        assert_eq!(page.total, None);
    }

    #[test]
    fn paginate_middle_page_has_both_cursors() {
        let items: Vec<u32> = (0..10).collect();
        let page = Page::paginate(items, Some("4"), Some(4), true);
        assert_eq!(page.items, vec![4, 5, 6, 7]);
        assert_eq!(page.next_cursor, Some("8".to_string()));
        assert_eq!(page.prev_cursor, Some("0".to_string()));
        assert_eq!(page.total, Some(10));
    }

    #[test]
    fn paginate_last_page_has_no_next() {
        let items: Vec<u32> = (0..10).collect();
        let page = Page::paginate(items, Some("8"), Some(4), false);
        assert_eq!(page.items, vec![8, 9]);
        assert_eq!(page.next_cursor, None);
    }

    #[test]
    fn paginate_unparseable_cursor_is_treated_as_the_start() {
        let items: Vec<u32> = (0..3).collect();
        let page = Page::paginate(items, Some("not-a-number"), Some(10), false);
        assert_eq!(page.items, vec![0, 1, 2]);
    }

    #[test]
    fn admin_user_default_is_well_formed() {
        let user = AdminUser::default();
        assert_eq!(user.user_id, "");
        assert!(!user.admin);
        assert_eq!(user.device_count, 0);
    }

    #[test]
    fn admin_room_default_is_well_formed() {
        let room = AdminRoom::default();
        assert_eq!(room.room_id, "");
        assert!(!room.blocked);
        assert_eq!(room.joined_members_count, 0);
    }

    #[test]
    fn server_info_with_uptime_carries_static_fields() {
        let info = ServerInfo::default();
        let response = info.with_uptime_ms(1234);
        assert_eq!(response.uptime_ms, 1234);
        assert_eq!(response.contract_version, info.contract_version);
    }

    #[test]
    fn server_health_is_ok_only_when_every_check_is_ok() {
        let mut checks = std::collections::BTreeMap::new();
        checks.insert("audit".to_string(), "ok".to_string());
        checks.insert("users".to_string(), "ok".to_string());
        let health = ServerHealth::from_checks(checks);
        assert_eq!(health.status, "ok");
    }

    #[test]
    fn server_health_is_degraded_when_a_check_is_unknown() {
        let mut checks = std::collections::BTreeMap::new();
        checks.insert("audit".to_string(), "ok".to_string());
        checks.insert("users".to_string(), "unknown".to_string());
        let health = ServerHealth::from_checks(checks);
        assert_eq!(health.status, "degraded");
    }

    #[test]
    fn server_health_is_down_when_any_check_is_down() {
        let mut checks = std::collections::BTreeMap::new();
        checks.insert("audit".to_string(), "down".to_string());
        checks.insert("users".to_string(), "ok".to_string());
        let health = ServerHealth::from_checks(checks);
        assert_eq!(health.status, "down");
    }

    #[test]
    fn principal_to_actor_maps_legacy_to_user() {
        let principal = Principal {
            kind: PrincipalKind::Legacy,
            id: "@ops:example.org".into(),
            display_name: Some("Ops".into()),
            scopes: vec![Scope::AdminWrite],
            token_id: Some("tok1".into()),
            expires_at: None,
            issued_by: None,
        };
        let actor = principal.to_actor();
        assert_eq!(actor.kind, ActorKind::User);
        assert_eq!(actor.id, "@ops:example.org");
        assert_eq!(actor.token_id, Some("tok1".into()));
    }
}
