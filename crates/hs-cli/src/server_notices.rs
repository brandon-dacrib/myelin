//! Server notices (the Matrix specification's "Server Notices" module), sent through the admin
//! API's `server_notices.*` operations and the Synapse-compatible `send_server_notice`.
//!
//! A notice comes from the server-notices user, [`NOTICES_LOCALPART`] on this server. Each
//! recipient has one server-notices room: created by that user the first time the recipient is
//! sent anything, private, named "Server Notices", with the recipient invited and unable to
//! speak in it (`users_default` below `events_default`), and tagged `m.server_notice` in the
//! recipient's room account data, which is how a client knows to show it as a notice from the
//! server. Every later notice goes to the same room; a recipient who left it is invited back.
//! The recipient cannot reject the invitation (`hs_room`'s `post_leave`, which knows the room
//! by its creator), but may leave once joined.
//!
//! This lives here, where the server is assembled, because it needs three things no single
//! crate has: accounts (`hs-auth`, to create the server-notices user and check recipients),
//! rooms (`hs-room`) and room account data (`hs-user`, for the tag).
//!
//! # Storage
//!
//! Three keyspaces over the server's backend:
//!
//! - `hs_admin.server_notices`: `(sequence, id)` to the notice as sent, the history
//!   `GET /server-notices` reads newest first.
//! - `hs_admin.server_notice_rooms`: `(user_id,)` to the recipient's room id.
//! - `hs_admin.server_notices_meta`: `("user",)` to the server-notices user's id once this
//!   module has created that account. An account with that name that this module did *not*
//!   create -- somebody registered the name first -- is never sent as: every notice would
//!   otherwise hand that person a room with every recipient in it.
//!
//! Sends are serialized in this process, so two notices to a new recipient at once make one
//! room, not two. Two replicas of a cluster could still race on a recipient's first notice;
//! the second room would be orphaned (the mapping keeps the last one written).

use std::ops::Bound;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use hs_admin::server_notices::{AdminServerNotice, ServerNoticeRequest, ServerNoticeSource};
use hs_admin::sources::SourceError;
use hs_auth::state::AuthState;
use hs_auth::store::UserRecord;
use hs_kv::{KvBackend, KvError, RangeSpec, TransactConfig, transact};
use hs_room::error::RoomError;
use hs_room::membership::Action;
use hs_room::registry::RoomRegistry;
use hs_tables::keyspace::TypedKeyspace;
use ruma::{OwnedRoomId, OwnedUserId, RoomId, UserId};
use serde_json::{Value, json};

/// The server-notices user's localpart: `@_server:<server name>`, the name Synapse deployments
/// (and Complement's `TestServerNotices`) use.
pub const NOTICES_LOCALPART: &str = "_server";

/// The server-notices user's display name, and the name of each server-notices room.
pub const NOTICES_NAME: &str = "Server Notices";

/// The most history rows `list` returns.
const MAX_LISTED: usize = 10_000;

/// The server-notices user of a server called `server_name`.
///
/// # Errors
/// If `server_name` does not make a valid user ID.
pub fn notices_user_id(server_name: &str) -> Result<OwnedUserId, ruma::IdParseError> {
    UserId::parse(format!("@{NOTICES_LOCALPART}:{server_name}"))
}

fn now_ms() -> i64 {
    i64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis(),
    )
    .unwrap_or(i64::MAX)
}

fn unavailable(e: impl std::fmt::Display) -> SourceError {
    SourceError::Unavailable(e.to_string())
}

fn room_error(e: RoomError) -> SourceError {
    match e {
        RoomError::Forbidden(detail) => SourceError::Conflict(detail),
        other => SourceError::Unavailable(other.to_string()),
    }
}

/// Sends server notices over the room registry and remembers them. See the module docs.
pub struct ServerNotices<B: KvBackend> {
    backend: B,
    auth: AuthState,
    rooms: Arc<RoomRegistry<B>>,
    account_data: hs_user::store::DynUserStore,
    user_id: OwnedUserId,
    history: TypedKeyspace<B::Keyspace, (u64, String)>,
    room_of: TypedKeyspace<B::Keyspace, (String,)>,
    meta: TypedKeyspace<B::Keyspace, (String,)>,
    /// One send at a time, so a recipient's first two notices make one room.
    sending: tokio::sync::Mutex<()>,
}

