#!/usr/bin/env bash
#
# Run the operator against a real API server and prove what it reconciles: the chart's bridge
# operator turns a `Bridge` into a claim, a Deployment and a Service and writes Ready, Degraded
# and Ready again into its status, and deleting the `Bridge` removes everything built from it.
# With --homeserver, `hs operator --homeservers` (deploy/operator/) turns a `Homeserver` into
# the chart's objects, rolls an image change through its StatefulSet partition, and lets them
# go when it is deleted. With --heisenbridge, the server itself deploys a real bridge: an
# offering made through the admin API with the `cluster` runtime, its instance walked from
# `requested` to `ready` by the bridge manager, with a real heisenbridge pod.
#
# It is the companion of deploy/helm/hs/ci/install-smoke.sh (the same image handling, the same
# transcript style: every command it runs is echoed with a `$` in front of it, and every state
# change is printed with the seconds since that part began). CD runs it on the amd64 image
# leg's kind cluster after the install smoke, with --homeserver (.github/workflows/cd.yml).
#
# Usage:
#
#   deploy/operator/ci/kind-smoke.sh IMAGE [options]
#
#   IMAGE                The server image (the operator is the same image): `myelin:smoke`,
#                        `ghcr.io/brandon-dacrib/myelin:sha-...`. Split as install-smoke.sh does.
#   --kind NAME          Load IMAGE into the kind cluster NAME first, use its context, and pin
#                        pullPolicy Never. Also what lets --homeserver roll an image: the second
#                        tag is made with `docker tag` and loaded the same way.
#   --context CTX        The kubectl context. Default: kind-NAME with --kind, else the current.
#   --namespace NS       The chart's namespace (the `Homeserver` gets NS-hs). Default: random.
#   --stand-in IMAGE     The image the hand-written `Bridge` runs. It needs `sh` (the operator's
#                        init container copies the files Secret with it) and to listen on
#                        --stand-in-port. Default: public.ecr.aws/docker/library/nginx:alpine, 80.
#   --stand-in-port N    Default: 80.
#   --homeserver         Also run deploy/operator and a single-node `Homeserver` (installs the
#                        `Homeserver` CRD if it is missing, and removes it afterwards if it did).
#   --heisenbridge       Also offer heisenbridge with the `cluster` runtime and walk its instance
#                        to `ready`. Needs the cluster to pull hif1/heisenbridge:latest (the
#                        catalogue's image) from Docker Hub.
#   --timeout SECONDS    How long any one state may take. Default: 300.
#   --local-port PORT    The local end of port-forwards. Default: 18018.
#   --keep               Leave everything in place afterwards.
#
# Examples:
#
#   kind create cluster --name op-smoke
#   deploy/operator/ci/kind-smoke.sh myelin:smoke --kind op-smoke --homeserver --heisenbridge
#   kind delete cluster --name op-smoke
#
# Exit status is 0 only if every check passed. On failure what the cluster says (objects,
# events, the operators' logs) is printed before the cleanup runs.
#
# Needs kubectl, helm, curl and jq; kind and docker with --kind.

set -euo pipefail

usage() { sed -n '2,/^$/p' "$0" | sed 's/^# \{0,1\}//'; }

# --- Arguments -------------------------------------------------------------------------------

IMAGE=""
KIND_CLUSTER=""
CONTEXT=""
NAMESPACE=""
STAND_IN="public.ecr.aws/docker/library/nginx:alpine"
STAND_IN_PORT=80
HOMESERVER=0
HEISENBRIDGE=0
TIMEOUT=300
LOCAL_PORT=18018
KEEP=0
ROOT="$(cd "$(dirname "$0")/../../.." && pwd)"
CHART="$ROOT/deploy/helm/hs"
RELEASE="myelin"

