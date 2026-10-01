//! `PUT /invite`: another server inviting one of this server's users into a room it is in.
//!
//! The invite handshake is the one membership handshake that runs the other way round. The
//! inviting server builds and signs the `m.room.member` event (`membership: invite`) for a user
//! of *this* server, and sends it here before it adds it to the room; this server checks it,
//! adds its own signature -- the invitee's server agreeing that its user was invited -- and
//! answers with the co-signed event, which the inviting server then puts into the room and
//! sends to the room's other servers. Alongside the event comes the room's stripped state
//! (`invite_room_state`, `crate::stripped`), which is all the invitee's client will have to show
//! what the room is: this server is usually not in it.
//!
//! What is checked here ([`receive_invite`]): the room version is one this server supports; the
//! event verifies (content hash, the inviting server's signature); it is an `m.room.member`
//! invite for the room in the path, with the ID in the path; its sender is a user of the
//! requesting server; its target is a user of this one. What is **not** checked is the event's
//! authorization against the room's state: this server does not hold the room's state (that is
//! the point), and neither does Synapse at this step. The room's own servers authorize the event
//! when the inviting server sends it to them.
//!
//! Where the invite goes is [`InviteSink`]'s business: `hs-cli` records it in the room here
//! (`hs_room::registry::RoomRegistry::accept_out_of_room_membership`), where the invitee's
//! `/sync` finds it. With no sink installed ([`crate::transport::FederationState::invites`] is
//! `None`) the route answers `501`, as it did while it was a seam.

use std::sync::Arc;

use async_trait::async_trait;
use hs_model::Event;
use hs_model::canonical::CanonicalJsonValue;
use hs_model::signing::{SigningKeyPair, sign_object};
use ruma::{RoomVersionId, ServerName, UserId};
use serde_json::Value;

use crate::inbound::verify_pdu;
use crate::keys::DynRemoteKeyCache;

/// Why an [`InviteSink`] could not record an invite.
#[derive(Debug, Clone, thiserror::Error)]
#[error("{0}")]
pub struct InviteRejected(pub String);

/// Records an invite this server has accepted and co-signed, so the invitee sees it.
#[async_trait]
pub trait InviteSink: Send + Sync {
    /// Records `event` -- an `m.room.member` invite for a user of this server, verified and
    /// co-signed by [`receive_invite`] -- with `invite_room_state`, the room's stripped state as
    /// the inviting server described it (already reduced to stripped-state shape by
    /// [`crate::stripped::sanitize_received`]).
    ///
    /// # Errors
    /// [`InviteRejected`] if the invite cannot be recorded; the inviting server is told the
    /// invite was refused.
    async fn accept_invite(
        &self,
        room_version: &RoomVersionId,
        event: &Event,
        invite_room_state: &[Value],
    ) -> Result<(), InviteRejected>;
}

/// What the `/invite` route needs beyond the rest of the federation state: somewhere to put an
/// invite, and this server's key to co-sign it with.
#[derive(Clone)]
pub struct InviteHandling {
    /// Where accepted invites go.
    pub sink: Arc<dyn InviteSink>,
    /// This server's event-signing key.
    pub signing_key: Arc<SigningKeyPair>,
}

/// Why [`receive_invite`] refused an invite.
#[derive(Debug, Clone, thiserror::Error)]
pub enum InviteError {
    /// The room's version is not one this server supports (`M_INCOMPATIBLE_ROOM_VERSION`).
    #[error("room version {0} is not supported by this server")]
    IncompatibleRoomVersion(String),
    /// The event is not a well-formed invite for this request, or does not verify.
    #[error("{0}")]
    Malformed(String),
    /// The event is well formed but not one this server will accept from this requester.
    #[error("{0}")]
    Forbidden(String),
    /// The invite could not be recorded.
    #[error("{0}")]
    Store(String),
}

