//! Third-party (3PID) invites: inviting somebody by an email address (or another third-party
//! identifier) through an identity server, the spec's "Third-party invites" module.
//!
//! `POST /rooms/{roomId}/invite` with `id_server`, `id_access_token`, `medium` and `address`
//! (and `createRoom`'s `invite_3pid`) asks the identity server who the address belongs to. If
//! somebody, they are invited as usual. If nobody yet, the identity server stores the invitation
//! and this server sends an `m.room.third_party_invite` state event whose `state_key` is the
//! identity server's token and whose content carries its public keys. When the address is later
//! bound to a Matrix user, the identity server tells that user's server (`PUT
//! /_matrix/federation/v1/3pid/onbind`) with a `signed` block; [`exchange`] turns that into an
//! `m.room.member` invite carrying `third_party_invite.signed`, sent by whoever sent the
//! `m.room.third_party_invite`, which the auth rules accept only if the signature verifies
//! against one of the room's stored public keys (`hs_state::auth`'s `third_party_invite` step).
//! A join with `third_party_signed` does the same exchange first, then joins.
//!
//! Before an exchange, the identity server is asked whether the stored keys are still valid
//! (`key_validity_url`): a revoked key, or an identity server that cannot be reached, refuses
//! the invite, as Synapse does.
//!
//! The identity server is reached through [`IdentityService`], which `hs-cli` installs
//! (`RoomRegistry::install_identity_service`) and which allows only the identity servers
//! `auth.identity_servers` names: any other, and every one while the setting is empty (the
//! default), is refused `403 M_THREEPID_DENIED` -- this server makes requests only to identity
//! servers its operator chose. Without an installed service (this crate's tests) every 3PID
//! invite is refused the same way. Counted in `hs_room_third_party_invites_total{outcome}`.
//!
//! Not done: an exchange for a room this server does not hold (the invitee's server forwarding
//! to the room's, `PUT /_matrix/federation/v1/exchange_third_party_invite/{roomId}`) is refused.

use std::sync::LazyLock;

use async_trait::async_trait;
use hs_kv::KvBackend;
use prometheus_client::encoding::EncodeLabelSet;
use prometheus_client::metrics::counter::Counter;
use prometheus_client::metrics::family::Family;
use ruma::{OwnedUserId, RoomId, UserId};
use serde_json::{Value, json};

use crate::error::RoomError;
use crate::membership::Action;
use crate::state::RoomState;

/// What an identity server answered `store-invite` with.
#[derive(Debug, Clone, PartialEq)]
pub struct StoredInvite {
    /// The invitation's token: the `m.room.third_party_invite` event's `state_key`.
    pub token: String,
    /// A redacted form of the address, safe to show to the room ("Bob", "b...@e...").
    pub display_name: String,
    /// The identity server's long-term public key (unpadded base64), for older clients.
    pub public_key: String,
    /// Every key the invitation may be signed with, each with the URL that says whether it is
    /// still valid: `[{"public_key": ..., "key_validity_url": ...}]`.
    pub public_keys: Vec<Value>,
}

/// The identity-server calls a 3PID invite needs. `hs-cli` implements it over HTTPS.
#[async_trait]
pub trait IdentityService: Send + Sync {
    /// Whether this server may ask `id_server` (`host[:port]`) anything: whether
    /// `auth.identity_servers` names it.
    fn allows(&self, id_server: &str) -> bool;

    /// Who `address` (of `medium`, such as `email`) is bound to on `id_server`, if anybody.
    async fn lookup(
        &self,
        id_server: &str,
        id_access_token: Option<&str>,
        medium: &str,
        address: &str,
    ) -> Result<Option<OwnedUserId>, RoomError>;

    /// Asks `id_server` to keep an invitation for `address` until it is bound (`store-invite`).
    /// `request` is the spec's body: `medium`, `address`, `room_id`, `sender`, and what the room
    /// is called for the invitation email.
    async fn store_invite(
        &self,
        id_server: &str,
        id_access_token: Option<&str>,
        request: Value,
    ) -> Result<StoredInvite, RoomError>;

    /// Whether `public_key` is still valid, by asking `key_validity_url`.
    async fn key_is_valid(
        &self,
        key_validity_url: &str,
        public_key: &str,
    ) -> Result<bool, RoomError>;
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, EncodeLabelSet)]
struct OutcomeLabels {
    outcome: &'static str,
}

static OUTCOMES: LazyLock<Family<OutcomeLabels, Counter>> = LazyLock::new(Family::default);

