//! The rest of the Federation area (RFC 0004 section 4, "Federation"): the rooms this server
//! shares with a destination (`federation.destinations.rooms`), forgetting destinations
//! (`federation.destinations.forget`, `.prune`) and the signing keys (`federation.keys.list`,
//! `.get`, `.refresh`). The three `federation.destinations.*` operations that read the
//! destination records themselves live in `crate::router`.
//!
//! # Forgetting destinations
//!
//! A destination this server shares no room with is state, not a relationship (decision 0042):
//! nothing will be sent to it until a room brings the two together again, and then it is
//! learned from nothing. `federation.destinations.forget` (`DELETE
//! /federation/destinations/{server_name}`) drops one -- its queue, backoff, catch-up mark and
//! cached keys -- and is refused with `409 conflict` while this server still shares a room with
//! it, naming how many, unless `force=true`. `federation.destinations.prune` (`POST
//! /federation/destinations/prune`) forgets every destination the rules in [`decide`] say to:
//! one sharing no room with nothing durable queued for it, and, with `failing_for`, one failing
//! for at least that long whose queued events are only for rooms this server has since left.
//! `dry_run=true` answers the same report without forgetting anything. Both are audited and
//! published (`federation.destination_forgotten`, `federation.destinations_pruned`). The
//! [`FederationSource`] gathers the [`DestinationFacts`]; the rules live here so that the real
//! source (`hs-federation`'s) and the in-memory one decide alike.
//!
//! # Shared rooms
//!
//! Composed here from the room directory ([`crate::sources::RoomDirectory`]): every room this
//! server knows in which at least one member of the destination is joined, with the room's own
//! joined count and how many of those are the destination's. It reads each room's members, so
//! it costs one member list per room; an administrator's page, not a hot path.
//!
//! # Keys
//!
//! `federation.keys.list` is this server's own signing keys, the ones `/_matrix/key/v2/server`
//! publishes. `federation.keys.get` is what the key cache holds for another server: the keys
//! its signatures are checked against, current and old, and when they were fetched. Both read
//! the [`FederationSource`]. `federation.keys.refresh` is a Task
//! (`federation.refetch_keys`, resource `{type: destination, id}`): it fetches the server's keys
//! again, whatever is cached, and ends `succeeded` with what the cache then holds as its
//! `result`, or `failed` when the server could not be reached or answered something that does
//! not verify. The request is audited (`federation.keys.refresh`) and published
//! (`federation.keys_refresh_started`); a success is published as `federation.keys_refreshed`.
//! A state with no task registry runs the fetch inside the request and answers the task
//! finished.

use std::collections::BTreeMap;
use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use hs_http::Problem;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::handler_kit::{authorize, check_replay, record, respond_and_remember, unwired};
use crate::model::{Actor, Event, Page, ResourceRef, Scope, Task, TaskStatus};
use crate::router::AdminState;
use crate::sources::{FederationSource, RoomFilter, SourceError};

/// The OpenAPI `ServerSigningKey` schema: one signing key, this server's or another's.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdminSigningKey {
    /// `ed25519:<version>`.
    pub key_id: String,
    /// `ed25519`.
    pub algorithm: String,
    /// Base64 (standard alphabet, unpadded), as published.
    pub public_key: String,
    /// Until when it may be used: a cached current key's `valid_until_ts`, an old key's
    /// `expired_ts`. `None` for this server's own keys, which are valid until rotated.
    pub valid_until_at: Option<String>,
    /// Whether it is an old key (`old_verify_keys`), usable only for what was signed before
    /// `valid_until_at`.
    pub old: bool,
}

/// The OpenAPI `RemoteServerKeys` schema: what the key cache holds for one server.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdminRemoteServerKeys {
    pub server_name: String,
    /// Current keys first, then old ones, each by key id.
    pub keys: Vec<AdminSigningKey>,
    /// When a key response from (or about) the server was last accepted; `None` when the
    /// cache has keys but no record of when (never, in practice).
    pub cached_at: Option<String>,
}

/// The OpenAPI `DestinationRoom` schema: one room this server shares with a destination.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdminDestinationRoom {
    pub room_id: String,
    pub name: Option<String>,
    pub canonical_alias: Option<String>,
    /// Everyone joined to the room, from every server.
    pub joined_members_count: u64,
    /// How many of them are the destination's users.
    pub destination_members_count: u64,
}

/// The task action `federation.keys.refresh` starts.
pub const REFRESH_TASK_ACTION: &str = "federation.refetch_keys";

/// The OpenAPI `DestinationForgotten` schema: what forgetting a destination dropped.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdminDestinationForgotten {
    pub server_name: String,
    /// Room events that were queued for it and are now gone, unsent.
    pub dropped_pdu_count: u64,
    /// To-device messages and device-list updates that were queued for it and are now gone.
    pub dropped_edu_count: u64,
    /// Signing keys of it this server held and no longer does.
    pub dropped_key_count: u64,
    /// Whether it was in catch-up mode.
    pub was_catching_up: bool,
    /// How many rooms this server shared with it when it was forgotten (non-zero only with
    /// `force=true`).
    pub shared_rooms_count: u64,
}

/// How `federation.destinations.forget` ended at the source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ForgetOutcome {
    /// Dropped.
    Forgotten(AdminDestinationForgotten),
    /// Refused: this server still shares `rooms` rooms with it, and `force` was not given.
    SharesRooms { rooms: u64 },
}

/// What `federation.destinations.prune` was asked, parsed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PruneOptions {
    /// Report only; nothing is forgotten.
    pub dry_run: bool,
    /// The second rule of [`decide`]: forget a destination failing for at least this long whose
    /// queued events are only for rooms this server has left. `None` leaves those alone.
    pub failing_for: Option<std::time::Duration>,
}

/// The OpenAPI `DestinationPruneReport` schema.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdminPruneReport {
    /// Whether this was a dry run (nothing forgotten).
    pub dry_run: bool,
    /// The destinations forgotten (or, in a dry run, that would be).
    pub forgotten: AdminPruneGroup,
    /// The destinations kept, and why.
    pub kept: AdminPruneGroup,
}

/// The OpenAPI `DestinationPruneGroup` schema: one side of a prune report.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdminPruneGroup {
    /// How many in all.
    pub count: u64,
    /// How many for each reason ([`PruneReason`]'s names).
    pub by_reason: BTreeMap<String, u64>,
    /// The first [`PRUNE_REPORT_NAMES`] of them, by name.
    pub servers: Vec<AdminPruneEntry>,
}

