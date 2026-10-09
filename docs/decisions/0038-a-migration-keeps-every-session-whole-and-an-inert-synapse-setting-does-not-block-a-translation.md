# 0038. A migration keeps every session whole, and an inert Synapse setting does not block a translation

Date: 2026-10-09. Track 13 (`agent/migration-95`). Affects tracks 04 (one line in `hs-room`'s
import path), 16 (the Migration page's stream list) and anyone running the translator.

## Context

The README rated "Synapse migration" at 75% with three things missing: threaded receipts,
partial-state rooms, and most of the `/_synapse/admin` surface. A rehearsal against a real
Synapse 1.162 (the first time the importer met one newer than the fixture's 1.161) showed what
an operator would actually hit first, in this order:

1. **The translator refused Synapse's own generated `homeserver.yaml`** over `pid_file`,
   `form_secret` and `trusted_key_servers`: keys that cannot change anything here.
2. **Every room failed to import**: Synapse's default room version is 12 now, and Synapse keeps
   a `room_id` inside its stored `m.room.create` JSON (it strips it only when serving the event),
   while the version forbids one on the wire and this server's authorization rightly refused it.
3. **Sessions did not survive whole**: access tokens were copied but not refresh tokens (a client
   whose access token expired after cutover was signed out), nor the email addresses and phone
   numbers people sign in by, nor the upstream identities SSO sign-ins land in, nor erasures,
   nor the room keys waiting in `device_inbox` for phones that were offline, nor registration
   tokens already handed out.
4. **synapse-admin's screens** got `404 M_UNRECOGNIZED` from all but eight routes.

## Decision

1. **An unsupported key whose value cannot change anything here is inert, and inert keys never
   block.** The list is literal and short (`hs_compat::translate::inert_reason`): process
   supervision, Python tuning, worker topology (one process does all of it, D2), Synapse's
   caches and background updates, the SSO-form secret, two warning switches, and two keys at
   Synapse's own default when that default is what this server does anyway
   (`trusted_key_servers` naming only matrix.org; `presence.enabled: true`). Every other
   unsupported key still blocks: the translator does not try to know Synapse's default for 165
   keys, and a key an operator wrote down is a decision to acknowledge. The report says
   `inert (no effect here)` for each.
2. **The importer copies what a signed-in session needs to go on as it was**, as streams of
   their own (counts, checkpoints, metrics and log lines per kind): `refresh_tokens` (unspent
   ones, beside their access tokens), `threepids`, `external_ids`, `to_device` (messages still
   waiting, recognized on a second pass by sender, type and content), `registration_tokens`;
   erasure travels inside `users`. Verification compares each. A refresh token Synapse already
   exchanged is left out: it would be refused on either server.
3. **The reader hands over events as Synapse serves them, not as it stores them**
   (`source::as_synapse_serves_it`): no `unsigned`, and no `room_id` in a create event whose
   room id is its own hash. And `hs-room`'s import path judges a room-version-12 create event
   without the room id the actor already knows, the way `IncomingEvent::room_id`'s own contract
   says ("absent for a room version 12+ `m.room.create`"). That one-line change is in track
   04's crate, made here because the rehearsal could not pass without it and nobody else was in
   the crate; it has its test (`crates/hs-room/tests/import.rs`, which fails without it) and
   track 04 should review it.
4. **The `/_synapse/admin` surface mounts what synapse-admin's screens call, and nothing that
   cannot be answered honestly.** Every route forwards into `/api/v1` behind the caller's own
   token and reshapes the answer; a Synapse route whose native counterpart is a background task
   answers the task's id in Synapse's asynchronous shape (room deletion answers `{"delete_id"}`
   for v1 and v2 alike; redaction answers `{"redact_id"}`), and a route that would have to
   invent a synchronous result (v1's list of kicked users, bulk media deletes, the cache purge)
   is not mounted. `hs-compat` names every mounted route in `SYNAPSE_ADMIN_ROUTES`, its tests
   hold the router to that list, and `hs-cli`'s manifest is derived from it rather than
   mirrored by hand.
5. **The cutover step is "stop Synapse, or make it read-only", with the delta brought over by
   the final pass**, and the rehearsal is a real-binary test that does exactly that against a
   real Synapse (`crates/hs-cli/tests/migration_rehearsal.rs`), skipping without Docker.

## Consequences

- A `homeserver.yaml` straight out of `generate` serves without
  `--allow-unsupported-synapse-config` (`testdata/generated-1.162.yaml` is in the corpus).
- `hs_compat::migration::Stream` has 19 variants; `MigrationTarget` has five more import and
  five more verify methods and `TargetUser` an `erased` flag. The Migration page must name the
  five new streams and drop "Receipts in threads" from what does not move (status 13 has the
  exact list for track 16).
- What still does not move, by design or for want of a home: unread counts, Synapse's
  server-notice rooms as notice rooms, bridges' stream positions, dehydrated devices,
  thumbnails, a room Synapse is still joining, a registration under way.
