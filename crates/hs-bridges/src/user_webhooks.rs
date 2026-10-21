//! Self-service webhooks for every user (decision 0057): the API behind the `/integrations/`
//! page, under [`PREFIX`] on the client listener, authenticated by a person's ordinary Matrix
//! access token through a [`CallerResolver`] the binary provides (the `Requester` middleware).
//! Not under `/api/v1`: the admin API's verifier refuses a non-administrator's token like an
//! unknown one, by design.
//!
//! What it does is what the admin API's `POST /bridge-offerings/matrix-hookshot/connections`
//! does (decision 0056), as the shared Hookshot's bot, for a room the caller may set up:
//!
//! - `GET {PREFIX}/sources`: the webhook sources, as the admin API lists them.
//! - `GET {PREFIX}/rooms`: the caller's joined rooms (up to [`ROOM_LIMIT`]), each with
//!   whether a webhook may be added there and, if not, why.
//! - `GET {PREFIX}/webhooks`: the caller's webhooks: the active webhook connections in the
//!   rooms they are joined to, with who made each and when, where that is known.
//! - `POST {PREFIX}/webhooks`: `{source, name, room: {id} | {create: {name, topic?}}}`;
//!   answers `201` with `{room_id, name, source, url, setup, files}`, the URL's one showing.
//! - `DELETE {PREFIX}/webhooks/{room_id}/{name}`: removes a webhook; `200 {}`.
//!
//! The rules for an existing room ([`verdict`], read from the room's current state as the
//! caller sees it): the caller is a joined member with the power to send the connection's
//! state event type (`events[type]`, else `state_default`); the room is not encrypted (Hookshot
//! here runs without encryption and would post in the clear); if the bot is not in the room,
//! the caller may invite it, the server invites it as the caller and the bot joins; if the bot
//! has not the power to send the state event, the caller must be able to grant it, and the
//! server raises the bot to that level as the caller. For a new room the bot makes it
//! (private), invites the caller, gives them power 100 in it, and the server joins them to it
//! as themselves, so it is in their client at once. Then the manager makes the connection
//! exactly as it does for an administrator, with `io.myelin.created_by` in the state event.
//!
//! Every make and remove is written to the admin audit log with the user as actor
//! (`bridge_connection.created`/`.deleted`, never the URL), logged at INFO, and counted in
//! `hs_bridge_webhook_connections_total{via="user"}`. Creation is rate-limited per user:
//! [`CREATE_BURST`] at once, refilling one every [`CREATE_REFILL`] (a small in-process token
//! bucket; a user who hits it is answered `M_LIMIT_EXCEEDED` with `retry_after_ms`).
//!
//! Errors are Matrix errors: `M_FORBIDDEN` for a room the caller may not set up,
//! `M_NOT_FOUND` when the server offers no Hookshot with a public URL (or the webhook is not
//! there), `M_INVALID_PARAM` for a bad body, `M_LIMIT_EXCEEDED` over the limit.

// A `MatrixError` is the response a failed request gets, built once on the way out; the
// handlers' `Result`s carry it rather than box it, as `hs_admin::handler_kit` does.
#![allow(clippy::result_large_err)]

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use axum::extract::{Path, Request, State};
use axum::http::StatusCode;
use axum::http::request::Parts;
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get};
use axum::{Json, Router};
use hs_admin::audit::AuditSink;
use hs_admin::bridge_connections::{self, GENERIC_HOOK_TYPE};
use hs_admin::bridge_offerings::BridgeOfferingSource;
use hs_admin::model::{
    Actor, ActorKind, AuditEntry, AuditOutcome, BridgeConnectionCreate, BridgeConnectionCreated,
    BridgeConnectionRoom, BridgeWebhookSource, ResourceRef,
};
use hs_admin::sources::SourceError;
use hs_http::{MatrixError, MatrixErrorCode};
use hs_kv::KvBackend;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::manager::{BridgeManager, CREATED_AT_MARKER, CREATED_BY_MARKER};
use crate::matrix::{MatrixClient, MatrixError as ClientError};

/// Where the user API is mounted on the client listener.
pub const PREFIX: &str = "/_matrix/client/unstable/io.myelin.webhooks";

/// The one bridge type that takes webhooks: the shared Hookshot offering.
pub const BRIDGE_TYPE: &str = "matrix-hookshot";

/// How many webhooks a user may make at once before the limit bites.
pub const CREATE_BURST: u32 = 10;

/// How often one more creation is allowed once the burst is spent (ten an hour).
pub const CREATE_REFILL: Duration = Duration::from_secs(360);

/// How many of the caller's joined rooms `GET /rooms` looks at (newest first as the server
/// lists them): each costs a read of the room's state, and a page that offers more rooms
/// than this is not a page anyone scrolls.
pub const ROOM_LIMIT: usize = 200;

/// How many of the caller's rooms `GET /rooms` reads at once.
const ROOM_READS_AT_ONCE: usize = 8;

/// The power level a new room's maker gets in the room the bot made for them.
const ROOM_MAKER_POWER: i64 = 100;

/// Who a request is from: a person with their own access token.
#[derive(Debug, Clone)]
pub struct Caller {
    /// Their Matrix id.
    pub user_id: String,
    /// The access token the request carried: the server acts as them with it, over loopback,
    /// for what they asked (reading their rooms, inviting the bot, joining the new room).
    pub token: String,
    /// Where the request came from, for the audit entry.
    pub ip: Option<String>,
    /// The request's `User-Agent`, for the audit entry.
    pub user_agent: Option<String>,
}

/// Authenticates a request by its ordinary Matrix access token. The binary implements this
/// with the `Requester` middleware; a test with a table of tokens.
#[async_trait]
pub trait CallerResolver: Send + Sync {
    /// The caller, or the Matrix error response that refuses the request.
    async fn resolve(&self, parts: &mut Parts) -> Result<Caller, Response>;
}

