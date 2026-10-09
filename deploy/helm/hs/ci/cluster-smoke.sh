#!/usr/bin/env bash
#
# Prove the chart's cluster mode on real pods, under traffic, through everything an operator
# does to it: a two-replica release on PostgreSQL takes continuous client writes and reads
# through its Service while one replica is deleted gracefully, one is killed outright, the
# release is scaled to three replicas and down to one, rolled to another image and rolled back,
# and (with --rotate-certs) has its mesh CA rotated. The replicas talk over mutual TLS, as a
# production release would (values.yaml, `cluster.mesh.tls`): a private CA and one wildcard
# certificate for the headless Service, made here with openssl.
# A traffic pod inside the cluster logs every request; analyze.py judges each phase against
# the rule the design makes (docs/decisions/0017): a request that lands mid-handoff waits for
# the new owner, so a graceful termination, a scale and a roll lose nothing, and a replica
# killed outright may fail only the requests that reach its shards before its lease lapses
# and their forward deadline runs out (cluster.leaseTtl + the 10 s forward deadline).
#
# It also measures what the chart's defaults should be tuned from: each pod's time from
# container start to Ready, and (with --kind) its memory and CPU from the node's cgroup after
# the baseline phase, printed next to the chart's requests and limits.
#
# It is the companion of install-smoke.sh (single node) and deploy/operator/ci/kind-smoke.sh
# (the operator): the same image handling, the same transcript style. CD runs it on the amd64
# image leg's kind cluster after the operator smoke (.github/workflows/cd.yml).
#
# Usage:
#
#   deploy/helm/hs/ci/cluster-smoke.sh IMAGE [options]
#
#   IMAGE                 The image to run (`myelin:smoke`, `ghcr.io/.../myelin:sha-...`),
#                         split as install-smoke.sh does.
#   --kind NAME           Load IMAGE (and --upgrade-from's image, and the PostgreSQL and
#                         Python images when the local daemon has them) into the kind cluster
#                         NAME, use its context, pin pullPolicy Never, and read each pod's
#                         memory and CPU from the node's cgroup.
#   --context CTX         The kubectl context. Default: kind-NAME with --kind, else the current.
#   --namespace NS        Created, installed into, deleted afterwards. Default: random.
#   --upgrade-from IMAGE  Install this image first and roll to IMAGE in the upgrade phase, then
#                         roll back to it. It must be the same repository (the chart has one
#                         image.registry/repository). Without it the upgrade phase is a
#                         `kubectl rollout restart` of the StatefulSet (the same roll, the
#                         same image) and the rollback phase rolls it back again.
#   --phase SECONDS       The baseline and the settle time after each disruption. Default: 20.
#   --timeout DUR         How long any `helm ... --wait` may take. Default: 5m.
#   --local-port PORT     The local end of port-forwards. Default: 18028.
#   --postgres-image IMG  Default: public.ecr.aws/docker/library/postgres:17.
#   --python-image IMG    The traffic pod's image. Default: public.ecr.aws/docker/library/python:3.12-alpine.
#   --media-claim NAME    An existing ReadWriteMany claim for media. Default: a hostPath volume
#                         on the node (what a kind cluster can do; see the script).
#   --rotate-certs        After the rollback, rotate the mesh's certificate authority under
#                         traffic in the three rolls the chart's README prescribes (trust both
#                         CAs; issue from the new one; trust only the new one), each losing
#                         nothing. Needs openssl.
#   --keep                Leave everything in place afterwards.
#   --log-dir DIR         Where the traffic log (traffic.log, one line per request) and the
#                         phase table (phases.tsv) are copied at the end, pass or fail, for a
#                         closer look than the verdict table. Default: not copied.
#   --chart DIR           The chart to install. Default: the one this script lives in.
#
# Example (what CD does, with the image built a moment ago):
#
#   kind create cluster --name smoke
#   deploy/helm/hs/ci/cluster-smoke.sh myelin:smoke --kind smoke --upgrade-from ghcr.io/brandon-dacrib/myelin:main
#
# Exit status is 0 only if every phase kept its rule and every check passed. On failure the
# pods, events and logs are printed before the cleanup runs.
#
# Needs kubectl, helm, curl, python3, jq and openssl; kind and docker with --kind.

set -euo pipefail

usage() { sed -n '2,/^$/p' "$0" | sed 's/^# \{0,1\}//'; }

# --- Arguments -------------------------------------------------------------------------------

IMAGE=""
FROM_IMAGE=""
KIND_CLUSTER=""
CONTEXT=""
NAMESPACE=""
PHASE=20
TIMEOUT="5m"
LOCAL_PORT="18028"
POSTGRES_IMAGE="public.ecr.aws/docker/library/postgres:17"
PYTHON_IMAGE="public.ecr.aws/docker/library/python:3.12-alpine"
MEDIA_CLAIM=""
ROTATE_CERTS=0
LOG_DIR=""
KEEP=0
HERE="$(cd "$(dirname "$0")" && pwd)"
CHART="$(cd "$HERE/.." && pwd)"
RELEASE="myelin"
# fullnameOverride: the StatefulSet, Service and pods are `hs`, `hs-0`, `hs-1`, and the mesh
# domain `hs-headless.<namespace>.svc.cluster.local` is known before the install, which the
# certificate below needs.
STS="hs"

