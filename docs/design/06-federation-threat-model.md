# 06. Federation threat model

Status: draft, written before any `hs-federation` code, per this track's brief. Owner: track 06.
This is the specification the rest of the track builds against: every defence named here should
map to a concrete check in `hs-federation`, and every check in `hs-federation` should trace back
to a line here. When the two drift, this document loses.

## 0. Framing

Every byte this crate reads over the network — a `.well-known` body, a key-server response, an
`X-Matrix` header, a PDU, an EDU, a `/send_join` state snapshot — originates from a party we do not
control and must assume is either compromised or actively hostile. This is not a homeserver talking
to a trusted backend; it is a homeserver talking to the open federation, where "the other server" is
frequently a stranger's box running unknown code. The working assumption throughout is: **a remote
server can send anything that is syntactically deliverable over HTTPS**, bounded only by TCP/TLS
framing, not by good faith.

Two classes of hostile actor matter differently:

- **An unauthenticated remote peer** hitting our federation listener before any `X-Matrix` signature
  has been checked. Everything in section 2 through the request-signing layer must defend against
  this actor with zero trust in anything the request claims about itself (headers, body shape,
  declared origin).
- **An authenticated-but-malicious remote server** — one that holds a valid, currently-published
  signing key for some `server_name` and is willing to use it to attack us. Signature verification
  proves *who sent this*, not that what they sent is safe or true. Most of this document is about
  this second actor, because passing signature verification is necessary but nowhere near
  sufficient for trusting content.

## 1. Assets to protect

- **Local room state and history** — must not be corrupted, forked incorrectly, or have events
  attributed to servers/users that did not send them.
- **This server's own signing key** and the integrity of what it signs — must never sign anything
  we did not mean to assert.
- **Process resources** (CPU, memory, file descriptors, disk, outbound connection slots) — must not
  be exhaustible by one remote peer or by a burst from many.
- **Local users' data reachable through federation** (profiles, device lists, `/openid/userinfo`
  identity, room membership) — must only be disclosed to servers actually entitled to see it.
- **The outbound path to other servers** — must not be usable to attack a third party (SSRF via a
  malicious `.well-known`/SRV redirect pointing at internal infrastructure, or via a
  `send_join`/`send_leave` handshake target).

## 2. Per-endpoint / per-mechanism threat and defence catalogue

### 2.1 Server discovery (`.well-known`, SRV, direct)

**What a hostile actor controls:** the operator of the server name we are trying to reach controls
its `.well-known/matrix/server` body, its DNS zone (SRV and A/AAAA records), and the TLS certificate
served at whatever address discovery resolves to. A hostile actor who does *not* control the target
server name but sits on-path (or has poisoned a resolver) additionally controls DNS answers and can
attempt to redirect connections.

**Threats:**
- SSRF: a malicious `.well-known` body or SRV/A record pointing at `127.0.0.1`, a cloud metadata
  address (`169.254.169.254`), an internal service, or a port we should never originate a connection
  to, tricking us into treating an internal service as if it were the named homeserver and sending it
  attacker-shaped requests (with our own signature attached, which is itself a further hazard — see
  "signed SSRF" below).
- Resolution loops / amplification: a `.well-known` response delegating to a name that delegates back,
  or an oversized/slow response designed to hold a connection or an async task open.
- Cache poisoning: crafting `Cache-Control` or response bodies to make us cache a bad delegation far
  longer than intended, or to make us treat a transient failure as permanent (denial of service
  against the delegator) or a permanent failure as transient (repeated wasted lookups).
- Malformed bodies designed to crash or hang the JSON/HTTP parser (oversized body, deeply nested JSON,
  non-UTF-8, chunked-transfer abuse).

**Defences:**
- `ip_range_blocklist`/`ip_range_allowlist` (`hs-config::FederationConfig`, already landed) is
  enforced on every address we are about to *connect to*, not just ones we resolved via
  `.well-known` — literal IPs given directly as a server name go through the same filter. This closes
  the SSRF path regardless of which resolution step produced the address.