while [ $# -gt 0 ]; do
  case "$1" in
    --kind) KIND_CLUSTER="$2"; shift 2 ;;
    --context) CONTEXT="$2"; shift 2 ;;
    --namespace) NAMESPACE="$2"; shift 2 ;;
    --stand-in) STAND_IN="$2"; shift 2 ;;
    --stand-in-port) STAND_IN_PORT="$2"; shift 2 ;;
    --homeserver) HOMESERVER=1; shift ;;
    --heisenbridge) HEISENBRIDGE=1; shift ;;
    --timeout) TIMEOUT="$2"; shift 2 ;;
    --local-port) LOCAL_PORT="$2"; shift 2 ;;
    --keep) KEEP=1; shift ;;
    -h|--help) usage; exit 0 ;;
    -*) echo "unknown option: $1" >&2; usage >&2; exit 2 ;;
    *)
      if [ -n "$IMAGE" ]; then echo "only one IMAGE, please: got '$IMAGE' and '$1'" >&2; exit 2; fi
      IMAGE="$1"; shift ;;
  esac
done

if [ -z "$IMAGE" ]; then usage >&2; exit 2; fi
for tool in kubectl helm curl jq; do
  command -v "$tool" >/dev/null || { echo "$tool is not installed" >&2; exit 2; }
done
PULL_POLICY=""
if [ -n "$KIND_CLUSTER" ]; then
  for tool in kind docker; do
    command -v "$tool" >/dev/null || { echo "$tool is not installed, and --kind was given" >&2; exit 2; }
  done
  : "${CONTEXT:=kind-$KIND_CLUSTER}"
  PULL_POLICY="Never"
fi
if [ -z "$CONTEXT" ]; then CONTEXT="$(kubectl config current-context)"; fi
if [ -z "$NAMESPACE" ]; then NAMESPACE="hs-op-smoke-$(od -An -N3 -tx1 /dev/urandom | tr -d ' \n')"; fi
NS_HS="$NAMESPACE-hs"

# --- The image reference, split the way the chart wants it (as install-smoke.sh) -------------

