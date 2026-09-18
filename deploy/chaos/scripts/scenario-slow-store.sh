#!/usr/bin/env bash
# UNTESTED (see deploy/chaos/README.md). Injects latency between every replica and PostgreSQL via
# the toxiproxy HTTP API, and asserts no second writer appears for any shard while the store is
# slow (RFC 0001's risk: "store stalls masquerading as owner death; the fencing epoch makes this
# safe but not free").
set -euo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")/.."
NAMESPACE="hs-chaos"
TOXIPROXY_API="http://localhost:8474" # port-forwarded below

echo "==> scenario: slow store"

kubectl -n "${NAMESPACE}" port-forward svc/toxiproxy 8474:8474 >/tmp/hs-chaos-toxiproxy-pf.log 2>&1 &
PF_PID=$!
trap 'kill ${PF_PID} 2>/dev/null || true' EXIT
sleep 2

echo "    creating the postgres proxy (once, if absent)"
curl -sf -X POST "${TOXIPROXY_API}/proxies" \
  -d '{"name":"postgres","listen":"0.0.0.0:5433","upstream":"postgres:5432"}' \
  >/dev/null || true

echo "    adding 500ms +/- 200ms latency"
curl -sf -X POST "${TOXIPROXY_API}/proxies/postgres/toxics" \
  -d '{"name":"slow","type":"latency","attributes":{"latency":500,"jitter":200}}'

sleep 20
python3 scripts/checker.py --namespace "${NAMESPACE}" --assert-no-double-writes

echo "    removing the latency toxic"
curl -sf -X DELETE "${TOXIPROXY_API}/proxies/postgres/toxics/slow"

sleep 5
python3 scripts/checker.py --namespace "${NAMESPACE}" --assert-no-double-writes

echo "==> scenario: slow store passed"
