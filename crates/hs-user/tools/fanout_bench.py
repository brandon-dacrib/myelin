#!/usr/bin/env python3
"""One real `hs serve`: what a room update's fan-out costs its owner (status 05, session 14).

    MEMBERS=300 MESSAGES=50 [STORAGE=embedded] [HS_SYNC_FAN_OUT_UNBATCHED=1] [PATCH_RETENTION=2] \
        python3 crates/hs-user/tools/fanout_bench.py <label> target/release/hs <pg host> <pg port> <db> <user> <password>

STORAGE=embedded ignores the PostgreSQL arguments (pass anything). HS_SYNC_FAN_OUT_UNBATCHED=1
is the "before" (one member at a time). PATCH_RETENTION=n changes `server.sync` through the
admin API after setup, to see the hub compact. The server's log is written to <label>.log.

Registers alice and bob, makes a public room, joins MEMBERS more users through the admin API,
waits for the hub to catch up (a marker message reaching bob's /sync), then sends MESSAGES
messages from alice while bob long-polls, timing send-to-woken-sync; the hub's own
`hs_user_fan_out_duration_seconds` on /metrics gives the fan-out cost per update.

Usage: fanout_bench.py <label> <hs binary> <pg dsn parts...>  (env: MEMBERS, MESSAGES, HS_SYNC_FAN_OUT_UNBATCHED)
"""
import json, os, socket, subprocess, sys, tempfile, threading, time, urllib.request, urllib.error, re

LABEL = sys.argv[1]
HS = sys.argv[2]
PG_HOST, PG_PORT, PG_DB, PG_USER, PG_PASS = sys.argv[3:8]
MEMBERS = int(os.environ.get("MEMBERS", "300"))
MESSAGES = int(os.environ.get("MESSAGES", "50"))
SERVER = "bench.example.org"


def free_port():
    s = socket.socket(); s.bind(("127.0.0.1", 0)); p = s.getsockname()[1]; s.close(); return p


def http(method, url, body=None, token=None, timeout=120):
    data = None if body is None else json.dumps(body).encode()
    req = urllib.request.Request(url, data=data, method=method)
    req.add_header("Content-Type", "application/json")
    if token:
        req.add_header("Authorization", f"Bearer {token}")
    for attempt in range(30):
        try:
            with urllib.request.urlopen(req, timeout=timeout) as r:
                text = r.read().decode()
                return r.status, (json.loads(text) if text.startswith("{") else text)
        except urllib.error.HTTPError as e:
            text = e.read().decode()
            if e.code in (503, 429) or "fenced" in text or "M_LIMIT_EXCEEDED" in text:
                time.sleep(0.1 * (attempt + 1)); continue
            return e.code, (json.loads(text) if text.startswith("{") else text)
        except (urllib.error.URLError, ConnectionError, socket.timeout) as e:
            time.sleep(0.2 * (attempt + 1)); last = e
    raise RuntimeError(f"{method} {url}: gave up: {last}")


port = free_port()
base = f"http://127.0.0.1:{port}"
d = tempfile.mkdtemp(prefix="fanout-bench-")
storage = (f"  backend: postgres\n  host: {PG_HOST}\n  port: {PG_PORT}\n  database: {PG_DB}\n  user: {PG_USER}\n  password: {PG_PASS}\n  tls: false\n  pool_size: 8"
           if os.environ.get("STORAGE", "postgres") == "postgres" else f"  backend: embedded\n  data_dir: {d}/data")
cfg = f"""server:
  server_name: {SERVER}
  signing_key_path: {d}/keys
listeners:
  listeners:
    - port: {port}
      bind_addresses: ["127.0.0.1"]
      resources: [client, health, metrics]
storage:
{storage}
media:
  storage:
    backend: local
    path: {d}/media
auth:
  enable_registration: true
rate_limits:
  enabled: false
"""
cfg_path = os.path.join(d, "hs.yaml")
open(cfg_path, "w").write(cfg)
env = dict(os.environ)
env.pop("RUST_LOG", None)
proc = subprocess.Popen([HS, "serve", "-c", cfg_path], stdout=subprocess.PIPE, stderr=subprocess.STDOUT, env=env, text=True)
log_lines = []
setup_token = None
retention_line = None
unbatched_line = None
ready = threading.Event()


