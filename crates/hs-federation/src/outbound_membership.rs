//! The membership handshakes this server initiates besides the join (`crate::outbound_join`):
//! leaving and knocking on a room hosted elsewhere, and inviting a user of another server into a
//! room here.
//!
//! - [`leave_room`]: `make_leave`, sign, `send_leave` (v2, then v1 for a server that does not
//!   answer v2). What a user of this server does to
//!   reject an invite, or withdraw a knock, in a room this server is not in: there is no copy of
//!   the room here to author the leave against, so a server that is in it builds the template.
//! - [`knock_room`]: `make_knock`, sign, `send_knock`. The answer carries the room's stripped
//!   state (`knock_room_state`), which is what the knocking user's client shows.
//! - [`send_invite`]: `PUT /v2/invite` (`/v1/invite` for a server that does not answer v2) with
//!   an invite this server built and signed, for a user of
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
/// sign, `PUT /v2/send_leave` (`/v1/send_leave` when the server does not answer v2). `content`
/// (a `reason`) is merged into the template.
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
        "send_leave/{}/{}",
        crate::client::encode_path_segment(room_id),
        crate::client::encode_path_segment(event.event_id().as_str())
    );
    crate::outbound_join::put_v2_falling_back_to_v1(
        client,
        destination,
        "send_leave",
        &path,
        &value,
    )
    .await?;
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
        "/_matrix/federation/v1/send_knock/{}/{}",
        crate::client::encode_path_segment(room_id),
        crate::client::encode_path_segment(event.event_id().as_str())
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
    let segments = format!(
        "{}/{}",
        crate::client::encode_path_segment(room_id),
        crate::client::encode_path_segment(event.event_id().as_str())
    );
    let body = json!({
        "room_version": room_version.as_str(),
        "event": pdu,
        "invite_room_state": invite_room_state,
    });
    let answer = match submit(
        client,
        destination,
        "invite",
        &format!("/_matrix/federation/v2/invite/{segments}"),
        &body,
    )
    .await
    {
        // A server that does not know the v2 spelling (`404`, or `400 M_UNRECOGNIZED`) is sent
        // the v1 one: the event alone, the stripped state in its `unsigned`, the answer
        // `[200, {"event": ...}]` (unwrapped by `submit`). As Synapse falls back; Sytest's
        // "Outbound federation can send invites via v1 API" answers v2 `404`.
        Err(OutboundJoinError::Rejected {
            status,
            body: refusal,
            ..
        }) if status == 404
            || (status == 400
                && refusal.get("errcode").and_then(Value::as_str) == Some("M_UNRECOGNIZED")) =>
        {
            tracing::info!(
                destination,
                status,
                "the other server does not answer the v2 invite; sending v1"
            );
            let mut v1 = pdu.clone();
            if let Some(object) = v1.as_object_mut() {
                let unsigned = object.entry("unsigned").or_insert_with(|| json!({}));
                if let Some(unsigned) = unsigned.as_object_mut() {
                    unsigned.insert("invite_room_state".to_owned(), json!(invite_room_state));
                }
            }
            submit(
                client,
                destination,
                "invite",
                &format!("/_matrix/federation/v1/invite/{segments}"),
                &v1,
            )
            .await?
        }
        other => other?,
    };
    let returned = answer.get("event").ok_or_else(|| {
        OutboundJoinError::MalformedResponse(destination.to_owned(), "missing `event`".to_owned())
    })?;
    let strict = hs_model::room_version::rules_for(room_version)
        .is_some_and(|rules| rules.strict_canonical_json);
    hs_model::canonical::to_canonical_object(returned, strict).map_err(|e| {
        OutboundJoinError::NotCanonicalJson {
            destination: destination.to_owned(),
            reason: e.to_string(),
        }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::destination_store::InMemoryDestinationStore;
    use crate::discovery::{AddrResolver, SrvResolver, WellKnownFetcher, WellKnownOutcome};
    use crate::keys::{
        KeyServerFetcher, OwnSigningKeys, RemoteKeyCache, build_server_key_response,
    };
    use async_trait::async_trait;
    use hs_model::canonical::{CanonicalJsonObject, CanonicalJsonValue, to_canonical_object};
    use std::collections::HashMap;
    use std::net::IpAddr;
    use std::sync::Arc;

    struct Loopback;
    #[async_trait]
    impl AddrResolver for Loopback {
        async fn resolve_addr(&self, _hostname: &str) -> Vec<IpAddr> {
            vec!["127.0.0.1".parse().unwrap()]
        }
    }
    #[async_trait]
    impl SrvResolver for Loopback {
        async fn lookup_srv(&self, _service: &str, _hostname: &str) -> Vec<(String, u16)> {
            Vec::new()
        }
    }
    struct NoWellKnown;
    #[async_trait]
    impl WellKnownFetcher for NoWellKnown {
        async fn fetch(&self, _hostname: &str) -> WellKnownOutcome {
            panic!("a destination with an explicit port never asks .well-known")
        }
    }
    struct Keys(HashMap<String, Value>);
    #[async_trait]
    impl KeyServerFetcher for Keys {
        async fn fetch_server_key(&self, server_name: &str) -> Option<Value> {
            self.0.get(server_name).cloned()
        }
    }

    /// Signs `object`'s redacted form as `server` and adds the signature to `object`.
    fn add_signature(object: &mut CanonicalJsonObject, server: &str, keys: &OwnSigningKeys) {
        let rules = hs_model::room_version::rules_for(&RoomVersionId::V11).unwrap();
        let mut redacted = hs_model::redaction::redact(object, &rules.redaction).unwrap();
        redacted.remove("signatures");
        let name = ruma::ServerName::parse(server).unwrap();
        hs_model::signing::sign_object(&mut redacted, &name, keys.primary()).unwrap();
        let Some(CanonicalJsonValue::Object(mut new)) = redacted.remove("signatures") else {
            panic!("sign_object adds signatures");
        };
        match object.get_mut("signatures") {
            Some(CanonicalJsonValue::Object(existing)) => existing.append(&mut new),
            _ => {
                object.insert("signatures".to_owned(), CanonicalJsonValue::Object(new));
            }
        }
    }

    /// A destination that does not answer the v2 `invite` (`404`, as Sytest's own server in
    /// "Outbound federation can send invites via v1 API") is sent the v1 one -- the event with
    /// the stripped state in its `unsigned` -- and its co-signed answer, `[200, {"event": ..}]`,
    /// is verified and returned. Until 2026-10-01 the invite failed with a `502` to the client.
    #[tokio::test]
    async fn an_invite_goes_by_v1_to_a_server_without_v2() {
        let dir = tempfile::tempdir().unwrap();
        let inviter_keys = OwnSigningKeys::load_or_generate(dir.path()).unwrap();
        let dir2 = tempfile::tempdir().unwrap();
        let invitee_keys = Arc::new(OwnSigningKeys::load_or_generate(dir2.path()).unwrap());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let invitee = format!(
            "invitee.example.org:{}",
            listener.local_addr().unwrap().port()
        );

        let mut object = to_canonical_object(
            &json!({
                "type": "m.room.member",
                "room_id": "!r:inviter.example.org",
                "sender": "@alice:inviter.example.org",
                "state_key": format!("@bob:{invitee}"),
                "content": {"membership": "invite"},
                "origin_server_ts": 1,
                "depth": 5,
                "prev_events": ["$prev"],
                "auth_events": ["$create"],
            }),
            true,
        )
        .unwrap();
        let hash = hs_model::hash::content_hash_base64(&object);
        object.insert(
            "hashes".to_owned(),
            CanonicalJsonValue::Object(CanonicalJsonObject::from([(
                "sha256".to_owned(),
                CanonicalJsonValue::String(hash),
            )])),
        );
        add_signature(&mut object, "inviter.example.org", &inviter_keys);
        let value: Value =
            serde_json::from_slice(&CanonicalJsonValue::Object(object).to_canonical_bytes())
                .unwrap();
        let event = Event::parse(&value, RoomVersionId::V11).unwrap();

        let received: Arc<std::sync::Mutex<Option<Value>>> = Arc::default();
        let app = axum::Router::new()
            .route(
                "/_matrix/federation/v2/invite/{room}/{event}",
                axum::routing::put(|| async { axum::http::StatusCode::NOT_FOUND }),
            )
            .route(
                "/_matrix/federation/v1/invite/{room}/{event}",
                axum::routing::put({
                    let received = received.clone();
                    let invitee = invitee.clone();
                    let keys = invitee_keys.clone();
                    move |axum::Json(body): axum::Json<Value>| {
                        *received.lock().unwrap() = Some(body.clone());
                        let mut object = to_canonical_object(&body, true).unwrap();
                        object.remove("unsigned");
                        add_signature(&mut object, &invitee, &keys);
                        let cosigned: Value = serde_json::from_slice(
                            &CanonicalJsonValue::Object(object).to_canonical_bytes(),
                        )
                        .unwrap();
                        async move { axum::Json(json!([200, {"event": cosigned}])) }
                    }
                }),
            );
        tokio::spawn(async move {
            axum::serve(listener, app.into_make_service())
                .await
                .unwrap();
        });

        let client = FederationClient::new(
            "inviter.example.org".to_owned(),
            inviter_keys.primary().clone(),
            crate::client::ClientConfig {
                scheme: "http",
                ip_policy: crate::client::IpPolicy::from_cidrs(&[], &[]),
                ..Default::default()
            },
            Arc::new(InMemoryDestinationStore::default()),
            Arc::new(NoWellKnown),
            Arc::new(Loopback),
            Arc::new(Loopback),
        );
        let key_docs: HashMap<String, Value> = [
            (
                "inviter.example.org".to_owned(),
                build_server_key_response("inviter.example.org", &inviter_keys, &[], 3600).unwrap(),
            ),
            (
                invitee.clone(),
                build_server_key_response(&invitee, &invitee_keys, &[], 3600).unwrap(),
            ),
        ]
        .into_iter()
        .collect();
        let cache = RemoteKeyCache::new(Box::new(Keys(key_docs)) as Box<dyn KeyServerFetcher>);
        let stripped = vec![json!({
            "type": "m.room.name", "state_key": "", "content": {"name": "n"},
            "sender": "@alice:inviter.example.org",
        })];

        let cosigned = send_invite(
            &client,
            &cache,
            &invitee,
            &RoomVersionId::V11,
            &event,
            &stripped,
        )
        .await
        .expect("the invite goes by v1");
        assert_eq!(cosigned.event_id(), event.event_id());
        let sent = received.lock().unwrap().clone().expect("v1 was asked");
        assert_eq!(sent["unsigned"]["invite_room_state"], json!(stripped));
        assert!(
            sent.get("room_version").is_none(),
            "v1 carries the event alone"
        );
    }

    /// Sytest's "Outbound federation rejects invite response which include invalid JSON for
    /// room version 6": the invitee's server co-signs the invite and adds a float; the answer
    /// is bad JSON ([`OutboundJoinError::NotCanonicalJson`], a `400 M_BAD_JSON` to the client),
    /// not an unverifiable event or a server that could not be reached.
    #[tokio::test]
    async fn an_invite_answered_with_a_float_is_bad_json() {
        let dir = tempfile::tempdir().unwrap();
        let inviter_keys = OwnSigningKeys::load_or_generate(dir.path()).unwrap();
        let dir2 = tempfile::tempdir().unwrap();
        let invitee_keys = Arc::new(OwnSigningKeys::load_or_generate(dir2.path()).unwrap());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let invitee = format!(
            "invitee.example.org:{}",
            listener.local_addr().unwrap().port()
        );

        let mut object = to_canonical_object(
            &json!({
                "type": "m.room.member",
                "room_id": "!r:inviter.example.org",
                "sender": "@alice:inviter.example.org",
                "state_key": format!("@bob:{invitee}"),
                "content": {"membership": "invite"},
                "origin_server_ts": 1,
                "depth": 5,
                "prev_events": ["$prev"],
                "auth_events": ["$create"],
            }),
            true,
        )
        .unwrap();
        let hash = hs_model::hash::content_hash_base64(&object);
        object.insert(
            "hashes".to_owned(),
            CanonicalJsonValue::Object(CanonicalJsonObject::from([(
                "sha256".to_owned(),
                CanonicalJsonValue::String(hash),
            )])),
        );
        add_signature(&mut object, "inviter.example.org", &inviter_keys);
        let value: Value =
            serde_json::from_slice(&CanonicalJsonValue::Object(object).to_canonical_bytes())
                .unwrap();
        let event = Event::parse(&value, RoomVersionId::V6).unwrap();

        let app = axum::Router::new().route(
            "/_matrix/federation/v2/invite/{room}/{event}",
            axum::routing::put({
                let invitee = invitee.clone();
                let keys = invitee_keys.clone();
                move |axum::Json(body): axum::Json<Value>| {
                    let mut object = to_canonical_object(&body["event"], true).unwrap();
                    add_signature(&mut object, &invitee, &keys);
                    let mut cosigned: Value = serde_json::from_slice(
                        &CanonicalJsonValue::Object(object).to_canonical_bytes(),
                    )
                    .unwrap();
                    cosigned["bad_val"] = json!(1.1);
                    async move { axum::Json(json!({"event": cosigned})) }
                }
            }),
        );
        tokio::spawn(async move {
            axum::serve(listener, app.into_make_service())
                .await
                .unwrap();
        });

        let client = FederationClient::new(
            "inviter.example.org".to_owned(),
            inviter_keys.primary().clone(),
            crate::client::ClientConfig {
                scheme: "http",
                ip_policy: crate::client::IpPolicy::from_cidrs(&[], &[]),
                ..Default::default()
            },
            Arc::new(InMemoryDestinationStore::default()),
            Arc::new(NoWellKnown),
            Arc::new(Loopback),
            Arc::new(Loopback),
        );
        let key_docs: HashMap<String, Value> = [
            (
                "inviter.example.org".to_owned(),
                build_server_key_response("inviter.example.org", &inviter_keys, &[], 3600).unwrap(),
            ),
            (
                invitee.clone(),
                build_server_key_response(&invitee, &invitee_keys, &[], 3600).unwrap(),
            ),
        ]
        .into_iter()
        .collect();
        let cache = RemoteKeyCache::new(Box::new(Keys(key_docs)) as Box<dyn KeyServerFetcher>);

        let err = send_invite(&client, &cache, &invitee, &RoomVersionId::V6, &event, &[])
            .await
            .unwrap_err();
        assert!(
            matches!(err, OutboundJoinError::NotCanonicalJson { .. }),
            "{err}"
        );
    }
}
