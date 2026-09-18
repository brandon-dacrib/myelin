# 06. Federation

Wave 1.5, starts week 4. Discovery, keys, signing, the transport server and the client start immediately; inbound event processing waits for the room protocol at week 8.

**Expert profile.** Matrix federation (server-server API, joins, backfill, state fetching, EDUs), security mindset for hostile input, DNS and TLS, cryptography.

**Mission.** Federate correctly with every room version on the network, safely under adversarial traffic, with sender shards that survive failover. See `PLAN.md` sections 5.2, 5.5 and 8.1 item 4.

**Owns.** `hs-federation`: server discovery (`.well-known`, SRV, caching rules), key server and notary, `X-Matrix` signing and verification, the 31 transport routes at parity, the federation client (pooling, retries, backoff, per-destination limits), inbound `/send` dispatch to room and user owners, PDU and EDU validation, missing-event and backfill fetching, joins (`make_join`, `send_join` v2 with `omit_members` for faster joins, restricted joins, knocks, leaves, invites v1 and v2, third-party invites), sender shards with batching and `destination_rooms`-style catch-up, EDUs (presence, typing, receipts, device lists, to-device, signing-key updates), server ACLs both directions, `/publicRooms`, `/hierarchy`, `/timestamp_to_event`, `/openid/userinfo`, `/user/devices` and `/user/keys/*` with 08, remote media proxying for 09, policy servers (MSC4284), MSC4242 serving and receiving with 02, MSC4512 with 11.

**Provides.** The federation client API for 08, 09 and 11; inbound event delivery into 04's actor protocol.

**Consumes.** 02 (`hs-state`, model), 04 (room protocol), 03 (sender shards, ownership), 07 (server keys, config), 01.

**Day-one work (week 4).** Discovery with the spec's resolution test cases; key fetching and caching; request signing; transport server skeleton with `/version` and the key endpoints; the client with `hickory-resolver`; fake peers in `hs-testkit` with 14; a written threat model.

**Phase 0 deliverables (behind the `phase0` flag).** The above; fuzz targets scaffolded for PDUs, EDUs, `X-Matrix` headers and `.well-known` bodies.

**Phase 1 and 2 deliverables.** Everything in "owns"; faster joins with resumption; sender catch-up semantics; per-origin concurrency limits; federation allow and deny lists; metrics; Complement and Sytest federation suites; MSC4242 experimental; preparation for the external security review.

**Definition of done.** Complement federation and Sytest federation suites at Synapse's pass level; fuzzers running continuously; joining a 100k-member room within the section 13 target; differential tests of transaction processing against recorded Synapse traffic; review checklist complete.

**References.** Spec server-server API; `refs/synapse/synapse/federation/`, `refs/synapse/synapse/handlers/federation.py` and `federation_event.py`, `refs/synapse/synapse/crypto/keyring.py`, `refs/synapse/synapse/http/federation/` (behavior, AGPL, read only); `refs/ruma/crates/ruma-federation-api`; `refs/palpo/crates/server/src/federation/`; `refs/conduit/conduit/src/api/server_server.rs`; `refs/mautrix-go/federation/` and `mediaproxy/` for how a bridge acts as a tiny federation server.

**Open questions to settle first.** HTTP/1.1 only to peers (recommended) or HTTP/2 where offered; signing-verification cache shape; partial-state resumption semantics; how sender shards persist queue state per backend; backfill limits.

**Risks.** This is the security surface of the product: unbounded state in `send_join`, signature and hash confusion between room versions, denial of service through backfill. Nothing here ships without fuzzing and the external review.
