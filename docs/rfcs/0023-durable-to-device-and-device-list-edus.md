# 0023. To-device and device-list EDUs should survive a restart of the sending server

Status: **accepted and implemented**, 2026-10-05 (branch `agent/fed-wave3`; proposed 2026-10-04
on `agent/e2ee-gaps`). See "As implemented" at the end. Author: track 08 (E2EE). Owner of
the change: track 06 (federation), which owns `hs_federation::sender`. Affects:
`hs_federation::sender::FederationSender`, `hs_federation::outbound_store`,
`crates/hs-cli/src/edus.rs` (`SenderEduOutbox`, `DeviceListAnnouncer`).

## The problem

`FederationSender` keeps PDUs in its `OutboundStore` until the destination accepts them, but
keeps EDUs **in memory only** (its module docs: "an EDU describes a moment, and one delivered
after a restart would mostly describe a moment that has passed"). That is right for typing,
receipts and presence. It is wrong for two EDU types this server sends:

- **`m.direct_to_device`**: a to-device message is a message, often an Olm-encrypted room key.
  Lose it and the recipient cannot decrypt the room's messages until the sender's client happens
  to re-share. Synapse keeps these in `device_federation_outbox` until the destination takes
  them.
- **`m.device_list_update` and `m.signing_key_update`**: lose one and the other server serves a
  stale device list until the user's next change. The update that follows names the lost one in
  `prev_id`, so the receiver eventually resyncs, but only at that next change. Synapse keeps these
  in `device_lists_outbound_pokes` with a `sent` flag.

Complement checks both: `TestToDeviceMessagesOverFederation/stopped_server` and
`TestDeviceListsUpdateOverFederation/stopped_server` stop the destination, send (a to-device
message; a new device and its keys), restart the **sending** server, start the destination, and
wait 30 s and 50 s for delivery. Both fail here on `c2d74174` and on `agent/e2ee-gaps`: the
restart drops the queued EDU. The `good_connectivity` and `interrupted_connectivity` variants of
the to-device test pass.

Nothing in tracks 08's crates can fix this without an acknowledgement from the sender. Replaying
recent to-device messages at every boot would be safe for this server's receivers (they
deduplicate on `message_id`), but it would resend every recent message after every restart, and
nothing would ever say when one may be forgotten.

## The change

A second EDU class in the sender, durable:

```rust
impl FederationSender {
    /// As `enqueue_edu_local`, but the EDU is written to the outbound store (one row per
    /// destination, as a PDU is) and deleted only when the destination has accepted the
    /// transaction carrying it. Coalescing by key applies to rows not yet sent.
    pub fn enqueue_durable_edu(
        &self,
        destinations: impl IntoIterator<Item = String>,
        edu_type: &str,
        content: serde_json::Value,
        coalesce_key: Option<String>,
    );
}
```

- Stored in `OutboundStore` beside the destination's PDUs, under its own bound
  (`federation.max_queued_durable_edus_per_destination`, say 10 000; past it the oldest is
  dropped and counted, as the in-memory queue does now). `FederationSender::resume` drains them
  like PDUs at start.
- Carried in the same transactions as everything else, ahead of the in-memory EDUs, within
  `MAX_EDUS_PER_TRANSACTION`.
- In a cluster the store is shared, so the replica that owns the destination's shard sends them,
  exactly as for PDUs: no mesh forwarding is needed for this class.

Callers (track 08, once it exists):

- `SenderEduOutbox::send_direct_to_device` uses it for every `m.direct_to_device`.
- `DeviceListAnnouncer` uses it for `m.device_list_update` and `m.signing_key_update`, and keeps
  the stream position it has handed over in `hs-e2e`'s store, so a change committed while the
  process stops is announced at the next start (today the announcer starts from the stream's
  position at start: `crates/hs-cli/src/edus.rs`, "What is not sent").

## Migration

None: new rows in the existing outbound keyspace. A server that has never written one reads an
empty queue.

## As implemented (2026-10-05, track 06)

- `FederationSender::enqueue_durable_edu` exists as proposed, and the three EDU types
  (`hs_federation::sender::DURABLE_EDU_TYPES`: `m.direct_to_device`, `m.device_list_update`,
  `m.signing_key_update`) take the durable path from `enqueue_edu` and `enqueue_edu_local` too, so
  every caller -- `SenderEduOutbox`, the `DeviceListAnnouncer`, and the mesh forwarding of
  `crate::edu_forward` on the owning replica -- gets it without a second code path.
- Store layout (`hs_federation::outbound_store`'s module docs): `hs_federation.outbound_edus`
  `(destination, seq) -> {edu, key}` numbered from the PDUs' counter;
  `hs_federation.outbound_edu_keys` `(destination, key) -> seq` for coalescing in one read;
  `hs_federation.outbound_edu_lengths` per-destination counters for the bound and the worker's
  "anything waiting" check. New `OutboundStore` methods: `enqueue_durable_edu`,
  `peek_durable_edus`, `ack_durable_edus`, `durable_edu_len`, `durable_edus_queued`, and
  `cursor`/`set_cursor` for the announcer's position.
- The bound is `SenderConfig::max_queued_durable_edus_per_destination` (default 10 000), not yet
  an `hs-config` key: past it the oldest goes, logged at `warn`.
- Each transaction carries the destination's oldest durable EDUs first, then in-memory ones, up
  to 100; the durable ones are deleted once the transaction is accepted (or this server's policy
  forbids the destination). `resume` starts a worker for every destination with durable EDUs
  waiting.
- In a cluster, a durable EDU for a destination another replica sends for is still forwarded
  over the mesh (as before), and the owning replica stores it durably; it is not written to the
  shared store by the replica that produced it, so no rescan is needed to find it.
- The announcer stores its stream position under `device_list_announcer` in the sender's store
  (`hs-cli` `edus::ANNOUNCER_POSITION`) after each batch it hands over, and resumes from it at
  start. One position for the whole cluster.
