"""Populates a real Synapse with a small, varied history for the importer fixture (see README.md).

    <synapse venv>/bin/python populate.py <synapse base url> <synapse homeserver.yaml> <facts.json to write>
"""
import base64
import copy
import json
import os
import struct
import subprocess
import sys
import time
import urllib.parse
import urllib.request
import zlib

# Both ship with Synapse; they sign the keys the clients here upload, as a real client would.
import nacl.public
from signedjson.key import generate_signing_key, get_verify_key, encode_verify_key_base64
from signedjson.sign import sign_json

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


def b64(raw):
    return base64.b64encode(raw).decode().rstrip("=")


def curve25519():
    return b64(bytes(nacl.public.PrivateKey.generate().public_key))


def ed25519(key_id):
    """A signing key whose id is `ed25519:<key_id>`, and its public half."""
    key = generate_signing_key(key_id)
    return key, encode_verify_key_base64(get_verify_key(key))


def upload_device_keys(token, user, device, otks=5):
    """What a client does on first sign-in: its identity keys, one-time keys and a fallback key."""
    signing, public = ed25519(device)
    keys = {
        "user_id": user, "device_id": device,
        "algorithms": ["m.olm.v1.curve25519-aes-sha2", "m.megolm.v1.aes-sha2"],
        "keys": {f"curve25519:{device}": curve25519(), f"ed25519:{device}": public},
    }
    sign_json(keys, user, signing)
    one_time = {}
    for i in range(otks):
        one_time[f"signed_curve25519:AAAAA{i}"] = sign_json({"key": curve25519()}, user, signing)
    fallback = {"signed_curve25519:AAAAFB": sign_json({"key": curve25519(), "fallback": True}, user, signing)}
    counts = call("POST", "/_matrix/client/v3/keys/upload", token, {
        "device_keys": keys, "one_time_keys": one_time, "fallback_keys": fallback,
    })
    assert counts["one_time_key_counts"]["signed_curve25519"] == otks, counts
    return keys


def uia(method, path, token, body, user, password):
    """`path` behind user-interactive auth: the first try names the session, the second signs in."""
    try:
        return call(method, path, token, body)
    except urllib.error.HTTPError as e:
        if e.code != 401:
            raise
    session = call_status(method, path, token, body)["session"]
    body = dict(body, auth={"type": "m.login.password", "session": session,
                            "identifier": {"type": "m.id.user", "user": user}, "password": password})
    return call(method, path, token, body)


def call_status(method, path, token, body):
    """The JSON body of a 401 (the user-interactive auth flows)."""
    data = json.dumps(body).encode()
    req = urllib.request.Request(BASE + path, data=data, method=method,
                                 headers={"content-type": "application/json", "authorization": "Bearer " + token})
    try:
        with urllib.request.urlopen(req) as resp:
            return json.loads(resp.read())
    except urllib.error.HTTPError as e:
        return json.loads(e.read())


def cross_signing(token, user, password):
    """A master key, and the self-signing and user-signing keys it signs."""
    master, master_pub = ed25519("master")
    ssk, ssk_pub = ed25519("ssk")
    usk, usk_pub = ed25519("usk")
    master, ssk, usk = rename(master, master_pub), rename(ssk, ssk_pub), rename(usk, usk_pub)
    master_key = {"user_id": user, "usage": ["master"], "keys": {f"ed25519:{master_pub}": master_pub}}
    self_signing = sign_json({"user_id": user, "usage": ["self_signing"],
                              "keys": {f"ed25519:{ssk_pub}": ssk_pub}}, user, master)
    user_signing = sign_json({"user_id": user, "usage": ["user_signing"],
                              "keys": {f"ed25519:{usk_pub}": usk_pub}}, user, master)
    uia("POST", "/_matrix/client/v3/keys/device_signing/upload", token, {
        "master_key": master_key, "self_signing_key": self_signing, "user_signing_key": user_signing,
    }, user.split(":")[0][1:], password)
    return {"master": (master, master_pub, master_key), "ssk": (ssk, ssk_pub), "usk": (usk, usk_pub)}


