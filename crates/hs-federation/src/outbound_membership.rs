//! The membership handshakes this server initiates besides the join (`crate::outbound_join`):
//! leaving and knocking on a room hosted elsewhere, and inviting a user of another server into a
//! room here.
//!
//! - [`leave_room`]: `make_leave`, sign, `send_leave` (v2). What a user of this server does to
//!   reject an invite, or withdraw a knock, in a room this server is not in: there is no copy of
//!   the room here to author the leave against, so a server that is in it builds the template.
//! - [`knock_room`]: `make_knock`, sign, `send_knock`. The answer carries the room's stripped
//!   state (`knock_room_state`), which is what the knocking user's client shows.
//! - [`send_invite`]: `PUT /v2/invite` with an invite this server built and signed, for a user of
//!   `destination`. The answer is the same event co-signed by `destination`, verified here
//!   before it is handed back: same event ID, both signatures.
//!
//! Every outbound request goes through the caller's [`FederationClient`], as every other one
//! does; every event handed back has been verified.

use hs_model::Event;
use hs_model::signing::SigningKeyPair;
use ruma::{RoomVersionId, ServerName};
use serde_json::{Value, json};

use crate::client::FederationClient;
use crate::inbound::{verify_pdu, verify_server_signature};
use crate::keys::DynRemoteKeyCache;
use crate::outbound_join::{OutboundJoinError, SignedTemplate, make_and_sign};

/// A leave or knock this server's user made through another server, as that server accepted
/// it.
#[derive(Debug)]
pub struct RemoteMembershipOutcome {
    /// The room version the template was made (and the event parsed) under.
    pub room_version: RoomVersionId,
    /// This server's event, signed here and accepted by the resident.
    pub event: Event,
    /// For a knock, the room's stripped state as the resident described it (reduced to
    /// stripped-state shape, `crate::stripped::sanitize_received`); empty for a leave.
    pub room_state: Vec<Value>,
}

/// Leaves `room_id` as `user_id` through `destination`, a server in the room: `GET make_leave`,
/// sign, `PUT /v2/send_leave`. `content` (a `reason`) is merged into the template.
///
/// # Errors
/// See [`OutboundJoinError`].
#[allow(clippy::too_many_arguments)]
pub async fn leave_room(
    client: &FederationClient,
    destination: &str,
    room_id: &str,
    user_id: &str,
    own_server_name: &ServerName,
    signing_key: &SigningKeyPair,
    content: Option<&Value>,
) -> Result<RemoteMembershipOutcome, OutboundJoinError> {
    let SignedTemplate {
        value,
        event,
        room_version,
    } = make_and_sign(
        client,
        destination,
        "make_leave",
        false,
        room_id,
        user_id,
        own_server_name,
        signing_key,
        content,
    )
    .await?;
    check_membership(&event, "leave", destination)?;
    let path = format!(
        "/_matrix/federation/v2/send_leave/{room_id}/{}",
        event.event_id()
    );
    submit(client, destination, "send_leave", &path, &value).await?;
    Ok(RemoteMembershipOutcome {
        room_version,
        event,
        room_state: Vec::new(),
    })
}

/// Knocks on `room_id` as `user_id` through `destination`: `GET make_knock` (with every
/// supported room version), sign, `PUT /v1/send_knock`. `content` (a `reason`, the user's
/// profile) is merged into the template.
///
/// # Errors
/// See [`OutboundJoinError`].
#[allow(clippy::too_many_arguments)]
pub async fn knock_room(
    client: &FederationClient,
    destination: &str,
    room_id: &str,
    user_id: &str,
    own_server_name: &ServerName,
    signing_key: &SigningKeyPair,
    content: Option<&Value>,
) -> Result<RemoteMembershipOutcome, OutboundJoinError> {
    let SignedTemplate {
        value,
        event,
        room_version,
    } = make_and_sign(
        client,
        destination,
        "make_knock",
        true,
        room_id,
        user_id,
        own_server_name,
        signing_key,
        content,
    )
    .await?;
    check_membership(&event, "knock", destination)?;
    let path = format!(
        "/_matrix/federation/v1/send_knock/{room_id}/{}",
        event.event_id()
    );
    let body = submit(client, destination, "send_knock", &path, &value).await?;
    let room_state = body
        .get("knock_room_state")
        .and_then(Value::as_array)
        .map(|list| crate::stripped::sanitize_received(list))
        .unwrap_or_default();
    Ok(RemoteMembershipOutcome {
        room_version,
        event,
        room_state,
    })
}