/// The user API's state.
pub struct UserWebhooks<B: KvBackend> {
    manager: Arc<BridgeManager<B>>,
    audit: Arc<dyn AuditSink>,
    resolver: Arc<dyn CallerResolver>,
    limiter: CreateLimiter,
}

/// The routes under [`PREFIX`], to be merged into the client listener's router.
pub fn router<B: KvBackend + 'static>(
    manager: Arc<BridgeManager<B>>,
    audit: Arc<dyn AuditSink>,
    resolver: Arc<dyn CallerResolver>,
) -> Router {
    let state = Arc::new(UserWebhooks {
        manager,
        audit,
        resolver,
        limiter: CreateLimiter::new(CREATE_BURST, CREATE_REFILL),
    });
    Router::new()
        .route(&format!("{PREFIX}/sources"), get(sources::<B>))
        .route(&format!("{PREFIX}/rooms"), get(rooms::<B>))
        .route(
            &format!("{PREFIX}/webhooks"),
            get(list::<B>).post(create::<B>),
        )
        .route(
            &format!("{PREFIX}/webhooks/{{room_id}}/{{name}}"),
            delete(remove::<B>),
        )
        .with_state(state)
}

// ---- the answers -----------------------------------------------------------------------------

/// `GET /sources`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SourceList {
    pub items: Vec<BridgeWebhookSource>,
}

/// `GET /rooms`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoomList {
    pub rooms: Vec<UserRoom>,
}

/// One of the caller's rooms, with whether a webhook may be added there.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserRoom {
    pub room_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Whether the bridge's bot is in the room already (if not, it is invited on the way).
    pub bot_joined: bool,
    /// Whether the caller may add a webhook there.
    pub may_add: bool,
    /// Why not, when `may_add` is false; in the caller's terms.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// `GET /webhooks`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WebhookList {
    pub items: Vec<UserWebhook>,
}

/// One webhook in a room the caller is in.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserWebhook {
    pub room_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub room_name: Option<String>,
    /// The connection's name (its state key).
    pub name: String,
    /// The source it was made from, when it was made through the server.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// Who made it through this API, when it was.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_by: Option<String>,
    /// When it was made through this API, in milliseconds since the epoch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_at: Option<u64>,
}

/// `POST /webhooks`: the admin API's `BridgeConnectionCreate`, without `invite` (the caller is
/// the one invited to a new room).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct WebhookCreate {
    /// A source id (`GET /sources`).
    pub source: String,
    /// 3 to 64 characters, Hookshot's limit.
    pub name: String,
    #[serde(default)]
    pub room: RoomChoice,
}

/// Which room: one the caller is in, or a new one.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct RoomChoice {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub create: Option<RoomCreate>,
}

/// A room to make: private, by the bot, with the caller invited, joined and made its admin.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct RoomCreate {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub topic: Option<String>,
}

// ---- the rules -------------------------------------------------------------------------------

/// What a room's current state says that matters here, read from the full state events as
/// the caller sees them ([`RoomFacts::from_state`]).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RoomFacts {
    /// `m.room.name`.
    pub name: Option<String>,
    /// The caller's membership (`join`, `invite`, `leave`, ...), if they have one.
    pub user_membership: Option<String>,
    /// The bot's membership, if it has one.
    pub bot_membership: Option<String>,
    /// `m.room.power_levels`' content, if the room has the event.
    pub power_levels: Option<Value>,
    /// The room's creators: the sender of `m.room.create` and, in room version 12 and later,
    /// its `additional_creators`.
    pub creators: Vec<String>,
    /// Room version 12 and later: creators have infinite power and are not in `users`.
    pub privileged_creators: bool,
    /// `m.room.encryption` is set.
    pub encrypted: bool,
    /// Active webhook connections (`uk.half-shot.matrix-hookshot.generic.hook` state keys
    /// whose event has content).
    pub active_webhooks: Vec<String>,
}

impl RoomFacts {
    /// Reads the facts from `events` (whole state events: `type`, `state_key`, `content`,
    /// `sender`) for `user` and `bot`.
    #[must_use]
    pub fn from_state(events: &[Value], user: &str, bot: &str) -> Self {
        let mut facts = Self::default();
        for event in events {
            let kind = event["type"].as_str().unwrap_or_default();
            let key = event["state_key"].as_str().unwrap_or_default();
            let content = &event["content"];
            match kind {
                "m.room.name" => {
                    facts.name = content["name"]
                        .as_str()
                        .filter(|n| !n.is_empty())
                        .map(str::to_owned);
                }
                "m.room.member" if key == user => {
                    facts.user_membership = content["membership"].as_str().map(str::to_owned);
                }
                "m.room.member" if key == bot => {
                    facts.bot_membership = content["membership"].as_str().map(str::to_owned);
                }
                "m.room.power_levels" if key.is_empty() => {
                    facts.power_levels = Some(content.clone());
                }
                "m.room.create" if key.is_empty() => {
                    if let Some(sender) = event["sender"].as_str() {
                        facts.creators.push(sender.to_owned());
                    }
                    let version = content["room_version"].as_str().unwrap_or("1");
                    facts.privileged_creators = version.parse::<u32>().is_ok_and(|v| v >= 12);
                    if facts.privileged_creators {
                        facts.creators.extend(
                            content["additional_creators"]
                                .as_array()
                                .into_iter()
                                .flatten()
                                .filter_map(Value::as_str)
                                .map(str::to_owned),
                        );
                    }
                }
                "m.room.encryption" if key.is_empty() => {
                    facts.encrypted =
                        content.is_object() && !bridge_connections::is_removed(content);
                }
                GENERIC_HOOK_TYPE if !bridge_connections::is_removed(content) => {
                    facts.active_webhooks.push(key.to_owned());
                }
                _ => {}
            }
        }
        facts
    }

