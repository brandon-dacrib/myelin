//! `POST /search`: full-text search over the events of the rooms the requester is joined to,
//! the spec's one category, `room_events`. The index is `crate::search`; this module reads it,
//! checks each hit against its room (the event is still there, still says what was searched for,
//! passes the filter, and the requester may see it under the room's history visibility, exactly
//! as `/messages` and `/context` decide), orders, pages and renders.
//!
//! Supported: `search_term`, `keys`, `filter` (`limit`, `rooms`, `not_rooms`, `senders`,
//! `not_senders`, `types`, `not_types`), `order_by` (`rank`, the default, or `recent`),
//! `event_context` (`before_limit`, `after_limit`, `include_profile`), `include_state`,
//! `groupings` by `room_id` or `sender`, `next_batch`, `count` and `highlights`.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;
use std::time::Instant;

use axum::Json;
use axum::extract::{Query, State};
use axum::response::{IntoResponse, Response};
use hs_http::body::PermissiveJson;
use hs_kv::KvBackend;
use hs_model::{Event, RoomSn};
use ruma::{OwnedRoomId, OwnedUserId};
use serde_json::{Map, Value, json};

use crate::actor::RoomActor;
use crate::error::RoomError;
use crate::routes::render::{client_event_json, client_event_json_bundled};
use crate::search::{Candidate, Field, event_matches, tokenize};
use crate::state::{RoomRequester, RoomState};
use crate::timeline::{Direction, PaginationToken};

/// Results per page when the filter gives no `limit`.
pub const DEFAULT_LIMIT: usize = 10;
/// The most results one page holds, whatever the filter asks.
pub const MAX_LIMIT: usize = 100;
/// Context events either side of a result when `event_context` gives no limit.
pub const DEFAULT_CONTEXT: usize = 5;
/// The most context events either side of a result.
pub const MAX_CONTEXT: usize = 100;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Order {
    Rank,
    Recent,
}

