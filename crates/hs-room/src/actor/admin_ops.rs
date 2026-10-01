//! What a server administrator does to one room through the admin API (`hs-admin`'s
//! `rooms.*` long tail), as [`RoomActor`] methods: purging history, reading and pruning forward
//! extremities, finding the event nearest a timestamp, listing the media a room refers to, and
//! removing every record of a deleted room.
//!
//! A child module of `crate::actor` so it can reach the actor's private fields; it adds no state
//! of its own beyond the two fields the actor declares for it (`purged`, `deleted`).
//!
//! # Purging history
//!
//! Synapse's `purge_history` is the reference: message events older than a point are removed;
//! state events are kept (the room's state is built from them), the newest event is kept, and so
//! are events this server's own users sent unless the administrator asks otherwise. A purged event
//! does not disappear from the store entirely, because later events cite it as a `prev_event` and
//! the state store derives each event's state from its ancestors: its row is rewritten as its
//! redacted skeleton, flagged [`PersistedEvent::purged`], and on load it is fed to the state store
//! and kept out of the timeline. Nothing reads it any more -- not `/messages`, `/context`,
//! `/event`, relations, sync, or the admin API.
//!
//! # Deleting a room
//!
//! [`RoomActor::delete_everything`] removes every row this crate keeps for the room (events,
//! timeline, timeline gaps, extremities, outliers, snapshots, relations, aliases, directory
//! entry, the joined-by index and the room's metadata, which is what makes it "not found"
//! afterwards), in batches, and
//! marks the actor deleted so a handle somebody still holds cannot write into it. The block row is
//! kept on purpose: a blocked, deleted room stays blocked. The state store's rows are
//! content-addressed and shared between rooms, so they are left; nothing reaches them without the
//! room's metadata.

use hs_kv::RangeSpec;
use hs_tables::key::TupleKey;
use hs_tables::keyspace::TypedKeyspace;

use super::*;

/// How many rows one purge or deletion transaction touches.
const ADMIN_BATCH: usize = 512;

/// What a purge would remove, worked out before anything is removed (see
/// [`RoomActor::purge_plan`]).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PurgePlan {
    /// The timeline positions of the events to purge, oldest first.
    pub positions: Vec<i64>,
    /// Events before the cutoff kept because they are state events.
    pub kept_state: u64,
    /// Events before the cutoff kept because a local user sent them.
    pub kept_local: u64,
}

impl<B: KvBackend> RoomActor<B> {
    /// Whether an administrator has deleted this room.
    #[must_use]
    pub fn is_deleted(&self) -> bool {
        self.deleted
    }

    /// Which events a purge before `before_ts` (milliseconds) and/or strictly before
    /// `before_event` would remove. Either bound may be left out, not both.
    ///
    /// # Errors
    /// [`RoomError::BadRequest`] when neither bound is given, or `before_event` is not an event
    /// in this room's timeline.
    pub fn purge_plan(
        &self,
        before_ts: Option<i64>,
        before_event: Option<&EventId>,
        delete_local_events: bool,
    ) -> Result<PurgePlan, RoomError> {
        if before_ts.is_none() && before_event.is_none() {
            return Err(RoomError::BadRequest(
                "a purge needs a point to purge before".to_owned(),
            ));
        }
        let before_pos = match before_event {
            Some(event_id) => Some(self.timeline_position(event_id).ok_or_else(|| {
                RoomError::BadRequest(format!("{event_id} is not in this room's timeline"))
            })?),
            None => None,
        };
        let newest = self.timeline.keys().next_back().copied();
        let own = self.identity.server_name.as_str();
        let mut plan = PurgePlan::default();
        for (pos, sn) in &self.timeline {
            if before_pos.is_some_and(|limit| *pos >= limit) {
                break;
            }
            if Some(*pos) == newest || self.forward_extremities.contains(sn) {
                continue;
            }
            let Some(event) = self.events.get(sn) else {
                continue;
            };
            let header = event.header();
            if before_ts.is_some_and(|ts| header.origin_server_ts >= ts) {
                continue;
            }
            if header.state_key.is_some() {
                plan.kept_state += 1;
                continue;
            }
            if !delete_local_events && header.sender.server_name().as_str() == own {
                plan.kept_local += 1;
                continue;
            }
            plan.positions.push(*pos);
        }
        Ok(plan)
    }