    /// `who`'s power level: infinite for a privileged creator, else `users[who]`,
    /// `users_default`, and 100 for the creator of a room without a power levels event.
    #[must_use]
    pub fn power_of(&self, who: &str) -> i64 {
        if self.privileged_creators && self.creators.iter().any(|c| c == who) {
            return i64::MAX;
        }
        match &self.power_levels {
            Some(pl) => pl["users"][who]
                .as_i64()
                .or_else(|| pl["users_default"].as_i64())
                .unwrap_or(0),
            None => {
                if self.creators.iter().any(|c| c == who) {
                    100
                } else {
                    0
                }
            }
        }
    }

    /// The power needed to send the state event `event_type`: `events[type]`, else
    /// `state_default` (50 when the event is there but says nothing; 0 without the event).
    #[must_use]
    pub fn required_to_send_state(&self, event_type: &str) -> i64 {
        match &self.power_levels {
            Some(pl) => pl["events"][event_type]
                .as_i64()
                .or_else(|| pl["state_default"].as_i64())
                .unwrap_or(50),
            None => 0,
        }
    }

    /// The power needed to invite: `invite`, else 0.
    #[must_use]
    pub fn required_to_invite(&self) -> i64 {
        self.power_levels
            .as_ref()
            .and_then(|pl| pl["invite"].as_i64())
            .unwrap_or(0)
    }
}

/// What the caller may do in a room, by [`verdict`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// A webhook may be added. `invite_bot`: the bot is not in the room and is to be invited
    /// as the caller. `raise_bot_to`: the bot lacks the power to send the connection's state
    /// event and is to be raised to this level as the caller.
    Allowed {
        invite_bot: bool,
        raise_bot_to: Option<i64>,
    },
    /// Not, and why, in the caller's terms.
    Refused(String),
}

/// Whether `user` may add a webhook to a room with `facts`, and what the server has to do for
/// `bot` on the way.
#[must_use]
pub fn verdict(facts: &RoomFacts, user: &str, bot: &str) -> Verdict {
    if facts.user_membership.as_deref() != Some("join") {
        return Verdict::Refused("you are not in that room".into());
    }
    if facts.encrypted {
        return Verdict::Refused(
            "that room is encrypted, and the webhooks bridge does not encrypt: use a room without encryption".into(),
        );
    }
    let required = facts.required_to_send_state(GENERIC_HOOK_TYPE);
    let user_power = facts.power_of(user);
    if user_power < required {
        return Verdict::Refused(format!(
            "adding a webhook there takes power level {required} (it is a state event in the room) and you have {user_power}; ask a room admin to raise you, or use a room of your own"
        ));
    }
    let bot_joined = facts.bot_membership.as_deref() == Some("join");
    if !bot_joined && user_power < facts.required_to_invite() {
        return Verdict::Refused(format!(
            "the bridge's bot {bot} is not in that room and you may not invite it (inviting takes power level {})",
            facts.required_to_invite()
        ));
    }
    let bot_power = facts.power_of(bot);
    let raise_bot_to = if bot_power < required {
        let changes_power = facts.required_to_send_state("m.room.power_levels");
        if user_power < changes_power || user_power < required {
            return Verdict::Refused(format!(
                "the bridge's bot {bot} needs power level {required} in that room to add the webhook, and you cannot grant it (changing power levels takes {changes_power}); ask a room admin"
            ));
        }
        Some(required)
    } else {
        None
    };
    Verdict::Allowed {
        invite_bot: !bot_joined,
        raise_bot_to,
    }
}

/// `facts.power_levels` with `who` at `level` in `users`; a room without the event gets one
/// with the room's defaults, which is what the server would have assumed.
#[must_use]
pub fn power_levels_with(facts: &RoomFacts, who: &str, level: i64) -> Value {
    let mut pl = facts.power_levels.clone().unwrap_or_else(|| json!({}));
    if !pl["users"].is_object() {
        pl["users"] = json!({});
    }
    pl["users"][who] = json!(level);
    pl
}

// ---- the rate limit --------------------------------------------------------------------------

struct Bucket {
    tokens: f64,
    last: Instant,
}

/// A per-user token bucket for creations: `burst` at once, one more every `refill`.
struct CreateLimiter {
    burst: f64,
    refill: Duration,
    buckets: Mutex<HashMap<String, Bucket>>,
}

impl CreateLimiter {
    fn new(burst: u32, refill: Duration) -> Self {
        Self {
            burst: f64::from(burst),
            refill,
            buckets: Mutex::new(HashMap::new()),
        }
    }

    /// Takes one token for `user` at `now`, or says how long until the next one.
    fn take(&self, user: &str, now: Instant) -> Result<(), Duration> {
        let mut buckets = match self.buckets.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        let bucket = buckets.entry(user.to_owned()).or_insert(Bucket {
            tokens: self.burst,
            last: now,
        });
        let elapsed = now.saturating_duration_since(bucket.last);
        let refilled = elapsed.as_secs_f64() / self.refill.as_secs_f64();
        bucket.tokens = (bucket.tokens + refilled).min(self.burst);
        bucket.last = now;
        if bucket.tokens >= 1.0 {
            bucket.tokens -= 1.0;
            Ok(())
        } else {
            let wait = (1.0 - bucket.tokens) * self.refill.as_secs_f64();
            Err(Duration::from_secs_f64(wait.max(0.001)))
        }
    }
}

// ---- the handlers ----------------------------------------------------------------------------

/// The bridge, as far as the user API needs it: the bot, the token the server acts as it with,
/// and the client.
struct Bridge {
    client: MatrixClient,
    token: String,
    bot: String,
}

