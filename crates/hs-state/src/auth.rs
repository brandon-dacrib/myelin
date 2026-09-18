//! Event authorization for room versions 1 to 12.
//!
//! Re-expressed directly from the "Authorization rules" section of the Matrix server-server
//! specification (`refs/matrix-spec/content/server-server-api.md#authorisation-rules`,
//! Apache-2.0), parameterized by [`hs_model::room_version::RoomVersionRules`] so one
//! implementation covers every room version rather than twelve near-duplicates. Cross-checked in
//! this crate's tests against `ruma_state_res::event_auth` (MIT license, Ruma project) on randomly
//! generated rooms and candidate events (`docs/workstreams/02-state-and-model.md`, "Event auth for
//! versions 1 to 12 ... cross-checked against `ruma-state-res`'s auth implementation on random
//! events").
//!
//! # The two-phase split
//!
//! The spec's authorization rules naturally split into two phases, which this module keeps
//! separate because callers run them at different times and against different inputs
//! (`hs-room`'s ingestion pipeline, mirroring the checks-on-receipt-of-a-PDU list in the
//! server-server spec):
//!
//! - [`check_auth_events_selection`]: state-*independent*. Given only the incoming event and the
//!   `(type, state_key)` of each event it names in `auth_events`, checks that the selection is
//!   exactly what the [auth events selection algorithm] would have produced, with no duplicates
//!   and no already-rejected events. Run once per event, right after signature verification.
//! - [`check_event_auth`]: state-*dependent*. Given the incoming event and a [`StateFetch`] over
//!   some state snapshot, checks the rest of the rules (membership transitions, power levels, the
//!   generic power-level gate). The spec requires this to be run three times per event, against
//!   three different snapshots (the state implied by `auth_events`, the state before the event,
//!   and the room's current state at receipt time; a failure against the third snapshot alone is a
//!   soft failure, not a rejection) -- `hs-room` owns picking those three snapshots and
//!   interpreting which failures are hard rejections versus soft failures.
//!
//! `m.room.create` is a special case in both phases (it has no `auth_events` and no
//! state-dependent rules) and is handled by [`check_room_create`] for the state-independent side;
//! [`check_event_auth`] returns `Ok(())` immediately for it.
//!
//! [auth events selection algorithm]: https://spec.matrix.org/v1.19/server-server-api/#auth-events-selection

use std::collections::{BTreeMap, BTreeSet, HashSet};

use ed25519_dalek::Verifier as _;
use hs_model::canonical::{CanonicalJsonObject, CanonicalJsonValue};
use hs_model::power_levels::PowerLevels;
use hs_model::room_version::RoomVersionRules;
use ruma::{EventId, OwnedUserId, RoomId, UserId};

use crate::error::{AuthError, AuthResult};
use crate::state_fetch::{StateEntry, StateFetch};

/// The event under authorization, plus the small amount of graph context the checks need
/// (`prev_events` shape) that a `StateFetch` cannot provide.
#[derive(Debug, Clone, Copy)]
pub struct IncomingEvent<'a> {
    /// The event's `type`.
    pub event_type: &'a str,
    /// The event's `sender`.
    pub sender: &'a UserId,
    /// The event's `room_id`, if present (absent for a room version 12+ `m.room.create`).
    pub room_id: Option<&'a RoomId>,
    /// The event's `state_key`, if it is a state event.
    pub state_key: Option<&'a str>,
    /// The event's `content`.
    pub content: &'a CanonicalJsonObject,
    /// How many entries `prev_events` has. Only `m.room.create`'s check
    /// (["cannot have previous events"]) needs the count rather than the identity.
    pub prev_event_count: usize,
    /// Whether `prev_events` is exactly `[create_event_id]` -- the one shape that lets a
    /// `m.room.member` join event with no other authorization succeed (the room creator's own
    /// first join). The caller (which holds the actual DAG) computes this.
    pub only_prev_event_is_room_create: bool,
    /// The event's own ID, if known. Only used by the pre-v3 `m.room.redaction` special case.
    pub event_id: Option<&'a EventId>,
    /// The event ID in `redacts` (for an `m.room.redaction` event), if any. Only used by the
    /// pre-v3 `m.room.redaction` special case.
    pub redacts: Option<&'a EventId>,
}

impl<'a> IncomingEvent<'a> {
    /// Builds an `IncomingEvent` with the graph-context fields left at their most common values
    /// (some `prev_events`, not the create-only shape, no `event_id`/`redacts`). Use the public
    /// fields directly to override.
    #[must_use]
    pub fn new(
        event_type: &'a str,
        sender: &'a UserId,
        room_id: Option<&'a RoomId>,
        state_key: Option<&'a str>,
        content: &'a CanonicalJsonObject,
    ) -> Self {
        Self {
            event_type,
            sender,
            room_id,
            state_key,
            content,
            prev_event_count: 1,
            only_prev_event_is_room_create: false,
            event_id: None,
            redacts: None,
        }
    }
}

/// One entry in the incoming event's `auth_events`, as the caller (which fetched the events)
/// already knows it.
#[derive(Debug, Clone, Copy)]
pub struct AuthEventRef<'a> {
    /// The referenced event's `type`.
    pub event_type: &'a str,
    /// The referenced event's `state_key`.
    pub state_key: &'a str,
    /// Whether the referenced event was itself rejected.
    pub rejected: bool,
}

/// The membership states the spec defines. An unrecognized `membership` value is not representable
/// here; parsing it is an [`AuthError`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Membership {
    /// `join`.
    Join,
    /// `invite`.
    Invite,
    /// `leave`.
    Leave,
    /// `ban`.
    Ban,
    /// `knock`.
    Knock,
}

impl Membership {
    fn from_content(content: &CanonicalJsonObject) -> Result<Self, AuthError> {
        match content
            .get("membership")
            .and_then(CanonicalJsonValue::as_str)
        {
            Some("join") => Ok(Self::Join),
            Some("invite") => Ok(Self::Invite),
            Some("leave") => Ok(Self::Leave),
            Some("ban") => Ok(Self::Ban),
            Some("knock") => Ok(Self::Knock),
            Some(other) => Err(AuthError::reject(format!("unknown membership {other:?}"))),
            None => Err(AuthError::reject(
                "missing or invalid membership field in m.room.member event",
            )),
        }
    }
}

/// The join rules the spec defines. An unrecognized `join_rule` value parses to [`Self::Other`]
/// (it is not an error by itself: the spec defines specific behavior for each *known* rule and
/// falls through to "otherwise, reject" for anything else, so the unrecognized case is handled at
/// the point of use, not at parse time).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JoinRule {
    /// `public`.
    Public,
    /// `invite`.
    Invite,
    /// `knock`.
    Knock,
    /// `restricted`.
    Restricted,
    /// `knock_restricted`.
    KnockRestricted,
    /// `private`, or any value this table does not recognize.
    Other,
}

impl JoinRule {
    fn from_content(content: &CanonicalJsonObject) -> Result<Self, AuthError> {
        match content
            .get("join_rule")
            .and_then(CanonicalJsonValue::as_str)
        {
            Some("public") => Ok(Self::Public),
            Some("invite") => Ok(Self::Invite),
            Some("knock") => Ok(Self::Knock),
            Some("restricted") => Ok(Self::Restricted),
            Some("knock_restricted") => Ok(Self::KnockRestricted),
            Some(_) => Ok(Self::Other),
            None => Err(AuthError::reject(
                "missing or invalid join_rule field in m.room.join_rules event",
            )),
        }
    }
}