def reader():
    global setup_token, retention_line, unbatched_line
    for line in proc.stdout:
        log_lines.append(line.rstrip())
        if "sync feed retention in effect" in line:
            retention_line = line.strip()
        if "fanned out one member at a time" in line:
            unbatched_line = line.strip()
        if "setup_link=" in line:
            m = re.search(r"#token=([A-Za-z]+)", line)
            setup_token = m.group(1)
            ready.set()


threading.Thread(target=reader, daemon=True).start()
if not ready.wait(300):
    print("\n".join(log_lines[-30:])); sys.exit("no setup link")
print(f"[{LABEL}] startup: {retention_line}")
if unbatched_line:
    print(f"[{LABEL}] startup: {unbatched_line}")

try:
    st, admin = http("POST", f"{base}/api/v1/setup", {"setup_token": setup_token, "username": "ops", "password": "hunter2-first-admin"})
    admin_token = admin["access_token"]

    def register(name):
        st, first = http("POST", f"{base}/_matrix/client/v3/register", {"username": name, "password": "correct horse"})
        st, done = http("POST", f"{base}/_matrix/client/v3/register", {"username": name, "password": "correct horse", "auth": {"type": "m.login.dummy", "session": first["session"]}})
        return done["user_id"], done["access_token"]

    if os.environ.get("PATCH_RETENTION"):
        n = int(os.environ["PATCH_RETENTION"])
        st, body = http("PATCH", f"{base}/api/v1/config/server", {"sync": {"feed_retention_entries": n, "hot_room_stream_retention_entries": n}}, admin_token)
        print(f"[{LABEL}] PATCH /api/v1/config/server sync retention={n}: {st} {json.dumps(body)[:300]}")
    alice, alice_tok = register("alice")
    bob, bob_tok = register("bob")
    st, created = http("POST", f"{base}/_matrix/client/v3/createRoom", {"preset": "public_chat", "name": "Big"}, alice_tok)
    room = created["room_id"]
    st, j = http("POST", f"{base}/_matrix/client/v3/rooms/{room}/join", {}, bob_tok)
    assert j.get("room_id") == room, j
    esc = room.replace("!", "%21").replace(":", "%3A")

    def add_member(i):
        lp = f"member{i:04}"
        st, _ = http("POST", f"{base}/api/v1/users", {"localpart": lp, "password": "member password, long enough"}, admin_token)
        assert st in (200, 201, 409), (st, _)
        st, body = http("POST", f"{base}/api/v1/rooms/{esc}/join", {"user_id": f"@{lp}:{SERVER}"}, admin_token)
        assert st < 300, (st, body)

    def metrics():
        st, text = http("GET", f"{base}/metrics")
        def sample(name):
            m = re.search(rf"^{re.escape(name)}(?:\{{[^}}]*\}})? ([0-9.e+-]+)$", text, re.M)
            return float(m.group(1)) if m else 0.0
        return {
            "fan_out_sum": sample("hs_user_fan_out_duration_seconds_sum"),
            "fan_out_count": sample("hs_user_fan_out_duration_seconds_count"),
            "members": sample("hs_user_fan_out_members_total"),
            "batched": sample('hs_user_fan_out_transactions_total{outcome="batched"}'),
            "fallback": sample('hs_user_fan_out_transactions_total{outcome="fallback"}'),
            "pruned_feed": sample('hs_user_pruned_entries_total{stream="feed"}'),
            "compactions_feed": sample('hs_user_compactions_total{stream="feed"}'),
        }

    joins_started = time.monotonic()
    threads = []
    sem = threading.Semaphore(8)
    def worker(i):
        with sem:
            add_member(i)
    for i in range(MEMBERS):
        t = threading.Thread(target=worker, args=(i,)); t.start(); threads.append(t)
    for t in threads:
        t.join()
    joins_done = time.monotonic()
    print(f"[{LABEL}] {MEMBERS} members joined in {joins_done - joins_started:.1f}s (requests); waiting for the hub")

    # The hub catches up: a marker from alice reaches bob's sync.
    marker = f"ready {time.time()}"
    http("PUT", f"{base}/_matrix/client/v3/rooms/{room}/send/m.room.message/marker{int(time.time()*1000)}", {"msgtype": "m.text", "body": marker}, alice_tok)
    since = None
    deadline = time.monotonic() + 3600
    while True:
        url = f"{base}/_matrix/client/v3/sync?timeout=10000&set_presence=offline" + (f"&since={since}" if since else "")
        st, body = http("GET", url, token=bob_tok, timeout=60)
        since = body["next_batch"]
        if marker in json.dumps(body):
            break
        if time.monotonic() > deadline:
            sys.exit("the hub never caught up")
    caught_up = time.monotonic()
    print(f"[{LABEL}] hub caught up {caught_up - joins_done:.1f}s after the last join request ({caught_up - joins_started:.1f}s after the first)")

    before = metrics()
    latencies = []
    for i in range(MESSAGES):
        body_text = f"msg {i} {time.time()}"
        result = {}
        def poll():
            s = since
            while True:
                st, b = http("GET", f"{base}/_matrix/client/v3/sync?timeout=30000&set_presence=offline&since={s}", token=bob_tok, timeout=60)
                s = b["next_batch"]
                if body_text in json.dumps(b):
                    result["t"] = time.monotonic(); result["since"] = s; return
        t = threading.Thread(target=poll); t.start()
        time.sleep(0.05)
        sent = time.monotonic()
        http("PUT", f"{base}/_matrix/client/v3/rooms/{room}/send/m.room.message/m{i}{int(time.time()*1000)}", {"msgtype": "m.text", "body": body_text}, alice_tok)
        t.join()
        latencies.append(result["t"] - sent)
        since = result["since"]
        t0 = time.monotonic(); http("GET", f"{base}/_matrix/client/v3/sync?timeout=0&since={since}", token=bob_tok); print(f"[{LABEL}]   message {i}: woken after {latencies[-1]:.2f}s; an empty sync now takes {time.monotonic() - t0:.2f}s", flush=True)
    after = metrics()
    latencies.sort()
    p = lambda q: latencies[min(len(latencies) - 1, round((len(latencies) - 1) * q))]
    updates = after["fan_out_count"] - before["fan_out_count"]
    print(f"[{LABEL}] {MESSAGES} messages, {MEMBERS + 2} members: fan-out {1000 * (after['fan_out_sum'] - before['fan_out_sum']) / max(updates, 1):.1f} ms/update over {updates:.0f} updates; "
          f"transactions batched {after['batched'] - before['batched']:.0f} fallback {after['fallback'] - before['fallback']:.0f}; "
          f"send-to-woken-sync p50 {1000 * p(0.5):.0f} ms p95 {1000 * p(0.95):.0f} ms max {1000 * latencies[-1]:.0f} ms")
    whole = metrics()
    print(f"[{LABEL}] whole run: {whole['fan_out_count']:.0f} fan-outs, {whole['members']:.0f} member writes, {1000 * whole['fan_out_sum'] / max(whole['fan_out_count'], 1):.1f} ms/update mean; feed compactions {whole['compactions_feed']:.0f} pruned {whole['pruned_feed']:.0f}")
finally:
    proc.terminate()
    try:
        proc.wait(30)
    except subprocess.TimeoutExpired:
        proc.kill()
    open(f"{LABEL}.log", "w").write("\n".join(log_lines))
    warns = [l for l in log_lines if " WARN " in l or " ERROR " in l]
    print(f"[{LABEL}] {len(warns)} WARN/ERROR lines" + ("; first: " + warns[0][:300] if warns else ""))