impl<B: KvBackend + 'static> ServerNotices<B> {
    /// Opens the keyspaces and installs the server-notices user on the room registry, so its
    /// rooms' invitations cannot be rejected.
    ///
    /// # Errors
    /// If a keyspace cannot be opened, or the server name does not make a user ID.
    pub fn open(
        backend: B,
        auth: AuthState,
        rooms: Arc<RoomRegistry<B>>,
        account_data: hs_user::store::DynUserStore,
    ) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let user_id = notices_user_id(auth.server_name().as_str())?;
        rooms.install_server_notices_user(user_id.clone());
        Ok(Self {
            history: TypedKeyspace::new(backend.keyspace("hs_admin.server_notices")?),
            room_of: TypedKeyspace::new(backend.keyspace("hs_admin.server_notice_rooms")?),
            meta: TypedKeyspace::new(backend.keyspace("hs_admin.server_notices_meta")?),
            backend,
            auth,
            rooms,
            account_data,
            user_id,
            sending: tokio::sync::Mutex::new(()),
        })
    }

    /// The user notices are sent as.
    #[must_use]
    pub fn user_id(&self) -> &UserId {
        &self.user_id
    }

    fn meta_get(&self, key: &str) -> Result<Option<String>, SourceError> {
        let snap = self.backend.snapshot();
        Ok(self
            .meta
            .get(&snap, &(key.to_owned(),))
            .map_err(unavailable)?
            .map(|b| String::from_utf8_lossy(&b).into_owned()))
    }

    fn put(
        &self,
        keyspace: &TypedKeyspace<B::Keyspace, (String,)>,
        key: &str,
        value: &str,
    ) -> Result<(), SourceError> {
        transact(&self.backend, TransactConfig::default(), |txn| {
            keyspace
                .put(txn, &(key.to_owned(),), value.as_bytes())
                .map_err(KvError::backend)
        })
        .map_err(unavailable)
    }

    /// Creates the server-notices account the first time it is needed, and refuses to send as
    /// one this module did not create.
    async fn ensure_user(&self) -> Result<(), SourceError> {
        let mine = self.meta_get("user")?.as_deref() == Some(self.user_id.as_str());
        let existing = self
            .auth
            .store
            .get_user(&self.user_id)
            .await
            .map_err(unavailable)?;
        match (existing, mine) {
            (Some(_), true) => Ok(()),
            (Some(_), false) => Err(SourceError::Conflict(format!(
                "{} is an ordinary account (somebody registered the server-notices name), so \
                 notices cannot be sent as it",
                self.user_id
            ))),
            (None, _) => {
                let mut record = UserRecord::new(
                    self.user_id.clone(),
                    u64::try_from(now_ms()).unwrap_or_default(),
                );
                record.display_name = Some(NOTICES_NAME.to_owned());
                self.auth
                    .store
                    .create_user(record)
                    .await
                    .map_err(unavailable)?;
                self.put(&self.meta, "user", self.user_id.as_str())
            }
        }
    }

    /// Checks every recipient is an account on this server that can receive a notice.
    async fn check_recipients(
        &self,
        recipients: &[String],
    ) -> Result<Vec<OwnedUserId>, SourceError> {
        let mut out = Vec::with_capacity(recipients.len());
        for recipient in recipients {
            let invalid = |detail: String| SourceError::InvalidField {
                pointer: "/recipients",
                detail,
            };
            let user_id = UserId::parse(recipient.as_str())
                .map_err(|_| invalid(format!("{recipient} is not a user ID")))?;
            if user_id.server_name() != self.auth.server_name() {
                return Err(invalid(format!(
                    "{recipient} is not a user of this server; notices go to local users only"
                )));
            }
            if user_id == self.user_id {
                return Err(invalid(format!(
                    "{recipient} is the server-notices user itself"
                )));
            }
            match self
                .auth
                .store
                .get_user(&user_id)
                .await
                .map_err(unavailable)?
            {
                None => return Err(invalid(format!("{recipient} does not exist"))),
                Some(u) if u.deactivated => {
                    return Err(invalid(format!("{recipient} is deactivated")));
                }
                Some(_) => {}
            }
            out.push(user_id);
        }
        Ok(out)
    }

    /// The recipient's server-notices room, created (and tagged) if they have none, with the
    /// recipient joined or invited.
    async fn room_for(&self, recipient: &UserId) -> Result<OwnedRoomId, SourceError> {
        let snap = self.backend.snapshot();
        let known = self
            .room_of
            .get(&snap, &(recipient.to_string(),))
            .map_err(unavailable)?
            .and_then(|b| RoomId::parse(String::from_utf8_lossy(&b).as_ref()).ok());
        drop(snap);

        if let Some(room_id) = known {
            match self.rooms.get_or_load(&room_id).await {
                Ok(handle) => {
                    let who = recipient.to_owned();
                    let membership = handle
                        .query(move |actor| {
                            actor
                                .state_event("m.room.member", who.as_str())
                                .ok()
                                .flatten()
                                .and_then(|e| {
                                    e.json()
                                        .get("content")
                                        .and_then(|c| c.as_object())
                                        .and_then(|c| c.get("membership"))
                                        .and_then(|m| m.as_str())
                                        .map(str::to_owned)
                                })
                        })
                        .await;
                    if !matches!(membership.as_deref(), Some("join" | "invite")) {
                        handle
                            .membership(
                                self.user_id.clone(),
                                Action::Invite,
                                recipient.to_owned(),
                                json!({}),
                                now_ms(),
                            )
                            .await
                            .map_err(room_error)?;
                    }
                    self.tag(recipient, &room_id).await?;
                    return Ok(room_id);
                }
                // The room is gone (purged): make a new one below.
                Err(RoomError::RoomNotFound(_)) => {}
                Err(e) => return Err(room_error(e)),
            }
        }

        let mut member_content = std::collections::HashMap::new();
        member_content.insert(self.user_id.clone(), json!({ "displayname": NOTICES_NAME }));
        let handle = self
            .rooms
            .create_room(
                self.user_id.clone(),
                hs_room::actor::CreateRoomRequest {
                    preset: Some("private_chat".to_owned()),
                    name: Some(NOTICES_NAME.to_owned()),
                    invite: vec![recipient.to_owned()],
                    // The recipient reads; only the server speaks.
                    power_level_content_override: Some(json!({ "users_default": -10 })),
                    member_content,
                    ..Default::default()
                },
                now_ms(),
            )
            .await
            .map_err(room_error)?;
        let room_id = handle.query(|actor| actor.room_id().to_owned()).await;
        self.put(&self.room_of, recipient.as_str(), room_id.as_str())?;
        self.tag(recipient, &room_id).await?;
        Ok(room_id)
    }

    /// Adds `m.server_notice` to the recipient's `m.tag` for the room, keeping their other tags.
    async fn tag(&self, recipient: &UserId, room_id: &RoomId) -> Result<(), SourceError> {
        let existing = self
            .account_data
            .list_room_account_data(recipient, room_id)
            .await
            .map_err(unavailable)?
            .into_iter()
            .find(|a| a.event_type == "m.tag")
            .map(|a| a.content);
        let mut content = existing.unwrap_or_else(|| json!({ "tags": {} }));
        if content.get("tags").and_then(Value::as_object).is_none() {
            content = json!({ "tags": {} });
        }
        if content["tags"].get("m.server_notice").is_some() {
            return Ok(());
        }
        content["tags"]["m.server_notice"] = json!({});
        self.account_data
            .put_room_account_data(recipient, room_id, "m.tag", content)
            .await
            .map_err(unavailable)?;
        Ok(())
    }

    fn record(&self, notice: &AdminServerNotice) -> Result<(), SourceError> {
        let value = serde_json::to_vec(notice).map_err(unavailable)?;
        transact(&self.backend, TransactConfig::default(), |txn| {
            let last = RangeSpec {
                start: Bound::Unbounded,
                end: Bound::Unbounded,
                reverse: true,
                limit: Some(1),
            };
            let next = match self.history.range(txn, last).next() {
                Some(item) => item.map_err(KvError::backend)?.0.0 + 1,
                None => 0,
            };
            self.history
                .put(txn, &(next, notice.id.clone()), &value)
                .map_err(KvError::backend)
        })
        .map_err(unavailable)
    }
}

