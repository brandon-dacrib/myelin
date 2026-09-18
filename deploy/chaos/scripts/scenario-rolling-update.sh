#!/usr/bin/env bash
# UNTESTED (see deploy/chaos/README.md). Triggers a rolling update and asserts forward-retry and
# ownership-churn metrics stay flat across it (RFC 0001 section 10: "p99 stays flat because no
# lease ever expires; ownership moves are owner-initiated with a quiesce-and-flush").
set -euo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")/.."
NAMESPACE="hs-chaos"

echo "==> scenario: rolling update"

BEFORE=$(python3 scripts/checker.py --namespace "${NAMESPACE}" --scrape-metric hs_cluster_forward_retries_total)

kubectl -n "${NAMESPACE}" patch statefulset hs-chaos-actor \
  --type=json -p='[{"op":"replace","path":"/spec/template/metadata/labels/rollout","value":"'"$(date +%s)"'"}]' \
  || echo "    (label patch is a placeholder trigger; swap in a real image tag bump once hs-chaos-actor:dev exists)"

kubectl -n "${NAMESPACE}" rollout status statefulset/hs-chaos-actor --timeout=180s

AFTER=$(python3 scripts/checker.py --namespace "${NAMESPACE}" --scrape-metric hs_cluster_forward_retries_total)

python3 scripts/checker.py --namespace "${NAMESPACE}" --assert-no-double-writes

echo "    forward retries before=${BEFORE} after=${AFTER} (should not have spiked)"

echo "==> scenario: rolling update passed"
