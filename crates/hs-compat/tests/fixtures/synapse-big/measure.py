"""Measures the importer: boots an `hs` binary over an empty data directory, points its
migration at a Synapse database, copies it, and reports how long each stream took, how fast the
rooms went, and the most memory the process held (see README.md).

    python3 measure.py <hs binary> <server name> <synapse signing key> \
        --db-host 127.0.0.1 --db-port 5491 --db-name synapse_big --db-user postgres \
        --db-password hspg [--batch-size 500] [--verify]

The process's resident memory is sampled every 0.2 seconds with `ps`; the peak is reported, as
is the throughput the importer logged itself (newer binaries log it; older ones do not).
"""
import argparse
import json
import os
import shutil
import socket
import subprocess
import tempfile
import threading
import time
import urllib.error
import urllib.request

ARGS = argparse.ArgumentParser()
ARGS.add_argument("hs")
ARGS.add_argument("server_name")
ARGS.add_argument("signing_key")
ARGS.add_argument("--db-host", default="127.0.0.1")
ARGS.add_argument("--db-port", type=int, default=5491)
ARGS.add_argument("--db-name", default="synapse_big")
ARGS.add_argument("--db-user", default="postgres")
ARGS.add_argument("--db-password", default="hspg")
ARGS.add_argument("--batch-size", type=int, default=500)
ARGS.add_argument("--verify", action="store_true")
OPTS = ARGS.parse_args()


def free_port():
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


def call(base, method, path, token=None, body=None):
    data = json.dumps(body).encode() if body is not None else None
    headers = {"content-type": "application/json"} if body is not None else {}
    if token:
        headers["authorization"] = "Bearer " + token
    req = urllib.request.Request(base + path, data=data, method=method, headers=headers)
    try:
        with urllib.request.urlopen(req, timeout=60) as resp:
            text = resp.read()
            return json.loads(text) if text else {}
    except urllib.error.HTTPError as e:
        raise RuntimeError(f"{method} {path}: {e.code} {e.read().decode()}") from None


def main():
    work = tempfile.mkdtemp(prefix="hs-measure-")
    keys = os.path.join(work, "keys")
    os.makedirs(keys)
    shutil.copy(OPTS.signing_key, os.path.join(keys, f"{OPTS.server_name}.signing.key"))
    port = free_port()
    config = os.path.join(work, "hs.yaml")
    with open(config, "w") as f:
        f.write(
            f"server:\n  server_name: {OPTS.server_name}\n  signing_key_path: {json.dumps(keys)}\n"
            f"listeners:\n  listeners:\n    - port: {port}\n      bind_addresses: [\"127.0.0.1\"]\n"
            f"      resources: [client, federation, media, admin, health, metrics]\n"
            f"storage:\n  backend: embedded\n  data_dir: {json.dumps(os.path.join(work, 'db'))}\n"
            f"media:\n  storage:\n    backend: local\n    path: {json.dumps(os.path.join(work, 'media'))}\n"
            f"rate_limits:\n  enabled: false\n")
    env = dict(os.environ)
    env.pop("RUST_LOG", None)
    env.pop("HS_DATA_DIR", None)
    hs = subprocess.Popen([OPTS.hs, "serve", "-c", config], stdout=subprocess.PIPE,
                          stderr=subprocess.DEVNULL, text=True, env=env)
    setup_token = None
    lines = []

    def read():
        for line in hs.stdout:
            lines.append(line)

    threading.Thread(target=read, daemon=True).start()
    deadline = time.time() + 180
    while setup_token is None and time.time() < deadline:
        for line in list(lines):
            if "setup_link=" in line:
                setup_token = line.rsplit("#token=", 1)[1].strip().strip('"')
        time.sleep(0.2)
    if setup_token is None:
        hs.kill()
        raise SystemExit("the server never printed its setup link")
    base = f"http://127.0.0.1:{port}"
    ops = call(base, "POST", "/api/v1/setup",
               body={"setup_token": setup_token, "username": "ops", "password": "ops-password-1"})["access_token"]
    call(base, "PATCH", "/api/v1/config/migration", ops, {"synapse": {
        "database": {"host": OPTS.db_host, "port": OPTS.db_port, "database": OPTS.db_name,
                     "user": OPTS.db_user, "password": OPTS.db_password},
        "batch_size": OPTS.batch_size}})

    peak = [0]
    sampling = [True]

    def sample():
        while sampling[0]:
            out = subprocess.run(["ps", "-o", "rss=", "-p", str(hs.pid)], capture_output=True, text=True)
            if out.stdout.strip():
                peak[0] = max(peak[0], int(out.stdout.strip()) * 1024)
            time.sleep(0.2)

    threading.Thread(target=sample, daemon=True).start()
    rss_before = peak[0]
    started = time.time()
    call(base, "POST", "/api/v1/migration/start", ops)
    while True:
        status = call(base, "GET", "/api/v1/migration", ops)
        if status["status"] in ("ready_for_cutover", "failed"):
            break
        time.sleep(1)
    copy_seconds = time.time() - started
    result = {"status": status["status"], "copy_seconds": round(copy_seconds, 1),
              "peak_rss_mib": round(peak[0] / 1048576, 1), "rss_at_start_mib": round(rss_before / 1048576, 1),
              "streams": {s["name"]: {k: s.get(k) for k in ("copied_count", "skipped_count", "failed_count",
                                                            "rate_per_second")}
                          for s in status["streams"]}}
    log = call(base, "GET", "/api/v1/migration/log?limit=1000", ops)["items"]
    result["logged"] = [e["message"] for e in log
                        if "throughput" in e["message"] or "peak memory" in e["message"]
                        or "refused" in e["message"]][-12:]
    if OPTS.verify:
        started = time.time()
        task = call(base, "POST", "/api/v1/migration/verify", ops)
        while True:
            t = call(base, "GET", f"/api/v1/tasks/{task['id']}", ops)
            if t["status"] in ("succeeded", "failed", "cancelled"):
                break
            time.sleep(1)
        status = call(base, "GET", "/api/v1/migration", ops)
        result["verify_seconds"] = round(time.time() - started, 1)
        result["verified"] = status.get("verification", {}).get("passed")
        result["peak_rss_mib_after_verify"] = round(peak[0] / 1048576, 1)
    sampling[0] = False
    hs.terminate()
    hs.wait(timeout=60)
    shutil.rmtree(work, ignore_errors=True)
    print(json.dumps(result, indent=2))


main()
