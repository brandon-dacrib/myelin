#!/usr/bin/env bash
# Runs Complement against this server in cluster mode: several server processes sharing one
# `SERVER_NAME` (or federating as distinct server names, depending on the blueprint), exercising
# sharded-room ownership, lease handover, and failover (track 03 Cluster; PLAN.md section 7).
#
# This is further from working than run_single_node.sh: Complement's own image contract
# (refs/complement/README.md) assumes one container is one homeserver, so "cluster mode" here
# means a *multi-container* Complement blueprint (several `hs-server` containers behind one
# `SERVER_NAME`, or a docker-compose-style sidecar network) rather than anything Complement's
# stock harness does automatically. Track 03/12 own the actual cluster deployment topology
# (Kubernetes manifests, `kind` cluster fixtures per PLAN.md section 12 layer L8); this script is
# the seam where that topology gets plugged in, not a working implementation of it.
#
# Until that topology exists, this script only validates preconditions and explains what is
# missing, so a later change only needs to fill in $CLUSTER_COMPOSE_FILE and re-run.
set -euo pipefail
cd "$(dirname "$0")"

if ! command -v docker >/dev/null 2>&1 || ! docker info >/dev/null 2>&1; then
  echo "run_cluster.sh: SKIP: Docker is not available." >&2
  exit 0
fi

CLUSTER_COMPOSE_FILE="${CLUSTER_COMPOSE_FILE:-cluster/docker-compose.yaml}"
if [ ! -f "$CLUSTER_COMPOSE_FILE" ]; then
  echo "run_cluster.sh: SKIP: no cluster topology at $CLUSTER_COMPOSE_FILE yet." >&2
  echo "This needs track 03/12's cluster deployment manifests (leases, shard ownership, mesh" >&2
  echo "TLS -- see docs/workstreams/03-cluster.md and 12-platform-and-kubernetes.md); track 14" >&2
  echo "provides the Complement wiring (this script) once that topology exists to point at." >&2
  exit 0
fi

echo "run_cluster.sh: found $CLUSTER_COMPOSE_FILE but the rest of this script is not yet" >&2
echo "implemented (bring the cluster up, point COMPLEMENT_BASE_IMAGE or a multi-homeserver" >&2
echo "blueprint at it, run go test, tear down). Update this script alongside the compose file." >&2
exit 0
