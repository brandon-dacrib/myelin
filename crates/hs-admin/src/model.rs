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
