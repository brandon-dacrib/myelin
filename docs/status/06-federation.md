# 06 Federation: status

> **Integration note, 2026-09-26, last (integration lead): what a server is served.**
> `/backfill` and `/get_missing_events` applied only the room-level gate (a member of the
> requesting server now, or `world_readable`) and served everything whole, so a server whose
> member joined a members-only room yesterday could fetch the whole of last year. Both now serve
> an event the requesting server was not in the room for in its redacted form
> (`hs_cli::federation::pdu_for_server` over `RoomActor::server_may_see`): `joined` needs one of
> that server's users joined as of the event, `invited` joined or invited, `shared` and
> `world_readable` allow anyone past the gate. Redacted rather than omitted, because a hole in a
> batch reads as missing history to the requester and the redacted form still verifies. Also
> `min_depth` on `/get_missing_events`, parsed and applied as a floor. Both tested in
> `crates/hs-cli/tests/federation_reads.rs`.

> **Integration note, 2026-09-26, later (integration lead): the gap-shaped request.** Reading
> `TestGetMissingEventsGapFilling` for why it failed found that Complement's reference federation
> server answers exactly one request when a homeserver receives an event with unknown ancestors:
> `POST /get_missing_events` with `earliest_events` naming the homeserver's forward extremities
> and `latest_events` naming the event just received. It has no `/backfill` handler, and it
> checks both lists. This crate's `backfill::resolve_missing_ancestors` only ever asked
> `/backfill`, so against that server -- and against Synapse, which serves both but is asked the
> gap-shaped one by every other implementation -- the loop could not begin. It asks
> `/get_missing_events` first now (`GapContext`, filled by `inbound::process_transaction` from
> `RoomDataSource::forward_extremities` and the triggering event), and only when that does not
> close the gap does it fall back to the `/backfill` rounds, under the same limits; three unit
> tests cover closed-by-the-first-request, unsupported-then-backfill, and partial-then-backfill.
> Found on the way: `hs_cli::federation::RegistryRoomSource::forward_extremities` answered
> "the newest timeline event", an assumption from before remote joins, forks over `/send` and
> fetched history existed; it reads the actor's real extremity set now
> (`RoomActor::forward_extremity_ids`). Not re-measured yet: the federation package run in
> progress at the time was from the commit before this.

> **Integration note, 2026-09-26 (integration lead):** the other trigger for `/backfill`. This
> crate's `backfill::resolve_missing_ancestors` fetches the missing *ancestors* of an event that
> arrived over `/send`; `hs_cli::backfill::FederationBackfill` (implementing
> `hs_room::backfill::Backfill`) now runs the same `FederationClient::backfill` call on a
> client's behalf -- one batch of a hundred from the oldest event held, against the room's
> server then the other members' servers, every PDU through `inbound::verify_pdu` -- and hands
> the batch to `RoomActor::accept_backfilled_events`, which is the history path rather than the
> ancestor-resolution one: the events are the history, not something newer's prerequisites. One
> lock per room keeps two clients paging the same room from fetching the same batch twice.
> Verified by `crates/hs-cli/tests/federation_two_servers.rs`: 120 messages before the join read
> back in three pages of fifty, down to the create event, nothing in the next incremental sync.
> Not done here: the state at a backfilled event is walked on the room side rather than asked
> for (`/state_ids` would make it exact at each batch boundary), and no auth events are fetched
> for it (`/event/{eventId}`); both are the noted next step in the actor's doc.

> **Integration note, 2026-09-25 (integration lead):** the eighth session's sender (below) and
> track 04's RFC 0015 bootstrap landed together with the piece between them: `POST /join` on a
> room this server does not hold now runs `crate::outbound_join::join_room_with_content` against
> each `via` and hands the verified snapshot to `RoomRegistry::bootstrap_from_remote_join`
> (`hs_room::remote_join::RemoteJoin`, implemented by `hs_cli::remote_join`). The joining side
> also sends `?ver=` with every supported room version on `make_join` (Synapse refuses a joiner
> that does not) and merges the user's profile into the join template. Proven end to end by
> `crates/hs-cli/tests/federation_two_servers.rs` (two in-process servers, plain HTTP, join
> through the client API, messages both ways) and by the TLS script. The sender's "B cannot hold
> the room yet" caveat in section 8 below was true when written and is closed.

> **Integration note, 2026-09-19 (integration lead):** the gap this file describes below as "the
> one gap this session could not close" — no way to persist a newly received foreign event — was
> **closed** by track 04's `hs_room::actor::RoomActor::accept_remote_event`. The fifth session
> closed the next one: the backfill-then-retry loop `MissingAncestors` was reported for but never
> consumed. The sixth session closed the TLS/CA gap Complement's federation run was blocked on,
> fixed a real PDU signature-verification bug it uncovered underneath, and found a second, more
> consequential instance of the same signature bug in `hs-room`'s own outbound pipeline (fixed by
> track 04 since, per RFC-0014). **This (seventh) session put two real, live instances of this
> server in front of each other for the first time — the first genuine "join a room on a different
> server" this workspace has ever run — and found that the pieces the previous six sessions built
> had never actually been assembled: `federation.custom_ca_certificates` had no effect on a real
> `hs serve` process (the config field existed, the client field existed, nothing read the file
> off disk), an IP-literal destination with an explicit port produced a malformed, doubled-port
> URL, and nothing in this workspace could *initiate* an outbound join at all — only answer one.**
> All three are fixed; see "Seventh session" below.

