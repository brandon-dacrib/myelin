//! [`EventRecord`]: what the production state store keeps durably per event, and its codec.
//!
//! This is [`crate::state_res::ResolutionEvent`] with `auth_events` and `prev_events` as
//! `EventSn`s instead of event IDs (the IDs are translated back on read, through the store's
//! `state_event_id` rows), stored as one compact row per event in the `state_event` keyspace
//! (`crate::durable`). Read only by state resolution and by an explicit-state ingestion; never
//! by an open. The content is the event's canonical JSON bytes.
//!
//! The encoding is hand-rolled over `crate::varint` rather than JSON so a record of a message
//! (which the room actor hands in with empty content and no auth events) is a few dozen bytes.

use hs_model::canonical::{CanonicalJsonObject, CanonicalJsonValue, to_canonical_object};
use hs_model::ids::EventSn;
use ruma::{OwnedEventId, OwnedRoomId, OwnedUserId};
use thiserror::Error;

use crate::varint::{read_uvarint, write_uvarint};

/// The record's format version, the first byte of every row.
const VERSION: u8 = 1;

/// One event as the store keeps it durably. See the module docs.
#[derive(Debug, Clone, PartialEq)]
pub struct EventRecord {
    /// The event's ID.
    pub event_id: OwnedEventId,
    /// The room the event belongs to.
    pub room_id: OwnedRoomId,
    /// The event's `type`.
    pub event_type: String,
    /// The event's `state_key` (empty for a non-state event, as `ResolutionEvent` has it).
    pub state_key: String,
    /// The event's `sender`.
    pub sender: OwnedUserId,
    /// The event's `content`.
    pub content: CanonicalJsonObject,
    /// The event's `depth`.
    pub depth: i64,
    /// The event's `origin_server_ts`.
    pub origin_server_ts: i64,
    /// The event's `auth_events`, as the store's own short IDs of the entries it knew when the
    /// event was ingested.
    pub auth_events: Vec<EventSn>,
    /// The event's `prev_events`, likewise.
    pub prev_events: Vec<EventSn>,
    /// See [`crate::auth::IncomingEvent::only_prev_event_is_room_create`].
    pub only_prev_event_is_room_create: bool,
}

/// A stored record that did not decode.
#[derive(Debug, Error, Clone, Copy, PartialEq, Eq)]
#[error("state event record was corrupt: {0}")]
pub struct RecordError(pub &'static str);

fn write_bytes(out: &mut Vec<u8>, bytes: &[u8]) {
    write_uvarint(out, bytes.len() as u64);
    out.extend_from_slice(bytes);
}

fn read_bytes<'a>(input: &mut &'a [u8]) -> Result<&'a [u8], RecordError> {
    let len = read_uvarint(input).map_err(|()| RecordError("length"))? as usize;
    if input.len() < len {
        return Err(RecordError("truncated"));
    }
    let (bytes, rest) = input.split_at(len);
    *input = rest;
    Ok(bytes)
}

fn read_str<'a>(input: &mut &'a [u8]) -> Result<&'a str, RecordError> {
    std::str::from_utf8(read_bytes(input)?).map_err(|_| RecordError("utf-8"))
}

fn write_sns(out: &mut Vec<u8>, sns: &[EventSn]) {
    write_uvarint(out, sns.len() as u64);
    for sn in sns {
        out.extend_from_slice(&sn.to_be_bytes());
    }
}

fn read_sns(input: &mut &[u8]) -> Result<Vec<EventSn>, RecordError> {
    let n = read_uvarint(input).map_err(|()| RecordError("count"))? as usize;
    if input.len() < n.saturating_mul(8) {
        return Err(RecordError("truncated ids"));
    }
    let mut out = Vec::with_capacity(n);
    for _ in 0..n {
        let (bytes, rest) = input.split_at(8);
        let arr: [u8; 8] = bytes.try_into().map_err(|_| RecordError("id"))?;
        out.push(EventSn::from_be_bytes(arr));
        *input = rest;
    }
    Ok(out)
}

fn read_i64(input: &mut &[u8]) -> Result<i64, RecordError> {
    if input.len() < 8 {
        return Err(RecordError("truncated integer"));
    }
    let (bytes, rest) = input.split_at(8);
    let arr: [u8; 8] = bytes.try_into().map_err(|_| RecordError("integer"))?;
    *input = rest;
    Ok(i64::from_be_bytes(arr))
}

