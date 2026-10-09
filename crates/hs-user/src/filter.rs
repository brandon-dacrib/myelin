//! `/sync`'s `filter` query parameter and the `POST`/`GET /user/{userId}/filter` endpoints'
//! bodies: [`SyncFilter`] parses the spec's filter JSON shape in full (every field the spec
//! defines deserializes without error, so an unrecognized filter is never silently rejected as
//! malformed), but [`crate::sync`] only *applies* a documented subset of it.
//!
//! # What is applied, and what is not
//!
//! Per this track's brief ("if not, implement the filter parsing and say plainly which filter
//! fields are ignored, because silently ignoring a filter is worse than rejecting it"), here is
//! that list, kept next to the parser rather than buried in `crate::sync` so it cannot drift out
//! of sync with what the parser accepts:
//!
//! **Applied** (`crate::sync` reads these):
//! - `room.rooms` / `room.not_rooms`: restrict which rooms appear in the response at all.
//! - `room.timeline.limit`: caps how many timeline events a room's response carries.
//! - `room.timeline.rooms` / `room.timeline.not_rooms`: same restriction, scoped to the timeline
//!   section specifically (applied identically to the top-level `room.rooms`/`not_rooms` --  this
//!   crate does not yet support timeline and state having *different* room sets, see below).
//! - `room.timeline.types` / `not_types` / `senders` / `not_senders`: content-based filtering of
//!   which timeline events a room's response carries, applied via [`RoomEventFilter::matches`].
//!   See [`crate::sync::build_incremental_timeline`]/`build_fresh_timeline`'s doc comments for the
//!   bounded-scan simplification this implies once a content filter is present (a filter that
//!   excludes nearly everything cannot turn one `/sync` call into an unbounded history scan).
//! - `room.state.types` / `not_types` / `senders` / `not_senders`: the equivalent content filter
//!   for the `state` section.
//! - `room.state.lazy_load_members`: when true, a room's `state` section includes only the
//!   members who sent events in the returned `timeline`, plus the requesting user's own
//!   membership on a sync the client has no baseline for, plus -- in a gapped incremental sync
//!   -- whoever joined or left inside the gap (`crate::sync`'s `LazyScope`). A member event this
//!   device was already sent is left out of later incremental syncs (`crate::lazy_members`).
//! - `room.state.include_redundant_members`: when true, that memory is not consulted and every
//!   sender's membership is sent again.
//! - `room.include_leave`: when true, left rooms are included in an initial sync's room set (an
//!   incremental sync always includes a room the user just left, regardless of this flag, since
//!   the client needs to see the leave event itself).
//!
//! - `event_format: "federation"`: events are rendered as servers exchange them
//!   (`prev_events`, `auth_events`, `depth`, `hashes`, `signatures`), with `event_id`, `room_id`
//!   and `unsigned` added (`crate::sync`'s `federation_format`).
//! - `presence` (top-level): `types`/`not_types`/`senders`/`not_senders` on the `m.presence`
//!   events, within the shared-room scope `crate::sync` already applies.
//! - `account_data` (top-level) and `room.account_data`: by event type (and, for the room one,
//!   `rooms`/`not_rooms`); account data has no sender, and the user's own id stands for one.
//! - `room.ephemeral`: by event type (`m.typing`, `m.receipt`) and `rooms`/`not_rooms`.
//! - `event_fields`: every timeline and state event of a joined, left or peeked room is pruned
//!   to the fields named ([`SyncFilter::prune_event_fields`]): dotted paths name sub-fields
//!   (`content.body`), `\.` and `\\` are a literal dot and backslash, a path the event does
//!   not have is skipped, and an empty list means every field, as Synapse reads it. Stripped
//!   state (`invite_state`, `knock_state`), account data, ephemeral events and presence are
//!   rendered whole, as Synapse renders them.
//!
//! **Parsed but ignored** (present in [`SyncFilter`]'s fields so a client's filter round-trips
//! and is never rejected, but `crate::sync` does not act on it):
//! - `limit` anywhere but `room.timeline`. Synapse does the same: its presence and ephemeral
//!   sources take the filter's limit and never apply it, so a client has no behaviour to miss.
//!
//! This asymmetry (timeline vs. state cannot independently restrict which rooms they cover) is a
//! known simplification: the spec allows `room.timeline.rooms` and `room.state.rooms` to differ,
//! but `crate::sync` builds one candidate room set per response and applies it uniformly.

use serde::Deserialize;