/// The OpenAPI `DestinationPruneEntry` schema: one destination in a prune report.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdminPruneEntry {
    pub server_name: String,
    /// A [`PruneReason`] name.
    pub reason: String,
    /// One sentence: the numbers behind the reason.
    pub detail: String,
}

/// How many names each side of a prune report carries; the counts are the whole list's.
pub const PRUNE_REPORT_NAMES: usize = 50;

/// How many rooms a destination's queue is inspected for when deciding whether what is queued
/// is only for rooms this server has left.
pub const PRUNE_ROOMS_INSPECTED: usize = 1000;

/// What [`decide`] looks at for one destination. The source gathers it from its stores; the
/// in-memory source from its rows.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DestinationFacts {
    pub server_name: String,
    /// Rooms where both this server and the destination have a joined member.
    pub shared_rooms: u64,
    /// Room events durably queued for it.
    pub queued_pdus: u64,
    /// To-device messages and device-list updates durably queued for it.
    pub queued_edus: u64,
    /// Whether it is in catch-up mode (its queue overflowed; it is owed the latest event of
    /// each room it is behind in).
    pub catching_up: bool,
    /// Rooms it is behind in that this server still has a joined member in.
    pub rooms_behind_current: u64,
    /// Rooms it is behind in that this server has since left.
    pub rooms_behind_left: u64,
    /// When its current run of failures began, ms since the epoch; `None` while not failing.
    pub failing_since_ms: Option<u64>,
    /// When it was last tried, ms since the epoch.
    pub last_attempt_ms: Option<u64>,
    /// When it last accepted something, ms since the epoch.
    pub last_success_ms: Option<u64>,
}

/// The rules [`decide`] applies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PruneRules {
    /// A destination sharing no room with nothing durable queued is forgotten once nothing has
    /// happened with it (no attempt, no success, no failure) for this long; zero means at once.
    /// The administrator's prune uses zero; the background sweep its retention setting.
    pub idle_for: std::time::Duration,
    /// A destination failing for at least this long whose queued events are only for rooms this
    /// server has left is forgotten, queue and all. `None` keeps every destination with a queue.
    pub failing_for: Option<std::time::Duration>,
}

/// Why a destination is forgotten or kept. Serialized in `snake_case` as the report's `reason`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PruneReason {
    /// Forgotten: shares no room, nothing queued, idle long enough.
    Unused,
    /// Forgotten: failing long enough, and what is queued is only for rooms this server left.
    Failing,
    /// Kept: this server shares a room with it.
    SharesRooms,
    /// Kept: something is queued for a room this server is still in.
    QueuedForCurrentRooms,
    /// Kept: something is queued (only for rooms this server left) and it is not failing, so
    /// it will be delivered.
    QueuedNotFailing,
    /// Kept: something is queued (only for rooms this server left) and it has not been failing
    /// for long enough yet.
    FailingRecently,
    /// Kept: shares no room and nothing is queued, but it was active too recently.
    ActiveRecently,
}

impl PruneReason {
    /// The `snake_case` name the report carries.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            PruneReason::Unused => "unused",
            PruneReason::Failing => "failing",
            PruneReason::SharesRooms => "shares_rooms",
            PruneReason::QueuedForCurrentRooms => "queued_for_current_rooms",
            PruneReason::QueuedNotFailing => "queued_not_failing",
            PruneReason::FailingRecently => "failing_recently",
            PruneReason::ActiveRecently => "active_recently",
        }
    }

    /// Whether the destination is forgotten for this reason.
    #[must_use]
    pub fn forgets(self) -> bool {
        matches!(self, PruneReason::Unused | PruneReason::Failing)
    }
}

/// What [`decide`] says about one destination.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PruneDecision {
    pub server_name: String,
    pub reason: PruneReason,
    pub detail: String,
}

impl PruneDecision {
    /// Whether the destination is to be forgotten.
    #[must_use]
    pub fn forget(&self) -> bool {
        self.reason.forgets()
    }

    /// The report row.
    #[must_use]
    pub fn entry(&self) -> AdminPruneEntry {
        AdminPruneEntry {
            server_name: self.server_name.clone(),
            reason: self.reason.name().to_owned(),
            detail: self.detail.clone(),
        }
    }
}

fn plural(n: u64, one: &str, many: &str) -> String {
    if n == 1 {
        format!("{n} {one}")
    } else {
        format!("{n} {many}")
    }
}

fn age(now_ms: u64, then_ms: u64) -> std::time::Duration {
    std::time::Duration::from_millis(now_ms.saturating_sub(then_ms))
}