    /// Purges the events at `positions` (from [`RoomActor::purge_plan`]) in one transaction,
    /// and answers how many were purged. Positions no longer in the timeline are skipped.
    ///
    /// # Errors
    /// [`RoomError::Store`] on a storage failure, in which case nothing in this batch changed.
    pub fn purge_positions(&mut self, positions: &[i64]) -> Result<u64, RoomError> {
        if self.deleted {
            return Err(RoomError::RoomNotFound(self.room_id.to_string()));
        }
        let mut rewrites: Vec<(i64, EventSn, Event, Vec<u8>)> = Vec::new();
        let mut relation_rows: Vec<Option<(EventSn, String)>> = Vec::new();
        let snapshot = self.backend.snapshot();
        for pos in positions {
            let Some(sn) = self.timeline.get(pos).copied() else {
                continue;
            };
            let Some(event) = self.events.get(&sn) else {
                continue;
            };
            let redacted = hs_model::redaction::redact(event.json(), &self.rules.redaction)
                .map_err(|e| RoomError::Internal(e.to_string()))?;
            let json: serde_json::Value =
                serde_json::from_slice(&CanonicalJsonValue::Object(redacted).to_canonical_bytes())
                    .map_err(|e| RoomError::Internal(e.to_string()))?;
            let mut skeleton = Event::parse(&json, self.room_version.clone())?;
            *skeleton.flags_mut() = event.header().flags;
            skeleton.flags_mut().set_redacted(true);
            let row = PersistedEvent {
                room_id: self.room_id.to_string(),
                json,
                room_version: self.room_version.as_str().to_owned(),
                flags: skeleton.header().flags.to_byte(),
                room_pos: Some(*pos),
                purged: true,
            };
            let bytes = serde_json::to_vec(&row).map_err(|e| RoomError::Internal(e.to_string()))?;
            let full: serde_json::Value = serde_json::from_slice(event.canonical_bytes())
                .map_err(|e| RoomError::Internal(e.to_string()))?;
            let relation = match full.get("content").and_then(relations::relation_of) {
                Some(rel) => self
                    .tables
                    .event_sn
                    .lookup(&snapshot, rel.target.as_bytes())?
                    .map(|target| (target, rel.rel_type)),
                None => None,
            };
            relation_rows.push(relation);
            rewrites.push((*pos, sn, skeleton, bytes));
        }
        let room_sn = self.room_sn;
        transact(&self.backend, TransactConfig::default(), |txn| {
            for (index, (_, sn, _, bytes)) in rewrites.iter().enumerate() {
                self.tables.events.put(txn, &(*sn,), bytes).map_err(to_kv)?;
                if let Some(Some((target, rel_type))) = relation_rows.get(index) {
                    self.tables
                        .relations
                        .delete(txn, &(room_sn, *target, rel_type.clone(), *sn))
                        .map_err(to_kv)?;
                }
            }
            Ok(())
        })
        .map_err(RoomError::from)?;
        let purged = rewrites.len() as u64;
        for (pos, sn, skeleton, _) in rewrites {
            self.timeline.remove(&pos);
            self.purged.insert(sn);
            self.events.insert(sn, skeleton);
            for children in self.relations_by_target.values_mut() {
                children.retain(|child| *child != sn);
            }
        }
        Ok(purged)
    }

