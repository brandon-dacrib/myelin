#!/usr/bin/env python3
"""Continuous writes through hs-0 (:18008) to every room while hs-1 is deleted.

Usage (after verify.py, from the same directory, hs-0 port-forwarded on :18008):

  python3 deploy/two-pod/failover.py <alice-password> 90 &
  sleep 15; kubectl -n myelin-cluster delete pod hs-1

Round-robins a send to each room from rooms.json, one at a time, and prints one line per send
that failed or took over a second, plus a per-5-second summary. Some of the six rooms are owned
by hs-1's shards, so their writes are forwarded over the mesh until hs-1 drains (graceful
delete) and hs-0 takes the shards. NOT YET RUN (2026-09-27; see verify.py).
"""
import json
import os
import sys
import time
import urllib.parse

sys.path.insert(0, os.path.dirname(__file__))
from verify import A, jreq, login  # noqa: E402


def main():
    pw, secs = sys.argv[1], float(sys.argv[2])
    meta = json.load(open("rooms.json"))
    rooms = meta["rooms"]
    tok, _ = login(A, "alice", pw)
    t0 = time.monotonic()
    wall0 = time.strftime("%H:%M:%S", time.gmtime())
    print(f"start {wall0}Z, {len(rooms)} rooms, writes through hs-0 only", flush=True)
    n = 0
    stats = {}
    lastsec = -1
    first_fail = last_fail = None
    while time.monotonic() - t0 < secs:
        r = rooms[n % len(rooms)]
        n += 1
        ts = time.monotonic() - t0
        s, j, dt = jreq(A, "PUT",
                        f"/_matrix/client/v3/rooms/{urllib.parse.quote(r)}/send/m.room.message/fo-{meta['run']}-{n}",
                        tok, body={"msgtype": "m.text", "body": f"failover #{n}"}, timeout=40)
        sec = int(ts)
        ok, fail, worst = stats.get(sec, (0, 0, 0.0))
        if s == 200:
            ok += 1
        else:
            fail += 1
            first_fail = first_fail if first_fail is not None else ts
            last_fail = ts
        stats[sec] = (ok, fail, max(worst, dt))
        if s != 200 or dt > 1.0:
            err = j.get("errcode", "") + " " + str(j.get("error", ""))[:90]
            print(f"  t={ts:6.2f}s room#{rooms.index(r)} -> {s} in {dt*1000:.0f} ms {err if s != 200 else ''}",
                  flush=True)
        if sec != lastsec and sec % 5 == 0:
            lastsec = sec
    print("per-5s summary (ok/fail/worst ms):", flush=True)
    for base in range(0, int(secs) + 1, 5):
        ok = sum(stats.get(s, (0, 0, 0))[0] for s in range(base, base + 5))
        fail = sum(stats.get(s, (0, 0, 0))[1] for s in range(base, base + 5))
        worst = max([stats.get(s, (0, 0, 0))[2] for s in range(base, base + 5)])
        print(f"  {base:3d}-{base+5:3d}s ok={ok:3d} fail={fail:3d} worst={worst*1000:.0f} ms", flush=True)
    print(f"total sends {n}; failures between t={first_fail} and t={last_fail}", flush=True)


if __name__ == "__main__":
    main()
