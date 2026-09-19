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
//!   senders of events already in the returned `timeline`, plus (always) the requesting user's
//!   own membership event -- the spec's minimum lazy-loading contract.
//! - `room.include_leave`: when true, left rooms are included in an initial sync's room set (an
//!   incremental sync always includes a room the user just left, regardless of this flag, since
//!   the client needs to see the leave event itself).
//!
//! **Parsed but ignored** (present in [`SyncFilter`]'s fields so a client's filter round-trips
//! and is never rejected, but `crate::sync` does not act on it):
//! - `event_fields`, `event_format`: no field-pruning or federation-format rendering is
//!   implemented; every event is always rendered in full client format
//!   (`hs_room::routes::render::client_event_json`).
//! - `presence`, `account_data` (top-level, i.e. the *global* account-data filter): global account
//!   data is always returned in full, unfiltered; the top-level `presence` filter is unused since
//!   `presence.events` is always built from shared-room scope, not filtered further.
//! - `room.account_data`, `room.ephemeral`: room-scoped account data and ephemeral events
//!   (`m.typing`, `m.receipt`) are always returned in full, not content-filtered -- both are
//!   already small, bounded sets (one typing/receipt snapshot per room, not a history), so this
//!   crate does not apply `EventFilter`/`RoomEventFilter`'s `types`/`senders` restrictions to
//!   them.
//! - `room.state.include_redundant_members`: lazy loading here always includes only the minimal
//!   set (never redundant members), so this flag has no effect either way.
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
    /// Event types to include (a `*` suffix wildcard is spec-legal; this crate does not expand
    /// it, since content-type filtering is not applied at all -- see the module docs).
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
    /// Whether a lazy-loaded response should still include a member's redundant re-appearance.
    /// Parsed, not applied (this crate never sends redundant members either way).
    pub include_redundant_members: Option<bool>,
    /// Whether to include events with a relation to another event that the filter would
    /// otherwise exclude. Parsed, not applied.
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
        if let Some(not_types) = &self.not_types
            && not_types
                .iter()
                .any(|t| type_pattern_matches(t, event_type))
        {
            return false;
        }
        if let Some(not_senders) = &self.not_senders
            && not_senders.iter().any(|s| s == sender)
        {
            return false;
        }
        if let Some(types) = &self.types
            && !types.iter().any(|t| type_pattern_matches(t, event_type))
        {
            return false;
        }
        if let Some(senders) = &self.senders
            && !senders.iter().any(|s| s == sender)
        {
            return false;
        }
        true
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
    /// Timeline filter. `limit`, `rooms`/`not_rooms` and (via `RoomEventFilter`) nothing else
    /// applied.
    pub timeline: Option<RoomEventFilter>,
    /// State filter. `lazy_load_members` applied; everything else parsed only.
    pub state: Option<RoomEventFilter>,
    /// Ephemeral-event filter. Parsed only (`crate::sync` sends no ephemeral events at all yet).
    pub ephemeral: Option<RoomEventFilter>,
    /// Room-scoped account-data filter. Parsed only.
    pub account_data: Option<RoomEventFilter>,
}

/// A full `/sync` filter, as `POST /user/{userId}/filter` accepts and `GET /sync?filter=` names
/// (by inline JSON or by a previously uploaded filter's id). See the module docs for exactly
/// which parts of this `crate::sync` acts on.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct SyncFilter {
    /// Which event fields to include. Parsed only.
    pub event_fields: Option<Vec<String>>,
    /// `"client"` or `"federation"`. Parsed only (this crate always renders client format).
    pub event_format: Option<String>,
    /// Presence filter. Parsed only (presence is not implemented in this crate yet).
    pub presence: Option<EventFilter>,
    /// Global account-data filter. Parsed only.
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

    /// Whether lazy-loading room members is requested.
    #[must_use]
    pub fn lazy_load_members(&self) -> bool {
        self.room
            .as_ref()
            .and_then(|r| r.state.as_ref())
            .and_then(|s| s.lazy_load_members)
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
    if filter.event_fields.is_some() {
        ignored.push("event_fields");
    }
    if filter.event_format.is_some() {
        ignored.push("event_format");
    }
    if filter.presence.is_some() {
        ignored.push("presence");
    }
    if filter.account_data.is_some() {
        ignored.push("account_data (global)");
    }
    if let Some(room) = &filter.room {
        if room.ephemeral.is_some() {
            ignored.push("room.ephemeral");
        }
        if room.account_data.is_some() {
            ignored.push("room.account_data");
        }
        if room
            .state
            .as_ref()
            .is_some_and(|s| s.include_redundant_members.is_some())
        {
            ignored.push("room.state.include_redundant_members");
        }
    }
    if !ignored.is_empty() {
        tracing::debug!(?ignored, "filter fields present but not applied by hs-user");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
