"""Populates two real Synapses that federate, so that one of them holds rooms it joined over
federation, for the importer fixture (see README.md).

    <synapse venv>/bin/python populate.py <home client url> <remote client url> <shared secret> <facts.json to write>

The remote server (`127.0.0.1:18302`) makes two public rooms and talks in them; then two
accounts of the home server (`127.0.0.1:18301`) join each over federation, one after the other,
and the three of them talk, change the topic and leave a read receipt. In "Elsewhere" hana reads
back to the beginning, so the home server backfills the room's whole history, its create event
included; in "Faraway" nobody reads back, so the home server holds the room only from hana's
join on (its earlier history and its create event only as outliers, or not at all). Only the
home server's database is the fixture.
"""
import hashlib
import hmac
import json
import sys
import time
import urllib.error
import urllib.parse
import urllib.request

HOME, REMOTE, SECRET, FACTS = sys.argv[1:5]
REMOTE_NAME = "127.0.0.1:18302"


def call(base, method, path, token=None, body=None):
    data = json.dumps(body).encode() if body is not None else None
    headers = {"content-type": "application/json"} if body is not None else {}
    if token:
        headers["authorization"] = "Bearer " + token
    req = urllib.request.Request(base + path, data=data, method=method, headers=headers)
    try:
        with urllib.request.urlopen(req, timeout=120) as resp:
            text = resp.read()
            return json.loads(text) if text else {}
    except urllib.error.HTTPError as e:
        raise RuntimeError(f"{method} {base}{path}: {e.code} {e.read().decode()}") from None


def register(base, localpart):
    nonce = call(base, "GET", "/_synapse/admin/v1/register")["nonce"]
    password = localpart + "-password-1"
    mac = hmac.new(SECRET.encode(), digestmod=hashlib.sha1)
    mac.update(b"\x00".join([nonce.encode(), localpart.encode(), password.encode(), b"notadmin"]))
    return call(base, "POST", "/_synapse/admin/v1/register", body={
        "nonce": nonce, "username": localpart, "password": password, "admin": False,
        "mac": mac.hexdigest(), "device_id": localpart.upper() + "DEVICE",
    })["access_token"]


txn = [0]


def send(base, token, room, body):
    txn[0] += 1
    return call(base, "PUT", f"/_matrix/client/v3/rooms/{urllib.parse.quote(room)}/send/m.room.message/f{txn[0]}",
                token, {"msgtype": "m.text", "body": body})["event_id"]


def arrived(base, token, room, body, timeout=120):
    """Until `body` reaches `room` on `base`, watched through `/sync` (which never backfills)."""
    deadline = time.time() + timeout
    since = None
    filter_ = urllib.parse.quote(json.dumps({"room": {"timeline": {"limit": 50}}}))
    while time.time() < deadline:
        path = f"/_matrix/client/v3/sync?timeout=2000&filter={filter_}" + (f"&since={since}" if since else "")
        sync = call(base, "GET", path, token)
        since = sync["next_batch"]
        timeline = sync.get("rooms", {}).get("join", {}).get(room, {}).get("timeline", {}).get("events", [])
        if any(e.get("content", {}).get("body") == body for e in timeline):
            return
    raise RuntimeError(f"{body!r} never reached {base}")


def read_back(base, token, room):
    """Pages back through `room` to its beginning, which makes `base` backfill it."""
    path = f"/_matrix/client/v3/rooms/{urllib.parse.quote(room)}/messages?dir=b&limit=100"
    for _ in range(5):
        page = call(base, "GET", path, token)
        if any(e.get("type") == "m.room.create" for e in page.get("chunk", [])):
            return
        time.sleep(1)
    raise RuntimeError(f"{room} was never backfilled to its beginning")


def main():
    rita = register(REMOTE, "rita")
    hana = register(HOME, "hana")
    hugo = register(HOME, "hugo")
    via = urllib.parse.quote(REMOTE_NAME)
    facts = {"hana_token": hana, "hugo_token": hugo}
    for name, read in [("elsewhere", True), ("faraway", False)]:
        room = call(REMOTE, "POST", "/_matrix/client/v3/createRoom", rita, {
            "preset": "public_chat", "name": name.title(), "topic": f"{name.title()}, on another server",
            "room_alias_name": name,
        })["room_id"]
        for i in range(5):
            send(REMOTE, rita, room, f"{name}: before anyone from home joined, {i + 1}")
        q = urllib.parse.quote(room)
        call(HOME, "POST", f"/_matrix/client/v3/join/{q}?server_name={via}", hana, {})
        send(REMOTE, rita, room, f"{name}: welcome, hana")
        arrived(HOME, hana, room, f"{name}: welcome, hana")
        send(HOME, hana, room, f"{name}: hello from home")
        call(REMOTE, "PUT", f"/_matrix/client/v3/rooms/{q}/state/m.room.topic", rita,
             {"topic": f"{name.title()}, on another server, with guests"})
        call(HOME, "POST", f"/_matrix/client/v3/join/{q}?server_name={via}", hugo, {})
        send(HOME, hugo, room, f"{name}: hugo here too")
        last = send(REMOTE, rita, room, f"{name}: the last word, from elsewhere")
        arrived(HOME, hana, room, f"{name}: the last word, from elsewhere")
        call(HOME, "POST", f"/_matrix/client/v3/rooms/{q}/receipt/m.read/{urllib.parse.quote(last)}", hana, {})
        if read:
            read_back(HOME, hana, room)
        facts[name] = room
        facts[f"{name}_last"] = last
    # Let partial-state joins finish resyncing before the servers are stopped.
    time.sleep(10)
    json.dump(facts, open(FACTS, "w"), indent=2)
    print("ok", facts["elsewhere"], facts["faraway"])


main()