/// One event-filter object (used for `presence`, top-level `account_data`, and as the shape most
/// of `RoomFilter`'s sub-filters share). Every field is optional and defaults to "no restriction"
/// per the spec.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct EventFilter {
    /// Maximum number of events to return. Applied only for `room.timeline` -- see the module
    /// docs.
    pub limit: Option<usize>,
    /// Event types to include; a trailing `*` is the spec's wildcard (`m.room.*`).
    pub types: Option<Vec<String>>,
    /// Event types to exclude.
    pub not_types: Option<Vec<String>>,
    /// Senders to include.
    pub senders: Option<Vec<String>>,
    /// Senders to exclude.
    pub not_senders: Option<Vec<String>>,
}

/// A `RoomEventFilter`: an [`EventFilter`] plus the room-scoped and lazy-loading fields the spec
/// adds for `room.timeline`, `room.state` and `room.ephemeral`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct RoomEventFilter {
    /// Maximum number of events.
    pub limit: Option<usize>,
    /// Event types to include.
    pub types: Option<Vec<String>>,
    /// Event types to exclude.
    pub not_types: Option<Vec<String>>,
    /// Senders to include.
    pub senders: Option<Vec<String>>,
    /// Senders to exclude.
    pub not_senders: Option<Vec<String>>,
    /// Rooms to include.
    pub rooms: Option<Vec<String>>,
    /// Rooms to exclude.
    pub not_rooms: Option<Vec<String>>,
    /// Whether to send only the state necessary to display the timeline (lazy loading of room
    /// members, MSC1227 / the stable spec feature). Applied -- see the module docs.
    pub lazy_load_members: Option<bool>,
    /// Whether a lazy-loaded incremental sync should send a member's event again although this
    /// device was sent it before. Applied -- see the module docs.
    pub include_redundant_members: Option<bool>,
    /// Whether `/sync` splits a room's notification counts by thread (MSC3773, spec v1.4):
    /// `unread_notifications` for the main timeline and `unread_thread_notifications` per thread.
    /// Applied from the room timeline filter ([`SyncFilter::unread_thread_notifications`]).
    pub unread_thread_notifications: Option<bool>,
}

/// Whether `pattern` (one entry of a `types`/`not_types` list) matches `event_type`. The spec
/// allows a trailing `*` wildcard (e.g. `"m.room.*"`); anything else is an exact match.
fn type_pattern_matches(pattern: &str, event_type: &str) -> bool {
    pattern
        .strip_suffix('*')
        .map_or(pattern == event_type, |prefix| {
            event_type.starts_with(prefix)
        })
}

impl EventFilter {
    /// Whether an event of `event_type` from `sender` passes this filter's `types`/`not_types`/
    /// `senders`/`not_senders`, with the same precedence as [`RoomEventFilter::matches`].
    #[must_use]
    pub fn matches(&self, event_type: &str, sender: &str) -> bool {
        type_and_sender_match(
            self.types.as_deref(),
            self.not_types.as_deref(),
            self.senders.as_deref(),
            self.not_senders.as_deref(),
            event_type,
            sender,
        )
    }
}

/// The `types`/`not_types`/`senders`/`not_senders` rule both filter shapes share: a denylist
/// excludes outright, then an allowlist must name the event.
fn type_and_sender_match(
    types: Option<&[String]>,
    not_types: Option<&[String]>,
    senders: Option<&[String]>,
    not_senders: Option<&[String]>,
    event_type: &str,
    sender: &str,
) -> bool {
    if not_types.is_some_and(|list| list.iter().any(|t| type_pattern_matches(t, event_type))) {
        return false;
    }
    if not_senders.is_some_and(|list| list.iter().any(|s| s == sender)) {
        return false;
    }
    if types.is_some_and(|list| !list.iter().any(|t| type_pattern_matches(t, event_type))) {
        return false;
    }
    if senders.is_some_and(|list| !list.iter().any(|s| s == sender)) {
        return false;
    }
    true
}

impl RoomEventFilter {
    /// Whether this filter has no content restriction at all (`types`/`not_types`/`senders`/
    /// `not_senders` all absent) -- used by `crate::sync` to take its unfiltered, single-`paginate`-call
    /// fast path for the overwhelmingly common case of a filter that only sets `limit` or
    /// `lazy_load_members`.
    #[must_use]
    pub fn is_content_noop(&self) -> bool {
        self.types.is_none()
            && self.not_types.is_none()
            && self.senders.is_none()
            && self.not_senders.is_none()
    }