/// Which scalar `m.room.power_levels` field to read; see [`power_level_field_or_default`].
#[derive(Debug, Clone, Copy)]
enum PowerLevelField {
    Ban,
    Kick,
    Redact,
    Invite,
}

// ---------------------------------------------------------------------------------------------
// Phase 1: state-independent (auth_events selection, and m.room.create)
// ---------------------------------------------------------------------------------------------

/// The [relevant auth events] for an event: the `(event_type, state_key)` pairs its `auth_events`
/// must be exactly (no more, no fewer, no duplicates).
///
/// # Errors
/// Returns [`AuthError`] if `event` is an `m.room.member` event with no `state_key`, an
/// unparsable `membership`, or a malformed `third_party_invite` / `join_authorised_via_users_server`
/// field -- these are all cases where the selection itself cannot be computed, which the spec
/// treats as equivalent to selecting the wrong auth events.
///
/// [relevant auth events]: https://spec.matrix.org/v1.19/server-server-api/#auth-events-selection
pub fn expected_auth_types(
    event: &IncomingEvent<'_>,
    rules: &RoomVersionRules,
) -> Result<Vec<(String, String)>, AuthError> {
    if event.event_type == "m.room.create" {
        return Ok(Vec::new());
    }

    let mut types: Vec<(String, String)> = vec![
        ("m.room.power_levels".to_owned(), String::new()),
        ("m.room.member".to_owned(), event.sender.to_string()),
    ];
    if !rules.room_create_event_id_as_room_id {
        types.push(("m.room.create".to_owned(), String::new()));
    }

    if event.event_type == "m.room.member" {
        let state_key = event
            .state_key
            .ok_or_else(|| AuthError::reject("missing state_key for m.room.member event"))?;
        push_unique(&mut types, "m.room.member", state_key);

        let membership = Membership::from_content(event.content)?;
        if matches!(
            membership,
            Membership::Join | Membership::Invite | Membership::Knock
        ) {
            push_unique(&mut types, "m.room.join_rules", "");
        }
        if membership == Membership::Invite
            && let Some(token) = third_party_invite_token(event.content)?
        {
            push_unique(&mut types, "m.room.third_party_invite", &token);
        }
        if membership == Membership::Join
            && rules.restricted_join_rule
            && let Some(via) = join_authorised_via_users_server(event.content)?
        {
            push_unique(&mut types, "m.room.member", via.as_str());
        }
    }

    Ok(types)
}

fn push_unique(types: &mut Vec<(String, String)>, event_type: &str, state_key: &str) {
    let pair = (event_type.to_owned(), state_key.to_owned());
    if !types.contains(&pair) {
        types.push(pair);
    }
}