/// The prune rules (decision 0042), applied to one destination at `now_ms`:
///
/// 1. A destination this server shares a room with is kept, whatever else (`shares_rooms`).
/// 2. One with something queued for a room this server is still in is kept
///    (`queued_for_current_rooms`).
/// 3. One with nothing queued (no PDU, no durable EDU, and not catching up in any room) is
///    forgotten once idle for `rules.idle_for` (`unused`), else kept (`active_recently`).
/// 4. One whose queue is only for rooms this server has left is forgotten when it has been
///    failing for `rules.failing_for` (`failing`); kept while it is not failing
///    (`queued_not_failing`: what is queued will be delivered) or has not failed for long
///    enough (`failing_recently`).
#[must_use]
pub fn decide(facts: &DestinationFacts, rules: &PruneRules, now_ms: u64) -> PruneDecision {
    let decision = |reason, detail| PruneDecision {
        server_name: facts.server_name.clone(),
        reason,
        detail,
    };
    if facts.shared_rooms > 0 {
        return decision(
            PruneReason::SharesRooms,
            format!(
                "shares {} with this server",
                plural(facts.shared_rooms, "room", "rooms")
            ),
        );
    }
    if facts.rooms_behind_current > 0 {
        return decision(
            PruneReason::QueuedForCurrentRooms,
            format!(
                "has events waiting for {} this server is still in",
                plural(facts.rooms_behind_current, "room", "rooms")
            ),
        );
    }
    let queued = facts.queued_pdus + facts.queued_edus;
    let behind = facts.catching_up && facts.rooms_behind_left > 0;
    if queued == 0 && !behind {
        let last_active = [
            facts.last_attempt_ms,
            facts.last_success_ms,
            facts.failing_since_ms,
        ]
        .into_iter()
        .flatten()
        .max();
        return match last_active {
            Some(at) if age(now_ms, at) < rules.idle_for => decision(
                PruneReason::ActiveRecently,
                format!(
                    "shares no room and has nothing queued, but was active {} ago",
                    humanize(age(now_ms, at))
                ),
            ),
            Some(at) => decision(
                PruneReason::Unused,
                format!(
                    "shares no room, has nothing queued, last active {} ago",
                    humanize(age(now_ms, at))
                ),
            ),
            None => decision(
                PruneReason::Unused,
                "shares no room, has nothing queued, never tried".to_owned(),
            ),
        };
    }
    let what = if behind && queued == 0 {
        format!(
            "is owed the latest event of {} this server has left",
            plural(facts.rooms_behind_left, "room", "rooms")
        )
    } else {
        format!(
            "has {} queued, only for {} this server has left",
            plural(queued, "event", "events"),
            plural(facts.rooms_behind_left.max(1), "room", "rooms")
        )
    };
    match (facts.failing_since_ms, rules.failing_for) {
        (None, _) => decision(
            PruneReason::QueuedNotFailing,
            format!("{what}; it is not failing, so they will be delivered"),
        ),
        (Some(since), Some(for_at_least)) if age(now_ms, since) >= for_at_least => decision(
            PruneReason::Failing,
            format!("{what}; failing for {}", humanize(age(now_ms, since))),
        ),
        (Some(since), Some(_)) => decision(
            PruneReason::FailingRecently,
            format!(
                "{what}; failing for {}, not long enough",
                humanize(age(now_ms, since))
            ),
        ),
        (Some(since), None) => decision(
            PruneReason::FailingRecently,
            format!(
                "{what}; failing for {}, kept because no failing_for was given",
                humanize(age(now_ms, since))
            ),
        ),
    }
}

/// `90s` as "1 minute", `3d 4h` as "3 days": a duration as a person says it, roughly.
fn humanize(d: std::time::Duration) -> String {
    let secs = d.as_secs();
    if secs < 60 {
        plural(secs, "second", "seconds")
    } else if secs < 3600 {
        plural(secs / 60, "minute", "minutes")
    } else if secs < 86_400 {
        plural(secs / 3600, "hour", "hours")
    } else {
        plural(secs / 86_400, "day", "days")
    }
}

/// Folds decisions into a report: counts by reason for both sides, the first
/// [`PRUNE_REPORT_NAMES`] names of each.
#[must_use]
pub fn report(decisions: &[PruneDecision], dry_run: bool) -> AdminPruneReport {
    let mut out = AdminPruneReport {
        dry_run,
        ..AdminPruneReport::default()
    };
    for decision in decisions {
        let group = if decision.forget() {
            &mut out.forgotten
        } else {
            &mut out.kept
        };
        group.count += 1;
        *group
            .by_reason
            .entry(decision.reason.name().to_owned())
            .or_default() += 1;
        if group.servers.len() < PRUNE_REPORT_NAMES {
            group.servers.push(decision.entry());
        }
    }
    out
}

/// `"7d"`, `"12h"`, `"0s"`: a `failing_for` value, in `hs-config`'s duration syntax.
///
/// # Errors
/// The parse error's text, for a `400` naming the field.
pub fn parse_failing_for(text: &str) -> Result<std::time::Duration, String> {
    text.parse::<hs_config::Duration>()
        .map(std::time::Duration::from)
        .map_err(|e| e.to_string())
}

/// The server part of a user id (`@alice:example.org:8448` → `example.org:8448`).
fn server_of(user_id: &str) -> Option<&str> {
    user_id.split_once(':').map(|(_, server)| server)
}

// -------------------------------------------------------------------------------------------
// Handlers
// -------------------------------------------------------------------------------------------

/// `GET /federation/destinations/{server_name}/rooms`'s query string.
#[derive(Debug, Deserialize)]
pub(crate) struct DestinationRoomsQuery {
    limit: Option<usize>,
    cursor: Option<String>,
    include_total: Option<bool>,
}

/// `GET /api/v1/federation/destinations/{server_name}/rooms` (`admin:read`): the rooms this
/// server shares with the destination, most of its members first. `404` when this server has
/// never tried to reach it and shares no room with it.
pub(crate) async fn destination_rooms(
    State(state): State<AdminState>,
    headers: HeaderMap,
    Path(server_name): Path<String>,
    Query(query): Query<DestinationRoomsQuery>,
) -> Response {
    let instance = format!("/api/v1/federation/destinations/{server_name}/rooms");
    if let Err(response) = authorize(&state, &headers, Scope::AdminRead, &instance).await {
        return response;
    }
    let Some(rooms) = &state.rooms else {
        return unwired("room directory", &instance);
    };
    let all = match rooms.list_rooms(&RoomFilter::default()).await {
        Ok(all) => all,
        Err(e) => return e.to_problem().with_instance(instance).into_response(),
    };
    let mut shared = Vec::new();
    for room in all {
        let members = match rooms.list_members(&room.room_id).await {
            Ok(members) => members,
            // Gone between the listing and now.
            Err(SourceError::NotFound) => continue,
            Err(e) => return e.to_problem().with_instance(instance).into_response(),
        };
        let theirs = members
            .iter()
            .filter(|m| m.membership == "join" && server_of(&m.user_id) == Some(&server_name))
            .count() as u64;
        if theirs > 0 {
            shared.push(AdminDestinationRoom {
                room_id: room.room_id,
                name: room.name,
                canonical_alias: room.canonical_alias,
                joined_members_count: room.joined_members_count,
                destination_members_count: theirs,
            });
        }
    }
    if shared.is_empty() {
        let known = match &state.federation {
            Some(federation) => match federation.get_destination(&server_name).await {
                Ok(destination) => destination.is_some(),
                Err(e) => return e.to_problem().with_instance(instance).into_response(),
            },
            None => false,
        };
        if !known {
            return Problem::not_found()
                .with_detail(format!(
                    "this server has never tried to reach {server_name} and shares no room with it"
                ))
                .with_instance(instance)
                .into_response();
        }
    }
    shared.sort_by(|a, b| {
        b.destination_members_count
            .cmp(&a.destination_members_count)
            .then_with(|| a.room_id.cmp(&b.room_id))
    });
    axum::Json(Page::paginate(
        shared,
        query.cursor.as_deref(),
        query.limit,
        query.include_total.unwrap_or(false),
    ))
    .into_response()
}

