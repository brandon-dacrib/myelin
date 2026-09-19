# 10 Push: status

## Session 1: the `/sync` push-rules seam, and checking the default ruleset against the spec

**Starting point.** `hs-push` already existed from earlier phase-0/1 work (the rules engine on
`ruma::push`, `CachedRulesetStore`, `CountsStore`, HTTP pushers with retry/backoff, the
`/pushrules`/`/pushers` HTTP surface) — 28 tests green, `cargo clippy -p hs-push` clean. What did
not exist: any way for `/sync` (track 05, `hs-user`) to actually learn a user's push rules or
notification counts. Confirmed directly in `hs-user`'s own code before touching anything:

- `crates/hs-user/src/sync/mod.rs`'s module doc, "Not implemented in this pass": *"Unread
  notification counts: is included... shaped correctly..., with both counts hard-zero until
  track 10 (push) lands."* Its module doc's first line literally says `"unread notification
  counts (zero -- track 10 has not landed)"`.
- The same file (lines ~536-539, before this session) hardcodes
  `"unread_notifications": {"highlight_count": 0, "notification_count": 0}` and
  `"unread_thread_notifications": {}` for every joined room, unconditionally.
- `m.push_rules` never appears anywhere in that file at all — confirming this session's assigned
  bug (`docs/status/14-test-and-conformance.md`'s track-10 item, and Complement's "no pushrules
  found in sync response" on every subtest).

This session's job, per the ownership split (`crates/hs-user` is track 05's; I own `hs-push`
only): make `hs-push` expose the seam correctly, and write down *exactly* what track 05 needs to
add, rather than reach across and edit `hs-user` myself.

### Done

**1. `RulesetStore` learned a change-seq, mirroring `hs_user::store::UserStore`'s own
`account_data_seq`/`changed_seq` convention exactly** (`crates/hs-push/src/rulesets.rs`):

- `RulesetStore::set_ruleset` now returns `Result<u64, StoreError>` — the new change-seq value the
  write landed at (previously `Result<(), StoreError>`).
- `RulesetStore::changed_seq(&self, user_id) -> Result<u64, StoreError>` (new): `0` for a user who
  has never written a custom ruleset (the server-default ruleset never changes on its own),
  otherwise whatever the most recent `set_ruleset` call returned.
- `memory::InMemoryRulesetStore`: rows are now `{ruleset, changed_seq}`; `set_ruleset` increments
  under the existing write lock.
- `tables::TablesRulesetStore`: added a raw (non-`TypedKeyspace`) `hs_push.ruleset_seq` keyspace
  and uses `hs_kv`'s `atomic_add` inside the same transaction that writes the ruleset row —
  byte-for-byte the same pattern `hs_user.account_data_counter` uses in
  `crates/hs-user/src/store/tables.rs` (`put_global_account_data`/`latest_account_data_seq`), for
  the identical reason: one atomic counter per user, safe under concurrent writers via
  serializable-transaction retry, not a special lock-free primitive.

**2. `CachedRulesetStore::account_data_for_sync`** (new, `crates/hs-push/src/rulesets.rs`): the
one call `/sync` needs.

```rust
pub struct PushRulesForSync {
    pub content: serde_json::Value, // {"global": {...}}, exactly the m.push_rules content
    pub changed_seq: u64,
}

pub async fn account_data_for_sync(&self, user_id: &UserId) -> Result<PushRulesForSync, StoreError>;
```

