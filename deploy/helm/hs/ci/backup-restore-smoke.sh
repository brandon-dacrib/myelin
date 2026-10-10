#!/usr/bin/env bash
#
# Prove that a backup of a release restores it: the data an operator must copy, the way to
# copy it, and that what comes back is the same server, with the same signing key, the same
# accounts and the same messages up to the backup, on a real cluster.
#
# Two layouts, one per `--mode` (both by default):
#
#   embedded   The single-node install: database, signing key and media on one volume
#              (`data-hs-0`). The embedded engine has no online snapshot, so the copy is taken
#              with the server scaled to zero (seconds): a helper pod mounts the claim and
#              `tar`s it out, and the server is scaled back up. Then the release is
#              uninstalled and its claim deleted, a fresh install comes up with a new key (proof
#              the data is gone), and the tar is put back the same way.
#   postgres   The cluster install: the database in PostgreSQL (`pg_dump`, online and
#              consistent, no downtime), media on the shared claim (`tar` through a helper pod)
#              and the signing key in its Secret (`kubectl get secret -o yaml`). The database is
#              dropped and recreated, the media claim emptied and the Secret deleted; then the
#              three are restored in the opposite order and the release reinstalled.
#
# In both, a message written *after* the backup must be gone afterwards and one written
# before it must be there: a restore is a return to the backup, not a merge. The chart's
# README has the procedure in operator terms; this script is its executable form, and CD runs
# it on the amd64 image leg's kind cluster (.github/workflows/cd.yml).
#
# Usage:
#
#   deploy/helm/hs/ci/backup-restore-smoke.sh IMAGE [options]
#
#   IMAGE                 The image to run, split as install-smoke.sh does.
#   --kind NAME           Load the images into the kind cluster NAME and use its context.
#   --context CTX         The kubectl context. Default: kind-NAME with --kind, else the current.
#   --namespace NS        Created, installed into, deleted afterwards. Default: random.
#   --mode MODE           embedded, postgres or both. Default: both.
#   --timeout DUR         How long any `helm ... --wait` may take. Default: 5m.
#   --local-port PORT     The local end of port-forwards. Default: 18038.
#   --postgres-image IMG  Default: public.ecr.aws/docker/library/postgres:17.
#   --helper-image IMG    The helper pod's image (needs sh and tar). Default:
#                         public.ecr.aws/docker/library/python:3.12-alpine.
#   --keep                Leave everything in place afterwards.
#   --chart DIR           The chart to install. Default: the one this script lives in.
#
# Exit status is 0 only if every check passed. Needs kubectl, helm, curl and jq; kind and
# docker with --kind.

set -euo pipefail
# errtrace, or the ERR trap below does not fire inside a function: on CD run 38008712117 a
# `helm install --wait` failed inside `run` and the script cleaned up without the diagnostics.
set -o errtrace

usage() { sed -n '2,/^$/p' "$0" | sed 's/^# \{0,1\}//'; }

IMAGE=""
KIND_CLUSTER=""
CONTEXT=""
NAMESPACE=""
MODE="both"
TIMEOUT="5m"
LOCAL_PORT="18038"
POSTGRES_IMAGE="public.ecr.aws/docker/library/postgres:17"
HELPER_IMAGE="public.ecr.aws/docker/library/python:3.12-alpine"
KEEP=0
HERE="$(cd "$(dirname "$0")" && pwd)"
CHART="$(cd "$HERE/.." && pwd)"
RELEASE="myelin"
STS="hs"

while [ $# -gt 0 ]; do
  case "$1" in
    --kind) KIND_CLUSTER="$2"; shift 2 ;;
    --context) CONTEXT="$2"; shift 2 ;;
    --namespace) NAMESPACE="$2"; shift 2 ;;
    --mode) MODE="$2"; shift 2 ;;
    --timeout) TIMEOUT="$2"; shift 2 ;;
    --local-port) LOCAL_PORT="$2"; shift 2 ;;
    --postgres-image) POSTGRES_IMAGE="$2"; shift 2 ;;
    --helper-image) HELPER_IMAGE="$2"; shift 2 ;;
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
case "$MODE" in embedded|postgres|both) ;; *) echo "--mode must be embedded, postgres or both" >&2; exit 2 ;; esac
for tool in kubectl helm curl jq; do
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
if [ -z "$NAMESPACE" ]; then NAMESPACE="hs-backup-$(od -An -N3 -tx1 /dev/urandom | tr -d ' \n')"; fi