/// `DELETE /federation/destinations/{server_name}`'s query string.
#[derive(Debug, Deserialize)]
pub(crate) struct ForgetQuery {
    force: Option<bool>,
}

/// `DELETE /api/v1/federation/destinations/{server_name}` (`admin:write`): forgets the
/// destination (see the module docs). `404` for one never tried; `409 conflict` while this
/// server shares a room with it, unless `force=true`.
pub(crate) async fn destination_forget(
    State(state): State<AdminState>,
    headers: HeaderMap,
    Path(server_name): Path<String>,
    Query(query): Query<ForgetQuery>,
) -> Response {
    let instance = format!("/api/v1/federation/destinations/{server_name}");
    let operation_id = "federation.destinations.forget";
    let principal = match authorize(&state, &headers, Scope::AdminWrite, &instance).await {
        Ok(principal) => principal,
        Err(response) => return response,
    };
    let Some(federation) = state.federation.clone() else {
        return unwired("federation", &instance);
    };
    let force = query.force.unwrap_or(false);
    let fingerprint = format!("{server_name}?force={force}").into_bytes();
    if let Err(response) = check_replay(&state, &headers, operation_id, &fingerprint, &instance) {
        return response;
    }
    let forgotten = match federation.forget_destination(&server_name, force).await {
        Ok(ForgetOutcome::Forgotten(forgotten)) => forgotten,
        Ok(ForgetOutcome::SharesRooms { rooms }) => {
            return Problem::conflict()
                .with_detail(format!(
                    "this server still shares {} with {server_name}; its users are in them, so \
                     it is a relationship, not just state. Forget it anyway with force=true: \
                     what is queued for it is dropped and it is learned again from nothing",
                    plural(rooms, "room", "rooms")
                ))
                .with_instance(instance)
                .into_response();
        }
        Err(SourceError::NotFound) => {
            return Problem::not_found()
                .with_detail(format!(
                    "this server has never tried to reach {server_name}"
                ))
                .with_instance(instance)
                .into_response();
        }
        Err(e) => return e.to_problem().with_instance(instance).into_response(),
    };
    tracing::info!(
        server = %server_name,
        force,
        dropped_pdus = forgotten.dropped_pdu_count,
        dropped_edus = forgotten.dropped_edu_count,
        dropped_keys = forgotten.dropped_key_count,
        shared_rooms = forgotten.shared_rooms_count,
        "an administrator forgot a federation destination"
    );
    if let Err(response) = record(
        &state,
        &principal,
        operation_id,
        "federation.destination_forgotten",
        ResourceRef::new("destination", server_name.clone()),
        Vec::new(),
        serde_json::to_value(&forgotten).unwrap_or_default(),
        StatusCode::OK.as_u16(),
    )
    .await
    {
        return response;
    }
    respond_and_remember(
        &state,
        &headers,
        operation_id,
        &fingerprint,
        StatusCode::OK,
        &forgotten,
        &[],
    )
}

/// `POST /federation/destinations/prune`'s query string.
#[derive(Debug, Deserialize)]
pub(crate) struct PruneQuery {
    dry_run: Option<bool>,
}

/// `POST /federation/destinations/prune`'s body (the OpenAPI `DestinationPruneRequest`).
#[derive(Debug, Default, Deserialize)]
pub(crate) struct PruneBody {
    failing_for: Option<String>,
}

/// `POST /api/v1/federation/destinations/prune` (`admin:write`): forgets every destination the
/// rules say to (see the module docs and [`decide`]); `?dry_run=true` only reports. The body
/// is optional: `{"failing_for": "7d"}` turns the second rule on.
pub(crate) async fn destinations_prune(
    State(state): State<AdminState>,
    headers: HeaderMap,
    Query(query): Query<PruneQuery>,
    body: axum::body::Bytes,
) -> Response {
    let instance = "/api/v1/federation/destinations/prune";
    let operation_id = "federation.destinations.prune";
    let principal = match authorize(&state, &headers, Scope::AdminWrite, instance).await {
        Ok(principal) => principal,
        Err(response) => return response,
    };
    let Some(federation) = state.federation.clone() else {
        return unwired("federation", instance);
    };
    let parsed: PruneBody = if body.iter().all(u8::is_ascii_whitespace) {
        PruneBody::default()
    } else {
        match serde_json::from_slice(&body) {
            Ok(parsed) => parsed,
            Err(e) => {
                return Problem::validation_failed()
                    .with_detail(format!("the body is not a prune request: {e}"))
                    .with_instance(instance)
                    .into_response();
            }
        }
    };
    let failing_for = match parsed.failing_for.as_deref().map(str::trim) {
        None | Some("") => None,
        Some(text) => match parse_failing_for(text) {
            Ok(duration) => Some(duration),
            Err(reason) => {
                return Problem::validation_failed()
                    .with_detail(format!("failing_for: {reason}"))
                    .with_errors(vec![hs_http::ValidationError::new("/failing_for", reason)])
                    .with_instance(instance)
                    .into_response();
            }
        },
    };
    let dry_run = query.dry_run.unwrap_or(false);
    let fingerprint = [format!("dry_run={dry_run}&").as_bytes(), &body].concat();
    if !dry_run
        && let Err(response) = check_replay(&state, &headers, operation_id, &fingerprint, instance)
    {
        return response;
    }
    let options = PruneOptions {
        dry_run,
        failing_for,
    };
    let report = match federation.prune_destinations(options).await {
        Ok(report) => report,
        Err(e) => return e.to_problem().with_instance(instance).into_response(),
    };
    if dry_run {
        return axum::Json(report).into_response();
    }
    tracing::info!(
        forgotten = report.forgotten.count,
        kept = report.kept.count,
        failing_for_secs = failing_for.map(|d| d.as_secs()),
        "an administrator pruned the federation destinations"
    );
    if let Err(response) = record(
        &state,
        &principal,
        operation_id,
        "federation.destinations_pruned",
        ResourceRef::new("destination", "*"),
        Vec::new(),
        json!({
            "forgotten_count": report.forgotten.count,
            "kept_count": report.kept.count,
            "forgotten": report.forgotten.servers.iter().map(|e| e.server_name.clone()).collect::<Vec<_>>(),
            "failing_for_secs": failing_for.map(|d| d.as_secs()),
        }),
        StatusCode::OK.as_u16(),
    )
    .await
    {
        return response;
    }
    respond_and_remember(
        &state,
        &headers,
        operation_id,
        &fingerprint,
        StatusCode::OK,
        &report,
        &[],
    )
}