    /// Absorbs one purged row on load: into the state store and the id index (later events cite
    /// it), never into the timeline or the relations index.
    pub(super) fn absorb_purged_event(
        &mut self,
        event_sn: EventSn,
        event: Event,
        room_pos: i64,
        explicit_state: Option<&[EventSn]>,
    ) -> Result<(), RoomError> {
        self.event_id_index
            .insert(event.event_id().to_owned(), event_sn);
        self.next_room_pos = self.next_room_pos.max(room_pos + 1);
        match explicit_state {
            Some(state) => self.feed_store_with_state(&event, event_sn, state)?,
            None => self.feed_store(&event, event_sn)?,
        };
        self.purged.insert(event_sn);
        self.events.insert(event_sn, event);
        Ok(())
    }

    /// The room's forward extremities, newest first (highest depth, then latest in this
    /// server's timeline).
    #[must_use]
    pub fn forward_extremity_events(&self) -> Vec<&Event> {
        let mut out: Vec<(&Event, i64)> = self
            .forward_extremities
            .iter()
            .filter_map(|sn| self.events.get(sn))
            .map(|e| (e, self.timeline_position(e.event_id()).unwrap_or(i64::MIN)))
            .collect();
        out.sort_by(|(a, a_pos), (b, b_pos)| {
            b.header()
                .depth
                .cmp(&a.header().depth)
                .then(b_pos.cmp(a_pos))
        });
        out.into_iter().map(|(e, _)| e).collect()
    }

    /// Keeps the newest forward extremity and forgets the others (Synapse's
    /// `delete_forward_extremities_for_room`), answering the ids forgotten. A room with one
    /// extremity is left alone.
    ///
    /// # Errors
    /// [`RoomError::Store`] on a storage failure, in which case nothing changed.
    pub fn prune_forward_extremities(&mut self) -> Result<Vec<OwnedEventId>, RoomError> {
        if self.deleted {
            return Err(RoomError::RoomNotFound(self.room_id.to_string()));
        }
        let ordered: Vec<OwnedEventId> = self
            .forward_extremity_events()
            .iter()
            .map(|e| e.event_id().to_owned())
            .collect();
        if ordered.len() <= 1 {
            return Ok(Vec::new());
        }
        let dropped: Vec<(OwnedEventId, EventSn)> = ordered[1..]
            .iter()
            .filter_map(|id| self.event_id_index.get(id).map(|sn| (id.clone(), *sn)))
            .collect();
        let room_sn = self.room_sn;
        transact(&self.backend, TransactConfig::default(), |txn| {
            for (_, sn) in &dropped {
                self.tables
                    .extremities_fwd
                    .delete(txn, &(room_sn, *sn))
                    .map_err(to_kv)?;
            }
            Ok(())
        })
        .map_err(RoomError::from)?;
        for (_, sn) in &dropped {
            self.forward_extremities.remove(sn);
        }
        Ok(dropped.into_iter().map(|(id, _)| id).collect())
    }

    /// The timeline event nearest `ts` (milliseconds) in direction `direction`: forwards, the
    /// first sent at or after it; backwards, the last sent at or before it (MSC3030's
    /// `timestamp_to_event`, over what this server holds).
    #[must_use]
    pub fn event_nearest(&self, ts: i64, direction: Direction) -> Option<&Event> {
        let events = self.timeline.values().filter_map(|sn| self.events.get(sn));
        match direction {
            Direction::Forward => events
                .filter(|e| e.header().origin_server_ts >= ts)
                .min_by_key(|e| e.header().origin_server_ts),
            Direction::Backward => events
                .filter(|e| e.header().origin_server_ts <= ts)
                .max_by_key(|e| e.header().origin_server_ts),
        }
    }