#[async_trait::async_trait]
impl<B: KvBackend + 'static> ServerNoticeSource for ServerNotices<B> {
    async fn send(&self, request: ServerNoticeRequest) -> Result<AdminServerNotice, SourceError> {
        let _one_at_a_time = self.sending.lock().await;
        let recipients = self.check_recipients(&request.recipients).await?;
        self.ensure_user().await?;

        let mut room_ids = Vec::with_capacity(recipients.len());
        let mut event_ids = Vec::with_capacity(recipients.len());
        for recipient in &recipients {
            let room_id = self.room_for(recipient).await?;
            let handle = self.rooms.get_or_load(&room_id).await.map_err(room_error)?;
            let event = handle
                .send_event(
                    self.user_id.clone(),
                    request.event_type.clone(),
                    request.state_key.clone(),
                    request.content.clone(),
                    None,
                    now_ms(),
                )
                .await
                .map_err(room_error)?;
            room_ids.push(room_id.to_string());
            event_ids.push(event.event_id().to_string());
        }

        let notice = AdminServerNotice {
            id: hs_admin::model::new_id(),
            sender: self.user_id.to_string(),
            event_type: request.event_type,
            content: request.content,
            recipients: recipients.iter().map(ToString::to_string).collect(),
            room_ids,
            event_ids,
            sent_at: hs_http::time::now_rfc3339(),
        };
        // The notices are out; a history that fails to record them is logged, not reported as
        // a failed send (which would invite sending them again).
        if let Err(error) = self.record(&notice) {
            tracing::error!(%error, id = %notice.id, "could not record a sent server notice");
        }
        Ok(notice)
    }

    async fn list(&self) -> Result<Vec<AdminServerNotice>, SourceError> {
        let snap = self.backend.snapshot();
        let newest_first = RangeSpec {
            start: Bound::Unbounded,
            end: Bound::Unbounded,
            reverse: true,
            limit: Some(MAX_LISTED),
        };
        self.history
            .range(&snap, newest_first)
            .map(|item| {
                let (_key, bytes) = item.map_err(unavailable)?;
                serde_json::from_slice(&bytes).map_err(unavailable)
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use hs_kv::memory::MemoryBackend;

    use super::*;

    struct Fixture {
        notices: ServerNotices<MemoryBackend>,
        rooms: Arc<RoomRegistry<MemoryBackend>>,
        account_data: hs_user::store::DynUserStore,
        auth: AuthState,
    }

    async fn fixture() -> Fixture {
        let backend = MemoryBackend::new();
        let auth = AuthState::in_memory();
        let identity = hs_room::identity::HomeserverIdentity::for_tests("example.org");
        let rooms = Arc::new(RoomRegistry::open(backend.clone(), identity).unwrap());
        let account_data: hs_user::store::DynUserStore =
            Arc::new(hs_user::store::tables::TablesUserStore::open(backend.clone()).unwrap());
        for name in ["alice", "bob"] {
            auth.store
                .create_user(UserRecord::new(
                    UserId::parse(format!("@{name}:example.org")).unwrap(),
                    0,
                ))
                .await
                .unwrap();
        }
        let notices =
            ServerNotices::open(backend, auth.clone(), rooms.clone(), account_data.clone())
                .unwrap();
        Fixture {
            notices,
            rooms,
            account_data,
            auth,
        }
    }

    fn message(recipients: &[&str], body: &str) -> ServerNoticeRequest {
        ServerNoticeRequest {
            recipients: recipients.iter().map(|r| (*r).to_owned()).collect(),
            event_type: "m.room.message".to_owned(),
            content: json!({"msgtype": "m.text", "body": body}),
            state_key: None,
        }
    }

    async fn membership(
        rooms: &RoomRegistry<MemoryBackend>,
        room_id: &str,
        user: &str,
    ) -> Option<String> {
        let handle = rooms
            .get_or_load(&RoomId::parse(room_id).unwrap())
            .await
            .unwrap();
        let user = user.to_owned();
        handle
            .query(move |actor| {
                actor
                    .state_event("m.room.member", &user)
                    .ok()
                    .flatten()
                    .and_then(|e| {
                        e.json()
                            .get("content")
                            .and_then(|c| c.as_object())
                            .and_then(|c| c.get("membership"))
                            .and_then(|m| m.as_str())
                            .map(str::to_owned)
                    })
            })
            .await
    }

    #[tokio::test]
    async fn a_first_notice_makes_a_tagged_room_invites_the_recipient_and_sends_the_message() {
        let f = fixture().await;
        let sent = f
            .notices
            .send(message(&["@alice:example.org"], "hello"))
            .await
            .unwrap();
        assert_eq!(sent.sender, "@_server:example.org");
        let room_id = &sent.room_ids[0];
        assert_eq!(
            membership(&f.rooms, room_id, "@alice:example.org")
                .await
                .as_deref(),
            Some("invite")
        );

        // The server-notices account exists and cannot be signed in to.
        let account = f
            .auth
            .store
            .get_user(&notices_user_id("example.org").unwrap())
            .await
            .unwrap()
            .unwrap();
        assert!(account.password_hash.is_none());
        assert_eq!(account.display_name.as_deref(), Some(NOTICES_NAME));

        let tags = f
            .account_data
            .list_room_account_data(
                &UserId::parse("@alice:example.org").unwrap(),
                &RoomId::parse(room_id.as_str()).unwrap(),
            )
            .await
            .unwrap();
        let tag = tags.iter().find(|a| a.event_type == "m.tag").unwrap();
        assert!(tag.content["tags"].get("m.server_notice").is_some());

        let history = f.notices.list().await.unwrap();
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].event_ids, sent.event_ids);
    }

    #[tokio::test]
    async fn later_notices_reuse_the_room_and_invite_a_recipient_who_left() {
        let f = fixture().await;
        let first = f
            .notices
            .send(message(&["@alice:example.org"], "one"))
            .await
            .unwrap();
        let room_id = RoomId::parse(first.room_ids[0].as_str()).unwrap();
        let alice = UserId::parse("@alice:example.org").unwrap();
        let handle = f.rooms.get_or_load(&room_id).await.unwrap();
        handle
            .membership(alice.clone(), Action::Join, alice.clone(), json!({}), 1)
            .await
            .unwrap();
        handle
            .membership(alice.clone(), Action::Leave, alice.clone(), json!({}), 2)
            .await
            .unwrap();

        let second = f
            .notices
            .send(message(&["@alice:example.org", "@bob:example.org"], "two"))
            .await
            .unwrap();
        assert_eq!(second.room_ids[0], first.room_ids[0]);
        assert_ne!(second.room_ids[1], first.room_ids[0]);
        assert_eq!(
            membership(&f.rooms, &second.room_ids[0], "@alice:example.org")
                .await
                .as_deref(),
            Some("invite")
        );
        let history = f.notices.list().await.unwrap();
        assert_eq!(history.len(), 2);
        assert_eq!(history[0].id, second.id, "newest first");
    }

    #[tokio::test]
    async fn unknown_remote_or_deactivated_recipients_send_nothing() {
        let f = fixture().await;
        for bad in [
            "@nobody:example.org",
            "@alice:elsewhere.org",
            "@_server:example.org",
        ] {
            let err = f
                .notices
                .send(message(&["@alice:example.org", bad], "x"))
                .await
                .unwrap_err();
            assert!(
                matches!(
                    err,
                    SourceError::InvalidField {
                        pointer: "/recipients",
                        ..
                    }
                ),
                "{bad}: {err:?}"
            );
        }
        f.auth
            .store
            .set_deactivated(&UserId::parse("@bob:example.org").unwrap(), true)
            .await
            .unwrap();
        assert!(
            f.notices
                .send(message(&["@bob:example.org"], "x"))
                .await
                .is_err()
        );
        assert!(f.notices.list().await.unwrap().is_empty());
        assert!(
            f.rooms.list_all_room_ids().unwrap().is_empty(),
            "no room was made for a notice that was not sent"
        );
    }

    #[tokio::test]
    async fn a_squatted_server_notices_name_is_never_sent_as() {
        let f = fixture().await;
        f.auth
            .store
            .create_user(UserRecord::new(notices_user_id("example.org").unwrap(), 0))
            .await
            .unwrap();
        let err = f
            .notices
            .send(message(&["@alice:example.org"], "x"))
            .await
            .unwrap_err();
        assert!(matches!(err, SourceError::Conflict(_)), "{err:?}");
    }
}