/// `GET /api/v1/federation/keys` (`admin:read`): this server's own signing keys.
pub(crate) async fn keys_list(State(state): State<AdminState>, headers: HeaderMap) -> Response {
    let instance = "/api/v1/federation/keys";
    if let Err(response) = authorize(&state, &headers, Scope::AdminRead, instance).await {
        return response;
    }
    let Some(federation) = &state.federation else {
        return unwired("federation", instance);
    };
    match federation.own_keys().await {
        Ok(keys) => axum::Json(keys).into_response(),
        Err(e) => e.to_problem().with_instance(instance).into_response(),
    }
}

/// `GET /api/v1/federation/keys/{server_name}` (`admin:read`): what the key cache holds for
/// `server_name`; `404` when it holds nothing.
pub(crate) async fn keys_get(
    State(state): State<AdminState>,
    headers: HeaderMap,
    Path(server_name): Path<String>,
) -> Response {
    let instance = format!("/api/v1/federation/keys/{server_name}");
    if let Err(response) = authorize(&state, &headers, Scope::AdminRead, &instance).await {
        return response;
    }
    let Some(federation) = &state.federation else {
        return unwired("federation", &instance);
    };
    match federation.remote_keys(&server_name).await {
        Ok(Some(keys)) => axum::Json(keys).into_response(),
        Ok(None) => Problem::not_found()
            .with_detail(format!(
                "this server holds no keys for {server_name}: it has not needed to check one of \
                 its signatures since it started"
            ))
            .with_instance(instance)
            .into_response(),
        Err(e) => e.to_problem().with_instance(instance).into_response(),
    }
}

/// `POST /api/v1/federation/keys/{server_name}/refresh` (`admin:write`, Task): fetches
/// `server_name`'s keys again. See the module docs.
pub(crate) async fn keys_refresh(
    State(state): State<AdminState>,
    headers: HeaderMap,
    Path(server_name): Path<String>,
    body: axum::body::Bytes,
) -> Response {
    let instance = format!("/api/v1/federation/keys/{server_name}/refresh");
    let operation_id = "federation.keys.refresh";
    let principal = match authorize(&state, &headers, Scope::AdminWrite, &instance).await {
        Ok(principal) => principal,
        Err(response) => return response,
    };
    let Some(federation) = state.federation.clone() else {
        return unwired("federation", &instance);
    };
    if server_name.is_empty() || server_name.contains(['/', '@', ' ']) {
        return Problem::not_found()
            .with_detail(format!("{server_name:?} is not a server name"))
            .with_instance(instance)
            .into_response();
    }
    // The body is empty; the idempotency key covers the server named in the path.
    let fingerprint = [server_name.as_bytes(), &body].concat();
    if let Err(response) = check_replay(&state, &headers, operation_id, &fingerprint, &instance) {
        return response;
    }
    let actor = principal.to_actor();
    let resource = ResourceRef::new("destination", server_name.clone());
    let refresh = Refresh {
        federation,
        server_name: server_name.clone(),
        events: Arc::clone(&state.events),
        actor: actor.clone(),
    };
    let task = match &state.tasks {
        Some(tasks) => match tasks
            .spawn(
                REFRESH_TASK_ACTION,
                Some(resource.clone()),
                actor,
                move |_context| refresh.run(),
            )
            .await
        {
            Ok(task) => task,
            Err(e) => return e.to_problem().with_instance(instance).into_response(),
        },
        None => refresh.run_inline(resource.clone()).await,
    };
    tracing::info!(task = %task.id, server = %server_name, "an administrator asked to refetch a server's signing keys");
    if let Err(response) = record(
        &state,
        &principal,
        operation_id,
        "federation.keys_refresh_started",
        resource,
        Vec::new(),
        json!({ "server_name": server_name, "task_id": task.id }),
        StatusCode::ACCEPTED.as_u16(),
    )
    .await
    {
        return response;
    }
    respond_and_remember(
        &state,
        &headers,
        operation_id,
        &fingerprint,
        StatusCode::ACCEPTED,
        &task,
        &[("location", format!("/api/v1/tasks/{}", task.id))],
    )
}

/// One refetch of a server's keys: what the task runs.
struct Refresh {
    federation: Arc<dyn FederationSource>,
    server_name: String,
    events: Arc<crate::events::EventBus>,
    actor: Actor,
}

impl Refresh {
    // The shape `TaskRegistry::spawn` runs: its problem becomes the task's `error`.
    #[allow(clippy::result_large_err)]
    async fn run(self) -> Result<serde_json::Value, Problem> {
        match self.federation.refresh_remote_keys(&self.server_name).await {
            Ok(keys) => {
                tracing::info!(
                    server = %self.server_name,
                    keys = keys.keys.len(),
                    "refetched a server's signing keys"
                );
                let value = serde_json::to_value(&keys).unwrap_or_default();
                self.events.publish(
                    Event::new("federation.keys_refreshed", value.clone())
                        .with_resource(ResourceRef::new("destination", self.server_name.clone()))
                        .with_actor(self.actor),
                );
                Ok(value)
            }
            Err(error) => {
                tracing::warn!(server = %self.server_name, %error, "could not refetch a server's signing keys");
                Err(error.to_problem())
            }
        }
    }

    /// [`Refresh::run`] inside the request, for a state with no task registry.
    async fn run_inline(self, resource: ResourceRef) -> Task {
        let mut task = Task::scheduled(REFRESH_TASK_ACTION, Some(resource), self.actor.clone());
        task.started_at = Some(hs_http::time::now_rfc3339());
        match self.run().await {
            Ok(result) => {
                task.status = TaskStatus::Succeeded;
                task.result = Some(result);
            }
            Err(problem) => {
                task.status = TaskStatus::Failed;
                task.error = Some(problem);
            }
        }
        task.finished_at = Some(hs_http::time::now_rfc3339());
        task
    }
}