while [ $# -gt 0 ]; do
  case "$1" in
    --kind) KIND_CLUSTER="$2"; shift 2 ;;
    --context) CONTEXT="$2"; shift 2 ;;
    --namespace) NAMESPACE="$2"; shift 2 ;;
    --upgrade-from) FROM_IMAGE="$2"; shift 2 ;;
    --phase) PHASE="$2"; shift 2 ;;
    --timeout) TIMEOUT="$2"; shift 2 ;;
    --local-port) LOCAL_PORT="$2"; shift 2 ;;
    --postgres-image) POSTGRES_IMAGE="$2"; shift 2 ;;
    --python-image) PYTHON_IMAGE="$2"; shift 2 ;;
    --media-claim) MEDIA_CLAIM="$2"; shift 2 ;;
    --rotate-certs) ROTATE_CERTS=1; shift ;;
    --log-dir) LOG_DIR="$2"; shift 2 ;;
    --keep) KEEP=1; shift ;;
    --chart) CHART="$2"; shift 2 ;;
    -h|--help) usage; exit 0 ;;
    -*) echo "unknown option: $1" >&2; usage >&2; exit 2 ;;
    *)
      if [ -n "$IMAGE" ]; then echo "only one IMAGE, please: got '$IMAGE' and '$1'" >&2; exit 2; fi
      IMAGE="$1"; shift ;;
  esac
done

if [ -z "$IMAGE" ]; then usage >&2; exit 2; fi
for tool in kubectl helm curl python3 jq openssl; do
  command -v "$tool" >/dev/null || { echo "$tool is not installed" >&2; exit 2; }
done
PULL_POLICY=""
if [ -n "$KIND_CLUSTER" ]; then
  for tool in kind docker; do
    command -v "$tool" >/dev/null || { echo "$tool is not installed, and --kind was given" >&2; exit 2; }
  done
  : "${CONTEXT:=kind-$KIND_CLUSTER}"
  PULL_POLICY=Never
fi
if [ -z "$CONTEXT" ]; then CONTEXT="$(kubectl config current-context)"; fi
if [ -z "$NAMESPACE" ]; then NAMESPACE="hs-cluster-$(od -An -N3 -tx1 /dev/urandom | tr -d ' \n')"; fi

# --- Image references, split the way the chart wants them (install-smoke.sh has the rule) ----

