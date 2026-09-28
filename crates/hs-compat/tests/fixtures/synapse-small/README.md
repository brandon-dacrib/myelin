# `synapse-small`: a real Synapse database, for the importer

What a Synapse 1.161 on PostgreSQL held after `populate.py` drove it through its own client
API: four accounts (`alice` an administrator, `dave` deactivated), each signed in on two devices;
a public room `#lobby:fixture.test` with a topic change, an edit, a redaction, an image, a member
who joined and left, and a read receipt; a direct chat between bob and alice; global account
data (`m.direct`, a custom type) and a room tag; two uploads, one of them alice's avatar.

- `schema.sql`: only the tables and columns the importer reads, written for this fixture from
  the column names in `docs/compat/synapse-importer-mapping.md`. It is not Synapse's schema.
- `data.sql`: those rows, as Synapse wrote them (`export.py`).
- `media_store/`: Synapse's `media_store_path` (`local_content`, `local_thumbnails`).
- `signing.key`: the fixture server's signing key (a test key, used nowhere else). A migrated
  server keeps signing with it, so the events Synapse signed stay verifiable.
- `facts.json`: ids the tests need (the two rooms, alice's and bob's access tokens, the media).

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
# 127.0.0.1:18099, `registration_shared_secret`, and generous `rc_*` rate limits.
/tmp/synapse-venv/bin/python -m synapse.app.homeserver --config-path homeserver.yaml &
/tmp/synapse-venv/bin/python populate.py http://127.0.0.1:18099 homeserver.yaml facts.json
kill %1
/tmp/synapse-venv/bin/python export.py 'dbname=synapse_fixture host=127.0.0.1 port=5439 user=postgres password=hspg'
```

Then copy the new `media_store/`, `fixture.test.signing.key` (as `signing.key`) and `facts.json`
here. The tests assert the counts above; a fixture with other content needs them changed.
