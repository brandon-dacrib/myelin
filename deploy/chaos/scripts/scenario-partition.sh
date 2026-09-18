#!/usr/bin/env bash
# UNTESTED (see deploy/chaos/README.md). Partitions one pod from the mesh and the store (but not
# from its own health port), waits for failover, then asserts a write using the partitioned
# replica's stale fence is rejected once healed -- the Kubernetes analogue of the in-process
# `a_partitioned_replica_cannot_write_after_being_fenced` test.
set -euo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")/.."
NAMESPACE="hs-chaos"
POD_ORDINAL="${1:-0}"

echo "==> scenario: partition (pod ordinal ${POD_ORDINAL})"

POD_ORDINAL="${POD_ORDINAL}" envsubst < manifests/network-policy-partition.yaml | kubectl apply -f -

echo "    waiting for the rest of the cluster to take over"
sleep 15

python3 scripts/checker.py --namespace "${NAMESPACE}" --assert-no-double-writes

echo "    healing the partition"
kubectl -n "${NAMESPACE}" delete networkpolicy "hs-chaos-partition-${POD_ORDINAL}"
kubectl apply -f manifests/network-policy-baseline.yaml

sleep 5
python3 scripts/checker.py --namespace "${NAMESPACE}" --assert-no-double-writes

echo "==> scenario: partition passed"