def rename(signing_key, public):
    """A signing key whose id is its own public key, as cross-signing keys are named."""
    signing_key.version = public
    return signing_key


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

    # End-to-end encryption: alice's and bob's devices upload their keys; both set up
    # cross-signing; alice's self-signing key signs her phone, and her user-signing key signs bob's
    # master key (she verified him). She backs up two room keys, after deleting a first backup.
    alice_keys = upload_device_keys(alice, "@alice:fixture.test", "ALICEPHONE")
    upload_device_keys(bob, "@bob:fixture.test", "BOBLAPTOP", otks=3)
    alice_xs = cross_signing(alice, "@alice:fixture.test", "alice-password-1")
    bob_xs = cross_signing(bob, "@bob:fixture.test", "bob-password-1")
    signed_phone = sign_json(copy.deepcopy(alice_keys), "@alice:fixture.test", alice_xs["ssk"][0])
    signed_bob = sign_json(copy.deepcopy(bob_xs["master"][2]), "@alice:fixture.test", alice_xs["usk"][0])
    failures = call("POST", "/_matrix/client/v3/keys/signatures/upload", alice, {
        "@alice:fixture.test": {"ALICEPHONE": signed_phone},
        "@bob:fixture.test": {bob_xs["master"][1]: signed_bob},
    })
    assert not failures.get("failures"), failures
    backup_auth = {"public_key": curve25519()}
    discarded = call("POST", "/_matrix/client/v3/room_keys/version", alice, {
        "algorithm": "m.megolm_backup.v1.curve25519-aes-sha2", "auth_data": backup_auth})["version"]
    call("DELETE", f"/_matrix/client/v3/room_keys/version/{discarded}", alice)
    backup = call("POST", "/_matrix/client/v3/room_keys/version", alice, {
        "algorithm": "m.megolm_backup.v1.curve25519-aes-sha2",
        "auth_data": sign_json(dict(backup_auth), "@alice:fixture.test", alice_xs["master"][0])})["version"]
    sessions = {}
    for i, room in enumerate([lobby, lobby, dm]):
        sessions.setdefault(room, {"sessions": {}})["sessions"][f"session{i}" + "x" * 20] = {
            "first_message_index": i, "forwarded_count": 0, "is_verified": i == 0,
            "session_data": {"ephemeral": curve25519(), "ciphertext": b64(os.urandom(48)), "mac": b64(os.urandom(8))},
        }
    call("PUT", f"/_matrix/client/v3/room_keys/keys?version={backup}", alice, {"rooms": sessions})

    # Push rules: a keyword, a muted room, a custom override, a base rule disabled and a base
    # rule's actions changed. And a pusher for alice's phone.
    call("PUT", "/_matrix/client/v3/pushrules/global/content/lobbyword", alice,
         {"pattern": "lobby", "actions": ["notify", {"set_tweak": "highlight"}]})
    call("PUT", f"/_matrix/client/v3/pushrules/global/room/{urllib.parse.quote(dm)}", alice, {"actions": []})
    call("PUT", "/_matrix/client/v3/pushrules/global/override/fixture.quiet_bots", alice, {
        "conditions": [{"kind": "event_match", "key": "sender", "pattern": "@bot*"}], "actions": []})
    call("PUT", "/_matrix/client/v3/pushrules/global/override/.m.rule.suppress_notices/enabled", alice, {"enabled": False})
    call("PUT", "/_matrix/client/v3/pushrules/global/underride/.m.rule.message/actions", alice,
         {"actions": ["notify", {"set_tweak": "sound", "value": "default"}]})
    call("POST", "/_matrix/client/v3/pushers/set", alice, {
        "kind": "http", "app_id": "org.example.fixture", "app_display_name": "Fixture",
        "device_display_name": "Alice's phone", "pushkey": "alice-pushkey", "lang": "en",
        "data": {"url": "https://push.fixture.test/_matrix/push/v1/notify", "format": "event_id_only"},
    })

    # A private read receipt, and each of alice and bob uploads a sync filter.
    last = send(bob, dm, {"msgtype": "m.text", "body": "Read this privately"})
    call("POST", f"/_matrix/client/v3/rooms/{dm}/receipt/m.read.private/{last}", alice, {})
    alice_filter = call("POST", "/_matrix/client/v3/user/@alice:fixture.test/filter", alice,
                        {"room": {"timeline": {"limit": 20}}, "presence": {"not_types": ["*"]}})["filter_id"]
    bob_filter = call("POST", "/_matrix/client/v3/user/@bob:fixture.test/filter", bob,
                      {"event_fields": ["type", "content", "sender"]})["filter_id"]

    # A deactivated account, and an administrator-created one.
    call("POST", "/_synapse/admin/v1/deactivate/@dave:fixture.test", alice, {"erase": False})

    json.dump({"lobby": lobby, "dm": dm, "alice_token": alice, "bob_token": bob,
               "avatar": avatar["content_uri"], "picture": picture["content_uri"],
               "first_message": first, "private_receipt": last,
               "alice_filter": alice_filter, "bob_filter": bob_filter, "backup_version": backup,
               "alice_master_key": alice_xs["master"][1], "bob_master_key": bob_xs["master"][1],
               "alice_self_signing_key": alice_xs["ssk"][1]},
              open(FACTS, "w"), indent=2)
    print("ok", lobby, dm)


main()