# --- The image reference, split as install-smoke.sh does -------------------------------------
ref="$IMAGE"; DIGEST=""; TAG=""
if [[ "$ref" == *@* ]]; then DIGEST="${ref##*@}"; ref="${ref%@*}"; fi
last="${ref##*/}"
if [[ "$last" == *:* ]]; then TAG="${last##*:}"; ref="${ref%:*}"; fi
if [[ "$ref" == */* ]]; then
  first="${ref%%/*}"
  if [[ "$first" == *.* || "$first" == *:* || "$first" == localhost ]]; then REGISTRY="$first"; REPOSITORY="${ref#*/}"
  else REGISTRY="docker.io"; REPOSITORY="$ref"; fi
else REGISTRY="docker.io"; REPOSITORY="library/$ref"; fi
if [ -z "$TAG" ] && [ -z "$DIGEST" ]; then TAG="latest"; fi
IMAGE_ARGS=(--set-string "image.registry=$REGISTRY" --set-string "image.repository=$REPOSITORY")
if [ -n "$DIGEST" ]; then IMAGE_ARGS+=(--set-string "image.digest=$DIGEST" --set-string "image.tag=")
else IMAGE_ARGS+=(--set-string "image.tag=$TAG"); fi
if [ -n "$PULL_POLICY" ]; then IMAGE_ARGS+=(--set-string "image.pullPolicy=$PULL_POLICY"); fi

# --- Helpers ---------------------------------------------------------------------------------

say() { printf '\n== %s\n' "$*"; }
run() { printf '$ %s\n' "$*"; "$@"; }
k() { kubectl --context "$CONTEXT" -n "$NAMESPACE" "$@"; }
rk() { printf '$ kubectl %s\n' "$*"; k "$@"; }
http_code() { curl -s -o /dev/null -w '%{http_code}' "$@"; }
redact() { sed -E 's/(token=)[A-Za-z0-9_-]+/\1<redacted>/g; s/"access_token":"[^"]*"/"access_token":"<redacted>"/g'; }

WORK="$(mktemp -d "${TMPDIR:-/tmp}/backup-smoke.XXXXXX")"
PF_PID=""
DIAGNOSED=0
# --- The smoke image on the kind node -------------------------------------------------------
#
# With pullPolicy Never a pod starts only if the node holds the image *by name*. On CD runs
# 38011878606 and 38014342577 `kind load` found `myelin:smoke` present by name and minutes
# later the kubelet answered ErrImageNeverPull for it, with nothing in these scripts removing
# it. Until the node says why (diagnose prints its images and the kubelet's image-removal log
# lines), every install or upgrade that starts a pod from IMAGE first checks the name on the
# node and imports the image again if it is gone.
kind_node() { echo "${KIND_CLUSTER}-control-plane"; }
node_has_image() { docker exec "$(kind_node)" crictl inspecti "$1" >/dev/null 2>&1; }
ensure_image_on_node() {
  [ -n "$KIND_CLUSTER" ] || return 0
  local ref="docker.io/library/$1"; [[ "$1" == */* ]] && ref="$1"
  if node_has_image "$ref" || node_has_image "$1"; then return 0; fi
  echo "$1 is not on node $(kind_node) by name any more; importing it again"
  docker save "$1" | docker exec -i "$(kind_node)" ctr -n k8s.io images import --all-platforms --digests - >/dev/null \
    || fail "could not import $1 into node $(kind_node)"
  node_has_image "$ref" || node_has_image "$1" || fail "$1 is still not on node $(kind_node) after the import"
}
node_image_diagnostics() {
  [ -n "$KIND_CLUSTER" ] || return 0
  echo "--- images on node $(kind_node) (crictl)"
  docker exec "$(kind_node)" crictl images 2>&1 | grep -iE 'IMAGE|myelin' || true
  for ref in "docker.io/library/$IMAGE" "$IMAGE"; do
    if docker exec "$(kind_node)" crictl inspecti "$ref" >/dev/null 2>&1; then echo "crictl inspecti $ref: present"; else echo "crictl inspecti $ref: NOT present"; fi
  done
  echo "--- the kubelet's image settings"
  kubectl --context "$CONTEXT" get --raw "/api/v1/nodes/$(kind_node)/proxy/configz" 2>/dev/null \
    | python3 -c 'import json,sys; c=json.load(sys.stdin)["kubeletconfig"]; print({k: c.get(k) for k in ("imageGCHighThresholdPercent","imageGCLowThresholdPercent","imageMinimumGCAge","imageMaximumGCAge","evictionHard")})' 2>&1 || true
  echo "--- the kubelet and containerd on images (journal, last 30 matching lines)"
  docker exec "$(kind_node)" journalctl --no-pager -u kubelet -u containerd 2>/dev/null \
    | grep -iE 'garbage|ImageGC|remov.*image|image.*remov|delet.*image|ErrImageNeverPull' | tail -30 || true
}

diagnose() {
  [ "$DIAGNOSED" -eq 1 ] && return 0
  DIAGNOSED=1
  say "What the cluster says (diagnostics on failure)"
  node_image_diagnostics
  k get pods,pvc,svc,sts,secrets -o wide 2>&1 || true
  echo
  k get events --sort-by=.lastTimestamp 2>&1 | tail -40 || true
  echo
  for p in $(k get pods -o name 2>/dev/null); do
    echo "--- $p (last 80 lines)"; k logs "$p" --tail=80 2>&1 | redact || true
  done
}
# diagnose writes to stderr: the ERR trap fires inside `run`, whose stdout a caller may have
# sent to /dev/null (CD run 38011878606 printed "FAILED" and nothing else).
fail() { echo; echo "FAILED: $*" >&2; diagnose >&2; exit 1; }
stop_port_forward() {
  if [ -n "$PF_PID" ]; then kill "$PF_PID" 2>/dev/null || true; wait "$PF_PID" 2>/dev/null || true; PF_PID=""; fi
}
# Port-forward TARGET (pod/x or svc/x) to 127.0.0.1:$LOCAL_PORT, replacing the previous one.
pf() {
  stop_port_forward
  # kubectl itself, not the k() function: backgrounding a function forks a subshell whose PID
  # is not kubectl's, and killing it leaves the port-forward running.
  kubectl --context "$CONTEXT" -n "$NAMESPACE" port-forward "$1" "$LOCAL_PORT:8008" --address 127.0.0.1 >/dev/null 2>&1 &
  PF_PID=$!
  for _ in $(seq 1 40); do [ "$(http_code "$BASE/health/live")" = "200" ] && return 0; sleep 0.5; done
  fail "nothing answered /health/live through the port-forward to $1 within 20s"
}
BASE="http://127.0.0.1:$LOCAL_PORT"
cleanup() {
  local rc=$?
  trap - EXIT
  stop_port_forward
  if [ "$KEEP" -eq 1 ]; then
    say "Kept, as asked: namespace $NAMESPACE (context $CONTEXT); work dir $WORK"
  else
    say "Cleaning up"
    helm --kube-context "$CONTEXT" -n "$NAMESPACE" uninstall "$RELEASE" --wait 2>&1 || true
    kubectl --context "$CONTEXT" delete namespace "$NAMESPACE" --wait=true --timeout=180s 2>&1 || true
    kubectl --context "$CONTEXT" delete pv "hs-media-$NAMESPACE" --ignore-not-found --wait=false 2>&1 || true
    rm -rf "$WORK"
  fi
  if [ "$rc" -eq 0 ]; then say "PASSED: a backup of $IMAGE's release restores it ($MODE)"; else say "FAILED (exit $rc); see above"; fi
  exit "$rc"
}
trap cleanup EXIT
trap 'fail "a command failed unexpectedly (line $LINENO)"' ERR

# A pod that mounts CLAIM at /data and sleeps, for tar in and out. `helper_rm` removes it.
helper_up() {
  k apply -f - <<EOF >/dev/null
apiVersion: v1
kind: Pod
metadata: {name: backup-helper}
spec:
  restartPolicy: Never
  containers:
    - name: helper
      image: $HELPER_IMAGE
      command: [sleep, "3600"]
      volumeMounts: [{name: data, mountPath: /data}]
  volumes: [{name: data, persistentVolumeClaim: {claimName: $1}}]
EOF
  k wait --for=condition=Ready pod/backup-helper --timeout=180s >/dev/null
}
helper_rm() { k delete pod backup-helper --wait=true --timeout=120s >/dev/null 2>&1 || true; }
scale() {
  [ "$1" = 0 ] || ensure_image_on_node "$IMAGE"
  rk scale "statefulset/$STS" --replicas="$1"
  if [ "$1" = "0" ]; then
    k wait --for=delete "pod/$STS-0" --timeout=180s >/dev/null 2>&1 || true
    [ -z "$(k get pod "$STS-0" -o name 2>/dev/null)" ] || fail "$STS-0 did not stop"
    echo "$STS-0 has stopped"
  else
    k rollout status "statefulset/$STS" --timeout=300s >/dev/null || fail "$STS did not come back"
    k wait --for=condition=Ready "pod/$STS-0" --timeout=180s >/dev/null || fail "$STS-0 is not Ready"
    echo "$STS-0 is Ready"
  fi
}
key_id() { curl -sf "$BASE/_matrix/key/v2/server" | jq -r '.verify_keys | keys[0]'; }
# Claim the server through its setup link (hs-0's log), create a room, send a message.
# Sets ADMIN_PASSWORD, TOKEN, ROOM.
first_run() {
  local log link token body code
  log="$(k logs "$STS-0")"
  link="$(printf '%s\n' "$log" | grep 'setup_link' | grep -oE 'http://localhost:8008/admin/setup#token=[A-Za-z0-9_-]+' | head -1 || true)"
  [ -n "$link" ] || fail "$STS-0 did not log a setup link"
  token="${link##*token=}"
  ADMIN_PASSWORD="smoke-$(od -An -N8 -tx1 /dev/urandom | tr -d ' \n')"
  pf "pod/$STS-0"
  body="$(curl -s -w '\n%{http_code}' -X POST "$BASE/api/v1/setup" -H 'content-type: application/json' \
    -d "{\"setup_token\":\"$token\",\"username\":\"smoke\",\"password\":\"$ADMIN_PASSWORD\"}")"
  code="${body##*$'\n'}"
  [ "$code" = "201" ] || fail "the setup link did not create the first administrator (got $code)"
  TOKEN="$(printf '%s' "${body%$'\n'*}" | jq -r .access_token)"
  echo "the first administrator @smoke:smoke.invalid exists"
  ROOM="$(curl -sf -X POST "$BASE/_matrix/client/v3/createRoom" -H "authorization: Bearer $TOKEN" \
    -H 'content-type: application/json' -d '{"name":"backup smoke","preset":"private_chat"}' | jq -r .room_id)"
  [ -n "$ROOM" ] && [ "$ROOM" != null ] || fail "could not create a room"
  echo "room $ROOM"
}
send() {
  local code
  code="$(http_code -X PUT "$BASE/_matrix/client/v3/rooms/$(jq -rn --arg r "$ROOM" '$r|@uri')/send/m.room.message/$2" \
    -H "authorization: Bearer $TOKEN" -H 'content-type: application/json' -d "{\"msgtype\":\"m.text\",\"body\":\"$1\"}")"
  [ "$code" = "200" ] || fail "sending '$1' answered $code"
  echo "sent: $1"
}
login() {
  local body
  body="$(curl -s -X POST "$BASE/_matrix/client/v3/login" -H 'content-type: application/json' \
    -d "{\"type\":\"m.login.password\",\"identifier\":{\"type\":\"m.id.user\",\"user\":\"smoke\"},\"password\":\"$ADMIN_PASSWORD\"}")"
  TOKEN="$(printf '%s' "$body" | jq -r '.access_token // empty')"
  [ -n "$TOKEN" ] || fail "the administrator could not log in after the restore: $(printf '%s' "$body" | redact)"
  echo "login as @smoke:smoke.invalid with the password from before the backup: 200"
}
# The room's messages must hold "$1" and not "$2".
messages_are() {
  local bodies
  bodies="$(curl -sf "$BASE/_matrix/client/v3/rooms/$(jq -rn --arg r "$ROOM" '$r|@uri')/messages?dir=b&limit=50" \
    -H "authorization: Bearer $TOKEN" | jq -r '.chunk[] | select(.type=="m.room.message") | .content.body')"
  printf '%s\n' "$bodies" | grep -Fx "$1" >/dev/null || fail "the room does not have '$1' after the restore; it has: $bodies"
  if printf '%s\n' "$bodies" | grep -Fx "$2" >/dev/null; then fail "the room still has '$2', which was written after the backup"; fi
  echo "the room has '$1' and not '$2'"
}

say "Backup and restore smoke: $IMAGE, mode $MODE, chart $CHART"
echo "context $CONTEXT, namespace $NAMESPACE, release $RELEASE, work dir $WORK"
if [ -n "$KIND_CLUSTER" ]; then
  say "Loading images into kind cluster $KIND_CLUSTER"
  run kind load docker-image "$IMAGE" --name "$KIND_CLUSTER" || fail "kind could not load $IMAGE"
  for img in "$POSTGRES_IMAGE" "$HELPER_IMAGE"; do
    if docker image inspect "$img" >/dev/null 2>&1; then run kind load docker-image "$img" --name "$KIND_CLUSTER"
    else echo "$img is not in the local daemon; the node will pull it"; fi
  done
fi
run kubectl --context "$CONTEXT" create namespace "$NAMESPACE"

# =============================================================================================
if [ "$MODE" = "embedded" ] || [ "$MODE" = "both" ]; then
say "EMBEDDED: install a single-node release"
ensure_image_on_node "$IMAGE"
run helm --kube-context "$CONTEXT" -n "$NAMESPACE" install "$RELEASE" "$CHART" --set fullnameOverride="$STS" \
  --set serverName=smoke.invalid --set bridges.enabled=false --set telemetry.logging.format=text \
  "${IMAGE_ARGS[@]}" --wait --timeout "$TIMEOUT" >/dev/null
first_run
KEY_BEFORE="$(key_id)"
echo "signing key id $KEY_BEFORE"
send "before the backup" "b-$RANDOM"

say "EMBEDDED: back up the data volume (server stopped for the copy, seconds)"
stop_port_forward
scale 0
helper_up "data-$STS-0"
run sh -c "kubectl --context '$CONTEXT' -n '$NAMESPACE' exec backup-helper -- tar cf - -C /data . > '$WORK/data.tar'"
helper_rm
echo "$(wc -c < "$WORK/data.tar" | tr -d ' ') bytes; top-level entries: $(tar tf "$WORK/data.tar" | awk -F/ '{print $2}' | grep -v '^$' | sort -u | tr '\n' ' ')"
scale 1
pf "svc/$STS"
send "after the backup" "a-$RANDOM"

say "EMBEDDED: lose everything (uninstall, delete the claim), reinstall: a different server"
stop_port_forward
run helm --kube-context "$CONTEXT" -n "$NAMESPACE" uninstall "$RELEASE" --wait >/dev/null
rk delete pvc "data-$STS-0" --wait=true
ensure_image_on_node "$IMAGE"
run helm --kube-context "$CONTEXT" -n "$NAMESPACE" install "$RELEASE" "$CHART" --set fullnameOverride="$STS" \
  --set serverName=smoke.invalid --set bridges.enabled=false --set telemetry.logging.format=text \
  "${IMAGE_ARGS[@]}" --wait --timeout "$TIMEOUT" >/dev/null
pf "svc/$STS"
KEY_FRESH="$(key_id)"
echo "the fresh install's signing key id is $KEY_FRESH (was $KEY_BEFORE): the data is really gone"
[ "$KEY_FRESH" != "$KEY_BEFORE" ] || fail "a fresh install has the old key; the claim was not deleted"
curl -sf "$BASE/api/v1/setup" | grep '"needs_setup":true' >/dev/null || fail "a fresh install should need setup"

say "EMBEDDED: restore the volume (server stopped), start it again"
stop_port_forward
scale 0
helper_up "data-$STS-0"
run sh -c "kubectl --context '$CONTEXT' -n '$NAMESPACE' exec backup-helper -- sh -c 'rm -rf /data/* /data/.[!.]* 2>/dev/null; true'"
run sh -c "kubectl --context '$CONTEXT' -n '$NAMESPACE' exec -i backup-helper -- tar xf - -C /data < '$WORK/data.tar'"
helper_rm
scale 1
pf "svc/$STS"

say "EMBEDDED: the same server is back"
KEY_AFTER="$(key_id)"
echo "signing key id $KEY_AFTER"
[ "$KEY_AFTER" = "$KEY_BEFORE" ] || fail "the restored server signs with $KEY_AFTER, not $KEY_BEFORE"
curl -sf "$BASE/api/v1/setup" | grep '"needs_setup":false' >/dev/null || fail "the restored server should not need setup"
login
messages_are "before the backup" "after the backup"
stop_port_forward
run helm --kube-context "$CONTEXT" -n "$NAMESPACE" uninstall "$RELEASE" --wait >/dev/null
rk delete pvc "data-$STS-0" --wait=true
fi

# =============================================================================================
if [ "$MODE" = "postgres" ] || [ "$MODE" = "both" ]; then
say "POSTGRES: a PostgreSQL, a shared media volume, the signing key Secret, a one-replica cluster-mode release"
PG_PASSWORD="pg-$(od -An -N8 -tx1 /dev/urandom | tr -d ' \n')"
k apply -f - <<EOF >/dev/null
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
          readinessProbe: {exec: {command: [pg_isready, -U, hs, -d, hs]}, periodSeconds: 2}
          volumeMounts: [{name: data, mountPath: /var/lib/postgresql/data}]
      volumes: [{name: data, emptyDir: {}}]
---
apiVersion: v1
kind: Service
metadata: {name: postgres}
spec:
  selector: {app: postgres}
  ports: [{port: 5432, targetPort: 5432}]
---
apiVersion: v1
kind: PersistentVolume
metadata: {name: hs-media-$NAMESPACE}
spec:
  capacity: {storage: 1Gi}
  accessModes: [ReadWriteMany]
  persistentVolumeReclaimPolicy: Delete
  storageClassName: backup-smoke-media
  claimRef: {namespace: $NAMESPACE, name: hs-media}
  hostPath: {path: /var/local/hs-media-$NAMESPACE, type: DirectoryOrCreate}
---
apiVersion: v1
kind: PersistentVolumeClaim
metadata: {name: hs-media}
spec:
  accessModes: [ReadWriteMany]
  storageClassName: backup-smoke-media
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
          image: $HELPER_IMAGE
          command: [sh, -c, "chmod 0777 /media"]
          volumeMounts: [{name: media, mountPath: /media}]
      volumes: [{name: media, persistentVolumeClaim: {claimName: hs-media}}]
EOF
k rollout status deployment/postgres --timeout=180s >/dev/null
k wait --for=condition=complete job/hs-media-open --timeout=180s >/dev/null
PG_POD="$(k get pod -l app=postgres -o jsonpath='{.items[0].metadata.name}')"
if [ -n "$KIND_CLUSTER" ] && docker run --rm "$IMAGE" generate-signing-key > "$WORK/signing.key" 2>/dev/null && [ -s "$WORK/signing.key" ]; then :; else
  python3 -c 'import base64, os; print("ed25519 a_smoke " + base64.b64encode(os.urandom(32)).decode().rstrip("="))' > "$WORK/signing.key"
fi
k create secret generic hs-signing-key --from-file=signing.key="$WORK/signing.key" >/dev/null
od -An -N24 -tx1 /dev/urandom | tr -d ' \n' > "$WORK/mesh-shared-secret"
k create secret generic hs-mesh --from-file="mesh-shared-secret=$WORK/mesh-shared-secret" >/dev/null
cat > "$WORK/values.yaml" <<EOF
fullnameOverride: $STS
mode: cluster
replicaCount: 1
serverName: smoke.invalid
storage:
  backend: postgres
  postgres: {host: postgres, port: 5432, database: hs, user: hs, sslMode: disable, password: {value: "$PG_PASSWORD"}}
media:
  storage: {backend: local, local: {existingClaim: hs-media}}
secrets:
  signingKey: {existingSecret: hs-signing-key}
cluster:
  mesh: {sharedSecret: {existingSecret: hs-mesh}}
bridges: {enabled: false}
telemetry: {logging: {format: text, level: info}}
EOF
ensure_image_on_node "$IMAGE"
run helm --kube-context "$CONTEXT" -n "$NAMESPACE" install "$RELEASE" "$CHART" -f "$WORK/values.yaml" \
  "${IMAGE_ARGS[@]}" --wait --timeout "$TIMEOUT" >/dev/null
first_run
KEY_BEFORE="$(key_id)"
echo "signing key id $KEY_BEFORE"
send "before the backup" "b-$RANDOM"
head -c 20000 /dev/urandom > "$WORK/upload.bin"
MXC="$(curl -sf -X POST "$BASE/_matrix/media/v3/upload?filename=smoke.bin" -H "authorization: Bearer $TOKEN" \
  -H 'content-type: application/octet-stream' --data-binary "@$WORK/upload.bin" | jq -r .content_uri)"
[ -n "$MXC" ] && [ "$MXC" != null ] || fail "the media upload failed"
MEDIA_PATH="${MXC#mxc://}"
echo "uploaded 20000 bytes of media as $MXC"

say "POSTGRES: back up (pg_dump online, the media volume, the signing key Secret)"
run sh -c "kubectl --context '$CONTEXT' -n '$NAMESPACE' exec '$PG_POD' -- pg_dump -U hs -Fc hs > '$WORK/hs.dump'"
echo "$(wc -c < "$WORK/hs.dump" | tr -d ' ') bytes of database dump"
helper_up hs-media
run sh -c "kubectl --context '$CONTEXT' -n '$NAMESPACE' exec backup-helper -- tar cf - -C /data . > '$WORK/media.tar'"
helper_rm
echo "$(wc -c < "$WORK/media.tar" | tr -d ' ') bytes of media"
run sh -c "kubectl --context '$CONTEXT' -n '$NAMESPACE' get secret hs-signing-key -o yaml > '$WORK/hs-signing-key.yaml'"
send "after the backup" "a-$RANDOM"

say "POSTGRES: lose everything (uninstall; drop the database; empty the media volume; delete the key Secret)"
stop_port_forward
run helm --kube-context "$CONTEXT" -n "$NAMESPACE" uninstall "$RELEASE" --wait >/dev/null
run kubectl --context "$CONTEXT" -n "$NAMESPACE" exec "$PG_POD" -- psql -U hs -d postgres -q -c 'DROP DATABASE hs' -c 'CREATE DATABASE hs'
helper_up hs-media
run sh -c "kubectl --context '$CONTEXT' -n '$NAMESPACE' exec backup-helper -- sh -c 'rm -rf /data/* /data/.[!.]* 2>/dev/null; true'"
helper_rm
rk delete secret hs-signing-key

say "POSTGRES: restore (the Secret, pg_restore, the media volume), reinstall"
run sh -c "kubectl --context '$CONTEXT' -n '$NAMESPACE' apply -f '$WORK/hs-signing-key.yaml'"
run sh -c "kubectl --context '$CONTEXT' -n '$NAMESPACE' exec -i '$PG_POD' -- pg_restore -U hs -d hs --no-owner < '$WORK/hs.dump'"
helper_up hs-media
run sh -c "kubectl --context '$CONTEXT' -n '$NAMESPACE' exec -i backup-helper -- tar xf - -C /data < '$WORK/media.tar'"
helper_rm
ensure_image_on_node "$IMAGE"
run helm --kube-context "$CONTEXT" -n "$NAMESPACE" install "$RELEASE" "$CHART" -f "$WORK/values.yaml" \
  "${IMAGE_ARGS[@]}" --wait --timeout "$TIMEOUT" >/dev/null
pf "svc/$STS"

say "POSTGRES: the same server is back"
KEY_AFTER="$(key_id)"
echo "signing key id $KEY_AFTER"
[ "$KEY_AFTER" = "$KEY_BEFORE" ] || fail "the restored server signs with $KEY_AFTER, not $KEY_BEFORE"
curl -sf "$BASE/api/v1/setup" | grep '"needs_setup":false' >/dev/null || fail "the restored server should not need setup"
login
messages_are "before the backup" "after the backup"
curl -sf "$BASE/_matrix/client/v1/media/download/$MEDIA_PATH" -H "authorization: Bearer $TOKEN" > "$WORK/download.bin" \
  || fail "the media uploaded before the backup cannot be downloaded"
cmp "$WORK/upload.bin" "$WORK/download.bin" || fail "the restored media differs from what was uploaded"
echo "the media uploaded before the backup downloads byte for byte"
stop_port_forward
fi

say "All checks passed"