impl<B: KvBackend + 'static> UserWebhooks<B> {
    /// The shared Hookshot, when it is offered, has a public URL and is registered.
    async fn bridge(&self) -> Result<Bridge, MatrixError> {
        let offering = self
            .manager
            .get(BRIDGE_TYPE)
            .await
            .map_err(source_error)?
            .ok_or_else(|| {
                MatrixError::not_found(
                    "this server does not offer webhooks (no Hookshot bridge is offered); ask an administrator",
                )
            })?;
        if offering
            .options
            .public_url
            .as_deref()
            .map(str::trim)
            .is_none_or(str::is_empty)
        {
            return Err(MatrixError::not_found(
                "this server's webhooks bridge has no public URL yet, so no webhook can be reached from outside; ask an administrator",
            ));
        }
        let (client, token, bot) = self
            .manager
            .shared_bot(BRIDGE_TYPE)
            .map_err(source_error)?
            .ok_or_else(|| {
                MatrixError::not_found(
                    "this server's webhooks bridge is not running yet; try again in a moment, or ask an administrator",
                )
            })?;
        Ok(Bridge { client, token, bot })
    }

    async fn caller(&self, parts: &mut Parts) -> Result<Caller, Response> {
        self.resolver.resolve(parts).await
    }

    /// The room's state as `caller` sees it, or the refusal.
    async fn facts(
        &self,
        bridge: &Bridge,
        caller: &Caller,
        room_id: &str,
    ) -> Result<RoomFacts, MatrixError> {
        let events = bridge
            .client
            .room_state_events(&caller.token, &caller.user_id, room_id)
            .await
            .map_err(|e| match e.status {
                403 | 404 => {
                    MatrixError::forbidden("you are not in that room, or there is no such room")
                }
                _ => client_error("reading the room's state", &e),
            })?;
        Ok(RoomFacts::from_state(&events, &caller.user_id, &bridge.bot))
    }

    async fn record(
        &self,
        caller: &Caller,
        action: &str,
        room_id: &str,
        name: &str,
        status: u16,
    ) -> Result<(), MatrixError> {
        let entry = AuditEntry::new(
            action,
            Actor {
                kind: ActorKind::User,
                id: caller.user_id.clone(),
                display_name: None,
                token_id: None,
                ip: caller.ip.clone(),
                user_agent: caller.user_agent.clone(),
            },
            ResourceRef::new(
                "bridge_connection",
                format!("{BRIDGE_TYPE}/{room_id}/{name}"),
            ),
            AuditOutcome::success(status),
        );
        self.audit.append(entry).await.map_err(|e| {
            tracing::error!(error = %e, action, room = %room_id, name, "the audit log refused the user's webhook action");
            MatrixError::custom(
                StatusCode::SERVICE_UNAVAILABLE,
                MatrixErrorCode::Unknown,
                "the audit log is unavailable; the change was made but could not be recorded",
            )
        })
    }
}

async fn sources<B: KvBackend + 'static>(
    State(state): State<Arc<UserWebhooks<B>>>,
    request: Request,
) -> Response {
    let (mut parts, _) = request.into_parts();
    let _caller = match state.caller(&mut parts).await {
        Ok(c) => c,
        Err(refused) => return refused,
    };
    match sources_answer(&state).await {
        Ok(list) => Json(list).into_response(),
        Err(e) => e.into_response(),
    }
}

async fn sources_answer<B: KvBackend + 'static>(
    state: &UserWebhooks<B>,
) -> Result<SourceList, MatrixError> {
    state.bridge().await?;
    let items = state
        .manager
        .webhook_sources(BRIDGE_TYPE)
        .await
        .map_err(source_error)?;
    Ok(SourceList { items })
}

async fn rooms<B: KvBackend + 'static>(
    State(state): State<Arc<UserWebhooks<B>>>,
    request: Request,
) -> Response {
    let (mut parts, _) = request.into_parts();
    let caller = match state.caller(&mut parts).await {
        Ok(c) => c,
        Err(refused) => return refused,
    };
    match rooms_answer(&state, &caller).await {
        Ok(list) => Json(list).into_response(),
        Err(e) => e.into_response(),
    }
}

async fn rooms_answer<B: KvBackend + 'static>(
    state: &UserWebhooks<B>,
    caller: &Caller,
) -> Result<RoomList, MatrixError> {
    let bridge = state.bridge().await?;
    let joined = bridge
        .client
        .joined_rooms(&caller.token, &caller.user_id)
        .await
        .map_err(|e| client_error("listing your rooms", &e))?;
    let mut tasks = tokio::task::JoinSet::new();
    // A handful of rooms at a time: the reads are loopback calls into this server, and a
    // person with two hundred rooms is not a reason to make two hundred at once.
    let at_once = Arc::new(tokio::sync::Semaphore::new(ROOM_READS_AT_ONCE));
    for room_id in joined.into_iter().take(ROOM_LIMIT) {
        let client = bridge.client.clone();
        let token = caller.token.clone();
        let user = caller.user_id.clone();
        let bot = bridge.bot.clone();
        let at_once = at_once.clone();
        tasks.spawn(async move {
            let _slot = at_once.acquire().await.ok()?;
            let facts = match client.room_state_events(&token, &user, &room_id).await {
                Ok(events) => RoomFacts::from_state(&events, &user, &bot),
                Err(e) => {
                    tracing::debug!(room = %room_id, error = %e, "skipping a room whose state could not be read");
                    return None;
                }
            };
            let (may_add, reason) = match verdict(&facts, &user, &bot) {
                Verdict::Allowed { .. } => (true, None),
                Verdict::Refused(why) => (false, Some(why)),
            };
            Some(UserRoom {
                room_id,
                name: facts.name,
                bot_joined: facts.bot_membership.as_deref() == Some("join"),
                may_add,
                reason,
            })
        });
    }
    let mut rooms = Vec::new();
    while let Some(done) = tasks.join_next().await {
        if let Ok(Some(room)) = done {
            rooms.push(room);
        }
    }
    rooms.sort_by(|a, b| {
        b.may_add
            .cmp(&a.may_add)
            .then_with(|| a.name.is_none().cmp(&b.name.is_none()))
            .then_with(|| a.name.cmp(&b.name))
            .then_with(|| a.room_id.cmp(&b.room_id))
    });
    Ok(RoomList { rooms })
}