Updated: 2026-09-25 (eighth session -- the outbound sender: this server now sends its own events
to the servers of a room's remote members over `PUT /send/{txnId}`, and a resident forwards a join
it accepts to the room's other servers. In memory only, no EDUs, not shard-gated; see "Eighth
session" below). Previously updated 2026-09-19 (seventh session -- the two-instance session: two real `hs serve` processes,
different server names, federated over real HTTPS with a private CA and real X-Matrix signatures,
for the first time. Found and fixed a `hs-cli` wiring bug that silently no-op'd
`federation.custom_ca_certificates` on every real deployment, a `hs-federation` discovery bug that
broke every IP-literal-with-port destination, and closed the "nothing can initiate an outbound
join" gap with a new `crate::outbound_join` module. See "Seventh session" below). Previously
updated 2026-09-19 (sixth session -- the TLS/CA session: `hs-config`/`hs-federation` gained a real
config surface for trusting a custom CA (matching Synapse's `federation_custom_ca_list`), the
outbound client now uses it, `verify_certificates: false` is now loud, and a real send_join
signature-verification bug Complement found underneath the TLS gap is fixed. See "Sixth session"
below). Previously updated 2026-09-19 (fifth session -- the backfill session: a remote event citing
ancestors this server lacks now gets them fetched, verified and persisted, then the original event
is retried; see "Fifth session: the backfill loop" below). Before that, 2026-09-18 (fourth session
-- the write session: `/send` and the join handshake are real now, not seams; see "Fourth session:
`/send`, `make_join`/`send_join`, and the v2 mount fix" below). Before that, 2026-09-18 (third
session, the mounting session; see "Mounted into `hs serve`" below for what changed then). The
first session wrote the threat model and the plan below but stopped before any crate code existed;
the second implemented items 1-7 of that plan.
`crates/hs-federation` is no longer a placeholder: 136 passing lib tests (up from 124), plus the
`hs-cli` end-to-end suites (8 federation_reads, 8 federation_writes, 2 federation_sender -- new
this session -- and e2e, all passing), `cargo clippy -p hs-federation -p hs-cli --all-targets --
-D warnings` clean (without `--no-deps`: the `hs-http` breakage the seventh session noted is
gone), five fuzz targets that type-check. Read this file before touching `hs-federation` further.

## Eighth session (2026-09-25): the outbound sender

Scope, per this session's brief: build the outbound federation sender. Before it, this server
never sent a locally created event to any other server -- there was no `sender` module, and
`docs/next-steps.md`'s "there is no outbound queue yet" was true. After it, an event a local user
sends in a room with remote members reaches those servers via `PUT
/_matrix/federation/v1/send/{txnId}`. Ownership this session: `crates/hs-federation/**`, the
named parts of `crates/hs-cli` (a new `federation_sender.rs`, `FederationMount`/`build_mount`,
`RegistryRoomSource`, the minimum in `serve.rs`, tests), this file. `crates/hs-room` and the
client `/join` routes were another agent's and were not touched.

### 1. `crate::sender`: `FederationSender`, what is real

`FederationSender::new(client: Arc<FederationClient>, own_server_name)` (or `with_config` with an
explicit `SenderConfig { initial_backoff, max_backoff }`); `enqueue_pdu(destinations, pdu)`;
`pending_pdus()`, `pending_pdus_for(dest)`, `pending_by_destination()`; `shutdown()`. Plus the
`OutboundPduSink` trait (one method, `enqueue_pdu(Vec<String>, Value)`) that `FederationSender`
implements, so the transport server and `send_join` take an `Arc<dyn OutboundPduSink>` and are
tested with a recording sink.

- **One worker per destination**, spawned on the first `enqueue_pdu` naming it, on the current
  Tokio runtime (outside one: logged and dropped, never a panic). It drains its queue into
  transactions of at most `MAX_PDUS_PER_TRANSACTION` (50, the same constant `crate::inbound`
  enforces on receipt), body `{"origin", "origin_server_ts", "pdus", "edus": []}`, sent through
  `FederationClient::send` -- so discovery, TLS/CA trust, `X-Matrix` signing, the per-destination
  concurrency semaphore and the destination-store backoff records all apply unchanged.
- **`txnId` is `{process_start_ms}-{counter}`**: unique across restarts, monotonic within one.
  **A retry reuses the same `txnId`**, so a receiver whose response was lost replays its cached
  answer (`crate::inbound::TransactionStore`) rather than applying the PDUs twice.
- **Retried, in order, until accepted**: a non-2xx status or a connection/discovery error waits a
  doubling delay from `initial_backoff` (1s) capped at `max_backoff` (the client's own
  `max_retry_backoff`, so the two backoffs an operator sees on one destination share a ceiling).
  `ClientError::Backoff { retry_at_ms }` (the destination store's judgement, set by the client on
  connection-level failures) is slept out in slices of at most `BACKOFF_POLL_INTERVAL` (30s), so an
  administrator's reset of the destination (`federation.destinations.reset`) is honoured within
  30s instead of at the end of an hour-long wait. Only `Disabled`/`DomainDenied`/`IpDenied` --
  this server's own policy -- drop the transaction (logged at `error`); everything else retries.
- **A per-PDU `error` in a 200 response is final**: logged at `warn` with the event ID and the
  receiver's reason, not retried. Per-destination ordering is preserved throughout; destinations
  are independent (a failing one delays only its own queue).
- **Nothing is ever queued for `own_server_name`**, whatever a caller passes; duplicates in one
  call collapse to one copy.
- **In memory only, said loudly** in the module docs and here: a restart, a crash or `shutdown()`
  loses every unaccepted PDU, and there is no catch-up afterwards. `PLAN.md` section 5.2 item 6
  (per-destination queues sharded by destination hash, persisting queue state so failover resumes)
  and Synapse's `destination_rooms` catch-up are the target; a `KvBackend`-backed queue is the next
  step, not this one. `shutdown()` logs the count it lost.
- **Not shard-gated**: nothing consults `hs-cluster`. In a cluster this does not duplicate traffic
  by itself (a room's actor is resident on the replica that owns its shard, and only it publishes
  that room's updates), but a persisted, sharded sender will need to own "who sends for this
  destination" explicitly.
- **Not sent yet**: EDUs of every kind (typing, presence, receipts, device-list updates,
  to-device, signing-key updates -- no `enqueue_edu` seam, since one that discarded its argument
  would be worse than none); invites (`/invite` is its own handshake); leaves and knocks against a
  remote resident (`make_leave`/`send_leave`, `make_knock`/`send_knock`).

### 2. Resident-side forwarding of an accepted join

The spec requires the resident that accepts a `send_join` to send the new join event to every
other server in the room -- it is the only way they learn of the new member. `FederationState`
gained `sender: Option<Arc<dyn OutboundPduSink>>`; `RoomDataSource` gained `async fn
member_servers(&self, room_id) -> Vec<String>` (implemented on `InMemoryRoomSource` from
`FakeRoom::joined_servers`, and on `hs_cli::federation::RegistryRoomSource` from
`RoomActor::joined_members()`); `crate::join::send_join` takes two more parameters,
`own_server_name: &str` and `forward: Option<&dyn OutboundPduSink>`, and after a `Stored`
outcome (not `AlreadyKnown` -- a replayed join was forwarded the first time) enqueues the verified
event to every member server except `origin` and itself. Member servers are read after the store.
Every `FederationState` construction site in this crate and `hs-cli` (tests included) carries the
new field.

### 3. `hs-cli` wiring: `crate::federation_sender`

`hs_cli::federation_sender::OutboundFederation::start(rooms, sender, own_server_name)` subscribes
to `RoomRegistry::subscribe_global()` and follows it; `stop()` aborts the task and shuts the sender
down. `serve.rs` starts it right after `build_mount`, before any listener is bound (the stream does
not replay), and `ServeHandle::shutdown` stops it after appservice delivery, through a
type-erased `stop_outbound_federation` closure modelled on `stop_appservice_delivery`.
`FederationMount` gained `pub sender: Arc<FederationSender>`, built in `build_mount` over the same
client as everything else and installed as `state.sender`.

For each `RoomUpdate` whose `sender`'s server is ours, `forward_update` loads the room
(`get_or_load`), reads the event as stored and signed (`event_by_id` ->
`serde_json::from_slice(canonical_bytes())`, the federation form: `hashes`, `signatures`, no
`event_id`), and computes the destinations: the servers of `RoomActor::joined_members_after(event)`
(which exists in exactly the shape needed, so no approximation), plus, for an `m.room.member` with
`membership` `leave` or `ban`, the target's server -- a kicked or banned user's server is not
"joined after" the event, and if that was its last member it would otherwise never hear why its
user is gone (this is the "state before the event" rule Synapse applies, expressed as "after, plus
the removed target"). Our own name is dropped; invite targets are not added (the `/invite`
handshake is not built, and an invitee's server that is not in the room would only answer
"unknown room"). Events whose sender is remote are never re-sent. `RecvError::Lagged(n)` is logged
at `warn` saying exactly what it means: the skipped updates' local events will not be sent to
remote servers, because the sender has no catch-up.

The admin API's Federation page now shows real pending counts:
`DestinationStoreSource::with_sender(sender)` reports `pending_pdu_count` per destination
(and lists a destination with a queue but no backoff record yet). `pending_edu_count` stays zero,
truthfully.

### 4. Tests (all fail, or do not compile, without the change)

`crates/hs-federation/src/sender.rs` (against `hs_testkit::fake_federation::FakeFederationPeer`
on loopback, plaintext via `ClientConfig::scheme`, explicit-port destinations, with an axum layer
in front of the fake recording the `Authorization` header since the fake does not):
`three_pdus_go_out_as_one_signed_transaction`,
`sixty_pdus_split_into_transactions_of_fifty_then_ten_in_order`,
`a_failing_destination_is_retried_in_order_after_waiting` (500, 500, 200: three attempts with
the same `txnId`, elapsed at least 100ms + 200ms, no fourth attempt),
`destinations_are_served_independently`, `nothing_is_ever_sent_to_our_own_server_name`,
`a_destination_in_backoff_is_not_contacted_before_its_retry_time` (a fixed-`retry_at` destination
store), `a_per_pdu_rejection_is_final_not_retried`, `shutdown_stops_the_workers_and_drops_the_queue`,
`backoff_doubles_from_the_initial_delay_and_is_capped`.
`crates/hs-federation/src/join.rs`:
`an_accepted_join_is_forwarded_to_the_other_member_servers_but_not_the_origin`,
`a_replayed_join_is_not_forwarded_again`.
`crates/hs-cli/tests/federation_sender.rs` (a real `RoomRegistry`, the real feeder, sender and
client, a fake peer; `hs serve` itself cannot be told to federate in plaintext, so the task is
tested in isolation as the brief allowed):
`a_local_message_reaches_the_server_of_a_remote_member_and_nothing_earlier_does` (a message from
before the remote member joined, and the remote member's own join, are not in the transaction;
the message after it is, byte-for-byte the stored PDU),
`a_kick_reaches_the_kicked_users_server_and_later_events_do_not` (drives `forward_update` one
update at a time and asserts each event's destinations). Conditions with deadlines, not sleeps.

### Verification

```
cargo fmt --all
cargo clippy -p hs-federation -p hs-cli --all-targets -- -D warnings        # clean
cargo test -p hs-federation                                                 # 136/136
cargo test -p hs-cli --test federation_sender --test federation_writes --test federation_reads
                                                                            # 2/2, 8/8, 8/8
cargo test -p hs-cli --test e2e                                             # 24/24
bash crates/hs-federation/scripts/two-server-federation.sh                  # passes, see below
```

**Live, two real processes** (the seventh session's script, extended with a step 8): after bob's
server B joins alice's room on A, alice posts again; A's feeder logged `queueing a local event
for federation ... servers=1`, A's sender sent it to `127.0.0.1:8449` as
`PUT /_matrix/federation/v1/send/1790383931330-1` over stunnel-terminated TLS with the private
CA and a real `X-Matrix` signature, B's real inbound layer verified it (fetching A's key over the
same TLS) and answered 200 with a per-PDU `unknown room` error -- B cannot hold the room until
RFC-0015 -- which A logged as a final rejection and counted the transaction accepted. The wire
path from a local `/send` on A to a verified transaction on B is proven; delivery into a room on
B is not, and cannot be until track 04's bootstrap API lands. The script now launches both
servers with `RUST_LOG=info,hs_federation=debug,hs_cli=debug` (overridable) so that step can
watch A's log.

## Seventh session: two real instances, federating for real -- and three bugs only that could find

Scope, per this session's brief (`docs/next-steps.md` item 2, sharpened): put two local instances
of this server in front of each other, different server names, real HTTP, real X-Matrix signatures
-- not a public join (no public name or CA on this laptop), but the same code paths. Ownership this
session: `crates/hs-federation/**`, `crates/hs-cli/**`, `docs/status/06-federation.md` only.

### 0. The headline finding: assembling working parts for the first time finds bugs no unit test can

Every one of this session's three bugs (below) was invisible to every existing test in this
workspace -- 120 passing `hs-federation` lib tests, 24 passing `hs-cli` end-to-end tests, all still
green *with the bugs present* -- because every one of them is a seam between two things that had
never both been real at once before: a config file loaded by the actual `hs-cli` wiring (not a
`ClientConfig` built by hand in a test), a destination string shaped like a real deployment might
plausibly use, and a join initiated by an actual second process rather than a synthetic
already-signed event handed to `send_join`. This is the same lesson the fourth, fifth and sixth
sessions each drew independently (a missing `room_id` in a join response, a trailing slash, the
redaction-before-signing bug) -- restated here because it happened a third time, at a different
seam, the moment real assembly was attempted again.

### 1. `federation.custom_ca_certificates` had never worked on a real server

**Root cause.** `crates/hs-cli/src/federation.rs::client_config` -- the one function that converts
`hs-config::FederationConfig` into the `hs_federation::client::ClientConfig` a real `hs serve`
process's `FederationClient` is built from -- listed `verify_certificates`, `request_timeout` and
a few others explicitly, then filled in everything it did not mention with
`..ClientConfig::default()`. `custom_root_certificates` and `trust_os_root_store` were never in
that explicit list, so they silently took their `ClientConfig::default()` values (empty /
`false`) regardless of what `federation.custom_ca_certificates` said in the YAML. The sixth
session built the schema field, the `ClientConfig` field, the PEM-parsing logic, and proved all of
it works with an in-process TLS test that constructs `ClientConfig` directly -- and that test is
exactly why this went unnoticed: it never went through `client_config`, the one place a real
config file's file *paths* get turned into bytes. Confirmed by grep before touching anything:
`custom_root_certificates`/`custom_ca_certificates`/`trust_os_root_store` appeared nowhere in
`crates/hs-cli/src/*.rs`.

**Fixed** in `client_config` (`crates/hs-cli/src/federation.rs`): reads each configured path with
`std::fs::read`, collecting the bytes into `ClientConfig::custom_root_certificates`; a path that
fails to read is logged with `tracing::error!` and skipped, not a fatal boot error (matching
`FederationClient::new`'s own tolerance for a CA entry that reads fine but parses as malformed
PEM). `trust_os_root_store` is copied straight through. Proven by running, not just reading: this
session's two-instance script failed outbound TLS verification against the private CA until this
fix landed, then succeeded.

### 2. An IP-literal destination with an explicit port produced a malformed, doubled-port URL

**Root cause**, found while diagnosing why `federation-join-room` (below) could not even reach a
server whose TLS and registration all demonstrably worked (`curl` against the same address
succeeded). `crates/hs-federation/src/discovery.rs::resolve`'s `ParsedServerName::IpLiteral` arm
(both the direct one and the well-known-delegates-to-an-IP-literal one) set
`tls_server_name: server_name.to_string()` -- the **entire original input string**, e.g.
`"192.0.2.1:8449"` -- instead of just the IP. `crate::client::FederationClient::send_inner` then
builds the request URL as `format!("{scheme}://{tls_server_name}:{connect_port}{path}")`, which for
an IP literal with an explicit port produces `https://192.0.2.1:8449:8449/...` -- a syntactically
invalid authority that `reqwest` simply fails to connect, surfacing as a generic
`ClientError::Request` with no further detail (the actual gap that made this take real
investigation: `reqwest::Error`'s `Display` does not show the malformed-URL cause, only "error
sending request for url (...)"). Every `Hostname` arm already stored the bare host correctly;
`ipv4_literal_with_port_bypasses_discovery_entirely`, the one existing test for this exact input
shape, asserted `connect_port` and `via` but never `tls_server_name`, so the bug shipped invisibly.

**Fixed**: both `IpLiteral` arms in `discovery.rs::resolve` now set `tls_server_name: ip.to_string()`
(the bare IP), matching every other arm's convention. Two regression assertions added to the
existing tests (`ipv4_literal_with_port_bypasses_discovery_entirely`,
`well_known_delegates_to_ip_literal`) that would have caught this on day one. `cargo test -p
hs-federation --lib discovery` -- 20/20 pass.

Also discovered along the way, not a code bug but worth recording: `hs-federation`'s
`AddrResolver` (`HickoryResolver`, backed by `hickory-resolver`) is a pure userspace DNS stub that
queries the configured nameservers directly and does **not** consult `/etc/hosts`. On a network
with a search-domain-configured `/etc/resolv.conf` (this laptop's has one), resolving the hostname
`"localhost"` through it is genuinely unreliable -- it is not guaranteed to return `127.0.0.1`,
unlike `curl`/`dig`/anything going through `getaddrinfo`. This session's script uses IP literals
(`127.0.0.1:8448`/`127.0.0.1:8449`) specifically to sidestep this, not merely for convenience; see
the script's own comments. This is not a bug to fix (a pure-Rust stub resolver correctly not
reading `/etc/hosts` is ordinary, documented `hickory-resolver` behavior) but is worth any future
session knowing before spending an hour on it again.

### 3. Nothing in this workspace could *initiate* a federated join -- only answer one

**The gap.** `crate::join::{make_join, send_join}` and their mount (`crate::transport::join`) are
the *resident* side of the join handshake: this server, hosting a room, answering a remote's
`GET /make_join` and `PUT /send_join`. That side is real and was already tested end to end
(`crates/hs-cli/tests/federation_writes.rs`). Nothing anywhere in this workspace played the other
role -- a server whose own user wants to join a room hosted elsewhere, which means calling *out* to
another server's `/make_join`/`/send_join`. This was invisible until this session because nothing
had ever tried: every previous test of the join handshake, in this crate and in `hs-cli`, hands a
synthetic already-signed join event to `send_join` as if a remote had produced it.

**Closed the handshake half.** New module `crates/hs-federation/src/outbound_join.rs`,
`pub async fn join_room(client, key_cache, destination, room_id, user_id, own_server_name,
signing_key) -> Result<RemoteJoinOutcome, OutboundJoinError>`: calls `GET make_join` via the same
`FederationClient::send` every other outbound call uses (so discovery, TLS/CA trust and outbound
`X-Matrix` signing all come for free), signs the returned template the spec's real way -- hash the
full event, redact, sign the *redacted* form, copy the signature back onto the full event, per
RFC-0014, which this module follows rather than re-deriving -- calls `PUT send_join` (v2), and
verifies every event in the response's `state` and `auth_chain` through the same
`crate::inbound::verify_pdu` any other inbound PDU gets. Four tests, including a full live-HTTP
round trip against a real `axum::serve` resident bound to a real loopback socket (not
`tower::oneshot`, no mocked transport) -- see `outbound_join::tests::
join_room_completes_the_real_handshake_against_a_live_resident`. `cargo test -p hs-federation
--lib outbound_join` -- 4/4 pass.

**Did not, and could not, close persistence.** `join_room` returns a fully verified snapshot and
stops there: `hs-room` has no API to create a local room from a federation join response's state,
only to originate a brand new one (`RoomActor::create_room`) or apply one more event to a room it
already has (`RoomActor::accept_remote_event`) -- neither fits "this room's real `m.room.create`
was authored by a different server and I have never seen this room before." Filed as
`docs/rfcs/0015-outbound-join-needs-a-room-bootstrap-api.md`, addressed to track 04, with the exact
shape of the needed entry point. **Consequence**: the join is real and durably persisted on the
*resident's* side (proven live, see below) but the joining server cannot yet represent the room for
its own user to sync or post into -- a one-way proof, honestly reported as such by both the RFC and
the script's own printed summary.

**A diagnostic CLI surface, since nothing in the ordinary client API can trigger this yet.** New
`hs federation-join-room -c <config> --destination <server> --room <id> --user <user_id>`
(`crates/hs-cli/src/cli.rs`'s `Command::FederationJoinRoom`, implemented in
`crates/hs-cli/src/federation.rs::run_join_room`): loads a real `hs-config` file the same way `hs
serve` would (same server name, same signing key, same federation policy) and runs `join_room`
against it, printing what was verified. Opens no storage -- nothing it produces can be persisted
locally yet, so there is nothing for it to open. This is not what `POST /join` calls (no wiring
exists there yet, and adding it needs RFC-0015 first): it is the only way, today, to exercise the
real cross-server join handshake this crate provides.

### 4. The script: `crates/hs-federation/scripts/two-server-federation.sh`

Runnable in one command (`bash crates/hs-federation/scripts/two-server-federation.sh [workdir]`,
workdir defaults to a fresh `mktemp -d`). Requires `cargo`, `openssl`, `curl`, `jq`, and `stunnel`
(`brew install stunnel` on macOS -- `hs serve` does not terminate TLS itself yet, exactly the same
reasoning and the same tool `tests/complement/Dockerfile.template` already uses; this script's
`stunnel.conf`s are the same accept-here-forward-there shape as `tests/complement/stunnel.conf.template`).
No Docker.

What it does, in order, against two real `hs serve` processes on 127.0.0.1 with different
server names (`127.0.0.1:8448`, `127.0.0.1:8449`) and different embedded-storage data directories:
generates a private CA and one IP-SAN server certificate; writes each server's native config
(`federation.custom_ca_certificates` pointing at the shared CA, `ip_range_blocklist: []` since both
instances are on loopback); starts both `hs serve` processes and a `stunnel` in front of each
(TLS on `:8448`/`:8449`, forwarding to plaintext `:8008`/`:8018`); sanity-checks that A can fetch
B's `/_matrix/key/v2/server` over real TLS with the private CA (the §1 fix, exercised first,
because everything after it depends on outbound TLS actually working); registers `@alice` on A and
`@bob` on B through the real client-server UI-auth dance; alice creates a public room and sends a
message; **B joins A's room via `hs federation-join-room`, the real make_join/send_join handshake**;
and finally verifies, by querying A's own client API (not the script's own say-so), that bob really
is a joined member. Prints a clear summary of what was proven and what a real public join would
still exercise that this run does not (DNS-based discovery, a publicly trusted CA, another
implementation's quirks, version negotiation against a server that is not itself) -- see the
script's own final output for the exact wording, since it is the artifact this file should not
duplicate and risk drifting from.

Ran twice against two fresh workdirs this session; both runs succeeded identically.

### Verification

```
cargo fmt -p hs-federation -p hs-cli                                          # applied, no diffs after
cargo test -p hs-federation                                                   # 124/124 (was 120)
cargo test -p hs-cli --test federation_reads --test federation_writes --test e2e   # 7+8+9 = 24/24
cargo test -p hs-loadgen --test real_client                                   # 1/1 (single-server path unaffected)
bash crates/hs-federation/scripts/two-server-federation.sh                    # succeeds end to end, twice
```

`cargo clippy -p hs-federation --all-targets -- -D warnings`: **fails**, but not on this crate's
code -- `crates/hs-http/src/cors.rs` (a different track's crate, with uncommitted, in-progress
changes present in the working tree at the time of this session, confirmed via `git status`/`git
diff`) trips `clippy::double_must_use` on a function this session did not touch, and workspace
clippy lints every crate in the dependency graph, not just the one named with `-p`, unless
`--no-deps` is passed. `cargo clippy -p hs-federation --all-targets --no-deps -- -D warnings` is
clean, proving this crate's own code is not the source. Not something this track can or should fix
(`crates/hs-http/**` is out of this session's ownership); flagged here so the next session does not
waste time re-diagnosing it, and re-run without `--no-deps` once that other track's work lands or
is reverted.

## Sixth session: TLS/CA trust, and the redaction-before-signing bug

Scope, per this session's brief: close the TLS/CA gap `docs/status/14-test-and-conformance.md`
identified as blocking almost all of Complement's federation package (5/89 passing; 27 of the
remaining failures showed `tls: unknown certificate authority` in the harness's container logs),
and diagnose/fix the real `send_join` signature-verification bug track 14 found waiting underneath
it once the TLS symptom was worked around. Ownership this session: `crates/hs-federation/**`,
`crates/hs-config/**`, `docs/status/06-federation.md` only -- no `hs-cli`, `hs-room`, or any other
crate, and no Docker (track 14 owns Complement runs).

### 1. The TLS/CA gap: root cause, confirmed

`crates/hs-federation/src/client.rs::client_for` builds every outbound `reqwest::Client` with
`danger_accept_invalid_certs(!verify_certificates)` and otherwise reqwest's default TLS behaviour.
The workspace's `reqwest` dependency (`Cargo.toml`: `features = ["json", "rustls-tls"]`) resolves
`rustls-tls` to `rustls-tls-webpki-roots` only -- the ~140 baked-in public root CAs, never the OS
trust store and never any application-supplied CA. There was no config surface anywhere in
`hs-config`/`hs-federation` to add a trusted CA (confirmed by grep, matching track 14's own
finding), so a harness like Complement that runs `update-ca-certificates` to trust its generated CA
system-wide had no effect on this server's outbound federation client: only `verify_certificates:
false` (which trusts *any* certificate) could get past it, at the cost of disabling TLS
authentication entirely.

### 2. The fix: a real config surface, honoured by the client

**`hs-config::FederationConfig`** (`crates/hs-config/src/federation.rs`) gained two new fields,
both `#[serde(default)]` (empty/false), with the reasoning behind each captured in the field's own
doc comment (the deliverable's own instruction) rather than only here:

- **`custom_ca_certificates: Vec<String>`** -- paths to PEM-encoded CA certificate files, trusted
  *in addition to* the built-in public roots. Directly matches Synapse's own
  `federation_custom_ca_list`, which is exactly what `refs/synapse/docker/complement/conf/workers-shared-extra.yaml.j2`
  sets for Complement. Validated (`Validate` impl): an empty-string entry is rejected with a
  helpful message, the same style as the existing `domain_allowlist` check.
- **`trust_os_root_store: bool`** (default `false`) -- whether outbound federation TLS also trusts
  whatever CA store the operating system trusts. **Decision, with the justification inline in the
  field's own doc comment**: default `false`. Trusting the OS store is the right call for *some*
  deployments (an admin who runs `update-ca-certificates` to add a corporate or test CA reasonably
  expects every TLS client on the box, including this one, to honour it), but it is the wrong
  *unconditional default* for federation specifically: federation traffic authenticates servers
  that never agreed on a shared root of trust ahead of time, so silently broadening that trust to
  whatever the OS store happens to contain (which can be widened by anyone with root, for reasons
  having nothing to do with running a homeserver -- an unrelated package, a corporate
  TLS-inspecting proxy, a forgotten test cert) is a real, quiet security regression for exactly this
  traffic. Pairing a `false` default with the explicit, narrow `custom_ca_certificates` puts the
  choice with whoever configures federation, not whoever last ran `update-ca-certificates` for an
  unrelated reason.

**`crate::client::ClientConfig`** (`crates/hs-federation/src/client.rs`) gained the matching fields
the outbound client actually reads: `custom_root_certificates: Vec<Vec<u8>>` (raw PEM bytes, not
paths -- file I/O stays at the config-loading wiring site, so this crate's own tests can hand it
bytes straight from `rcgen` without touching a filesystem) and `trust_os_root_store: bool`.
`FederationClient::new` parses `custom_root_certificates` once via
`reqwest::Certificate::from_pem_bundle` (one entry may itself be a multi-certificate bundle) into a
new `custom_roots: Vec<reqwest::Certificate>` field, logging `tracing::error!` (not panicking, not
silently dropping) for any entry that fails to parse. `client_for` calls
`.add_root_certificate(cert.clone())` for each one -- additive, never replacing the built-in public
bundle -- and `.tls_built_in_native_certs(self.config.trust_os_root_store)` to gate the OS store.

**Enabling the OS-store toggle for real** (not just documenting an inert field) needed reqwest's
`rustls-tls-native-roots` feature, which is off at the workspace level (only `rustls-tls`, i.e.
webpki-roots, is enabled there). Added it in `crates/hs-federation/Cargo.toml` specifically (`reqwest
= { workspace = true, features = ["rustls-tls-native-roots"] }`), not the workspace root -- it is
additive to the existing `rustls-tls` feature (both root sources compile in; which one(s) actually
get consulted per-request is controlled entirely by the two `tls_built_in_*` calls above, not by
which features happen to be compiled in) and costs nothing new to fetch: `rustls-native-certs` and
its platform dependencies (`security-framework` on macOS, `schannel` on Windows) were already
resolved in the workspace's `Cargo.lock` via another crate before this session. Confirmed via
`cargo check -p hs-config -p hs-federation` that no new crate needed fetching.

### 3. `verify_certificates: false` is now loud

`FederationClient::new` logs a prominent `tracing::warn!` once, at construction time, whenever
`config.verify_certificates` is `false`, spelling out exactly what it means (outbound TLS accepts
*any* certificate from *any* peer; every event's trust then rests entirely on its Ed25519
signature; a MITM on outbound federation traffic can impersonate any remote server) and naming
`custom_ca_certificates` as the narrower alternative. `hs-config::FederationConfig::verify_certificates`'s
own doc comment carries the same warning for anyone reading the schema directly rather than the
running server's logs. The field itself is unchanged (`hs-cli`'s existing wiring already threads it
through) -- "loud" was achieved entirely inside this crate, at the one place (`FederationClient::new`)
every real mount already calls exactly once per server startup, so no `hs-cli` change was needed to
satisfy this deliverable.

### 4. The real proof: an in-process TLS test, no Docker

`crates/hs-federation/src/client.rs::tests::outbound_tls_rejects_an_unconfigured_ca_but_trusts_a_configured_one`
(plus its helper `spawn_self_signed_tls_peer`): mints a real self-signed certificate for
`"localhost"` with `rcgen` (already a dev-dependency), terminates real TLS with it via
`rustls`/`tokio-rustls` over a real loopback `TcpListener`, and serves one plain HTTP/1.1 response
per connection via `hyper::server::conn::http1` (wrapped for hyper's IO traits via
`hyper_util::rt::TokioIo`) -- no axum, since `axum::serve` only accepts a `TcpListener`-shaped
`Listener` in this axum version and standing up a custom TLS-terminating `Listener` impl was not
worth it for a test this size. Two assertions against the *exact same* peer and certificate:
`FederationClient` with a default `ClientConfig` (no custom CA) gets `ClientError::Request` (the
TLS handshake genuinely fails, exactly Complement's pre-fix symptom); the same client with
`custom_root_certificates: vec![cert.pem().into_bytes()]` gets a real `200`. New dev-dependencies
for this one test, all already `[workspace.dependencies]` entries used elsewhere in the workspace
(no new crate to fetch): `rustls`, `tokio-rustls`, `rustls-pki-types`, `hyper`, `hyper-util`,
`http-body-util`.

### 5. The bug underneath: `send_join` rejecting a genuinely signed join

Per track 14's diagnosis (`docs/status/14-test-and-conformance.md`): once the TLS symptom was
worked around, `send_join` started failing with `M_BAD_JSON: signature from
host.docker.internal:.../ed25519:... does not verify` on a join this server had no legitimate
reason to reject. **Root cause, confirmed by reading the spec directly**
(`refs/matrix-spec/content/server-server-api.md`, "Validating hashes and signatures on received
events"): signature verification must always run against the event's **redacted** form, never the
full one -- "the event is redacted following the redaction algorithm, and the resultant object is
checked for signatures... this step should succeed whether we have been sent the full event or a
redacted copy." A conformant sender signs the redacted form too (the same document's "Adding hashes
and signatures to outgoing events": hash, then redact, then sign, then copy the signature back onto
the original). `crate::inbound::verify_pdu` was calling
`hs_model::signing::verify_object(event.json(), ...)` -- the **full, unredacted** event -- instead
of the redacted one. For any event whose content carries anything redaction would strip (which for
`m.room.message` is *all* of `content`, and for `m.room.member` is anything beyond `membership`
itself, e.g. a profile), this rejects a perfectly legitimate signature.

**Fixed** in `crates/hs-federation/src/inbound.rs::verify_pdu`: computes `event.redacted_json()`
(the existing, already-tested `hs_model::Event` method) and verifies the signature against that,
not `event.json()`. The returned `Event` is unchanged (full content and all) -- only the bytes
`verify_object` checks the signature against changed. Confirmed via a new, isolated test
(`inbound::tests`'s existing `verify_pdu_accepts_a_correctly_signed_event` etc. all still pass, and
this crate's own event-signing test helpers were updated to actually sign the redacted form --
see below) plus manual reasoning against the spec text quoted above.

**A necessary companion fix to this crate's own tests**: `crate::inbound::tests::signed_event` and
`crate::backfill::tests::signed_message` both built an `m.room.message` and signed the **full**
object directly (the same shape of bug §6 below describes in `hs-room`), which is exactly what
`verify_pdu`'s old, wrong check happened to accept and its new, correct check would reject. Both
were fixed to the spec's real order: build the full object with `hashes` attached, redact it
(`hs_model::redaction::redact`), sign the *redacted* copy, then copy `signatures` back onto the
full object before returning it -- matching what a real conformant sender does and what
`verify_pdu` now actually checks. `cargo test -p hs-federation --lib` is green at 120/120 with both
the production fix and both test-helper fixes in place; `crate::join::tests::sign_member_event` did
**not** need this fix, because its events' content is exactly `{"membership": "join"}`, which
`m.room.member` redaction keeps unchanged (full and redacted forms are byte-identical for that
narrow content shape), so the bug had no observable effect there.

### 6. The same bug, found live in `hs-room`'s own outbound pipeline -- not fixed this session, RFC filed

Fixing `verify_pdu` correctly (§5) also makes it reject **this server's own previously
self-consistent, but spec-non-compliant, signatures** wherever both sides used to agree only by
both being wrong the same way. Confirmed empirically (read-only `cargo test -p hs-cli --test
federation_writes`, no `hs-cli` file edited): 4 of 8 tests newly fail. Three
(`send_rejects_a_new_event_whose_auth_events_do_not_authorize_it`,
`send_backfills_a_missing_ancestor_then_accepts_the_original_event`,
`send_gives_up_when_the_remote_serves_an_endless_backfill_chain`) are the same test-fixture bug as
§5's companion fix -- hand-built synthetic PDUs signed unredacted, mechanical fix, three lines each,
full instructions in the RFC below. The fourth,
**`send_accepts_an_event_it_already_holds_idempotently`, is not a test bug**: it resubmits an event
this server actually built and signed through the real `RoomActor`/`pipeline.rs` path, and it now
fails signature verification too -- direct proof that `crates/hs-room/src/pipeline.rs`'s
hash-and-sign step (`build_and_authorize`, around line 360-373) signs the **full, unredacted**
canonical object with no redaction step, the identical bug `verify_pdu` just had, still live in
production code this session does not own. **Consequence, if left unfixed**: any real,
spec-compliant remote homeserver, correctly redacting before checking (as `verify_pdu` now does
too), would reject this server's own outbound events whenever their content carries anything
redaction would strip -- which is every ordinary `m.room.message`. This is filed as
`docs/rfcs/0014-event-signing-must-sign-the-redacted-form.md`, addressed to track 04
(`crates/hs-room/src/pipeline.rs`) with the exact three-line shape of the fix (mirroring what this
session already did twice in its own test helpers) plus the three `hs-cli` test-fixture call sites
that need the same mechanical correction. Not fixed here: `crates/hs-room/**` is outside this
session's ownership, and `crates/hs-cli/**` likewise.

### Verification

```
cargo fmt -p hs-federation -p hs-config                                # applied, no diffs after
cargo clippy -p hs-federation -p hs-config --all-targets -- -D warnings # clean
cargo test -p hs-federation -p hs-config                               # 120 + 73 passed, 0 failed
cargo run -p hs-config --bin gen_config_docs                           # regenerated docs/config.md
```

`cargo test -p hs-cli --test federation_writes` (read-only check, no `hs-cli` file touched): 4/8
pass, 4/8 fail exactly as described in §6 above -- expected, not a regression this session
introduced silently; see the RFC for the fix.

## Fifth session: the backfill loop

Scope (`docs/next-steps.md` item 4): consume `RoomError::MissingAncestors` instead of merely
reporting it. A remote's join could already be persisted (fourth session); the very next event
that server sent citing history from before the join could not, because nothing fetched that
history. This session closes that.

### 1. The loop, in one paragraph

New module `crates/hs-federation/src/backfill.rs`. When `RoomWriteSink::accept_verified_event`
rejects an event because it names ancestors this server does not hold
(`WriteRejected::missing_ancestors`, new -- see below), `crate::inbound::process_transaction`
calls `crate::backfill::resolve_missing_ancestors(origin, room_id, room_version, missing_ids,
fetcher, key_cache, sink, limits)`. That function asks `origin` (the server that sent the
transaction -- the natural peer to ask, since it is the one that told us about an event referencing
history it presumably has) for the missing events via `GET /backfill/{roomId}?v=...&limit=...`,
verifies each returned PDU exactly the way `verify_pdu` verifies any inbound PDU (content hash,
then signature against the *sender's* server, not `origin`), and persists them through the same
`RoomWriteSink` in dependency order. If persisting one of *those* events itself reports a deeper
`MissingAncestors` (the gap is more than one event deep), that becomes the next round's fetch
target -- the loop is genuinely recursive, not a single fetch-and-hope. Once the fetched events are
all either persisted or hard-rejected (bad auth -- see below), control returns to
`process_transaction`, which retries the original event exactly once. Success or failure of that
retry is reported the normal way: a per-event `{}` or `{"error": ...}` inside the transaction
response, never a fatal transaction failure -- backfill giving up looks, from `/send`'s caller's
point of view, exactly like the event being unresolvable for any other reason.

### 2. The limits, and why

`BackfillLimits` (`crate::backfill`), four independent dimensions, all required to fail before the
attempt gives up on a genuine gap it just cannot close, and any one of which stops the attempt on
its own:

| Field | Default | What it bounds |
|---|---|---|
| `max_events_per_fetch` | 100 | The `limit` sent on each `/backfill` request, **and** the most events accepted from one response even if the peer sends more. Matches `transport::read_routes::MAX_BACKFILL_LIMIT`, this server's own server-side clamp on the same endpoint -- this server never asks for more than it would itself agree to answer, and never trusts a peer that ignores the `limit` it was given. |
| `max_rounds` | 10 | The most `/backfill` round-trips one resolution attempt makes to the same peer. This is the recursion-depth bound: each round can surface a new, deeper gap (a fetched event's own `prev_events` can themselves be missing), so a chain deeper than 10 hops is given up on, not chased further. **Confirmed load-bearing by mutation test** -- see below. |
| `max_total_events` | 500 | The total number of events fetched and signature-*verified* (an asymmetric-crypto operation) across every round, independent of how few rounds it took to reach that count. This is the real cost-of-attack bound: rounds alone would not stop a peer that returns many events per round. |
| `max_duration` | 20s | Wall-clock ceiling for the whole attempt (`tokio::time::timeout` around the entire resolution), so a slow-but-not-failing peer cannot hold the task open indefinitely. |

The request itself is also bounded independent of the response: `frontier.iter().take
(max_events_per_fetch)` caps how many event IDs go into one `?v=...&v=...` query string, so a
round that somehow discovered many independent gaps at once cannot turn into an unbounded URL.

A peer that returns *nothing new* (an empty response, or a response containing only events already
seen earlier in the same attempt) is treated as "cannot make progress" and the attempt gives up
immediately (`BackfillGiveUpReason::StillMissing`) rather than retrying the same request pointlessly
for the remaining rounds -- a real inability to help is distinguished from "still trying".

### 3. What an attacker can and cannot cost this server

**Can**: force up to 10 HTTP round-trips to itself, up to 500 signature verifications (Ed25519
verify is cheap -- microseconds -- and `RemoteKeyCache` already deduplicates concurrent lookups for
the same `(server, key_id)`, so the realistic cost is closer to "500 verifies against a handful of
cached keys" than 500 separate key fetches), and up to 20 seconds of one Tokio task's wall-clock
time, **per event that names a missing ancestor**. It can repeat this for every hostile event it
sends, so the aggregate cost across many transactions is not bounded by this module alone --
but each individual attempt is small, finite, and cannot compound into recursion, an unbounded
response, or an indefinite hang. The existing per-destination concurrency limit
(`FederationClient`, default 1 in-flight request) and `DestinationStore` backoff additionally throttle
*how fast* a single hostile server can trigger repeated attempts, though that is a pre-existing
defence this session did not add, not something `backfill.rs` itself enforces.
**Cannot**: make this server recurse forever (bounded by `max_rounds` and `max_total_events`,
enforced independently so neither alone is a single point of failure), make it accept an
unauthorized or malformed event (every fetched event still goes through the exact same
`verify_pdu` + `RoomActor::accept_remote_event`'s two-snapshot authorization check as any other
inbound event -- backfill is a *source* of candidate events, not a bypass of anything that checks
them), make it hang past 20 seconds on one attempt, or make it send an unbounded request (the `v=`
list is capped the same as the response).

### 4. The outbound `/backfill` client

`FederationClient::backfill` (`crates/hs-federation/src/client.rs`), alongside the inbound
`/backfill` server (`transport::read_routes::backfill`, existing since the second session). Builds
`GET /_matrix/federation/v1/backfill/{roomId}?limit=N&v=...`, calls the existing
`FederationClient::send` (the one signed-request path every other outbound call already goes
through -- no second X-Matrix client), and returns the raw, **unverified** `pdus` array. Verifying
is deliberately not this method's job: `crate::backfill::resolve_missing_ancestors` is the one
place that owns "fetch, then verify" as a sequence, so there is exactly one path where a fetched
event might be trusted before it is checked, and it is easy to audit.

`crate::backfill::AncestorFetcher` is the seam `FederationClient` implements this through
(`impl AncestorFetcher for FederationClient`), the same shape as `RoomDataSource`/`RoomWriteSink`:
a trait this crate owns, so `crate::backfill`'s own tests can supply fakes (`QueuedFetcher`,
`EndlessFetcher`) without a real HTTP server, and `hs-cli`'s integration tests can supply a real
`FederationClient` pointed at a real loopback listener.

### 5. Mutation test performed this session

Per this session's instructions: `BackfillLimits::default().max_rounds` was changed from `10` to
`usize::MAX` in `crates/hs-federation/src/backfill.rs`, and
`backfill::tests::gives_up_cleanly_on_an_endless_chain_instead_of_looping_forever` (which asserts
the endless-chain peer above is given up on after exactly 10 round-trips with
`BackfillGiveUpReason::TooManyRounds`) was re-run:

```
thread 'backfill::tests::gives_up_cleanly_on_an_endless_chain_instead_of_looping_forever' panicked
  at crates/hs-federation/src/backfill.rs:604:9:
Err(TooManyEvents)
test result: FAILED. 0 passed; 1 failed; ... finished in 3.18s
```

The test failed exactly as expected: with the round bound removed, the *other* independent limit
(`max_total_events`, still 500) caught the runaway instead, 500 rounds in instead of 10 -- proving
`max_rounds` is what the original test's "exactly 10 round-trips" assertion depends on, not
incidental behaviour elsewhere in the loop. It did **not** hang (3.18 seconds for 500 in-process
mock round-trips, no real network involved in this unit test), which is itself informative: even
with one limit disabled, the layered design meant this session never had to interrupt a genuinely
runaway process to observe the failure. The change was reverted immediately
(`max_rounds` back to `10`); `cargo test -p hs-federation --lib backfill` is green (6/6) with the
revert in place. This is recorded here rather than kept as a second standing test, because a test
cannot mutate the default it is itself asserting against without either duplicating the limit or
ceasing to test what its name says (see the comment left in place of the test in
`crates/hs-federation/src/backfill.rs`).

### 6. `WriteRejected` gained a structured `missing_ancestors: Vec<String>` field

(`crates/hs-federation/src/inbound.rs`, plus its two constructors `WriteRejected::other` and
`WriteRejected::missing_ancestors`.) Previously the only signal was a human-readable `error`
string; `hs_cli::federation::RegistryWriteSink` had already started interpolating the missing IDs
into that string for operator-log readability, but nothing machine-readable distinguished "missing
ancestors, maybe backfillable" from "any other rejection, not backfillable" without string-matching
the message. `crate::backfill::resolve_inner` reads `rejected.missing_ancestors` directly.
`RegistryWriteSink::accept_verified_event` (`crates/hs-cli/src/federation.rs`) now constructs
`WriteRejected::missing_ancestors(id_strings, message)` for exactly the `RoomError::MissingAncestors`
case and `WriteRejected::other(...)` everywhere else, so the string in `error` and the structured
list can never drift apart (one format call builds both).

### 7. Testing shape

- **`crates/hs-federation/src/backfill.rs`** (6 new lib tests): `resolves_a_single_hop_gap`,
  `resolves_a_multi_hop_gap_across_several_rounds` (a two-deep chain, fetched one hop per round,
  proving the loop actually recurses and that an event fetched in an earlier round is retried once
  its own blocker lands -- this is what caught a real bug during development, see below),
  `gives_up_cleanly_on_an_endless_chain_instead_of_looping_forever`,
  `gives_up_when_the_remote_returns_nothing`. Fakes: `QueuedFetcher` (hands back a scripted
  sequence of responses), `EndlessFetcher` (never converges), `DagSink` (a minimal
  `RoomWriteSink` that actually enforces "prev_events must already be known", closely mirroring
  `RoomActor::accept_remote_event`'s real shape without needing a real room actor).
- **`crates/hs-federation/src/client.rs`** (1 new test): `backfill_sends_a_signed_get_and_parses_the_pdus`,
  against `hs_testkit::FakeFederationPeer` over a real loopback socket -- asserts the exact request
  shape (`GET .../backfill/{roomId}?limit=N&v=...`) a real peer would receive, not just that the
  method compiles.
- **`crates/hs-cli/tests/federation_writes.rs`** (2 new end-to-end tests, `Harness` extended with
  `Harness::with_backfill_peer(port)`, a real `FederationClient` pointed at a real loopback
  listener via the explicit-port destination form `localhost:{port}` -- exactly the seam
  `crates/hs-federation/src/client.rs`'s own tests already use, so this is not a new test pattern):
  - `send_backfills_a_missing_ancestor_then_accepts_the_original_event`: a hand-built, correctly
    signed and hashed `m2` cites a hand-built `m1` this server was never sent. A minimal axum
    server (not `hs-testkit`, which `hs-cli` does not depend on -- see "Decisions made") answers
    `/backfill` with exactly `m1`. Asserts `m2` is accepted and `m1` is independently readable back
    through `/event/{id}` afterwards -- both fetched-via-backfill and original-event persistence
    are checked, not just "the transaction returned 200".
  - `send_gives_up_when_the_remote_serves_an_endless_backfill_chain`: the hostile peer described
    above, over real HTTP. Asserts the transaction reports a per-event error mentioning the
    give-up (not a hang, not a fake success) and that **exactly** `BackfillLimits::default()
    .max_rounds` requests reached the peer -- the bound is checked by counting real HTTP requests
    that arrived, not just by inspecting the returned error type.

**A real bug this session's own tests caught before it reached these two integration tests**: the
first version of `resolve_inner` dropped a fetched-but-blocked event the moment its first
persistence attempt failed, re-deriving only the next `frontier` from it and discarding the event
itself. That worked for a single-hop gap but silently lost multi-hop chains: fetching `e1` (which
unblocks `e2`, already fetched and discarded in the previous round) would never retry `e2`, and the
loop would report success once `e1` alone persisted even though `e2` -- the actual descendant
needed -- was still missing. `backfill::tests::resolves_a_multi_hop_gap_across_several_rounds`
failed immediately (`assertion failed: sink.known.lock().unwrap().contains(&e2_id)`) and pinpointed
exactly this. Fixed by carrying a `pending: Vec<Event>` worklist across rounds (not just within one)
and re-attempting the *entire* worklist, sorted ancestors-first by `depth`, every round -- so an
event unblocked by this round's fetch is retried in the same pass that unblocks it, not abandoned
after its first failed attempt.

**A second real thing this session's tests caught, about the auth rules rather than backfill
itself**: constructing a hand-signed test event first failed with "no m.room.create event in auth
events" -- this session had assumed (incorrectly) that room version 11 excludes `m.room.create`
from a message event's `auth_events` selection the way version 12 does
(`room_create_event_id_as_room_id`). Reading `crates/hs-model/src/room_version.rs` directly showed
V11 does **not** set that flag (only V12 does); `hs_state::auth::expected_auth_types` therefore
still requires `m.room.create` in the selection for V11. Not a bug in this session's production
code -- a wrong assumption in the test's construction, caught by the real auth rules doing their
job. Recorded here because the next person hand-constructing a V11 test event will hit the exact
same thing.

### 8. Wiring

`hs_cli::federation::build_mount` (`crates/hs-cli/src/federation.rs`, this track's file) now sets
`ancestor_fetcher: Some(client.clone() as Arc<dyn hs_federation::backfill::AncestorFetcher>)` --
the *same* `FederationClient` every other outbound call already uses, so backfill shares its
signing key, per-destination concurrency limit and backoff state with everything else this server
sends. `manifest_only_mount` sets `ancestor_fetcher: None` (routes-manifest generation needs no
outbound capability). No `serve.rs` change was needed this session -- `FederationState` already
flowed through unchanged mount points; the two new fields are just two more fields on a struct that
was already being threaded through.

### Verification

```
cargo fmt -p hs-federation -p hs-cli                              # applied, no diffs after
cargo clippy -p hs-federation --all-targets -- -D warnings        # clean
cargo clippy -p hs-cli --all-targets --no-deps -- -D warnings     # clean (see fourth session's
                                                                   # note on why --no-deps)
cargo test -p hs-federation                                       # 119 passed (lib), 0 failed
cargo test -p hs-cli --test federation_reads --test federation_writes
                                                                   # 7 + 8 passed, 0 failed
```

## Done

All against `docs/design/06-federation-threat-model.md`'s catalogue; file paths below are all
under `crates/hs-federation/src/` unless stated otherwise.

- **`Cargo.toml`** (crate and workspace root). Added to `[workspace.dependencies]`:
  `hickory-resolver` (0.26, `tokio` feature — `system-config` comes along via its own default
  features) for DNS, and `ipnet` (2.x) for CIDR containment checks (promoted from a transitive dep
  of `hickory-net` to a direct one rather than hand-rolling CIDR matching). Both noted in the root
  `Cargo.toml` with "Added by track 06" comments per convention. The crate's own `Cargo.toml` pulls
  in `hs-model`, `hs-kv`, `hs-tables`, `hs-http`, `hs-config`, `ruma`, `ed25519-dalek`, `rand_core`,
  `axum`, `tokio`, `reqwest`, `regex`, `async-trait`, and dev-deps `hs-testkit`, `tempfile`,
  `tower`, `rcgen`.
- **`discovery.rs`** (20 tests). The full server-name resolution algorithm (IP literal / explicit
  port / well-known+SRV+fallback), behind three traits (`WellKnownFetcher`, `SrvResolver`,
  `AddrResolver`) so the resolution logic is unit-tested against fakes reproducing the spec's
  worked examples, with real `reqwest`-backed (`HttpWellKnownFetcher`) and
  `hickory-resolver`-backed (`HickoryResolver`) implementations for production. Covers: an explicit
  port bypassing discovery entirely, a well-known that itself needs SRV resolution, SRV falling
  back to the deprecated `_matrix._tcp` service, malformed/absent well-known falling back to SRV on
  the *original* host, no-redirects-followed (structurally impossible in this design — the fetcher
  trait has no redirect-following code path at all), body-size cap enforced without buffering past
  it, and `CachingWellKnownFetcher` (a TTL decorator honouring `clamp_cache_control`, with separate,
  shorter negative caching for failures — the plan's "wrap `WellKnownFetcher` in a decorator"
  item). `MIN`/`MAX`/`DEFAULT`/`FAILED` cache TTL constants are committed to concrete values (60s /
  24h / 24h / 60s) since the first session left them unpicked.
- **`keys.rs`** (14 tests). `OwnSigningKeys::load_or_generate` (Synapse's `algorithm version
  base64_seed` line format, first-boot generation, multiple keys from separate files for rotation,
  malformed lines skipped not fatal). `build_server_key_response` (self-signed with every active
  key, `old_verify_keys` carried through). `wrap_for_notary` (adds the notary's own signature
  without touching the origin's). `RemoteKeyCache` with `KeyServerFetcher` as the fetch seam,
  per-origin in-flight de-duplication (`tokio::sync::Mutex` keyed by origin, tested with 8
  concurrent callers producing exactly 1 fetch), self-signature verification before trusting
  anything (tampered key, wrong claimed `server_name`, both rejected), and the old-verify-keys
  rule: `get_current` (now-valid only) vs `get_valid_at(ts)` (accepts a key that was valid *when
  signed* even if since rotated out, rejects one that had already expired) — this is the
  "expired key rejected, key valid at signing time still accepted" pair from the assignment,
  tested directly. Cross-server key substitution is structurally impossible (cache keyed by
  `(server_name, key_id)`, a response can only populate the server name it claims and self-signed
  for) and tested.
- **`xmatrix.rs`** (12 tests). Header parse (quoted/bare values, any field order, RFC 7235 escapes,
  rejects multiple `Authorization` headers, rejects wrong scheme, rejects a missing field) and
  build. The exact signed-object shape (`method`, `uri`, `origin`, `destination`, `content?`) built
  on `hs_model::canonical`/`signing` directly, not reimplemented. `verify_x_matrix`, the axum
  middleware: destination-must-equal-own-name, body-size cap enforced before signature
  verification reads it (`axum::body::to_bytes` bounded at 50 MiB), key lookup through
  `RemoteKeyCache::get_current`, full signature reconstruction and verification. Tests: tampered
  body rejected, wrong destination rejected, wrong key rejected, missing signature rejected,
  replayed valid signature against a *different* route rejected (proves `uri` binding actually
  works, not just "any valid sig passes"), multiple `Authorization` headers rejected, unknown
  origin rejected without ever caching a bogus key. **Found and fixed a real layering-order bug
  while writing these tests**: `Router::layer` wraps outside-in (last `.layer()` call runs
  first), so `Extension(ctx)` must be added *after* `from_fn(verify_x_matrix)`, not before, or the
  context is missing and every request 500s. Documented prominently in both the module doc and the
  `verify_x_matrix` doc comment so `crate::transport::router` (which has the same ordering
  requirement) doesn't regress it.
- **`acl.rs`** (11 tests). One `is_allowed(server_name, &ServerAcl) -> bool` function, used for
  both directions per the threat model's explicit requirement (no direction parameter exists, so
  there is no way to call it asymmetrically). Anchored glob-to-regex translation, tested for both
  match and adjacent-non-match (`*.example.org` vs `example.org` itself and vs
  `example.org.evil.com`; `?` matching exactly one character, not zero or two; a literal `.` in a
  pattern not acting as regex "any character"). `allow_ip_literals` checked against the *string
  form* of the server name, distinct from (and not a replacement for) `ip_range_blocklist`.
- **`room_source.rs`** (4 tests). The `RoomDataSource` trait per threat model section 5's seam
  decision (membership/visibility, event/state/state-ids/auth-chain/backfill/missing-events/
  hierarchy/timestamp-lookup, plus `get_event_by_id` for `/event/{eventId}`'s room-less path
  shape), and `InMemoryRoomSource`/`FakeRoom` for this crate's own tests. Every method takes
  `requesting_server` explicitly; `InMemoryRoomSource` enforces the visibility check *before*
  returning content in every method, and a test asserts a non-member gets `NotVisible` before ever
  seeing event content.
- **`destination_store.rs`** (7 tests). `DestinationState` (failure count, next retry time,
  exponential backoff base 1s doubling capped at `max_backoff_ms`, full jitter). `DestinationStore`
  trait with `InMemoryDestinationStore` and `KvDestinationStore<B: KvBackend>` (a real
  `hs_tables::TypedKeyspace` under `hs_federation.destinations`). The "persisted so a restart
  resumes" requirement is tested directly: two independent `KvDestinationStore` handles opened over
  the *same* `MemoryBackend` instance (simulating a process restart against the same on-disk store)
  see the same accumulated failure count.
- **`client.rs`** (10 tests). `FederationClient`: per-destination `tokio::sync::Semaphore`
  (default concurrency 1, tested that two concurrent sends to the same destination both complete
  rather than deadlocking), `DomainPolicy` (checked against the *original* server name, before any
  discovery happens — tested that a denied domain never reaches the network), `IpPolicy` (CIDR
  block/allow via `ipnet`, allowlist overrides blocklist, checked against the *resolved* address —
  tested that a blocked destination is rejected even though `AddrResolver` successfully resolved
  it), backoff-aware dispatch via `DestinationStore` (a backing-off destination is rejected before
  any network call), and a `reqwest::Client` cache pinned per destination via `.resolve()` so the
  TLS SNI / `Host` stays the delegated name while the TCP connection goes to the actually-resolved
  address (the spec's delegation contract). HTTP/1.1-only to peers, per the brief's recommendation
  — committed as a decision below. Tested end-to-end against `hs_testkit::FakeFederationPeer` bound
  to a real loopback socket (see `ClientConfig::scheme`, a documented test-only seam — see
  "Decisions made").
- **`transport/`** (a `mod.rs` + `read_routes.rs` + `seams.rs` + `queries.rs`, 9 tests in `mod.rs`
  + `read_routes.rs` + `seams.rs` combined). `router()` builds one merged `axum::Router` from
  `read_routes::add_routes` and `seams::add_routes` composed into a single `hs_http::router::Builder`
  (not two separately-built routers merged at the root — axum forbids nesting at `""`, discovered
  while wiring this up), then applies `axum::middleware::from_fn(verify_x_matrix)` and
  `Extension(x_matrix_ctx)` **exactly once**, over the whole thing, in the correct order (see the
  xmatrix.rs bug above). **The load-bearing test**
  (`transport::tests::every_route_is_behind_the_x_matrix_layer`) walks the router's own
  `RouteManifest` — not a hand-maintained list — substitutes a placeholder for every `{param}`
  path segment, and asserts every single registered route returns 401 with no `Authorization`
  header. This test does not need updating when a route is added; it fails automatically the
  moment a future route ships outside the merge. Fully implemented, against `RoomDataSource` and a
  small `FederationQuerySource` seam (profile/directory/devices/openid — not room-scoped, so not
  part of `RoomDataSource`): `/version`, `/query/{queryType}` (profile, directory),
  `/user/devices/{userId}` (gated on `allow_device_name_lookup_over_federation`), `/publicRooms`
  (gated on `allow_public_rooms_over_federation`, limit clamped to 100), `/hierarchy/{roomId}`,
  `/timestamp_to_event/{roomId}`, `/openid/userinfo`, `/event/{eventId}`, `/state/{roomId}`,
  `/state_ids/{roomId}`, `/event_auth/{roomId}/{eventId}`, `/backfill/{roomId}` (limit clamped to
  100), `/get_missing_events/{roomId}` (limit clamped to 100). Every per-room handler checks
  membership/visibility via `RoomDataSource` before returning content — tested directly (a
  non-member gets 403, a member gets 200, for the same event). `/send`, every join/leave/knock/
  invite handshake variant (v1 and v2 where the spec has both), `/exchange_third_party_invite`,
  `/3pid/onbind`, `/user/keys/claim`, `/user/keys/query` (joint-owned with track 08), and
  `/rooms/{roomId}/complexity`, `/extremities/{roomId}`, `/query/account_status` are mounted as
  seams: one shared `not_implemented` handler, behind the same layer as everything else, returning
  a typed 501. A test walks every seam route the same way the layer test does and asserts every one
  responds 501. Media endpoints and the two `_synapse/client/*` compat entries are deliberately
  **not** mounted (media is track 09's; `_synapse/client/*` is client-prefixed, not
  server-to-server).
- **`edu.rs`** (6 tests, new — not in the original plan's enumerated file list, but needed to give
  the required "EDU JSON" fuzz target something real to call). `parse_edu`: structural validation
  only (`edu_type` string required, `content` object-or-absent, size-capped at the same 64 KiB
  PDUs get, canonical-JSON validated via `hs_model::canonical::to_canonical_object` so a malformed
  number is rejected the same way it would be for a PDU). Not wired into a handler yet (`/send`
  is still a seam) — this is deliberately just the parser, matching the instruction that a parser
  touching remote input needs a fuzz target regardless of whether its caller exists yet (see
  `crates/hs-media/fuzz`'s own precedent, cited in the brief, for exactly this pattern).
- **Fuzz targets** (`crates/hs-federation/fuzz/`, five targets, one seed pair each). `pdu_parse`
  (drives `hs_model::Event::parse` directly, reusing track 02's parser, not a new one).
  `edu_parse` (drives `crate::edu::parse_edu`). `xmatrix_header_parse` (drives
  `crate::xmatrix::parse_x_matrix_header` via a fuzzed `HeaderValue`). `well_known_body_parse`
  (drives a newly-extracted `crate::discovery::parse_well_known_body`, now also used by the real
  `HttpWellKnownFetcher` so there is exactly one parsing path, not two that could drift).
  `key_server_response_parse` (drives `RemoteKeyCache::ingest_response`, made `pub` specifically so
  it is fuzzable without a live fetcher). **Confirmed these type-check on the stable toolchain
  installed here** (`cargo check` inside `crates/hs-federation/fuzz`, a separate `[workspace]` per
  the `hs-media/fuzz` template, succeeds) — actually *running* them needs `cargo-fuzz` and a
  nightly toolchain per the brief, neither available in this sandbox, so they have not been
  executed, only type-checked and given one valid + one adversarial seed file each under
  `fuzz/corpus/<target>/`.

## Mounted into `hs serve` (third session)

The transport server is no longer written-but-unserved. `hs serve` mounts it, and it answers from
this server's real data.

- **`crates/hs-cli/src/federation.rs`** (new, integration lead). `RegistryRoomSource` implements
  [`RoomDataSource`] over `hs_room::registry::RoomRegistry` and `hs-user`'s published-room
  directory; `ServerQuerySource` implements `FederationQuerySource` over `hs-auth`'s user/device
  store, `hs-e2e`'s device keys and `hs-room`'s alias keyspace; `ClientKeyFetcher` implements
  `KeyServerFetcher` over the real `FederationClient`, which is what makes inbound verification
  able to fetch a stranger's keys. `build_mount` assembles all of it, and takes the signing key
  from the *same* `HomeserverIdentity` `hs-room` signs events with, so the key this server
  advertises and the key it signs with cannot drift apart.
- **`crates/hs-cli/src/serve.rs`**. Mounts the transport router at `/_matrix/federation/v1` when
  `federation.enabled`, plus `GET /_matrix/key/v2/server` *outside* the `X-Matrix` layer (it is
  the one federation endpoint that must answer an unsigned request -- it is how a remote gets the
  keys it would need to sign one).
- **Two real bugs this surfaced**, both only visible once the router was mounted the way a
  deployment mounts it:
  1. **`xmatrix.rs`: the verifier signed over the wrong URI under a prefix mount.**
     `axum::Router::nest` rewrites `req.uri()` to the path *relative to* the nest prefix before
     inner layers run, so the layer was verifying against `/version` while every real sender signs
     `/_matrix/federation/v1/version`. Every inbound request from every real homeserver would have
     failed verification. Fixed by preferring the `OriginalUri` extension (`signed_uri`).
     `crates/hs-cli/tests/federation_reads.rs` mounts the router under the real prefix precisely
     so this stays caught.
  2. **`read_routes.rs`: `/backfill?v=$a&v=$b` was rejected with `400`.** `axum::extract::Query`
     deserializes through `serde_urlencoded`, which cannot build a sequence from repeated keys, so
     a `Vec<String>` field failed the whole extraction rather than collecting. Now parsed from raw
     pairs.
- **Two spec deviations fixed**: `/3pid/onbind` was registered `POST` where the spec says `PUT`,
  and `/query/profile` and `/query/directory` are now registered as their own paths (the spec
  names them; the generic `/query/{queryType}` still works).

### What the adapter deliberately will not answer

- **`/state` and `/state_ids` answer only for the room's newest event**, and return `404` for any
  older one. The room actor holds one flat current-state map and has no state-at-an-event query,
  so the newest event is the only one whose state it can report correctly. Answering a question
  about the past with the present would give a remote state it cannot tell is wrong. Lifting this
  needs `hs-state`'s historical snapshots.
- **`/openid/userinfo` resolves nothing**: no OpenID token is ever issued, because the
  client-server `POST /user/{userId}/openid/request_token` endpoint does not exist yet.
- **`/query/profile` returns an empty profile for a local user that exists**, because no profile
  storage exists anywhere in the workspace yet (there is no client-server `/profile` route
  either). It is `404` only for a user this server does not have.
- Auth chains are walked transitively from stored `auth_events` (bounded at 2,000 events), not
  read from `hs-state`'s chain-cover index, which the room actor does not maintain yet.

### Still unmounted or still wrong (as of the third session; see the fourth session below for
what changed)

- ~~The v2 join/leave/invite paths are registered under v1~~ -- fixed the fourth session, see below.
- ~~`GET /.well-known/matrix/server` is not served.~~ -- served as of this session's manifest (added
  by another track's work landing between sessions; not this track's change, noted here only so
  the "still wrong" list stays accurate).
- ~~`/send` and the join handshakes are still seams.~~ -- `/send`, `make_join` and `send_join` (v1
  and v2) are real as of the fourth session; `send_leave` and `invite` remain seams (out of this
  session's scope) but are now mounted at the *correct* v2 path. See below.

## Fourth session: `/send`, `make_join`/`send_join`, and the v2 mount fix

Scope for this session (`docs/next-steps.md` item 3): close the two federation writes that matter
most so this server can be joined by a remote and can receive its events, and fix the v2 routing
bug first. Every file below is new or changed under `crates/hs-federation/src/` and
`crates/hs-cli/src/federation.rs` / `crates/hs-cli/tests/`.

### 1. The v2 mount bug, fixed

`crates/hs-federation/src/transport/mod.rs` now exports two router functions:

- **`router(state, x_matrix_ctx)`** -- the v1 router: every read/query endpoint, every remaining
  seam, plus the newly-real `/send`, `make_join` and the v1 `send_join` spelling. Unchanged mount
  point (`/_matrix/federation/v1`).
- **`router_v2(state, x_matrix_ctx)`** -- a **new**, separate router for the v2-only spellings:
  the newly-real v2 `send_join`, plus the still-seam v2 `send_leave` and `invite`. Meant to be
  mounted at `/_matrix/federation/v2` -- a genuinely different mount point, not a `/v2/` path
  segment appended to the v1 router. This is what fixes the bug: v1 and v2 `send_join` share the
  *exact same route string* (`/send_join/{roomId}/{eventId}`) and only the mount prefix
  distinguishes them, so they cannot both be registered in one `Builder` (it would be registering
  the same `(method, path)` twice). `crates/hs-federation/src/transport/seams.rs` gained a
  matching `add_routes_v2` for the two seams that still need it.
- Both routers apply the same `X-Matrix` layer the same way (factored into a shared
  `apply_x_matrix_layer` helper); `transport::seams::tests::every_v2_seam_route_responds_not_implemented`
  and `transport::seams::tests::the_real_write_routes_are_not_registered_as_seams_here` guard both
  halves of this (the second one fails the moment `/send`/`make_join`/`send_join` are accidentally
  re-added as seams here, which is exactly the kind of regression this fix could otherwise invite).

**`hs serve` does not mount `router_v2` yet** -- I do not own `crates/hs-cli/src/serve.rs` this
session. See "Wiring the integration lead must add" below for the exact lines. Until that lands,
`docs/status/routes.json`/the coverage dashboard will keep showing only the v1 spellings; that is
this gap, not a regression.

### 2. `PUT /send/{txnId}`: real, with one honest, load-bearing gap

New module `crates/hs-federation/src/inbound.rs`:

- **`verify_pdu(raw, room_version, key_cache)`**: parses the PDU (`hs_model::Event::parse`),
  recomputes and checks its content hash against its declared `hashes.sha256` (an `Event::parse`
  does *not* do this -- confirmed by reading `hs-model`'s own doc comment on `parse`, which says so
  explicitly), then verifies its signature against the **sender's** server (not the transmitting
  `origin` -- a resident server relays other domains' events, so checking against `origin` would be
  wrong) using `hs_federation::keys::RemoteKeyCache::get_valid_at` (the existing "valid at the time
  it was signed" lookup, not `get_current`, which is what makes an old-but-was-valid key still
  verify).
- **`process_transaction(origin, txn_id, body, rooms, sink, key_cache, transactions)`**: the real
  `/send` algorithm -- checks `(origin, txn_id)` against the idempotency cache first (returns the
  cached response unprocessed if found), rejects the whole transaction with `400` if `pdus.len() >
  50` or `edus.len() > 100` (the spec's resource-limits table), otherwise processes every PDU **in
  order**, building the `{"pdus": {"$id": {}|{"error": "..."}}}` result map, then caches the
  response before returning it. EDUs are parsed for structural validity
  (`crate::edu::parse_edu`, already existed) and otherwise ignored -- no EDU handler exists yet.
- **`RoomWriteSink`** (the seam this session had to invent): `accept_verified_event(room_id,
  event_id, event_json) -> Result<WriteOutcome, WriteRejected>`. This is where the wall is -- see
  "The one gap this session could not close" below.
- **`TransactionStore`** (`get`/`put` by `(origin, txn_id)`), with an `InMemoryTransactionStore`.
  Deliberately in-memory only, not `KvBackend`-backed: the realistic threat this defends
  (a network-level retry of a transaction whose response was lost) does not survive a process
  restart anyway on the sending side either, and a `KvDestinationStore`-shaped persistent version is
  a mechanical follow-up, not a design question -- noted under "Next" rather than built this session
  to leave budget for the join handshake.
- Wired into the transport layer at `crates/hs-federation/src/transport/send.rs` (the axum handler:
  extracts the `X-Matrix`-verified origin, parses the body, calls `process_transaction`, maps
  `TransactionError` to `400`).

### 3. `make_join` / `send_join` (v1 and v2)

New module `crates/hs-federation/src/join.rs`, wired at
`crates/hs-federation/src/transport/join.rs`:

- **`make_join` is fully real.** It reads the room's actual current state and actual forward
  extremities (see the two new `RoomDataSource` methods below) and builds a genuine unsigned
  `m.room.member` template: real `room_version` (checked against the requester's `?ver=` list,
  `M_INCOMPATIBLE_ROOM_VERSION` if none match), real `prev_events`/`depth` from the room's actual
  extremity, and real `auth_events` selected via `hs_state::auth::expected_auth_types` against
  current state -- the actual algorithm the spec names, not a re-derivation of it. It also runs
  `hs_state::auth::check_event_auth` as a courtesy pre-check (rejects a hopeless join, e.g. no
  invite in an invite-only room, before handing out a template that `send_join` would refuse
  anyway).
- **`send_join` validates for real**: `crate::inbound::verify_pdu` (signature + content hash) on
  the submitted event, shape checks (it is `m.room.member`, `content.membership == "join"`, its
  `room_id` matches the path, and -- the check that actually matters for security -- its
  `sender`'s server matches the requester that signed the HTTP request, so one server cannot submit
  a join "on behalf of" another server's user), then `hs_state::auth::check_event_auth` against
  real current state. What it does **not** run: `hs_state::auth::check_auth_events_selection` (the
  state-*independent* half of the spec's checks) or the spec's required three-snapshot check
  (implied-by-`auth_events`, before-the-event, current-at-receipt) -- `hs-room`'s own
  `pipeline.rs` documents this exact gap as "still track 06's job" for a *locally* authored event,
  and it is out of scope here too, for the same reason: doing it properly needs the DAG-walking
  machinery this session did not have budget to build on top of the persistence gap below.
- Two new `RoomDataSource` methods (`crates/hs-federation/src/room_source.rs`), needed by both:
  `room_version(room_id) -> Option<String>` and `forward_extremities(room_id) ->
  Result<Vec<(String, i64)>, RoomSourceError>`, plus `state_for_join(room_id) ->
  Result<StateForJoin, RoomSourceError>` (current state + its auth chain, **not** gated by
  `is_visible_to` -- handing a prospective joiner's server what it needs to construct and authorize
  a join is the entire point of the handshake, not a bypass of the membership check that gates
  ordinary reads; the join itself is authorized separately). `hs-cli`'s `RegistryRoomSource`
  implements all three against `hs-room`'s existing public `RoomActor` API
  (`full_state`/`paginate`/`room_version`) -- no `hs-room` change needed for the read side.
- **Only `EventsReferenceFormat::V2IdOnly` room versions are supported** (bare event-ID references
  in `prev_events`/`auth_events` -- room versions 3 through 12). Room versions 1-2
  (`V1WithHash`, `[event_id, {"sha256": ...}]` pairs) are explicitly rejected by `make_join`/
  `send_join` with `UnsupportedRoomVersion`: computing that pair for a forward extremity needs the
  extremity's full event body, which `forward_extremities` does not carry (only `(id, depth)`).
  This server's own default and only-tested room version is 11 (`V2IdOnly`), so this is a narrow,
  named gap, not a silent one.
- **Faster joins (MSC3706/MSC4229, `omit_members`) are explicitly out of scope**, as this session's
  brief said to declare up front. Every response is the full, unabridged state and auth chain;
  `members_omitted` is always `false`.
- v2's response adds `event` (the verified event, echoed back) and `members_omitted: false` on top
  of v1's `state`/`auth_chain`/`origin`.

### The one gap this session could not close: persisting a newly-received event

Both `/send` and `send_join` bottom out at `RoomWriteSink::accept_verified_event`. Every
implementation this session can offer -- `crate::inbound::StaticWriteSink` (this crate's own
tests) and `hs_cli::federation::RegistryWriteSink` (the real one, wrapping `hs-room`'s
`RoomRegistry`) -- can only ever report success for an event **this server already holds** (checked
by event ID against the resident `RoomActor`). For a genuinely new event, both return a distinct,
documented error (`error` contains "cannot yet persist"; `send_join`'s HTTP response carries
`errcode: M_HS_INBOUND_INGESTION_UNSUPPORTED`, `501`) rather than a generic seam response or -- far
worse -- a fake success.

**Why**: `hs-room`'s `RoomActor` (`crates/hs-room/src/actor.rs`) has exactly two ways to add an
event to a room, and both build and sign a **new** event from scratch using this server's own
identity (`send_event`/`send_event_citing`, both calling `pipeline::build_and_authorize`, which
takes a `NewEvent { event_type, state_key, sender, content, redacts }` -- content and metadata
only, never an already-built `Event`). There is no entry point that takes an already-signed,
already-hashed, foreign `hs_model::Event` and persists it as-is. `hs-room`'s own `pipeline.rs` names
this precisely: its doc comment says the general inbound-event check "is still track 06's job --
see `docs/design/04-room-actor-protocol.md`'s `Command::PersistInbound`" -- a command that was
*named* in the design doc but never implemented, by either track. I do not own `crates/hs-room` and
did not edit it. I considered and rejected re-authoring the received event locally (calling
`membership_action`/`send_event` with the remote user as `sender` but signed by this server's own
key): that would silently produce a cryptographically wrong event (signed by the wrong server for
its sender's domain) that every other real homeserver in the room would reject, and would give this
event a *different* ID than the one the joining server has -- exactly the kind of half-built
handshake this project's own conventions call more dangerous than an honest error.

**What `hs-room` needs, concretely, to lift this** (for whoever picks up
`docs/design/04-room-actor-protocol.md`'s `Command::PersistInbound`, likely track 04):
a `RoomActor` entry point along the lines of

```rust
/// Persists an already-verified, already-authorized foreign event (its signature and content hash
/// checked by the caller, e.g. `hs_federation::inbound::verify_pdu`) exactly as received: no
/// re-signing, no `event_id` regeneration. Computes `depth`/forward-extremity bookkeeping from the
/// event's own `prev_events` (which may not be this actor's current extremities -- unlike
/// `send_event`, this does not get to assume convergence) and feeds `hs_state::api::StateStore` the
/// same way `RoomActor::persist` already does for a locally-built event.
pub fn accept_remote_event(&mut self, event: hs_model::Event) -> Result<(), RoomError>;
```

with the caller (this crate, or whoever owns the inbound pipeline) responsible for everything
`hs_state::auth` needs *before* calling it (the three-snapshot check this session also did not
build) and this method responsible only for the mechanical parts `RoomActor::persist` already knows
how to do (intern, write, update the state store, update forward extremities, publish a
`RoomUpdate`) minus the "build and sign a new event" half that does not apply to a foreign one.

### Mutation-tests performed this session

Per this session's instructions, both required guarantees were actually broken (not just reasoned
about) and the tests re-run to confirm they catch it, then reverted:

1. **Signature check disabled**: in `crate::inbound::verify_pdu`, short-circuited to `return
   Ok(event)` immediately after the key lookup, skipping the `signing::verify_object` call
   entirely. Result: `inbound::tests::verify_pdu_rejects_a_tampered_signature_with_hash_intact`
   failed, as did `hs-cli`'s end-to-end `federation_writes::send_rejects_a_pdu_with_a_tampered_signature`
   (a real signed PUT against the real composed router). **Notably, two *existing* tests did
   *not* catch this mutation**: `verify_pdu_rejects_a_tampered_body` (it tampers the *content*,
   which the independent content-hash check catches before signature verification is ever
   reached) and `verify_pdu_rejects_a_signature_from_the_wrong_key` (the wrong-key scenario is
   caught by the key-lookup step, upstream of `verify_object`). This is exactly the kind of gap
   this project's conventions warn about -- a test with the right *name* that is not actually
   exercising the code path it claims to -- so a new test,
   `verify_pdu_rejects_a_tampered_signature_with_hash_intact`, was added specifically to tamper
   *only* the signature bytes with the hash left intact, isolating the check. That test and the
   `hs-cli` integration test are the ones that actually prove this guarantee; the two older ones
   prove different (also real) guarantees and are kept.
2. **Idempotency disabled**: in `crate::inbound::process_transaction`, wrapped the `(origin,
   txn_id)` cache lookup in `if false { ... }`. Result:
   `inbound::tests::replaying_a_transaction_id_does_not_reprocess` failed immediately (its sink
   panics if called a second time for the same transaction, which is exactly what the disabled
   idempotency check let happen).

Both mutations were reverted immediately after confirming the failure; `cargo test -p hs-federation
--lib` is green at 114/114 with both reverted (see "Verification").

## Wiring the integration lead must add

Not done this session (`crates/hs-cli/src/serve.rs` is owned by the integration lead this
session). In `serve.rs`, the existing federation-mounting block

```rust
if let Some((state, x_matrix, own_keys, server_name)) = federation {
    let (federation_router, federation_manifest) =
        hs_federation::transport::router(state, x_matrix);
    // ...
    builder = builder
        .get("/_matrix/key/v2/server", /* ... unchanged ... */)
        .merge_router("/_matrix/federation/v1", federation_router, federation_manifest.routes);
}
```

needs to become (clone `state`/`x_matrix` -- `FederationState` derives `Clone`, `x_matrix` is
already an `Arc` -- so both routers see the same data sources and the same key cache):

```rust
if let Some((state, x_matrix, own_keys, server_name)) = federation {
    let (federation_router, federation_manifest) =
        hs_federation::transport::router(state.clone(), x_matrix.clone());
    let (federation_router_v2, federation_manifest_v2) =
        hs_federation::transport::router_v2(state, x_matrix);
    // ...
    builder = builder
        .get("/_matrix/key/v2/server", /* ... unchanged ... */)
        .merge_router("/_matrix/federation/v1", federation_router, federation_manifest.routes)
        .merge_router("/_matrix/federation/v2", federation_router_v2, federation_manifest_v2.routes);
}
```

That is the only change needed there. `crate::federation::build_mount` and
`crate::federation::manifest_only_mount` (both in `crates/hs-cli/src/federation.rs`, which I do
own) already build a `FederationState` with the new `write_sink`/`transactions` fields populated
for real, so no other `hs-cli` change is needed to pick this up -- `cargo run -p hs-cli --bin hs
routes-manifest` will then list `send_join`/`send_leave`/`invite` under
`/_matrix/federation/v2/...` for the first time.

## Verification

```
cargo fmt -p hs-federation -p hs-cli                                   # applied, no diffs after
cargo clippy -p hs-federation --all-targets -- -D warnings             # clean
cargo clippy -p hs-cli --all-targets --no-deps -- -D warnings          # clean (see note below)
cargo test -p hs-federation --lib                                     # 114 passed, 0 failed
cargo test -p hs-cli --test federation_reads                          # 7 passed, 0 failed
cargo test -p hs-cli --test federation_writes                         # 6 passed, 0 failed (new this session)
cd crates/hs-federation/fuzz && cargo check                            # clean (5 bins type-check, unchanged)
```

Note on the `hs-cli` clippy command: without `--no-deps`, `cargo clippy -p hs-cli` also lints
every path-dependency crate in the workspace as part of the same build, and `hs-push` (not this
track's crate) currently fails `-D warnings` on unrelated `result_large_err` lints. That is
pre-existing and not something this session touched; `--no-deps` scopes the check to `hs-cli`'s
own code, which is clean.

`cargo run -p hs-cli --bin hs routes-manifest` still only lists the v1 federation spellings
(`send_join`/`send_leave`/`invite` under `/_matrix/federation/v1/...`) because `router_v2` is not
mounted in `serve.rs` yet — see "Wiring the integration lead must add" above.

## In progress

Nothing mid-file. The eighth session's work (`crate::sender`, the `send_join` forwarding, the
`hs-cli` feeder and wiring, the admin pending counts) is complete and green; what it deliberately
does not do is listed at the top of "Next" below.

Earlier sessions' note, still accurate: Everything listed under "Done" (second/third session) and above (fourth session)
is a complete, tested unit, except the one named gap (`RoomWriteSink` cannot persist a new event —
see above) which is honestly reported as a gap, not left half-built. The sixth session's own work
(TLS/CA config surface, `verify_pdu`'s redaction fix) is likewise complete and fully green within
this crate; the one thing left genuinely unfinished is outside this crate's ownership -- see item 0
below and `docs/rfcs/0014-event-signing-must-sign-the-redacted-form.md`. **RFC-0014 has since been
applied by track 04** (`crates/hs-room/src/pipeline.rs` now signs the redacted form; confirmed this
(seventh) session by re-running `cargo test -p hs-cli --test federation_writes` -- 8/8 pass, not
4/8). The seventh session's own work (the `client_config`/discovery bug fixes,
`crate::outbound_join`, `hs federation-join-room`, the two-server script) is likewise complete and
fully green within this crate and `hs-cli`; the one thing left genuinely unfinished is, again,
outside this crate's ownership -- see `docs/rfcs/0015-outbound-join-needs-a-room-bootstrap-api.md`,
addressed to track 04.

## Next (for whoever resumes this track)

Superseded from earlier sessions' lists (wiring `hs-federation` into `hs serve`, the real
`RoomDataSource` adapter, `HttpKeyServerFetcher`, the key-server axum handlers) are all done as of
the third and fourth sessions and removed from this list. What remains:

New after the eighth session (the outbound sender exists; these are what it still lacks):

- **Persist the outbound queue and add catch-up.** `crate::sender` is in memory: a restart loses
  everything unaccepted and nothing is resent afterwards. The target is `PLAN.md` 5.2 item 6
  (per-destination queues sharded by destination hash, persisted queue state) with a
  `destination_rooms`-style "last position sent per destination" so an outage is caught up from
  the room's own history rather than from a queue -- which also closes the `Lagged` hole in
  `hs_cli::federation_sender` (a missed update is a lost event today) and makes the sender
  shard-gated on `hs-cluster` ownership explicitly instead of relying on room-actor residency.
- **EDUs.** Nothing outbound: typing, presence, receipts, device-list updates, to-device,
  signing-key updates. Needs its own queue (coalescing rules differ per kind) -- deliberately no
  `enqueue_edu` seam was left, see the module docs.
- **Invites over federation** (`PUT /invite` v1/v2, client role) and the client role of
  `make_leave`/`send_leave`, `make_knock`/`send_knock`: separate handshakes, not `/send`.

-1. **(New, urgent, not this track's crate)** `hs-room` needs a room-bootstrap API so a federated
   join's verified state snapshot can become a real local room, not just a verified-and-discarded
   one. Filed as `docs/rfcs/0015-outbound-join-needs-a-room-bootstrap-api.md`, addressed to track
   04, with the exact shape of the needed entry point. Until this lands, `hs
   federation-join-room`/`crate::outbound_join::join_room` (this session) proves the handshake and
   every signature real and live, and the resident server genuinely persists the join -- but the
   joining server's own user can never sync or post into the room. This is the single largest gap
   standing between this workspace and "two servers, both directions, both send messages."
0. ~~**(not this track's crate)** `crates/hs-room/src/pipeline.rs` signs outgoing events over their
   full, unredacted form instead of the redacted one the spec requires~~ -- **fixed by track 04**
   since the sixth session (confirmed this (seventh) session:
   `cargo test -p hs-cli --test federation_writes` is 8/8, not 4/8). RFC-0014 is closed.
1. ~~**`Command::PersistInbound` on `hs-room`'s `RoomActor`**~~ -- built by track 04 between
   sessions (`RoomActor::accept_remote_event`), consumed by the fourth session
   (`RegistryWriteSink`) and, as of this (fifth) session, actually reachable end to end: the
   backfill loop means an event citing history from before a join can now be resolved, not just
   accepted when its ancestors happen to already be present.
2. **The three-snapshot auth check** for both `/send`'s PDUs and `send_join`'s event
   (implied-by-`auth_events`, before-the-event, current-at-receipt state, per the server-server
   spec) -- this session's `send_join` only checks against *current* state, and `/send`'s PDUs are
   not authorization-checked at all (only signature/hash-verified) since there is nothing to gain
   from authorizing an event this server cannot yet persist either way. Both depend on (1) existing
   first: authorization checking three snapshots of a DAG this server cannot record is a check with
   nowhere to attach its result.
3. **`make_leave`/`send_leave`, `make_knock`/`send_knock`, `invite` v1/v2**: still seams (correctly
   mounted, including at the now-fixed v2 path for `send_leave`/`invite`). Same shape of work as
   `make_join`/`send_join`, and blocked on the same persistence gap for the "send" half of each
   pair; `make_leave`/`make_knock` (the read/template half) could be done independently and would
   follow `make_join`'s pattern closely.
4. **Server ACL enforcement wiring**: `acl.rs`'s `is_allowed` function exists and is tested in
   isolation, but nothing calls it yet -- it needs threading into (a) the inbound accept path
   (natural home: `crate::inbound::process_transaction`, reading the room's `m.room.server_acl` via
   a new `RoomDataSource` method) and (b) `crate::client::FederationClient::send`. Unblocked by this
   session's work (the `RoomDataSource` real adapter exists now) but not done this session --
   budget went to the write paths per this session's explicit priority order.
5. **A `KvBackend`-backed `TransactionStore`**, mirroring `destination_store.rs`'s
   `KvDestinationStore` pattern, if a transaction retry surviving a server restart ever turns out to
   matter in practice (see `crate::inbound`'s module doc for why in-memory was judged sufficient
   this session).
6. **Discovery result caching in `client.rs`** (carried over, unchanged): `client_for` caches the
   pinned `reqwest::Client` per destination but only invalidates it when a *fresh* `resolve()` call
   produces a different `ResolvedServer` -- no proactive TTL-based re-resolution independent of a
   new `send` call. Acceptable for now, noted as a gap.
7. **Backfill only ever asks `origin`** (new this session): `resolve_missing_ancestors` fetches from
   the one server that sent the transaction, never tries a different member of the room if `origin`
   is unreachable or uncooperative. Real deployments often have several servers in a room that could
   answer; this session deliberately scoped to the single-peer case (it is what closes the
   join-then-stall gap named in `docs/next-steps.md`) and left multi-peer fallback as a named gap
   rather than a half-built heuristic for picking among peers this crate has no signal to rank.
8. **`crate::acl::is_allowed` is still not threaded into the backfill path either** -- it inherits
   item 4's gap (nothing calls `is_allowed` anywhere yet), not a new one: once item 4 is done,
   `resolve_missing_ancestors`'s calls to `AncestorFetcher::fetch_backfill` go through the same
   `FederationClient::send` every other outbound call does, so wiring ACL into `FederationClient`
   once covers this path too, with no separate change needed here.

## Blockers

None for this crate's own work -- every deliverable this (seventh) session was asked for is done
and tested inside `hs-federation`/`hs-cli`. Two historical entries, resolved:

- `verify_pdu`'s correctness fix making 4 of `hs-cli`'s 8 `federation_writes` tests fail (sixth
  session) -- **resolved**: track 04 applied RFC-0014's fix since, confirmed 8/8 this session.
- `RoomActor::accept_remote_event` needing to exist on `hs-room` -- resolved between the fourth
  and fifth sessions.

**Current, real blocker for the next milestone ("both directions"), not this session's own work**:
`hs-room` has no room-bootstrap API (`docs/rfcs/0015-outbound-join-needs-a-room-bootstrap-api.md`),
so a federated join this server's own user initiates can be fully verified but never durably
represented locally. Everything up to that boundary is done, tested and proven live; that one gap
is track 04's, not track 06's, to close.

**Environmental, not this track's**: `cargo clippy -p hs-federation --all-targets -- -D warnings`
(without `--no-deps`) currently fails on an unrelated, in-progress `crates/hs-http` change (see
"Seventh session"'s verification section). `hs-federation`'s own code is clean
(`--no-deps` variant passes); this is not a regression this session introduced and not something
this track can fix (`crates/hs-http/**` is out of this session's ownership).

## Interfaces provided

- **`crate::sender::{FederationSender, OutboundPduSink, SenderConfig, BACKOFF_POLL_INTERVAL}`**
  (new this eighth session): the outbound sender. `FederationSender::new(Arc<FederationClient>,
  own_server_name) -> Self` (wrap in `Arc`), `with_config(.., SenderConfig)`,
  `enqueue_pdu(impl IntoIterator<Item = String>, serde_json::Value)`, `pending_pdus() -> usize`,
  `pending_pdus_for(&str) -> usize`, `pending_by_destination() -> Vec<(String, usize)>`,
  `shutdown()`. Any track that has a PDU to distribute (a future `/invite` sender, a bridge that
  needs to fan out) hands it here. `OutboundPduSink` is the one-method trait to take when a
  recording double is wanted.
- **`crate::transport::FederationState::sender: Option<Arc<dyn OutboundPduSink>>`** and
  **`crate::room_source::RoomDataSource::member_servers(&self, room_id) -> Vec<String>`** (new
  this session): every constructor and implementor must supply them.
- **`crate::join::send_join(rooms, sink, key_cache, room_id, event_id, signed_event, origin,
  own_server_name, forward: Option<&dyn OutboundPduSink>)`**: two new trailing parameters.
- **`crate::admin_source::DestinationStoreSource::with_sender(Arc<FederationSender>)`**: makes the
  admin Federation page's `pending_pdu_count` real. **`FederationClient::max_retry_backoff()`**:
  the ceiling the sender shares.
- **`hs_cli::federation_sender::{OutboundFederation, forward_update}`** (`crates/hs-cli`, new
  this session): `OutboundFederation::start(Arc<RoomRegistry<B>>, Arc<FederationSender>,
  OwnedServerName) -> Self`, `sender()`, `stop()`; `forward_update(&RoomRegistry<B>,
  &FederationSender, &ServerName, &RoomUpdate) -> Result<Vec<String>, RoomError>` is the per-update
  step, exposed so a test (or a future catch-up) can drive it one update at a time.
  `hs_cli::federation::FederationMount::sender: Arc<FederationSender>`.
- **`crate::outbound_join::{join_room, RemoteJoinOutcome, OutboundJoinError}`** (new this seventh
  session): the client-role join handshake -- any track that needs "make this server's user join a
  room hosted elsewhere" (a future `/join` wiring, once RFC-0015 lands, or a bridge/appservice that
  needs the same) calls this directly. Returns a fully verified snapshot; persisting it is the
  caller's job once `hs-room` can (RFC-0015).
- **`hs federation-join-room` CLI subcommand** (`crates/hs-cli`, new this session): drives
  `join_room` from a real config file with no storage open. Diagnostic/administrative today; the
  natural first caller once RFC-0015 lands is `hs-room`'s own client-facing `/join` route (not this
  CLI command, which would then become redundant with it -- kept anyway as a lower-level tool for
  debugging a stuck join without a full client).
- **`crate::client::FederationClient`** (tracks 08, 09, 11 per the brief): `new(...)` takes an
  owned server name, a `SigningKeyPair`, a `ClientConfig`, and `Arc<dyn DestinationStore>` /
  `Arc<dyn WellKnownFetcher>` / `Arc<dyn SrvResolver>` / `Arc<dyn AddrResolver>`; `.send(destination,
  method, path, body) -> Result<FederationResponse, ClientError>` is the one call site. Wired into
  `hs-cli` since the third session.
- **`crate::room_source::RoomDataSource`**: the read-only room seam, now with three more methods
  (`room_version`, `forward_extremities`, `state_for_join`) added this session for the join
  handshake. `hs_cli::federation::RegistryRoomSource` is the real implementation over `hs-room`;
  `InMemoryRoomSource`/`FakeRoom` remain available for any track's own tests.
- **`crate::inbound::{verify_pdu, process_transaction, RoomWriteSink, WriteOutcome, WriteRejected,
  TransactionStore, InMemoryTransactionStore, StaticWriteSink}`**: the inbound PDU-verification and
  transaction-envelope primitives. `WriteRejected` gained a structured `missing_ancestors: Vec<String>`
  field this (fifth) session, plus `WriteRejected::other`/`WriteRejected::missing_ancestors`
  constructors -- any other implementation of `RoomWriteSink` should use these rather than
  constructing the struct literal directly, so a future field addition here does not need every
  implementation to change. `process_transaction`'s signature grew two parameters this session
  (`ancestor_fetcher: Option<&dyn crate::backfill::AncestorFetcher>`, `backfill_limits:
  &crate::backfill::BackfillLimits`) -- pass `None` and `&BackfillLimits::default()` to keep the old
  behaviour (report the gap, do not try to close it).
- **`crate::backfill::{AncestorFetcher, AncestorFetchError, BackfillLimits, BackfillGiveUpReason,
  resolve_missing_ancestors}`** (new this session): the backfill resolution loop described above.
  `FederationClient` implements `AncestorFetcher`; anything that wants to trigger a backfill
  resolution outside `process_transaction` (a future explicit "resync this room" admin action,
  say) can call `resolve_missing_ancestors` directly against any `RoomWriteSink`.
- **`crate::client::FederationClient::backfill(destination, room_id, from_event_ids, limit) ->
  Result<Vec<Value>, ClientError>`** (new this session): the outbound `/backfill` client, returning
  raw unverified PDUs -- callers must run each through `crate::inbound::verify_pdu` themselves.
- **`crate::join::{make_join, send_join, JoinTemplate, SendJoinResult, JoinError, RoomWriteSink}`**
  (new this session): the join-handshake logic, usable directly by anything that wants to build or
  validate a join without going through the axum layer (e.g. a future differential test against
  recorded Synapse traffic, per this track's definition of done).
- **`crate::transport::{FederationState, FederationQuerySource, InMemoryQuerySource, router,
  router_v2}`**: the federation router-fragment functions, both mounted in `hs-cli` as of the fourth
  session. `FederationState` gained two more fields this (fifth) session:
  `ancestor_fetcher: Option<Arc<dyn crate::backfill::AncestorFetcher>>` (`None` disables backfill)
  and `backfill_limits: crate::backfill::BackfillLimits`; any other track constructing one directly
  (none do today, per a repo-wide grep) needs to supply both -- `None` and `BackfillLimits::default()`
  reproduce the pre-this-session behaviour exactly.
- **`crate::xmatrix::{sign_request, verify_x_matrix, XMatrixContext}`**: request signing for any
  track that needs to make an authenticated federation call directly (though `FederationClient`
  should normally be preferred), and the verification middleware/context type for whoever wires
  the federation listener into `hs-cli`.
- **`crate::keys::{OwnSigningKeys, RemoteKeyCache, KeyServerFetcher, DynRemoteKeyCache}`**: key
  management for any track that needs to verify a federation signature outside the request path
  (e.g. verifying a signed `m.room.third_party_invite`).
- **`crate::acl::{ServerAcl, is_allowed}`**: the one ACL evaluation function, for whoever wires
  inbound/outbound enforcement (see "Next" item 4).
- **`crate::client::ClientConfig::{custom_root_certificates, trust_os_root_store}`** (new this
  sixth session): the fields any track constructing a `ClientConfig` directly (none do today
  outside `hs-cli`'s `client_config` conversion function) needs to populate to preserve or opt into
  custom-CA/OS-store trust; both default to "off" (`Vec::new()`/`false`) via `ClientConfig::default()`,
  reproducing pre-this-session behaviour exactly for any caller using `..ClientConfig::default()`.
- **`hs-config::FederationConfig::{custom_ca_certificates, trust_os_root_store}`** (new this sixth
  session): the schema fields; see "Sixth session" above for their doc comments and the reasoning
  behind `trust_os_root_store`'s `false` default.

## Interfaces needed

- ~~**Track 04**: the real `RoomDataSource` adapter over `RoomActorHandle`.~~ Built in the third
  session as `hs_cli::federation::RegistryRoomSource`. ~~What it still needs from track 04 is a
  **state-at-an-event** query~~ -- also lifted the third session (`RoomActor::state_at_event`);
  `/state` and `/state_ids` now answer for any event this server holds, not just the newest.
- ~~**Track 04, the real blocker**: a `RoomActor` entry point that accepts an already-verified,
  already-signed foreign event and persists it as-is.~~ Built by track 04 between the fourth and
  fifth sessions (`RoomActor::accept_remote_event`, `docs/design/04-room-actor-protocol.md`'s
  `Command::PersistInbound`). Nothing further needed from track 04 as of this session.
- **Nothing new needed from another track this (fifth) session** -- the backfill loop is entirely
  built from interfaces this crate already owned (`RoomWriteSink`, `RoomDataSource`,
  `FederationClient`) plus one already-existing `hs-room` entry point (`accept_remote_event`).
- **Track 08 (E2EE)**: `/user/keys/claim` and `/user/keys/query` remain mounted as seams pending
  track 08's contract, per the brief's joint-ownership note.
- ~~**hs-cli / whoever owns `hs serve`'s wiring**: needs to call `crate::transport::router`,
  `crate::client::FederationClient::new`, and load `OwnSigningKeys` at startup.~~ Done in the
  third session. **New this session**: `hs serve`'s wiring also needs to mount `router_v2` at
  `/_matrix/federation/v2` -- see "Wiring the integration lead must add" above; this one is not
  done yet.
- **Track 04 (`crates/hs-room/src/pipeline.rs`), urgent**: needs the redact-then-sign-then-copy-back
  fix described in `docs/rfcs/0014-event-signing-must-sign-the-redacted-form.md` -- this server's
  own outbound events are signed over their full, unredacted form, which any real spec-compliant
  remote homeserver's inbound verification (redact-then-check, matching this session's `verify_pdu`
  fix) would reject whenever the event's content is not fully retained by redaction. Not this
  track's crate to fix.
- **`hs-cli` (whoever owns `crates/hs-cli/tests/federation_writes.rs`)**: three test call sites
  (`build_signed_message` and two inline `sign_object` calls -- see the RFC for exact locations)
  need the same mechanical redact-then-sign fix already applied twice in this crate's own tests
  this session. Confirmed (read-only) these three, plus the one real-pipeline-dependent test named
  above, are the only `federation_writes` failures caused by this session's `verify_pdu` fix.
- ~~**`hs-cli`'s `crates/hs-cli/src/federation.rs::client_config`**: to actually honour the new
  `hs-config` fields end-to-end, needs two more lines...~~ **Done this (seventh) session** -- see
  "Seventh session" §1. This is the track's own crate (`crates/hs-cli/**` is in this session's
  ownership, unlike the sixth session that wrote this item), so it was fixed directly rather than
  filed as a request to another track.
- **Track 04, current**: the room-bootstrap API `docs/rfcs/0015-outbound-join-needs-a-room-bootstrap-api.md`
  asks for -- see "Blockers" above.

## Decisions made

New this (eighth) session:

- **In memory first, persistence next.** The brief asked for the sender that makes "a local event
  reaches remote members" true at all; a `KvBackend`-backed, sharded, catch-up-capable queue is a
  design of its own (see "Next"). The module docs say what a restart loses, in the first
  paragraph, so nobody mistakes this for the plan's section 5.2 sender.
- **Retry everything except this server's own policy refusals.** A non-2xx status, a connection
  or discovery failure, and a destination-store backoff are all "later"; only
  `Disabled`/`DomainDenied`/`IpDenied` are "never", and those drop the transaction with an
  `error` log. A permanently 4xx-ing destination therefore retries at the cap (1h) until restart;
  accepted, since the alternative -- guessing which 4xx codes are permanent -- silently loses
  events on the guesses that are wrong.
- **A retry reuses the transaction ID.** Otherwise a receiver that processed the first attempt
  but whose response was lost would apply the same PDUs under two IDs, defeating its own
  idempotency cache.
- **The sender's own backoff has no jitter.** The destination store already jitters the
  connection-level backoff the client records; a second layer of jitter would only make the
  retry tests non-deterministic. Doubling from 1s, capped at the client's `max_retry_backoff`.
- **A destination in backoff is polled every 30s at most** (`BACKOFF_POLL_INTERVAL`), so an
  administrator's reset takes effect promptly. A poll is a store read, not a network call.
- **Per-PDU rejections are final.** The receiver looked at the event; resending gets the same
  answer. Logged at `warn`, with the receiver's reason.
- **Destinations are "joined after the event, plus the target of a leave/ban".** Equivalent to
  Synapse's "hosts in the room at the event's prev_events" for a locally originated event, using
  the `joined_members_after` `hs-room` already provides rather than asking track 04 for a
  `joined_members_before`. Invite targets are excluded until `/invite` exists.
- **Remote senders' events are never re-sent by the feeder**; the one exception the spec makes --
  the resident forwarding a `send_join` it accepted -- is done at `send_join` itself, once, on
  `Stored` only.
- **A lagged update stream is a lost event, and the log says so.** No catch-up exists to make it
  anything else; pretending otherwise (silently continuing) is the one thing the log must not do.
- **Not shard-gated**, recorded rather than half-built: room-actor residency already makes each
  local event's update reach one replica; a sharded sender belongs with the persisted queue.
- **`hs-testkit` became an `hs-cli` dev-dependency** (path, no workspace root change) so the new
  `hs-cli` test can use `FakeFederationPeer` like every `hs-federation` test does, reversing the
  fifth session's "did not add it" note.
- **`hs_cli::federation_sender::forward_update` is public** so the integration test can assert
  each event's destinations directly rather than infer them from what a fake peer eventually did
  or did not receive ("nothing was sent" is otherwise a claim about a timeout).

New this (seventh) session:

- **The two-server script uses IP-literal server names (`127.0.0.1:8448`/`127.0.0.1:8449`), not
  hostnames.** Discovered live: this crate's `AddrResolver` (`hickory-resolver`) does not consult
  `/etc/hosts`, so a hostname like `"localhost"` resolved through a real, search-domain-configured
  `/etc/resolv.conf` is not guaranteed to reach `127.0.0.1` -- confirmed on this machine's own
  network. IP literals bypass discovery entirely (`crate::discovery` step 1) and are exactly as
  spec-valid a `server_name` as a hostname, so they are the right choice for a script that must be
  reproducible on any machine's network configuration, not a workaround.
- **A malformed-CA-file entry is logged and skipped, not a fatal boot error** (`client_config`'s
  fix, §1): matches `FederationClient::new`'s own existing tolerance for a CA entry that reads but
  fails to *parse*; a path that cannot even be *read* (typo, permissions) should be equally visible
  in the log rather than crashing a server that might otherwise boot and serve local users fine.
- **`hs federation-join-room` opens no storage.** Everything it needs (server name, signing key,
  federation policy) lives in the config file alone; nothing it produces can be durably persisted
  yet regardless (RFC-0015), so there is no room store for it to open. Uses
  `InMemoryDestinationStore` rather than `KvDestinationStore` for the same reason a one-shot
  command has no backoff state worth persisting across runs.
- **`join_room` fails closed on the first unverifiable event in `state`/`auth_chain`**, rather than
  collecting partial results: a resident server that hands back even one event that fails content-
  hash or signature verification is not a resident worth trusting further for this join, matching
  `verify_pdu`'s own all-or-nothing contract for a single PDU.

Previously, sixth session:

- **`trust_os_root_store` defaults to `false`.** Full reasoning in the field's own doc comment
  (`hs-config::FederationConfig::trust_os_root_store`) and in "Sixth session" §2 above; recorded
  here per this session's brief, which asked specifically for this decision to be made and
  justified. Short version: federation authenticates servers that never agreed on a shared root of
  trust ahead of time, so silently trusting whatever the OS happens to trust (which anyone with
  root can broaden, for reasons unrelated to this server) is a quiet regression as an *unconditional
  default*; `custom_ca_certificates` is the explicit, narrow alternative, and the operator chooses
  either per deployment.
- **`ClientConfig::custom_root_certificates` takes raw PEM bytes, not file paths.** Considered
  taking `Vec<String>` (paths) directly, matching `hs-config`'s own field, and rejected it: this
  crate does not otherwise do filesystem I/O anywhere (`OwnSigningKeys::load_or_generate` is the one
  exception, and that is a different, already-established seam), and keeping `ClientConfig` free of
  I/O let this session's own TLS test hand it certificate bytes straight from `rcgen` with no
  filesystem involved at all. File reading is one `std::fs::read` per configured path at the
  `hs-cli` wiring site, which already does config-loading I/O.
- **A parse failure in `custom_root_certificates` is logged and skipped, not fatal.** Considered
  making `FederationClient::new` fallible (returning `Result`) so a malformed CA file could be a
  hard startup error, and rejected it: every other construction path in this crate today is
  infallible (`FederationClient::new` returns `Self`, not `Result<Self, _>`), and changing that
  signature would touch every call site (`hs-cli`, every test in this crate) for a case that is
  already loud (`tracing::error!` naming the exact index that failed) without also making a
  single malformed file a hard crash for a server that might otherwise start up fine on its public
  roots alone.
- **The `verify_pdu` redaction fix was kept despite the collateral `hs-cli` test failures it
  causes.** See "Sixth session" §5-6 and the RFC. Considered reverting to avoid the 4 failing
  `hs-cli` tests and rejected it: the fix is objectively spec-correct (quoted directly from
  `refs/matrix-spec/content/server-server-api.md`), it is what actually resolves the named target
  bug (`send_join` rejecting a real, correctly-signed join), and `cargo test -p hs-federation` (this
  crate's own, complete responsibility) is fully green with it in place. The failures it exposes in
  `hs-cli` are in code this session does not own, are precisely diagnosed, and are documented with
  an exact fix rather than silently left for someone else to rediscover.
- **The companion bug in `hs-room/src/pipeline.rs` was documented as an RFC rather than left as a
  one-line status-file mention.** Considered just noting "hs-room has the same bug" in this file's
  "Next" list and decided the severity (every outbound event with non-trivial content is
  mis-signed, for every remote federation partner) warranted the fuller treatment `docs/rfcs/`
  gives -- an exact reproduction, an exact patch shape with the current file's variable names
  confirmed by reading it, and an exact list of the affected `hs-cli` test call sites, so track 04
  does not have to re-derive any of it before applying the fix.

New this (fifth) session:

- **The backfill target is always `origin`** (the server that sent the transaction reporting the
  gap), never a different member of the room. Considered trying every joined server this crate
  knows about and rejected it for this session: this crate has no signal to rank peers by
  reliability, and falling back through an unranked list on every failure risks turning one
  hostile or slow peer into several round-trips' worth of wasted work for a gap that peer alone
  cannot close either. `origin` is also the server most likely to actually have the history (it is
  the one that just cited it), so it is the correct first (and, this session, only) choice. Recorded
  as "Next" item 7, not treated as a design flaw.
- **`/backfill` was chosen over `/get_missing_events` as the fetch primitive**, per the brief's
  explicit permission to pick "as the spec and the situation decide". `/backfill` takes exactly
  "the IDs I'm missing" and a limit and walks backwards from them, which is precisely this
  session's situation (an event named specific ancestor IDs this server does not hold);
  `/get_missing_events` additionally requires communicating this server's own `earliest_events`
  frontier to the peer, which needs a concept of "this room's backward frontier from this server's
  point of view" that `RoomDataSource` does not expose today and that this session judged
  unnecessary complexity for the case actually being solved. `/get_missing_events`'s outbound
  client was not built this session; noted as a possible future addition, not a gap in what this
  session was asked to close.
- **`WriteRejected` grew a structured field instead of a new error type.** Considered a
  `Result<WriteOutcome, WriteError>` where `WriteError` is an enum with a `MissingAncestors(Vec
  <String>)` variant, and rejected it: every existing caller (`crate::join::send_join`,
  `crate::inbound::process_transaction`, `hs_cli::federation::RegistryWriteSink`, this crate's own
  tests) already matches on `WriteRejected { error, .. }` as a struct; changing the error's *type*
  would touch every one of those call sites for a change that is really just "add one more field
  most callers will ignore". The two new constructors (`WriteRejected::other`,
  `WriteRejected::missing_ancestors`) keep every construction site from having to remember to set
  the new field explicitly, and are the only sites this session changed.
- **The resolution loop tracks a `pending` worklist across rounds, not just within one**, per the
  real multi-hop bug this session's own test caught (see "Fifth session" above). This is the one
  piece of this session's design that changed shape *after* being written and tested, not before --
  recorded here so the reasoning is not lost: an event fetched in round N that cannot yet be
  persisted must still be attempted again in round N+1 once whatever blocked it lands, not
  discarded the moment its first attempt fails.
- **A hard-rejected fetched event (bad auth, malformed, ...) is silently dropped, not retried and
  not reported as part of the eventual give-up reason.** Considered surfacing it (e.g. a
  `BackfillGiveUpReason::AncestorRejected(id, reason)` variant) and decided the extra type
  complexity was not worth it: a hard rejection during backfill is not actually a *backfill*
  failure -- it means a fetched event failed the same authorization check any inbound event would
  fail, which is already a defended, tested code path (`crate::inbound::verify_pdu`,
  `RoomActor::accept_remote_event`'s own authorization). The caller's eventual retry of the
  original event will report whatever ancestor is *still* actually missing (if the hard-rejected
  event was itself required), which is the information that actually matters to whoever reads the
  `/send` response.

New this (fourth) session:

- **`hs-state` was added as a direct dependency of `hs-federation`** (`Cargo.toml`, path
  dependency, not a `[workspace.dependencies]` entry -- matches how `hs-model`/`hs-kv`/etc. are
  already declared here). `crate::join` calls `hs_state::auth::{check_event_auth,
  expected_auth_types}` directly rather than re-deriving the join-rules/power-level/membership auth
  rules a second time, per this session's explicit instruction ("not a second copy of the auth
  rules"). No cycle risk: `hs-state` depends on nothing above it in the crate graph.
- **`RoomWriteSink` is a new, separate seam from `RoomDataSource`**, not a fourth read method
  bolted onto the existing (deliberately read-only, per its own doc comment) trait. `/send` and
  `send_join` share exactly one write seam rather than each inventing their own, which is what
  makes the "one honest gap" in this session's write-up a single, named thing instead of two.
- **`send_join`'s persistence-gap error is a `501` with a distinct errcode
  (`M_HS_INBOUND_INGESTION_UNSUPPORTED`)**, not a `200` with a state/auth_chain response that
  claims success. Considered and rejected: returning `200` would tell a real remote server its join
  succeeded when this server recorded nothing, which is a worse failure mode than an honest error --
  the remote would proceed to sync and participate in a room it believes it joined while this
  server's data never reflects the membership.
- **Re-authoring a received foreign event with this server's own identity, to make persistence
  "work", was considered and rejected** (see "The one gap this session could not close" above for
  the full reasoning): it would produce a cryptographically wrong event (signed by the wrong
  server for its sender's domain) with a different ID than what the joining server holds, which
  every other real homeserver in the room would reject on sight. An honest, typed error that
  proves everything *up to* persistence is real is safer than a handshake that looks complete and
  is quietly wrong.
- **`/send`'s PDUs are not authorization-checked against room state this session, only
  signature/hash-verified.** Considered doing a single-snapshot `check_event_auth` (as `send_join`
  does) and rejected it for `/send` specifically: since no PDU can be persisted regardless of the
  outcome (see the gap above), an auth check here would only ever change *which* error message an
  operator sees, at the cost of building `FlatState` for every PDU in every transaction. Revisit
  once (1) under "Next" exists and an accepted PDU has somewhere to go.
- **Only `EventsReferenceFormat::V2IdOnly` room versions are supported by the join handshake**
  (documented above under item 3) -- a deliberate, narrow scope decision given this server's
  default and only-tested room version (11) is already in that family, rather than building
  `V1WithHash` reference-pair support for forward extremities that would need a `RoomDataSource`
  signature change (fetching full event bodies, not just `(id, depth)`) to support two room
  versions this server has never been run against.

From the third session (the second session's decisions, still valid, are below under "Decisions
made (first session, unchanged)"):

- **`ClientConfig` is decoupled from `hs_config::FederationConfig`** (its own struct in
  `client.rs`, with `DomainPolicy`/`IpPolicy` built from plain `Vec<String>` CIDR/domain lists via
  `from_cidrs`/`new`, not from `hs-config`'s type directly). Keeps `client.rs` testable without a
  dependency on `hs-config`'s validation/schema machinery and keeps the conversion (a handful of
  lines) at the wiring site where `hs-config` is already in scope, rather than making this crate's
  core client logic depend on another crate's config schema shape. `hs-config` remains a
  `Cargo.toml` dependency of this crate (used nowhere yet — it was speculatively added in the
  first session's plan; still fine to keep, since `ServerConfig`/`FederationConfig` will be read at
  the `hs-cli` wiring point that lives logically alongside this crate's own types, even if not
  literally inside `client.rs`).
- **`ClientConfig::scheme` is a test-only seam, `"https"` by production default, never settable by
  any config loader.** Added specifically so `client.rs`'s own tests could exercise real discovery
  + signing + pooling + concurrency + backoff against `hs_testkit::FakeFederationPeer` over a real
  loopback socket, without also having to stand up a self-signed TLS certificate and trust chain
  (which would mostly be testing `reqwest`'s already-tested TLS stack, not this crate's logic).
  Documented inline on the field itself; not exposed through `hs-config`.
- **The `X-Matrix` middleware re-parses the `Authorization` header per read-route handler**
  (`transport::read_routes::requesting_server`) rather than threading the already-verified
  `origin` through axum request extensions from `verify_x_matrix`. This is a deliberate
  simplification for this pass, not an oversight: parsing is infallible at that point (the layer
  already proved the header is well-formed enough to have signed correctly) and cheap, and it
  avoids introducing an extension-passing contract between two independently-testable modules
  (`xmatrix` and `transport`) before there's a second consumer that would justify it. Revisit if a
  profiler ever cares, or if a second handler module needs the same value and the duplication
  starts to feel real rather than theoretical.
- **`RoomDataSource::get_event_by_id`** (room-less event lookup) was added beyond what the first
  session's plan enumerated for the trait, because `/event/{eventId}`'s actual spec path shape has
  no room ID in it — a server has to know which room an event belongs to before it can apply the
  membership check. `InMemoryRoomSource`'s implementation linearly scans every room, which is fine
  for a test fake and would not be for a real adapter (track 04's adapter should back this with an
  actual event-ID index, not a scan).
- **`edu.rs` was added, beyond the original plan's file list**, purely to give the required "EDU
  JSON" fuzz target a real parser to call rather than fuzzing raw `serde_json::Value` parsing with
  no structural validation at all. It intentionally does not interpret any `edu_type`'s `content`
  schema — that's real work for whichever session first implements EDU handling inside `/send`.
- **`RemoteKeyCache::ingest_response` and `discovery::parse_well_known_body` were made `pub`**
  (the former was previously private, the latter was extracted from being inline in
  `HttpWellKnownFetcher::fetch`) specifically so both are independently fuzzable without needing a
  live network fetcher to drive them. This also had the side benefit of eliminating a
  near-duplicate parsing path in `HttpWellKnownFetcher::fetch`, which now calls the same function
  the fuzz target does.
- **`transport::router` composes both sub-routers into one `Builder` before calling `.build()`
  once**, rather than building two separate `axum::Router`s and merging them via
  `Builder::merge_router`. Not a style preference: `Builder::merge_router` nests the incoming
  router under a prefix via `axum::Router::nest`, and axum panics ("Nesting at the root is no
  longer supported") when that prefix is `""` — which it must be here, since `read_routes` and
  `seams` both register spec-relative paths (`/version`, `/send/{txnId}`, ...) that are meant to
  live at the same level, not under a sub-prefix. Discovered by the test suite immediately (both
  `transport::tests` cases panicked at construction), not by production use. `read_routes` and
  `seams` therefore expose `pub(super) fn add_routes(builder) -> builder` (composable into a
  shared `Builder`) rather than `pub(super) fn router() -> (Router, Vec<Route>)`.

## Decisions made (first session, unchanged)

- **`RoomDataSource` is a trait owned by `hs-federation`, not a dependency on `hs-room`** — see
  threat model section 5. Confirmed correct by this session's own experience: `InMemoryRoomSource`
  made every `transport::read_routes` handler fully testable today, with the real adapter still
  entirely someone else's future work.
- **No federation route is exposed before `X-Matrix` verification wraps the whole router** — now
  enforced by an actual test (`transport::tests::every_route_is_behind_the_x_matrix_layer`), not
  just a stated intention.
- **Join/leave/knock/invite handshakes and `/send` ship as seams this pass** — done exactly as
  specified: signature verification (via the shared layer), a shared typed 501, nothing else.
- **`hickory-resolver` is the DNS resolver** — added and in use (`discovery::HickoryResolver`).
- **HTTP/1.1-only to federation peers** — committed in code (`client.rs`'s `client_for`:
  `.http1_only()` on every outbound `reqwest::Client`).

## Reuse considered (decision 0007)

Unchanged from the first session's analysis (`hickory-resolver`, `reqwest`, `hs-model`'s
canonical/signing/hashing, `ruma-federation-api` considered-but-not-adopted-for-handlers), plus:

- **`ipnet`** (new this session): adopted for CIDR parsing/containment
  (`client::IpPolicy`) rather than hand-rolling prefix-length arithmetic over `IpAddr`. Already
  present transitively via `hickory-net`; promoting it to a direct dependency costs nothing new in
  the dependency tree and avoids a bug-prone reimplementation of CIDR containment (off-by-one
  errors in prefix-length masking are a classic source of exactly the kind of SSRF-adjacent bug
  this check exists to prevent).
- **`rcgen`** (dev-dependency only, unused in the end): added anticipating a real-TLS test harness
  for `client.rs`'s integration tests, then not used — `ClientConfig::scheme`'s plaintext-test seam
  turned out to test this crate's own logic more directly without exercising `reqwest`'s TLS stack
  (someone else's already-tested code). Left in `Cargo.toml` as a dev-dependency in case a future
  session wants a real-TLS test after all; flagged here so it isn't mistaken for dead weight
  without an explanation.

## Shared dependencies added

This (eighth) session: **no root `Cargo.toml` change.** `crates/hs-cli/Cargo.toml` gained
`hs-testkit = { path = "../hs-testkit" }` under `[dev-dependencies]` (already a workspace crate,
already a dev-dependency of `hs-federation`; no cycle -- `hs-testkit` depends on no `hs-cli`).
`crates/hs-federation/Cargo.toml` is unchanged: `crate::sender` is built from `tokio`,
`serde_json` and `tracing`, all already depended on.

This (seventh) session: **none.** No `Cargo.toml` in either owned crate (`hs-federation`,
`hs-cli`) changed; `crate::outbound_join` and `hs federation-join-room` are built entirely from
types and crates both already depended on.

Sixth session: **no new `[workspace.dependencies]` root-`Cargo.toml` entries** -- every
crate this session's `hs-federation/Cargo.toml` change touches (`reqwest`'s extra feature;
`rustls`, `tokio-rustls`, `rustls-pki-types`, `hyper`, `hyper-util`, `http-body-util` as new
dev-dependencies) was already a workspace-level dependency used by some other crate, so nothing
needed fetching and no root `Cargo.toml` edit was needed or made. The one change worth flagging
explicitly, since it does grow this crate's own compiled dependency tree even though it touches no
shared workspace entry: `crates/hs-federation/Cargo.toml`'s `reqwest` line gained the
`rustls-tls-native-roots` feature (on top of the workspace's existing `rustls-tls`), which pulls in
`rustls-native-certs` and its platform-specific dependencies (`security-framework` on macOS,
`schannel` on Windows) as compiled code for this crate specifically -- see "Sixth session" §2 for
why (it is what makes `trust_os_root_store` a real, working toggle rather than a documented no-op).

None this (fifth) session -- the backfill loop is built entirely from crates already depended on
(`tokio` for `time::timeout`, `async-trait`, `ruma`, `serde_json`), and `hs-cli`'s two new
integration tests use only `axum`/`tokio` (already plain, non-dev dependencies of `hs-cli`) rather
than adding `hs-testkit` as a dev-dependency there (see "Decisions made" -- not actually a decision
this session made explicitly, but worth noting: `hs-testkit::fake_federation::FakeFederationPeer`
would have been a natural fit for the two new `hs-cli` tests' "remote server" double, and *is* used
for `crate::client`'s own new unit test in this crate, but `hs-cli/Cargo.toml` is not a file this
session owns, so the `hs-cli` tests build their own minimal axum catch-all instead).

- **`hickory-resolver`** (0.26, workspace): added the second session. Features: `tokio` (brings
  `system-config` along via its own defaults).
- **`ipnet`** (2, workspace): added the second session, for `client::IpPolicy`'s CIDR containment
  checks.
- **`hs-state`** (path dependency, this crate's own `Cargo.toml`, not a `[workspace.dependencies]`
  entry): added this (fourth) session, so `crate::join` can call the real
  `hs_state::auth::{check_event_auth, expected_auth_types}` rather than re-deriving the auth rules.
  See "Decisions made" above.

The workspace-level entries (`hickory-resolver`, `ipnet`) are noted with "Added by track 06"
attribution comments in the root `Cargo.toml`; `hs-state` needed no root `Cargo.toml` change since
internal crate-to-crate path dependencies are declared directly in each crate's own manifest (the
same way this crate already depends on `hs-model`/`hs-kv`/etc.).