    /// Whether an event of `event_type` from `sender` passes this filter's `types`/`not_types`/
    /// `senders`/`not_senders`. `not_types`/`not_senders` are checked first and win outright (an
    /// event excluded by either can never be let back in by `types`/`senders`), matching the
    /// spec's own denylist-wins-over-allowlist framing (the same precedence
    /// [`SyncFilter::room_allowed`] already uses for `not_rooms`).
    #[must_use]
    pub fn matches(&self, event_type: &str, sender: &str) -> bool {
        type_and_sender_match(
            self.types.as_deref(),
            self.not_types.as_deref(),
            self.senders.as_deref(),
            self.not_senders.as_deref(),
            event_type,
            sender,
        )
    }
}

/// The `room` section of a filter.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct RoomFilter {
    /// Rooms to include, applied to every section uniformly -- see the module docs.
    pub rooms: Option<Vec<String>>,
    /// Rooms to exclude.
    pub not_rooms: Option<Vec<String>>,
    /// Whether rooms the user has left should appear in an initial sync's room set. Applied.
    pub include_leave: Option<bool>,
    /// Timeline filter: `limit`, `rooms`/`not_rooms`, the content rule and
    /// `unread_thread_notifications` applied.
    pub timeline: Option<RoomEventFilter>,
    /// State filter: the content rule, `lazy_load_members` and `include_redundant_members`
    /// applied; `limit` parsed only.
    pub state: Option<RoomEventFilter>,
    /// Ephemeral-event filter: by type and `rooms`/`not_rooms`; `limit` parsed only.
    pub ephemeral: Option<RoomEventFilter>,
    /// Room-scoped account-data filter: by type and `rooms`/`not_rooms`; `limit` parsed only.
    pub account_data: Option<RoomEventFilter>,
}

/// A full `/sync` filter, as `POST /user/{userId}/filter` accepts and `GET /sync?filter=` names
/// (by inline JSON or by a previously uploaded filter's id). See the module docs for exactly
/// which parts of this `crate::sync` acts on.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct SyncFilter {
    /// Which event fields to include, as dotted paths (`content.body`). Applied to timeline
    /// and state events -- see the module docs and [`SyncFilter::prune_event_fields`].
    pub event_fields: Option<Vec<String>>,
    /// `"client"` (the default) or `"federation"`. Applied.
    pub event_format: Option<String>,
    /// Presence filter: `types`/`not_types`/`senders`/`not_senders` applied.
    pub presence: Option<EventFilter>,
    /// Global account-data filter: by type, with the user's own id as the sender. Applied.
    pub account_data: Option<EventFilter>,
    /// Room filter. See [`RoomFilter`].
    pub room: Option<RoomFilter>,
}

impl SyncFilter {
    /// The empty filter: every section absent, meaning "no restriction" everywhere `crate::sync`
    /// checks one.
    #[must_use]
    pub fn none() -> Self {
        Self::default()
    }

    /// Checks that every entry of a `rooms`/`not_rooms` list is a room id and every entry of a
    /// `senders`/`not_senders` list a user id, as the spec types them; serde has already checked
    /// the shapes. A filter naming `"not_a_room_id"` used to be stored and then matched nothing
    /// (Sytest's "Check creating invalid filters returns 4xx").
    ///
    /// # Errors
    /// Names the first offending entry and the list it is in.
    pub fn validate_ids(&self) -> Result<(), String> {
        fn user_ids(list: Option<&Vec<String>>, section: &str) -> Result<(), String> {
            match list
                .into_iter()
                .flatten()
                .find(|s| ruma::UserId::parse(s).is_err())
            {
                Some(bad) => Err(format!("{section}: {bad:?} is not a user id")),
                None => Ok(()),
            }
        }
        fn room_ids(list: Option<&Vec<String>>, section: &str) -> Result<(), String> {
            match list
                .into_iter()
                .flatten()
                .find(|s| ruma::RoomId::parse(s).is_err())
            {
                Some(bad) => Err(format!("{section}: {bad:?} is not a room id")),
                None => Ok(()),
            }
        }
        for (name, section) in [
            ("presence", &self.presence),
            ("account_data", &self.account_data),
        ] {
            if let Some(filter) = section {
                user_ids(filter.senders.as_ref(), &format!("{name}.senders"))?;
                user_ids(filter.not_senders.as_ref(), &format!("{name}.not_senders"))?;
            }
        }
        let Some(room) = &self.room else {
            return Ok(());
        };
        room_ids(room.rooms.as_ref(), "room.rooms")?;
        room_ids(room.not_rooms.as_ref(), "room.not_rooms")?;
        for (name, section) in [
            ("timeline", &room.timeline),
            ("state", &room.state),
            ("ephemeral", &room.ephemeral),
            ("account_data", &room.account_data),
        ] {
            if let Some(filter) = section {
                room_ids(filter.rooms.as_ref(), &format!("room.{name}.rooms"))?;
                room_ids(filter.not_rooms.as_ref(), &format!("room.{name}.not_rooms"))?;
                user_ids(filter.senders.as_ref(), &format!("room.{name}.senders"))?;
                user_ids(
                    filter.not_senders.as_ref(),
                    &format!("room.{name}.not_senders"),
                )?;
            }
        }
        Ok(())
    }