split_image() {
  local ref="$1" digest="" tag="" last first
  if [[ "$ref" == *@* ]]; then digest="${ref##*@}"; ref="${ref%@*}"; fi
  last="${ref##*/}"
  if [[ "$last" == *:* ]]; then tag="${last##*:}"; ref="${ref%:*}"; fi
  if [[ "$ref" == */* ]]; then
    first="${ref%%/*}"
    if [[ "$first" == *.* || "$first" == *:* || "$first" == localhost ]]; then
      SPLIT_REGISTRY="$first"; SPLIT_REPOSITORY="${ref#*/}"
    else
      SPLIT_REGISTRY="docker.io"; SPLIT_REPOSITORY="$ref"
    fi
  else
    SPLIT_REGISTRY="docker.io"; SPLIT_REPOSITORY="library/$ref"
  fi
  if [ -z "$tag" ] && [ -z "$digest" ]; then tag="latest"; fi
  SPLIT_TAG="$tag"; SPLIT_DIGEST="$digest"
}

# `helm --set` arguments for one image. A digest is pinned as image.digest with an empty tag.
image_sets() {
  split_image "$1"
  printf -- '--set-string\nimage.registry=%s\n--set-string\nimage.repository=%s\n' "$SPLIT_REGISTRY" "$SPLIT_REPOSITORY"
  if [ -n "$SPLIT_DIGEST" ]; then
    printf -- '--set-string\nimage.digest=%s\n--set-string\nimage.tag=\n' "$SPLIT_DIGEST"
  else
    printf -- '--set-string\nimage.tag=%s\n--set-string\nimage.digest=\n' "$SPLIT_TAG"
  fi
  if [ -n "$PULL_POLICY" ]; then printf -- '--set-string\nimage.pullPolicy=%s\n' "$PULL_POLICY"; fi
}

# --- Helpers ---------------------------------------------------------------------------------

say() { printf '\n== %s\n' "$*"; }
run() { printf '$ %s\n' "$*"; "$@"; }
k() { kubectl --context "$CONTEXT" -n "$NAMESPACE" "$@"; }
rk() { printf '$ kubectl %s\n' "$*"; k "$@"; }
http_code() { curl -s -o /dev/null -w '%{http_code}' "$@"; }
redact() { sed -E 's/(token=)[A-Za-z0-9_-]+/\1<redacted>/g; s/"access_token":"[^"]*"/"access_token":"<redacted>"/g'; }
now() { python3 -c 'import time; print(f"{time.time():.3f}")'; }
epoch() {
  date -u -d "$1" +%s 2>/dev/null || date -u -j -f '%Y-%m-%dT%H:%M:%SZ' "$1" +%s 2>/dev/null || echo 0
}

WORK="$(mktemp -d "${TMPDIR:-/tmp}/cluster-smoke.XXXXXX")"
PHASES="$WORK/phases.tsv"
TRAFFIC_LOG="$WORK/traffic.log"
: > "$PHASES"
PF_PID=""
TRAFFIC_LOG_PID=""
CRD_EXISTED=1
DIAGNOSED=0

diagnose() {
  [ "$DIAGNOSED" -eq 1 ] && return 0
  DIAGNOSED=1
  say "What the cluster says (diagnostics on failure)"
  k get pods,pvc,svc,sts -o wide 2>&1 || true
  echo
  k get events --sort-by=.lastTimestamp 2>&1 | tail -60 || true
  echo
  # The pod description shows environment values, the database password among them.
  k describe pod -l "app.kubernetes.io/instance=$RELEASE" 2>&1 | sed -E 's/(PASSWORD: +).*/\1<redacted>/' | tail -80 || true
  echo
  for p in $(k get pods -l "app.kubernetes.io/instance=$RELEASE" -o name 2>/dev/null); do
    echo "--- $p (last 150 lines)"
    k logs "$p" --tail=150 2>&1 | redact || true
  done
  if [ -s "$TRAFFIC_LOG" ]; then
    echo "--- traffic (failures and markers)"
    grep -vE '^R [0-9.]+ [a-z]+ 200 ' "$TRAFFIC_LOG" | tail -80 || true
  fi
}
fail() {
  echo
  echo "FAILED: $*" >&2
  diagnose
  exit 1
}
stop_port_forward() {
  if [ -n "$PF_PID" ]; then kill "$PF_PID" 2>/dev/null || true; wait "$PF_PID" 2>/dev/null || true; PF_PID=""; fi
}
cleanup() {
  local rc=$?
  trap - EXIT
  stop_port_forward
  if [ -n "$TRAFFIC_LOG_PID" ]; then kill "$TRAFFIC_LOG_PID" 2>/dev/null || true; wait "$TRAFFIC_LOG_PID" 2>/dev/null || true; fi
  if [ -n "$LOG_DIR" ] && [ -s "$TRAFFIC_LOG" ]; then
    mkdir -p "$LOG_DIR" && cp "$TRAFFIC_LOG" "$LOG_DIR/traffic.log" && cp "$PHASES" "$LOG_DIR/phases.tsv" \
      && echo "traffic log and phase table copied to $LOG_DIR"
  fi
  if [ "$KEEP" -eq 1 ]; then
    say "Kept, as asked: release $RELEASE in namespace $NAMESPACE (context $CONTEXT); work dir $WORK"
    echo "helm --kube-context $CONTEXT -n $NAMESPACE uninstall $RELEASE && kubectl --context $CONTEXT delete namespace $NAMESPACE"
  else
    say "Cleaning up"
    kubectl --context "$CONTEXT" -n "$NAMESPACE" delete pod traffic --ignore-not-found --wait=false 2>&1 || true
    helm --kube-context "$CONTEXT" -n "$NAMESPACE" uninstall "$RELEASE" --wait 2>&1 || true
    kubectl --context "$CONTEXT" delete namespace "$NAMESPACE" --wait=true --timeout=180s 2>&1 || true
    kubectl --context "$CONTEXT" delete pv "hs-media-$NAMESPACE" --ignore-not-found --wait=false 2>&1 || true
    if [ "$CRD_EXISTED" -eq 0 ]; then
      kubectl --context "$CONTEXT" delete crd bridges.hs.matrix.org --ignore-not-found --wait=true 2>&1 || true
    fi
    rm -rf "$WORK"
  fi
  if [ "$rc" -eq 0 ]; then
    say "PASSED: two replicas of $IMAGE served every request through a graceful delete, a scale up and down, a roll and a rollback$([ "$ROTATE_CERTS" -eq 1 ] && echo ' and a CA rotation'), and a kill cost only what the lease allows"
  else
    say "FAILED (exit $rc); see above"
  fi
  exit "$rc"
}
trap cleanup EXIT
trap 'fail "a command failed unexpectedly (line $LINENO)"' ERR

# Pods of the release, Ready, with `replicas` of them: `helm --wait` covers an install and an
# upgrade; a pod deleted by hand needs this.
wait_ready() {
  local want="$1" deadline=$((SECONDS + ${2:-300})) ready
  while [ $SECONDS -lt $deadline ]; do
    ready="$(k get pods -l "app.kubernetes.io/instance=$RELEASE,app.kubernetes.io/component!=bridges-operator" \
      -o jsonpath='{range .items[*]}{.metadata.name}={.status.conditions[?(@.type=="Ready")].status}{"\n"}{end}' 2>/dev/null \
      | grep -c '=True' || true)"
    if [ "$ready" -eq "$want" ] && [ "$(k get sts "$STS" -o jsonpath='{.status.replicas}')" = "$want" ]; then return 0; fi
    sleep 2
  done
  return 1
}

phase_begin() { PHASE_NAME="$1"; PHASE_RULE="$2"; PHASE_NOTE="${3:-}"; PHASE_START="$(now)"; say "Phase $PHASE_NAME (rule: $PHASE_RULE)"; }
phase_end() {
  local end; end="$(now)"
  printf '%s\t%s\t%s\t%s\t%s\n' "$PHASE_NAME" "$PHASE_START" "$end" "$PHASE_RULE" "$PHASE_NOTE" >> "$PHASES"
  local n; n="$(awk -v s="$PHASE_START" -v e="$end" '$1=="R" && $2>=s && $2<=e' "$TRAFFIC_LOG" | wc -l | tr -d ' ')"
  local f; f="$(awk -v s="$PHASE_START" -v e="$end" '$1=="R" && $2>=s && $2<=e && $4!=200' "$TRAFFIC_LOG" | wc -l | tr -d ' ')"
  echo "phase $PHASE_NAME: $(python3 -c "print(f'{$end-$PHASE_START:.0f}')") s, $n requests so far in the log, $f failed"
}
settle() { echo "settling ${PHASE}s"; sleep "$PHASE"; }

# How long each pod took from container start to Ready, from its own timestamps.
boot_times() {
  local p created started ready
  for p in $(k get pods -l "app.kubernetes.io/instance=$RELEASE" -o jsonpath='{.items[*].metadata.name}'); do
    created="$(k get pod "$p" -o jsonpath='{.metadata.creationTimestamp}')"
    started="$(k get pod "$p" -o jsonpath='{.status.containerStatuses[0].state.running.startedAt}')"
    ready="$(k get pod "$p" -o jsonpath='{.status.conditions[?(@.type=="Ready")].lastTransitionTime}')"
    [ -n "$started" ] && [ -n "$ready" ] || continue
    echo "  $p: container started $(( $(epoch "$started") - $(epoch "$created") ))s after creation, Ready $(( $(epoch "$ready") - $(epoch "$started") ))s after the container started"
  done
}

# With --kind: the pod's memory and CPU from its cgroup on the node, next to the chart's values.
measure_pods() {
  [ -n "$KIND_CLUSTER" ] || { echo "  (not on kind: no cgroup to read; use kubectl top with a metrics-server)"; return 0; }
  local node p cid dir mem cpu
  node="$(kind get nodes --name "$KIND_CLUSTER" | head -1)"
  for p in $(k get pods -l "app.kubernetes.io/instance=$RELEASE" -o jsonpath='{.items[*].metadata.name}'); do
    cid="$(k get pod "$p" -o jsonpath='{.status.containerStatuses[0].containerID}' | sed 's#containerd://##')"
    [ -n "$cid" ] || continue
    dir="$(docker exec "$node" sh -c "find /sys/fs/cgroup -type d -name '*${cid}*' 2>/dev/null | head -1")"
    [ -n "$dir" ] || { echo "  $p: no cgroup found for container $cid"; continue; }
    mem="$(docker exec "$node" cat "$dir/memory.current" 2>/dev/null || echo 0)"
    cpu="$(docker exec "$node" sh -c "awk '/^usage_usec/ {print \$2}' $dir/cpu.stat" 2>/dev/null || echo 0)"
    echo "  $p: memory.current $((mem / 1048576)) MiB, cpu usage $((cpu / 1000000)) s since start"
  done
  echo "  chart values: requests $(helm --kube-context "$CONTEXT" -n "$NAMESPACE" get values "$RELEASE" -a -o json | jq -c '.resources')"
}

# The cluster's own view, per pod, from /metrics: shards owned, ownership changes, forwards
# retried, fenced writes. A port-forward per pod, read once.
cluster_metrics() {
  local p port=$((LOCAL_PORT + 1)) pid text
  for p in $(k get pods -l "app.kubernetes.io/instance=$RELEASE" -o jsonpath='{.items[*].metadata.name}'); do
    kubectl --context "$CONTEXT" -n "$NAMESPACE" port-forward "pod/$p" "$port:8008" --address 127.0.0.1 >/dev/null 2>&1 &
    pid=$!
    for _ in $(seq 1 20); do [ "$(http_code "http://127.0.0.1:$port/health/live")" = "200" ] && break; sleep 0.5; done
    text="$(curl -s "http://127.0.0.1:$port/metrics" || true)"
    kill "$pid" 2>/dev/null || true; wait "$pid" 2>/dev/null || true
    echo "  $p:"
    printf '%s\n' "$text" | grep -E '^hs_cluster_(owned_shards|ownership_changes_total|forward_retries_total|fenced_total|live_replicas|lease_age_seconds)' | sed 's/^/    /' || true
  done
}

# --- Go --------------------------------------------------------------------------------------

say "Cluster smoke: $IMAGE, two replicas on PostgreSQL, chart $CHART"
echo "context $CONTEXT, namespace $NAMESPACE, release $RELEASE, work dir $WORK"
[ -n "$FROM_IMAGE" ] && echo "installing $FROM_IMAGE first, upgrading to $IMAGE, rolling back"

if [ -n "$KIND_CLUSTER" ]; then
  say "Loading images into kind cluster $KIND_CLUSTER"
  run kind load docker-image "$IMAGE" --name "$KIND_CLUSTER" || fail "kind could not load $IMAGE; is it in the local Docker daemon?"
  if [ -n "$FROM_IMAGE" ]; then
    if docker image inspect "$FROM_IMAGE" >/dev/null 2>&1; then
      run kind load docker-image "$FROM_IMAGE" --name "$KIND_CLUSTER"
    else
      echo "$FROM_IMAGE is not in the local daemon; the node will pull it (pullPolicy Never would stop that, so the upgrade-from image uses IfNotPresent)"
    fi
  fi
  for img in "$POSTGRES_IMAGE" "$PYTHON_IMAGE"; do
    if docker image inspect "$img" >/dev/null 2>&1; then
      run kind load docker-image "$img" --name "$KIND_CLUSTER"
    else
      echo "$img is not in the local daemon; the node will pull it"
    fi
  done
fi

say "Creating the namespace, a PostgreSQL and a shared media volume"
run kubectl --context "$CONTEXT" create namespace "$NAMESPACE"
PG_PASSWORD="pg-$(od -An -N8 -tx1 /dev/urandom | tr -d ' \n')"
k apply -f - <<EOF
apiVersion: apps/v1
kind: Deployment
metadata: {name: postgres}
spec:
  replicas: 1
  selector: {matchLabels: {app: postgres}}
  template:
    metadata: {labels: {app: postgres}}
    spec:
      containers:
        - name: postgres
          image: $POSTGRES_IMAGE
          env:
            - {name: POSTGRES_USER, value: hs}
            - {name: POSTGRES_PASSWORD, value: "$PG_PASSWORD"}
            - {name: POSTGRES_DB, value: hs}
            - {name: PGDATA, value: /var/lib/postgresql/data/pgdata}
          ports: [{containerPort: 5432}]
          readinessProbe:
            exec: {command: [pg_isready, -U, hs, -d, hs]}
            periodSeconds: 2
          volumeMounts: [{name: data, mountPath: /var/lib/postgresql/data}]
      volumes: [{name: data, emptyDir: {}}]
---
apiVersion: v1
kind: Service
metadata: {name: postgres}
spec:
  selector: {app: postgres}
  ports: [{port: 5432, targetPort: 5432}]
EOF
rk rollout status deployment/postgres --timeout=180s
# The replicas share media through a ReadWriteMany claim (values.yaml, `media.storage`). kind's
# local-path provisioner refuses ReadWriteMany, so on a cluster with no such class this is a
# hostPath PersistentVolume on the node, bound by name, and a one-shot Job as root opens its
# directory to the server's uid (hostPath ignores fsGroup). --media-claim names a claim that
# already exists instead.
if [ -n "$MEDIA_CLAIM" ]; then
  echo "media: using the existing claim $MEDIA_CLAIM"
else
  MEDIA_CLAIM=hs-media
  k apply -f - <<EOF
apiVersion: v1
kind: PersistentVolume
metadata: {name: hs-media-$NAMESPACE}
spec:
  capacity: {storage: 1Gi}
  accessModes: [ReadWriteMany]
  persistentVolumeReclaimPolicy: Delete
  storageClassName: cluster-smoke-media
  claimRef: {namespace: $NAMESPACE, name: hs-media}
  hostPath: {path: /var/local/hs-media-$NAMESPACE, type: DirectoryOrCreate}
---
apiVersion: v1
kind: PersistentVolumeClaim
metadata: {name: hs-media}
spec:
  accessModes: [ReadWriteMany]
  storageClassName: cluster-smoke-media
  volumeName: hs-media-$NAMESPACE
  resources: {requests: {storage: 1Gi}}
---
apiVersion: batch/v1
kind: Job
metadata: {name: hs-media-open}
spec:
  backoffLimit: 2
  template:
    spec:
      restartPolicy: Never
      containers:
        - name: open
          image: $PYTHON_IMAGE
          command: [sh, -c, "chmod 0777 /media && ls -ld /media"]
          volumeMounts: [{name: media, mountPath: /media}]
      volumes: [{name: media, persistentVolumeClaim: {claimName: hs-media}}]
EOF
  rk wait --for=condition=complete job/hs-media-open --timeout=180s
fi

say "Secrets: a signing key the replicas share, and the mesh's certificate authority and certificate"
if [ -n "$KIND_CLUSTER" ] && docker run --rm "$IMAGE" generate-signing-key > "$WORK/signing.key" 2>/dev/null && [ -s "$WORK/signing.key" ]; then
  echo "signing key generated by \`hs generate-signing-key\` in $IMAGE"
else
  # The same Synapse-shaped line, without running the image: `ed25519 <key id> <unpadded
  # base64 of a 32-byte seed>`.
  python3 - > "$WORK/signing.key" <<'EOF'
import base64, os
print("ed25519 a_smoke " + base64.b64encode(os.urandom(32)).decode().rstrip("="))
EOF
  echo "signing key generated locally"
fi
run kubectl --context "$CONTEXT" -n "$NAMESPACE" create secret generic hs-signing-key --from-file=signing.key="$WORK/signing.key"
MESH_DOMAIN="$STS-headless.$NAMESPACE.svc.cluster.local"
# A CA and a wildcard leaf for the headless Service's domain, as values.yaml's `cluster.mesh.tls`
# describes (the SAN through an extfile, which LibreSSL and OpenSSL both take). `make_ca NAME`
# and `make_leaf NAME CA` write NAME.key/NAME.crt into $WORK; `apply_mesh_secret LEAF CA_BUNDLE`
# writes the kubernetes.io/tls Secret the chart mounts.
make_ca() {
  openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:P-256 -nodes -days 3650 \
    -subj "/CN=$1" -keyout "$WORK/$1.key" -out "$WORK/$1.crt" 2>/dev/null
}
make_leaf() {
  openssl req -newkey ec -pkeyopt ec_paramgen_curve:P-256 -nodes -subj "/CN=*.$MESH_DOMAIN" \
    -keyout "$WORK/$1.key" -out "$WORK/$1.csr" 2>/dev/null
  printf 'subjectAltName=DNS:*.%s\n' "$MESH_DOMAIN" > "$WORK/$1.ext"
  openssl x509 -req -in "$WORK/$1.csr" -CA "$WORK/$2.crt" -CAkey "$WORK/$2.key" -CAcreateserial \
    -days 825 -extfile "$WORK/$1.ext" -out "$WORK/$1.crt" 2>/dev/null
}
apply_mesh_secret() {
  kubectl --context "$CONTEXT" -n "$NAMESPACE" create secret generic hs-mesh-tls --type=kubernetes.io/tls \
    --from-file="tls.crt=$WORK/$1.crt" --from-file="tls.key=$WORK/$1.key" --from-file="ca.crt=$WORK/$2" \
    --dry-run=client -o yaml | kubectl --context "$CONTEXT" -n "$NAMESPACE" apply -f - >/dev/null
  echo "Secret hs-mesh-tls: certificate $1 ($(openssl x509 -in "$WORK/$1.crt" -noout -issuer | sed 's/^issuer=//')), trusting $(grep -c 'BEGIN CERTIFICATE' "$WORK/$2") CA certificate(s)"
}
make_ca mesh-ca
make_leaf mesh mesh-ca
cp "$WORK/mesh-ca.crt" "$WORK/ca-bundle.crt"
apply_mesh_secret mesh ca-bundle.crt

cat > "$WORK/values.yaml" <<EOF
fullnameOverride: $STS
mode: cluster
replicaCount: 2
serverName: smoke.invalid
storage:
  backend: postgres
  postgres:
    host: postgres
    port: 5432
    database: hs
    user: hs
    sslMode: disable
    password: {value: "$PG_PASSWORD"}
media:
  storage:
    backend: local
    local: {existingClaim: $MEDIA_CLAIM}
secrets:
  signingKey: {existingSecret: hs-signing-key}
cluster:
  mesh:
    tls: {existingSecret: hs-mesh-tls}
bridges:
  enabled: false
telemetry:
  logging: {format: text, level: info}
## The traffic pod is one account sending as fast as four loops can, which the per-user
## message limit (0.2/s, burst 10 by default) is there to refuse. These seed the database on
## the first start (decision 0010) with limits the smoke does not reach, so every refusal in
## the log is the cluster's and not the limiter's.
extraConfig:
  rate_limits:
    message: {per_second: 1000, burst_count: 10000}
    login: {per_second: 100, burst_count: 1000}
affinity:
  podAntiAffinity: {style: soft}
EOF

say "helm install: cluster mode, two replicas"
if kubectl --context "$CONTEXT" get crd bridges.hs.matrix.org >/dev/null 2>&1; then CRD_EXISTED=1; else CRD_EXISTED=0; fi
mapfile -t first_image < <(image_sets "${FROM_IMAGE:-$IMAGE}")
if [ -n "$FROM_IMAGE" ] && [ -n "$KIND_CLUSTER" ] && ! docker image inspect "$FROM_IMAGE" >/dev/null 2>&1; then
  first_image+=(--set-string image.pullPolicy=IfNotPresent)
fi
install_started=$SECONDS
run helm --kube-context "$CONTEXT" -n "$NAMESPACE" install "$RELEASE" "$CHART" -f "$WORK/values.yaml" \
  "${first_image[@]}" --wait --timeout "$TIMEOUT" \
  || fail "helm install did not reach Ready within $TIMEOUT"
echo "helm install returned after $((SECONDS - install_started))s"
k get statefulset "$STS" >/dev/null || fail "no StatefulSet $STS"
rk get pods,pvc,svc -o wide
k logs "$STS-0" | grep -i 'mesh authentication' | sed 's/^/  /' || true
say "Boot times (first start, PostgreSQL)"
boot_times

say "Claiming the server through hs-0's setup link"
# The setup token lives in the process that printed it, so the claim goes to that pod, not
# through the Service.
log="$(k logs "$STS-0")"
setup_link="$(printf '%s\n' "$log" | grep 'setup_link' | grep -oE 'http://localhost:8008/admin/setup#token=[A-Za-z0-9_-]+' | head -1 || true)"
[ -n "$setup_link" ] || fail "$STS-0 did not log a setup link"
setup_token="${setup_link##*token=}"
# kubectl itself, not the k() function: backgrounding a function forks a subshell whose PID is
# not kubectl's, and killing it would leave the port-forward running.
kubectl --context "$CONTEXT" -n "$NAMESPACE" port-forward "pod/$STS-0" "$LOCAL_PORT:8008" --address 127.0.0.1 >/dev/null 2>&1 &
PF_PID=$!
base="http://127.0.0.1:$LOCAL_PORT"
for _ in $(seq 1 30); do [ "$(http_code "$base/health/live")" = "200" ] && break; sleep 1; done
[ "$(http_code "$base/health/live")" = "200" ] || fail "nothing answered /health/live through the port-forward within 30s"
ADMIN_PASSWORD="smoke-$(od -An -N8 -tx1 /dev/urandom | tr -d ' \n')"
created_body="$(curl -s -w '\n%{http_code}' -X POST "$base/api/v1/setup" -H 'content-type: application/json' \
  -d "{\"setup_token\":\"$setup_token\",\"username\":\"smoke\",\"password\":\"$ADMIN_PASSWORD\"}")"
code="${created_body##*$'\n'}"
echo "POST /api/v1/setup  $code $(printf '%s' "${created_body%$'\n'*}" | redact)"
[ "$code" = "201" ] || fail "the setup link did not create the first administrator (expected 201)"
# Both replicas see the account: a login through the other pod.
stop_port_forward
kubectl --context "$CONTEXT" -n "$NAMESPACE" port-forward "pod/$STS-1" "$LOCAL_PORT:8008" --address 127.0.0.1 >/dev/null 2>&1 &
PF_PID=$!
for _ in $(seq 1 30); do [ "$(http_code "$base/health/live")" = "200" ] && break; sleep 1; done
code="$(http_code -X POST "$base/_matrix/client/v3/login" -H 'content-type: application/json' \
  -d "{\"type\":\"m.login.password\",\"identifier\":{\"type\":\"m.id.user\",\"user\":\"smoke\"},\"password\":\"$ADMIN_PASSWORD\"}")"
echo "POST /_matrix/client/v3/login through $STS-1  $code"
[ "$code" = "200" ] || fail "the administrator created through $STS-0 could not log in through $STS-1"
stop_port_forward

say "Starting the traffic pod (hits the Service, so each request lands on either replica)"
run kubectl --context "$CONTEXT" -n "$NAMESPACE" create configmap traffic --from-file=traffic.py="$HERE/cluster-smoke/traffic.py"
printf '%s' "$ADMIN_PASSWORD" > "$WORK/password"
run kubectl --context "$CONTEXT" -n "$NAMESPACE" create secret generic traffic --from-file="password=$WORK/password"
k apply -f - <<EOF
apiVersion: v1
kind: Pod
metadata: {name: traffic}
spec:
  restartPolicy: Never
  containers:
    - name: traffic
      image: $PYTHON_IMAGE
      command: [python3, -u, /traffic/traffic.py]
      env:
        - {name: HS_URL, value: "http://$STS.$NAMESPACE.svc:8008"}
        - {name: HS_USER, value: smoke}
        - {name: HS_PASSWORD, valueFrom: {secretKeyRef: {name: traffic, key: password}}}
        - {name: HS_ROOMS, value: "16"}
        - {name: HS_THREADS, value: "4"}
      volumeMounts: [{name: traffic, mountPath: /traffic}]
  volumes: [{name: traffic, configMap: {name: traffic}}]
EOF
rk wait --for=condition=Ready pod/traffic --timeout=180s
# The log is followed into a file for the whole run; kubectl logs -f ends if the pod does.
k logs -f traffic > "$TRAFFIC_LOG" 2>&1 &
TRAFFIC_LOG_PID=$!
for _ in $(seq 1 120); do grep -q '^READY' "$TRAFFIC_LOG" 2>/dev/null && break; sleep 1; done
grep '^READY' "$TRAFFIC_LOG" >/dev/null || fail "the traffic pod did not log READY within 120s: $(tail -5 "$TRAFFIC_LOG")"
grep '^READY' "$TRAFFIC_LOG"

# --- Phases ----------------------------------------------------------------------------------
#
# Order: with --upgrade-from, the release was installed from the previous image and is rolled
# to the candidate first, so every disruption below runs on the candidate; the rollback to the
# previous image is the last phase. Without it the roll is a rollout restart, run last with
# its rollback.

revision_before_roll="$(helm --kube-context "$CONTEXT" -n "$NAMESPACE" list -o json | jq -r ".[] | select(.name==\"$RELEASE\") | .revision")"
if [ -n "$FROM_IMAGE" ]; then
  phase_begin upgrade none "helm upgrade from $FROM_IMAGE to $IMAGE: the StatefulSet rolls $STS-1 then $STS-0, each handing its shards on and taking them back"
  mapfile -t target_image < <(image_sets "$IMAGE")
  run helm --kube-context "$CONTEXT" -n "$NAMESPACE" upgrade "$RELEASE" "$CHART" --reuse-values "${target_image[@]}" --wait --timeout "$TIMEOUT"
  wait_ready 2 300 || fail "the roll to $IMAGE did not settle"
  running="$(k get pod "$STS-0" -o jsonpath='{.spec.containers[0].image}')"
  echo "$STS-0 now runs $running"
  split_image "$IMAGE"
  case "$running" in "$SPLIT_REGISTRY/$SPLIT_REPOSITORY:$SPLIT_TAG"|"$SPLIT_REGISTRY/$SPLIT_REPOSITORY@$SPLIT_DIGEST") ;; *) fail "the pods do not run $IMAGE after the upgrade" ;; esac
  settle
  phase_end
  say "Boot times after the roll to $IMAGE"
  boot_times
