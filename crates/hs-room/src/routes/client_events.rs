//! What the room-scoped read endpoints (`/messages`, `/context`, `/event`, `/relations`) share
//! about showing one event to one reader: the `RoomEventFilter` a client passes as `filter`, the
//! reader's own membership at the event (MSC4115's `unsigned.membership`), the pruned form of an
//! erased user's events for a reader who was not in the room when they were sent, and the
//! member events a lazy-loading client is sent alongside a page.
//!
//! An event is rendered in two steps. Inside the room actor's query, [`view_event`] renders it
//! the way every one of these endpoints did before (bundled relations, the sender's own
//! `transaction_id`, `prev_content` and friends), adds `unsigned.membership`, and keeps the
//! pruned content beside it when the event could need it -- a local sender, and a reader who was
//! not joined at the event. Outside it, [`finish`] asks the account store whether those senders
//! were erased, once per sender, and swaps the content for the pruned one where they were. The
//! actor's query is synchronous; the account store is not, which is why there are two steps.

use std::collections::{HashMap, HashSet};

use hs_kv::KvBackend;
use hs_model::Event;
use hs_model::canonical::CanonicalJsonValue;
use ruma::{OwnedUserId, ServerName, UserId};
use serde::Deserialize;
use serde_json::Value;

use crate::actor::RoomActor;
use crate::error::RoomError;
use crate::routes::render::{
    attach_replaced_state, attach_transaction_id, canonical_to_json, client_event_json,
    client_event_json_bundled,
};
use crate::state::RoomState;

/// The client-server API's `RoomEventFilter`, as `/messages` and `/context` take it in their
/// `filter` query parameter (JSON, URL-encoded). `rooms`/`not_rooms` are accepted and ignored:
/// these endpoints read one room.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct RoomEventFilter {
    /// Event types to include; a trailing `*` matches any suffix.
    pub types: Option<Vec<String>>,
    /// Event types to leave out; wins over `types`.
    pub not_types: Option<Vec<String>>,
    /// Senders to include.
    pub senders: Option<Vec<String>>,
    /// Senders to leave out; wins over `senders`.
    pub not_senders: Option<Vec<String>>,
    /// `true`: only events whose content has a string `url`; `false`: only events without one.
    pub contains_url: Option<bool>,
    /// Send the member events of the page's senders alongside it, in `state`.
    pub lazy_load_members: Option<bool>,
    /// The filter's own `limit`, which the endpoint's `limit` parameter overrides.
    pub limit: Option<usize>,
}

/// Whether `pattern` (one entry of `types`/`not_types`) matches `event_type`: exact, or a prefix
/// ending in `*`.
fn type_matches(pattern: &str, event_type: &str) -> bool {
    pattern
        .strip_suffix('*')
        .map_or(pattern == event_type, |prefix| {
            event_type.starts_with(prefix)
        })
}

impl RoomEventFilter {
    /// Parses the `filter` query parameter. Absent or empty is no filter.
    ///
    /// # Errors
    /// [`RoomError::InvalidParam`] when it is not a JSON `RoomEventFilter`.
    pub fn from_param(raw: Option<&str>) -> Result<Self, RoomError> {
        match raw.map(str::trim) {
            None | Some("") => Ok(Self::default()),
            Some(raw) => serde_json::from_str(raw)
                .map_err(|e| RoomError::InvalidParam(format!("invalid filter: {e}"))),
        }
    }

    /// Whether this filter asks for the page's senders' member events.
    #[must_use]
    pub fn lazy_loads_members(&self) -> bool {
        self.lazy_load_members == Some(true)
    }

    /// Whether `event` passes `types`, `not_types`, `senders`, `not_senders` and
    /// `contains_url`.
    #[must_use]
    pub fn matches(&self, event: &Event) -> bool {
        let event_type = event.header().event_type.as_str();
        let sender = event.header().sender.as_str();
        if self
            .not_types
            .as_ref()
            .is_some_and(|not| not.iter().any(|t| type_matches(t, event_type)))
            || self
                .not_senders
                .as_ref()
                .is_some_and(|not| not.iter().any(|s| s == sender))
        {
            return false;
        }
        if self
            .types
            .as_ref()
            .is_some_and(|types| !types.iter().any(|t| type_matches(t, event_type)))
            || self
                .senders
                .as_ref()
                .is_some_and(|senders| !senders.iter().any(|s| s == sender))
        {
            return false;
        }
        if let Some(want_url) = self.contains_url {
            let has_url = event
                .json()
                .get("content")
                .and_then(CanonicalJsonValue::as_object)
                .and_then(|c| c.get("url"))
                .and_then(CanonicalJsonValue::as_str)
                .is_some();
            if has_url != want_url {
                return false;
            }
        }
        true
    }
}

/// One event rendered for one reader, before [`finish`] has applied erasure.
#[derive(Debug)]
pub struct ViewedEvent {
    /// The client-format JSON.
    pub json: Value,
    /// The event's sender.
    pub sender: OwnedUserId,
    /// The pruned content, kept when the reader would be shown it if the sender has been
    /// erased: a sender of this server, a reader not joined at the event, an event not already
    /// redacted.
    pub pruned_content: Option<Value>,
}

