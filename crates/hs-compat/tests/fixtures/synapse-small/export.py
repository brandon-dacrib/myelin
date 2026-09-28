"""Writes data.sql: the rows of a populated Synapse database that the importer reads, as
column-named INSERT statements for the tables and columns schema.sql declares.

    <synapse venv>/bin/python export.py 'dbname=synapse_fixture host=127.0.0.1 port=5439 user=postgres password=hspg'

Run it against a Synapse that populate.py has filled and then stopped (see README.md).
"""
import json
import re
import sys
from pathlib import Path

import psycopg2

HERE = Path(__file__).parent
ORDER = {
    "users": "name",
    "profiles": "user_id",
    "devices": "user_id, device_id",
    "access_tokens": "id",
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