fn count(outcome: &'static str) {
    OUTCOMES.get_or_create(&OutcomeLabels { outcome }).inc();
}

/// Registers `hs_room_third_party_invites_total{outcome}`: `invited` (the address was bound, so
/// an ordinary invite), `stored` (an `m.room.third_party_invite` was sent), `exchanged` (a bound
/// address turned into an invite), `refused` (no identity server allowed, or the identity server
/// or the auth rules said no).
pub fn register_metrics(registry: &mut prometheus_client::registry::Registry) {
    registry.register(
        "hs_room_third_party_invites",
        "Invitations by email or other third-party identifier, by outcome: invited, stored, \
         exchanged, refused",
        OUTCOMES.clone(),
    );
}

/// The `403 M_THREEPID_DENIED` every 3PID invite gets when this server may not use the identity
/// server it names.
fn denied(id_server: &str) -> RoomError {
    count("refused");
    RoomError::ThreepidDenied(format!(
        "this server does not use the identity server {id_server:?}; an administrator can add \
         it to auth.identity_servers"
    ))
}

/// Whether `body` (an `/invite` body) is a third-party invite: it names an address, not a user.
#[must_use]
pub fn is_third_party(body: &Value) -> bool {
    body.get("user_id").is_none() && body.get("address").is_some() && body.get("medium").is_some()
}

fn field<'a>(body: &'a Value, key: &str) -> Result<&'a str, RoomError> {
    body.get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| RoomError::BadRequest(format!("a third-party invite needs {key}")))
}

/// Invites the owner of `body`'s `medium`/`address` to `room_id` on `sender`'s behalf: an
/// ordinary invite when the identity server knows who that is, otherwise an
/// `m.room.third_party_invite` the invitee can claim once they bind the address. See the module
/// docs.
///
/// # Errors
/// [`RoomError::ThreepidDenied`] without an identity service or for an identity server it does
/// not allow; [`RoomError::BadRequest`] for a body missing a field; whatever the identity server
/// or the room answers.
pub async fn invite<B: KvBackend + 'static>(
    state: &RoomState<B>,
    room_id: &RoomId,
    sender: &UserId,
    body: &Value,
) -> Result<(), RoomError> {
    let id_server = field(body, "id_server")?;
    let medium = field(body, "medium")?;
    let address = field(body, "address")?;
    let id_access_token = body.get("id_access_token").and_then(Value::as_str);
    let Some(service) = state.rooms.identity_service().cloned() else {
        return Err(denied(id_server));
    };
    if !service.allows(id_server) {
        return Err(denied(id_server));
    }
    let handle = state.rooms.get_or_load(room_id).await?;
    if let Some(invitee) = service
        .lookup(id_server, id_access_token, medium, address)
        .await?
    {
        tracing::info!(%room_id, inviter = %sender, %invitee, medium, "a third-party invite named a bound address; inviting its owner");
        let mut extra = json!({});
        crate::routes::membership::fill_in_profile(state, &invitee, &mut extra).await;
        let remote = invitee.server_name() != &*state.identity.server_name;
        match (&state.remote_join, remote) {
            (Some(hook), true) => {
                crate::routes::membership::invite_remote(
                    hook.as_ref(),
                    &handle,
                    sender.to_owned(),
                    invitee,
                    extra,
                )
                .await?;
            }
            _ => {
                handle
                    .membership(sender.to_owned(), Action::Invite, invitee, extra, now_ms())
                    .await?;
            }
        }
        count("invited");
        return Ok(());
    }
    let (room_name, room_alias) = handle
        .query(|actor| {
            let text = |event_type: &str, key: &str| {
                actor
                    .state_event(event_type, "")
                    .ok()
                    .flatten()
                    .and_then(|e| {
                        e.json()
                            .get("content")
                            .and_then(hs_model::canonical::CanonicalJsonValue::as_object)
                            .and_then(|c| c.get(key))
                            .and_then(hs_model::canonical::CanonicalJsonValue::as_str)
                            .map(str::to_owned)
                    })
            };
            (
                text("m.room.name", "name"),
                text("m.room.canonical_alias", "alias"),
            )
        })
        .await;
    let mut request = json!({
        "medium": medium,
        "address": address,
        "room_id": room_id,
        "sender": sender,
    });
    if let Some(name) = room_name {
        request["room_name"] = Value::String(name);
    }
    if let Some(alias) = room_alias {
        request["room_alias"] = Value::String(alias);
    }
    let stored = service
        .store_invite(id_server, id_access_token, request)
        .await?;
    let key_validity_url = stored
        .public_keys
        .first()
        .and_then(|k| k.get("key_validity_url"))
        .cloned()
        .unwrap_or(Value::Null);
    handle
        .send_event(
            sender.to_owned(),
            "m.room.third_party_invite".to_owned(),
            Some(stored.token.clone()),
            json!({
                "display_name": stored.display_name,
                "key_validity_url": key_validity_url,
                "public_key": stored.public_key,
                "public_keys": stored.public_keys,
            }),
            None,
            now_ms(),
        )
        .await?;
    count("stored");
    tracing::info!(%room_id, inviter = %sender, medium, "stored a third-party invite with the identity server");
    Ok(())
}

