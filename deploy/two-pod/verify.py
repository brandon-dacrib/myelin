#!/usr/bin/env python3
"""Two-pod verification against port-forwards: A = hs-0 on :18008, B = hs-1 on :18009.

Written 2026-09-27 for the two-pod experiment on the owner's cluster (docs/status/03-cluster.md,
"Where this stopped"); NOT YET RUN against pods -- the cluster's etcd was unhealthy and nothing
was installed. Expects users `alice` and `bob` (made with `hs register`) and:

  kubectl -n myelin-cluster port-forward pod/hs-0 18008:8008 &
  kubectl -n myelin-cluster port-forward pod/hs-1 18009:8008 &
  python3 deploy/two-pod/verify.py <alice-password> <bob-password>

Checks: six rooms created through hs-0, bob joins each through hs-1, concurrent sends through
both pods, identical /messages on both, a long-poll /sync on each pod woken by a write on the
other, a media upload through hs-0 downloaded byte-for-byte through hs-1. Writes rooms.json in
the current directory for failover.py. Prints a transcript; never prints tokens or passwords.
Server name is SERVER below; change it with the values file.
"""
import hashlib
import json
import os
import sys
import threading
import time
import urllib.error
import urllib.parse
import urllib.request

A = "http://127.0.0.1:18008"
B = "http://127.0.0.1:18009"
SERVER = "myelin-cluster.dacrib.net"


def req(base, method, path, token=None, body=None, raw=None, ctype=None, timeout=60):
    data = None
    headers = {}
    if body is not None:
        data = json.dumps(body).encode()
        headers["Content-Type"] = "application/json"
    if raw is not None:
        data = raw
        headers["Content-Type"] = ctype or "application/octet-stream"
    if token:
        headers["Authorization"] = "Bearer " + token
    r = urllib.request.Request(base + path, data=data, method=method, headers=headers)
    t0 = time.monotonic()
    try:
        with urllib.request.urlopen(r, timeout=timeout) as resp:
            out = resp.read()
            status = resp.status
    except urllib.error.HTTPError as e:
        out = e.read()
        status = e.code
    dt = time.monotonic() - t0
    return status, out, dt


def jreq(*a, **k):
    status, out, dt = req(*a, **k)
    try:
        return status, json.loads(out or b"{}"), dt
    except ValueError:
        return status, {"_raw": out[:200].decode(errors="replace")}, dt


def say(*a):
    print(*a, flush=True)


def login(base, user, pw):
    s, j, _ = jreq(base, "POST", "/_matrix/client/v3/login", body={
        "type": "m.login.password",
        "identifier": {"type": "m.id.user", "user": user},
        "password": pw,
    })
    assert s == 200, (s, j)
    return j["access_token"], j["user_id"]


def bodies(base, token, room):
    q = urllib.parse.quote(room)
    s, j, _ = jreq(base, "GET", f"/_matrix/client/v3/rooms/{q}/messages?dir=b&limit=100", token)
    assert s == 200, (s, j)
    ev = [e for e in reversed(j["chunk"]) if e.get("type") == "m.room.message"]
    return [(e["event_id"], e["content"].get("body")) for e in ev]


