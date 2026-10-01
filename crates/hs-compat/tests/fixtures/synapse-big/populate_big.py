"""Loads a real Synapse with one large room, for measuring the importer (see README.md).

    <synapse venv>/bin/python populate_big.py <synapse base url> <registration shared secret> \
        [--members 2000] [--events 100000] [--threads 16]

Registers `--members` accounts through Synapse's shared-secret registration, has them all join
one public room, and has them send `--events` messages into it, with a reaction, a reply and an
edit every so often and a topic change every 5,000 events, from `--threads` senders at once.
Prints the room id. Only Synapse's own client and admin APIs are used, so what lands in its
database is exactly what Synapse writes for such a room.
"""
import argparse
import hashlib
import hmac
import json
import random
import sys
import threading
import time
import urllib.error
import urllib.parse
import urllib.request
from concurrent.futures import ThreadPoolExecutor

ARGS = argparse.ArgumentParser()
ARGS.add_argument("base")
ARGS.add_argument("secret")
ARGS.add_argument("--members", type=int, default=2000)
ARGS.add_argument("--events", type=int, default=100_000)
ARGS.add_argument("--threads", type=int, default=16)
OPTS = ARGS.parse_args()
BASE = OPTS.base.rstrip("/")

WORDS = ("the quick brown fox jumps over a lazy dog while the migration copies every event of "
         "this rather large room into a server written in rust and nobody notices anything at all "
         "except perhaps that the numbers in the log are bigger than usual").split()


def call(method, path, token=None, body=None, attempts=8):
    data = json.dumps(body).encode() if body is not None else None
    headers = {"content-type": "application/json"} if body is not None else {}
    if token:
        headers["authorization"] = "Bearer " + token
    for attempt in range(attempts):
        req = urllib.request.Request(BASE + path, data=data, method=method, headers=headers)
        try:
            with urllib.request.urlopen(req, timeout=120) as resp:
                text = resp.read()
                return json.loads(text) if text else {}
        except urllib.error.HTTPError as e:
            detail = e.read().decode()
            if e.code in (429, 500, 502, 503) and attempt + 1 < attempts:
                try:
                    wait = json.loads(detail).get("retry_after_ms", 500) / 1000
                except ValueError:
                    wait = 0.5
                time.sleep(max(wait, 0.5 * (attempt + 1)))
                continue
            raise RuntimeError(f"{method} {path}: {e.code} {detail}") from None
        except (urllib.error.URLError, TimeoutError, ConnectionError):
            if attempt + 1 < attempts:
                time.sleep(0.5 * (attempt + 1))
                continue
            raise


def register(localpart, admin=False):
    nonce = call("GET", "/_synapse/admin/v1/register")["nonce"]
    password = localpart + "-password"
    mac = hmac.new(OPTS.secret.encode(), digestmod=hashlib.sha1)
    mac.update(b"\x00".join([nonce.encode(), localpart.encode(), password.encode(),
                             b"admin" if admin else b"notadmin"]))
    return call("POST", "/_synapse/admin/v1/register", body={
        "nonce": nonce, "username": localpart, "password": password, "admin": admin,
        "mac": mac.hexdigest(),
    })["access_token"]


def sentence(rng, n):
    return " ".join(rng.choice(WORDS) for _ in range(n)).capitalize() + "."


def main():
    started = time.time()
    owner = register("owner", admin=True)
    room = call("POST", "/_matrix/client/v3/createRoom", owner, {
        "preset": "public_chat", "name": "Big room", "topic": "Where the importer is measured",
        "room_alias_name": "big", "visibility": "public",
    })["room_id"]
    quoted = urllib.parse.quote(room)
    print("room", room, flush=True)

    tokens = [owner]
    lock = threading.Lock()

    def join(i):
        token = register(f"member{i:05d}")
        call("POST", f"/_matrix/client/v3/rooms/{quoted}/join", token, {})
        with lock:
            tokens.append(token)
            if len(tokens) % 250 == 0:
                print(f"{len(tokens)} members after {time.time() - started:.0f}s", flush=True)

    with ThreadPoolExecutor(OPTS.threads) as pool:
        list(pool.map(join, range(1, OPTS.members)))

    sent = [0]
    recent = []

    def send(i):
        rng = random.Random(i)
        token = tokens[rng.randrange(len(tokens))]
        kind, content = "m.room.message", {"msgtype": "m.text", "body": sentence(rng, rng.randint(4, 40))}
        with lock:
            target = recent[rng.randrange(len(recent))] if recent else None
        if target and i % 10 == 3:
            kind, content = "m.reaction", {"m.relates_to": {"rel_type": "m.annotation", "event_id": target,
                                                              "key": rng.choice(["+1", "smile", "eyes"])}}
        elif target and i % 25 == 7:
            content["m.relates_to"] = {"m.in_reply_to": {"event_id": target}}
        elif target and i % 50 == 11:
            content = {"msgtype": "m.text", "body": "* " + content["body"],
                       "m.new_content": {"msgtype": "m.text", "body": content["body"]},
                       "m.relates_to": {"rel_type": "m.replace", "event_id": target}}
        if i % 5000 == 4999:
            call("PUT", f"/_matrix/client/v3/rooms/{quoted}/state/m.room.topic", owner,
                 {"topic": f"Where the importer is measured, part {i // 5000 + 1}"})
        event_id = call("PUT", f"/_matrix/client/v3/rooms/{quoted}/send/{kind}/b{i}", token, content)["event_id"]
        with lock:
            recent.append(event_id)
            del recent[:-200]
            sent[0] += 1
            if sent[0] % 5000 == 0:
                print(f"{sent[0]} events after {time.time() - started:.0f}s", flush=True)

    with ThreadPoolExecutor(OPTS.threads) as pool:
        list(pool.map(send, range(OPTS.events)))
    print(f"done: {len(tokens)} members, {sent[0]} events sent in {time.time() - started:.0f}s; room {room}",
          flush=True)


main()