ref="$IMAGE"
DIGEST=""
TAG=""
if [[ "$ref" == *@* ]]; then DIGEST="${ref##*@}"; ref="${ref%@*}"; fi
last="${ref##*/}"
if [[ "$last" == *:* ]]; then TAG="${last##*:}"; ref="${ref%:*}"; fi
if [[ "$ref" == */* ]]; then
  first="${ref%%/*}"
  if [[ "$first" == *.* || "$first" == *:* || "$first" == localhost ]]; then
    REGISTRY="$first"; REPOSITORY="${ref#*/}"
  else
    REGISTRY="docker.io"; REPOSITORY="$ref"
  fi
else
  REGISTRY="docker.io"; REPOSITORY="library/$ref"
fi
if [ -z "$TAG" ] && [ -z "$DIGEST" ]; then TAG="latest"; fi

# --- Helpers ---------------------------------------------------------------------------------

say() { printf '\n== %s\n' "$*"; }
run() { printf '$ %s\n' "$*"; "$@"; }
kc() { kubectl --context "$CONTEXT" "$@"; }
k() { kc -n "$NAMESPACE" "$@"; }
kh() { kc -n "$NS_HS" "$@"; }
rk() { printf '$ kubectl -n %s %s\n' "$NAMESPACE" "$*"; k "$@"; }
rkh() { printf '$ kubectl -n %s %s\n' "$NS_HS" "$*"; kh "$@"; }
redact() { sed -E -e 's/(token=)[A-Za-z0-9_-]+/\1<redacted>/g' -e 's/"(access_token|hs_token|as_token)":"[^"]*"/"\1":"<redacted>"/g'; }

T0=$SECONDS
mark() { T0=$SECONDS; }
stamp() { printf '+%3ds  %s\n' "$((SECONDS - T0))" "$*"; }

# Prints each distinct value of `$1` (a command, evaluated) with the time since `mark`, until it
# matches the extended regex `$2`; fails after TIMEOUT seconds, or at once on a match of `$3`.
watch_until() {
  local probe="$1" want="$2" refuse="${3:-}" deadline=$((SECONDS + TIMEOUT)) seen="" now
  while :; do
    now="$(eval "$probe" 2>/dev/null || true)"
    if [ "$now" != "$seen" ]; then stamp "$now"; seen="$now"; fi
    if [[ "$now" =~ $want ]]; then return 0; fi
    if [ -n "$refuse" ] && [[ "$now" =~ $refuse ]]; then fail "reached '$now' while waiting for /$want/"; fi
    if [ "$SECONDS" -ge "$deadline" ]; then fail "still '$now' after ${TIMEOUT}s, waiting for /$want/"; fi
    sleep 1
  done
}

PF_PIDS=()
port_forward() { # namespace service -> sets BASE
  kc -n "$1" port-forward "svc/$2" "$LOCAL_PORT:8008" --address 127.0.0.1 >/dev/null 2>&1 &
  PF_PIDS+=($!)
  BASE="http://127.0.0.1:$LOCAL_PORT"
  for _ in $(seq 1 30); do
    if curl -sf -o /dev/null "$BASE/health/live"; then return 0; fi
    sleep 1
  done
  fail "nothing answered $BASE/health/live through a port-forward to svc/$2 in $1"
}
stop_port_forwards() {
  local pid
  for pid in "${PF_PIDS[@]+"${PF_PIDS[@]}"}"; do kill "$pid" 2>/dev/null || true; wait "$pid" 2>/dev/null || true; done
  PF_PIDS=()
}

DIAGNOSED=0
diagnose() {
  [ "$DIAGNOSED" -eq 1 ] && return 0
  DIAGNOSED=1
  say "What the cluster says (diagnostics on failure)"
  local ns
  for ns in "$NAMESPACE" "$NS_HS"; do
    kc get namespace "$ns" >/dev/null 2>&1 || continue
    echo "--- namespace $ns"
    kc -n "$ns" get bridges,homeservers,deploy,sts,svc,pvc,pods -o wide 2>&1 || true
    kc -n "$ns" get events --sort-by=.lastTimestamp 2>&1 | tail -30 || true
    kc -n "$ns" logs -l app.kubernetes.io/component=bridges-operator --tail=60 2>&1 | redact || true
    kc -n "$ns" logs -l app.kubernetes.io/name=myelin-operator --tail=60 2>&1 | redact || true
    kc -n "$ns" logs -l app.kubernetes.io/name=hs --tail=40 2>&1 | redact || true
  done
}
fail() {
  echo
  echo "FAILED: $*" >&2
  diagnose
  exit 1
}

CREATED_HS_CRD=0
cleanup() {
  local rc=$?
  trap - EXIT
  stop_port_forwards
  if [ "$KEEP" -eq 1 ]; then
    say "Kept, as asked: namespaces $NAMESPACE and $NS_HS (context $CONTEXT)"
  else
    say "Cleaning up"
    kc -n "$NS_HS" delete homeserver --all --wait=true --timeout=120s >/dev/null 2>&1 || true
    helm --kube-context "$CONTEXT" -n "$NAMESPACE" uninstall "$RELEASE" --wait >/dev/null 2>&1 || true
    kc delete namespace "$NAMESPACE" "$NS_HS" --ignore-not-found --wait=true --timeout=180s 2>&1 || true
    if [ "$CREATED_HS_CRD" -eq 1 ]; then
      kc delete crd homeservers.hs.matrix.org --ignore-not-found 2>&1 || true
    fi
  fi
  if [ "$rc" -eq 0 ]; then say "PASSED"; else say "FAILED (exit $rc); see above"; fi
  exit "$rc"
}
trap cleanup EXIT
trap 'fail "a command failed unexpectedly (line $LINENO)"' ERR

# --- The chart, with its bridge operator ------------------------------------------------------

say "The chart with bridges.enabled (its default), image $IMAGE"
echo "context $CONTEXT, namespace $NAMESPACE"
if [ -n "$KIND_CLUSTER" ]; then
  run kind load docker-image "$IMAGE" --name "$KIND_CLUSTER" || fail "kind could not load $IMAGE"
fi
run kc create namespace "$NAMESPACE"
image_args=(--set-string "image.registry=$REGISTRY" --set-string "image.repository=$REPOSITORY")
if [ -n "$DIGEST" ]; then
  image_args+=(--set-string "image.digest=$DIGEST" --set-string "image.tag=")
else
  image_args+=(--set-string "image.tag=$TAG")
fi
if [ -n "$PULL_POLICY" ]; then image_args+=(--set-string "image.pullPolicy=$PULL_POLICY"); fi
mark
run helm --kube-context "$CONTEXT" -n "$NAMESPACE" install "$RELEASE" "$CHART" \
  --set serverName=smoke.invalid "${image_args[@]}" --wait --timeout "${TIMEOUT}s" >/dev/null \
  || fail "helm install did not reach Ready"
stamp "helm install --wait returned"
OPERATOR="$(k get deploy -l app.kubernetes.io/component=bridges-operator -o jsonpath='{.items[0].metadata.name}' 2>/dev/null || true)"
[ -n "$OPERATOR" ] || OPERATOR="$RELEASE-hs-bridges-operator"
rk get "deploy/$OPERATOR" "sts/$RELEASE-hs"
k logs "deploy/$OPERATOR" | grep -q 'bridge operator starting' \
  || fail "the bridge operator's log does not say it started"
echo "the bridge operator says: bridge operator starting (namespace $NAMESPACE)"

# --- A hand-written Bridge --------------------------------------------------------------------

say "A Bridge running a stand-in ($STAND_IN, port $STAND_IN_PORT)"
if [ -n "$KIND_CLUSTER" ] && docker image inspect "$STAND_IN" >/dev/null 2>&1; then
  run kind load docker-image "$STAND_IN" --name "$KIND_CLUSTER" >/dev/null
fi
stand_in_repo="${STAND_IN%:*}"
stand_in_tag="${STAND_IN##*:}"
bridge_yaml() { # tag
  cat <<EOF
apiVersion: hs.matrix.org/v1alpha1
kind: Bridge
metadata:
  name: smoke
spec:
  bridgeType: stand-in
  appserviceId: smoke-stand-in
  image:
    repository: $stand_in_repo
    tag: "$1"
    pullPolicy: IfNotPresent
  port: $STAND_IN_PORT
  filesSecret: smoke-files
  storage:
    size: 64Mi
EOF
}
echo '$ kubectl create secret generic smoke-files --from-literal=config.yaml=...'
k create secret generic smoke-files --from-literal=config.yaml='written: by the smoke test' >/dev/null
bridge_yaml "$stand_in_tag" | sed 's/^/  /'
mark
bridge_yaml "$stand_in_tag" | k apply -f - >/dev/null
bridge_state() {
  k get bridge smoke -o jsonpath='{.status.phase} {.status.conditions[0].reason}: {.status.conditions[0].message}'
}
watch_until bridge_state '^Ready ' '^Degraded '

say "What the operator made for it"
rk get deploy/smoke svc/smoke pvc/smoke-data
for obj in deploy/smoke svc/smoke pvc/smoke-data; do
  owner="$(k get "$obj" -o jsonpath='{.metadata.ownerReferences[0].kind}/{.metadata.ownerReferences[0].name} controller={.metadata.ownerReferences[0].controller}')"
  echo "$obj is owned by $owner"
  [ "$owner" = "Bridge/smoke controller=true" ] || fail "$obj is not owned by the Bridge"
done
files_log="$(k logs deploy/smoke -c files)"
echo "init container 'files': $files_log"
[[ "$files_log" == *"wrote /data/config.yaml"* ]] || fail "the init container did not copy the files Secret into /data"
endpoints="$(k get endpointslices -l kubernetes.io/service-name=smoke -o jsonpath='{.items[*].endpoints[*].addresses[*]}')"
echo "svc/smoke endpoints: $endpoints"
[ -n "$endpoints" ] || fail "the Service has no ready endpoint"
k get bridge smoke -o jsonpath='{"status: "}{.status}{"\n"}'

say "An image that does not exist: Degraded, with the kubelet's reason"
mark
echo "\$ kubectl patch bridge smoke: image tag no-such-tag-myelin-smoke"
k patch bridge smoke --type merge -p '{"spec":{"image":{"tag":"no-such-tag-myelin-smoke"}}}' >/dev/null
watch_until bridge_state '^Degraded (ErrImagePull|ImagePullBackOff)'

say "The image put back: Ready again"
mark
echo "\$ kubectl patch bridge smoke: image tag $stand_in_tag"
k patch bridge smoke --type merge -p "{\"spec\":{\"image\":{\"tag\":\"$stand_in_tag\"}}}" >/dev/null
watch_until bridge_state '^Ready '

say "Deleting the Bridge removes what was built from it"
mark
rk delete bridge smoke --wait=true
smoke_left() {
  echo "$(k get deploy,svc,pvc,pods -l app.kubernetes.io/name=myelin-bridge,app.kubernetes.io/instance=smoke \
    --no-headers 2>/dev/null | wc -l | tr -d ' ') objects left"
}
watch_until smoke_left '^0 objects left'
k delete secret smoke-files >/dev/null

# --- heisenbridge, deployed by the server itself ---------------------------------------------

if [ "$HEISENBRIDGE" -eq 1 ]; then
  say "heisenbridge offered with the cluster runtime, through the admin API"
  POD="$(k get pod -l app.kubernetes.io/instance="$RELEASE",app.kubernetes.io/name=hs -o jsonpath='{.items[0].metadata.name}')"
  token="$(k logs "$POD" | grep -oE 'setup#token=[A-Za-z0-9_-]+' | tail -1 | cut -d= -f2 || true)"
  [ -n "$token" ] || fail "no setup link in $POD's log"
  port_forward "$NAMESPACE" "$RELEASE-hs"
  setup="$(curl -sf -X POST "$BASE/api/v1/setup" -H 'content-type: application/json' \
    -d "{\"setup_token\":\"$token\",\"username\":\"smoke\",\"password\":\"smoke-$RANDOM-$RANDOM-passphrase\"}")" \
    || fail "the setup link did not create an administrator"
  ADMIN="$(printf '%s' "$setup" | jq -r .access_token)"
  echo "POST /api/v1/setup: signed in as $(printf '%s' "$setup" | jq -r .user_id)"
  api() { curl -s -H "authorization: Bearer $ADMIN" -H 'content-type: application/json' "$@"; }
  target="$(api "$BASE/api/v1/bridge-deployment-target")"
  echo "GET /api/v1/bridge-deployment-target: $target"
  [ "$(printf '%s' "$target" | jq -r .available)" = "true" ] || fail "the server says it cannot deploy bridges"
  mark
  offering="$(api -X PUT "$BASE/api/v1/bridge-offerings/heisenbridge" -d '{"runtime":"cluster"}')"
  echo "PUT /api/v1/bridge-offerings/heisenbridge {\"runtime\":\"cluster\"}: $offering"
  [ "$(printf '%s' "$offering" | jq -r .runtime)" = "cluster" ] || fail "the offering was not made with the cluster runtime"
  instance_state() {
    api "$BASE/api/v1/bridge-offerings/heisenbridge/instances/_" \
      | jq -r '[.state, (.deployment.name // "-"), (.deployment.phase // "-"), (.deployment.message // "-"), (.reason // "")] | join(" | ")'
  }
  # A quarter-second poll: `starting` can last less than a second once the pod is Ready.
  deadline=$((SECONDS + TIMEOUT * 2)); seen=""
  while :; do
    now="$(instance_state 2>/dev/null || true)"
    if [ "$now" != "$seen" ]; then stamp "$now"; seen="$now"; fi
    case "$now" in ready*) break ;; failed*) fail "the instance failed: $now" ;; esac
    [ "$SECONDS" -lt "$deadline" ] || fail "the instance is still '$now' after $((TIMEOUT * 2))s"
    sleep 0.25
  done
  instance="$(api "$BASE/api/v1/bridge-offerings/heisenbridge/instances/_")"
  printf '%s\n' "$instance" | jq .
  echo "the bridge manager's own account of it (the server's log; it has each step to the millisecond):"
  k logs "$POD" | jq -rR 'fromjson? | select(.target == "hs_bridges::manager")
      | "  \(.timestamp) \(.fields.message)\(if .fields.from then ": \(.fields.from) -> \(.fields.to)" else "" end)\(if (.fields.reason // "") != "" then " (\(.fields.reason))" else "" end)"'
  name="$(printf '%s' "$instance" | jq -r .deployment.name)"
  [ "$(printf '%s' "$instance" | jq -r .health)" = "healthy" ] || fail "ready but not healthy"
  rk get bridge "$name"
  pod="$(k get pods -l app.kubernetes.io/instance="$name" -o jsonpath='{.items[0].metadata.name}')"
  echo "the bridge runs in pod $pod, image $(k get pod "$pod" -o jsonpath='{.spec.containers[0].image}')"
  bridge_log="$(k logs "$pod" -c bridge)"
  printf '%s\n' "$bridge_log" | tail -5 | sed 's/^/  /'
  [[ "$bridge_log" == *"bridge is now running"* ]] || fail "heisenbridge's log does not say it is running"
  bot="$(api "$BASE/api/v1/users?limit=100" | jq -c '.items[] | select(.user_id == "@heisenbridge:smoke.invalid") | {user_id, appservice_id}')"
  echo "its bot, registered through the server with its own token: $bot"
  [ -n "$bot" ] || fail "@heisenbridge:smoke.invalid is not a user"

  say "Removing the instance and the offering"
  mark
  code="$(api -o /dev/null -w '%{http_code}' -X DELETE "$BASE/api/v1/bridge-offerings/heisenbridge/instances/_")"
  echo "DELETE .../instances/_: $code"
  [ "$code" = "204" ] || fail "removing the instance answered $code"
  instance_left() {
    echo "$(k get bridge,secret,deploy,svc,pvc,pods --no-headers 2>/dev/null | grep -c "$name") objects left"
  }
  watch_until instance_left '^0 objects left'
  code="$(api -o /dev/null -w '%{http_code}' -X DELETE "$BASE/api/v1/bridge-offerings/heisenbridge")"
  echo "DELETE /api/v1/bridge-offerings/heisenbridge: $code"
  [ "$code" = "204" ] || fail "removing the offering answered $code"
  stop_port_forwards