def main():
    apw, bpw = sys.argv[1], sys.argv[2]
    run = str(int(time.time()))
    say("$ login: alice through A (hs-0), bob through B (hs-1), alice also through B")
    at_a, alice = login(A, "alice", apw)
    bt_b, bob = login(B, "bob", bpw)
    at_b, _ = login(B, "alice", apw)
    say(" ", alice, bob)

    say("\n$ POST /createRoom through A (alice), inviting bob; six rooms, so both pods own some")
    rooms = []
    for i in range(6):
        s, j, dt = jreq(A, "POST", "/_matrix/client/v3/createRoom", at_a,
                        body={"name": f"two-pod {run} #{i}", "invite": [bob], "preset": "private_chat"})
        say(f"  #{i} -> {s} {j.get('room_id', j)} ({dt*1000:.0f} ms)")
        assert s == 200
        rooms.append(j["room_id"])
    room = rooms[0]

    say("\n$ bob joins every room through B")
    for r in rooms:
        s, j, dt = jreq(B, "POST", f"/_matrix/client/v3/rooms/{urllib.parse.quote(r)}/join", bt_b, body={})
        say(f"  {r} -> {s} ({dt*1000:.0f} ms)")
        assert s == 200, j

    say(f"\n$ PUT /send in {room}: alice through A x5 and bob through B x5, concurrently")
    results = []

    def send(base, tok, who, i, r=room):
        txn = f"{run}-{who}-{i}"
        s, j, dt = jreq(base, "PUT",
                        f"/_matrix/client/v3/rooms/{urllib.parse.quote(r)}/send/m.room.message/{txn}",
                        tok, body={"msgtype": "m.text", "body": f"from {who} #{i}"})
        results.append((who, i, s, dt))

    ts = [threading.Thread(target=send, args=(A, at_a, "A", i)) for i in range(1, 6)]
    ts += [threading.Thread(target=send, args=(B, bt_b, "B", i)) for i in range(1, 6)]
    for t in ts:
        t.start()
    for t in ts:
        t.join()
    for who, i, s, dt in sorted(results):
        say(f"  {who}#{i} -> {s} ({dt*1000:.0f} ms)")
    assert all(s == 200 for _, _, s, _ in results)

    say("\n$ GET /messages on A and on B")
    ma = bodies(A, at_a, room)
    mb = bodies(B, bt_b, room)
    say("  A:", len(ma), [b for _, b in ma])
    say("  B:", len(mb), [b for _, b in mb])
    say("  identical event ids and order on both pods:", ma == mb)
    assert ma == mb and len(ma) == 10

    say("\n$ every room written through both pods, read back from both")
    for r in rooms[1:]:
        for base, tok, who in ((A, at_a, "A"), (B, bt_b, "B")):
            s, j, _ = jreq(base, "PUT",
                           f"/_matrix/client/v3/rooms/{urllib.parse.quote(r)}/send/m.room.message/{run}-x-{who}",
                           tok, body={"msgtype": "m.text", "body": f"hello from {who}"})
            assert s == 200, (s, j)
        xa, xb = bodies(A, at_a, r), bodies(B, bt_b, r)
        say(f"  {r}: A={[b for _, b in xa]} B={[b for _, b in xb]} same={xa == xb}")
        assert xa == xb and len(xa) == 2

    for label, sbase, stok, wbase, wtok, wname in (
        ("bob long-polls /sync on B, alice writes through A", B, bt_b, A, at_a, "A"),
        ("alice long-polls /sync on A, bob writes through B", A, at_a, B, bt_b, "B"),
    ):
        say(f"\n$ {label}")
        filt = urllib.parse.quote(json.dumps({"room": {"timeline": {"limit": 5}}}))
        s, j, _ = jreq(sbase, "GET", f"/_matrix/client/v3/sync?timeout=0&filter={filt}", stok)
        assert s == 200, (s, j)
        since = j["next_batch"]
        # Settle: drain anything already pending so the long poll really waits.
        for _ in range(3):
            s, j, _ = jreq(sbase, "GET", f"/_matrix/client/v3/sync?timeout=0&since={since}&filter={filt}", stok)
            since = j["next_batch"]
        out = {}

        def poll():
            s, j, dt = jreq(sbase, "GET",
                            f"/_matrix/client/v3/sync?timeout=30000&since={since}&filter={filt}",
                            stok, timeout=45)
            out["s"], out["j"], out["dt"], out["end"] = s, j, dt, time.monotonic()

        t = threading.Thread(target=poll)
        t.start()
        time.sleep(3)
        body = f"wake-up via {wname} {run}"
        t_send = time.monotonic()
        s, j, dt = jreq(wbase, "PUT",
                        f"/_matrix/client/v3/rooms/{urllib.parse.quote(room)}/send/m.room.message/{run}-wake-{wname}",
                        wtok, body={"msgtype": "m.text", "body": body})
        say(f"  send through {wname} after the poll had waited 3 s -> {s} ({dt*1000:.0f} ms)")
        t.join()
        tl = out["j"].get("rooms", {}).get("join", {}).get(room, {}).get("timeline", {}).get("events", [])
        got = [e["content"].get("body") for e in tl if e.get("type") == "m.room.message"]
        say(f"  /sync returned {out['s']} after {out['dt']:.2f} s total, "
            f"{(out['end'] - t_send)*1000:.0f} ms after the send started; timeline: {got}")
        assert body in got, "long poll was not woken with the event"

    say("\n$ media: upload through A, download through B")
    blob = os.urandom(200_000)
    s, j, dt = jreq(A, "POST", "/_matrix/media/v3/upload?filename=two-pod.bin", at_a, raw=blob)
    say(f"  upload through A -> {s} {j.get('content_uri', j)} ({dt*1000:.0f} ms)")
    assert s == 200
    mxc = j["content_uri"]
    srv, mid = mxc[len("mxc://"):].split("/", 1)
    s, out, dt = req(B, "GET", f"/_matrix/client/v1/media/download/{srv}/{mid}", bt_b)
    say(f"  download through B -> {s}, {len(out)} bytes, sha256 match: "
        f"{hashlib.sha256(out).hexdigest() == hashlib.sha256(blob).hexdigest()} ({dt*1000:.0f} ms)")
    assert s == 200 and out == blob
    s, out, dt = req(A, "GET", f"/_matrix/client/v1/media/download/{srv}/{mid}", at_a)
    say(f"  download through A -> {s}, {len(out)} bytes, same: {out == blob}")

    with open("rooms.json", "w") as f:
        json.dump({"rooms": rooms, "run": run}, f)
    say("\nALL CHECKS PASSED")


if __name__ == "__main__":
    main()