async fn list<B: KvBackend + 'static>(
    State(state): State<Arc<UserWebhooks<B>>>,
    request: Request,
) -> Response {
    let (mut parts, _) = request.into_parts();
    let caller = match state.caller(&mut parts).await {
        Ok(c) => c,
        Err(refused) => return refused,
    };
    match list_answer(&state, &caller).await {
        Ok(list) => Json(list).into_response(),
        Err(e) => e.into_response(),
    }
}

async fn list_answer<B: KvBackend + 'static>(
    state: &UserWebhooks<B>,
    caller: &Caller,
) -> Result<WebhookList, MatrixError> {
    let bridge = state.bridge().await?;
    let mine: std::collections::HashSet<String> = bridge
        .client
        .joined_rooms(&caller.token, &caller.user_id)
        .await
        .map_err(|e| client_error("listing your rooms", &e))?
        .into_iter()
        .collect();
    let all = state
        .manager
        .connections(BRIDGE_TYPE)
        .await
        .map_err(source_error)?;
    let items = all
        .items
        .into_iter()
        .filter(|c| c.service == "webhook" && c.state == "active" && mine.contains(&c.room_id))
        .map(|c| UserWebhook {
            created_by: c.content[CREATED_BY_MARKER].as_str().map(str::to_owned),
            created_at: c.content[CREATED_AT_MARKER].as_u64(),
            room_id: c.room_id,
            room_name: c.room_name,
            name: c.name,
            source: c.source,
        })
        .collect();
    Ok(WebhookList { items })
}

async fn create<B: KvBackend + 'static>(
    State(state): State<Arc<UserWebhooks<B>>>,
    request: Request,
) -> Response {
    let (mut parts, body) = request.into_parts();
    let caller = match state.caller(&mut parts).await {
        Ok(c) => c,
        Err(refused) => return refused,
    };
    let bytes = match axum::body::to_bytes(body, 64 * 1024).await {
        Ok(bytes) => bytes,
        Err(e) => return MatrixError::not_json(format!("reading the body: {e}")).into_response(),
    };
    match create_answer(&state, &caller, &bytes).await {
        Ok(created) => (StatusCode::CREATED, Json(created)).into_response(),
        Err(e) => e.into_response(),
    }
}