/// Turns a bound third-party invitation into an `m.room.member` invite of `signed.mxid` in
/// `room_id`, sent by whoever sent the matching `m.room.third_party_invite`, carrying
/// `third_party_invite: {display_name, signed}` for the auth rules to check. The keys the room
/// stored are first checked with the identity server (`key_validity_url`); a revoked key or an
/// identity server that does not answer refuses the exchange.
///
/// # Errors
/// [`RoomError::BadRequest`] for a `signed` block without `mxid` or `token`;
/// [`RoomError::Forbidden`] if the room has no matching `m.room.third_party_invite`, its keys are
/// not valid, or the auth rules reject the invite; [`RoomError::RoomNotFound`] for a room this
/// server does not hold.
pub async fn exchange<B: KvBackend + 'static>(
    state: &RoomState<B>,
    room_id: &RoomId,
    signed: &Value,
) -> Result<OwnedUserId, RoomError> {
    let mxid = UserId::parse(field(signed, "mxid")?)
        .map_err(|e| RoomError::BadRequest(format!("signed.mxid: {e}")))?;
    let token = field(signed, "token")?.to_owned();
    let handle = state.rooms.get_or_load(room_id).await?;
    let wanted = token.clone();
    let invitation = handle
        .query(move |actor| {
            actor
                .state_event("m.room.third_party_invite", &wanted)
                .ok()
                .flatten()
                .map(|e| {
                    let content = e
                        .json()
                        .get("content")
                        .and_then(|c| serde_json::from_slice(&c.to_canonical_bytes()).ok())
                        .unwrap_or(Value::Null);
                    (e.header().sender.clone(), content)
                })
        })
        .await;
    let Some((sender, content)) = invitation else {
        count("refused");
        return Err(RoomError::Forbidden(
            "no third-party invitation in this room matches the token".into(),
        ));
    };
    if let Some(service) = state.rooms.identity_service().cloned() {
        let mut keys: Vec<(String, String)> = content
            .get("public_keys")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|k| {
                Some((
                    k.get("public_key")?.as_str()?.to_owned(),
                    k.get("key_validity_url")?.as_str()?.to_owned(),
                ))
            })
            .collect();
        if keys.is_empty()
            && let (Some(key), Some(url)) = (
                content.get("public_key").and_then(Value::as_str),
                content.get("key_validity_url").and_then(Value::as_str),
            )
        {
            keys.push((key.to_owned(), url.to_owned()));
        }
        let mut any_valid = false;
        for (key, url) in &keys {
            match service.key_is_valid(url, key).await {
                Ok(true) => any_valid = true,
                Ok(false) => {}
                Err(error) => {
                    tracing::info!(%room_id, %error, "could not check a third-party invite's key with its identity server");
                }
            }
        }
        if !any_valid {
            count("refused");
            return Err(RoomError::Forbidden(
                "the identity server no longer vouches for this third-party invitation's keys"
                    .into(),
            ));
        }
    }
    let display_name = content
        .get("display_name")
        .cloned()
        .unwrap_or(Value::String(String::new()));
    let mut extra = json!({
        "third_party_invite": {"display_name": display_name, "signed": signed},
    });
    crate::routes::membership::fill_in_profile(state, &mxid, &mut extra).await;
    match handle
        .membership(
            sender.clone(),
            Action::Invite,
            mxid.clone(),
            extra,
            now_ms(),
        )
        .await
    {
        Ok(_) => {
            count("exchanged");
            tracing::info!(%room_id, invitee = %mxid, inviter = %sender, "a bound third-party invitation became an invite");
            Ok(mxid)
        }
        Err(error) => {
            count("refused");
            tracing::info!(%room_id, invitee = %mxid, %error, "a third-party invitation was refused");
            Err(error)
        }
    }
}