impl Order {
    fn as_str(self) -> &'static str {
        match self {
            Self::Rank => "rank",
            Self::Recent => "recent",
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct ContextSpec {
    before: usize,
    after: usize,
    include_profile: bool,
}

/// The request's `room_events` criteria, parsed.
#[derive(Debug, Clone)]
struct Criteria {
    terms: Vec<String>,
    fields: Vec<Field>,
    order: Order,
    limit: usize,
    rooms: Option<HashSet<String>>,
    not_rooms: HashSet<String>,
    senders: Option<HashSet<String>>,
    not_senders: HashSet<String>,
    types: Option<Vec<String>>,
    not_types: Vec<String>,
    context: Option<ContextSpec>,
    include_state: bool,
    group_by_room: bool,
    group_by_sender: bool,
}

fn bad(message: impl Into<String>) -> RoomError {
    RoomError::BadRequest(message.into())
}

fn string_list(value: Option<&Value>, what: &str) -> Result<Option<Vec<String>>, RoomError> {
    let Some(value) = value else {
        return Ok(None);
    };
    if value.is_null() {
        return Ok(None);
    }
    let items = value
        .as_array()
        .ok_or_else(|| bad(format!("{what} must be an array of strings")))?;
    items
        .iter()
        .map(|v| {
            v.as_str()
                .map(str::to_owned)
                .ok_or_else(|| bad(format!("{what} must be an array of strings")))
        })
        .collect::<Result<Vec<_>, _>>()
        .map(Some)
}

fn usize_field(object: &Value, key: &str, default: usize, max: usize) -> Result<usize, RoomError> {
    match object.get(key) {
        None | Some(Value::Null) => Ok(default),
        Some(v) => v
            .as_u64()
            .map(|n| usize::try_from(n).unwrap_or(max).min(max))
            .ok_or_else(|| bad(format!("{key} must be a non-negative integer"))),
    }
}

fn parse_criteria(criteria: &Value) -> Result<Criteria, RoomError> {
    let search_term = criteria
        .get("search_term")
        .and_then(Value::as_str)
        .ok_or_else(|| bad("search_categories.room_events.search_term must be a string"))?;
    let mut terms: Vec<String> = Vec::new();
    for term in tokenize(search_term) {
        if !terms.contains(&term) {
            terms.push(term);
        }
    }

    let fields = match string_list(criteria.get("keys"), "keys")? {
        None => Field::ALL.to_vec(),
        Some(keys) => keys
            .iter()
            .map(|key| {
                Field::from_key(key).ok_or_else(|| {
                    RoomError::InvalidParam(format!(
                        "unknown search key {key:?}: expected content.body, content.name or \
                         content.topic"
                    ))
                })
            })
            .collect::<Result<Vec<_>, _>>()?,
    };

    let order = match criteria.get("order_by").and_then(Value::as_str) {
        None | Some("rank") => Order::Rank,
        Some("recent") => Order::Recent,
        Some(other) => {
            return Err(RoomError::InvalidParam(format!(
                "unknown order_by {other:?}: expected rank or recent"
            )));
        }
    };

    let empty = json!({});
    let filter = criteria
        .get("filter")
        .filter(|f| !f.is_null())
        .unwrap_or(&empty);
    if !filter.is_object() {
        return Err(bad("filter must be an object"));
    }
    let set = |list: Option<Vec<String>>| list.map(|l| l.into_iter().collect::<HashSet<_>>());
    let context = match criteria.get("event_context") {
        None | Some(Value::Null) => None,
        Some(spec) if spec.is_object() => Some(ContextSpec {
            before: usize_field(spec, "before_limit", DEFAULT_CONTEXT, MAX_CONTEXT)?,
            after: usize_field(spec, "after_limit", DEFAULT_CONTEXT, MAX_CONTEXT)?,
            include_profile: spec
                .get("include_profile")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        }),
        Some(_) => return Err(bad("event_context must be an object")),
    };
    let mut group_by_room = false;
    let mut group_by_sender = false;
    if let Some(groups) = criteria
        .get("groupings")
        .and_then(|g| g.get("group_by"))
        .and_then(Value::as_array)
    {
        for group in groups {
            match group.get("key").and_then(Value::as_str) {
                Some("room_id") => group_by_room = true,
                Some("sender") => group_by_sender = true,
                _ => {}
            }
        }
    }

    Ok(Criteria {
        terms,
        fields,
        order,
        limit: usize_field(filter, "limit", DEFAULT_LIMIT, MAX_LIMIT)?.max(1),
        rooms: set(string_list(filter.get("rooms"), "filter.rooms")?),
        not_rooms: set(string_list(filter.get("not_rooms"), "filter.not_rooms")?)
            .unwrap_or_default(),
        senders: set(string_list(filter.get("senders"), "filter.senders")?),
        not_senders: set(string_list(
            filter.get("not_senders"),
            "filter.not_senders",
        )?)
        .unwrap_or_default(),
        types: string_list(filter.get("types"), "filter.types")?,
        not_types: string_list(filter.get("not_types"), "filter.not_types")?.unwrap_or_default(),
        context,
        include_state: criteria
            .get("include_state")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        group_by_room,
        group_by_sender,
    })
}

/// A filter's event type pattern: exact, or a prefix ending in `*`.
fn type_matches(pattern: &str, event_type: &str) -> bool {
    match pattern.strip_suffix('*') {
        Some(prefix) => event_type.starts_with(prefix),
        None => pattern == event_type,
    }
}

impl Criteria {
    fn passes_filter(&self, event: &Event) -> bool {
        let header = event.header();
        let sender = header.sender.as_str();
        let event_type = header.event_type.as_str();
        self.senders.as_ref().is_none_or(|s| s.contains(sender))
            && !self.not_senders.contains(sender)
            && self
                .types
                .as_ref()
                .is_none_or(|t| t.iter().any(|p| type_matches(p, event_type)))
            && !self.not_types.iter().any(|p| type_matches(p, event_type))
    }
}

/// A result's place in the order: compared descending, so the first result has the greatest.
type SortKey = (u64, i64, u32, i64);

fn sort_key(candidate: &Candidate, order: Order) -> SortKey {
    let primary = match order {
        // A score is positive and finite, so its bits order as it does.
        Order::Rank => candidate.score.to_bits(),
        Order::Recent => 0,
    };
    (
        primary,
        candidate.ts,
        candidate.room_sn.get(),
        candidate.pos,
    )
}

fn batch_token(order: Order, key: SortKey) -> String {
    format!(
        "{}_{:x}_{}_{}_{}",
        order.as_str(),
        key.0,
        key.1,
        key.2,
        key.3
    )
}

fn parse_batch_token(token: &str, order: Order) -> Result<SortKey, RoomError> {
    let invalid = || RoomError::InvalidParam(format!("invalid next_batch {token:?}"));
    let mut parts = token.split('_');
    if parts.next() != Some(order.as_str()) {
        return Err(invalid());
    }
    let primary =
        u64::from_str_radix(parts.next().ok_or_else(invalid)?, 16).map_err(|_| invalid())?;
    let ts = parts
        .next()
        .ok_or_else(invalid)?
        .parse()
        .map_err(|_| invalid())?;
    let room = parts
        .next()
        .ok_or_else(invalid)?
        .parse()
        .map_err(|_| invalid())?;
    let pos = parts
        .next()
        .ok_or_else(invalid)?
        .parse()
        .map_err(|_| invalid())?;
    if parts.next().is_some() {
        return Err(invalid());
    }
    Ok((primary, ts, room, pos))
}

/// One hit that passed its room's checks.
#[derive(Debug, Clone)]
struct Visible {
    candidate: Candidate,
    room_id: OwnedRoomId,
    sender: String,
}

/// The hits of one room that are still there, still match, pass the filter and are visible to
/// `user`.
fn check_hits<B: KvBackend>(
    actor: &RoomActor<B>,
    hits: Vec<Candidate>,
    criteria: &Criteria,
    user: &ruma::UserId,
) -> Vec<(Candidate, String)> {
    hits.into_iter()
        .filter_map(|hit| {
            let event = actor.event_at(hit.pos)?;
            let ok = criteria.passes_filter(event)
                && event_matches(event, &criteria.terms, &criteria.fields)
                && actor.event_visible_to(event, user).unwrap_or(false);
            ok.then(|| (hit, event.header().sender.to_string()))
        })
        .collect()
}

fn render_event<B: KvBackend>(actor: &RoomActor<B>, event: &Event, user: &ruma::UserId) -> Value {
    let bundle = actor.relation_bundle(event.event_id(), user);
    client_event_json_bundled(event, &bundle)
}

fn profile_of<B: KvBackend>(actor: &RoomActor<B>, user_id: &str) -> Value {
    let content = actor
        .state_event("m.room.member", user_id)
        .ok()
        .flatten()
        .map(client_event_json)
        .and_then(|e| e.get("content").cloned())
        .unwrap_or_else(|| json!({}));
    let mut profile = Map::new();
    for key in ["displayname", "avatar_url"] {
        if let Some(value) = content.get(key).filter(|v| v.is_string()) {
            profile.insert(key.to_owned(), value.clone());
        }
    }
    Value::Object(profile)
}

/// The rendered page entries of one room: each result (and its context), and the room's state
/// when asked for.
struct RoomRender {
    results: HashMap<i64, (Value, Option<Value>)>,
    state: Option<Vec<Value>>,
}

fn render_room<B: KvBackend>(
    actor: &RoomActor<B>,
    positions: &[i64],
    criteria: &Criteria,
    user: &ruma::UserId,
) -> RoomRender {
    let visible = |e: &Event| actor.event_visible_to(e, user).unwrap_or(false);
    let mut results = HashMap::new();
    for &pos in positions {
        let Some(event) = actor.event_at(pos) else {
            continue;
        };
        let context = criteria.context.map(|spec| {
            let (before, after) = actor.events_around(pos, spec.before, spec.after);
            let before: Vec<(i64, &Event)> = before.into_iter().filter(|(_, e)| visible(e)).collect();
            let after: Vec<(i64, &Event)> = after.into_iter().filter(|(_, e)| visible(e)).collect();
            let start = before.last().map_or(pos, |(p, _)| *p);
            let end = after.last().map_or(pos, |(p, _)| *p);
            let mut context = json!({
                "events_before": before.iter().map(|(_, e)| render_event(actor, e, user)).collect::<Vec<_>>(),
                "events_after": after.iter().map(|(_, e)| render_event(actor, e, user)).collect::<Vec<_>>(),
                "start": PaginationToken::new(start, Direction::Backward).to_string(),
                "end": PaginationToken::new(end, Direction::Forward).to_string(),
            });
            if spec.include_profile {
                let mut senders: Vec<String> = vec![event.header().sender.to_string()];
                for (_, e) in before.iter().chain(after.iter()) {
                    let sender = e.header().sender.to_string();
                    if !senders.contains(&sender) {
                        senders.push(sender);
                    }
                }
                let profiles: Map<String, Value> = senders
                    .into_iter()
                    .map(|s| {
                        let profile = profile_of(actor, &s);
                        (s, profile)
                    })
                    .collect();
                context["profile_info"] = Value::Object(profiles);
            }
            context
        });
        results.insert(pos, (render_event(actor, event, user), context));
    }
    let state = criteria.include_state.then(|| {
        actor
            .full_state_for_reader(user)
            .ok()
            .flatten()
            .unwrap_or_default()
            .into_iter()
            .map(client_event_json)
            .collect()
    });
    RoomRender { results, state }
}

/// `POST /search`.
///
/// # Errors
/// `400` for a malformed request (no `search_term`, an unknown key or `order_by`, a bad
/// `next_batch`); a storage error otherwise.
pub async fn post_search<B: KvBackend + 'static>(
    State(state): State<RoomState<B>>,
    RoomRequester(requester): RoomRequester,
    Query(params): Query<HashMap<String, String>>,
    PermissiveJson(body): PermissiveJson<Value>,
) -> Result<Response, RoomError> {
    let started = Instant::now();
    let result = search(
        &state,
        requester.user_id.clone(),
        &body,
        params.get("next_batch"),
    )
    .await;
    crate::metrics::observe_search_duration(started.elapsed());
    if let Ok(value) = &result {
        tracing::debug!(
            user = %requester.user_id,
            count = value["search_categories"]["room_events"]["count"].as_u64().unwrap_or(0),
            elapsed_ms = started.elapsed().as_millis() as u64,
            "searched room events"
        );
    }
    result.map(|v| Json(v).into_response())
}

async fn search<B: KvBackend + 'static>(
    state: &RoomState<B>,
    user: OwnedUserId,
    body: &Value,
    next_batch: Option<&String>,
) -> Result<Value, RoomError> {
    let Some(raw) = body
        .get("search_categories")
        .ok_or_else(|| bad("search_categories is required"))?
        .get("room_events")
        .filter(|v| !v.is_null())
    else {
        return Ok(json!({"search_categories": {}}));
    };
    let criteria = Arc::new(parse_criteria(raw)?);
    let after = next_batch
        .map(|token| parse_batch_token(token, criteria.order))
        .transpose()?;

    // The rooms the requester is joined to, narrowed by the filter.
    let mut rooms: HashMap<RoomSn, OwnedRoomId> = HashMap::new();
    let index = state.rooms.search_index().clone();
    for room_id in state.rooms.rooms_joined_by_user(&user)? {
        let wanted = criteria
            .rooms
            .as_ref()
            .is_none_or(|r| r.contains(room_id.as_str()))
            && !criteria.not_rooms.contains(room_id.as_str());
        if wanted && let Some(sn) = index.room_sn(&room_id)? {
            rooms.insert(sn, room_id);
        }
    }

    // Read-your-writes: a room this replica owns whose newest events the indexer has not reached
    // yet (a message sent a moment ago) is brought up to date before the index is read.
    for room_id in rooms.values() {
        if state.rooms.owns_room(room_id)
            && let Err(error) = crate::search::index_room(&state.rooms, room_id).await
        {
            tracing::warn!(%error, %room_id, "search: could not bring a room's index up to date");
        }
    }

    let room_set: HashSet<RoomSn> = rooms.keys().copied().collect();
    let query = {
        let (index, terms, fields) = (
            index.clone(),
            criteria.terms.clone(),
            criteria.fields.clone(),
        );
        tokio::task::spawn_blocking(move || index.query(&terms, &fields, &room_set))
            .await
            .map_err(|e| RoomError::Internal(format!("search task failed: {e}")))??
    };
    if query.truncated {
        tracing::info!(
            terms = ?criteria.terms,
            "a search word matched more postings than are read per word; count is a lower bound"
        );
    }

    // Each hit checked against its room.
    let mut by_room: BTreeMap<RoomSn, Vec<Candidate>> = BTreeMap::new();
    for candidate in query.candidates {
        by_room
            .entry(candidate.room_sn)
            .or_default()
            .push(candidate);
    }
    let mut visible: Vec<Visible> = Vec::new();
    for (room_sn, hits) in by_room {
        let Some(room_id) = rooms.get(&room_sn).cloned() else {
            continue;
        };
        let (criteria, user) = (criteria.clone(), user.clone());
        let checked = match state
            .rooms
            .read_room(&room_id, move |actor| {
                check_hits(actor, hits, &criteria, &user)
            })
            .await
        {
            Ok(checked) => checked,
            Err(RoomError::RoomNotFound(_)) => continue,
            Err(e) => return Err(e),
        };
        visible.extend(checked.into_iter().map(|(candidate, sender)| Visible {
            candidate,
            room_id: room_id.clone(),
            sender,
        }));
    }
    let order = criteria.order;
    visible.sort_by_key(|v| std::cmp::Reverse(sort_key(&v.candidate, order)));
    let count = visible.len();

    // The page: what comes after the token, `limit` of it.
    let remaining: Vec<Visible> = match after {
        Some(token) => visible
            .into_iter()
            .filter(|v| sort_key(&v.candidate, order) < token)
            .collect(),
        None => visible,
    };
    let page: Vec<Visible> = remaining.iter().take(criteria.limit).cloned().collect();
    // A full page has a `next_batch`, whether or not anything is left (as Synapse, and as
    // Complement's `TestSearch` expects); the page after the last is empty and has none.
    let next = (!page.is_empty() && page.len() == criteria.limit)
        .then(|| {
            page.last()
                .map(|v| batch_token(order, sort_key(&v.candidate, order)))
        })
        .flatten();

    // Rendered a room at a time.
    let mut page_rooms: Vec<OwnedRoomId> = Vec::new();
    let mut positions: HashMap<OwnedRoomId, Vec<i64>> = HashMap::new();
    for v in &page {
        if !positions.contains_key(&v.room_id) {
            page_rooms.push(v.room_id.clone());
        }
        positions
            .entry(v.room_id.clone())
            .or_default()
            .push(v.candidate.pos);
    }
    let mut rendered: HashMap<OwnedRoomId, RoomRender> = HashMap::new();
    for room_id in &page_rooms {
        let room_positions = positions.remove(room_id).unwrap_or_default();
        let (criteria, user) = (criteria.clone(), user.clone());
        let render = state
            .rooms
            .read_room(room_id, move |actor| {
                render_room(actor, &room_positions, &criteria, &user)
            })
            .await?;
        rendered.insert(room_id.clone(), render);
    }

    let mut results = Vec::with_capacity(page.len());
    let mut result_ids: Vec<(OwnedRoomId, String, String)> = Vec::new();
    for v in &page {
        let Some((event, context)) = rendered
            .get_mut(&v.room_id)
            .and_then(|r| r.results.remove(&v.candidate.pos))
        else {
            continue;
        };
        if let Some(event_id) = event.get("event_id").and_then(Value::as_str) {
            result_ids.push((v.room_id.clone(), v.sender.clone(), event_id.to_owned()));
        }
        let mut result = json!({"rank": v.candidate.score, "result": event});
        if let Some(context) = context {
            result["context"] = context;
        }
        results.push(result);
    }

    let mut room_events = json!({
        "results": results,
        "count": count,
        "highlights": query.highlights.into_iter().collect::<Vec<_>>(),
    });
    if let Some(next) = &next {
        room_events["next_batch"] = json!(next);
    }
    if criteria.include_state {
        let state_map: Map<String, Value> = rendered
            .iter_mut()
            .filter_map(|(room_id, r)| Some((room_id.to_string(), Value::from(r.state.take()?))))
            .collect();
        room_events["state"] = Value::Object(state_map);
    }
    if criteria.group_by_room || criteria.group_by_sender {
        let mut groups = Map::new();
        let mut group = |by: &str, key_of: &dyn Fn(&(OwnedRoomId, String, String)) -> String| {
            let mut map: Map<String, Value> = Map::new();
            for entry in &result_ids {
                let key = key_of(entry);
                let order = map.len();
                let slot = map.entry(key).or_insert_with(|| {
                    let mut g = json!({"results": [], "order": order});
                    if let Some(next) = &next {
                        g["next_batch"] = json!(next);
                    }
                    g
                });
                if let Some(list) = slot["results"].as_array_mut() {
                    list.push(json!(entry.2));
                }
            }
            groups.insert(by.to_owned(), Value::Object(map));
        };
        if criteria.group_by_room {
            group("room_id", &|e| e.0.to_string());
        }
        if criteria.group_by_sender {
            group("sender", &|e| e.1.clone());
        }
        room_events["groups"] = Value::Object(groups);
    }
    Ok(json!({"search_categories": {"room_events": room_events}}))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_batch_token_reads_back_and_names_its_order() {
        let key = (42.5f64.to_bits(), 1_700_000_000_000, 7, 12);
        let token = batch_token(Order::Rank, key);
        assert_eq!(parse_batch_token(&token, Order::Rank).unwrap(), key);
        assert!(parse_batch_token(&token, Order::Recent).is_err());
        assert!(parse_batch_token("rank_zz_1_2_3", Order::Rank).is_err());
    }

    #[test]
    fn criteria_default_and_refuse_what_the_spec_does_not_define() {
        let c = parse_criteria(&json!({"search_term": "Hello  hello world"})).unwrap();
        assert_eq!(c.terms, ["hello", "world"]);
        assert_eq!(c.fields, Field::ALL.to_vec());
        assert_eq!(c.order, Order::Rank);
        assert_eq!(c.limit, DEFAULT_LIMIT);
        assert!(parse_criteria(&json!({})).is_err());
        assert!(parse_criteria(&json!({"search_term": "x", "keys": ["content.nope"]})).is_err());
        assert!(parse_criteria(&json!({"search_term": "x", "order_by": "oldest"})).is_err());
    }

    #[test]
    fn type_patterns_take_a_trailing_wildcard() {
        assert!(type_matches("m.room.*", "m.room.message"));
        assert!(type_matches("m.room.message", "m.room.message"));
        assert!(!type_matches("m.room.name", "m.room.message"));
    }
}
