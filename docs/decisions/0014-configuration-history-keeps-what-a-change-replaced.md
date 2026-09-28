# 0014. Configuration history keeps what a change replaced, secrets included, and never serves it

Date: 2026-09-28. Tracks: 13 (`hs-config`), 15 (`hs-admin`), 16 (web). Status: accepted.

## Context

`ConfigStore` recorded one `ChangeRecord` per write, holding only the merge patch. That says
what a change *wrote*, not what it *replaced*, so neither "Login rate limit: 5 → 10" nor an
automatic revert was possible. The store cannot work it out afterwards either: the seed writes
revision 1 with no history record, so replaying patches does not recover a starting point.

## Decision

1. **Every write records what the database held at each setting it touched**
   (`ChangeRecord::before`, a map from section-relative JSON Pointer to value, `null` where the
   database held nothing). It is computed inside the write's own transaction
   (`hs_config::history::before_values`). Where a patch writes beneath a value that is not an
   object, that whole value is recorded at its own pointer so a revert gives it back. Records
   written before this change have no `before`. They are listed with `from: null` and
   `revertible: false`, and a revert of one is `409` with the reason. Nothing guesses.
2. **A revert is an ordinary write.** `ConfigStore::revert_plan` computes the merge patch that
   puts every touched setting back, against what is stored now, plus the later changes to the
   same settings (`ChangeRecord::reverts` names the reverted revision).
   `ConfigStore::apply_revert` writes that patch only over the revision it was planned at. The
   caller validates, checks bootstrap and environment pins, audits and publishes it exactly as
   it does `config.update`.
3. **A later change to the same setting blocks a revert unless the caller forces it**
   (`409 conflict`; each overlapping setting is in `errors[]` with the revision, actor and time
   that wrote it). Settings the change did not touch are never changed, even when forced.
4. **Secrets.** The store keeps a secret's earlier value in `before`, because restoring a rotated
   secret is what a revert is for, and the server can do it without the value ever crossing the
   wire. The admin API redacts history exactly as it redacts values: both sides of a secret row
   and the patch are `{"$secret": true}`, and the row is marked `secret`. The trade-off is that a
   rotated-away secret stays in the configuration keyspace for as long as history does. That is
   the same database that holds the current secret, and it is readable only by someone who can
   already read that.
5. The audit action is the operation id, `config.history.revert`, as it is for every other
   audited operation. The event is `config.reverted`.

## Consequences

- `GET /config/{section}/history` (`config.history.list`) and
  `POST /config/{section}/history/{revision}/revert` (`config.history.revert`) are in the
  contract. `ConfigChange` gained `settings`, `reverts` and `revertible`, all additive.
- `ConfigSource` gained `history_page` and `revert`. Both have defaults that answer `503`, so an
  older implementation compiles and says honestly that it cannot do this.
- History is not pruned. Configuration writes are rare, and pruning would need a retention
  setting.
