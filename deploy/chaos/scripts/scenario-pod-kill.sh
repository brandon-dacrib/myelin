#!/usr/bin/env bash
# UNTESTED (see deploy/chaos/README.md). Kills a random chaos-actor pod and asserts its shards
# fail over within `lease_ttl + heartbeat_interval` plus one tick (RFC 0001 section 4), the
# Kubernetes analogue of the in-process `failover_completes_within_configured_ttl` test.
set -euo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")/.."
NAMESPACE="hs-chaos"
LEASE_TTL_MS="${HS_CLUSTER_LEASE_TTL_MS:-3000}"
HEARTBEAT_INTERVAL_MS="${HS_CLUSTER_HEARTBEAT_INTERVAL_MS:-1000}"
BUDGET_S=$(( (LEASE_TTL_MS + HEARTBEAT_INTERVAL_MS * 2) / 1000 + 2 ))

echo "==> scenario: pod kill (budget: ${BUDGET_S}s)"

TARGET=$(kubectl -n "${NAMESPACE}" get pods -l app=hs-chaos-actor -o jsonpath='{.items[0].metadata.name}')
echo "    killing ${TARGET}"

BEFORE_OWNERS=$(python3 scripts/checker.py --namespace "${NAMESPACE}" --dump-owners)
kubectl -n "${NAMESPACE}" delete pod "${TARGET}" --grace-period=0 --force

echo "    waiting ${BUDGET_S}s for failover"
sleep "${BUDGET_S}"

kubectl -n "${NAMESPACE}" rollout status statefulset/hs-chaos-actor --timeout=60s

python3 scripts/checker.py --namespace "${NAMESPACE}" \
  --assert-no-double-writes \
  --assert-failed-over-from "${TARGET}" \
  --before-owners "${BEFORE_OWNERS}"

echo "==> scenario: pod kill passed"