    /// The room-level allow/deny lists to apply uniformly across timeline and state (see the
    /// module docs on why they are not kept separate).
    #[must_use]
    pub fn room_allow_deny(&self) -> (Option<&[String]>, Option<&[String]>) {
        let Some(room) = &self.room else {
            return (None, None);
        };
        let allow = room
            .rooms
            .as_deref()
            .or_else(|| room.timeline.as_ref().and_then(|t| t.rooms.as_deref()));
        let deny = room
            .not_rooms
            .as_deref()
            .or_else(|| room.timeline.as_ref().and_then(|t| t.not_rooms.as_deref()));
        (allow, deny)
    }

    /// Whether `room_id` passes this filter's room allow/deny lists.
    #[must_use]
    pub fn room_allowed(&self, room_id: &str) -> bool {
        let (allow, deny) = self.room_allow_deny();
        if let Some(deny) = deny
            && deny.iter().any(|r| r == room_id)
        {
            return false;
        }
        if let Some(allow) = allow {
            return allow.iter().any(|r| r == room_id);
        }
        true
    }

    /// The timeline event limit, defaulting to `default_limit` (the spec's own default is 10;
    /// `crate::sync` passes that in explicitly rather than hard-coding it here, so a caller
    /// building a different response shape -- an initial sync's smaller default, for instance --
    /// is not forced to accept this filter's).
    #[must_use]
    pub fn timeline_limit(&self, default_limit: usize) -> usize {
        self.room
            .as_ref()
            .and_then(|r| r.timeline.as_ref())
            .and_then(|t| t.limit)
            .unwrap_or(default_limit)
    }

    /// Whether the client asked for notification counts per thread
    /// (`room.timeline.unread_thread_notifications`, as Synapse reads it).
    #[must_use]
    pub fn unread_thread_notifications(&self) -> bool {
        self.room
            .as_ref()
            .and_then(|r| r.timeline.as_ref())
            .and_then(|t| t.unread_thread_notifications)
            .unwrap_or(false)
    }

    /// Whether lazy-loading room members is requested.
    #[must_use]
    pub fn lazy_load_members(&self) -> bool {
        self.room
            .as_ref()
            .and_then(|r| r.state.as_ref())
            .and_then(|s| s.lazy_load_members)
            .unwrap_or(false)
    }

    /// Whether, under lazy loading, a member's event is to be sent again in an incremental
    /// sync although this device was sent it before (`room.state.include_redundant_members`).
    /// Meaningless without [`SyncFilter::lazy_load_members`].
    #[must_use]
    pub fn include_redundant_members(&self) -> bool {
        self.room
            .as_ref()
            .and_then(|r| r.state.as_ref())
            .and_then(|s| s.include_redundant_members)
            .unwrap_or(false)
    }

    /// Whether left rooms should appear in an initial sync's room set.
    #[must_use]
    pub fn include_leave(&self) -> bool {
        self.room
            .as_ref()
            .and_then(|r| r.include_leave)
            .unwrap_or(false)
    }

    /// Whether events are to be rendered in the federation format (`event_format:
    /// "federation"`) rather than the client one.
    #[must_use]
    pub fn federation_format(&self) -> bool {
        self.event_format.as_deref() == Some("federation")
    }

    /// The `event_fields` paths, split into their keys: `None` when the filter names none or
    /// an empty list (every field, as Synapse reads an empty list).
    #[must_use]
    pub fn event_field_paths(&self) -> Option<Vec<Vec<String>>> {
        let fields = self.event_fields.as_ref()?;
        if fields.is_empty() {
            return None;
        }
        Some(fields.iter().map(|field| split_field_path(field)).collect())
    }

    /// Prunes each of `events` to the fields `event_fields` names, in place; a no-op without
    /// the filter. The spec lets a server send more than was asked for; this one sends exactly
    /// what was asked for among the fields the event has, as Synapse does. Call it last: the
    /// batch's own bookkeeping (lazy-loaded members, device-list changes, the user's own
    /// membership) reads `type`, `state_key` and `event_id` first.
    pub fn prune_event_fields(&self, events: &mut [serde_json::Value]) {
        let Some(paths) = self.event_field_paths() else {
            return;
        };
        for event in events {
            *event = only_fields(event, &paths);
        }
    }