/// What an in-memory [`FederationSource`] (`crate::sources::InMemoryFederationSource`) serves
/// for keys: set by tests and `hs-admin-mock`.
#[derive(Debug, Clone, Default)]
pub struct InMemoryKeys {
    /// This server's own keys.
    pub own: Vec<AdminSigningKey>,
    /// The cache, by server.
    pub cached: BTreeMap<String, AdminRemoteServerKeys>,
    /// What a refresh finds, by server; a server missing here cannot be reached.
    pub reachable: BTreeMap<String, AdminRemoteServerKeys>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handler_kit::testing::{call, state};
    use crate::model::{AdminDestination, AdminRoom, AdminRoomMember};
    use crate::sources::{InMemoryFederationSource, InMemoryRoomDirectory};

    fn key(id: &str, old: bool) -> AdminSigningKey {
        AdminSigningKey {
            key_id: format!("ed25519:{id}"),
            algorithm: "ed25519".to_owned(),
            public_key: format!("pk-{id}"),
            valid_until_at: None,
            old,
        }
    }

    fn remote(server: &str, ids: &[&str]) -> AdminRemoteServerKeys {
        AdminRemoteServerKeys {
            server_name: server.to_owned(),
            keys: ids.iter().map(|id| key(id, false)).collect(),
            cached_at: Some("2026-09-28T00:00:00Z".to_owned()),
        }
    }

    fn federation() -> InMemoryFederationSource {
        InMemoryFederationSource::new()
            .with_destination(AdminDestination {
                server_name: "lonely.example".to_owned(),
                ..AdminDestination::default()
            })
            .with_keys(InMemoryKeys {
                own: vec![key("a_1", false)],
                cached: [(
                    "remote.example".to_owned(),
                    remote("remote.example", &["old"]),
                )]
                .into(),
                reachable: [(
                    "remote.example".to_owned(),
                    remote("remote.example", &["new"]),
                )]
                .into(),
            })
    }

    fn member(user_id: &str, membership: &str) -> AdminRoomMember {
        AdminRoomMember {
            user_id: user_id.to_owned(),
            membership: membership.to_owned(),
            display_name: None,
            avatar_url: None,
        }
    }

    fn room(id: &str, joined: u64) -> AdminRoom {
        AdminRoom {
            room_id: id.to_owned(),
            joined_members_count: joined,
            ..AdminRoom::default()
        }
    }