async fn create_answer<B: KvBackend + 'static>(
    state: &UserWebhooks<B>,
    caller: &Caller,
    body: &[u8],
) -> Result<BridgeConnectionCreated, MatrixError> {
    let request: WebhookCreate = if body.is_empty() {
        return Err(MatrixError::not_json("a JSON body is needed"));
    } else {
        serde_json::from_slice(body).map_err(|e| MatrixError::bad_json(e.to_string()))?
    };
    hs_admin::bridge_offerings::validate_connection_name(&request.name).map_err(source_error)?;
    if crate::hookshot_sources::get(&request.source).is_none() {
        return Err(MatrixError::custom(
            StatusCode::BAD_REQUEST,
            MatrixErrorCode::InvalidParam,
            format!(
                "`{}` is not a webhook source; one of {}",
                request.source,
                crate::hookshot_sources::SOURCES
                    .iter()
                    .map(|s| s.id)
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        ));
    }
    let bridge = state.bridge().await?;
    if let Err(wait) = state.limiter.take(&caller.user_id, Instant::now()) {
        tracing::info!(user = %caller.user_id, "a user's webhook creation was rate-limited");
        return Err(MatrixError::rate_limited(
            u64::try_from(wait.as_millis()).unwrap_or(u64::MAX),
        ));
    }
    let room_id = match (&request.room.id, &request.room.create) {
        (Some(room_id), None) => {
            prepare_room(&bridge, caller, room_id).await?;
            room_id.clone()
        }
        (None, Some(create)) => make_room(&bridge, caller, create).await?,
        _ => {
            return Err(MatrixError::custom(
                StatusCode::BAD_REQUEST,
                MatrixErrorCode::InvalidParam,
                "say which room: `room.id` of one you are in, or `room.create` with a name",
            ));
        }
    };
    let created = state
        .manager
        .create_connection_by(
            BRIDGE_TYPE,
            BridgeConnectionCreate {
                source: request.source.clone(),
                name: request.name.clone(),
                room: BridgeConnectionRoom {
                    id: Some(room_id.clone()),
                    create: None,
                },
            },
            Some(&caller.user_id),
        )
        .await
        .map_err(source_error)?;
    tracing::info!(
        user = %caller.user_id,
        room = %created.room_id,
        name = %created.name,
        source = %created.source,
        "a user made a webhook through the user API"
    );
    state
        .record(
            caller,
            "bridge_connection.created",
            &created.room_id,
            &created.name,
            201,
        )
        .await?;
    Ok(created)
}

/// Gets an existing room ready for the bot: the caller's right to set it up, the bot invited
/// and joined if it is not in it, and raised to the level the state event takes if it is below.
async fn prepare_room(bridge: &Bridge, caller: &Caller, room_id: &str) -> Result<(), MatrixError> {
    if !room_id.starts_with('!') {
        return Err(MatrixError::custom(
            StatusCode::BAD_REQUEST,
            MatrixErrorCode::InvalidParam,
            "`room.id` is a room id (!...:server)",
        ));
    }
    let events = bridge
        .client
        .room_state_events(&caller.token, &caller.user_id, room_id)
        .await
        .map_err(|e| match e.status {
            403 | 404 => {
                MatrixError::forbidden("you are not in that room, or there is no such room")
            }
            _ => client_error("reading the room's state", &e),
        })?;
    let facts = RoomFacts::from_state(&events, &caller.user_id, &bridge.bot);
    let (invite_bot, raise_bot_to) = match verdict(&facts, &caller.user_id, &bridge.bot) {
        Verdict::Allowed {
            invite_bot,
            raise_bot_to,
        } => (invite_bot, raise_bot_to),
        Verdict::Refused(why) => return Err(MatrixError::forbidden(why)),
    };
    if invite_bot {
        bridge
            .client
            .invite(&caller.token, &caller.user_id, room_id, &bridge.bot)
            .await
            .map_err(|e| match e.status {
                403 => MatrixError::forbidden(format!(
                    "you may not invite the bridge's bot {} to that room ({})",
                    bridge.bot, e.errcode
                )),
                _ => client_error("inviting the bot", &e),
            })?;
        bridge
            .client
            .join(&bridge.token, &bridge.bot, room_id)
            .await
            .map_err(|e| client_error("the bot joining the room", &e))?;
        tracing::info!(user = %caller.user_id, room = %room_id, bot = %bridge.bot, "the bot was invited to a room for a user's webhook and joined");
    }
    if let Some(level) = raise_bot_to {
        bridge
            .client
            .send_state(
                &caller.token,
                &caller.user_id,
                room_id,
                "m.room.power_levels",
                "",
                power_levels_with(&facts, &bridge.bot, level),
            )
            .await
            .map_err(|e| match e.status {
                403 => MatrixError::forbidden(format!(
                    "the bridge's bot {} needs power level {level} in that room and the room refused to grant it ({})",
                    bridge.bot, e.errcode
                )),
                _ => client_error("raising the bot's power level", &e),
            })?;
        tracing::info!(user = %caller.user_id, room = %room_id, bot = %bridge.bot, level, "the bot's power level was raised for a user's webhook");
    }
    Ok(())
}

/// Makes a room as the bot for the caller: private, named, with the caller invited, given
/// power 100 and joined as themselves. Returns its id.
async fn make_room(
    bridge: &Bridge,
    caller: &Caller,
    create: &RoomCreate,
) -> Result<String, MatrixError> {
    let name = create.name.trim();
    if name.is_empty() {
        return Err(MatrixError::custom(
            StatusCode::BAD_REQUEST,
            MatrixErrorCode::InvalidParam,
            "`room.create.name`: the room needs a name",
        ));
    }
    let room_id = bridge
        .client
        .create_room(
            &bridge.token,
            &bridge.bot,
            name,
            create.topic.as_deref(),
            std::slice::from_ref(&caller.user_id),
        )
        .await
        .map_err(|e| client_error("making the room", &e))?;
    // The caller is its admin alongside the bot: it is their room.
    let power_levels = bridge
        .client
        .state_event(
            &bridge.token,
            &bridge.bot,
            &room_id,
            "m.room.power_levels",
            "",
        )
        .await
        .map_err(|e| client_error("reading the new room's power levels", &e))?;
    let facts = RoomFacts {
        power_levels,
        ..RoomFacts::default()
    };
    bridge
        .client
        .send_state(
            &bridge.token,
            &bridge.bot,
            &room_id,
            "m.room.power_levels",
            "",
            power_levels_with(&facts, &caller.user_id, ROOM_MAKER_POWER),
        )
        .await
        .map_err(|e| client_error("making you the new room's admin", &e))?;
    bridge
        .client
        .join(&caller.token, &caller.user_id, &room_id)
        .await
        .map_err(|e| client_error("joining you to the new room", &e))?;
    tracing::info!(user = %caller.user_id, room = %room_id, "a room was made for a user's webhook and they joined it");
    Ok(room_id)
}

async fn remove<B: KvBackend + 'static>(
    State(state): State<Arc<UserWebhooks<B>>>,
    Path((room_id, name)): Path<(String, String)>,
    request: Request,
) -> Response {
    let (mut parts, _) = request.into_parts();
    let caller = match state.caller(&mut parts).await {
        Ok(c) => c,
        Err(refused) => return refused,
    };
    match remove_answer(&state, &caller, &room_id, &name).await {
        Ok(()) => Json(json!({})).into_response(),
        Err(e) => e.into_response(),
    }
}

async fn remove_answer<B: KvBackend + 'static>(
    state: &UserWebhooks<B>,
    caller: &Caller,
    room_id: &str,
    name: &str,
) -> Result<(), MatrixError> {
    let bridge = state.bridge().await?;
    let facts = state.facts(&bridge, caller, room_id).await?;
    if facts.user_membership.as_deref() != Some("join") {
        return Err(MatrixError::forbidden("you are not in that room"));
    }
    let required = facts.required_to_send_state(GENERIC_HOOK_TYPE);
    let power = facts.power_of(&caller.user_id);
    if power < required {
        return Err(MatrixError::forbidden(format!(
            "removing a webhook there takes power level {required} and you have {power}; ask a room admin"
        )));
    }
    if !facts.active_webhooks.iter().any(|k| k == name) {
        return Err(MatrixError::not_found(format!(
            "that room has no webhook named `{name}`"
        )));
    }
    state
        .manager
        .delete_connection_by(BRIDGE_TYPE, room_id, name, Some(&caller.user_id))
        .await
        .map_err(source_error)?;
    tracing::info!(user = %caller.user_id, room = %room_id, name, "a user removed a webhook through the user API");
    state
        .record(caller, "bridge_connection.deleted", room_id, name, 200)
        .await
}