fi

phase_begin baseline none "two replicas, nothing happening to them"
settle
phase_end
say "Memory and CPU after the baseline"
measure_pods
say "The cluster's own numbers after the baseline"
cluster_metrics

phase_begin graceful-delete none "kubectl delete pod $STS-1 (SIGTERM: readiness withdrawn, shards handed to $STS-0, listeners drained), then it returns and takes its shards back"
t0=$SECONDS
rk delete pod "$STS-1" --wait=true
echo "the pod was gone after $((SECONDS - t0))s"
wait_ready 2 300 || fail "$STS-1 did not come back Ready"
echo "$STS-1 is Ready again after $((SECONDS - t0))s"
settle
phase_end

LEASE_TTL="$(helm --kube-context "$CONTEXT" -n "$NAMESPACE" get values "$RELEASE" -a -o json | jq -r '.cluster.leaseTtl' | sed 's/s$//')"
WINDOW=$((LEASE_TTL + 10 + 5))
phase_begin kill "window:$WINDOW" "kubectl delete pod $STS-0 --grace-period=0 --force: no handoff; its shards are orphaned until its lease (${LEASE_TTL}s) lapses, and forwards to them retry until their 10 s deadline (decision 0017), so failures are allowed for ${WINDOW}s and none after"
t0=$SECONDS
rk delete pod "$STS-0" --grace-period=0 --force --wait=true
wait_ready 2 300 || fail "$STS-0 did not come back Ready"
echo "$STS-0 is Ready again after $((SECONDS - t0))s"
settle
settle
phase_end

