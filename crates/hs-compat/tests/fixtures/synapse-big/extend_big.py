"""Extends the room `populate_big.py` made to `--total` events, writing them straight into the
Synapse database the way Synapse would have persisted them (see README.md).

    <synapse venv>/bin/python extend_big.py '<libpq dsn of the Synapse database>' \
        <the server's signing key file> --total 100000

Why: Synapse itself persists a message into a room of 2,000 local members at a few events a
second on a busy machine (it works out push actions for every member, for every event), so
loading 100,000 events through its client API takes many hours. The room `populate_big.py`
made -- its members, joins and first few thousand messages -- is Synapse's own; this script adds
messages to it from those members, each built as Synapse builds one: hashed, signed with the
server's key and given its event id by Synapse's own functions (`synapse.crypto.event_signing`,
`synapse.events.make_event_from_dict`, imported from the installed Synapse, not copied), citing
the room's forward extremities as `prev_events` (sometimes only one of them, so the history
forks and merges as a busy room's does) and the create, power-levels and sender's membership
events as `auth_events`. They are written to `events`, `event_json` and `event_edges`, and
`event_forward_extremities` and the stream-ordering sequence are moved on. What the importer
does not read (push actions, room statistics) is not filled in: the database is for measuring
the importer, not for running Synapse again.
"""
import argparse
import json
import random
import time

import psycopg2
from signedjson.key import read_signing_keys
from synapse.api.room_versions import KNOWN_ROOM_VERSIONS
from synapse.crypto.event_signing import add_hashes_and_signatures
from synapse.events import make_event_from_dict

ARGS = argparse.ArgumentParser()
ARGS.add_argument("dsn")
ARGS.add_argument("signing_key")
ARGS.add_argument("--total", type=int, default=100_000)
ARGS.add_argument("--room", default=None)
OPTS = ARGS.parse_args()

WORDS = ("the quick brown fox jumps over a lazy dog while the migration copies every event of "
         "this rather large room into a server written in rust and nobody notices anything at all "
         "except perhaps that the numbers in the log are bigger than usual").split()


def main():
    conn = psycopg2.connect(OPTS.dsn)
    cur = conn.cursor()
    if OPTS.room:
        room_id = OPTS.room
    else:
        cur.execute("SELECT room_id FROM events GROUP BY room_id ORDER BY count(*) DESC LIMIT 1")
        room_id = cur.fetchone()[0]
    cur.execute("SELECT room_version FROM rooms WHERE room_id = %s", (room_id,))
    version = KNOWN_ROOM_VERSIONS[cur.fetchone()[0]]
    server_name = room_id.split(":", 1)[1]
    with open(OPTS.signing_key) as f:
        key = read_signing_keys(f)[0]

    cur.execute("SELECT count(*) FROM events WHERE room_id = %s", (room_id,))
    have = cur.fetchone()[0]
    todo = OPTS.total - have
    print(f"{room_id} (version {version.identifier}) has {have} events; adding {max(todo, 0)}", flush=True)
    if todo <= 0:
        return

    cur.execute("SELECT type, state_key, event_id FROM current_state_events WHERE room_id = %s", (room_id,))
    state = {(t, k): e for t, k, e in cur.fetchall()}
    cur.execute(
        "SELECT state_key FROM current_state_events WHERE room_id = %s AND type = 'm.room.member' "
        "AND membership = 'join'", (room_id,))
    members = sorted(r[0] for r in cur.fetchall())
    cur.execute(
        "SELECT x.event_id, e.depth FROM event_forward_extremities x JOIN events e USING (event_id) "
        "WHERE x.room_id = %s", (room_id,))
    heads = dict(cur.fetchall())
    cur.execute("SELECT max(stream_ordering), max(origin_server_ts) FROM events")
    stream, ts = cur.fetchone()
    rng = random.Random(7)
    started = time.time()
    rows_events, rows_json, rows_edges = [], [], []

    def flush():
        cur.executemany(
            "INSERT INTO events (topological_ordering, event_id, type, room_id, processed, outlier, "
            "depth, origin_server_ts, received_ts, sender, contains_url, instance_name, stream_ordering) "
            "VALUES (%s, %s, 'm.room.message', %s, true, false, %s, %s, %s, %s, false, 'master', %s)",
            rows_events)
        cur.executemany(
            "INSERT INTO event_json (event_id, room_id, internal_metadata, json, format_version) "
            "VALUES (%s, %s, '{}', %s, 3)", rows_json)
        cur.executemany(
            "INSERT INTO event_edges (event_id, prev_event_id) VALUES (%s, %s)", rows_edges)
        rows_events.clear(), rows_json.clear(), rows_edges.clear()

    for i in range(todo):
        sender = members[rng.randrange(len(members))]
        newest = sorted(heads, key=lambda h: (-heads[h], h))[:10]  # as Synapse, at most ten
        if len(newest) > 1 and rng.random() < 0.9:
            prev = sorted(newest)  # merge the forks
        else:
            prev = [rng.choice(newest)]  # or carry on one branch, leaving a fork
        depth = max(heads[p] for p in prev) + 1
        ts += rng.randint(1, 3000)
        body = " ".join(rng.choice(WORDS) for _ in range(rng.randint(4, 40))).capitalize() + "."
        event = {
            "type": "m.room.message", "room_id": room_id, "sender": sender,
            "content": {"msgtype": "m.text", "body": body},
            "prev_events": prev,
            "auth_events": [state[("m.room.create", "")], state[("m.room.power_levels", "")],
                            state[("m.room.member", sender)]],
            "depth": depth, "origin_server_ts": ts,
        }
        add_hashes_and_signatures(version, event, server_name, key)
        event_id = make_event_from_dict(event, version).event_id
        stream += 1
        stored = dict(event, unsigned={"age_ts": ts})
        rows_events.append((depth, event_id, room_id, depth, ts, ts, sender, stream))
        rows_json.append((event_id, room_id, json.dumps(stored, separators=(",", ":"))))
        rows_edges.extend((event_id, p) for p in prev)
        for p in prev:
            heads.pop(p, None)
        heads[event_id] = depth
        while len(heads) > 5:
            heads.pop(min(heads, key=heads.get))
        if len(rows_events) >= 2000:
            flush()
        if (i + 1) % 10000 == 0:
            print(f"{i + 1} events after {time.time() - started:.0f}s", flush=True)
    flush()
    cur.execute("DELETE FROM event_forward_extremities WHERE room_id = %s", (room_id,))
    cur.executemany("INSERT INTO event_forward_extremities (event_id, room_id) VALUES (%s, %s)",
                    [(h, room_id) for h in heads])
    cur.execute("SELECT setval('events_stream_seq', %s)", (stream,))
    conn.commit()
    print(f"done: {todo} events added in {time.time() - started:.0f}s", flush=True)


main()
