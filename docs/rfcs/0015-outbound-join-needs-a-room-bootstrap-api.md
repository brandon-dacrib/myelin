# 0015. Joining a room hosted elsewhere needs a room-bootstrap API in `hs-room`

Status: proposed. Owner of the fix: track 04 (room and events), `crates/hs-room/src/actor.rs` and
`crates/hs-room/src/registry.rs`. Also affects: `crates/hs-cli/src/federation.rs` (the wiring that
would call it). Discovered by, and worked around (not fixed) in: track 06 (federation),
`crates/hs-federation/src/outbound_join.rs`.

Companion artifacts: `docs/rfcs/0014-event-signing-must-sign-the-redacted-form.md` (the sibling
outbound-signing bug this same session's live two-server test exercised for the first time),
`crates/hs-federation/scripts/two-server-federation.sh` (the script this RFC's gap was found by
running), `docs/status/06-federation.md`'s seventh session.

## 1. The gap

This server has real code for both halves of a federated join, but only one half of the pair that
actually lets a user join a room hosted somewhere else:

- **Resident/target side (this server hosts the room, a remote user joins it): real.**
  `crate::join::make_join`/`crate::join::send_join`, mounted at
  `crate::transport::join`, build a real template against this server's own state, verify a
  submitted join event for real (signature, content hash, shape, authorization), and persist it
  via `hs-cli`'s `RegistryWriteSink` → `hs_room::actor::RoomActor::accept_remote_event` — which is
  itself real (closed by track 04 between this crate's fourth and sixth sessions). Tested end to
  end in `crates/hs-cli/tests/federation_writes.rs` and, this session, against a second live
  process in `crates/hs-federation/scripts/two-server-federation.sh`.
- **Client/joining side (this server's own user joins a room hosted elsewhere): the handshake is
  now real, persistence is not.** `crates/hs-federation/src/outbound_join.rs::join_room` (new this
  session) performs the actual `GET make_join` / sign / `PUT send_join` / verify round trip against
  a live resident server, and returns a fully verified [`RemoteJoinOutcome`] — every event in
  `state` and `auth_chain` has passed the same `verify_pdu` check any other inbound PDU gets. But
  nothing can take that verified snapshot and make it a room this server's own user can read from
  or post into, because **no `hs-room` API exists to create a room from someone else's state
  snapshot**:
  - `RoomRegistry::get_or_load` (`crates/hs-room/src/registry.rs`) returns `RoomError::RoomNotFound`
    for any room ID this server has never created — it only *loads* a room whose `RoomMeta` a prior
    `create_room` call already persisted (`RoomActor::load`, `crates/hs-room/src/actor.rs`, checks
    `tables.room_sn`/`tables.room_meta` and returns `Ok(None)` if either is absent).
  - `RoomRegistry::create_room` → `RoomActor::create` only ever *originates* a brand-new room: it
    builds and signs a fresh `m.room.create` this server authors (`crates/hs-room/src/actor.rs`,
    around `RoomActor::create`), which is the wrong event for a room whose real `m.room.create` was
    authored by a different server entirely and already exists, signed, in the join response's
    `state`.
  - `RoomActor::accept_remote_event` (the API that closed the *other* half of this gap) requires an
    existing, already-loaded `&mut self` — it applies one more event to a room that is already
    there, which is exactly right for the resident side (`send_join`'s new member, an ordinary
    `/send` PDU) and exactly wrong for bootstrapping: there is no room yet to call it on.

Confirmed structurally (reading `RoomActor::load`/`create_room`/`accept_remote_event` and
`RoomRegistry::get_or_load`/`create_room`) and confirmed by running: `join_room` was exercised live
against a real second `hs serve` process (`two-server-federation.sh`), and the join itself
completes and is genuinely, observably persisted **on the resident's side** — the joining user
really does show up in `GET /_matrix/client/v3/rooms/{roomId}/members` on the server that hosts the
room. The joining server has no way to represent that same room for its own user afterward: no
local `RoomActor` for that room ID exists or can be created by anything in `hs-room`'s current API.

## 2. Why this was never noticed before this session

Every existing test of the join handshake exercises exactly one side of it in isolation:

- `crates/hs-federation/src/join.rs`'s own tests and `crates/hs-cli/tests/federation_writes.rs`
  play the *resident* role only — a synthetic, already-signed join event is handed to `send_join`
  as if it arrived from a remote, and the resident's own `RoomWriteSink` (a real `RegistryWriteSink`
  in the `hs-cli` tests) applies it to a room that `hs-cli`'s test setup already created locally.
  That is a real end-to-end proof of the resident side, and it is the only side these tests ever
  needed a room to exist locally for.
- Nothing anywhere called `make_join`/`send_join` as a *client* before this session, because
  nothing existed that could: this session's `crate::outbound_join` is the first code in this
  workspace to do so. There was therefore no test, fixture, or manual run that could have found the
  bootstrap gap, because finding it requires actually attempting the join from the initiating
  side — which is precisely what running two live instances against each other, for the first
  time, does.

This is the same shape of discovery as RFC-0014 (a bug invisible until two real instances actually
talk to each other) but on the opposite side of the same handshake: RFC-0014 was "this server's
outbound *events* are signed wrong"; this is "this server has no outbound *join* at all, only an
inbound one."

## 3. What `hs-room` needs to add

A new `RoomRegistry`/`RoomActor` entry point, roughly:

```rust
impl<B: KvBackend> RoomActor<B> {
    /// Creates a local `RoomActor` for `room_id` from a **verified** federation join response:
    /// `create_event` is the room's real `m.room.create` (authored by the room's original
    /// creator, not this server), `state` is the full state snapshot `send_join` returned, and
    /// `join_event` is this server's own now-accepted join event. All three have already passed
    /// `hs_federation::inbound::verify_pdu` (content hash + signature) before reaching this
    /// function — it applies `hs_state`'s state resolution/authorization to what it is given, the
    /// same way `accept_remote_event` does for one event, but seeded from nothing rather than
    /// from an already-loaded room.
    pub fn create_from_remote_join(
        backend: B,
        tables: Tables<B>,
        identity: HomeserverIdentity,
        room_id: &RoomId,
        state: Vec<Event>,
        auth_chain: Vec<Event>,
        join_event: Event,
        now_ms: i64,
    ) -> Result<Self, RoomError>;
}
```

plus a `RoomRegistry::bootstrap_from_remote_join(...)` wrapper mirroring `create_room`'s existing
`spawn_blocking` + `insert` pattern, and a corresponding extension to
`hs_federation::inbound::RoomWriteSink` (or a new, sibling trait — `RoomWriteSink::accept_verified_event`
assumes the room already exists, per its own doc comment, so bootstrapping is a distinct operation,
not a variant of it) that `crates/hs-cli/src/federation.rs`'s `RegistryWriteSink` (or a new
`RegistryJoinSink`) can implement by calling it.

Open questions this RFC deliberately leaves to track 04, since they are `hs-room`'s own storage and
state-resolution concerns, not `hs-federation`'s:

- Whether to persist `auth_chain` events that are not themselves in `state` (outliers), and how
  `RoomActor`'s existing timeline/extremity bookkeeping should represent a room whose entire
  history before the join is exactly one snapshot, not a DAG this server derived by replay.
- Whether state resolution needs to run at all for a freshly bootstrapped room (the snapshot is, by
  construction, already-resolved current state from the resident's point of view) or whether it
  should be trusted as-is once every event's signature and hash check out.
- How this interacts with the "forward extremities assumed to number exactly one" simplification
  `crate::join`'s own module doc already flags as a federation-wide assumption that will need
  revisiting once real inbound ingestion diverges.

## 4. What is proven without this fix, and what still is not

Proven live, this session, between two real `hs serve` processes and codified as an automated
regression (`crates/hs-federation/src/outbound_join.rs::tests::join_room_completes_the_real_handshake_against_a_live_resident`,
plus the manual script):

- Discovery, TLS with a private CA (`federation.custom_ca_certificates` — see the wiring bug fixed
  in `crates/hs-cli/src/federation.rs::client_config` this same session), and outbound `X-Matrix`
  request signing all work against a real second process, not just in-process fakes.
- A joining server can build a real join event, sign it correctly (RFC-0014's fixed order), and get
  it accepted by a real, independent resident server.
- The resident server's `send_join` response — its real, live, currently-signed room state — is
  fully verifiable by the joining server using only the public information the spec says it should
  need (server keys fetched via `/_matrix/key/v2/server`).

Not proven, and not possible without this RFC's fix: the joining user's own client ever seeing that
room, syncing it, reading its history, or posting into it. The join is real from the resident's
point of view and inert from the joiner's.