- "Signed SSRF": because request signing happens *after* the destination is resolved and connected
  to, a successful SSRF bypass would carry a validly-signed `X-Matrix` header pointed at an internal
  target. The IP filter above is what prevents this, not the signature (the signature was never meant
  to be a defence against this — it authenticates the request's claimed origin/destination pair to
  the party receiving it, not the safety of who we're sending it to).
- Well-known body size cap (documented limit in section 3) and a strict, single-level parse — a
  delegated `m.server` value is used as a plain string for the next resolution step, never
  interpreted as HTML/JS or re-fetched recursively as another `.well-known` document (the spec's own
  algorithm does not recurse `.well-known` fetches; only one fetch per resolution).
  `.well-known` redirects (HTTP 3xx) are **not followed** — the spec's algorithm is a single GET, and
  following redirects would let a `.well-known` host point discovery at an arbitrary origin via HTTP
  redirect rather than the documented JSON field, which is a second, undocumented SSRF surface with
  none of the JSON-shape defences above.
- Fetch timeout (section 3) bounds slow-loris-style stalls.
- Caching follows the spec's rules (success TTL from `Cache-Control` clamped to a sane range;
  failure cached briefly) — clamping (not blind trust of an attacker-supplied `max-age`) is what
  prevents cache-poisoning-driven unbounded staleness in either direction.
- SRV and A/AAAA lookups go through `hickory-resolver` with the process's normal resolver
  configuration; we do not implement our own DNS parser, so DNS-parser-level memory-safety bugs are
  not this crate's attack surface (reuse, not reimplementation, per decision 0007).

### 2.2 The key server and notary (`/_matrix/key/v2/server`, `/_matrix/key/v2/query*`, our client
fetching remote keys)

**What a hostile actor controls:** the body of any key-server response we fetch (from the server the
key claims to belong to, or from a notary we asked), including deliberately-wrong `verify_keys`,
`old_verify_keys`, `valid_until_ts`, and the self-signature over all of it.

**Threats:**
- A remote server publishing a key response that is *not* validly self-signed, or self-signed by a
  key it does not also list — accepting either would let an attacker plant an arbitrary "verify key"
  for a server name it does not control the private key for, defeating request/event signature
  verification entirely.
- A `valid_until_ts` far in the future to make a compromised key un-revocable in our cache, or far in
  the past/absent to force us to refetch on every request (a self-inflicted denial-of-service lever
  against *us*, and a resource-exhaustion lever if the remote makes us refetch expensively).
- Replaying an old, previously-valid key-server response after the real server has rotated away from
  a compromised key (a downgrade/rollback attack) — relevant because notary responses are meant to be
  cacheable and re-servable by third parties.
- A notary asked to vouch for a server it has never itself verified, or a notary response that isn't
  itself signed by the notary (should be rejected as not meeting the "signed by the notary" contract).
- Oversized or pathological JSON (huge `old_verify_keys` maps, deeply nested `signatures`).

**Defences:**
- Every key-server response is verified self-signed (the claimed server signed its own key list with
  a key present in that same list) before any key in it is trusted, using `hs-model`'s existing
  `signing::verify_object`/`verifying_key_from_base64` (reused, not reimplemented).
- `valid_until_ts` bounds trust duration; a response is only used for *current* signature verification
  while `now < valid_until_ts`. Expired responses are refetched, not silently extended.
- `old_verify_keys` are accepted only for verifying signatures, never for verifying that a key is
  still *current* (never advertised as a valid signing key going forward), which forecloses the
  downgrade path where an attacker replays an old-key response to keep using a rotated-away key as if
  it were live.
- We do not blindly trust a notary's re-signed response as equivalent to a direct fetch: the notary's
  own signature is checked (proves the notary vouches for it), but the *content* still has to satisfy
  the same self-signature-by-the-origin-server check above — a notary cannot mint a key for a server it
  doesn't control, it can only forward/cache what that server already published.
- Response size cap and parse depth bound (section 3).

### 2.3 Request authentication (`X-Matrix`, both directions)

**What a hostile actor controls:** every header and the entire body of any request sent to our
federation listener, and (as the client) every header/body of any response a destination sends back
to us.

**Threats:**
- Missing, malformed, or multiply-valued `Authorization` headers designed to confuse a naive parser
  into either accepting an unsigned request or crashing.