Always returns a value — a user with no stored ruleset still has an effective one (the server
default) and a well-defined `changed_seq` (`0`). The content is built by the new free function
`rulesets::account_data_content(&Ruleset) -> serde_json::Value`, which is just `{"global":
ruleset}` — `ruma::push::Ruleset`'s own `Serialize` impl already emits the wire field names
(`content`, `override`, `room`, `sender`, `underride`) verbatim (verified: ruma-common 0.18.0's
`push.rs`, the version this crate's `Cargo.lock` actually resolves to per
`cargo tree -p hs-push -i ruma-common` — a second, newer `ruma-common` 0.20.0 is also in the lock
file for some other crate's transitive dependency, not this one), so no reshaping was needed; this
is also exactly what `GET /pushrules/`'s handler already returns
(`crate::routes::pushrules::get_pushrules_all`).

**3. Checked the default ruleset against the spec, and against Synapse.** Compared
`ruma::push::Ruleset::server_default` (what `rulesets::default_ruleset` calls) against:

- **The spec** (`refs/matrix-spec/content/client-server-api/modules/push.md`, "Predefined Rules",
  v1.18): as of MSC4210 (spec v1.17, changelog note in that same file: *"the legacy default push
  rules that looked for mentions in the body of the event were removed"*), the normative default
  list is ten override rules (`.m.rule.master`, `.m.rule.suppress_notices`,
  `.m.rule.invite_for_me`, `.m.rule.member_event`, `.m.rule.is_user_mention`,
  `.m.rule.is_room_mention`, `.m.rule.tombstone`, `.m.rule.reaction`, `.m.rule.room.server_acl`,
  `.m.rule.suppress_edits`, in that priority order), **no** default content rules (the old
  `.m.rule.contains_user_name` was retired by MSC4210, not replaced), no default room/sender
  rules, and five underride rules (`.m.rule.call`, `.m.rule.encrypted_room_one_to_one`,
  `.m.rule.room_one_to_one`, `.m.rule.message`, `.m.rule.encrypted`, in that order).
  `ruma::push::Ruleset::server_default` (`ruma-common-0.18.0/src/push/predefined.rs`) matches this
  **exactly** — same rules, same order, same actions, right down to the wire encoding of implied
  values (`Tweak::Highlight(HighlightTweakValue::Yes)` serializes as the bare
  `{"set_tweak":"highlight"}` the spec's own JSON examples use, not `{"set_tweak":"highlight",
  "value":true}` — checked in `ruma-common-0.18.0/src/push/action.rs`'s own
  `serialize_highlight_tweak` test). **No fix needed here**; found one stale doc comment instead
  (see "Decisions made"). This claim is now a regression test, not just a one-time read:
  `rulesets::tests::default_ruleset_matches_the_spec_predefined_rule_list` pins the exact rule-id
  list and order for override/content/room/sender/underride, plus `.m.rule.master`'s
  disabled-by-default / everything-else-enabled-by-default invariant.
- **Synapse** (`refs/synapse/rust/src/push/base_rules.rs`, behavior read only, not copied —
  AGPL-3.0): Synapse's `BASE_APPEND_OVERRIDE_RULES`/`BASE_APPEND_CONTENT_RULES` still ship the
  three MSC4210-retired rules (`.m.rule.contains_display_name`, `.m.rule.contains_user_name`,
  `.m.rule.roomnotif`) for backward compatibility with older clients that still string-match
  `content.body`, plus three unstable/vendor-prefixed rules this project's own brief explicitly
  names as owned but not yet implemented: MSC3664's `.im.nheko.msc3664.reply` (needs a
  "related-event" condition — the referenced event's sender — that `ruma::push::PushCondition`
  does not have any variant for at all; would need a new condition kind plus event-relation
  lookup in `crate::context`/`crate::engine`, a real feature, not a one-line add), MSC4028's
  `.org.matrix.msc4028.encrypted_event` (an override rule that is easy to *write* — plain
  `event_match(type, m.room.encrypted)` → `notify`, no new condition kind needed — but whose
  purpose (waking a client via push so it can replenish one-time-keys, MSC4028's actual concern)
  I did not want to half-implement without the OTK-count plumbing it exists for), and the
  `unstable-msc3930` poll rules for MSC3381 (gated behind a Ruma Cargo feature this crate's
  `Cargo.toml` does not currently enable). **Not implemented this session** — flagged under "Next"
  rather than guessed at, per this track's own risk note ("a server serving a subtly wrong
  ruleset makes every client notify wrongly" applies just as much to a rushed *addition* as to a
  wrong default).

**4. Notification counts: the store-side contract was already correct and tested** (nothing to
fix in `crates/hs-push/src/counts.rs` itself — `CountsStore::get_room_counts`/
`record_notification`/`reset` and both backends already had contract tests). What was missing
was the same thing as push rules: nobody outside this crate calls any of it yet, and `hs-user`
hardcodes zero. Counts do **not** need a `changed_seq`/token seam the way push rules do — they are
naturally scoped to a room already in the sync response (per `counts.rs`'s own module doc: "a
direct keyed lookup per room, never a scan"), so the seam is just "call `get_room_counts` for
every room `/sync` is about to emit" (see "Interfaces provided" below). Confirmed
`context.rs`/`state.rs`'s own doc comments: the reason counts are never populated in a real
`hs serve` process is that `hs_room::protocol::RoomUpdate::push_evaluation_inputs` is still
`Vec<()>` (track 04's placeholder) — a pre-existing, already-documented cross-track dependency,
not something this session introduced or can close from `hs-push` alone.

**5. Tests**: 28 → 31 (all in `crates/hs-push`):
- `rulesets::tests::default_ruleset_matches_the_spec_predefined_rule_list` (new)
- `rulesets::tests::account_data_for_sync_tracks_the_change_seq` (new)
- `rulesets::tests::account_data_content_has_exactly_the_client_facing_global_field` (new)

### In progress

Nothing left mid-flight; this session's scope (the `/sync` seam plus the default-ruleset check) is
complete on the `hs-push` side. The seam is unconsumed until track 05 does its half (below).

### Next

In the brief's stated priority order, and per the gap this session found but did not close:

- **MSC3664 (reply notifications)**: needs a new `PushCondition`-shaped concept (`ruma::push`
  itself has no "related event's sender" condition) plus event-relation lookup added to
  `crate::context::PushEvaluationInput`/`crate::engine`. A real feature, not a default-ruleset
  tweak.
- **MSC4028 (notify on all encrypted events)**: straightforward to add as an override rule (no new
  condition kind), but its point is to help a client with critically low one-time-keys wake up and
  replenish them — implementing the rule without also plumbing something that decides *when* to
  surface it felt like guessing at product behavior neither the spec nor this project's own docs
  pin down yet. Left for whoever picks up the OTK-count-driven push story (adjacent to track 08's
  one-time-key claim work).
- **MSC3381 (polls)**: gated behind Ruma's `unstable-msc3930` Cargo feature, not currently enabled
  in this crate's `Cargo.toml`; enabling it and re-running the golden test would need to also
  extend the default-ruleset regression test's expected list.
- Wiring `hs_room::protocol::RoomUpdate::push_evaluation_inputs` for real (track 04) so
  `record_notification` is ever actually called in a running server — everything on this crate's
  side (`crate::context`, `CountsStore`) is ready and tested; this is purely the other side of an
  already-documented cross-track dependency.
- Once track 05 implements read receipts (`SyncToken::receipts_seq`, reserved but unused per that
  crate's own docs), it should call `CountsStore::reset` when a receipt advances past a notifying
  event — the call this crate's `counts.rs` module doc already names as the reset trigger.

### Blockers

None for this session's delivered scope. The counts *pipeline* (an event actually reaching
`record_notification`) is blocked on track 04's `push_evaluation_inputs`, as already documented in
this crate's own `context.rs`/`state.rs` before this session started — not a new blocker this
session discovered.

### Interfaces provided

**For track 05 (`hs-user`), the exact wiring `/sync` needs — this is the contract, not a
suggestion:**

#### `m.push_rules` account data

Call, once per `/sync` response, per user:

```rust
let push_rules = push_state.rulesets.account_data_for_sync(user_id).await
    .map_err(/* into whatever hs-user's own error type is */)?;
```

(`push_state.rulesets` is `Arc<hs_push::rulesets::CachedRulesetStore<hs_push::rulesets::tables::TablesRulesetStore<B>>>`
— the exact same `Arc` `crates/hs-cli/src/serve.rs`'s `build_session_mounts` already constructs
for `hs_push::state::PushState`. It needs to be handed to whatever constructs `hs-user`'s
`SessionHub`/`UserState` too — today `build_session_mounts` builds `user` and `push` side by side
in the same function but does not share anything between them; that function is `hs-cli`'s, not
mine or 05's alone, so wiring the actual `Arc` through will need a small change there in whichever
order tracks 05/14 pick up this seam.)

Then, mirroring `crates/hs-user/src/sync/mod.rs`'s *existing* `global_account_data`/
`latest_account_data_seq` pattern (lines ~494-513 as of this session) field-for-field:

- **Initial sync** (`is_initial == true`): always include
  `{"type": "m.push_rules", "content": push_rules.content}` in the global `account_data.events`
  list, regardless of `push_rules.changed_seq`.
- **Incremental sync**: include it only if `push_rules.changed_seq > baseline.push_rules_seq`
  (a **new** field you'll need to add to `SyncToken`, see below).
- Either way, when composing the outgoing token: `push_rules_seq:
  push_rules.changed_seq.max(baseline.push_rules_seq)` — the same `.max(...)`-against-baseline
  shape `new_account_data_seq` already uses at line ~511.
- **Long-poll wake condition** (`crate::sync::has_new_data`, `crates/hs-user/src/sync/mod.rs`
  around line 716): add `if push_state.rulesets.store().changed_seq(user_id).await.map_err(...)? >
  baseline.push_rules_seq { return Ok(true); }` alongside the existing
  `latest_account_data_seq` check at line 727 — a rule change should wake a blocked long-poll the
  same way any other account-data change does. (`store()` is
  `CachedRulesetStore::store`, already public, returns `&TablesRulesetStore<B>` — bypasses the
  cache, which is fine here since this is a cheap counter read, not the full ruleset.)

**`SyncToken` change needed** (`crates/hs-user/src/token.rs`): add `pub push_rules_seq: u64`,
following exactly the precedent that file's own doc comment records for adding `typing_seq`
(version 1 → 2): bump `const VERSION: u8` (currently `2`) to `3`, add the field to
`SyncToken::initial()`, extend `PAYLOAD_LEN` from `1 + 7 * 8` to `1 + 8 * 8`, and extend
`encode`/`decode`'s field list. A version bump invalidates old tokens outright
(`TokenError::UnsupportedVersion`), which that file's own doc says is fine here: every deployment
of this greenfield server restarts from a freshly built binary, so no long-lived client holds a
stale-version token across the change.

#### `unread_notifications`/`unread_thread_notifications`

No token/seq plumbing needed — call, per room, right where
`crates/hs-user/src/sync/mod.rs`'s hardcoded zero block is today (~line 536):

```rust
let counts = push_state.counts.get_room_counts(user_id, room_id).await
    .map_err(/* ... */)?;
let totals = counts.totals();
```

```json
"unread_notifications": {
    "highlight_count": totals.highlight_count,
    "notification_count": totals.notification_count
},
"unread_thread_notifications": /* counts.threads, one entry per thread root event id, each
    {"highlight_count": ..., "notification_count": ...} -- empty map if counts.threads is empty,
    exactly today's `{}` placeholder for a room with no thread activity */
```

`push_state.counts` is `Arc<dyn hs_push::counts::CountsStore>` — same object
`hs_push::state::PushState` already holds; same sharing note as above applies (needs to reach
whatever builds `hs-user`'s state too). This call is cheap and side-effect-free (a direct keyed
lookup, `counts.rs`'s own module doc), safe to call unconditionally for every room in every
response — no "did this change" gate needed the way push rules needs one, since the room is
already being emitted for its own reasons (new timeline events, membership, or state) and its
counts should always be current as of the moment the response is built.

**Existing, unchanged interfaces** (present before this session, still the contract):
- `hs_push::counts::CountsStore`: `get_room_counts`, `record_notification`, `reset` — see
  `counts.rs`'s own "One source of truth" note. `record_notification` is called once per event per
  local recipient by whatever consumes track 04's publish stream (not this crate's job to call it
  from `/sync`).
- `hs_push::routes::router`: the `/pushrules`, `/pushers` HTTP surface, already mounted by
  `hs-cli`.

### Interfaces needed

- **04 (room and events)**: a real `hs_room::protocol::RoomUpdate::push_evaluation_inputs` (today
  `Vec<()>`) so this crate's own evaluation loop (`crate::pushers`, once wired to consume the
  publish stream) ever calls `CountsStore::record_notification` in a running server. Already
  documented in `crate::context`'s module doc before this session.
- **05 (sync)**: everything under "Interfaces provided" above — this is the actual ask this
  session exists to make precise.
- **05 or hs-cli (whoever owns `build_session_mounts`)**: thread the same `Arc<CachedRulesetStore<...>>`
  and `Arc<dyn CountsStore>` `crates/hs-cli/src/serve.rs` already builds for `hs_push::state::PushState`
  into whatever constructs `hs-user`'s `SessionHub`/`UserState`, so both crates share one set of
  stores over one backend rather than opening the ruleset/counts keyspaces twice.

### Decisions made

- **`RulesetStore::set_ruleset`'s return type changed from `Result<(), StoreError>` to
  `Result<u64, StoreError>`** (the new change-seq). A breaking change to this crate's own trait,
  fully within `hs-push`'s ownership; the two call sites in `crate::routes::pushrules` needed no
  code change (the returned value is already discarded as a bare statement).
- **Push-rules change tracking is a per-user monotonic counter (`atomic_add` over one raw KV
  key), not a content hash or a timestamp** — mirrors `hs_user::store::tables`'s
  `account_data_counter` exactly (same primitive, same reasoning: cheap, monotonic, safe under
  concurrent writers via the existing transaction-retry machinery, no clock dependency).
- **A user who has never customized their ruleset has `changed_seq == 0` forever, not a
  seeded/materialized row.** Rejected the alternative of writing the default ruleset into storage
  the first time it's read (which would give every never-customized user an artificial "change" at
  first read/first sync): `0` cleanly means "never changed" for the overwhelming common case (most
  users never touch `/pushrules`), and correctly compares as "unchanged" against any baseline a
  client's token could carry once `push_rules_seq` starts at `0` for `SyncToken::initial()` too.
- **Counts need no seq/token seam**, unlike push rules — documented above under "Done" item 3.
  This was worth stating explicitly since the task framing ("fold them into the same interface if
  the same seam gap blocks them") suggested they might need one; they don't, because they're
  scoped per-room-already-in-the-response rather than a standalone top-level account-data event.
- **MSC3664/MSC4028/MSC3381 were not implemented this session**, despite being named in this
  track's brief. See "Next" for why each was left rather than guessed at: MSC3664 needs a new
  condition kind Ruma does not provide, MSC4028's rule is trivial to write but its purpose depends
  on OTK-count plumbing this session did not build, and MSC3381 needs a Cargo feature flip plus
  updating the new golden test. Recorded here rather than silently dropped, per this track's own
  brief naming them as owned.
- Fixed a stale doc comment on `rulesets::default_ruleset` that named `.m.rule.contains_user_name`
  as an example of a user-ID-dependent default rule — that rule was retired by MSC4210 and is not
  part of `ruma::push::Ruleset::server_default`'s actual output; replaced with a citation-backed
  comment (see "Done" item 3) instead of a wrong example.

### Shared dependencies added

None. This session used only what `hs-push` already depended on (`ruma`, `hs-kv`, `hs-tables`,
`serde_json`) — no `Cargo.toml` or root `[workspace.dependencies]` change.