/// Sends `event` -- an invite this server built and signed, for a user of `destination` -- with
/// the room's stripped state, and returns the event as `destination` co-signed it: parsed at
/// `room_version`, checked to be the same event (same ID), and carrying valid signatures from
/// both its sender's server and `destination`.
///
/// # Errors
/// [`OutboundJoinError::Client`] if the request fails, [`OutboundJoinError::Rejected`] if
/// `destination` refuses the invite, [`OutboundJoinError::MalformedResponse`] or
/// [`OutboundJoinError::UnverifiedEvent`] if what comes back is not the invite, co-signed.
pub async fn send_invite(
    client: &FederationClient,
    key_cache: &DynRemoteKeyCache,
    destination: &str,
    room_version: &RoomVersionId,
    event: &Event,
    invite_room_state: &[Value],
) -> Result<Event, OutboundJoinError> {
    let pdu: Value = serde_json::from_slice(event.canonical_bytes()).map_err(|e| {
        OutboundJoinError::Signing(format!("the invite does not round-trip to JSON: {e}"))
    })?;
    let room_id = pdu.get("room_id").and_then(Value::as_str).unwrap_or("");
    let path = format!(
        "/_matrix/federation/v2/invite/{room_id}/{}",
        event.event_id()
    );
    let body = json!({
        "room_version": room_version.as_str(),
        "event": pdu,
        "invite_room_state": invite_room_state,
    });
    let answer = submit(client, destination, "invite", &path, &body).await?;
    let returned = answer.get("event").ok_or_else(|| {
        OutboundJoinError::MalformedResponse(destination.to_owned(), "missing `event`".to_owned())
    })?;
    let cosigned = verify_pdu(returned, room_version, key_cache)
        .await
        .map_err(|source| OutboundJoinError::UnverifiedEvent {
            destination: destination.to_owned(),
            source,
        })?;
    if cosigned.event_id() != event.event_id() {
        return Err(OutboundJoinError::MalformedResponse(
            destination.to_owned(),
            format!(
                "the invite came back as a different event ({} rather than {})",
                cosigned.event_id(),
                event.event_id()
            ),
        ));
    }
    verify_server_signature(&cosigned, destination, key_cache)
        .await
        .map_err(|source| OutboundJoinError::UnverifiedEvent {
            destination: destination.to_owned(),
            source,
        })?;
    Ok(cosigned)
}

/// The template's `membership` must be the one asked for: a resident that answers `make_leave`
/// with a join template is not one to sign for.
fn check_membership(
    event: &Event,
    wanted: &str,
    destination: &str,
) -> Result<(), OutboundJoinError> {
    let membership = event
        .json()
        .get("content")
        .and_then(hs_model::canonical::CanonicalJsonValue::as_object)
        .and_then(|content| content.get("membership"))
        .and_then(hs_model::canonical::CanonicalJsonValue::as_str);
    if membership == Some(wanted) {
        Ok(())
    } else {
        Err(OutboundJoinError::MalformedTemplate(
            destination.to_owned(),
            format!("the template's membership is {membership:?}, not {wanted:?}"),
        ))
    }
}

/// `PUT path` with `body` to `destination`; the response body on a 2xx, a
/// [`OutboundJoinError::Rejected`] naming `step` otherwise. A v1-style `[200, {...}]` answer is
/// unwrapped to its object.
async fn submit(
    client: &FederationClient,
    destination: &str,
    step: &'static str,
    path: &str,
    body: &Value,
) -> Result<Value, OutboundJoinError> {
    let response = client
        .send(destination, "PUT", path, Some(body))
        .await
        .map_err(|source| OutboundJoinError::Client {
            destination: destination.to_owned(),
            source,
        })?;
    if response.status / 100 != 2 {
        return Err(OutboundJoinError::Rejected {
            destination: destination.to_owned(),
            step,
            status: response.status,
            body: response.body,
        });
    }
    Ok(match response.body {
        Value::Array(mut pair) if pair.len() == 2 => pair.pop().unwrap_or(Value::Null),
        other => other,
    })
}