/// Checks, co-signs and records an invite sent by `origin` (see the module docs). Returns the
/// co-signed event, which is what the inviting server gets back.
///
/// # Errors
/// See [`InviteError`].
#[allow(clippy::too_many_arguments)]
pub async fn receive_invite(
    handling: &InviteHandling,
    key_cache: &DynRemoteKeyCache,
    own_server_name: &str,
    origin: &str,
    room_id: &str,
    event_id: &str,
    room_version: &str,
    raw_event: &Value,
    invite_room_state: &[Value],
) -> Result<Value, InviteError> {
    let version = RoomVersionId::try_from(room_version)
        .map_err(|_| InviteError::IncompatibleRoomVersion(room_version.to_owned()))?;
    if hs_model::room_version::rules_for(&version).is_none() {
        return Err(InviteError::IncompatibleRoomVersion(
            room_version.to_owned(),
        ));
    }

    let event = verify_pdu(raw_event, &version, key_cache)
        .await
        .map_err(|e| InviteError::Malformed(format!("the invite does not verify: {e}")))?;
    if event.event_id().as_str() != event_id {
        return Err(InviteError::Malformed(
            "the event ID in the path is not the event's".to_owned(),
        ));
    }
    check_shape(&event, room_id, origin, own_server_name)?;

    let own = ServerName::parse(own_server_name)
        .map_err(|e| InviteError::Store(format!("this server's own name does not parse: {e}")))?;
    let cosigned = cosign(&event, &own, &handling.signing_key)?;
    let cosigned_event = Event::parse(&cosigned, version.clone())
        .map_err(|e| InviteError::Store(format!("the co-signed invite does not parse: {e}")))?;

    let stripped = crate::stripped::sanitize_received(invite_room_state);
    handling
        .sink
        .accept_invite(&version, &cosigned_event, &stripped)
        .await
        .map_err(|e| InviteError::Forbidden(e.0))?;
    tracing::info!(
        room_id,
        event_id,
        origin,
        "accepted an invite from another server"
    );
    Ok(cosigned)
}

/// The shape checks [`receive_invite`] makes once the event has verified.
fn check_shape(
    event: &Event,
    room_id: &str,
    origin: &str,
    own_server_name: &str,
) -> Result<(), InviteError> {
    if event.header().event_type != "m.room.member" {
        return Err(InviteError::Malformed(
            "an invite must be an m.room.member event".to_owned(),
        ));
    }
    let membership = event
        .json()
        .get("content")
        .and_then(CanonicalJsonValue::as_object)
        .and_then(|content| content.get("membership"))
        .and_then(CanonicalJsonValue::as_str);
    if membership != Some("invite") {
        return Err(InviteError::Malformed(
            "content.membership is not \"invite\"".to_owned(),
        ));
    }
    if event
        .json()
        .get("room_id")
        .and_then(CanonicalJsonValue::as_str)
        != Some(room_id)
    {
        return Err(InviteError::Malformed(
            "the event's room_id is not the room in the path".to_owned(),
        ));
    }
    let sender_server = event.header().sender.server_name().as_str();
    if sender_server != origin {
        return Err(InviteError::Forbidden(format!(
            "the invite's sender is on {sender_server}, not the requesting server {origin}"
        )));
    }
    let target = event
        .header()
        .state_key
        .as_deref()
        .and_then(|key| UserId::parse(key).ok())
        .ok_or_else(|| {
            InviteError::Malformed("the invite's state_key is not a user ID".to_owned())
        })?;
    if target.server_name().as_str() != own_server_name {
        return Err(InviteError::Forbidden(format!(
            "{target} is not a user of this server"
        )));
    }
    Ok(())
}