- A signature computed over the wrong bytes (wrong method/URI/destination/content) being accepted
  because our canonicalization of "what was signed" doesn't match what the sender actually signed —
  this class of bug is exactly how signature schemes get bypassed without breaking cryptography at
  all.
- Replaying a previously-valid, correctly-signed request (Matrix's request signing has no built-in
  nonce/timestamp binding at the HTTP layer beyond what the body itself carries) — accepted as a
  known, spec-level limitation, not something `hs-federation` can unilaterally fix; downstream
  idempotency (transaction IDs, event IDs) is the actual defence and is a Phase 1/2 concern for the
  endpoints that need it (`/send`).
- A request claiming an `origin` whose key we have never fetched, forcing an unbounded,
  attacker-triggered burst of outbound key-fetch traffic (a confused-deputy resource-exhaustion
  vector: hostile peer A claims to be origin B, forcing us to go fetch B's keys).
- Processing *any* part of the request (routing to a handler, touching room state, even
  deserializing the body into anything beyond bytes) before the signature is checked.

**Defences:**
- Signature verification is a single axum middleware layer that runs before every federation route
  handler, with no per-handler opt-out; unsigned or invalidly-signed requests never reach handler
  code. This is the single most important invariant this crate owns and is called out explicitly in
  the status file as such.
- The bytes signed are reconstructed exactly per the spec's request-signing object shape
  (`method`, `uri`, `origin`, `destination`, `content`) using the same canonical-JSON machinery
  `hs-model` already uses for events, not a bespoke implementation, so the two "canonical JSON" code
  paths in this codebase cannot silently disagree.
- `destination`, when present, must equal our own `server_name`; a request signed for a different
  destination is rejected (defends against a captured/leaked signed request being replayed against a
  server it wasn't meant for).
- Key-fetch-on-demand is bounded (per-origin in-flight-fetch de-duplication so N concurrent forged
  requests claiming the same unknown origin trigger one fetch, not N) and itself subject to the same
  resource limits as any other outbound federation call.
- Body size cap is enforced *before* signature verification reads it (can't be tricked into
  buffering unboundedly to compute a signature over attacker-controlled length).

### 2.4 The transport server's read/query endpoints

**What a hostile actor controls:** every path parameter, query parameter, and (for the endpoints that
take one) request body, from an authenticated-but-malicious peer (see framing above — passing
`X-Matrix` verification is necessary but not sufficient).

**Threats, by endpoint shape:**
- **Unbounded backfill/history disclosure**: `/backfill`, `/state`, `/state_ids`, `/event_auth`,
  `/get_missing_events` all let a remote ask for an amount of history; an unbounded `limit` or an
  unbounded auth-chain walk is a resource-exhaustion lever and, if we serve rooms/events we should
  not, a disclosure lever.
- **Existence/enumeration oracles**: `/event/{eventId}` and friends returning a distinguishable
  error for "room doesn't exist" vs "room exists but you're not in it" vs "event doesn't exist" leaks
  more than intended to a peer that is not a member of the room in question. The correct response to
  "you are not entitled to this" and "this doesn't exist" should not be distinguishable where the spec
  doesn't require it to be.
- **`/publicRooms`, `/query/directory`, `/query/profile`, `/user/devices/{userId}`,
  `/openid/userinfo`**: each discloses local-user or local-room data to a remote party; each must
  respect the corresponding config toggle (`allow_public_rooms_over_federation`,
  `allow_device_name_lookup_over_federation`) and must not become a bulk-enumeration path (no
  wildcard user/room lookups beyond what the spec's own shape allows).
- **`/timestamp_to_event`**: a fishing mechanism for room history shape if not scoped to rooms the
  requester (or the room's join rules) actually allow.
- **Room membership check bypass**: every per-room endpoint must independently confirm the requesting
  server has a joined member in that room (or the room is world-readable, where the spec allows it)
  before returning anything about it — a signature proving *who is asking* is not the same as proving
  *they're allowed to know this*.

**Defences:**
- Every per-room read endpoint checks room membership/visibility for the requesting server before
  touching room content, via the `RoomDataSource` seam (section 5), not ad hoc per-handler logic.
- Every endpoint that accepts a `limit`/count clamps it server-side to a fixed maximum (section 3)
  regardless of what the caller asked for; there is no "give me everything" path.
- 404-shaped and 403-shaped responses are chosen to match the spec's own documented behaviour per
  endpoint (some deliberately do distinguish, most don't) rather than an ad hoc per-handler choice —
  tracked per-handler in the implementation, not asserted blanket here.
- Response bodies are capped in the same way request bodies are (section 3) — a read endpoint that
  can be made to emit gigabytes because a hostile peer asked cleverly is still a resource-exhaustion
  bug even though it "only" reads.

### 2.5 The join/leave/knock/invite handshakes (`make_*`/`send_*`, `/invite`,
`/exchange_third_party_invite`) — **seams for this pass, still threat-modelled now**

These are exactly the highest-risk surface in federation (`PLAN.md`'s own risk callout: "unbounded
state in `send_join`") and are deliberately not implemented against real state in this pass — they
validate and reject cleanly instead of running unimplemented logic against production data. Recorded
here so the seam's eventual implementation has to answer these, not rediscover them:

- **`send_join`/`send_knock` unbounded state**: the joining server can be handed (in v1) or the
  requesting server can supply (in the reverse direction, `make_join` response consumption) an
  arbitrarily large room state to authorize against. Must be bounded (event count, total byte size,
  event-auth-chain depth) before any authorization work begins, not after.
- **State/auth confusion across room versions**: an event or state snapshot claiming one room version
  while actually shaped like another (different event-ID derivation, different auth rules) is a
  classic downgrade/confusion attack; the room version must be pinned from *our own* record of the
  room, never taken from the incoming payload.
- **Faster joins (`omit_members`) partial-state trust window**: a room joined via a partial state
  snapshot is, by construction, trusting the resident server's summary of membership beyond what we
  can independently verify until backfill completes; the partial-state flag
  (`hs_model::event::EventFlags::is_partial_state`, already landed) exists precisely to make sure
  nothing downstream (state resolution, push, sync) treats that state as final before it's
  reconciled.
- **Third-party invite signature confusion**: `m.room.third_party_invite`/`m.room.member` signed
  third-party-invite content must be checked against the identity server's key the invite actually
  named, not any key we happen to have cached, and the signed content must match the room/sender it's
  being redeemed in (already partially covered by `hs-state::auth`'s existing third-party-invite
  signature check, per `docs/status/02-state-and-model.md`).

The stub handlers for these routes must: verify the `X-Matrix` signature (same as every other route),
parse and structurally validate the request body against the spec shape (bounded size, required
fields), reject with a clear, typed "not implemented yet" error — and do nothing else. They must not
attempt partial logic that looks plausible but skips the above; a half-built handshake handler is
more dangerous than an honest 501, because a peer (or a future contributor) can mistake it for
working.

### 2.6 The federation client (outbound)

**What a hostile actor controls:** for a destination we are actively trying to reach, everything the
destination's HTTP server sends back (status, headers, body, connection behaviour — slow responses,
huge responses, connection resets mid-stream); indirectly, a malicious room member can also cause us
to *originate* federation traffic (fan-out to every server in a room) that we do not fully control the
destination list of.

**Threats:**
- A malicious or compromised destination hanging a connection open, sending an unbounded response
  body, or accepting a connection and then never responding, to exhaust our outbound connection pool
  or per-destination concurrency slots.
- A room with many hostile/dead servers as members causing unbounded fan-out and pool exhaustion
  (denial of service against *us*, via other people's room membership) if concurrency isn't bounded
  per destination and in aggregate.
- Retry storms against a destination that is failing on purpose (or by accident) to see if we can be
  made to hammer it, or to keep our destinations table growing unboundedly.
- Domain allow/deny list bypass: a destination resolved via `.well-known`/SRV to a delegated name not
  itself checked against the allow/deny list (checking only the original server name and then
  connecting to whatever it delegated to would defeat an operator's explicit block).
- TLS verification bypass or downgrade (accepting an invalid certificate, or negotiating down to a
  version `verify_certificates`/`federation_client_minimum_tls_version`-equivalent config disallows).

**Defences:**
- Response body size cap and a hard per-request timeout (`FederationConfig::client_timeout`, already
  landed) enforced by the HTTP client regardless of what the server does.
- Per-destination concurrency limit (a semaphore per destination server name) bounds both intentional
  and room-membership-driven fan-out; an aggregate connection-pool ceiling bounds total outbound
  federation concurrency across all destinations.
- Backoff is destination-scoped and persisted (`destinations` table), so a destination we've already
  marked as failing does not get retried on every caller's schedule independently — one shared backoff
  state per destination, checked before every attempt.
- The allow/deny check (`FederationConfig::domain_allowlist`) is evaluated against the *original*
  server name we were asked to federate with, and the resolved connection address is separately
  checked against `ip_range_blocklist`/`ip_range_allowlist` (section 2.1) — both checks apply on every
  call, so delegation cannot smuggle a request past either.
- TLS certificate verification follows `FederationConfig::verify_certificates`; `rustls` (already a
  workspace dependency) is used rather than a hand-rolled TLS stack.

### 2.7 Server ACLs (`m.room.server_acl`)

**What a hostile actor controls:** nothing about the ACL mechanism itself (it's local room state we
compute), but a room's ACL can be *stale or absent* by the time a hostile server tries to act, and the
mechanism must be applied symmetrically or it's a one-sided defence.

**Threats:**
- Checking ACLs only on inbound (accepting events from a denied server) while still sending that
  server outbound traffic (leaking room activity to a server the room explicitly excluded) — or the
  reverse. Either asymmetry defeats the point of the room deciding "we do not federate with this
  server."
- Glob-matching bugs (an overly permissive translation of `*`/`?` into a regex that matches more or
  less than intended) either silently widening a deny list (security hole) or silently widening an
  allow list beyond what the room admin wrote (also a security hole, from the room's perspective).
- IP-literal servers bypassing hostname-based deny patterns (`allow_ip_literals` exists precisely
  because a deny list written as hostnames does nothing against a server operating by bare IP).

**Defences:**
- One ACL evaluation function, used for both the inbound accept-path and the outbound send-path, so
  there is exactly one place the allow/deny/glob logic can be wrong, not two that can drift apart.
- Glob translation is anchored (`^...$`) and escapes every regex metacharacter except the two the
  spec defines (`*`, `?`), with unit tests asserting both "matches what it should" and "does not match
  what it shouldn't" for adjacent patterns (a common bug class: `*.example.org` matching
  `example.org` itself, or `?` accidentally matching zero characters).
- `allow_ip_literals` is checked explicitly against the resolved-as-IP-literal form of the server name
  string (not the DNS-resolved address of a hostname, which is a different question already covered
  by `ip_range_blocklist`).

## 3. Resource limits (concrete numbers)

These are this track's defaults; each is a named constant in the implementation (not a magic number
inline) so they can be tuned later without hunting for call sites. Chosen conservatively from the
spec's own size limits plus Synapse's published defaults as a behavioural reference (never its code).

| Limit | Value | Applies to |
|---|---|---|
| Max PDU size | 64 KiB | Matches `hs_model::event::MAX_PDU_BYTES`, already the spec's own figure; every PDU we accept from a transaction or a join/leave response is checked against it before parsing. |
| Max EDU size | 64 KiB | Same spec figure, applied per-EDU inside a transaction. |
| Max transaction body size | 1 MiB | Bounds `/send`'s total PDU+EDU batch before we even start iterating it (the spec caps a transaction at 50 PDUs / 100 EDUs; 1 MiB is a generous outer bound given the 64 KiB per-item cap, chosen so a compliant peer never hits it and a hostile one can't pad past it). |
| Max PDUs per transaction | 50 | Spec-recommended figure. |
| Max EDUs per transaction | 100 | Spec-recommended figure. |
| Max `.well-known` body size | 16 KiB | Generous for a JSON object with one string field; anything larger is refused outright, body not buffered past the cap. |
| Max key-server response body size | 64 KiB | Generous for a `verify_keys`/`old_verify_keys` map at realistic key-rotation cadence; still bounded. |
| Max generic federation response body size (client) | 50 MiB | Backstops `/state`, `/backfill` and similar bulk responses; enforced by the HTTP client independent of any `Content-Length` the peer claims. |
| Max generic federation request body size (server) | 50 MiB | Same bound, inbound direction, enforced before JSON parsing begins. |
| `/backfill` `limit` clamp | 100 events | Server-side ceiling regardless of the caller's requested value. |
| `/get_missing_events` `limit` clamp | 100 events | Same. |
| `/state`/`/state_ids`/`/event_auth` response size | bounded by the room's actual state/auth-chain size, but the *request* `event_id` count and any array inputs are clamped to what a single well-formed request needs (1 event) | Prevents a hostile peer from batching an unbounded number of state-snapshot requests behind one call once such a shape exists (none of the current spec endpoints in this pass accept a batch on this path, but the clamp is defensive) . |
| `/publicRooms` page size clamp | 100 rooms per page | Matches typical client-server pagination limits, applied to the federation-facing endpoint too. |
| Per-destination outbound concurrency | 1 in-flight request per destination by default | Matches Synapse's default and bounds fan-out against any single destination; configurable per `open questions` in the brief. |
| Aggregate outbound federation concurrency | bounded by the shared `reqwest::Client` connection pool size | Prevents unbounded total fan-out across many destinations at once. |
| Outbound request timeout | `FederationConfig::client_timeout` (default 30 s, already landed) | Every outbound call. |
| `.well-known`/SRV/A fetch timeout | 10 s | Tighter than the general client timeout since discovery blocks the first real request. |
| Key-fetch-on-demand de-duplication | one in-flight fetch per unknown origin, concurrent callers await it rather than each triggering a fetch | Bounds the confused-deputy amplification described in 2.3. |
| Max destination backoff ceiling | `FederationConfig::max_retry_backoff` (default 60 min, already landed) | Exponential backoff between retries to a failing destination is capped here. |
| JSON parse depth (well-known, key responses, PDUs/EDUs, transaction bodies) | 32 levels | Backstops stack-exhaustion-shaped attacks against `serde_json`'s recursive descent regardless of the byte-size cap. |

## 4. What is explicitly out of scope for this pass

Recorded so it isn't mistaken for an oversight: `/send` and the join/leave/knock/invite handshakes
run against real room state (section 2.5's seams only); MSC4284 policy servers; MSC4242 explicit
federation; remote media proxying (owned by track 09, this crate only need not block it); `user/keys/*`
(owned jointly with track 08, which has not started — wave 2, week 8); faster-join resumption and
sender-shard catch-up semantics (Phase 1/2 per the brief). None of these are defended against yet
because none of them run yet; each must get its own pass through this document's per-endpoint
catalogue before it ships.

## 5. The `RoomDataSource` seam

Every per-room read endpoint in section 2.4 needs to ask "does this room exist, is this server's
membership current, what is the state/event/auth-chain content" — questions that, in the finished
system, are answered by the room actor (`hs-room`). Per this track's sequencing instructions, inbound
persistence is not built against the room actor's current flat state map (`docs/rfcs/0010`, gap 1: the
actor's `current_state` map is only correct for the single-writer, no-fork case, which inbound
federation immediately violates). The same reasoning applies to *reads*: a federation read endpoint
that silently degrades to "whatever the flat map currently holds" is not a defensible foundation to
build the section 2.4 defences on top of.

`hs-federation` therefore defines its own `RoomDataSource` trait expressing exactly the read
operations this catalogue needs (room membership/visibility for a server, event lookup, state lookup,
auth-chain lookup, backfill, missing-events), independent of `hs-room`'s internal representation. This
keeps every handler's logic — including every defence above — real and testable today against a
fake implementation, while the concrete adapter onto `hs-room`'s actual query surface
(`RoomActorHandle::query`, per `crates/hs-room/src/actor.rs`) is written once track 04's state-engine
wiring lands, per this track's own sequencing instructions. Recorded here rather than only in the
status file because it is a security-relevant boundary, not just an implementation convenience: it is
the seam every defence in section 2.4 is checked against.
