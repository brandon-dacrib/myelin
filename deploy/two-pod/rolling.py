#!/usr/bin/env python3
"""Continuous writes through both pods while the StatefulSet rolls: A = hs-0 on :18008,
B = hs-1 on :18009.

Usage (after verify.py, from the same directory, with port-forwards that are re-opened when
their pod is replaced -- `pf-loop.sh` below does that):

  python3 deploy/two-pod/rolling.py <alice-password> <bob-password> 240 &
  sleep 15; helm upgrade hs deploy/helm/hs -n myelin-cluster -f ... --set image.tag=sha-<new>

Two writers, one per pod, each round-robining a send to every room in rooms.json. A send that
fails because its own pod is down (connection refused or reset: the pod is restarting and the
port-forward is being re-opened) is counted as "pod down", not as a failure: a client behind
the Service would have been sent to the other pod. Every other non-200 is a failure and is
printed. The last lines are a per-10-second summary per pod.
"""
import json
import os
import sys
import threading
import time
import urllib.error
import urllib.parse

sys.path.insert(0, os.path.dirname(__file__))
from verify import A, B, jreq, login  # noqa: E402


def writer(base, name, user, pw, rooms, run, secs, t0, out):
    tok = None
    n = 0
    while time.monotonic() - t0 < secs:
        ts = time.monotonic() - t0
        try:
            if tok is None:
                tok, _ = login(base, user, pw)
            r = rooms[n % len(rooms)]
            n += 1
            s, j, dt = jreq(base, "PUT",
                            f"/_matrix/client/v3/rooms/{urllib.parse.quote(r)}/send/m.room.message/"
                            f"roll-{run}-{name}-{n}",
                            tok, body={"msgtype": "m.text", "body": f"rolling {name} #{n}"}, timeout=40)
        except (urllib.error.URLError, ConnectionError, OSError, AssertionError) as e:
            out.append((ts, name, "down", 0.0, str(e)[:80]))
            time.sleep(0.5)
            continue
        if s == 200:
            out.append((ts, name, "ok", dt, ""))
        else:
            err = j.get("errcode", "") + " " + str(j.get("error", ""))[:110]
            out.append((ts, name, "fail", dt, f"{s} {err}"))
            print(f"  t={ts:6.2f}s via {name} room#{rooms.index(r)} -> {s} in {dt*1000:.0f} ms {err}",
                  flush=True)
        if dt > 2.0 and s == 200:
            print(f"  t={ts:6.2f}s via {name} slow: {dt*1000:.0f} ms", flush=True)


def main():
    apw, bpw, secs = sys.argv[1], sys.argv[2], float(sys.argv[3])
    meta = json.load(open("rooms.json"))
    rooms, run = meta["rooms"], meta["run"]
    t0 = time.monotonic()
    print(f"start {time.strftime('%H:%M:%S', time.gmtime())}Z, {len(rooms)} rooms, "
          f"alice through A and bob through B", flush=True)
    out = []
    ts = [threading.Thread(target=writer, args=(A, "A", "alice", apw, rooms, run, secs, t0, out)),
          threading.Thread(target=writer, args=(B, "B", "bob", bpw, rooms, run, secs, t0, out))]
    for t in ts:
        t.start()
    for t in ts:
        t.join()
    print("per-10s summary (ok / fail / pod-down, worst ok ms):", flush=True)
    for base in range(0, int(secs) + 1, 10):
        cells = []
        for name in ("A", "B"):
            win = [o for o in out if o[1] == name and base <= o[0] < base + 10]
            ok = [o for o in win if o[2] == "ok"]
            cells.append(f"{name}: {len(ok):3d}/{sum(o[2] == 'fail' for o in win):2d}/"
                         f"{sum(o[2] == 'down' for o in win):2d} {max([o[3] for o in ok], default=0)*1000:5.0f}")
        print(f"  {base:3d}-{base+10:3d}s  " + "   ".join(cells), flush=True)
    fails = [o for o in out if o[2] == "fail"]
    print(f"total ok {sum(o[2] == 'ok' for o in out)}, failures {len(fails)}, "
          f"pod-down {sum(o[2] == 'down' for o in out)}", flush=True)


if __name__ == "__main__":
    main()