/// Encodes a record into its row bytes.
#[must_use]
pub fn encode(record: &EventRecord) -> Vec<u8> {
    let mut out = Vec::with_capacity(128);
    out.push(VERSION);
    write_bytes(&mut out, record.event_id.as_bytes());
    write_bytes(&mut out, record.room_id.as_bytes());
    write_bytes(&mut out, record.event_type.as_bytes());
    write_bytes(&mut out, record.state_key.as_bytes());
    write_bytes(&mut out, record.sender.as_bytes());
    out.extend_from_slice(&record.depth.to_be_bytes());
    out.extend_from_slice(&record.origin_server_ts.to_be_bytes());
    write_sns(&mut out, &record.auth_events);
    write_sns(&mut out, &record.prev_events);
    out.push(u8::from(record.only_prev_event_is_room_create));
    let content = CanonicalJsonValue::Object(record.content.clone()).to_canonical_bytes();
    write_bytes(&mut out, &content);
    out
}

/// Decodes a row written by [`encode`].
///
/// # Errors
/// Returns [`RecordError`] if the bytes are not a record this module wrote.
pub fn decode(bytes: &[u8]) -> Result<EventRecord, RecordError> {
    let mut p = bytes;
    let (version, rest) = p.split_first().ok_or(RecordError("empty"))?;
    if *version != VERSION {
        return Err(RecordError("version"));
    }
    p = rest;
    let event_id =
        OwnedEventId::try_from(read_str(&mut p)?).map_err(|_| RecordError("event id"))?;
    let room_id = OwnedRoomId::try_from(read_str(&mut p)?).map_err(|_| RecordError("room id"))?;
    let event_type = read_str(&mut p)?.to_owned();
    let state_key = read_str(&mut p)?.to_owned();
    let sender = OwnedUserId::try_from(read_str(&mut p)?).map_err(|_| RecordError("sender"))?;
    let depth = read_i64(&mut p)?;
    let origin_server_ts = read_i64(&mut p)?;
    let auth_events = read_sns(&mut p)?;
    let prev_events = read_sns(&mut p)?;
    let (flag, rest) = p.split_first().ok_or(RecordError("flag"))?;
    p = rest;
    let content_bytes = read_bytes(&mut p)?;
    let value: serde_json::Value =
        serde_json::from_slice(content_bytes).map_err(|_| RecordError("content json"))?;
    // Lenient: the content was a `CanonicalJsonObject` when written, so a strict content comes
    // back exactly as it was, and a pre-v6 lenient content keeps its best-effort floats.
    let content = to_canonical_object(&value, false).map_err(|_| RecordError("content"))?;
    if !p.is_empty() {
        return Err(RecordError("trailing bytes"));
    }
    Ok(EventRecord {
        event_id,
        room_id,
        event_type,
        state_key,
        sender,
        content,
        depth,
        origin_server_ts,
        auth_events,
        prev_events,
        only_prev_event_is_room_create: *flag != 0,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use ruma::{EventId, RoomId, UserId};
    use serde_json::json;

    fn sample() -> EventRecord {
        EventRecord {
            event_id: EventId::parse("$abc:hs1").unwrap().to_owned(),
            room_id: RoomId::parse("!r:hs1").unwrap().to_owned(),
            event_type: "m.room.member".to_owned(),
            state_key: "@alice:hs1".to_owned(),
            sender: UserId::parse("@alice:hs1").unwrap().to_owned(),
            content: to_canonical_object(
                &json!({"membership": "join", "displayname": "Alice", "n": -5}),
                true,
            )
            .unwrap(),
            depth: 42,
            origin_server_ts: 1_790_000_000_123,
            auth_events: vec![EventSn::new(1), EventSn::new(3)],
            prev_events: vec![EventSn::new(7)],
            only_prev_event_is_room_create: true,
        }
    }

    #[test]
    fn round_trips() {
        let record = sample();
        let bytes = encode(&record);
        assert_eq!(decode(&bytes).unwrap(), record);
    }

    #[test]
    fn a_message_with_empty_content_is_small() {
        let record = EventRecord {
            event_type: "m.room.message".to_owned(),
            state_key: String::new(),
            content: CanonicalJsonObject::new(),
            auth_events: Vec::new(),
            ..sample()
        };
        let bytes = encode(&record);
        assert!(bytes.len() < 96, "{} bytes", bytes.len());
        assert_eq!(decode(&bytes).unwrap(), record);
    }

    #[test]
    fn corrupt_rows_are_errors_not_panics() {
        let bytes = encode(&sample());
        assert!(decode(&[]).is_err());
        assert!(decode(&[9]).is_err());
        assert!(decode(&bytes[..bytes.len() / 2]).is_err());
        let mut trailing = bytes.clone();
        trailing.push(0);
        assert!(decode(&trailing).is_err());
    }
}