/// `PUT /_matrix/federation/v1/3pid/onbind`'s work: an identity server says `mxid` has bound an
/// address that has pending invitations; each invitation for a room this server holds becomes an
/// invite ([`exchange`]). One that fails is logged and the rest go on. Returns how many became
/// invites.
pub async fn on_bind<B: KvBackend + 'static>(state: &RoomState<B>, body: &Value) -> usize {
    let mut exchanged = 0;
    for invite in body
        .get("invites")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let Some(room_id) = invite
            .get("room_id")
            .and_then(Value::as_str)
            .and_then(|r| RoomId::parse(r).ok())
        else {
            continue;
        };
        let Some(signed) = invite.get("signed") else {
            continue;
        };
        match exchange(state, &room_id, signed).await {
            Ok(_) => exchanged += 1,
            Err(error) => {
                tracing::info!(%room_id, %error, "could not turn a bound third-party invitation into an invite");
            }
        }
    }
    exchanged
}

fn now_ms() -> i64 {
    i64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis(),
    )
    .unwrap_or(i64::MAX)
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use base64::Engine;
    use hs_auth::state::AuthState;
    use hs_kv::memory::MemoryBackend;
    use ruma::user_id;

    use super::*;
    use crate::identity::HomeserverIdentity;
    use crate::registry::RoomRegistry;

    const ID_SERVER: &str = "id.example";

    fn b64(bytes: &[u8]) -> String {
        base64::engine::general_purpose::STANDARD_NO_PAD.encode(bytes)
    }

    /// An identity server in memory: one key, the binding it was given, and whether it still
    /// vouches for its key.
    struct FakeIdentityServer {
        key: ed25519_dalek::SigningKey,
        bound: Mutex<Option<OwnedUserId>>,
        key_valid: Mutex<bool>,
    }

    impl FakeIdentityServer {
        fn with_key(seed: u8) -> Self {
            Self {
                key: ed25519_dalek::SigningKey::from_bytes(&[seed; 32]),
                bound: Mutex::new(None),
                key_valid: Mutex::new(true),
            }
        }

        fn public_key(&self) -> String {
            b64(&self.key.verifying_key().to_bytes())
        }

        /// The `signed` block an identity server sends with `onbind`.
        fn sign(&self, mxid: &UserId, token: &str) -> Value {
            let signed = json!({"mxid": mxid, "token": token});
            let canonical = hs_model::canonical::to_canonical_value(&signed, true).unwrap();
            let signature = ed25519_dalek::Signer::sign(&self.key, &canonical.to_canonical_bytes());
            let mut signed = signed;
            signed["signatures"] = json!({ID_SERVER: {"ed25519:0": b64(&signature.to_bytes())}});
            signed
        }
    }

    #[async_trait]
    impl IdentityService for FakeIdentityServer {
        fn allows(&self, id_server: &str) -> bool {
            id_server == ID_SERVER
        }

        async fn lookup(
            &self,
            _id_server: &str,
            _id_access_token: Option<&str>,
            _medium: &str,
            _address: &str,
        ) -> Result<Option<OwnedUserId>, RoomError> {
            Ok(self.bound.lock().unwrap().clone())
        }

        async fn store_invite(
            &self,
            _id_server: &str,
            _id_access_token: Option<&str>,
            _request: Value,
        ) -> Result<StoredInvite, RoomError> {
            Ok(StoredInvite {
                token: "tok1".to_owned(),
                display_name: "b...@e...".to_owned(),
                public_key: self.public_key(),
                public_keys: vec![json!({
                    "public_key": self.public_key(),
                    "key_validity_url": "https://id.example/_matrix/identity/v2/pubkey/isvalid",
                })],
            })
        }

        async fn key_is_valid(&self, _url: &str, public_key: &str) -> Result<bool, RoomError> {
            Ok(*self.key_valid.lock().unwrap() && public_key == self.public_key())
        }
    }

    async fn room_with(
        service: Option<Arc<FakeIdentityServer>>,
    ) -> (RoomState<MemoryBackend>, ruma::OwnedRoomId) {
        let identity = HomeserverIdentity::for_tests("hs1");
        let rooms = Arc::new(RoomRegistry::open(MemoryBackend::new(), identity.clone()).unwrap());
        if let Some(service) = service {
            rooms.install_identity_service(service);
        }
        let state = RoomState {
            auth: AuthState::in_memory(),
            rooms,
            identity,
            remote_join: None,
        };
        let handle = state
            .rooms
            .create_room(
                user_id!("@alice:hs1").to_owned(),
                crate::actor::CreateRoomRequest {
                    preset: Some("private_chat".to_owned()),
                    ..Default::default()
                },
                1,
            )
            .await
            .unwrap();
        let room_id = handle.query(|actor| actor.room_id().to_owned()).await;
        (state, room_id)
    }

    fn by_email(id_server: &str) -> Value {
        json!({"id_server": id_server, "id_access_token": "t", "medium": "email", "address": "bob@example.org"})
    }

    async fn membership(
        state: &RoomState<MemoryBackend>,
        room_id: &RoomId,
        user: &str,
    ) -> Option<Value> {
        let handle = state.rooms.get_or_load(room_id).await.unwrap();
        let user = user.to_owned();
        handle
            .query(move |actor| {
                actor
                    .state_event("m.room.member", &user)
                    .unwrap()
                    .and_then(|e| e.json().get("content").cloned())
                    .map(|c| serde_json::from_slice(&c.to_canonical_bytes()).unwrap())
            })
            .await
    }

    #[tokio::test]
    async fn without_an_identity_server_this_server_may_use_an_email_invite_is_denied() {
        let alice = user_id!("@alice:hs1");
        let (state, room_id) = room_with(None).await;
        let err = invite(&state, &room_id, alice, &by_email(ID_SERVER))
            .await
            .unwrap_err();
        assert!(matches!(err, RoomError::ThreepidDenied(_)), "{err}");

        let (state, room_id) = room_with(Some(Arc::new(FakeIdentityServer::with_key(9)))).await;
        let err = invite(&state, &room_id, alice, &by_email("other.example"))
            .await
            .unwrap_err();
        assert!(matches!(err, RoomError::ThreepidDenied(_)), "{err}");
    }

    #[tokio::test]
    async fn a_bound_address_is_an_ordinary_invite_of_its_owner() {
        let ids = Arc::new(FakeIdentityServer::with_key(9));
        *ids.bound.lock().unwrap() = Some(user_id!("@bob:hs1").to_owned());
        let (state, room_id) = room_with(Some(ids)).await;
        invite(
            &state,
            &room_id,
            user_id!("@alice:hs1"),
            &by_email(ID_SERVER),
        )
        .await
        .unwrap();
        let bob = membership(&state, &room_id, "@bob:hs1").await.unwrap();
        assert_eq!(bob["membership"], "invite");
    }

    #[tokio::test]
    async fn an_unbound_address_is_stored_and_its_binding_becomes_an_invite() {
        let ids = Arc::new(FakeIdentityServer::with_key(9));
        let (state, room_id) = room_with(Some(ids.clone())).await;
        invite(
            &state,
            &room_id,
            user_id!("@alice:hs1"),
            &by_email(ID_SERVER),
        )
        .await
        .unwrap();
        let handle = state.rooms.get_or_load(&room_id).await.unwrap();
        let stored = handle
            .query(|actor| {
                actor
                    .state_event("m.room.third_party_invite", "tok1")
                    .unwrap()
                    .is_some()
            })
            .await;
        assert!(stored, "the room holds the invitation");

        // A signature by another key is refused by the auth rules.
        let impostor = FakeIdentityServer::with_key(1);
        let bob = user_id!("@bob:hs1");
        assert!(
            exchange(&state, &room_id, &impostor.sign(bob, "tok1"))
                .await
                .is_err()
        );
        assert!(membership(&state, &room_id, "@bob:hs1").await.is_none());

        // The identity server's own signature, through `onbind`.
        let body = json!({
            "mxid": bob,
            "invites": [{"room_id": room_id, "sender": "@alice:hs1", "mxid": bob, "signed": ids.sign(bob, "tok1")}],
        });
        assert_eq!(on_bind(&state, &body).await, 1);
        let invited = membership(&state, &room_id, "@bob:hs1").await.unwrap();
        assert_eq!(invited["membership"], "invite");
        assert_eq!(invited["third_party_invite"]["display_name"], "b...@e...");
    }

    #[tokio::test]
    async fn a_key_the_identity_server_no_longer_vouches_for_is_refused() {
        let ids = Arc::new(FakeIdentityServer::with_key(9));
        let (state, room_id) = room_with(Some(ids.clone())).await;
        invite(
            &state,
            &room_id,
            user_id!("@alice:hs1"),
            &by_email(ID_SERVER),
        )
        .await
        .unwrap();
        *ids.key_valid.lock().unwrap() = false;
        let bob = user_id!("@bob:hs1");
        let err = exchange(&state, &room_id, &ids.sign(bob, "tok1"))
            .await
            .unwrap_err();
        assert!(matches!(err, RoomError::Forbidden(_)), "{err}");
        assert!(membership(&state, &room_id, "@bob:hs1").await.is_none());
    }
}