    /// Whether the top-level `presence` filter passes an `m.presence` event from `sender`.
    #[must_use]
    pub fn presence_allows(&self, sender: &str) -> bool {
        self.presence
            .as_ref()
            .is_none_or(|f| f.matches("m.presence", sender))
    }

    /// Whether the top-level `account_data` filter passes global account data of `event_type`.
    /// Account data has no sender; the user's own id stands for it, as Synapse does.
    #[must_use]
    pub fn account_data_allows(&self, event_type: &str, user_id: &str) -> bool {
        self.account_data
            .as_ref()
            .is_none_or(|f| f.matches(event_type, user_id))
    }

    /// Whether `room.account_data` passes room account data of `event_type` in `room_id`.
    #[must_use]
    pub fn room_account_data_allows(&self, room_id: &str, event_type: &str, user_id: &str) -> bool {
        self.room
            .as_ref()
            .and_then(|r| r.account_data.as_ref())
            .is_none_or(|f| room_section_allows(f, room_id, event_type, user_id))
    }

    /// Whether `room.ephemeral` passes an ephemeral event of `event_type` in `room_id`.
    /// Ephemeral events carry no sender of their own; the room-level rule is by type.
    #[must_use]
    pub fn ephemeral_allows(&self, room_id: &str, event_type: &str) -> bool {
        self.room
            .as_ref()
            .and_then(|r| r.ephemeral.as_ref())
            .is_none_or(|f| room_section_allows(f, room_id, event_type, ""))
    }

    /// `room.timeline`'s content filter (`types`/`not_types`/`senders`/`not_senders`), if this
    /// filter sets one. `None` (not merely a no-op [`RoomEventFilter`]) whenever `room.timeline`
    /// itself is absent, so a caller can cheaply skip the filtered code path entirely when there
    /// is nothing to filter on.
    #[must_use]
    pub fn timeline_content_filter(&self) -> Option<&RoomEventFilter> {
        self.room
            .as_ref()
            .and_then(|r| r.timeline.as_ref())
            .filter(|t| !t.is_content_noop())
    }

    /// `room.state`'s content filter, the equivalent of [`SyncFilter::timeline_content_filter`]
    /// for the `state` section.
    #[must_use]
    pub fn state_content_filter(&self) -> Option<&RoomEventFilter> {
        self.room
            .as_ref()
            .and_then(|r| r.state.as_ref())
            .filter(|s| !s.is_content_noop())
    }
}

/// Resolves `/sync`'s `filter` query parameter: absent means [`SyncFilter::none`]; a string that
/// parses as a JSON object is used inline (per the spec, `filter` accepts either a filter id or
/// inline JSON, distinguished by whether it parses as JSON); anything else is looked up as a
/// filter id via `crate::store::UserStore::get_filter`.
///
/// # Errors
/// Returns [`crate::error::UserError::InvalidFilter`] if inline JSON does not match
/// [`SyncFilter`]'s shape, or [`crate::error::UserError::UnknownFilterId`] if a filter id was
/// given but never uploaded by this user.
pub async fn resolve(
    store: &crate::store::DynUserStore,
    user_id: &ruma::UserId,
    raw: Option<&str>,
) -> Result<SyncFilter, crate::error::UserError> {
    let Some(raw) = raw else {
        return Ok(SyncFilter::none());
    };
    if raw.trim_start().starts_with('{') {
        let filter: SyncFilter = serde_json::from_str(raw)
            .map_err(|e| crate::error::UserError::InvalidFilter(e.to_string()))?;
        filter
            .validate_ids()
            .map_err(crate::error::UserError::InvalidFilter)?;
        log_ignored_fields(&filter);
        return Ok(filter);
    }
    let stored = store
        .get_filter(user_id, raw)
        .await?
        .ok_or_else(|| crate::error::UserError::UnknownFilterId(raw.to_owned()))?;
    let filter: SyncFilter = serde_json::from_value(stored)
        .map_err(|e| crate::error::UserError::InvalidFilter(e.to_string()))?;
    log_ignored_fields(&filter);
    Ok(filter)
}

/// Emits one `tracing::debug!` line naming which present-but-unapplied filter sections this
/// request asked for, so "silently ignoring a filter" (this track's brief explicitly calls out as
/// worse than rejecting one) is at minimum visible in the server's own logs even though the
/// client-server API has no protocol-level way to tell the client "this part was ignored".
fn log_ignored_fields(filter: &SyncFilter) {
    let mut ignored = Vec::new();
    if let Some(fields) = &filter.event_fields
        && !fields.is_empty()
    {
        tracing::debug!(
            event_fields = ?fields,
            "this sync's timeline and state events are pruned to the fields its filter names"
        );
    }
    if filter
        .event_format
        .as_deref()
        .is_some_and(|format| format != "client" && format != "federation")
    {
        ignored.push("event_format (neither client nor federation)");
    }
    if !ignored.is_empty() {
        tracing::debug!(?ignored, "filter fields present but not applied by hs-user");
    }
}

