# 0013: wiring `hs-e2e` into `GET /sync`'s `to_device`, `device_lists` and key-count fields

Status: proposed. Author: track 08 (E2EE). Affects: track 05 (sync, owns `hs-user`), track 08
(E2EE, owns `hs-e2e`), track 14 (integration).

## Problem

`crates/hs-loadgen/src/scenario_encrypted.rs` (this session's addition, track 08) drives two real
`matrix-sdk` clients with `e2e-encryption` enabled through the full first-contact flow against a
real `hs serve` process: device-key upload, `/keys/query`, an atomic `/keys/claim` under real
concurrency, cross-signing bootstrap, an encrypted room, and a real message send.

Every `hs-e2e` route involved worked correctly on the first run against a real client -- no bug
was found or needed fixing in `crates/hs-e2e`. The one place the scenario could not complete its
job is not in `hs-e2e` at all: **`hs-user`'s `GET /sync` never includes `to_device`,
`device_lists`, `device_one_time_keys_count` or `device_unused_fallback_key_types`.** This is a
known, documented gap -- see `crates/hs-user/src/sync/mod.rs`'s own module doc, "Not implemented
in this pass" -- recorded there as blocked on track 08's store existing. It now exists
(`crates/hs-e2e/src/store/mod.rs`), so this RFC specifies the interface `hs-user` needs to adopt
it.

### Confirmed, concrete consequences (not hypothetical)

Running `cargo test -p hs-loadgen --test real_client_encrypted -- --nocapture`:

1. Alice sends an encrypted message. `Room::send` on her `matrix-sdk` client transparently claims
   one of Bob's real one-time keys (`POST /keys/claim`), establishes an Olm session, and shares
   the Megolm room key over `PUT /sendToDevice` -- all three succeed (if any had failed, the send
   call itself would have returned an error rather than an event id, which it did not).
2. Bob's next `GET /sync` never carries that to-device message (confirmed directly against the
   wire response: the top-level `to_device` key is absent). His `matrix-sdk-crypto` `OlmMachine`
   never receives the room key.
3. Bob's client therefore cannot decrypt Alice's message:
   `UnableToDecryptReason::MissingMegolmSession { withheld_code: None }`.
4. Separately: Bob, who shares a room with Alice, never sees Alice's cross-signing-key upload
   reflected in `device_lists.changed` (confirmed: the `device_lists` key itself is absent from
   the raw `/sync` response) -- the mechanism real clients use to notice a device changed at all.
5. A secondary, likely-unintended effect of the same gap: because `device_one_time_keys_count` is
   never present, `matrix-sdk-crypto` treats a missing count as "zero keys left on the server" on
   *every* incremental sync (this is [MSC-documented client behavior](https://spec.matrix.org/v1.19/client-server-api/#device_one_time_keys_count),
   not a client bug) and re-generates and re-uploads a full batch of one-time keys after every
   single `sync_once` call. In this session's run, Alice (three `sync_once` calls before the
   check) had 200 uploaded `signed_curve25519` keys where Bob (fewer calls at that point) had 100
   -- an unbounded-growth pattern that will only get worse the longer a real client's sync loop
   runs. This resolves itself for free once `device_one_time_keys_count` is populated correctly.

None of this is fixable inside `crates/hs-e2e`: the store-side cursors already exist and are
already tested (see below); the gap is entirely that nothing outside `hs-e2e` calls them from
`GET /sync`.

## What already exists in `hs-e2e` (nothing new needed here)

`crates/hs-e2e/src/store/mod.rs`:

- `ToDeviceStore::poll_since(user, device, since, limit) -> (Vec<ToDeviceMessage>, u64)` -- exactly
  the cursor `to_device.events`/`to_device.next_batch` need. `ToDeviceMessage` is
  `{ sender: OwnedUserId, event_type: String, content: Value }`, already shaped like a to-device
  EDU minus the `type`/`content` wire wrapping (trivial to map to
  `{"sender": ..., "type": ..., "content": ...}`).
- `ToDeviceStore::delete_up_to(user, device, upto)` -- called once a sync response that included
  those messages has been durably returned (matching the spec's at-least-once delivery contract:
  do not delete until the response carrying them has been produced).
- `DeviceKeyStore::changed_users_since(since, upto) -> BTreeSet<OwnedUserId>` and
  `DeviceKeyStore::current_stream_pos()` -- exactly `device_lists.changed`'s source, already
  exposed today at `GET /keys/changes` (`crates/hs-e2e/src/routes/keys_changes.rs`). `hs-user`
  needs the same query, filtered to users the syncing user shares a room with (this crate has no
  room-membership notion at all -- see that route's own doc comment -- so this filtering step must
  happen in `hs-user`, which already knows room membership). `device_lists.left` needs the
  symmetric "user I no longer share any room with, but did as of `since`" computation, which
  belongs entirely in `hs-user`/`hs-room` state and has no `hs-e2e` component.