fi

# --- A Homeserver, through hs operator --homeservers -----------------------------------------

if [ "$HOMESERVER" -eq 1 ]; then
  say "hs operator --homeservers (deploy/operator) in $NS_HS"
  if ! kc get crd homeservers.hs.matrix.org >/dev/null 2>&1; then
    run kc apply --server-side -f "$ROOT/deploy/crds/homeserver.yaml"
    CREATED_HS_CRD=1
  fi
  run kc create namespace "$NS_HS"
  hs_image="$REGISTRY/$REPOSITORY${DIGEST:+@$DIGEST}${TAG:+:$TAG}"
  echo "\$ kubectl kustomize deploy/operator | <namespace $NS_HS, image $hs_image${PULL_POLICY:+, pullPolicy $PULL_POLICY}> | kubectl apply -f -"
  kubectl kustomize "$ROOT/deploy/operator" \
    | sed -e "s/^  namespace: myelin\$/  namespace: $NS_HS/" \
          -e "s#image: ghcr.io/brandon-dacrib/myelin:main#image: $hs_image#" \
          -e "s/imagePullPolicy: Always/imagePullPolicy: ${PULL_POLICY:-IfNotPresent}/" \
    | kh apply -f - >/dev/null
  # Debug for the operator's own targets, so the check at the end can see every Bridge the
  # Bridge controller was asked about, not only the ones it warned of.
  rkh set env deploy/myelin-operator RUST_LOG=info,hs_operator=debug >/dev/null
  rkh rollout status deploy/myelin-operator --timeout="${TIMEOUT}s"

  # The signing key, made by the image itself in a one-shot pod (no docker needed on the host).
  kh run keygen --image="$hs_image" --image-pull-policy="${PULL_POLICY:-IfNotPresent}" \
    --restart=Never --command -- /usr/local/bin/hs generate-signing-key >/dev/null
  for _ in $(seq 1 "$TIMEOUT"); do
    [ "$(kh get pod keygen -o jsonpath='{.status.phase}')" = "Succeeded" ] && break
    sleep 1
  done
  key="$(kh logs keygen)"
  [[ "$key" == ed25519\ * ]] || fail "hs generate-signing-key did not print a key"
  kh delete pod keygen --wait=false >/dev/null
  printf '%s\n' "$key" | kh create secret generic hs-signing-key --from-file=signing.key=/dev/stdin >/dev/null
  echo "signing key Secret hs-signing-key made with hs generate-signing-key ($(cut -d' ' -f1-2 <<<"$key") ...)"

  homeserver_yaml() { # tag-or-digest-line
    cat <<EOF
apiVersion: hs.matrix.org/v1alpha1
kind: Homeserver
metadata:
  name: hs
spec:
  serverName: hs-smoke.invalid
  image:
    repository: $REGISTRY/$REPOSITORY
    $1
    pullPolicy: ${PULL_POLICY:-IfNotPresent}
  storage:
    backend: embedded
    embedded:
      size: 1Gi
  signingKeySecretRef:
    name: hs-signing-key
    key: signing.key
EOF
  }
  if [ -n "$DIGEST" ]; then image_line="digest: \"$DIGEST\""; else image_line="tag: \"$TAG\""; fi
  homeserver_yaml "$image_line" | sed 's/^/  /'
  mark
  homeserver_yaml "$image_line" | kh apply -f - >/dev/null
  hs_state() {
    kh get homeserver hs -o jsonpath='{.status.phase} ready={.status.readyReplicas} {range .status.conditions[*]}{.type}={.status} {end}'
  }
  watch_until hs_state '^Ready ready=1 ' '^Degraded '

  say "What the operator made for it"
  rkh get sts/hs svc/hs svc/hs-headless cm/hs-config sa/hs pvc/data-hs-0
  for obj in sts/hs svc/hs svc/hs-headless cm/hs-config sa/hs; do
    owner="$(kh get "$obj" -o jsonpath='{.metadata.ownerReferences[0].kind}/{.metadata.ownerReferences[0].name}')"
    echo "$obj is owned by $owner"
    [ "$owner" = "Homeserver/hs" ] || fail "$obj is not owned by the Homeserver"
  done
  finalizers="$(kh get homeserver hs -o jsonpath='{.metadata.finalizers}')"
  echo "finalizers on the Homeserver: ${finalizers:-none (it has no adminApi, so there is no drain to undo on deletion)}"
  kh get homeserver hs -o jsonpath='{range .status.conditions[*]}  {.type}={.status} {.reason}: {.message}{"\n"}{end}'
  port_forward "$NS_HS" hs
  code="$(curl -s -o /dev/null -w '%{http_code}' "$BASE/health/ready")"
  echo "GET /health/ready through svc/hs: $code"
  [ "$code" = "200" ] || fail "the Homeserver's server is not ready"
  stop_port_forwards

  if [ -n "$KIND_CLUSTER" ]; then
    say "An image change, rolled through the StatefulSet's partition"
    roll="$REGISTRY/$REPOSITORY:smoke-roll"
    run docker tag "$IMAGE" "$roll"
    run kind load docker-image "$roll" --name "$KIND_CLUSTER" >/dev/null
    uid_before="$(kh get pod hs-0 -o jsonpath='{.metadata.uid}')"
    mark
    echo "\$ kubectl patch homeserver hs: image tag smoke-roll"
    kh patch homeserver hs --type merge -p '{"spec":{"image":{"tag":"smoke-roll","digest":null}}}' >/dev/null
    roll_state() {
      echo "$(hs_state)| partition=$(kh get sts hs -o jsonpath='{.spec.updateStrategy.rollingUpdate.partition}')" \
        "pod=$(kh get pod hs-0 -o jsonpath='{.metadata.uid} {.spec.containers[0].image} ready={.status.conditions[?(@.type=="Ready")].status}' 2>/dev/null)"
    }
    watch_until roll_state "^Ready ready=1 .*partition=1 pod=.* $roll ready=True"
    uid_after="$(kh get pod hs-0 -o jsonpath='{.metadata.uid}')"
    echo "pod hs-0 was $uid_before, is $uid_after"
    [ "$uid_before" != "$uid_after" ] || fail "the pod was not replaced"
  fi

  say "Deleting the Homeserver"
  mark
  rkh delete homeserver hs --wait=true --timeout="${TIMEOUT}s"
  hs_left() {
    echo "$(kh get sts,svc,cm,sa,pods -l app.kubernetes.io/instance=hs,app.kubernetes.io/managed-by=myelin-operator \
      --no-headers 2>/dev/null | wc -l | tr -d ' ') objects left"
  }
  watch_until hs_left '^0 objects left'
  echo "the data claim outlives it, as with the chart: $(kh get pvc data-hs-0 --no-headers 2>/dev/null | awk '{print $1, $2}')"

  # Both controllers ran in one namespace: the Bridge controller must not have taken the
  # Homeserver's pods (labelled managed-by=myelin-operator too) for those of a Bridge named
  # `hs`, as it did before 2026-10-01.
  strays="$(kh logs deploy/myelin-operator | grep -c 'Bridge.v1alpha1.hs.matrix.org/hs\.' || true)"
  echo "operator log lines (debug and up) about a Bridge named hs: $strays"
  [ "$strays" = "0" ] || fail "the Bridge controller reacted to the Homeserver's objects"
fi

say "All checks passed"