    #[tokio::test]
    async fn the_rooms_shared_with_a_destination_most_of_its_members_first() {
        let (state, _) = state();
        let rooms = InMemoryRoomDirectory::new()
            .with_room(room("!a:here", 2))
            .with_room(room("!b:here", 4))
            .with_room(room("!c:here", 1))
            .with_member("!a:here", member("@me:here", "join"))
            .with_member("!a:here", member("@x:remote.example", "join"))
            .with_member("!b:here", member("@me:here", "join"))
            .with_member("!b:here", member("@x:remote.example", "join"))
            .with_member("!b:here", member("@y:remote.example", "join"))
            .with_member("!b:here", member("@z:remote.example", "leave"))
            .with_member("!c:here", member("@me:here", "join"))
            .with_member("!c:here", member("@x:remote.example:8448", "join"));
        let state = state
            .with_rooms(Arc::new(rooms))
            .with_federation(Arc::new(federation()));
        let uri = "/api/v1/federation/destinations/remote.example/rooms?include_total=true";
        let (status, _, page) = call(&state, "GET", uri, Some("read"), None, None).await;
        assert_eq!(status, StatusCode::OK, "{page}");
        let items = page["items"].as_array().unwrap();
        assert_eq!(items.len(), 2, "{page}");
        assert_eq!(items[0]["room_id"], "!b:here");
        assert_eq!(items[0]["destination_members_count"], 2);
        assert_eq!(items[0]["joined_members_count"], 4);
        assert_eq!(items[1]["room_id"], "!a:here");
        assert_eq!(page["total"], 2);

        // Known but sharing nothing: an empty page. Neither: 404.
        let uri = "/api/v1/federation/destinations/lonely.example/rooms";
        let (status, _, page) = call(&state, "GET", uri, Some("read"), None, None).await;
        assert_eq!(status, StatusCode::OK, "{page}");
        assert_eq!(page["items"], json!([]));
        let uri = "/api/v1/federation/destinations/nobody.example/rooms";
        let (status, _, _) = call(&state, "GET", uri, Some("read"), None, None).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    fn facts(name: &str) -> DestinationFacts {
        DestinationFacts {
            server_name: name.to_owned(),
            ..DestinationFacts::default()
        }
    }

    const DAY_MS: u64 = 86_400_000;

    /// The prune rules, one case each (decision 0042).
    #[test]
    fn the_prune_rules_keep_relationships_and_forget_state() {
        let now = 100 * DAY_MS;
        let rules = PruneRules {
            idle_for: std::time::Duration::from_secs(7 * 86_400),
            failing_for: Some(std::time::Duration::from_secs(7 * 86_400)),
        };
        let shares = DestinationFacts {
            shared_rooms: 3,
            queued_pdus: 0,
            ..facts("shares.example")
        };
        let d = decide(&shares, &rules, now);
        assert_eq!(d.reason, PruneReason::SharesRooms);
        assert!(!d.forget());
        assert_eq!(d.detail, "shares 3 rooms with this server");

        let current = DestinationFacts {
            queued_pdus: 2,
            rooms_behind_current: 1,
            rooms_behind_left: 1,
            failing_since_ms: Some(now - 30 * DAY_MS),
            ..facts("kicked.example")
        };
        assert_eq!(
            decide(&current, &rules, now).reason,
            PruneReason::QueuedForCurrentRooms
        );

        let unused = DestinationFacts {
            last_attempt_ms: Some(now - 8 * DAY_MS),
            last_success_ms: Some(now - 9 * DAY_MS),
            ..facts("unused.example")
        };
        let d = decide(&unused, &rules, now);
        assert_eq!(d.reason, PruneReason::Unused);
        assert!(d.forget());
        assert_eq!(
            d.detail,
            "shares no room, has nothing queued, last active 8 days ago"
        );
        assert_eq!(
            decide(&facts("never.example"), &rules, now).detail,
            "shares no room, has nothing queued, never tried"
        );

        let active = DestinationFacts {
            last_success_ms: Some(now - 2 * DAY_MS),
            ..facts("active.example")
        };
        assert_eq!(
            decide(&active, &rules, now).reason,
            PruneReason::ActiveRecently
        );
        // At once, for the administrator's prune.
        let at_once = PruneRules {
            idle_for: std::time::Duration::ZERO,
            failing_for: None,
        };
        assert_eq!(decide(&active, &at_once, now).reason, PruneReason::Unused);

        let failing = DestinationFacts {
            queued_pdus: 1,
            rooms_behind_left: 1,
            failing_since_ms: Some(now - 10 * DAY_MS),
            ..facts("failing.example")
        };
        let d = decide(&failing, &rules, now);
        assert_eq!(d.reason, PruneReason::Failing);
        assert!(d.forget());
        assert_eq!(
            d.detail,
            "has 1 event queued, only for 1 room this server has left; failing for 10 days"
        );
        let d = decide(&failing, &at_once, now);
        assert_eq!(
            d.reason,
            PruneReason::FailingRecently,
            "no failing_for: kept"
        );
        assert!(d.detail.ends_with("kept because no failing_for was given"));
        let recent = DestinationFacts {
            failing_since_ms: Some(now - DAY_MS),
            ..failing.clone()
        };
        assert_eq!(
            decide(&recent, &rules, now).reason,
            PruneReason::FailingRecently
        );
        let healthy = DestinationFacts {
            failing_since_ms: None,
            ..failing.clone()
        };
        assert_eq!(
            decide(&healthy, &rules, now).reason,
            PruneReason::QueuedNotFailing
        );

        // Catching up in a left room only, nothing queued: the queue rule applies.
        let catching = DestinationFacts {
            catching_up: true,
            rooms_behind_left: 2,
            failing_since_ms: Some(now - 10 * DAY_MS),
            ..facts("catching.example")
        };
        let d = decide(&catching, &rules, now);
        assert_eq!(d.reason, PruneReason::Failing);
        assert!(d.detail.starts_with("is owed the latest event of 2 rooms"));

        let decisions = vec![
            decide(&shares, &rules, now),
            decide(&unused, &rules, now),
            decide(&failing, &rules, now),
        ];
        let summary = report(&decisions, true);
        assert!(summary.dry_run);
        assert_eq!(summary.forgotten.count, 2);
        assert_eq!(summary.forgotten.by_reason["unused"], 1);
        assert_eq!(summary.forgotten.by_reason["failing"], 1);
        assert_eq!(summary.kept.count, 1);
        assert_eq!(summary.kept.servers[0].reason, "shares_rooms");
        assert_eq!(parse_failing_for("7d").unwrap().as_secs(), 7 * 86_400);
        assert!(parse_failing_for("soon").is_err());
    }

    fn destinations() -> InMemoryFederationSource {
        let row = |name: &str, shared: u64, pending: u64, failing: Option<&str>| AdminDestination {
            server_name: name.to_owned(),
            shared_rooms_count: Some(shared),
            pending_pdu_count: pending,
            failing_since: failing.map(str::to_owned),
            ..AdminDestination::default()
        };
        InMemoryFederationSource::new()
            .with_destination(row("friend.example", 2, 0, None))
            .with_destination(row("lonely.example", 0, 0, None))
            .with_destination(row("stale.example", 0, 5, Some("2026-09-01T00:00:00Z")))
            .with_destination(row("fresh.example", 0, 5, None))
    }

    #[tokio::test]
    async fn forgetting_a_destination_is_refused_while_a_room_is_shared_unless_forced() {
        let (state, audit) = state();
        let state = state.with_federation(Arc::new(destinations()));
        let mut events = state.events.subscribe();
        let uri = "/api/v1/federation/destinations/friend.example";
        let (status, _, _) = call(&state, "DELETE", uri, Some("read"), None, None).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        let (status, _, problem) = call(&state, "DELETE", uri, Some("admin"), None, None).await;
        assert_eq!(status, StatusCode::CONFLICT, "{problem}");
        assert_eq!(problem["type"], "urn:hs:problem:conflict");
        let detail = problem["detail"].as_str().unwrap();
        assert!(detail.contains("shares 2 rooms"), "{detail}");
        assert!(detail.contains("force=true"), "{detail}");
        let (status, _, row) = call(&state, "GET", uri, Some("read"), None, None).await;
        assert_eq!(status, StatusCode::OK, "still there: {row}");

        let forced = format!("{uri}?force=true");
        let (status, _, forgotten) =
            call(&state, "DELETE", &forced, Some("admin"), None, None).await;
        assert_eq!(status, StatusCode::OK, "{forgotten}");
        assert_eq!(forgotten["server_name"], "friend.example");
        assert_eq!(forgotten["shared_rooms_count"], 2);
        let (status, _, _) = call(&state, "GET", uri, Some("read"), None, None).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "gone");
        let (status, _, _) = call(&state, "DELETE", uri, Some("admin"), None, None).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "forgetting it again");

        // One sharing no room needs no force, and takes its queue with it.
        let uri = "/api/v1/federation/destinations/stale.example";
        let (status, _, forgotten) = call(&state, "DELETE", uri, Some("admin"), None, None).await;
        assert_eq!(status, StatusCode::OK, "{forgotten}");
        assert_eq!(forgotten["dropped_pdu_count"], 5);

        let entries =
            crate::audit::AuditSink::query(audit.as_ref(), &crate::audit::AuditFilter::default())
                .await
                .unwrap();
        let forgets: Vec<_> = entries
            .iter()
            .filter(|e| e.action == "federation.destinations.forget")
            .collect();
        assert_eq!(
            forgets.len(),
            2,
            "the refusal is not audited; the two forgets are"
        );
        let mut targets: Vec<&str> = forgets.iter().map(|e| e.target.id.as_str()).collect();
        targets.sort_unstable();
        assert_eq!(targets, vec!["friend.example", "stale.example"]);
        let mut types = Vec::new();
        while let Ok(event) = events.try_recv() {
            types.push(event.r#type);
        }
        assert_eq!(
            types
                .iter()
                .filter(|t| *t == "federation.destination_forgotten")
                .count(),
            2
        );
    }