/// `event` with this server's signature added over its redacted form (the spec's signing
/// order), every other byte as received. The event ID does not change: it is derived from the
/// reference hash, which excludes signatures.
///
/// # Errors
/// [`InviteError::Store`] if the event cannot be redacted or signed.
pub fn cosign(
    event: &Event,
    own_server_name: &ServerName,
    key: &SigningKeyPair,
) -> Result<Value, InviteError> {
    let mut full = event.json().clone();
    let mut redacted = event
        .redacted_json()
        .map_err(|e| InviteError::Store(format!("cannot redact the invite to sign it: {e}")))?;
    sign_object(&mut redacted, own_server_name, key)
        .map_err(|e| InviteError::Store(format!("cannot sign the invite: {e}")))?;
    let signatures = redacted
        .remove("signatures")
        .ok_or_else(|| InviteError::Store("signing left no signatures".to_owned()))?;
    full.insert("signatures".to_owned(), signatures);
    serde_json::from_slice(&CanonicalJsonValue::Object(full).to_canonical_bytes())
        .map_err(|e| InviteError::Store(format!("the co-signed invite is not JSON: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keys::{
        KeyServerFetcher, OwnSigningKeys, RemoteKeyCache, build_server_key_response,
    };
    use hs_model::canonical::{CanonicalJsonObject, to_canonical_object};
    use serde_json::json;
    use std::sync::Mutex;

    struct PerServerFetcher(Vec<(String, Value)>);
    #[async_trait]
    impl KeyServerFetcher for PerServerFetcher {
        async fn fetch_server_key(&self, server_name: &str) -> Option<Value> {
            self.0
                .iter()
                .find(|(name, _)| name == server_name)
                .map(|(_, doc)| doc.clone())
        }
    }

    #[derive(Default)]
    struct RecordingSink(Mutex<Vec<(String, Vec<Value>)>>);
    #[async_trait]
    impl InviteSink for RecordingSink {
        async fn accept_invite(
            &self,
            _room_version: &RoomVersionId,
            event: &Event,
            invite_room_state: &[Value],
        ) -> Result<(), InviteRejected> {
            self.0
                .lock()
                .unwrap()
                .push((event.event_id().to_string(), invite_room_state.to_vec()));
            Ok(())
        }
    }

    /// An invite from `@alice:inviter.example.org` for `target`, signed by the inviter's key.
    fn signed_invite(keys: &OwnSigningKeys, target: &str, membership: &str) -> Value {
        let mut object: CanonicalJsonObject = to_canonical_object(
            &json!({
                "type": "m.room.member",
                "room_id": "!r:inviter.example.org",
                "sender": "@alice:inviter.example.org",
                "state_key": target,
                "origin_server_ts": 1,
                "depth": 5,
                "content": {"membership": membership},
                "prev_events": ["$prev"],
                "auth_events": ["$create"],
            }),
            true,
        )
        .unwrap();
        let hash = hs_model::hash::content_hash_base64(&object);
        object.insert(
            "hashes".to_owned(),
            CanonicalJsonValue::Object(
                [("sha256".to_owned(), CanonicalJsonValue::String(hash))]
                    .into_iter()
                    .collect(),
            ),
        );
        let rules = hs_model::room_version::rules_for(&RoomVersionId::V11).unwrap();
        let mut redacted = hs_model::redaction::redact(&object, &rules.redaction).unwrap();
        let server = ServerName::parse("inviter.example.org").unwrap();
        sign_object(&mut redacted, &server, keys.primary()).unwrap();
        object.insert(
            "signatures".to_owned(),
            redacted.remove("signatures").unwrap(),
        );
        serde_json::from_slice(&CanonicalJsonValue::Object(object).to_canonical_bytes()).unwrap()
    }

    struct Fixture {
        _dirs: (tempfile::TempDir, tempfile::TempDir),
        inviter_keys: OwnSigningKeys,
        cache: DynRemoteKeyCache,
        handling: InviteHandling,
        sink: Arc<RecordingSink>,
    }

    fn fixture() -> Fixture {
        let inviter_dir = tempfile::tempdir().unwrap();
        let own_dir = tempfile::tempdir().unwrap();
        let inviter_keys = OwnSigningKeys::load_or_generate(inviter_dir.path()).unwrap();
        let own_keys = OwnSigningKeys::load_or_generate(own_dir.path()).unwrap();
        let cache = RemoteKeyCache::new(Box::new(PerServerFetcher(vec![
            (
                "inviter.example.org".to_owned(),
                build_server_key_response("inviter.example.org", &inviter_keys, &[], 3600).unwrap(),
            ),
            (
                "invitee.example.org".to_owned(),
                build_server_key_response("invitee.example.org", &own_keys, &[], 3600).unwrap(),
            ),
        ])) as Box<dyn KeyServerFetcher>);
        let sink = Arc::new(RecordingSink::default());
        let handling = InviteHandling {
            sink: sink.clone(),
            signing_key: Arc::new(own_keys.primary().clone()),
        };
        Fixture {
            _dirs: (inviter_dir, own_dir),
            inviter_keys,
            cache,
            handling,
            sink,
        }
    }

    fn event_id_of(raw: &Value) -> String {
        Event::parse(raw, RoomVersionId::V11)
            .unwrap()
            .event_id()
            .to_string()
    }

    #[tokio::test]
    async fn an_invite_for_a_local_user_is_cosigned_recorded_and_keeps_its_event_id() {
        let f = fixture();
        let raw = signed_invite(&f.inviter_keys, "@bob:invitee.example.org", "invite");
        let event_id = event_id_of(&raw);
        let stripped = vec![
            json!({"type": "m.room.name", "state_key": "", "sender": "@alice:inviter.example.org", "content": {"name": "n"}, "event_id": "$x"}),
            json!("junk"),
        ];

        let cosigned = receive_invite(
            &f.handling,
            &f.cache,
            "invitee.example.org",
            "inviter.example.org",
            "!r:inviter.example.org",
            &event_id,
            "11",
            &raw,
            &stripped,
        )
        .await
        .unwrap();

        assert_eq!(event_id_of(&cosigned), event_id);
        let parsed = Event::parse(&cosigned, RoomVersionId::V11).unwrap();
        crate::inbound::verify_server_signature(&parsed, "invitee.example.org", &f.cache)
            .await
            .expect("this server's signature verifies");
        crate::inbound::verify_server_signature(&parsed, "inviter.example.org", &f.cache)
            .await
            .expect("the inviter's signature is kept");
        let recorded = f.sink.0.lock().unwrap().clone();
        assert_eq!(recorded.len(), 1);
        assert_eq!(recorded[0].0, event_id);
        assert_eq!(
            recorded[0].1,
            vec![
                json!({"type": "m.room.name", "state_key": "", "sender": "@alice:inviter.example.org", "content": {"name": "n"}})
            ]
        );
    }

    #[tokio::test]
    async fn an_invite_for_someone_elses_user_or_from_the_wrong_server_is_refused() {
        let f = fixture();
        let raw = signed_invite(&f.inviter_keys, "@carol:elsewhere.example.org", "invite");
        let err = receive_invite(
            &f.handling,
            &f.cache,
            "invitee.example.org",
            "inviter.example.org",
            "!r:inviter.example.org",
            &event_id_of(&raw),
            "11",
            &raw,
            &[],
        )
        .await
        .unwrap_err();
        assert!(matches!(err, InviteError::Forbidden(_)), "{err}");

        let raw = signed_invite(&f.inviter_keys, "@bob:invitee.example.org", "invite");
        let err = receive_invite(
            &f.handling,
            &f.cache,
            "invitee.example.org",
            "impostor.example.org",
            "!r:inviter.example.org",
            &event_id_of(&raw),
            "11",
            &raw,
            &[],
        )
        .await
        .unwrap_err();
        assert!(matches!(err, InviteError::Forbidden(_)), "{err}");
        assert!(f.sink.0.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_join_dressed_as_an_invite_a_wrong_room_or_version_is_refused() {
        let f = fixture();
        let raw = signed_invite(&f.inviter_keys, "@bob:invitee.example.org", "join");
        let err = receive_invite(
            &f.handling,
            &f.cache,
            "invitee.example.org",
            "inviter.example.org",
            "!r:inviter.example.org",
            &event_id_of(&raw),
            "11",
            &raw,
            &[],
        )
        .await
        .unwrap_err();
        assert!(matches!(err, InviteError::Malformed(_)), "{err}");

        let raw = signed_invite(&f.inviter_keys, "@bob:invitee.example.org", "invite");
        let err = receive_invite(
            &f.handling,
            &f.cache,
            "invitee.example.org",
            "inviter.example.org",
            "!other:inviter.example.org",
            &event_id_of(&raw),
            "11",
            &raw,
            &[],
        )
        .await
        .unwrap_err();
        assert!(matches!(err, InviteError::Malformed(_)), "{err}");

        let err = receive_invite(
            &f.handling,
            &f.cache,
            "invitee.example.org",
            "inviter.example.org",
            "!r:inviter.example.org",
            &event_id_of(&raw),
            "999",
            &raw,
            &[],
        )
        .await
        .unwrap_err();
        assert!(
            matches!(err, InviteError::IncompatibleRoomVersion(_)),
            "{err}"
        );
        assert!(f.sink.0.lock().unwrap().is_empty());
    }

    /// An invite whose signature does not verify is refused. One whose content was changed after
    /// it was signed -- its hash fails, its signature over the redacted form holds -- is taken
    /// redacted, as the spec has every received event taken (`crate::inbound::verify_pdu`).
    #[tokio::test]
    async fn a_tampered_invite_does_not_verify_and_a_changed_one_is_taken_redacted() {
        let f = fixture();
        let mut raw = signed_invite(&f.inviter_keys, "@bob:invitee.example.org", "invite");
        for signature in raw["signatures"]["inviter.example.org"]
            .as_object_mut()
            .unwrap()
            .values_mut()
        {
            *signature = json!("A".repeat(86));
        }
        let err = receive_invite(
            &f.handling,
            &f.cache,
            "invitee.example.org",
            "inviter.example.org",
            "!r:inviter.example.org",
            &event_id_of(&raw),
            "11",
            &raw,
            &[],
        )
        .await
        .unwrap_err();
        assert!(matches!(err, InviteError::Malformed(_)), "{err}");

        let mut raw = signed_invite(&f.inviter_keys, "@bob:invitee.example.org", "invite");
        raw["content"]["displayname"] = json!("changed after signing");
        let cosigned = receive_invite(
            &f.handling,
            &f.cache,
            "invitee.example.org",
            "inviter.example.org",
            "!r:inviter.example.org",
            &event_id_of(&raw),
            "11",
            &raw,
            &[],
        )
        .await
        .expect("an invite whose content changed is taken redacted");
        assert_eq!(cosigned["content"], json!({"membership": "invite"}));
    }
}
