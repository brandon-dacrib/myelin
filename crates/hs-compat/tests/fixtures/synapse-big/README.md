# `synapse-big`: a Synapse with one large room, for measuring the importer

Not loaded by any test: it is how the importer's throughput and memory were measured
(`docs/status/13-config-compat-and-migration.md`, 2026-10-01), and how to measure them again.
The database itself is not checked in (about 100,000 events).

1. **A real Synapse 1.161** (`big.test`, PostgreSQL, `bcrypt_rounds: 4`, every `rc_*` limit
   lifted including `rc_joins_per_room`), loaded through its own APIs by `populate_big.py`:
   2,000 accounts (shared-secret registration), all joined to one public room, then messages
   (with reactions, replies and edits) from them sent 16 at a time.

   ```sh
   python populate_big.py http://127.0.0.1:18199 <shared secret> --members 2000 --events 100000
   ```

   On a busy machine Synapse persists a message into a room of 2,000 local members at a few
   events a second (it works out push actions for every member for every event), so this was
   stopped once the 2,000 joins and the first few thousand messages were in.
2. **`extend_big.py`** then brings the room to 100,000 events by writing messages straight into
   Synapse's tables as Synapse persists them: each built, hashed, signed with the server's key
   and given its event id by Synapse's own functions (imported from the installed Synapse, not
   copied), sent by the room's real members, citing up to ten of the room's forward extremities
   (sometimes only one, so the history forks and merges) and the create, power-levels and
   sender's membership events.

   ```sh
   python extend_big.py 'dbname=synapse_big host=127.0.0.1 port=5491 user=postgres password=hspg' \
       big.test.signing.key --total 100000
   ```
3. **`measure.py`** boots an `hs` binary over an empty data directory, points its migration at
   the database, copies it, and prints how long it took, each stream's counts and rate, the peak
   resident memory (sampled with `ps`), and the throughput lines the importer logged.

   ```sh
   python3 measure.py target/release/hs big.test big.test.signing.key --db-name synapse_big
   ```