phase_begin scale-up none "helm upgrade --set replicaCount=3: a third replica joins and takes a third of the shards from both"
run helm --kube-context "$CONTEXT" -n "$NAMESPACE" upgrade "$RELEASE" "$CHART" --reuse-values --set replicaCount=3 --wait --timeout "$TIMEOUT"
wait_ready 3 300 || fail "three replicas did not become Ready"
settle
phase_end
say "Boot time of the third replica"
boot_times

phase_begin scale-down none "helm upgrade --set replicaCount=1: $STS-2 and then $STS-1 terminate gracefully, each handing its shards on"
run helm --kube-context "$CONTEXT" -n "$NAMESPACE" upgrade "$RELEASE" "$CHART" --reuse-values --set replicaCount=1 --wait --timeout "$TIMEOUT"
wait_ready 1 300 || fail "the release did not settle at one replica"
settle
phase_end

phase_begin scale-to-two none "back to two replicas for the roll"
run helm --kube-context "$CONTEXT" -n "$NAMESPACE" upgrade "$RELEASE" "$CHART" --reuse-values --set replicaCount=2 --wait --timeout "$TIMEOUT"
wait_ready 2 300 || fail "the release did not settle at two replicas"
settle
phase_end
if [ "$ROTATE_CERTS" -eq 1 ]; then
  # The mesh loads its certificate and CA at start and does not watch the files, so a new
  # Secret takes effect through a rollout restart; a CA is rotated in three rolls so that at
  # every moment every pod trusts the CA that signed every other pod's certificate
  # (deploy/helm/hs/README.md, "Rotating the mesh certificates").
  make_ca mesh-ca-2
  cat "$WORK/mesh-ca.crt" "$WORK/mesh-ca-2.crt" > "$WORK/ca-bundle.crt"
  phase_begin ca-rotate-1 none "ca.crt now holds both CAs, the certificate is still the old CA's; rollout restart"
  apply_mesh_secret mesh ca-bundle.crt
  rk rollout restart "statefulset/$STS"
  rk rollout status "statefulset/$STS" --timeout=300s
  wait_ready 2 300 || fail "the first rotation roll did not settle"
  settle
  phase_end
  make_leaf mesh-2 mesh-ca-2
  phase_begin ca-rotate-2 none "a certificate from the new CA, both CAs still trusted; rollout restart"
  apply_mesh_secret mesh-2 ca-bundle.crt
  rk rollout restart "statefulset/$STS"
  rk rollout status "statefulset/$STS" --timeout=300s
  wait_ready 2 300 || fail "the second rotation roll did not settle"
  settle
  phase_end
  cp "$WORK/mesh-ca-2.crt" "$WORK/ca-bundle.crt"
  phase_begin ca-rotate-3 none "only the new CA trusted; rollout restart"
  apply_mesh_secret mesh-2 ca-bundle.crt
  rk rollout restart "statefulset/$STS"
  rk rollout status "statefulset/$STS" --timeout=300s
  wait_ready 2 300 || fail "the third rotation roll did not settle"
  settle
  phase_end
  say "Every pod now presents the new CA's certificate"
  for p in "$STS-0" "$STS-1"; do
    k logs "$p" | grep -i 'mesh authentication' | tail -1 | sed "s/^/  $p: /" || true
  done