/// Splits one `event_fields` entry into its keys: a `.` separates keys, `\.` is a literal dot
/// and `\\` a literal backslash (the spec's escaping); any other backslash is kept as it is.
fn split_field_path(field: &str) -> Vec<String> {
    let mut keys = Vec::new();
    let mut key = String::new();
    let mut chars = field.chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' => match chars.next() {
                Some(escaped @ ('.' | '\\')) => key.push(escaped),
                Some(other) => {
                    key.push('\\');
                    key.push(other);
                }
                None => key.push('\\'),
            },
            '.' => keys.push(std::mem::take(&mut key)),
            other => key.push(other),
        }
    }
    keys.push(key);
    keys
}

/// A copy of `event` holding only the fields `paths` name. A path leads through objects to the
/// value it names, which is copied whole (`content` keeps all of `content`); a path the event
/// does not have, or that runs into a non-object, adds nothing. An event that is not an object
/// is returned as it is.
fn only_fields(event: &serde_json::Value, paths: &[Vec<String>]) -> serde_json::Value {
    let serde_json::Value::Object(source) = event else {
        return event.clone();
    };
    let mut pruned = serde_json::Map::new();
    for path in paths {
        let Some((last, parents)) = path.split_last() else {
            continue;
        };
        let mut from = source;
        let mut found = true;
        for parent in parents {
            match from.get(parent) {
                Some(serde_json::Value::Object(inner)) => from = inner,
                _ => {
                    found = false;
                    break;
                }
            }
        }
        if !found {
            continue;
        }
        let Some(value) = from.get(last) else {
            continue;
        };
        insert_at(&mut pruned, path, value.clone());
    }
    serde_json::Value::Object(pruned)
}

/// Puts `value` at `path` under `into`, making the objects on the way. Along a path only
/// objects are made, and a value an earlier, shorter path copied whole was an object in the
/// source at this key too, so a non-object on the way is not reached; if it ever were, the
/// whole value already there stands.
fn insert_at(
    into: &mut serde_json::Map<String, serde_json::Value>,
    path: &[String],
    value: serde_json::Value,
) {
    match path {
        [] => {}
        [last] => {
            into.insert(last.clone(), value);
        }
        [parent, rest @ ..] => {
            let entry = into
                .entry(parent.clone())
                .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()));
            if let serde_json::Value::Object(inner) = entry {
                insert_at(inner, rest, value);
            }
        }
    }
}

