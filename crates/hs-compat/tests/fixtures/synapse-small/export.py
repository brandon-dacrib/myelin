"""Writes data.sql: the rows of a populated Synapse database that the importer reads, as
column-named INSERT statements for the tables and columns schema.sql declares.

    <synapse venv>/bin/python export.py 'dbname=synapse_fixture host=127.0.0.1 port=5439 user=postgres password=hspg' [fixture directory]

Run it against a Synapse that populate.py has filled and then stopped (see README.md). The
fixture directory (the one holding schema.sql, where data.sql is written) is this one unless
given: `../synapse-federated` uses this script too.
"""
import json
import re
import sys
from pathlib import Path

import psycopg2

HERE = Path(sys.argv[2]) if len(sys.argv) > 2 else Path(__file__).parent
ORDER = {
    "event_to_state_groups": "event_id",
    "state_groups_state": "state_group, type, state_key",
    "state_group_edges": "state_group, prev_state_group",
    "partial_state_rooms": "room_id",
    "users": "name",
    "profiles": "user_id",
    "devices": "user_id, device_id",
    "access_tokens": "id",
    "refresh_tokens": "id",
    "user_threepids": "user_id, medium, address",
    "user_external_ids": "user_id, auth_provider, external_id",
    "erased_users": "user_id",
    "device_inbox": "stream_id",
    "registration_tokens": "token",
    "account_data": "user_id, account_data_type",
    "room_account_data": "user_id, room_id, account_data_type",
    "room_tags": "user_id, room_id, tag",
    "rooms": "room_id",
    "room_aliases": "room_alias",
    "events": "stream_ordering",
    "event_json": "event_id",
    "rejections": "event_id",
    "redactions": "event_id",
    "current_state_events": "room_id, type, state_key",
    "local_media_repository": "media_id",
    "e2e_device_keys_json": "user_id, device_id",
    "e2e_one_time_keys_json": "user_id, device_id, algorithm, key_id",
    "e2e_fallback_keys_json": "user_id, device_id, algorithm",
    "e2e_cross_signing_keys": "user_id, keytype, stream_id",
    "e2e_cross_signing_signatures": "user_id, target_user_id, target_device_id",
    "e2e_room_keys_versions": "user_id, version",
    "e2e_room_keys": "user_id, version, room_id, session_id",
    "push_rules": "user_name, rule_id",
    "push_rules_enable": "user_name, rule_id",
    "pushers": "id",
    "receipts_linearized": "stream_id",
    "user_filters": "full_user_id, filter_id",
    "remote_media_cache": "media_origin, media_id",
    "remote_media_cache_thumbnails": "media_origin, media_id, thumbnail_width, thumbnail_height, thumbnail_type",
}


def tables():
    """(table, [columns]) in schema.sql's order."""
    schema = (HERE / "schema.sql").read_text()
    for match in re.finditer(r"CREATE TABLE (\w+) \((.*?)\n\);", schema, re.S):
        columns = []
        for line in match.group(2).splitlines():
            line = line.strip()
            if line and not line.startswith("PRIMARY KEY"):
                columns.append(line.split()[0])
        yield match.group(1), columns


def literal(value):
    if value is None:
        return "NULL"
    if isinstance(value, bool):
        return "true" if value else "false"
    if isinstance(value, int):
        return str(value)
    if isinstance(value, memoryview):
        return "'\\x" + bytes(value).hex() + "'"
    return "'" + str(value).replace("'", "''") + "'"


def main():
    conn = psycopg2.connect(sys.argv[1])
    out = ["-- Rows from a real Synapse 1.161 populated by populate.py; regenerate with export.py.\n"]
    with conn.cursor() as cur:
        for table, columns in tables():
            cur.execute(f"SELECT {', '.join(columns)} FROM {table} ORDER BY {ORDER[table]}")
            for row in cur.fetchall():
                out.append(
                    f"INSERT INTO {table} ({', '.join(columns)}) VALUES ({', '.join(literal(v) for v in row)});\n"
                )
    (HERE / "data.sql").write_text("".join(out))
    print(f"wrote {len(out) - 1} rows to data.sql")


main()
