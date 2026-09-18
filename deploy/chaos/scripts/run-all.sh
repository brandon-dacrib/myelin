#!/usr/bin/env bash
# UNTESTED (see deploy/chaos/README.md). Bootstraps a `kind` cluster, applies the chaos manifests,
# runs every scenario in turn, and tears down on exit regardless of outcome.
#
# Requires: kind, kubectl, docker (or an equivalent kind provider), and an `hs-chaos-actor:dev`
# image built from this repo once `hs chaos-actor` exists (see README.md).
set -euo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")/.."

CLUSTER_NAME="${CLUSTER_NAME:-hs-cluster-chaos}"
NAMESPACE="hs-chaos"

cleanup() {
  echo "==> tearing down kind cluster ${CLUSTER_NAME}"
  kind delete cluster --name "${CLUSTER_NAME}" || true
}
trap cleanup EXIT

echo "==> creating kind cluster ${CLUSTER_NAME}"
kind create cluster --name "${CLUSTER_NAME}"

echo "==> building and loading the chaos actor image"
# Placeholder: replace with the real build once `hs chaos-actor` exists.
# docker build -t hs-chaos-actor:dev -f ../../Dockerfile ../..
# kind load docker-image hs-chaos-actor:dev --name "${CLUSTER_NAME}"

echo "==> applying manifests"
kubectl apply -f manifests/namespace.yaml
kubectl apply -f manifests/postgres.yaml
kubectl apply -f manifests/toxiproxy.yaml
kubectl apply -f manifests/headless-service.yaml
kubectl apply -f manifests/network-policy-baseline.yaml
kubectl apply -f manifests/statefulset.yaml

echo "==> waiting for the chaos actor StatefulSet to be ready"
kubectl -n "${NAMESPACE}" rollout status statefulset/hs-chaos-actor --timeout=180s

echo "==> running scenarios"
./scripts/scenario-pod-kill.sh
./scripts/scenario-partition.sh
./scripts/scenario-slow-store.sh
./scripts/scenario-rolling-update.sh

echo "==> all scenarios completed"
