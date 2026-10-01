# `synapse-federated`: a real Synapse that joined rooms over federation, for the importer

Two Synapse 1.161 on PostgreSQL federating with each other on one machine: `127.0.0.1:18301`
("home") and `127.0.0.1:18302` ("remote"). `populate.py` drove both through their own client
APIs: rita, of the remote server, made two public rooms and talked in them; hana and then hugo,
of the home server, joined each over federation; the three of them talked, rita changed the topic,
and hana left a read receipt on rita's last message.

- **Elsewhere**: hana read back to its beginning, so the home server backfilled its whole
  history, its `m.room.create` included. The importer replays it from the create event, like a
  room made at home.
- **Faraway**: nobody read back, so the home server holds it from hana's join on: its create
  event and earlier state only as outliers (what the remote server's `send_join` answered), its
  earlier messages not at all. The importer starts it from that join, as the join started it in
  Synapse.

Only the home server's database is the fixture:

- `schema.sql`: `../synapse-small/schema.sql` with the state tables the importer reads for a room
  joined over federation (`event_to_state_groups`, `state_groups_state`, `state_group_edges`,
  `partial_state_rooms`). Written for this fixture from column names; not Synapse's schema.
- `data.sql`: those rows, as Synapse wrote them (`../synapse-small/export.py`).
- `signing.key`: the home server's signing key (a test key, used nowhere else).
- `facts.json`: the two rooms, hana's and hugo's access tokens, each room's last event.

Used by `crates/hs-compat/tests/migration.rs`
(`rooms_joined_over_federation_are_held_from_the_join_and_verified`) and
`crates/hs-cli/tests/migration.rs` (`rooms_joined_over_federation_are_migrated_and_served`),
which import it into a server named `127.0.0.1:18301`.

## Regenerating

Synapse is only run, never copied (it is AGPL-3.0). With a PostgreSQL at 127.0.0.1:5491 and the
venv of `../synapse-small/README.md`:

```sh
# Two databases, synapse_fed_home and synapse_fed_remote (UTF8, C collation, template0).
# A self-signed certificate for 127.0.0.1 (federation is TLS only):
openssl req -x509 -newkey rsa:2048 -nodes -keyout tls.key -out tls.crt -days 30 \
  -subj "/CN=127.0.0.1" -addext "subjectAltName=IP:127.0.0.1"
# For each server, `--generate-config` with its server name, then in homeserver.yaml: a client
# listener (18311, 18312), a TLS federation listener on the server name's port with resources
# [federation, keys], tls_certificate_path/tls_private_key_path, federation_verify_certificates:
# false, ip_range_blacklist: [] and ip_range_whitelist: ['127.0.0.1'] (Synapse refuses private
# addresses by default), trusted_key_servers: [], registration_shared_secret, generous rc_*.
python -m synapse.app.homeserver --config-path remote/homeserver.yaml &
python -m synapse.app.homeserver --config-path home/homeserver.yaml &
python populate.py http://127.0.0.1:18311 http://127.0.0.1:18312 <shared secret> facts.json
kill %1 %2
python ../synapse-small/export.py 'dbname=synapse_fed_home host=127.0.0.1 port=5491 user=postgres password=hspg' .
```

Then copy the home server's `*.signing.key` here as `signing.key`.