- `OneTimeKeyStore::count_one_time_keys(user, device) -> BTreeMap<String, u64>` -- exactly
  `device_one_time_keys_count`.
- `FallbackKeyStore::unused_fallback_key_algorithms(user, device) -> Vec<String>` -- exactly
  `device_unused_fallback_key_types`.

All of the above are exercised by `crates/hs-e2e/tests/scenario.rs`,
`crates/hs-e2e/tests/otk_concurrency.rs` and unit tests in `crates/hs-e2e/src/store/tables.rs`
(concurrency-safe on both the in-memory and Fjall backends), and now additionally by
`crates/hs-loadgen/src/scenario_encrypted.rs` against a real client for the upload/query/claim
side.

## What `hs-user` needs to do

`crates/hs-cli/src/serve.rs`'s `ServerState` already holds both `user: UserState<...>` and
`e2e: hs_e2e::state::E2eState<B>` as sibling fields, built over the same backend in
`build_session_mounts`. The minimal-diff wiring:

1. Give `hs-user`'s sync route handler (`crates/hs-user/src/routes/sync.rs`) access to
   `Arc<dyn hs_e2e::store::E2eStore>` (or the individual trait objects it needs -- `DeviceKeyStore
   + OneTimeKeyStore + FallbackKeyStore + ToDeviceStore` covers everything above), the same way
   other cross-crate state is composed in this workspace (an additional field on whatever state
   struct `hs-cli` builds for the sync router, populated from `e2e.store` in `serve.rs`).
   `hs-user` cannot depend on `hs-e2e`'s concrete store type without a `Cargo.toml` dependency
   edit -- that edit, and the `serve.rs` wiring, are both outside track 08's owned files and are
   requested here rather than made directly.
2. In `crate::sync::build` (`crates/hs-user/src/sync/mod.rs`), after computing the responding
   user's device id:
   - `to_device`: call `poll_since(user, device, <device's last acked to-device stream position>,
     limit)`. The per-device to-device cursor needs to live somewhere durable in `hs-user`'s own
     device/session bookkeeping (analogous to how `SyncToken`/`room_pos_as_of` already track
     per-room positions) -- `hs-e2e` returns a fresh cursor value each call but does not persist
     "what has this device already acknowledged" across sync sessions itself; the moment a
     response is actually returned to the client, call `delete_up_to` with that response's cursor.
     Consider whether "delete after send" vs. "delete after next request's `since` proves receipt"
     matters here; Synapse's behavior (`refs/synapse/synapse/handlers/devicemessage.py`,
     `refs/synapse/synapse/handlers/sync.py`, AGPL, read-only reference) deletes eagerly and
     accepts the small window where a response that never reached the client drops those
     messages, which matches this store's own `delete_up_to` contract.
   - `device_lists.changed`: `changed_users_since(since_stream_pos, None)` intersected with "users
     the syncing user currently shares any room with" (from `hs-room`/`hs-user`'s own membership
     data). `since_stream_pos` needs the same "opaque `/sync` token embeds an e2e stream position"
     treatment `crates/hs-e2e/src/routes/keys_changes.rs` already documents as the seam sync must
     adopt -- likely a new field on `crate::token::SyncToken`.
   - `device_lists.left`: users present in that intersection as of `since` but not now.
   - `device_one_time_keys_count` / `device_unused_fallback_key_types`: straight passthrough of
     `count_one_time_keys`/`unused_fallback_key_algorithms` for the responding device. Per the
     spec (and `matrix-sdk-crypto`'s own documented handling, see "Confirmed, concrete
     consequences" #5 above), a **missing** `device_one_time_keys_count` on the classic `/sync`
     path is interpreted by clients as "zero keys" -- so this field must be populated on every
     response once wired, not just when non-empty, to stop the unbounded-reupload behavior
     described above.
3. All four fields are additive and optional-by-spec; existing passing tests
   (`crates/hs-user`'s own suite, `crates/hs-loadgen/tests/real_client.rs`) should be unaffected.

## Non-goals of this RFC

- Federation delivery of to-device messages and device-list EDUs to/from remote servers (a
  separate, already-documented seam owned by track 08 jointly with track 06; unaffected by this
  RFC, which is entirely about the local, single-server case).
- `device_lists.left` semantics beyond "no longer sharing a room" (e.g. redacted-membership
  edge cases) -- flagged for whoever implements this to settle against Synapse's behavior.

## Verification this RFC asks for once implemented

Re-run `cargo test -p hs-loadgen --test real_client_encrypted -- --nocapture`
(`crates/hs-loadgen/src/scenario_encrypted.rs`): its two `KNOWN BUG` log lines (device-list-change
visibility, and the decryption failure) should both disappear and be replaced by hard assertions
(the scenario already asserts the success path when decryption/device-list-visibility succeed; no
scenario code changes should be needed, only removing the two `else` branches once they stop
firing -- though re-reading the diff at that point is still worth it in case removing the branch
changes the log's step count anywhere).