    /// Every `mxc://` URI the room's timeline and current state refer to, as
    /// `(server_name, media_id)`, first mention first, without repeats. Purged and redacted
    /// events refer to nothing (their content is gone).
    ///
    /// # Errors
    /// [`RoomError::State`] if the current state could not be read.
    pub fn referenced_media(&self) -> Result<Vec<(String, String)>, RoomError> {
        let mut seen = HashSet::new();
        let mut out = Vec::new();
        let mut visit = |event: &Event| {
            if event.header().flags.is_redacted() {
                return;
            }
            if let Some(content) = event.json().get("content") {
                collect_mxc(content, &mut |server, id| {
                    if seen.insert((server.to_owned(), id.to_owned())) {
                        out.push((server.to_owned(), id.to_owned()));
                    }
                });
            }
        };
        for sn in self.timeline.values() {
            if let Some(event) = self.events.get(sn) {
                visit(event);
            }
        }
        for event in self.full_state()? {
            visit(event);
        }
        Ok(out)
    }

    /// Every local user with a membership of `join`, `invite` or `knock` in the current state
    /// (whom a room deletion makes leave), with that membership.
    ///
    /// # Errors
    /// [`RoomError::State`] if the current state could not be read.
    pub fn local_members_to_remove(&self) -> Result<Vec<(OwnedUserId, String)>, RoomError> {
        let own = self.identity.server_name.as_str();
        let mut out = Vec::new();
        for event in self.members()? {
            let Some(user) = event
                .header()
                .state_key
                .as_deref()
                .and_then(|k| UserId::parse(k).ok())
            else {
                continue;
            };
            if user.server_name().as_str() != own {
                continue;
            }
            let membership = content_str(event, "membership").unwrap_or("leave");
            if matches!(membership, "join" | "invite" | "knock") {
                out.push((user.to_owned(), membership.to_owned()));
            }
        }
        out.sort();
        Ok(out)
    }

    /// Local members with power to invite, most powerful first: whom an administrator's join
    /// asks to invite a user the join rules would not let in.
    ///
    /// # Errors
    /// [`RoomError::State`] if the current state could not be read.
    pub fn local_inviters(&self) -> Result<Vec<OwnedUserId>, RoomError> {
        let own = self.identity.server_name.as_str();
        let levels = self
            .state_event("m.room.power_levels", "")?
            .and_then(|e| e.json().get("content").cloned());
        let power_of = |user: &UserId| -> i64 {
            levels
                .as_ref()
                .and_then(CanonicalJsonValue::as_object)
                .and_then(|c| c.get("users"))
                .and_then(CanonicalJsonValue::as_object)
                .and_then(|u| u.get(user.as_str()))
                .and_then(|v| match v {
                    CanonicalJsonValue::Integer(i) => Some(*i),
                    _ => None,
                })
                .unwrap_or(0)
        };
        let mut joined: Vec<(i64, OwnedUserId)> = self
            .joined_members()?
            .into_iter()
            .filter_map(|e| e.header().state_key.as_deref())
            .filter_map(|k| UserId::parse(k).ok())
            .filter(|u| u.server_name().as_str() == own)
            .map(|u| (power_of(&u), u.to_owned()))
            .collect();
        joined.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
        Ok(joined.into_iter().map(|(_, u)| u).collect())
    }

