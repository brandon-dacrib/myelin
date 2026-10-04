# `synapse-small`: a real Synapse database, for the importer

What a Synapse 1.161 on PostgreSQL held after `populate.py` drove it through its own client
API: four accounts (`alice` an administrator, `dave` deactivated), each signed in on two devices;
a public room `#lobby:fixture.test` with a topic change, an edit, a redaction, an image, a member
who joined and left, and a read receipt; a direct chat between bob and alice, with alice's
private read receipt; global account data (`m.direct`, a custom type) and a room tag; two
uploads, one of them alice's avatar. And what a migrating user's clients leave behind:
end-to-end keys for alice's phone (five one-time keys and a fallback key) and bob's laptop
(three), signed by real ed25519 keys as a client signs them; cross-signing for both, alice's
self-signing key signing her phone and her user-signing key signing bob's master key; a key
backup whose first version was deleted and whose second holds three room keys; alice's push
rules (a keyword, a muted room, an override of her own, a server-default rule turned off and one
with changed actions) and a pusher; a sync filter each for alice and bob.

- `schema.sql`: only the tables and columns the importer reads, written for this fixture from
  the column names in `docs/compat/synapse-importer-mapping.md`. It is not Synapse's schema.
- `data.sql`: those rows, as Synapse wrote them (`export.py`).
- `media_store/`: Synapse's `media_store_path` (`local_content`, `local_thumbnails`).
- `signing.key`: the fixture server's signing key (a test key, used nowhere else). A migrated
  server keeps signing with it, so the events Synapse signed stay verifiable.
- `facts.json`: ids the tests need (the two rooms, alice's and bob's access tokens, the media,
  the receipted events, the filter ids, the backup version, the cross-signing keys).

Regenerated on 2026-10-01 with the end-to-end keys, push rules, pushers, receipts and filters
(the earlier one lacked them); the counts the tests assert are unchanged.

Added by hand on 2026-10-04, from Synapse's schema (`synapse/storage/schema/main/full_schemas/
72/full.sql.postgres` and its later deltas; no Synapse was run): `remote_media_cache` and
`remote_media_cache_thumbnails`, with two entries of a server `other.test` that never existed:
`RemoteCachedPictureOne` (a 2x2 PNG, under `media_store/remote_content/other.test/Re/mo/`, with
a thumbnail row) and `RemoteMissingFileTwo`, whose file is gone, as Synapse's cache eviction
leaves the row. `facts.json` names both (`remote_picture`, `remote_missing`). `export.py` keeps
both tables on a regeneration; a Synapse that had fetched another server's media writes the
same rows.

Used by `crates/hs-compat/tests/migration.rs` (the engine, against an in-memory target) and
`crates/hs-cli/tests/migration.rs` (the real `hs` binary, through the admin API), which load
`schema.sql` and `data.sql` into a fresh PostgreSQL database each.

## Regenerating

Synapse is only run, never copied (it is AGPL-3.0). With a PostgreSQL at 127.0.0.1:5439
(`docker run --rm -d --name hs-mig-pg -e POSTGRES_PASSWORD=hspg -p 127.0.0.1:5439:5432 postgres:17`):

```sh
uv venv --python 3.12 /tmp/synapse-venv
VIRTUAL_ENV=/tmp/synapse-venv uv pip install matrix-synapse psycopg2-binary
docker exec hs-mig-pg psql -U postgres -c "CREATE DATABASE synapse_fixture ENCODING 'UTF8' LC_COLLATE='C' LC_CTYPE='C' TEMPLATE template0"
cd /tmp/synapse-fixture
/tmp/synapse-venv/bin/python -m synapse.app.homeserver --server-name fixture.test \
  --config-path homeserver.yaml --generate-config --report-stats=no
# In homeserver.yaml: a psycopg2 `database` pointing at synapse_fixture, one client listener on
# 127.0.0.1:18099, `registration_shared_secret`, and generous `rc_*` rate limits. Copy this
# directory's signing.key over the generated one, so that the fixture keeps its key.
/tmp/synapse-venv/bin/python -m synapse.app.homeserver --config-path homeserver.yaml &
/tmp/synapse-venv/bin/python populate.py http://127.0.0.1:18099 homeserver.yaml facts.json
kill %1
/tmp/synapse-venv/bin/python export.py 'dbname=synapse_fixture host=127.0.0.1 port=5439 user=postgres password=hspg'
```

Then copy the new `media_store/`, `fixture.test.signing.key` (as `signing.key`) and `facts.json`
here. The tests assert the counts above; a fixture with other content needs them changed.