/// Whether a room section's filter passes an event of `event_type` from `sender` in `room_id`:
/// its own `rooms`/`not_rooms`, then its content rule.
fn room_section_allows(
    filter: &RoomEventFilter,
    room_id: &str,
    event_type: &str,
    sender: &str,
) -> bool {
    if filter
        .not_rooms
        .as_ref()
        .is_some_and(|rooms| rooms.iter().any(|r| r == room_id))
    {
        return false;
    }
    if filter
        .rooms
        .as_ref()
        .is_some_and(|rooms| !rooms.iter().any(|r| r == room_id))
    {
        return false;
    }
    if sender.is_empty() {
        return type_and_sender_match(
            filter.types.as_deref(),
            filter.not_types.as_deref(),
            None,
            None,
            event_type,
            sender,
        );
    }
    filter.matches(event_type, sender)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_field_paths_split_on_dots_and_honour_the_escapes() {
        assert_eq!(split_field_path("type"), vec!["type"]);
        assert_eq!(split_field_path("content.body"), vec!["content", "body"]);
        assert_eq!(
            split_field_path("content.m\\.relates_to.rel_type"),
            vec!["content", "m.relates_to", "rel_type"]
        );
        assert_eq!(split_field_path("a\\\\b.c"), vec!["a\\b", "c"]);
        assert_eq!(split_field_path("a\\nb"), vec!["a\\nb"]);
        assert_eq!(split_field_path("trailing\\"), vec!["trailing\\"]);
        assert_eq!(split_field_path(""), vec![""]);
    }

    #[test]
    fn only_fields_keeps_the_named_fields_and_skips_what_the_event_lacks() {
        let event = serde_json::json!({
            "type": "m.room.message",
            "event_id": "$e",
            "sender": "@alice:example.org",
            "content": {"body": "hi", "msgtype": "m.text", "m.relates_to": {"rel_type": "m.thread", "event_id": "$root"}},
            "unsigned": {"age": 1},
        });
        let paths: Vec<Vec<String>> = [
            "type",
            "content.body",
            "content.m\\.relates_to.rel_type",
            "content.missing",
            "origin_server_ts",
            "sender.not_an_object",
        ]
        .iter()
        .map(|p| split_field_path(p))
        .collect();
        assert_eq!(
            only_fields(&event, &paths),
            serde_json::json!({
                "type": "m.room.message",
                "content": {"body": "hi", "m.relates_to": {"rel_type": "m.thread"}},
            })
        );
        // A whole object named by one path and a field inside it by another: the object wins
        // whole, whichever order the paths come in.
        for order in [["content", "content.body"], ["content.body", "content"]] {
            let paths: Vec<Vec<String>> = order.iter().map(|p| split_field_path(p)).collect();
            assert_eq!(
                only_fields(&event, &paths)["content"],
                event["content"],
                "{order:?}"
            );
        }
    }

    #[test]
    fn prune_event_fields_is_a_no_op_without_the_filter_or_with_an_empty_list() {
        let event = serde_json::json!({"type": "m.room.message", "content": {"body": "hi"}});
        for filter in [
            SyncFilter::none(),
            serde_json::from_value(serde_json::json!({"event_fields": []})).unwrap(),
        ] {
            let mut events = vec![event.clone()];
            filter.prune_event_fields(&mut events);
            assert_eq!(events, vec![event.clone()]);
        }
        let filter: SyncFilter =
            serde_json::from_value(serde_json::json!({"event_fields": ["content.body"]})).unwrap();
        let mut events = vec![event.clone(), serde_json::json!({"type": "m.room.name"})];
        filter.prune_event_fields(&mut events);
        assert_eq!(
            events,
            vec![
                serde_json::json!({"content": {"body": "hi"}}),
                serde_json::json!({})
            ]
        );
    }

    #[test]
    fn room_event_filter_types_is_an_allowlist_with_wildcard_support() {
        let f: RoomEventFilter = serde_json::from_value(serde_json::json!({
            "types": ["m.room.message", "m.room.*"]
        }))
        .unwrap();
        assert!(!f.is_content_noop());
        assert!(f.matches("m.room.message", "@alice:example.org"));
        assert!(f.matches("m.room.topic", "@alice:example.org"));
        assert!(!f.matches("m.reaction", "@alice:example.org"));
    }

    #[test]
    fn room_event_filter_not_types_wins_over_types() {
        let f: RoomEventFilter = serde_json::from_value(serde_json::json!({
            "types": ["m.room.*"],
            "not_types": ["m.room.message"]
        }))
        .unwrap();
        assert!(!f.matches("m.room.message", "@alice:example.org"));
        assert!(f.matches("m.room.topic", "@alice:example.org"));
    }

    #[test]
    fn room_event_filter_senders_and_not_senders() {
        let f: RoomEventFilter = serde_json::from_value(serde_json::json!({
            "senders": ["@alice:example.org", "@bob:example.org"],
            "not_senders": ["@bob:example.org"]
        }))
        .unwrap();
        assert!(f.matches("m.room.message", "@alice:example.org"));
        assert!(
            !f.matches("m.room.message", "@bob:example.org"),
            "not_senders must win over senders"
        );
        assert!(!f.matches("m.room.message", "@carol:example.org"));
    }

    #[test]
    fn empty_room_event_filter_is_a_content_noop_and_matches_everything() {
        let f = RoomEventFilter::default();
        assert!(f.is_content_noop());
        assert!(f.matches("anything", "@anyone:example.org"));
    }

    #[test]
    fn timeline_and_state_content_filters_are_none_when_absent_or_a_noop() {
        let f = SyncFilter::none();
        assert!(f.timeline_content_filter().is_none());
        assert!(f.state_content_filter().is_none());

        let f: SyncFilter = serde_json::from_value(serde_json::json!({
            "room": {"timeline": {"limit": 5}, "state": {"lazy_load_members": true}}
        }))
        .unwrap();
        assert!(
            f.timeline_content_filter().is_none(),
            "limit alone is not a content filter"
        );
        assert!(
            f.state_content_filter().is_none(),
            "lazy_load_members alone is not a content filter"
        );

        let f: SyncFilter = serde_json::from_value(serde_json::json!({
            "room": {
                "timeline": {"types": ["m.room.message"]},
                "state": {"not_types": ["m.room.member"]}
            }
        }))
        .unwrap();
        assert!(f.timeline_content_filter().is_some());
        assert!(f.state_content_filter().is_some());
    }

    #[test]
    fn empty_filter_allows_every_room_and_uses_the_default_limit() {
        let f = SyncFilter::none();
        assert!(f.room_allowed("!a:example.org"));
        assert_eq!(f.timeline_limit(10), 10);
        assert!(!f.lazy_load_members());
        assert!(!f.include_leave());
    }

    /// Sytest's "Check creating invalid filters returns 4xx": a room list holding something
    /// that is not a room id, or a sender list something that is not a user id, is rejected;
    /// well-formed ids pass, wherever the list is.
    #[test]
    fn room_and_sender_lists_must_hold_ids() {
        let parse = |v: serde_json::Value| serde_json::from_value::<SyncFilter>(v).unwrap();
        assert!(
            parse(serde_json::json!({"room": {"timeline": {"rooms": ["not_a_room_id"]}}}))
                .validate_ids()
                .is_err()
        );
        assert!(
            parse(serde_json::json!({"room": {"state": {"senders": ["not_a_sender_id"]}}}))
                .validate_ids()
                .is_err()
        );
        assert!(
            parse(serde_json::json!({"room": {"not_rooms": ["nope"]}}))
                .validate_ids()
                .is_err()
        );
        assert!(
            parse(serde_json::json!({"presence": {"not_senders": ["@alice"]}}))
                .validate_ids()
                .is_err()
        );
        parse(serde_json::json!({
            "presence": {"senders": ["@alice:example.org"]},
            "room": {
                "rooms": ["!room:example.org"],
                "timeline": {"not_senders": ["@bob:example.org"], "rooms": ["!r:example.org"]},
                "ephemeral": {"senders": ["@carol:example.org"]}
            }
        }))
        .validate_ids()
        .unwrap();
        SyncFilter::none().validate_ids().unwrap();
    }

    #[test]
    fn room_rooms_is_an_allowlist() {
        let f: SyncFilter = serde_json::from_value(serde_json::json!({
            "room": {"rooms": ["!a:example.org"]}
        }))
        .unwrap();
        assert!(f.room_allowed("!a:example.org"));
        assert!(!f.room_allowed("!b:example.org"));
    }

    #[test]
    fn room_not_rooms_is_a_denylist_that_wins_over_the_allowlist() {
        let f: SyncFilter = serde_json::from_value(serde_json::json!({
            "room": {"rooms": ["!a:example.org"], "not_rooms": ["!a:example.org"]}
        }))
        .unwrap();
        assert!(!f.room_allowed("!a:example.org"));
    }

    #[test]
    fn timeline_limit_is_read_from_the_room_timeline_section() {
        let f: SyncFilter = serde_json::from_value(serde_json::json!({
            "room": {"timeline": {"limit": 3}}
        }))
        .unwrap();
        assert_eq!(f.timeline_limit(10), 3);
    }

    #[test]
    fn unrecognized_fields_do_not_fail_parsing() {
        let f: Result<SyncFilter, _> = serde_json::from_value(serde_json::json!({
            "room": {"some_future_msc_field": true},
            "another_unknown_top_level_field": 42
        }));
        assert!(
            f.is_ok(),
            "an unrecognized filter field must not be rejected"
        );
    }

    #[tokio::test]
    async fn resolve_treats_a_json_object_as_inline() {
        let store: crate::store::DynUserStore = std::sync::Arc::new(
            crate::store::tables::TablesUserStore::open(hs_kv::memory::MemoryBackend::new())
                .unwrap(),
        );
        let uid = ruma::user_id!("@alice:example.org");
        let f = resolve(&store, uid, Some(r#"{"room":{"timeline":{"limit":2}}}"#))
            .await
            .unwrap();
        assert_eq!(f.timeline_limit(10), 2);
    }

    #[tokio::test]
    async fn resolve_treats_a_bare_string_as_a_filter_id() {
        let store: crate::store::DynUserStore = std::sync::Arc::new(
            crate::store::tables::TablesUserStore::open(hs_kv::memory::MemoryBackend::new())
                .unwrap(),
        );
        let uid = ruma::user_id!("@alice:example.org");
        let id = store
            .put_filter(uid, serde_json::json!({"room": {"timeline": {"limit": 7}}}))
            .await
            .unwrap();
        let f = resolve(&store, uid, Some(&id)).await.unwrap();
        assert_eq!(f.timeline_limit(10), 7);

        let err = resolve(&store, uid, Some("never-uploaded")).await;
        assert!(matches!(
            err,
            Err(crate::error::UserError::UnknownFilterId(_))
        ));
    }
}