    #[tokio::test]
    async fn a_prune_reports_in_a_dry_run_and_forgets_for_real_with_failing_for() {
        let (state, audit) = state();
        let state = state.with_federation(Arc::new(destinations()));
        let uri = "/api/v1/federation/destinations/prune?dry_run=true";
        let (status, _, report) = call(&state, "POST", uri, Some("admin"), None, None).await;
        assert_eq!(status, StatusCode::OK, "{report}");
        assert_eq!(report["dry_run"], true);
        assert_eq!(report["forgotten"]["count"], 1, "{report}");
        assert_eq!(report["forgotten"]["by_reason"]["unused"], 1);
        assert_eq!(
            report["forgotten"]["servers"][0]["server_name"],
            "lonely.example"
        );
        assert_eq!(report["kept"]["count"], 3);
        assert_eq!(report["kept"]["by_reason"]["shares_rooms"], 1);
        assert_eq!(report["kept"]["by_reason"]["failing_recently"], 1);
        assert_eq!(report["kept"]["by_reason"]["queued_not_failing"], 1);
        let (status, _, page) = call(
            &state,
            "GET",
            "/api/v1/federation/destinations?include_total=true",
            Some("read"),
            None,
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(page["total"], 4, "a dry run forgets nothing");

        // A bad duration is a 400 naming the field.
        let uri = "/api/v1/federation/destinations/prune";
        let (status, _, problem) = call(
            &state,
            "POST",
            uri,
            Some("admin"),
            Some(json!({"failing_for": "whenever"})),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{problem}");
        assert_eq!(problem["errors"][0]["pointer"], "/failing_for");

        let (status, _, report) = call(
            &state,
            "POST",
            uri,
            Some("admin"),
            Some(json!({"failing_for": "7d"})),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{report}");
        assert_eq!(report["dry_run"], false);
        assert_eq!(report["forgotten"]["count"], 2, "{report}");
        assert_eq!(report["forgotten"]["by_reason"]["failing"], 1);
        assert_eq!(report["kept"]["count"], 2);
        let (_, _, page) = call(
            &state,
            "GET",
            "/api/v1/federation/destinations?include_total=true&shares_room=false",
            Some("read"),
            None,
            None,
        )
        .await;
        assert_eq!(page["total"], 1, "{page}");
        assert_eq!(page["items"][0]["server_name"], "fresh.example");
        let (_, _, page) = call(
            &state,
            "GET",
            "/api/v1/federation/destinations?include_total=true&shares_room=true",
            Some("read"),
            None,
            None,
        )
        .await;
        assert_eq!(page["total"], 1, "{page}");
        assert_eq!(page["items"][0]["server_name"], "friend.example");

        let entries =
            crate::audit::AuditSink::query(audit.as_ref(), &crate::audit::AuditFilter::default())
                .await
                .unwrap();
        let prunes: Vec<_> = entries
            .iter()
            .filter(|e| e.action == "federation.destinations.prune")
            .collect();
        assert_eq!(prunes.len(), 1, "the dry run is not audited; the prune is");
    }

    #[tokio::test]
    async fn own_keys_and_the_cache_are_readable_with_admin_read() {
        let (state, _) = state();
        let state = state.with_federation(Arc::new(federation()));
        let (status, _, keys) = call(
            &state,
            "GET",
            "/api/v1/federation/keys",
            Some("read"),
            None,
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{keys}");
        assert_eq!(keys[0]["key_id"], "ed25519:a_1");
        let (status, _, cached) = call(
            &state,
            "GET",
            "/api/v1/federation/keys/remote.example",
            Some("read"),
            None,
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{cached}");
        assert_eq!(cached["keys"][0]["key_id"], "ed25519:old");
        let (status, _, _) = call(
            &state,
            "GET",
            "/api/v1/federation/keys/unknown.example",
            Some("read"),
            None,
            None,
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        let (status, _, _) = call(&state, "GET", "/api/v1/federation/keys", None, None, None).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }

    async fn settled(state: &AdminState, id: &str) -> serde_json::Value {
        for _ in 0..200 {
            let uri = format!("/api/v1/tasks/{id}");
            let (_, _, task) = call(state, "GET", &uri, Some("read"), None, None).await;
            if task["status"] != "running" {
                return task;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        panic!("task {id} never ended");
    }

    #[tokio::test]
    async fn a_refresh_is_an_audited_task_that_ends_with_what_the_cache_holds() {
        let (state, audit) = state();
        let state = state
            .with_federation(Arc::new(federation()))
            .with_tasks(crate::tasks::TaskRegistry::in_memory());
        let mut events = state.events.subscribe();
        let uri = "/api/v1/federation/keys/remote.example/refresh";
        let (status, _, _) = call(&state, "POST", uri, Some("read"), None, None).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        let (status, headers, task) = call(&state, "POST", uri, Some("admin"), None, None).await;
        assert_eq!(status, StatusCode::ACCEPTED, "{task}");
        assert_eq!(task["action"], REFRESH_TASK_ACTION);
        assert_eq!(task["resource"]["id"], "remote.example");
        let id = task["id"].as_str().unwrap();
        assert_eq!(
            headers.get("location").unwrap(),
            &format!("/api/v1/tasks/{id}")
        );
        let task = settled(&state, id).await;
        assert_eq!(task["status"], "succeeded", "{task}");
        assert_eq!(task["result"]["keys"][0]["key_id"], "ed25519:new");
        let (_, _, cached) = call(
            &state,
            "GET",
            "/api/v1/federation/keys/remote.example",
            Some("read"),
            None,
            None,
        )
        .await;
        assert_eq!(
            cached["keys"][0]["key_id"], "ed25519:new",
            "the cache now holds it"
        );
        let mut types = Vec::new();
        while let Ok(event) = events.try_recv() {
            types.push(event.r#type);
        }
        assert!(
            types.contains(&"federation.keys_refresh_started".to_owned()),
            "{types:?}"
        );
        assert!(
            types.contains(&"federation.keys_refreshed".to_owned()),
            "{types:?}"
        );
        let entries =
            crate::audit::AuditSink::query(audit.as_ref(), &crate::audit::AuditFilter::default())
                .await
                .unwrap();
        let entry = entries
            .iter()
            .find(|e| e.action == "federation.keys.refresh")
            .unwrap();
        assert_eq!(entry.outcome.status, 202);

        // A server that cannot be reached: the task fails and says why.
        let uri = "/api/v1/federation/keys/gone.example/refresh";
        let (status, _, task) = call(&state, "POST", uri, Some("admin"), None, None).await;
        assert_eq!(status, StatusCode::ACCEPTED);
        let task = settled(&state, task["id"].as_str().unwrap()).await;
        assert_eq!(task["status"], "failed", "{task}");
        assert!(
            task["error"]["detail"]
                .as_str()
                .unwrap()
                .contains("gone.example")
        );
    }
}