// ---- errors ----------------------------------------------------------------------------------

/// A manager refusal as a Matrix error.
fn source_error(e: SourceError) -> MatrixError {
    match e {
        SourceError::NotFound => MatrixError::not_found("not found"),
        SourceError::Invalid(detail) => MatrixError::custom(
            StatusCode::BAD_REQUEST,
            MatrixErrorCode::InvalidParam,
            detail,
        ),
        SourceError::InvalidField { pointer, detail } => MatrixError::custom(
            StatusCode::BAD_REQUEST,
            MatrixErrorCode::InvalidParam,
            format!("{detail} (at {pointer})"),
        ),
        SourceError::Conflict(detail) => {
            MatrixError::custom(StatusCode::CONFLICT, MatrixErrorCode::Unknown, detail)
        }
        SourceError::PreconditionFailed(detail) => MatrixError::custom(
            StatusCode::PRECONDITION_FAILED,
            MatrixErrorCode::Unknown,
            detail,
        ),
        SourceError::Unavailable(detail) => MatrixError::custom(
            StatusCode::SERVICE_UNAVAILABLE,
            MatrixErrorCode::Unknown,
            detail,
        ),
    }
}

/// A failed loopback call on the caller's or the bot's behalf, as a Matrix error that says
/// what was being done.
fn client_error(context: &str, e: &ClientError) -> MatrixError {
    tracing::warn!(context, status = e.status, errcode = %e.errcode, "a user-webhook step failed on the client API");
    MatrixError::custom(
        StatusCode::BAD_GATEWAY,
        MatrixErrorCode::Unknown,
        format!(
            "{context}: the server answered {} {}",
            e.status,
            if e.errcode.is_empty() {
                "(no errcode)"
            } else {
                e.errcode.as_str()
            }
        ),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const USER: &str = "@alice:example.org";
    const BOT: &str = "@hookshot:example.org";

    fn event(kind: &str, key: &str, sender: &str, content: Value) -> Value {
        json!({"type": kind, "state_key": key, "sender": sender, "content": content})
    }

    fn joined(who: &str) -> Value {
        event("m.room.member", who, who, json!({"membership": "join"}))
    }

    /// A room alice made in an ordinary client: her at 100, state events at 50.
    fn alices_room(extra: Vec<Value>) -> Vec<Value> {
        let mut events = vec![
            event("m.room.create", "", USER, json!({"room_version": "10"})),
            joined(USER),
            event(
                "m.room.power_levels",
                "",
                USER,
                json!({"users": {USER: 100}, "users_default": 0, "state_default": 50, "invite": 0}),
            ),
            event("m.room.name", "", USER, json!({"name": "Deploys"})),
        ];
        events.extend(extra);
        events
    }

    #[test]
    fn the_rooms_creator_may_add_and_the_bot_is_invited_and_raised() {
        let facts = RoomFacts::from_state(&alices_room(vec![]), USER, BOT);
        assert_eq!(facts.name.as_deref(), Some("Deploys"));
        assert_eq!(facts.power_of(USER), 100);
        assert_eq!(facts.power_of(BOT), 0);
        assert_eq!(facts.required_to_send_state(GENERIC_HOOK_TYPE), 50);
        assert_eq!(
            verdict(&facts, USER, BOT),
            Verdict::Allowed {
                invite_bot: true,
                raise_bot_to: Some(50)
            }
        );
    }

    #[test]
    fn a_bot_already_in_with_power_needs_nothing() {
        let events = alices_room(vec![joined(BOT)]);
        let mut facts = RoomFacts::from_state(&events, USER, BOT);
        facts.power_levels = Some(json!({"users": {USER: 100, BOT: 50}, "state_default": 50}));
        assert_eq!(
            verdict(&facts, USER, BOT),
            Verdict::Allowed {
                invite_bot: false,
                raise_bot_to: None
            }
        );
    }

    #[test]
    fn a_plain_member_is_refused_with_the_level_it_takes() {
        let events = vec![
            event(
                "m.room.create",
                "",
                "@bob:example.org",
                json!({"room_version": "10"}),
            ),
            joined(USER),
            event(
                "m.room.power_levels",
                "",
                "@bob:example.org",
                json!({"users": {"@bob:example.org": 100}, "state_default": 50}),
            ),
        ];
        let facts = RoomFacts::from_state(&events, USER, BOT);
        match verdict(&facts, USER, BOT) {
            Verdict::Refused(why) => {
                assert!(why.contains("power level 50"), "{why}");
                assert!(why.contains("you have 0"), "{why}");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn the_event_type_s_own_level_wins_over_state_default() {
        let events = alices_room(vec![]);
        let mut facts = RoomFacts::from_state(&events, USER, BOT);
        facts.power_levels = Some(json!({
            "users": {USER: 60},
            "state_default": 50,
            "events": {GENERIC_HOOK_TYPE: 75}
        }));
        assert_eq!(facts.required_to_send_state(GENERIC_HOOK_TYPE), 75);
        assert!(matches!(verdict(&facts, USER, BOT), Verdict::Refused(_)));
        facts.power_levels = Some(json!({
            "users": {USER: 60},
            "state_default": 75,
            "events": {GENERIC_HOOK_TYPE: 25, "m.room.power_levels": 100}
        }));
        // May send the hook event (25) and the bot needs 25, but raising it takes 100.
        match verdict(&facts, USER, BOT) {
            Verdict::Refused(why) => assert!(why.contains("cannot grant"), "{why}"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn someone_not_in_the_room_is_refused() {
        let events = vec![
            event(
                "m.room.create",
                "",
                "@bob:example.org",
                json!({"room_version": "10"}),
            ),
            event("m.room.member", USER, USER, json!({"membership": "leave"})),
        ];
        let facts = RoomFacts::from_state(&events, USER, BOT);
        assert_eq!(
            verdict(&facts, USER, BOT),
            Verdict::Refused("you are not in that room".into())
        );
        let facts = RoomFacts::from_state(&[], USER, BOT);
        assert!(matches!(verdict(&facts, USER, BOT), Verdict::Refused(_)));
    }

    #[test]
    fn an_encrypted_room_is_refused() {
        let events = alices_room(vec![event(
            "m.room.encryption",
            "",
            USER,
            json!({"algorithm": "m.megolm.v1.aes-sha2"}),
        )]);
        let facts = RoomFacts::from_state(&events, USER, BOT);
        assert!(facts.encrypted);
        match verdict(&facts, USER, BOT) {
            Verdict::Refused(why) => assert!(why.contains("encrypted"), "{why}"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn inviting_the_bot_takes_the_invite_level() {
        let events = alices_room(vec![]);
        let mut facts = RoomFacts::from_state(&events, USER, BOT);
        facts.power_levels = Some(json!({"users": {USER: 50}, "state_default": 50, "invite": 75}));
        match verdict(&facts, USER, BOT) {
            Verdict::Refused(why) => assert!(why.contains("may not invite"), "{why}"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_version_12_creator_has_infinite_power_without_being_listed() {
        let events = vec![
            event(
                "m.room.create",
                "",
                USER,
                json!({"room_version": "12", "additional_creators": ["@carol:example.org"]}),
            ),
            joined(USER),
            joined("@carol:example.org"),
            event(
                "m.room.power_levels",
                "",
                USER,
                json!({"users": {}, "state_default": 50}),
            ),
        ];
        let facts = RoomFacts::from_state(&events, USER, BOT);
        assert!(facts.privileged_creators);
        assert_eq!(facts.power_of(USER), i64::MAX);
        assert_eq!(facts.power_of("@carol:example.org"), i64::MAX);
        assert_eq!(facts.power_of(BOT), 0);
        assert_eq!(
            verdict(&facts, USER, BOT),
            Verdict::Allowed {
                invite_bot: true,
                raise_bot_to: Some(50)
            }
        );
        // An older room's creator is in `users`, and is nothing special without it.
        let events = vec![
            event("m.room.create", "", USER, json!({"room_version": "10"})),
            joined(USER),
            event(
                "m.room.power_levels",
                "",
                USER,
                json!({"users": {}, "state_default": 50}),
            ),
        ];
        let facts = RoomFacts::from_state(&events, USER, BOT);
        assert_eq!(facts.power_of(USER), 0);
    }

    #[test]
    fn a_room_without_power_levels_lets_its_creator_do_everything() {
        let events = vec![
            event("m.room.create", "", USER, json!({"room_version": "10"})),
            joined(USER),
        ];
        let facts = RoomFacts::from_state(&events, USER, BOT);
        assert_eq!(facts.power_of(USER), 100);
        assert_eq!(facts.required_to_send_state(GENERIC_HOOK_TYPE), 0);
        assert_eq!(
            verdict(&facts, USER, BOT),
            Verdict::Allowed {
                invite_bot: true,
                raise_bot_to: None
            }
        );
    }

    #[test]
    fn active_webhooks_are_read_and_removed_ones_are_not() {
        let events = alices_room(vec![
            event(
                GENERIC_HOOK_TYPE,
                "deploys",
                BOT,
                json!({"name": "deploys"}),
            ),
            event(GENERIC_HOOK_TYPE, "old", BOT, json!({})),
            event(GENERIC_HOOK_TYPE, "older", BOT, json!({"disabled": true})),
        ]);
        let facts = RoomFacts::from_state(&events, USER, BOT);
        assert_eq!(facts.active_webhooks, vec!["deploys".to_owned()]);
    }

    #[test]
    fn power_levels_with_adds_the_bot_and_keeps_the_rest() {
        let facts = RoomFacts {
            power_levels: Some(json!({"users": {USER: 100}, "state_default": 50, "ban": 50})),
            ..RoomFacts::default()
        };
        let pl = power_levels_with(&facts, BOT, 50);
        assert_eq!(pl["users"][USER], 100);
        assert_eq!(pl["users"][BOT], 50);
        assert_eq!(pl["ban"], 50);
        let pl = power_levels_with(&RoomFacts::default(), BOT, 50);
        assert_eq!(pl, json!({"users": {BOT: 50}}));
    }

    #[test]
    fn the_limiter_allows_the_burst_then_refuses_then_refills() {
        let limiter = CreateLimiter::new(3, Duration::from_secs(60));
        let start = Instant::now();
        for _ in 0..3 {
            assert!(limiter.take(USER, start).is_ok());
        }
        let wait = limiter.take(USER, start).unwrap_err();
        assert!(
            wait > Duration::from_secs(59) && wait <= Duration::from_secs(60),
            "{wait:?}"
        );
        // Another user has their own bucket.
        assert!(limiter.take("@bob:example.org", start).is_ok());
        // A minute later one more is allowed, and only one.
        let later = start + Duration::from_secs(61);
        assert!(limiter.take(USER, later).is_ok());
        assert!(limiter.take(USER, later).is_err());
    }

    #[test]
    fn a_manager_refusal_becomes_a_matrix_error() {
        let e = source_error(SourceError::InvalidField {
            pointer: "/name",
            detail: "the name is 3 to 64 characters".into(),
        });
        assert_eq!(e.status, StatusCode::BAD_REQUEST);
        assert_eq!(e.errcode, MatrixErrorCode::InvalidParam);
        assert!(e.error.contains("/name"));
        let e = source_error(SourceError::Conflict("in use".into()));
        assert_eq!(e.status, StatusCode::CONFLICT);
        assert_eq!(
            source_error(SourceError::NotFound).status,
            StatusCode::NOT_FOUND
        );
    }
}