/// Checks the state-independent authorization rules: for `m.room.create`, the rules in
/// [`check_room_create`]; for everything else, that `auth_events` is exactly the expected
/// selection, with no duplicates and no rejected events, and (for room versions 1 to 11) that an
/// `m.room.create` event is among them.
///
/// `room_create_lookup` is called only for room version 12 and later (where the room ID *is* the
/// create event's ID rather than being referenced through `auth_events`): it should look up the
/// event named by [`IncomingEvent::room_id`] and return `Ok(true)` if it exists and was not
/// rejected.
///
/// # Errors
/// Returns [`AuthError`] describing the first check that failed.
pub fn check_auth_events_selection(
    rules: &RoomVersionRules,
    event: &IncomingEvent<'_>,
    auth_events: &[AuthEventRef<'_>],
    room_create_lookup: impl FnOnce() -> Result<bool, AuthError>,
) -> AuthResult {
    if event.event_type == "m.room.create" {
        return check_room_create(event, rules);
    }

    let expected: HashSet<(String, String)> =
        expected_auth_types(event, rules)?.into_iter().collect();
    let mut seen: HashSet<(String, String)> = HashSet::with_capacity(expected.len());

    for auth_event in auth_events {
        let key = (
            auth_event.event_type.to_owned(),
            auth_event.state_key.to_owned(),
        );
        if seen.contains(&key) {
            return Err(AuthError::reject(format!(
                "duplicate auth event for ({}, {})",
                auth_event.event_type, auth_event.state_key
            )));
        }
        if !expected.contains(&key) {
            return Err(AuthError::reject(format!(
                "unexpected auth event ({}, {}) not in the expected selection",
                auth_event.event_type, auth_event.state_key
            )));
        }
        if auth_event.rejected {
            return Err(AuthError::reject(format!(
                "auth event ({}, {}) was itself rejected",
                auth_event.event_type, auth_event.state_key
            )));
        }
        seen.insert(key);
    }

    if rules.room_create_event_id_as_room_id {
        let found = room_create_lookup()?;
        if !found {
            return Err(AuthError::reject(
                "m.room.create event for this room id was not found, or was rejected",
            ));
        }
    } else if !seen
        .iter()
        .any(|(event_type, _)| event_type == "m.room.create")
    {
        return Err(AuthError::reject("no m.room.create event in auth events"));
    }

    Ok(())
}

/// Checks the `m.room.create` authorization rules.
///
/// This does **not** check that `content.room_version` is a recognized version: by construction,
/// the caller already resolved `rules` from that field (see
/// [`hs_model::room_version::rules_for`]), so an unrecognized version never reaches this function.
///
/// # Errors
/// Returns [`AuthError`] describing the first check that failed.
pub fn check_room_create(event: &IncomingEvent<'_>, rules: &RoomVersionRules) -> AuthResult {
    if event.prev_event_count > 0 {
        return Err(AuthError::reject(
            "m.room.create event cannot have previous events",
        ));
    }

    if rules.room_create_event_id_as_room_id {
        if event.room_id.is_some() {
            return Err(AuthError::reject(
                "m.room.create event cannot have a room_id field",
            ));
        }
    } else {
        let room_id = event
            .room_id
            .ok_or_else(|| AuthError::reject("missing room_id field in m.room.create event"))?;
        let room_id_server = room_id
            .server_name()
            .ok_or_else(|| AuthError::reject("invalid room_id: could not parse server name"))?;
        if room_id_server != event.sender.server_name() {
            return Err(AuthError::reject(
                "room_id server name does not match sender's server name",
            ));
        }
    }

    if !rules.use_room_create_sender {
        let has_creator = matches!(
            event.content.get("creator"),
            Some(CanonicalJsonValue::String(_))
        );
        if !has_creator {
            return Err(AuthError::reject(
                "missing creator field in m.room.create event",
            ));
        }
    }

    if rules.additional_room_creators {
        validate_additional_creators(event.content)?;
    }

    Ok(())
}

fn validate_additional_creators(content: &CanonicalJsonObject) -> AuthResult {
    let Some(value) = content.get("additional_creators") else {
        return Ok(());
    };
    let items = value
        .as_array()
        .ok_or_else(|| AuthError::reject("additional_creators must be an array"))?;
    for item in items {
        let s = item
            .as_str()
            .ok_or_else(|| AuthError::reject("additional_creators entries must be strings"))?;
        UserId::parse(s).map_err(|_| {
            AuthError::reject(format!(
                "additional_creators entry {s:?} is not a valid user ID"
            ))
        })?;
    }
    Ok(())
}

fn third_party_invite_token(content: &CanonicalJsonObject) -> Result<Option<String>, AuthError> {
    let Some(tpi) = content.get("third_party_invite") else {
        return Ok(None);
    };
    let tpi_obj = tpi
        .as_object()
        .ok_or_else(|| AuthError::reject("third_party_invite must be an object"))?;
    let signed = tpi_obj
        .get("signed")
        .and_then(CanonicalJsonValue::as_object)
        .ok_or_else(|| AuthError::reject("third_party_invite.signed must be an object"))?;
    let token = signed
        .get("token")
        .and_then(CanonicalJsonValue::as_str)
        .ok_or_else(|| AuthError::reject("third_party_invite.signed.token must be a string"))?;
    Ok(Some(token.to_owned()))
}

fn join_authorised_via_users_server(
    content: &CanonicalJsonObject,
) -> Result<Option<OwnedUserId>, AuthError> {
    match content.get("join_authorised_via_users_server") {
        None => Ok(None),
        Some(CanonicalJsonValue::String(s)) => UserId::parse(s)
            .map(Some)
            .map_err(|_| AuthError::reject("invalid join_authorised_via_users_server")),
        Some(_) => Err(AuthError::reject(
            "join_authorised_via_users_server must be a string",
        )),
    }
}

// ---------------------------------------------------------------------------------------------
// Phase 2: state-dependent
// ---------------------------------------------------------------------------------------------

/// Checks the state-dependent authorization rules against a state snapshot.
///
/// For `m.room.create` this is always `Ok(())` (there are no state-dependent rules for it; see
/// [`check_room_create`] instead). For everything else, this requires an `m.room.create` entry in
/// `state` -- its absence is itself a rejection, since every other event in a room is causally
/// after the create event.
///
/// # Errors
/// Returns [`AuthError`] describing the first check that failed.
pub fn check_event_auth(
    rules: &RoomVersionRules,
    event: &IncomingEvent<'_>,
    state: &impl StateFetch,
) -> AuthResult {
    if event.event_type == "m.room.create" {
        return Ok(());
    }

    let create = state
        .create()
        .ok_or_else(|| AuthError::reject("no m.room.create event in room state"))?;

    if !room_is_federatable(create.content)
        && create.sender.server_name() != event.sender.server_name()
    {
        return Err(AuthError::reject(
            "room is not federated and event's sender domain does not match \
             m.room.create event's sender domain",
        ));
    }

    if rules.special_case_room_aliases && event.event_type == "m.room.aliases" {
        return if event.state_key == Some(event.sender.server_name().as_str()) {
            Ok(())
        } else {
            Err(AuthError::reject(
                "server name of the state_key of m.room.aliases event does not match sender",
            ))
        };
    }

    if event.event_type == "m.room.member" {
        return check_room_member(rules, event, create, state);
    }

    let sender_membership = user_membership(state, event.sender)?;
    if sender_membership != Membership::Join {
        return Err(AuthError::reject("sender's membership is not join"));
    }

    let creators = creators_of(create, rules)?;
    let sender_power = effective_power_level(state, rules, &creators, event.sender)?;

    if event.event_type == "m.room.third_party_invite" {
        let invite_power = power_level_field_or_default(state, rules, PowerLevelField::Invite)?;
        return if sender_power >= invite_power {
            Ok(())
        } else {
            Err(AuthError::reject(
                "sender does not have enough power to send invites in this room",
            ))
        };
    }

    let required = event_power_level(state, rules, event.event_type, event.state_key)?;
    if sender_power < required {
        return Err(AuthError::reject(format!(
            "sender does not have enough power to send event of type {}",
            event.event_type
        )));
    }

    if let Some(state_key) = event.state_key
        && state_key.starts_with('@')
        && state_key != event.sender.as_str()
    {
        return Err(AuthError::reject(
            "sender cannot send event with state_key matching another user's ID",
        ));
    }

    if event.event_type == "m.room.power_levels" {
        return check_room_power_levels(rules, event, state, sender_power, &creators);
    }

    if rules.special_case_room_redaction && event.event_type == "m.room.redaction" {
        return check_room_redaction(rules, event, sender_power, state);
    }

    Ok(())
}

fn room_is_federatable(content: &CanonicalJsonObject) -> bool {
    !matches!(
        content.get("m.federate"),
        Some(CanonicalJsonValue::Bool(false))
    )
}

fn creators_of(
    create: StateEntry<'_>,
    rules: &RoomVersionRules,
) -> Result<Vec<OwnedUserId>, AuthError> {
    let mut out = Vec::new();
    if rules.use_room_create_sender {
        out.push(create.sender.to_owned());
    } else {
        let creator = create
            .content
            .get("creator")
            .and_then(CanonicalJsonValue::as_str)
            .ok_or_else(|| AuthError::reject("m.room.create missing creator field"))?;
        out.push(
            UserId::parse(creator)
                .map_err(|_| AuthError::reject("invalid creator field in m.room.create"))?,
        );
    }
    if rules.additional_room_creators
        && let Some(CanonicalJsonValue::Array(items)) = create.content.get("additional_creators")
    {
        for item in items {
            let s = item
                .as_str()
                .ok_or_else(|| AuthError::reject("additional_creators entries must be strings"))?;
            out.push(
                UserId::parse(s)
                    .map_err(|_| AuthError::reject("invalid additional_creators entry"))?,
            );
        }
    }
    Ok(out)
}

fn user_membership(state: &impl StateFetch, user: &UserId) -> Result<Membership, AuthError> {
    match state.member(user) {
        None => Ok(Membership::Leave),
        Some(entry) => Membership::from_content(entry.content),
    }
}

fn current_join_rule(state: &impl StateFetch) -> Result<JoinRule, AuthError> {
    let entry = state
        .join_rules()
        .ok_or_else(|| AuthError::reject("no m.room.join_rules event in current state"))?;
    JoinRule::from_content(entry.content)
}

/// A user's effective power level: `i64::MAX` if they are a privileged creator (room version 12+),
/// otherwise the `m.room.power_levels` value, or -- if there is no `m.room.power_levels` event at
/// all yet -- 100 for a creator and 0 for anyone else (the spec's default before the room's first
/// power-levels event).
fn effective_power_level(
    state: &impl StateFetch,
    rules: &RoomVersionRules,
    creators: &[OwnedUserId],
    user: &UserId,
) -> Result<i64, AuthError> {
    let is_creator = creators.iter().any(|c| AsRef::<UserId>::as_ref(c) == user);
    if rules.explicitly_privilege_room_creators && is_creator {
        return Ok(i64::MAX);
    }
    match state.power_levels() {
        Some(entry) => {
            let pl = PowerLevels::parse(entry.content, rules).map_err(|e| {
                AuthError::reject(format!("invalid m.room.power_levels content: {e}"))
            })?;
            Ok(pl.user_power(user))
        }
        None => Ok(if is_creator {
            100
        } else {
            hs_model::power_levels::defaults::MEMBER
        }),
    }
}

fn power_level_field_or_default(
    state: &impl StateFetch,
    rules: &RoomVersionRules,
    field: PowerLevelField,
) -> Result<i64, AuthError> {
    let pl = match state.power_levels() {
        Some(entry) => PowerLevels::parse(entry.content, rules)
            .map_err(|e| AuthError::reject(format!("invalid m.room.power_levels content: {e}")))?,
        None => PowerLevels::default(),
    };
    Ok(match field {
        PowerLevelField::Ban => pl.ban,
        PowerLevelField::Kick => pl.kick,
        PowerLevelField::Redact => pl.redact,
        PowerLevelField::Invite => pl.invite,
    })
}

fn event_power_level(
    state: &impl StateFetch,
    rules: &RoomVersionRules,
    event_type: &str,
    state_key: Option<&str>,
) -> Result<i64, AuthError> {
    let pl = match state.power_levels() {
        Some(entry) => PowerLevels::parse(entry.content, rules)
            .map_err(|e| AuthError::reject(format!("invalid m.room.power_levels content: {e}")))?,
        None => PowerLevels::default(),
    };
    Ok(pl.required_power(event_type, state_key.is_some()))
}

// --- m.room.member ---

fn check_room_member(
    rules: &RoomVersionRules,
    event: &IncomingEvent<'_>,
    create: StateEntry<'_>,
    state: &impl StateFetch,
) -> AuthResult {
    let state_key = event
        .state_key
        .ok_or_else(|| AuthError::reject("missing state_key in m.room.member event"))?;
    let target = UserId::parse(state_key)
        .map_err(|_| AuthError::reject("invalid state_key in m.room.member event"))?;
    let membership = Membership::from_content(event.content)?;

    match membership {
        Membership::Join => check_member_join(rules, event, &target, create, state),
        Membership::Invite => check_member_invite(rules, event, &target, create, state),
        Membership::Leave => check_member_leave(rules, event, &target, create, state),
        Membership::Ban => check_member_ban(rules, event, &target, create, state),
        Membership::Knock if rules.knocking => check_member_knock(rules, event, &target, state),
        Membership::Knock => Err(AuthError::reject(
            "knocking is not supported by this room version",
        )),
    }
}

fn check_member_join(
    rules: &RoomVersionRules,
    event: &IncomingEvent<'_>,
    target: &UserId,
    create: StateEntry<'_>,
    state: &impl StateFetch,
) -> AuthResult {
    let creators = creators_of(create, rules)?;

    if event.only_prev_event_is_room_create {
        let allowed = if rules.use_room_create_sender {
            create.sender == target
        } else {
            creators
                .first()
                .is_some_and(|c| AsRef::<UserId>::as_ref(c) == target)
        };
        if allowed {
            return Ok(());
        }
    }

    if event.sender != target {
        return Err(AuthError::reject(
            "sender of join event must match target user",
        ));
    }

    let current = user_membership(state, target)?;
    if current == Membership::Ban {
        return Err(AuthError::reject("banned user cannot join room"));
    }

    let join_rule = current_join_rule(state)?;

    if (join_rule == JoinRule::Invite || (rules.knocking && join_rule == JoinRule::Knock))
        && matches!(current, Membership::Invite | Membership::Join)
    {
        return Ok(());
    }

    if (rules.restricted_join_rule && join_rule == JoinRule::Restricted)
        || (rules.knock_restricted_join_rule && join_rule == JoinRule::KnockRestricted)
    {
        if matches!(current, Membership::Join | Membership::Invite) {
            return Ok(());
        }
        let via = join_authorised_via_users_server(event.content)?.ok_or_else(|| {
            AuthError::reject(
                "cannot join restricted room without join_authorised_via_users_server \
                 if not invited",
            )
        })?;
        let via_membership = user_membership(state, &via)?;
        if via_membership != Membership::Join {
            return Err(AuthError::reject(
                "join_authorised_via_users_server is not joined",
            ));
        }
        let via_power = effective_power_level(state, rules, &creators, &via)?;
        let invite_power = power_level_field_or_default(state, rules, PowerLevelField::Invite)?;
        return if via_power >= invite_power {
            Ok(())
        } else {
            Err(AuthError::reject(
                "join_authorised_via_users_server does not have enough power",
            ))
        };
    }

    if join_rule == JoinRule::Public {
        Ok(())
    } else {
        Err(AuthError::reject("cannot join a room that is not public"))
    }
}

fn check_member_invite(
    rules: &RoomVersionRules,
    event: &IncomingEvent<'_>,
    target: &UserId,
    create: StateEntry<'_>,
    state: &impl StateFetch,
) -> AuthResult {
    if let Some(token) = third_party_invite_token(event.content)? {
        return check_third_party_invite(event, target, &token, state);
    }

    let sender_membership = user_membership(state, event.sender)?;
    if sender_membership != Membership::Join {
        return Err(AuthError::reject(
            "cannot invite user if sender is not joined",
        ));
    }
    let current_target = user_membership(state, target)?;
    if matches!(current_target, Membership::Join | Membership::Ban) {
        return Err(AuthError::reject(
            "cannot invite user that is joined or banned",
        ));
    }

    let creators = creators_of(create, rules)?;
    let sender_power = effective_power_level(state, rules, &creators, event.sender)?;
    let invite_power = power_level_field_or_default(state, rules, PowerLevelField::Invite)?;
    if sender_power >= invite_power {
        Ok(())
    } else {
        Err(AuthError::reject(
            "sender does not have enough power to invite",
        ))
    }
}

fn check_third_party_invite(
    event: &IncomingEvent<'_>,
    target: &UserId,
    token: &str,
    state: &impl StateFetch,
) -> AuthResult {
    let current_target = user_membership(state, target)?;
    if current_target == Membership::Ban {
        return Err(AuthError::reject("cannot invite user that is banned"));
    }

    let signed = event
        .content
        .get("third_party_invite")
        .and_then(CanonicalJsonValue::as_object)
        .and_then(|o| o.get("signed"))
        .and_then(CanonicalJsonValue::as_object)
        .ok_or_else(|| AuthError::reject("missing third_party_invite.signed"))?;
    let mxid = signed
        .get("mxid")
        .and_then(CanonicalJsonValue::as_str)
        .ok_or_else(|| AuthError::reject("missing third_party_invite.signed.mxid"))?;
    if mxid != target.as_str() {
        return Err(AuthError::reject(
            "third-party invite mxid does not match target user",
        ));
    }

    let room_tpi = state.third_party_invite(token).ok_or_else(|| {
        AuthError::reject("no m.room.third_party_invite in room state matches the token")
    })?;
    if room_tpi.sender != event.sender {
        return Err(AuthError::reject(
            "sender of m.room.third_party_invite does not match sender of m.room.member",
        ));
    }

    let public_keys = third_party_invite_public_keys(room_tpi.content)?;
    verify_third_party_signature(signed, &public_keys)
}

fn third_party_invite_public_keys(
    content: &CanonicalJsonObject,
) -> Result<Vec<Vec<u8>>, AuthError> {
    let mut keys = Vec::new();
    if let Some(CanonicalJsonValue::String(s)) = content.get("public_key") {
        keys.push(decode_base64_lenient(s)?);
    }
    if let Some(CanonicalJsonValue::Array(items)) = content.get("public_keys") {
        for item in items {
            if let Some(CanonicalJsonValue::String(s)) =
                item.as_object().and_then(|o| o.get("public_key"))
            {
                keys.push(decode_base64_lenient(s)?);
            }
        }
    }
    Ok(keys)
}

fn decode_base64_lenient(s: &str) -> Result<Vec<u8>, AuthError> {
    use base64::Engine as _;
    let trimmed = s.trim_end_matches('=');
    base64::engine::general_purpose::STANDARD_NO_PAD
        .decode(trimmed)
        .or_else(|_| base64::engine::general_purpose::STANDARD.decode(s))
        .map_err(|e| AuthError::reject(format!("invalid base64: {e}")))
}

/// Verifies that some signature in `signed.signatures` was made by one of `public_keys`, per the
/// third-party invite check ("if any signature in signed matches any public key ..., allow").
///
/// Only ed25519 keys (`ed25519:<version>` key IDs) are supported; this is a reasonable scoping
/// (it is, in practice, the only algorithm identity servers use) recorded in
/// `docs/status/02-state-and-model.md`.
fn verify_third_party_signature(
    signed: &CanonicalJsonObject,
    public_keys: &[Vec<u8>],
) -> AuthResult {
    let signatures = signed
        .get("signatures")
        .and_then(CanonicalJsonValue::as_object)
        .ok_or_else(|| AuthError::reject("missing third_party_invite.signed.signatures"))?;

    let mut signable = signed.clone();
    signable.remove("signatures");
    let message = CanonicalJsonValue::Object(signable).to_canonical_bytes();

    for entity_signatures in signatures.values() {
        let Some(entity_signatures) = entity_signatures.as_object() else {
            continue;
        };
        for (key_id, signature_value) in entity_signatures {
            if !key_id.starts_with("ed25519:") {
                continue;
            }
            let Some(signature_str) = signature_value.as_str() else {
                continue;
            };
            let Ok(signature_bytes) = decode_base64_lenient(signature_str) else {
                continue;
            };
            let Ok(signature_array): Result<[u8; 64], _> = signature_bytes.try_into() else {
                continue;
            };
            let signature = ed25519_dalek::Signature::from_bytes(&signature_array);

            for key_bytes in public_keys {
                let Ok(key_array): Result<[u8; 32], _> = key_bytes.clone().try_into() else {
                    continue;
                };
                let Ok(verifying_key) = ed25519_dalek::VerifyingKey::from_bytes(&key_array) else {
                    continue;
                };
                if verifying_key.verify(&message, &signature).is_ok() {
                    return Ok(());
                }
            }
        }
    }

    Err(AuthError::reject(
        "no signature on third-party invite matches a public key in m.room.third_party_invite event",
    ))
}

fn check_member_leave(
    rules: &RoomVersionRules,
    event: &IncomingEvent<'_>,
    target: &UserId,
    create: StateEntry<'_>,
    state: &impl StateFetch,
) -> AuthResult {
    let sender_membership = user_membership(state, event.sender)?;

    if event.sender == target {
        let ok = matches!(sender_membership, Membership::Join | Membership::Invite)
            || (rules.knocking && sender_membership == Membership::Knock);
        return if ok {
            Ok(())
        } else {
            Err(AuthError::reject(
                "cannot leave if not joined, invited or knocked",
            ))
        };
    }

    if sender_membership != Membership::Join {
        return Err(AuthError::reject("cannot kick if sender is not joined"));
    }

    let creators = creators_of(create, rules)?;
    let current_target = user_membership(state, target)?;
    let sender_power = effective_power_level(state, rules, &creators, event.sender)?;
    let ban_power = power_level_field_or_default(state, rules, PowerLevelField::Ban)?;

    if current_target == Membership::Ban && sender_power < ban_power {
        return Err(AuthError::reject(
            "sender does not have enough power to unban",
        ));
    }

    let kick_power = power_level_field_or_default(state, rules, PowerLevelField::Kick)?;
    let target_power = effective_power_level(state, rules, &creators, target)?;

    if sender_power >= kick_power && target_power < sender_power {
        Ok(())
    } else {
        Err(AuthError::reject(
            "sender does not have enough power to kick target user",
        ))
    }
}

fn check_member_ban(
    rules: &RoomVersionRules,
    event: &IncomingEvent<'_>,
    target: &UserId,
    create: StateEntry<'_>,
    state: &impl StateFetch,
) -> AuthResult {
    let sender_membership = user_membership(state, event.sender)?;
    if sender_membership != Membership::Join {
        return Err(AuthError::reject("cannot ban if sender is not joined"));
    }

    let creators = creators_of(create, rules)?;
    let sender_power = effective_power_level(state, rules, &creators, event.sender)?;
    let ban_power = power_level_field_or_default(state, rules, PowerLevelField::Ban)?;
    let target_power = effective_power_level(state, rules, &creators, target)?;

    if sender_power >= ban_power && target_power < sender_power {
        Ok(())
    } else {
        Err(AuthError::reject(
            "sender does not have enough power to ban target user",
        ))
    }
}

fn check_member_knock(
    rules: &RoomVersionRules,
    event: &IncomingEvent<'_>,
    target: &UserId,
    state: &impl StateFetch,
) -> AuthResult {
    let join_rule = current_join_rule(state)?;
    let supports_knock = join_rule == JoinRule::Knock
        || (rules.knock_restricted_join_rule && join_rule == JoinRule::KnockRestricted);
    if !supports_knock {
        return Err(AuthError::reject(
            "join rule is not knock or knock_restricted, knocking is not allowed",
        ));
    }

    if event.sender != target {
        return Err(AuthError::reject(
            "cannot make another user knock, sender does not match target",
        ));
    }

    let sender_membership = user_membership(state, event.sender)?;
    if matches!(
        sender_membership,
        Membership::Ban | Membership::Invite | Membership::Join
    ) {
        Err(AuthError::reject(
            "cannot knock if user is banned, invited or joined",
        ))
    } else {
        Ok(())
    }
}

// --- m.room.power_levels ---

fn check_room_power_levels(
    rules: &RoomVersionRules,
    event: &IncomingEvent<'_>,
    state: &impl StateFetch,
    sender_power: i64,
    creators: &[OwnedUserId],
) -> AuthResult {
    let new = PowerLevels::parse(event.content, rules)
        .map_err(|e| AuthError::reject(format!("invalid m.room.power_levels content: {e}")))?;

    if rules.explicitly_privilege_room_creators
        && let Some(CanonicalJsonValue::Object(users)) = event.content.get("users")
        && users.keys().any(|key| {
            UserId::parse(key).is_ok_and(|user| {
                creators
                    .iter()
                    .any(|c| AsRef::<UserId>::as_ref(c) == AsRef::<UserId>::as_ref(&user))
            })
        })
    {
        return Err(AuthError::reject(
            "creator user IDs are not allowed in the users field",
        ));
    }

    let Some(current_entry) = state.power_levels() else {
        // No previous m.room.power_levels event: the initial one is always allowed.
        return Ok(());
    };
    let current = PowerLevels::parse(current_entry.content, rules).map_err(|e| {
        AuthError::reject(format!("invalid current m.room.power_levels content: {e}"))
    })?;

    for (current_val, new_val, name) in [
        (current.ban, new.ban, "ban"),
        (current.events_default, new.events_default, "events_default"),
        (current.invite, new.invite, "invite"),
        (current.kick, new.kick, "kick"),
        (current.redact, new.redact, "redact"),
        (current.state_default, new.state_default, "state_default"),
        (current.users_default, new.users_default, "users_default"),
    ] {
        if current_val == new_val {
            continue;
        }
        if current_val > sender_power || new_val > sender_power {
            return Err(AuthError::reject(format!(
                "sender does not have enough power to change the power level of {name}"
            )));
        }
    }

    check_power_level_map_changes(&current.events, &new.events, sender_power, "event type")?;
    if rules.limit_notifications_power_levels {
        check_power_level_map_changes(
            &current.notifications,
            &new.notifications,
            sender_power,
            "notification",
        )?;
    }
    check_user_power_level_map_changes(&current.users, &new.users, sender_power, event.sender)?;

    Ok(())
}

fn check_power_level_map_changes(
    current: &BTreeMap<String, i64>,
    new: &BTreeMap<String, i64>,
    sender_power: i64,
    kind: &str,
) -> AuthResult {
    let mut keys: BTreeSet<&String> = current.keys().collect();
    keys.extend(new.keys());
    for key in keys {
        let cur = current.get(key).copied();
        let new_v = new.get(key).copied();
        if cur == new_v {
            continue;
        }
        let current_rejected = cur.is_some_and(|v| v > sender_power);
        let new_too_big = new_v.is_some_and(|v| v > sender_power);
        if current_rejected || new_too_big {
            return Err(AuthError::reject(format!(
                "sender does not have enough power to change the {kind} power level for {key:?}"
            )));
        }
    }
    Ok(())
}

fn check_user_power_level_map_changes(
    current: &BTreeMap<OwnedUserId, i64>,
    new: &BTreeMap<OwnedUserId, i64>,
    sender_power: i64,
    sender: &UserId,
) -> AuthResult {
    let mut keys: BTreeSet<&OwnedUserId> = current.keys().collect();
    keys.extend(new.keys());
    for key in keys {
        let cur = current.get(key).copied();
        let new_v = new.get(key).copied();
        if cur == new_v {
            continue;
        }
        let current_rejected =
            cur.is_some_and(|v| AsRef::<UserId>::as_ref(key) != sender && v >= sender_power);
        let new_too_big = new_v.is_some_and(|v| v > sender_power);
        if current_rejected || new_too_big {
            return Err(AuthError::reject(format!(
                "sender does not have enough power to change {key}'s power level"
            )));
        }
    }
    Ok(())
}

// --- m.room.redaction (pre-v3 special case) ---

fn check_room_redaction(
    rules: &RoomVersionRules,
    event: &IncomingEvent<'_>,
    sender_power: i64,
    state: &impl StateFetch,
) -> AuthResult {
    let _ = rules;
    let redact_power = power_level_field_or_default(state, rules, PowerLevelField::Redact)?;
    if sender_power >= redact_power {
        return Ok(());
    }

    let same_domain = match (event.event_id, event.redacts) {
        (Some(id), Some(redacted)) => id.server_name() == redacted.server_name(),
        _ => false,
    };
    if same_domain {
        return Ok(());
    }

    Err(AuthError::reject(
        "m.room.redaction event did not pass any of the allow rules",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state_fetch::FlatState;
    use hs_model::canonical::to_canonical_object;
    use hs_model::room_version::RoomVersionRules;
    use ruma::{EventId, RoomId};
    use serde_json::json;

    fn obj(value: serde_json::Value) -> CanonicalJsonObject {
        to_canonical_object(&value, true).unwrap()
    }

    fn user(s: &str) -> OwnedUserId {
        UserId::parse(s).unwrap()
    }

    fn room_id(s: &str) -> ruma::OwnedRoomId {
        RoomId::parse(s).unwrap()
    }

    fn no_create_lookup() -> Result<bool, AuthError> {
        Ok(true)
    }

    // --- m.room.create ---

    #[test]
    fn create_event_rejects_prev_events() {
        let content = obj(json!({"creator": "@a:hs1"}));
        let rid = room_id("!r:hs1");
        let sender = user("@a:hs1");
        let mut event =
            IncomingEvent::new("m.room.create", &sender, Some(&rid), Some(""), &content);
        event.prev_event_count = 1;
        assert!(check_room_create(&event, &RoomVersionRules::V1).is_err());
    }

    #[test]
    fn create_event_requires_room_id_server_to_match_sender() {
        let content = obj(json!({"creator": "@a:hs1"}));
        let rid = room_id("!r:other-server");
        let sender = user("@a:hs1");
        let mut event =
            IncomingEvent::new("m.room.create", &sender, Some(&rid), Some(""), &content);
        event.prev_event_count = 0;
        assert!(check_room_create(&event, &RoomVersionRules::V1).is_err());
    }

    #[test]
    fn v12_create_event_rejects_room_id_field() {
        let content = obj(json!({}));
        let rid = room_id("!r:hs1");
        let sender = user("@a:hs1");
        let mut event =
            IncomingEvent::new("m.room.create", &sender, Some(&rid), Some(""), &content);
        event.prev_event_count = 0;
        assert!(check_room_create(&event, &RoomVersionRules::V12).is_err());
    }

    #[test]
    fn v12_create_event_allows_missing_room_id_and_creator() {
        let content = obj(json!({}));
        let sender = user("@a:hs1");
        let mut event = IncomingEvent::new("m.room.create", &sender, None, Some(""), &content);
        event.prev_event_count = 0;
        assert!(check_room_create(&event, &RoomVersionRules::V12).is_ok());
    }

    #[test]
    fn v1_create_event_requires_creator_field() {
        let content = obj(json!({}));
        let rid = room_id("!r:hs1");
        let sender = user("@a:hs1");
        let mut event =
            IncomingEvent::new("m.room.create", &sender, Some(&rid), Some(""), &content);
        event.prev_event_count = 0;
        assert!(check_room_create(&event, &RoomVersionRules::V1).is_err());
    }

    // --- auth_events selection ---

    #[test]
    fn auth_events_selection_rejects_duplicates() {
        let content = obj(json!({"body": "hi"}));
        let rid = room_id("!r:hs1");
        let sender = user("@a:hs1");
        let event = IncomingEvent::new("m.room.message", &sender, Some(&rid), None, &content);
        let refs = vec![
            AuthEventRef {
                event_type: "m.room.power_levels",
                state_key: "",
                rejected: false,
            },
            AuthEventRef {
                event_type: "m.room.power_levels",
                state_key: "",
                rejected: false,
            },
        ];
        let err =
            check_auth_events_selection(&RoomVersionRules::V11, &event, &refs, no_create_lookup)
                .unwrap_err();
        assert!(err.0.contains("duplicate"));
    }

    #[test]
    fn auth_events_selection_rejects_unexpected_type() {
        let content = obj(json!({"body": "hi"}));
        let rid = room_id("!r:hs1");
        let sender = user("@a:hs1");
        let event = IncomingEvent::new("m.room.message", &sender, Some(&rid), None, &content);
        let refs = vec![AuthEventRef {
            event_type: "m.room.topic",
            state_key: "",
            rejected: false,
        }];
        assert!(
            check_auth_events_selection(&RoomVersionRules::V11, &event, &refs, no_create_lookup)
                .is_err()
        );
    }

    #[test]
    fn auth_events_selection_requires_create_event_pre_v12() {
        let content = obj(json!({"body": "hi"}));
        let rid = room_id("!r:hs1");
        let sender = user("@a:hs1");
        let event = IncomingEvent::new("m.room.message", &sender, Some(&rid), None, &content);
        let refs = vec![
            AuthEventRef {
                event_type: "m.room.power_levels",
                state_key: "",
                rejected: false,
            },
            AuthEventRef {
                event_type: "m.room.member",
                state_key: "@a:hs1",
                rejected: false,
            },
        ];
        let err =
            check_auth_events_selection(&RoomVersionRules::V11, &event, &refs, no_create_lookup)
                .unwrap_err();
        assert!(err.0.contains("m.room.create"));
    }

    #[test]
    fn auth_events_selection_rejects_referenced_rejected_event() {
        let content = obj(json!({"body": "hi"}));
        let rid = room_id("!r:hs1");
        let sender = user("@a:hs1");
        let event = IncomingEvent::new("m.room.message", &sender, Some(&rid), None, &content);
        let refs = vec![AuthEventRef {
            event_type: "m.room.power_levels",
            state_key: "",
            rejected: true,
        }];
        let err =
            check_auth_events_selection(&RoomVersionRules::V11, &event, &refs, no_create_lookup)
                .unwrap_err();
        assert!(err.0.contains("rejected"));
    }

    // --- state fixtures ---

    fn basic_room(rules: &RoomVersionRules) -> (FlatState, OwnedUserId, OwnedUserId) {
        let creator = user("@creator:hs1");
        let member = user("@member:hs1");
        let mut state = FlatState::new();
        let create_content = if rules.use_room_create_sender {
            obj(json!({}))
        } else {
            obj(json!({"creator": creator.as_str()}))
        };
        state.insert("m.room.create", "", creator.clone(), create_content);
        state.insert(
            "m.room.member",
            creator.as_str(),
            creator.clone(),
            obj(json!({"membership": "join"})),
        );
        state.insert(
            "m.room.member",
            member.as_str(),
            creator.clone(),
            obj(json!({"membership": "join"})),
        );
        state.insert(
            "m.room.power_levels",
            "",
            creator.clone(),
            obj(json!({
                "users": {creator.as_str(): 100},
                "ban": 50, "kick": 50, "redact": 50, "invite": 0,
                "users_default": 0, "events_default": 0, "state_default": 50,
            })),
        );
        state.insert(
            "m.room.join_rules",
            "",
            creator.clone(),
            obj(json!({"join_rule": "public"})),
        );
        (state, creator, member)
    }

    #[test]
    fn public_room_anyone_can_join() {
        let rules = RoomVersionRules::V11;
        let (state, _creator, _member) = basic_room(&rules);
        let outsider = user("@outsider:hs2");
        let content = obj(json!({"membership": "join"}));
        let event = IncomingEvent::new(
            "m.room.member",
            &outsider,
            None,
            Some(outsider.as_str()),
            &content,
        );
        assert!(check_event_auth(&rules, &event, &state).is_ok());
    }

    #[test]
    fn invite_only_room_rejects_unsolicited_join() {
        let rules = RoomVersionRules::V11;
        let (mut state, creator, _member) = basic_room(&rules);
        state.insert(
            "m.room.join_rules",
            "",
            creator,
            obj(json!({"join_rule": "invite"})),
        );
        let outsider = user("@outsider:hs2");
        let content = obj(json!({"membership": "join"}));
        let event = IncomingEvent::new(
            "m.room.member",
            &outsider,
            None,
            Some(outsider.as_str()),
            &content,
        );
        assert!(check_event_auth(&rules, &event, &state).is_err());
    }

    #[test]
    fn banned_user_cannot_join() {
        let rules = RoomVersionRules::V11;
        let (mut state, creator, _member) = basic_room(&rules);
        let banned = user("@banned:hs2");
        state.insert(
            "m.room.member",
            banned.as_str(),
            creator,
            obj(json!({"membership": "ban"})),
        );
        let content = obj(json!({"membership": "join"}));
        let event = IncomingEvent::new(
            "m.room.member",
            &banned,
            None,
            Some(banned.as_str()),
            &content,
        );
        assert!(check_event_auth(&rules, &event, &state).is_err());
    }

    #[test]
    fn low_power_user_cannot_kick_higher_power_user() {
        let rules = RoomVersionRules::V11;
        let (state, creator, member) = basic_room(&rules);
        let content = obj(json!({"membership": "leave"}));
        let event = IncomingEvent::new(
            "m.room.member",
            &member,
            None,
            Some(creator.as_str()),
            &content,
        );
        assert!(check_event_auth(&rules, &event, &state).is_err());
    }

    #[test]
    fn creator_can_kick_member() {
        let rules = RoomVersionRules::V11;
        let (state, creator, member) = basic_room(&rules);
        let content = obj(json!({"membership": "leave"}));
        let event = IncomingEvent::new(
            "m.room.member",
            &creator,
            None,
            Some(member.as_str()),
            &content,
        );
        assert!(check_event_auth(&rules, &event, &state).is_ok());
    }

    #[test]
    fn member_cannot_change_power_levels_above_their_own() {
        let rules = RoomVersionRules::V11;
        let (state, creator, member) = basic_room(&rules);
        let content = obj(json!({
            "users": {creator.as_str(): 100, member.as_str(): 100},
            "ban": 50, "kick": 50, "redact": 50, "invite": 0,
            "users_default": 0, "events_default": 0, "state_default": 50,
        }));
        let event = IncomingEvent::new("m.room.power_levels", &member, None, Some(""), &content);
        assert!(check_event_auth(&rules, &event, &state).is_err());
    }

    #[test]
    fn creator_can_raise_own_and_others_power() {
        let rules = RoomVersionRules::V11;
        let (state, creator, member) = basic_room(&rules);
        let content = obj(json!({
            "users": {creator.as_str(): 100, member.as_str(): 50},
            "ban": 50, "kick": 50, "redact": 50, "invite": 0,
            "users_default": 0, "events_default": 0, "state_default": 50,
        }));
        let event = IncomingEvent::new("m.room.power_levels", &creator, None, Some(""), &content);
        assert!(check_event_auth(&rules, &event, &state).is_ok());
    }

    #[test]
    fn v12_creator_has_infinite_power_even_if_power_levels_says_otherwise() {
        let rules = RoomVersionRules::V12;
        let creator = user("@creator:hs1");
        let mut state = FlatState::new();
        state.insert("m.room.create", "", creator.clone(), obj(json!({})));
        state.insert(
            "m.room.member",
            creator.as_str(),
            creator.clone(),
            obj(json!({"membership": "join"})),
        );
        // Absurdly low power levels for everyone, including the creator's own entry.
        state.insert(
            "m.room.power_levels",
            "",
            creator.clone(),
            obj(json!({
                "users": {creator.as_str(): 0},
                "ban": 100, "kick": 100, "redact": 100, "invite": 100,
                "users_default": 0, "events_default": 100, "state_default": 100,
            })),
        );
        state.insert(
            "m.room.join_rules",
            "",
            creator.clone(),
            obj(json!({"join_rule": "public"})),
        );

        let target = user("@x:hs2");
        let content = obj(json!({"membership": "ban"}));
        let event = IncomingEvent::new(
            "m.room.member",
            &creator,
            None,
            Some(target.as_str()),
            &content,
        );
        assert!(check_event_auth(&rules, &event, &state).is_ok());
    }

    #[test]
    fn v8_restricted_join_requires_authorised_via_user_with_invite_power() {
        let rules = RoomVersionRules::V8;
        let (mut state, creator, member) = basic_room(&rules);
        state.insert(
            "m.room.join_rules",
            "",
            creator.clone(),
            obj(json!({"join_rule": "restricted", "allow": []})),
        );
        let outsider = user("@outsider:hs2");

        // No `join_authorised_via_users_server` at all: rejected.
        let content = obj(json!({"membership": "join"}));
        let event = IncomingEvent::new(
            "m.room.member",
            &outsider,
            None,
            Some(outsider.as_str()),
            &content,
        );
        assert!(check_event_auth(&rules, &event, &state).is_err());

        // Authorised via a joined member with invite power (>= 0, the default invite level):
        // allowed.
        let content_ok = obj(json!({
            "membership": "join",
            "join_authorised_via_users_server": member.as_str(),
        }));
        let event_ok = IncomingEvent::new(
            "m.room.member",
            &outsider,
            None,
            Some(outsider.as_str()),
            &content_ok,
        );
        assert!(check_event_auth(&rules, &event_ok, &state).is_ok());

        // Authorised via a user who is not actually joined: rejected.
        let ghost = user("@ghost:hs3");
        let content_ghost = obj(json!({
            "membership": "join",
            "join_authorised_via_users_server": ghost.as_str(),
        }));
        let event_ghost = IncomingEvent::new(
            "m.room.member",
            &outsider,
            None,
            Some(outsider.as_str()),
            &content_ghost,
        );
        assert!(check_event_auth(&rules, &event_ghost, &state).is_err());
        let _ = &mut state; // state intentionally not mutated further
    }

    #[test]
    fn v7_knock_requires_knock_join_rule_and_matching_sender() {
        let rules = RoomVersionRules::V7;
        let (mut state, creator, _member) = basic_room(&rules);
        state.insert(
            "m.room.join_rules",
            "",
            creator,
            obj(json!({"join_rule": "knock"})),
        );
        let knocker = user("@knocker:hs2");
        let content = obj(json!({"membership": "knock"}));
        let event = IncomingEvent::new(
            "m.room.member",
            &knocker,
            None,
            Some(knocker.as_str()),
            &content,
        );
        assert!(check_event_auth(&rules, &event, &state).is_ok());

        // Cannot knock on someone else's behalf.
        let other = user("@other:hs2");
        let event_other = IncomingEvent::new(
            "m.room.member",
            &knocker,
            None,
            Some(other.as_str()),
            &content,
        );
        assert!(check_event_auth(&rules, &event_other, &state).is_err());
    }

    #[test]
    fn v6_knock_is_rejected_room_version_does_not_support_it() {
        let rules = RoomVersionRules::V6;
        let (mut state, creator, _member) = basic_room(&rules);
        // Even if the room somehow has a join_rule of "knock", v6 doesn't understand the
        // membership value at all.
        state.insert(
            "m.room.join_rules",
            "",
            creator,
            obj(json!({"join_rule": "public"})),
        );
        let knocker = user("@knocker:hs2");
        let content = obj(json!({"membership": "knock"}));
        let event = IncomingEvent::new(
            "m.room.member",
            &knocker,
            None,
            Some(knocker.as_str()),
            &content,
        );
        assert!(check_event_auth(&rules, &event, &state).is_err());
    }

    #[test]
    fn third_party_invite_verifies_ed25519_signature() {
        let rules = RoomVersionRules::V11;
        let (mut state, creator, _member) = basic_room(&rules);

        let key = ed25519_dalek::SigningKey::from_bytes(&[7u8; 32]);
        let verifying = key.verifying_key();
        let public_key_b64 = base64::Engine::encode(
            &base64::engine::general_purpose::STANDARD_NO_PAD,
            verifying.to_bytes(),
        );

        state.insert(
            "m.room.third_party_invite",
            "tok123",
            creator.clone(),
            obj(json!({"display_name": "friend", "public_key": public_key_b64})),
        );

        let target = user("@newperson:hs2");
        let signed = json!({"mxid": target.as_str(), "token": "tok123"});
        let signed_canonical = obj(signed.clone());
        let message = hs_model::canonical::CanonicalJsonValue::Object(signed_canonical.clone())
            .to_canonical_bytes();
        let signature = ed25519_dalek::Signer::sign(&key, &message);
        let sig_b64 = base64::Engine::encode(
            &base64::engine::general_purpose::STANDARD_NO_PAD,
            signature.to_bytes(),
        );

        let mut signed_with_sig = signed_canonical;
        signed_with_sig.insert(
            "signatures".to_owned(),
            hs_model::canonical::CanonicalJsonValue::Object(obj(
                json!({"identity.example.org": {"ed25519:1": sig_b64}}),
            )),
        );

        let content = obj(json!({
            "membership": "invite",
            "third_party_invite": {
                "display_name": "friend",
                "signed": {},
            }
        }));
        let mut content = content;
        content.insert(
            "third_party_invite".to_owned(),
            hs_model::canonical::CanonicalJsonValue::Object({
                let mut tpi = CanonicalJsonObject::new();
                tpi.insert(
                    "display_name".to_owned(),
                    hs_model::canonical::CanonicalJsonValue::String("friend".to_owned()),
                );
                tpi.insert(
                    "signed".to_owned(),
                    hs_model::canonical::CanonicalJsonValue::Object(signed_with_sig),
                );
                tpi
            }),
        );

        let event = IncomingEvent::new(
            "m.room.member",
            &creator,
            None,
            Some(target.as_str()),
            &content,
        );
        assert!(check_event_auth(&rules, &event, &state).is_ok());
    }

    #[test]
    fn v1_redaction_allowed_when_same_domain_even_without_power() {
        let rules = RoomVersionRules::V1;
        let (mut state, creator, member) = basic_room(&rules);
        // member has no redact power (default 50, member power 0).
        let content = obj(json!({}));
        let event_id = EventId::parse("$abc:hs1").unwrap();
        let redacts = EventId::parse("$xyz:hs1").unwrap();
        let mut event = IncomingEvent::new("m.room.redaction", &member, None, None, &content);
        event.event_id = Some(&event_id);
        event.redacts = Some(&redacts);
        assert!(check_event_auth(&rules, &event, &state).is_ok());

        // Different domain and no power: rejected.
        let redacts_other = EventId::parse("$xyz:other").unwrap();
        let mut event2 = IncomingEvent::new("m.room.redaction", &member, None, None, &content);
        event2.event_id = Some(&event_id);
        event2.redacts = Some(&redacts_other);
        assert!(check_event_auth(&rules, &event2, &state).is_err());
        let _ = &mut state;
        let _ = creator;
    }
}