/// Writes `unsigned.membership` (MSC4115): the reader's membership at the event.
#[must_use]
pub fn attach_membership(mut value: Value, membership: Option<&str>) -> Value {
    if let Some(membership) = membership
        && let Some(unsigned) = value.get_mut("unsigned").and_then(Value::as_object_mut)
    {
        unsigned.insert(
            "membership".to_owned(),
            Value::String(membership.to_owned()),
        );
    }
    value
}

/// Renders `event` for `requester` (see the module docs): what `/messages`, `/context`,
/// `/event` and `/relations` show, before [`finish`].
#[must_use]
pub fn view_event<B: KvBackend>(
    actor: &RoomActor<B>,
    event: &Event,
    requester: &hs_auth::requester::Requester,
    local_server: &ServerName,
) -> ViewedEvent {
    let bundle = actor.relation_bundle(event.event_id(), &requester.user_id);
    let txn_id = actor.transaction_id_for(
        event.event_id(),
        &requester.user_id,
        requester.device_id.as_deref(),
    );
    let membership = actor.membership_at_event(event, &requester.user_id).ok();
    let json = attach_membership(
        attach_replaced_state(
            attach_transaction_id(client_event_json_bundled(event, &bundle), txn_id),
            actor.replaced_state_for(event, &requester.user_id).as_ref(),
        ),
        membership.as_deref(),
    );
    let sender = event.header().sender.clone();
    let pruned_content = (sender.server_name() == local_server
        && membership.as_deref() != Some("join")
        && !event.header().flags.is_redacted())
    .then(|| pruned_content(event))
    .flatten();
    ViewedEvent {
        json,
        sender,
        pruned_content,
    }
}

/// The content `event` has once redacted, as JSON.
fn pruned_content(event: &Event) -> Option<Value> {
    let redacted = event.redacted_json().ok()?;
    let content = redacted
        .get("content")
        .and_then(CanonicalJsonValue::as_object)
        .cloned()
        .unwrap_or_default();
    Some(canonical_to_json(&content))
}

/// Applies erasure to `viewed` and returns the JSON: an event whose sender's account was erased
/// (`users.deactivate` with `erase`, Sytest's "Only original members of the room can see
/// messages from erased users") is shown pruned to a reader who was not joined when it was
/// sent, as Synapse shows it; a reader who was there keeps seeing what they saw. The account
/// store is asked once per sender; a sender it cannot answer for is taken as not erased.
pub async fn finish<B: KvBackend + 'static>(
    state: &RoomState<B>,
    viewed: Vec<ViewedEvent>,
) -> Vec<Value> {
    finish_with_accounts(state.auth.store.as_ref(), viewed).await
}

/// [`finish`] against an account store directly, for a caller without a [`RoomState`] (`/sync`
/// in `hs-user`, which renders through [`view_event`] too once it shows MSC4115's membership and
/// prunes erased senders).
pub async fn finish_with_accounts(
    accounts: &dyn hs_auth::store::AuthStore,
    viewed: Vec<ViewedEvent>,
) -> Vec<Value> {
    let candidates: HashSet<OwnedUserId> = viewed
        .iter()
        .filter(|v| v.pruned_content.is_some())
        .map(|v| v.sender.clone())
        .collect();
    let mut erased: HashMap<OwnedUserId, bool> = HashMap::with_capacity(candidates.len());
    for sender in candidates {
        let is_erased = match accounts.get_user(&sender).await {
            Ok(record) => record.is_some_and(|r| r.erased),
            Err(error) => {
                tracing::warn!(%sender, %error, "could not tell whether a sender was erased; showing their events");
                false
            }
        };
        erased.insert(sender, is_erased);
    }
    viewed
        .into_iter()
        .map(|v| {
            let ViewedEvent {
                mut json,
                sender,
                pruned_content,
            } = v;
            if let Some(pruned) = pruned_content
                && erased.get(&sender).copied().unwrap_or(false)
                && let Some(obj) = json.as_object_mut()
            {
                obj.insert("content".to_owned(), pruned);
            }
            json
        })
        .collect()
}

/// The member events a lazy-loading client is sent with a page (`state` in `/messages` and
/// `/context`): each sender's `m.room.member` in the room's state after `at` (Synapse uses the
/// page's first event), once each, in client format. A sender with no member event there is
/// left out.
#[must_use]
pub fn lazy_member_state<B: KvBackend>(
    actor: &RoomActor<B>,
    at: &Event,
    senders: &[&UserId],
    requester: &UserId,
) -> Vec<Value> {
    let mut seen = HashSet::new();
    senders
        .iter()
        .filter(|s| seen.insert(s.as_str()))
        .filter_map(|sender| actor.member_event_at(at, sender).ok().flatten())
        .map(|e| {
            attach_replaced_state(
                client_event_json(e),
                actor.replaced_state_for(e, requester).as_ref(),
            )
        })
        .collect()
}
