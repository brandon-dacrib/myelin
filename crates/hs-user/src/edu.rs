//! Typing, receipts and presence across servers: what this crate hands the federation sender
//! ([`EduOutbox`]), and what it does with the same three EDUs arriving from another server
//! ([`InboundEdu`], applied by `crate::hub::SessionHub::receive_edu`).
//!
//! # Outbound
//!
//! `hs-federation` owns the sender and knows nothing about rooms; this crate knows who is in a
//! room and nothing about transactions. [`EduOutbox`] is the seam between them: the hub builds
//! the spec's EDU content and names the servers that should receive it, and whatever `hs-cli`
//! installs ([`crate::hub::SessionHub::install_edu_outbox`]) queues it. Only this server's own
//! users' changes are sent -- a remote user's typing is its own server's to distribute.
//!
//! | change | EDU | sent to |
//! |---|---|---|
//! | a local user starts or stops typing, or their typing lapses | `m.typing` | the servers of the room's joined members |
//! | a local user's `m.read` receipt | `m.receipt` | the servers of the room's joined members |
//! | a local user's presence changes | `m.presence` | the servers of everyone they share a joined room with |
//!
//! `m.read.private` is never sent anywhere: it is private to its sender, and its sender's own
//! server is the only one that needs it.
//!
//! # Inbound
//!
//! The spec's rule for every one of these: an EDU speaks only for users of the server that sent
//! it (`origin`). An `m.typing` or `m.receipt` for a user who is not joined to the room here is
//! dropped (Synapse does the same; it would otherwise be a way to make a stranger appear in a
//! room). Presence is recorded for any user of the origin server and reaches only people who
//! share a room with them, which is the scope `/sync` already applies.

use std::collections::BTreeSet;
use std::time::Duration;

use ruma::{OwnedEventId, OwnedRoomId, OwnedUserId, UserId};
use serde_json::{Value, json};

/// How long a remote user's `typing: true` lasts here without a renewal. Their own server sends
/// `typing: false` when they stop or their timeout lapses; this bounds how long a lost stop can
/// leave them "typing". Synapse's `FEDERATION_TIMEOUT` is the same minute.
pub const REMOTE_TYPING_TIMEOUT: Duration = Duration::from_secs(60);

/// Where this crate hands an EDU for other servers. Implemented in `hs-cli` over
/// `hs_federation::sender::FederationSender`. Never blocks and never fails: a server that cannot
/// be reached is the sender's problem, and an EDU is not worth failing a client's request over.
pub trait EduOutbox: Send + Sync {
    /// Queues one EDU for every server in `destinations` (this server's own name among them is
    /// the implementation's to skip). `coalesce_key`, when given, lets a newer EDU replace an
    /// older unsent one with the same key for the same destination -- a typing or presence
    /// update supersedes the one before it; a receipt for a different room does not.
    fn send_edu(
        &self,
        destinations: BTreeSet<String>,
        edu_type: &str,
        content: Value,
        coalesce_key: Option<String>,
    );
}

/// The servers of `users`, deduplicated.
#[must_use]
pub fn servers_of<'a>(users: impl IntoIterator<Item = &'a OwnedUserId>) -> BTreeSet<String> {
    users
        .into_iter()
        .map(|user| user.server_name().to_string())
        .collect()
}

/// `m.typing`'s content.
#[must_use]
pub fn typing_content(room_id: &ruma::RoomId, user_id: &UserId, typing: bool) -> Value {
    json!({"room_id": room_id, "user_id": user_id, "typing": typing})
}

/// `m.receipt`'s content for one `m.read` receipt.
#[must_use]
pub fn receipt_content(
    room_id: &ruma::RoomId,
    user_id: &UserId,
    event_id: &ruma::EventId,
    ts: u64,
) -> Value {
    json!({
        room_id.as_str(): {
            "m.read": {
                user_id.as_str(): {
                    "event_ids": [event_id],
                    "data": {"ts": ts},
                }
            }
        }
    })
}

/// `m.presence`'s content for one user's update.
#[must_use]
pub fn presence_content(user_id: &UserId, record: &crate::presence::PresenceRecord) -> Value {
    let mut push = json!({
        "user_id": user_id,
        "presence": record.presence,
        "last_active_ago": record.last_active_ago_ms(),
        "currently_active": record.currently_active(),
    });
    if let Some(msg) = &record.status_msg {
        push["status_msg"] = Value::String(msg.clone());
    }
    json!({"push": [push]})
}

/// One inbound typing, receipt or presence update, parsed from an EDU and checked against the
/// server that sent it. See [`InboundEdu::parse`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InboundEdu {
    /// `m.typing`.
    Typing {
        /// The room.
        room_id: OwnedRoomId,
        /// Who is (or stopped) typing.
        user_id: OwnedUserId,
        /// Whether they are typing.
        typing: bool,
    },
    /// One user's `m.read` receipt from an `m.receipt` EDU (which may carry many).
    Receipt {
        /// The room.
        room_id: OwnedRoomId,
        /// Whose receipt.
        user_id: OwnedUserId,
        /// The event read up to.
        event_id: OwnedEventId,
        /// When, in milliseconds since the Unix epoch (`0` when the sender gave none).
        ts: u64,
    },
    /// One user's update from an `m.presence` EDU's `push` list.
    Presence {
        /// Whose presence.
        user_id: OwnedUserId,
        /// `online`, `unavailable` or `offline`.
        presence: String,
        /// Their status message, if any.
        status_msg: Option<String>,
        /// How long ago they were last active, as their server said.
        last_active_ago: Option<u64>,
        /// Whether their server says they are currently active.
        currently_active: Option<bool>,
    },
}