fi


if [ -n "$FROM_IMAGE" ]; then
  phase_begin rollback none "helm rollback to revision $revision_before_roll ($FROM_IMAGE): the same roll backwards"
  run helm --kube-context "$CONTEXT" -n "$NAMESPACE" rollback "$RELEASE" "$revision_before_roll" --wait --timeout "$TIMEOUT"
  wait_ready 2 300 || fail "the rollback did not settle"
  echo "$STS-0 now runs $(k get pod "$STS-0" -o jsonpath='{.spec.containers[0].image}')"
  settle
  phase_end
else
  phase_begin restart none "kubectl rollout restart (no previous image given): the StatefulSet rolls $STS-1 then $STS-0 exactly as an image change would"
  rk rollout restart "statefulset/$STS"
  rk rollout status "statefulset/$STS" --timeout=300s
  wait_ready 2 300 || fail "the roll did not settle"
  settle
  phase_end
  phase_begin rollback none "helm rollback to revision $revision_before_roll: a second roll, backwards"
  run helm --kube-context "$CONTEXT" -n "$NAMESPACE" rollback "$RELEASE" "$revision_before_roll" --wait --timeout "$TIMEOUT"
  wait_ready 2 300 || fail "the rollback did not settle"
  settle
  phase_end
fi

# --- The verdict -----------------------------------------------------------------------------

say "The cluster's own numbers at the end"
cluster_metrics
say "Pods at the end"
rk get pods -o wide
say "Verdict (every request the traffic pod made, judged per phase)"
sleep 2
python3 "$HERE/cluster-smoke/analyze.py" "$TRAFFIC_LOG" "$PHASES" || fail "a phase broke its rule"
say "All checks passed"
