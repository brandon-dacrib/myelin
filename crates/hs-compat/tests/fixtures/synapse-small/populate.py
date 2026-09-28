"""Populates a real Synapse with a small, varied history for the importer fixture (see README.md).

    <synapse venv>/bin/python populate.py <synapse base url> <synapse homeserver.yaml> <facts.json to write>
"""
import json
import struct
import subprocess
import sys
import time
import urllib.request
import zlib

import os

BASE = sys.argv[1]
CONFIG = sys.argv[2]
FACTS = sys.argv[3]
REGISTER = os.path.join(os.path.dirname(sys.executable), "register_new_matrix_user")


def call(method, path, token=None, body=None, raw=None, content_type="application/json"):
    data = None
    headers = {}
    if body is not None:
        data = json.dumps(body).encode()
        headers["content-type"] = "application/json"
    if raw is not None:
        data = raw
        headers["content-type"] = content_type
    if token:
        headers["authorization"] = "Bearer " + token
    req = urllib.request.Request(BASE + path, data=data, method=method, headers=headers)
    try:
        with urllib.request.urlopen(req) as resp:
            text = resp.read()
            return json.loads(text) if text else {}
    except urllib.error.HTTPError as e:
        print(method, path, e.code, e.read().decode(), file=sys.stderr)
        raise


def register(user, password, admin=False):
    args = [REGISTER, "-u", user, "-p", password, "-c", CONFIG, BASE]
    args.append("-a" if admin else "--no-admin")
    subprocess.run(args, check=True, capture_output=True)


def login(user, password, device):
    r = call("POST", "/_matrix/client/v3/login", body={
        "type": "m.login.password",
        "identifier": {"type": "m.id.user", "user": user},
        "password": password,
        "device_id": device,
        "initial_device_display_name": device.title(),
    })
    return r["access_token"]


def png(width, height, rgb):
    raw = b"".join(b"\x00" + bytes(rgb) * width for _ in range(height))
    def chunk(kind, payload):
        c = struct.pack(">I", len(payload)) + kind + payload
        return c + struct.pack(">I", zlib.crc32(kind + payload) & 0xFFFFFFFF)
    return (b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", struct.pack(">IIBBBBB", width, height, 8, 2, 0, 0, 0))
            + chunk(b"IDAT", zlib.compress(raw)) + chunk(b"IEND", b""))


txn = [0]


def send(token, room, content, kind="m.room.message"):
    txn[0] += 1
    return call("PUT", f"/_matrix/client/v3/rooms/{room}/send/{kind}/t{txn[0]}", token, content)["event_id"]


def main():
    register("alice", "alice-password-1", admin=True)
    register("bob", "bob-password-1")
    register("carol", "carol-password-1")
    register("dave", "dave-password-1")
    alice = login("alice", "alice-password-1", "ALICEPHONE")
    bob = login("bob", "bob-password-1", "BOBLAPTOP")
    carol = login("carol", "carol-password-1", "CAROLDESK")
    dave = login("dave", "dave-password-1", "DAVEPHONE")

    call("PUT", "/_matrix/client/v3/profile/@alice:fixture.test/displayname", alice, {"displayname": "Alice Liddell"})
    call("PUT", "/_matrix/client/v3/profile/@bob:fixture.test/displayname", bob, {"displayname": "Bob"})
    avatar = call("POST", "/_matrix/media/v3/upload?filename=alice.png", alice, raw=png(8, 8, (200, 30, 30)), content_type="image/png")
    call("PUT", "/_matrix/client/v3/profile/@alice:fixture.test/avatar_url", alice, {"avatar_url": avatar["content_uri"]})

    lobby = call("POST", "/_matrix/client/v3/createRoom", alice, {
        "preset": "public_chat", "name": "Lobby", "topic": "Where everyone starts",
        "room_alias_name": "lobby", "visibility": "public",
    })["room_id"]
    call("POST", f"/_matrix/client/v3/rooms/{lobby}/invite", alice, {"user_id": "@carol:fixture.test"})
    call("POST", "/_matrix/client/v3/join/%23lobby:fixture.test", bob, {})
    first = send(alice, lobby, {"msgtype": "m.text", "body": "Welcome to the lobby"})
    send(bob, lobby, {"msgtype": "m.text", "body": "Hello, Alice"})
    typo = send(alice, lobby, {"msgtype": "m.text", "body": "We moved from Synpase"})
    send(alice, lobby, {"msgtype": "m.text", "body": "* We moved from Synapse",
                        "m.new_content": {"msgtype": "m.text", "body": "We moved from Synapse"},
                        "m.relates_to": {"rel_type": "m.replace", "event_id": typo}})
    oops = send(bob, lobby, {"msgtype": "m.text", "body": "this one gets redacted"})
    call("PUT", f"/_matrix/client/v3/rooms/{lobby}/redact/{oops}/r1", alice, {"reason": "fixture"})
    picture = call("POST", "/_matrix/media/v3/upload?filename=red.png", bob, raw=png(16, 12, (10, 120, 200)), content_type="image/png")
    send(bob, lobby, {"msgtype": "m.image", "body": "red.png", "url": picture["content_uri"],
                      "info": {"mimetype": "image/png", "w": 16, "h": 12}})
    call("POST", f"/_matrix/client/v3/rooms/{lobby}/join", carol, {})
    send(carol, lobby, {"msgtype": "m.text", "body": "Carol was here"})
    call("POST", f"/_matrix/client/v3/rooms/{lobby}/leave", carol, {})
    call("PUT", f"/_matrix/client/v3/rooms/{lobby}/state/m.room.topic", alice, {"topic": "Where everyone starts, and stays"})
    for i in range(12):
        send(alice if i % 2 == 0 else bob, lobby, {"msgtype": "m.text", "body": f"Message number {i + 1}"})
    call("POST", f"/_matrix/client/v3/rooms/{lobby}/receipt/m.read/{first}", bob, {})

    dm = call("POST", "/_matrix/client/v3/createRoom", bob, {
        "preset": "trusted_private_chat", "is_direct": True, "invite": ["@alice:fixture.test"],
    })["room_id"]
    call("POST", f"/_matrix/client/v3/rooms/{dm}/join", alice, {})
    send(bob, dm, {"msgtype": "m.text", "body": "A private word"})
    send(alice, dm, {"msgtype": "m.text", "body": "A private reply"})

    call("PUT", "/_matrix/client/v3/user/@alice:fixture.test/account_data/m.direct", alice, {"@bob:fixture.test": [dm]})
    call("PUT", "/_matrix/client/v3/user/@alice:fixture.test/account_data/fixture.test.note", alice, {"note": "kept across the migration"})
    call("PUT", f"/_matrix/client/v3/user/@alice:fixture.test/rooms/{lobby}/tags/m.favourite", alice, {"order": 0.5})

    # A deactivated account, and an administrator-created one.
    call("POST", "/_synapse/admin/v1/deactivate/@dave:fixture.test", alice, {"erase": False})

    json.dump({"lobby": lobby, "dm": dm, "alice_token": alice, "bob_token": bob,
               "avatar": avatar["content_uri"], "picture": picture["content_uri"]},
              open(FACTS, "w"), indent=2)
    print("ok", lobby, dm)


main()