impl InboundEdu {
    /// Parses the updates in one EDU of type `edu_type` from `origin`, keeping only those about
    /// `origin`'s own users and in the shape the spec gives. An EDU type this module does not
    /// handle parses to nothing. Malformed entries are skipped, not fatal: one bad receipt in a
    /// batch of fifty does not lose the other forty-nine.
    #[must_use]
    pub fn parse(origin: &str, edu_type: &str, content: &Value) -> Vec<Self> {
        let from_origin = |user: &UserId| user.server_name().as_str() == origin;
        let mut out = Vec::new();
        match edu_type {
            "m.typing" => {
                let room_id = content
                    .get("room_id")
                    .and_then(Value::as_str)
                    .and_then(|r| ruma::RoomId::parse(r).ok());
                let user_id = content
                    .get("user_id")
                    .and_then(Value::as_str)
                    .and_then(|u| UserId::parse(u).ok());
                let typing = content.get("typing").and_then(Value::as_bool);
                if let (Some(room_id), Some(user_id), Some(typing)) = (room_id, user_id, typing)
                    && from_origin(&user_id)
                {
                    out.push(Self::Typing {
                        room_id,
                        user_id,
                        typing,
                    });
                }
            }
            "m.receipt" => {
                let Some(rooms) = content.as_object() else {
                    return out;
                };
                for (room_id, kinds) in rooms {
                    let Ok(room_id) = ruma::RoomId::parse(room_id.as_str()) else {
                        continue;
                    };
                    let Some(users) = kinds.get("m.read").and_then(Value::as_object) else {
                        continue;
                    };
                    for (user_id, receipt) in users {
                        let Ok(user_id) = UserId::parse(user_id.as_str()) else {
                            continue;
                        };
                        if !from_origin(&user_id) {
                            continue;
                        }
                        // The spec's `event_ids` is a list whose only meaningful entry is the
                        // last one read (Synapse sends exactly one).
                        let Some(event_id) = receipt
                            .get("event_ids")
                            .and_then(Value::as_array)
                            .and_then(|ids| ids.last())
                            .and_then(Value::as_str)
                            .and_then(|e| ruma::EventId::parse(e).ok())
                        else {
                            continue;
                        };
                        let ts = receipt
                            .get("data")
                            .and_then(|d| d.get("ts"))
                            .and_then(Value::as_u64)
                            .unwrap_or(0);
                        out.push(Self::Receipt {
                            room_id: room_id.clone(),
                            user_id,
                            event_id,
                            ts,
                        });
                    }
                }
            }
            "m.presence" => {
                let Some(push) = content.get("push").and_then(Value::as_array) else {
                    return out;
                };
                for update in push {
                    let Some(user_id) = update
                        .get("user_id")
                        .and_then(Value::as_str)
                        .and_then(|u| UserId::parse(u).ok())
                    else {
                        continue;
                    };
                    let Some(presence) = update.get("presence").and_then(Value::as_str) else {
                        continue;
                    };
                    if !from_origin(&user_id)
                        || !matches!(presence, "online" | "unavailable" | "offline")
                    {
                        continue;
                    }
                    out.push(Self::Presence {
                        user_id,
                        presence: presence.to_owned(),
                        status_msg: update
                            .get("status_msg")
                            .and_then(Value::as_str)
                            .map(str::to_owned),
                        last_active_ago: update.get("last_active_ago").and_then(Value::as_u64),
                        currently_active: update.get("currently_active").and_then(Value::as_bool),
                    });
                }
            }
            _ => {}
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ruma::{event_id, room_id, user_id};

    #[test]
    fn typing_round_trips_and_a_user_of_another_server_is_dropped() {
        let content = typing_content(room_id!("!r:a.example"), user_id!("@alice:a.example"), true);
        assert_eq!(
            InboundEdu::parse("a.example", "m.typing", &content),
            vec![InboundEdu::Typing {
                room_id: room_id!("!r:a.example").to_owned(),
                user_id: user_id!("@alice:a.example").to_owned(),
                typing: true,
            }]
        );
        assert!(
            InboundEdu::parse("b.example", "m.typing", &content).is_empty(),
            "b.example cannot say that a user of a.example is typing"
        );
    }

    #[test]
    fn a_receipt_round_trips_and_a_forged_one_is_dropped() {
        let content = receipt_content(
            room_id!("!r:a.example"),
            user_id!("@alice:a.example"),
            event_id!("$e"),
            7,
        );
        assert_eq!(
            InboundEdu::parse("a.example", "m.receipt", &content),
            vec![InboundEdu::Receipt {
                room_id: room_id!("!r:a.example").to_owned(),
                user_id: user_id!("@alice:a.example").to_owned(),
                event_id: event_id!("$e").to_owned(),
                ts: 7,
            }]
        );
        assert!(InboundEdu::parse("b.example", "m.receipt", &content).is_empty());
    }

    #[test]
    fn presence_keeps_only_the_origins_users_and_known_states() {
        let content = json!({"push": [
            {"user_id": "@alice:a.example", "presence": "unavailable", "status_msg": "away",
             "last_active_ago": 5, "currently_active": false},
            {"user_id": "@mallory:b.example", "presence": "online"},
            {"user_id": "@alice2:a.example", "presence": "dancing"},
        ]});
        assert_eq!(
            InboundEdu::parse("a.example", "m.presence", &content),
            vec![InboundEdu::Presence {
                user_id: user_id!("@alice:a.example").to_owned(),
                presence: "unavailable".to_owned(),
                status_msg: Some("away".to_owned()),
                last_active_ago: Some(5),
                currently_active: Some(false),
            }]
        );
    }

    #[test]
    fn an_edu_type_this_module_does_not_handle_parses_to_nothing() {
        assert!(InboundEdu::parse("a.example", "m.device_list_update", &json!({})).is_empty());
    }
}
