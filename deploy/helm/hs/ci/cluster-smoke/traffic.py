#!/usr/bin/env python3
"""Continuous client traffic at a clustered release, one line per request, for cluster-smoke.sh.

Runs as a pod inside the cluster (python:3-alpine, stdlib only) and talks to the chart's
ClusterIP Service, so every request opens a new connection and lands on whichever replica
kube-proxy picks -- the same path a client behind the Service takes. The host-side script
reads this pod's log and judges each phase (analyze.py).

Environment:
  HS_URL        the Service, e.g. http://myelin.hs-smoke.svc:8008
  HS_USER       a localpart with a password login
  HS_PASSWORD   its password
  HS_ROOMS      how many rooms to create and write to (default 16; spread over both replicas'
                shards, since a room's owner is its id hashed over the live replicas)
  HS_THREADS    concurrent request loops (default 4)
  HS_PAUSE_MS   pause between one thread's requests (default 25)
  HS_TIMEOUT_S  per-request timeout; a forward's deadline is 10 s, so anything past this is a
                failure in its own right (default 30)

Output, one record per line, all times in epoch seconds:
  START <t>                       the generator is up and logging in
  READY <t> rooms=<n>             rooms exist; the loop has begun (the host waits for this)
  R <t> <kind> <status> <ms> [<reason>]
                                  one request: kind is send|sync|messages, status the HTTP
                                  status or 0 for a transport error (reason says which)
  T <t> ok=<n> fail=<n>           a running total every five seconds
"""
import http.client
import json
import os
import socket
import sys
import threading
import time
import urllib.parse

URL = os.environ["HS_URL"]
USER = os.environ["HS_USER"]
PASSWORD = os.environ["HS_PASSWORD"]
ROOMS = int(os.environ.get("HS_ROOMS", "16"))
THREADS = int(os.environ.get("HS_THREADS", "4"))
PAUSE = int(os.environ.get("HS_PAUSE_MS", "25")) / 1000.0
TIMEOUT = float(os.environ.get("HS_TIMEOUT_S", "30"))

parsed = urllib.parse.urlsplit(URL)
HOST, PORT = parsed.hostname, parsed.port or 80

out_lock = threading.Lock()
totals = {"ok": 0, "fail": 0}


def emit(line):
    with out_lock:
        sys.stdout.write(line + "\n")
        sys.stdout.flush()


def request(method, path, token=None, body=None):
    """One request on a fresh connection. Returns (status, json, ms, reason)."""
    data = None
    headers = {}
    if body is not None:
        data = json.dumps(body).encode()
        headers["Content-Type"] = "application/json"
    if token:
        headers["Authorization"] = "Bearer " + token
    t0 = time.monotonic()
    conn = http.client.HTTPConnection(HOST, PORT, timeout=TIMEOUT)
    try:
        conn.request(method, path, body=data, headers=headers)
        resp = conn.getresponse()
        raw = resp.read()
        status = resp.status
        reason = ""
    except (socket.timeout, TimeoutError):
        status, raw, reason = 0, b"{}", "timeout"
    except ConnectionRefusedError:
        status, raw, reason = 0, b"{}", "refused"
    except (ConnectionResetError, http.client.RemoteDisconnected, BrokenPipeError):
        status, raw, reason = 0, b"{}", "reset"
    except OSError as e:
        status, raw, reason = 0, b"{}", "oserror:" + type(e).__name__
    finally:
        conn.close()
    ms = (time.monotonic() - t0) * 1000.0
    try:
        j = json.loads(raw or b"{}")
    except ValueError:
        j = {}
    if status != 200 and not reason:
        reason = str(j.get("errcode", "")) + ":" + str(j.get("error", ""))[:60].replace(" ", "_")
    return status, j, ms, reason


def login():
    for attempt in range(60):
        status, j, _, reason = request(
            "POST",
            "/_matrix/client/v3/login",
            body={"type": "m.login.password", "identifier": {"type": "m.id.user", "user": USER},
                  "password": PASSWORD, "initial_device_display_name": "cluster-smoke traffic"},
        )
        if status == 200 and j.get("access_token"):
            return j["access_token"]
        emit(f"LOGIN-RETRY {time.time():.3f} status={status} {reason}")
        time.sleep(2)
    raise SystemExit("could not log in")


def make_rooms(token):
    rooms = []
    for i in range(ROOMS):
        for attempt in range(20):
            status, j, _, reason = request(
                "POST", "/_matrix/client/v3/createRoom", token,
                body={"name": f"cluster-smoke room {i}", "preset": "private_chat"},
            )
            if status == 200 and j.get("room_id"):
                rooms.append(j["room_id"])
                break
            emit(f"CREATE-RETRY {time.time():.3f} room={i} status={status} {reason}")
            time.sleep(1)
        else:
            raise SystemExit(f"could not create room {i}")
    return rooms


def loop(index, token, rooms, run_id):
    n = 0
    since = None
    while True:
        n += 1
        slot = (index + n) % 8
        if slot == 7:
            kind = "sync"
            q = {"timeout": "0"}
            if since:
                q["since"] = since
            path = "/_matrix/client/v3/sync?" + urllib.parse.urlencode(q)
            status, j, ms, reason = request("GET", path, token)
            if status == 200 and j.get("next_batch"):
                since = j["next_batch"]
        elif slot == 3:
            kind = "messages"
            room = rooms[(index * 7 + n) % len(rooms)]
            path = f"/_matrix/client/v3/rooms/{urllib.parse.quote(room)}/messages?dir=b&limit=5"
            status, j, ms, reason = request("GET", path, token)
        else:
            kind = "send"
            room = rooms[(index * 7 + n) % len(rooms)]
            txn = f"smoke-{run_id}-{index}-{n}"
            path = f"/_matrix/client/v3/rooms/{urllib.parse.quote(room)}/send/m.room.message/{txn}"
            status, j, ms, reason = request(
                "PUT", path, token, body={"msgtype": "m.text", "body": f"cluster-smoke {index}/{n}"},
            )
        ok = status == 200
        with out_lock:
            totals["ok" if ok else "fail"] += 1
        emit(f"R {time.time():.3f} {kind} {status} {ms:.0f}{'' if ok else ' ' + reason}")
        time.sleep(PAUSE)


def main():
    emit(f"START {time.time():.3f}")
    token = login()
    rooms = make_rooms(token)
    run_id = int(time.time())
    emit(f"READY {time.time():.3f} rooms={len(rooms)}")
    for i in range(THREADS):
        threading.Thread(target=loop, args=(i, token, rooms, run_id), daemon=True).start()
    while True:
        time.sleep(5)
        with out_lock:
            ok, fail = totals["ok"], totals["fail"]
        emit(f"T {time.time():.3f} ok={ok} fail={fail}")


if __name__ == "__main__":
    main()