    /// Removes every record this crate keeps for the room (see the module docs) and marks the
    /// actor deleted. Answers how many events were removed. Idempotent: a second call removes
    /// whatever the first left and answers what it removed.
    ///
    /// # Errors
    /// [`RoomError::Store`] on a storage failure. Rows already removed stay removed; the actor
    /// is marked deleted regardless, so a retry finishes the job.
    pub fn delete_everything(&mut self) -> Result<u64, RoomError> {
        self.deleted = true;
        let room_sn = self.room_sn;
        let backend = self.backend.clone();
        let tables = self.tables.clone();
        let prefix =
            |table_prefix: &(RoomSn,)| RangeSpec::prefix(bytes::Bytes::from(table_prefix.encode()));
        let room = (room_sn,);

        // Users whose joined-rooms index names this room: every member in the current state.
        let members: Vec<String> = self
            .members()
            .unwrap_or_default()
            .iter()
            .filter_map(|e| e.header().state_key.clone())
            .collect();
        let aliases = self.list_aliases().unwrap_or_default();
        let event_sns: Vec<EventSn> = self.events.keys().copied().collect();

        let mut removed = 0u64;
        for chunk in event_sns.chunks(ADMIN_BATCH) {
            transact(&backend, TransactConfig::default(), |txn| {
                for sn in chunk {
                    tables.events.delete(txn, &(*sn,)).map_err(to_kv)?;
                }
                Ok(())
            })
            .map_err(RoomError::from)?;
            removed += chunk.len() as u64;
        }
        delete_range(&backend, &tables.timeline, prefix(&room))?;
        delete_range(&backend, &tables.extremities_fwd, prefix(&room))?;
        delete_range(&backend, &tables.extremities_bwd, prefix(&room))?;
        delete_range(&backend, &tables.outliers, prefix(&room))?;
        delete_range(&backend, &tables.state_snapshots, prefix(&room))?;
        delete_range(&backend, &tables.timeline_gaps, prefix(&room))?;
        delete_range(&backend, &tables.relations, prefix(&room))?;
        delete_range(&backend, &tables.room_aliases, prefix(&room))?;
        transact(&backend, TransactConfig::default(), |txn| {
            for alias in &aliases {
                tables
                    .aliases
                    .delete(txn, &(alias.clone(),))
                    .map_err(to_kv)?;
            }
            for member in &members {
                tables
                    .joined_rooms
                    .delete(txn, &(member.clone(), room_sn))
                    .map_err(to_kv)?;
            }
            tables.public_rooms.delete(txn, &room).map_err(to_kv)?;
            tables.room_meta.delete(txn, &room).map_err(to_kv)
        })
        .map_err(RoomError::from)?;

        self.timeline.clear();
        self.gaps.clear();
        self.forward_extremities.clear();
        self.relations_by_target.clear();
        Ok(removed)
    }
}

/// Deletes every row of `table` in `spec`, [`ADMIN_BATCH`] at a time.
fn delete_range<B: KvBackend, K: TupleKey>(
    backend: &B,
    table: &TypedKeyspace<B::Keyspace, K>,
    spec: RangeSpec,
) -> Result<u64, RoomError> {
    let snapshot = backend.snapshot();
    let mut keys: Vec<K> = Vec::new();
    for item in table.range(&snapshot, spec) {
        let (key, _) = item?;
        keys.push(key);
    }
    drop(snapshot);
    for chunk in keys.chunks(ADMIN_BATCH) {
        transact(backend, TransactConfig::default(), |txn| {
            for key in chunk {
                table.delete(txn, key).map_err(to_kv)?;
            }
            Ok(())
        })
        .map_err(RoomError::from)?;
    }
    Ok(keys.len() as u64)
}

/// Calls `found` with `(server_name, media_id)` for every `mxc://server/id` string anywhere in
/// `value`.
fn collect_mxc(value: &CanonicalJsonValue, found: &mut impl FnMut(&str, &str)) {
    match value {
        CanonicalJsonValue::String(s) => {
            if let Some(rest) = s.strip_prefix("mxc://")
                && let Some((server, id)) = rest.split_once('/')
                && !server.is_empty()
                && !id.is_empty()
                && !id.contains('/')
            {
                found(server, id);
            }
        }
        CanonicalJsonValue::Array(items) => {
            for item in items {
                collect_mxc(item, found);
            }
        }
        CanonicalJsonValue::Object(map) => {
            for item in map.values() {
                collect_mxc(item, found);
            }
        }
        _ => {}
    }
}

impl<B: KvBackend> RoomActorHandle<B> {
    /// Runs `f` against the actor under its lock, off the async executor: the admin API's
    /// operations on one room (`crate::admin`), which read and change it in ways no client
    /// request does.
    pub async fn administer<T, F>(&self, f: F) -> T
    where
        B: 'static,
        T: Send + 'static,
        F: FnOnce(&mut RoomActor<B>) -> T + Send + 'static,
    {
        self.with_actor(f).await
    }
}
