# 0014. Event signing must sign the redacted form, not the full event

Status: proposed, urgent. Owner of the fix: track 04 (room and events), `crates/hs-room/src/pipeline.rs`.
Also affects: `crates/hs-cli/tests/federation_writes.rs` (test-only, same bug, mechanical fix).
Discovered by, and already fixed in: track 06 (federation), `crates/hs-federation/src/inbound.rs`.

Companion artifacts: `crates/hs-model/src/signing.rs`, `crates/hs-model/src/redaction.rs`,
`refs/matrix-spec/content/server-server-api.md` ("Adding hashes and signatures to outgoing
events" / "Validating hashes and signatures on received events"), `docs/status/06-federation.md`
("Sixth session" or later — see that file for the session this RFC was written in).

## 1. The bug

The Matrix spec's signing algorithm for a PDU is, in order:

1. Compute the *content hash* of the full, unredacted event; store it in `hashes.sha256`.
2. **Redact** the event (client-server API's redaction algorithm, room-version-specific).
3. Sign the **redacted** object (strip `signatures`/`unsigned`, canonicalize, sign).
4. Copy the resulting signature back onto the **original, unredacted** event object.

(`refs/matrix-spec/content/server-server-api.md`, "Adding hashes and signatures to outgoing
events".) The matching verification algorithm is symmetric: "the event is redacted following the
redaction algorithm, and the resultant object is checked for signatures... Note that this step
should succeed whether we have been sent the full event or a redacted copy." Redaction is
deterministic from an event's `type` alone (`hs_model::redaction::redact`), so signing the *full*
event instead of the *redacted* one produces a signature that a spec-compliant verifier — which
always redacts before checking — will reject, for any event whose content carries anything
redaction would strip. For `m.room.message`, redaction strips *all* of `content`
(`hs_model::redaction::redact_content`'s match falls through to `_ => CanonicalJsonObject::new()`
for any type it does not special-case), so this is not an edge case: it is every ordinary message
this server has ever sent.

`crates/hs-room/src/pipeline.rs` (around line 360-373, `build_and_authorize`'s hash-and-sign step)
computes the content hash, inserts `hashes`, and then calls `signing::sign_object(&mut canonical,
server_name, signing_key)` directly on the **unredacted** `canonical` object — no redaction step in
between. Every event this server originates is therefore signed the wrong way: a spec-compliant
remote homeserver receiving one of this server's `m.room.message` events over federation (or any
other event type whose content is not fully retained by redaction — e.g. an `m.room.member` with a
`displayname`/`avatar_url`, once profile-carrying joins exist) will redact it first per its own
inbound verification, and the signature will not match.

## 2. How this was found

This session (track 06) fixed the *receiving* side of the same bug in
`crates/hs-federation/src/inbound.rs::verify_pdu`, which previously verified a received PDU's
signature against the full, unredacted event instead of the redacted one — the direct cause of a
Complement-observed failure (`send_join` rejecting a genuinely, correctly-signed join from a real
peer with `M_BAD_JSON: signature ... does not verify`). Fixing `verify_pdu` to redact before
verifying (matching the spec's "Validating hashes and signatures" text quoted above) is correct and
is what actually accepts a real remote server's join.

That same fix, applied honestly, also makes `verify_pdu` reject this server's *own* previously
self-consistent (but spec-non-compliant) signatures wherever the two sides used to agree only
because both signed and verified the same wrong way. This was caught empirically, not just
reasoned about: after the `verify_pdu` fix, four of `crates/hs-cli/tests/federation_writes.rs`'s
eight tests started failing —

```
cargo test -p hs-cli --test federation_writes
```

```
send_accepts_an_event_it_already_holds_idempotently ... FAILED
  "signature from local.example/ed25519:1 does not verify"
send_rejects_a_new_event_whose_auth_events_do_not_authorize_it ... FAILED
  (still rejected, but for the wrong reason -- a signature error, not an auth error)
send_backfills_a_missing_ancestor_then_accepts_the_original_event ... FAILED
send_gives_up_when_the_remote_serves_an_endless_backfill_chain ... FAILED
```

Three of the four (`send_rejects_a_new_event_whose_auth_events_do_not_authorize_it`,
`send_backfills_a_missing_ancestor_then_accepts_the_original_event`,
`send_gives_up_when_the_remote_serves_an_endless_backfill_chain`) are purely a **test-fixture** bug:
they hand-build a synthetic "remote" `m.room.message` and sign it the same (wrong) unredacted way
`crates/hs-federation`'s own tests used to (see §3 for the fix, already applied twice in this
session's own crate). `send_accepts_an_event_it_already_holds_idempotently` is different: it
resubmits `harness.full_pdu(...)`, an event this server **actually built and signed through the
real `RoomActor`/`pipeline.rs` path** — proof that the bug in §1 is live in production code, not
just in test helpers. `send_join_v2_persists_the_join_and_it_is_readable_afterwards` and
`make_join_builds_a_real_template_against_the_real_room` still pass only because the join events
they exercise have `content: {"membership": "join"}` and nothing else — membership is one of the
few fields `m.room.member` redaction always keeps, so for that narrow content shape the full and
redacted forms happen to be byte-identical and the bug has no observable effect.

## 3. The fix, precisely

### 3a. `crates/hs-room/src/pipeline.rs` (track 04's file — not edited by this session)

Around the existing hash-and-sign block:

```rust
let mut canonical = to_canonical_object(
    &serde_json::Value::Object(object),
    rules.strict_canonical_json,
)
.map_err(hs_model::EventError::from)?;
let content_hash = hash::content_hash_base64(&canonical);
canonical.insert(
    "hashes".to_owned(),
    CanonicalJsonValue::Object(CanonicalJsonObject::from([(
        "sha256".to_owned(),
        CanonicalJsonValue::String(content_hash),
    )])),
);
signing::sign_object(&mut canonical, server_name, signing_key)?;
```

needs to become (redact the object that gets signed, sign *that*, then copy the signature back
onto the real, unredacted `canonical` this method returns and persists):

```rust
signing::sign_object(&mut canonical, server_name, signing_key)?;
```
becomes
```rust
let rules_redaction = &rules.redaction; // `RoomVersionRules` already in scope as `rules` here
let mut redacted = hs_model::redaction::redact(&canonical, rules_redaction)
    .map_err(hs_model::EventError::from)?;
signing::sign_object(&mut redacted, server_name, signing_key)?;
canonical.insert(
    "signatures".to_owned(),
    redacted.remove("signatures").expect("sign_object always inserts a signature"),
);
```

(Exact local variable names will need adjusting to whatever `rules`/`RoomVersionRules` is called at
that point in `pipeline.rs` — this session did not edit that file and is describing the shape of
the fix, not a verified patch against its current line numbers.) This is the same three-step
pattern (`redact` → `sign_object` on the redacted copy → copy `signatures` back onto the original)
already applied in `crates/hs-federation/src/inbound.rs` and `crates/hs-federation/src/backfill.rs`
this session, in their own test helpers — see those files' `signed_event`/`signed_message`
functions for a working example of exactly this sequence.

**Impact of not fixing this**: every event this server sends to a real, spec-compliant remote
homeserver, whose content is not fully retained by redaction (which is most content — a message
body, a profile-carrying join, ...), has its signature rejected by that remote server today. This
is symmetric with, and arguably more consequential than, the bug `verify_pdu` just fixed on the
receiving side: this server's outbound federation traffic has likely never been correctly verifiable
by another compliant implementation, independent of the TLS/CA gap this session's other half closed.

### 3b. `crates/hs-cli/tests/federation_writes.rs` (test-only, mechanical)

Three call sites build a synthetic PDU and sign it the same wrong (unredacted) way:

- `build_signed_message` (around line 394-429): the shared helper for the backfill tests.
- The inline block in `send_rejects_a_new_event_whose_auth_events_do_not_authorize_it` (around
  line 495-519).
- Any other inline `hs_model::signing::sign_object(&mut object, ...)` call in this file signing an
  object built directly from a `serde_json::json!` literal rather than through the real pipeline
  (grep the file for `sign_object` to find all of them; this session counted three across two
  patterns).

Each needs the same three-line change already made twice in `crates/hs-federation/src/inbound.rs`
and `crates/hs-federation/src/backfill.rs` this session:

```rust
let server = ruma::ServerName::parse(sender.split_once(':').unwrap().1).unwrap();
hs_model::signing::sign_object(&mut object, &server, signing_key).unwrap();
```
becomes
```rust
let server = ruma::ServerName::parse(sender.split_once(':').unwrap().1).unwrap();
let rules = hs_model::room_version::rules_for(&ruma::RoomVersionId::V11).unwrap();
let mut redacted = hs_model::redaction::redact(&object, &rules.redaction).unwrap();
hs_model::signing::sign_object(&mut redacted, &server, signing_key).unwrap();
object.insert("signatures".to_owned(), redacted.remove("signatures").unwrap());
```

(`ruma::RoomVersionId::V11` matches every existing test in this file, which is already hard-coded
to room version 11 elsewhere.)

## 4. Why this session did not fix `pipeline.rs` itself

`crates/hs-room/**` is track 04's crate; this session's brief restricted it to
`crates/hs-federation/**`, `crates/hs-config/**`, and `docs/status/06-federation.md`, specifically
so that concurrently-running agents in `hs-room` and other crates are not collided with. The
`verify_pdu` fix (§2) was kept — it is correct, it is the actual named target of this session's
work (a real Complement-observed `send_join` rejection), and it is fully covered by this crate's
own green test suite (`cargo test -p hs-federation`, 120/120) after the two test-helper corrections
described in `docs/status/06-federation.md`. The four `hs-cli` test failures this exposed are left
red on purpose rather than silently worked around, since silently reverting the fix would mean
knowingly leaving `send_join` broken against real, correctly-signed remote peers — the specific bug
this session was asked to fix.

## 5. What to verify once both fixes land

```
cargo test -p hs-federation --lib                 # already green (120/120) with only this
                                                   # session's fix in place
cargo test -p hs-cli --test federation_writes      # 4/8 fail until §3a and §3b both land; should
                                                   # be 8/8 once they do
```

A differential/Complement re-run against a build with both fixes should show the previously-blocked
`federation_room_event_auth_test.go` case (`send_join` rejecting a legitimate join) passing, and
should **not** newly break anything that depends on this server's own outbound signatures being
independently verifiable (there is no such test today, since nothing outside this server has ever
verified one of its signatures against a spec-compliant redact-then-verify implementation — which
is precisely the gap this RFC closes).
