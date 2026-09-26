#!/usr/bin/env bash
#
# Install the chart with one value and a given image, and prove the result is a server an
# operator could use: the pod goes Ready, the log offers a setup link, /health/ready answers
# 200, /admin/ is the management interface (not the page that says it was left out), and the
# setup link actually creates the first administrator. Then uninstall and delete the namespace.
#
# CD runs this on a kind cluster against the image it has just built, before that image is
# tagged and before the chart is published (.github/workflows/cd.yml, job `image`). It is a
# script rather than workflow steps so the same check runs by hand against any cluster, and so
# its output is a transcript: every command it runs is echoed with a `$` in front of it.
#
# Usage:
#
#   deploy/helm/hs/ci/install-smoke.sh IMAGE [options]
#
#   IMAGE              The image to run, as `docker` would name it: `myelin:smoke`,
#                      `ghcr.io/brandon-dacrib/myelin:main`, `ghcr.io/.../myelin@sha256:...`.
#                      It is split into the chart's image.registry / image.repository and
#                      image.tag (or image.digest) by Docker's own normalization rule, so
#                      `myelin:smoke` becomes docker.io/library/myelin:smoke -- which is what a
#                      kind node calls that image after `kind load docker-image myelin:smoke`.
#   --kind NAME        Load IMAGE from the local Docker daemon into the kind cluster NAME first,
#                      install into its context (kind-NAME), and set image.pullPolicy=Never so
#                      the node runs exactly what was loaded and never asks a registry.
#   --context CTX      The kubectl context to install into. Default: kind-NAME with --kind,
#                      otherwise the current context.
#   --namespace NS     The namespace to create, install into and delete afterwards.
#                      Default: hs-smoke-<random>.
#   --pull-policy P    image.pullPolicy. Default: Never with --kind, otherwise the chart's own
#                      default (Always for a tag, IfNotPresent for a digest).
#   --set K=V          Passed to `helm install` as `--set K=V`; repeatable. For a hand run that
#                      needs, say, a storage class.
#   --timeout DUR      How long `helm install --wait` waits for Ready. Default: 5m.
#   --local-port PORT  The local end of the port-forward. Default: 18008.
#   --keep             Leave the release and the namespace in place for a look afterwards.
#   --chart DIR        The chart to install. Default: the one this script lives in.
#
# Examples:
#
#   # What CD does: the image built a moment ago, on a throwaway kind cluster.
#   docker buildx build --load -f deploy/Dockerfile -t myelin:smoke .
#   kind create cluster --name smoke
#   deploy/helm/hs/ci/install-smoke.sh myelin:smoke --kind smoke
#   kind delete cluster --name smoke
#
#   # The published image, on whatever cluster kubectl points at, in a namespace of your choosing.
#   deploy/helm/hs/ci/install-smoke.sh ghcr.io/brandon-dacrib/myelin:main --namespace hs-smoke
#
# Exit status is 0 only if every check passed. On failure the pod's description, events and
# log are printed before the cleanup runs, so a CI log has what it needs.
#
# Needs kubectl, helm and curl; kind too with --kind.

set -euo pipefail

usage() { sed -n '2,/^$/p' "$0" | sed 's/^# \{0,1\}//'; }

# --- Arguments -------------------------------------------------------------------------------

IMAGE=""
KIND_CLUSTER=""
CONTEXT=""
NAMESPACE=""
PULL_POLICY=""
TIMEOUT="5m"
LOCAL_PORT="18008"
KEEP=0
CHART="$(cd "$(dirname "$0")/.." && pwd)"
EXTRA_SETS=()
RELEASE="myelin"

while [ $# -gt 0 ]; do
  case "$1" in
    --kind) KIND_CLUSTER="$2"; shift 2 ;;
    --context) CONTEXT="$2"; shift 2 ;;
    --namespace) NAMESPACE="$2"; shift 2 ;;
    --pull-policy) PULL_POLICY="$2"; shift 2 ;;
    --set) EXTRA_SETS+=(--set "$2"); shift 2 ;;
    --timeout) TIMEOUT="$2"; shift 2 ;;
    --local-port) LOCAL_PORT="$2"; shift 2 ;;
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
for tool in kubectl helm curl; do
  command -v "$tool" >/dev/null || { echo "$tool is not installed" >&2; exit 2; }
done
if [ -n "$KIND_CLUSTER" ]; then
  command -v kind >/dev/null || { echo "kind is not installed, and --kind was given" >&2; exit 2; }
  : "${CONTEXT:=kind-$KIND_CLUSTER}"
  : "${PULL_POLICY:=Never}"
fi
if [ -z "$CONTEXT" ]; then
  CONTEXT="$(kubectl config current-context)"
fi
if [ -z "$NAMESPACE" ]; then
  # Six hex characters. Not `tr -dc ... </dev/urandom | head -c 6`: head closing the pipe gives
  # tr a SIGPIPE, which under `pipefail` is exit 141 before the script has printed a line.
  NAMESPACE="hs-smoke-$(od -An -N3 -tx1 /dev/urandom | tr -d ' \n')"
fi

# --- The image reference, split the way the chart wants it -----------------------------------
#
# Docker's rule: the first path component is a registry only if it has a `.` or a `:` in it or
# is `localhost`; otherwise the registry is docker.io, and a repository with no `/` in it is
# under `library/`. A tag is whatever follows the last `:` after the last `/`; a digest follows
# `@`. The chart composes registry/repository:tag (or @digest) back together in
# templates/_helpers.tpl, `hs.image`.

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
k() { kubectl --context "$CONTEXT" -n "$NAMESPACE" "$@"; }
# `run` for kubectl, printed without the context and namespace noise every line would carry.
rk() { printf '$ kubectl %s\n' "$*"; k "$@"; }
http_code() { curl -s -o /dev/null -w '%{http_code}' "$@"; }
# A setup token in a CI log is a token for a cluster that no longer exists, but the transcript
# should not teach anyone to paste one.
redact() { sed -E 's/(token=)[A-Za-z0-9_-]+/\1<redacted>/g'; }

# Seconds since the epoch for a Kubernetes RFC 3339 timestamp, on GNU and BSD date alike.
epoch() {
  date -u -d "$1" +%s 2>/dev/null || date -u -j -f '%Y-%m-%dT%H:%M:%SZ' "$1" +%s 2>/dev/null || echo 0
}

PF_PID=""
DIAGNOSED=0
diagnose() {
  [ "$DIAGNOSED" -eq 1 ] && return 0
  DIAGNOSED=1
  say "What the cluster says (diagnostics on failure)"
  k get pods,pvc,svc -o wide 2>&1 || true
  echo
  k get events --sort-by=.lastTimestamp 2>&1 | tail -40 || true
  echo
  k describe pod -l "app.kubernetes.io/instance=$RELEASE" 2>&1 | tail -60 || true
  echo
  k logs -l "app.kubernetes.io/instance=$RELEASE" --tail=200 2>&1 | redact || true
}
fail() {
  echo
  echo "FAILED: $*" >&2
  diagnose
  exit 1
}
cleanup() {
  local rc=$?
  trap - EXIT
  if [ -n "$PF_PID" ]; then kill "$PF_PID" 2>/dev/null || true; wait "$PF_PID" 2>/dev/null || true; fi
  if [ "$KEEP" -eq 1 ]; then
    say "Kept, as asked: release $RELEASE in namespace $NAMESPACE (context $CONTEXT)"
    echo "helm --kube-context $CONTEXT -n $NAMESPACE uninstall $RELEASE && kubectl --context $CONTEXT delete namespace $NAMESPACE"
  else
    say "Cleaning up"
    helm --kube-context "$CONTEXT" -n "$NAMESPACE" uninstall "$RELEASE" --wait 2>&1 || true
    # The data volume is kept by helm uninstall on purpose (storage.embedded.resourcePolicy);
    # deleting the namespace is what removes it.
    kubectl --context "$CONTEXT" delete namespace "$NAMESPACE" --wait=true --timeout=120s 2>&1 || true
  fi
  if [ "$rc" -eq 0 ]; then
    say "PASSED: the chart installed $IMAGE with one value, and it is a server"
  else
    say "FAILED (exit $rc); see above"
  fi
  exit "$rc"
}
trap cleanup EXIT
trap 'fail "a command failed unexpectedly (line $LINENO)"' ERR

# --- Go --------------------------------------------------------------------------------------

say "Installing $IMAGE with the chart at $CHART"
echo "context $CONTEXT, namespace $NAMESPACE, release $RELEASE"
echo "image.registry=$REGISTRY image.repository=$REPOSITORY image.tag=$TAG image.digest=$DIGEST image.pullPolicy=${PULL_POLICY:-(chart default)}"

if [ -n "$KIND_CLUSTER" ]; then
  say "Loading the image into kind cluster $KIND_CLUSTER"
  run kind load docker-image "$IMAGE" --name "$KIND_CLUSTER" || fail "kind could not load $IMAGE; is it in the local Docker daemon?"
fi

say "Creating the namespace"
run kubectl --context "$CONTEXT" create namespace "$NAMESPACE"

say "helm install, one value (serverName), plus the image under test"
image_args=(--set-string "image.registry=$REGISTRY" --set-string "image.repository=$REPOSITORY")
if [ -n "$DIGEST" ]; then
  image_args+=(--set-string "image.digest=$DIGEST" --set-string "image.tag=")
else
  image_args+=(--set-string "image.tag=$TAG")
fi
if [ -n "$PULL_POLICY" ]; then image_args+=(--set-string "image.pullPolicy=$PULL_POLICY"); fi
install_started=$SECONDS
if ! run helm --kube-context "$CONTEXT" -n "$NAMESPACE" install "$RELEASE" "$CHART" \
      --set serverName=smoke.invalid \
      "${image_args[@]}" "${EXTRA_SETS[@]+"${EXTRA_SETS[@]}"}" \
      --wait --timeout "$TIMEOUT"; then
  fail "helm install did not reach Ready within $TIMEOUT"
fi
install_seconds=$((SECONDS - install_started))
echo "helm install returned after ${install_seconds}s"

say "What got installed"
rk get pods,pvc,svc -o wide
STS="$(k get statefulset -l "app.kubernetes.io/instance=$RELEASE" -o jsonpath='{.items[0].metadata.name}')"
[ -n "$STS" ] || fail "no StatefulSet carries app.kubernetes.io/instance=$RELEASE"
POD="$STS-0"
running_image="$(k get pod "$POD" -o jsonpath='{.spec.containers[0].image}')"
echo "pod $POD runs image $running_image"
case "$running_image" in
  "$REGISTRY/$REPOSITORY:$TAG"|"$REGISTRY/$REPOSITORY@$DIGEST") ;;
  *) fail "the pod runs $running_image, not the image this script was given" ;;
esac

say "How long the boot took (from the pod's own timestamps and events)"
created="$(k get pod "$POD" -o jsonpath='{.metadata.creationTimestamp}')"
started="$(k get pod "$POD" -o jsonpath='{.status.containerStatuses[0].state.running.startedAt}')"
ready="$(k get pod "$POD" -o jsonpath='{.status.conditions[?(@.type=="Ready")].lastTransitionTime}')"
echo "pod created            $created"
echo "container started      $started   (+$(( $(epoch "$started") - $(epoch "$created") ))s: scheduling, volume, image)"
echo "pod Ready              $ready   (+$(( $(epoch "$ready") - $(epoch "$started") ))s after the container started)"
echo "helm install --wait    ${install_seconds}s end to end"
echo
echo "events, oldest first (an 'Unhealthy ... Startup probe failed' here is the first probe"
echo "arriving before the listener is bound; the startup probe absorbs it):"
k get events --sort-by=.lastTimestamp \
  -o custom-columns='AGE:.lastTimestamp,TYPE:.type,REASON:.reason,OBJECT:.involvedObject.name,MESSAGE:.message' \
  | grep -v '^AGE' | sed 's/^/  /' || true

say "The log, and the setup link in it"
log="$(k logs "$POD")"
printf '%s\n' "$log" | redact | sed 's/^/  /'
# The chart's ConfigMap asks for JSON logs, so the link is `"setup_link":"http://..."` here
# where `docker run`'s default format prints `setup_link=http://...`; match the URL itself.
# Without publicBaseUrl it is rooted at http://localhost:8008, which the port-forward below
# makes true (NOTES.txt says the same to the operator).
setup_link="$(printf '%s\n' "$log" | grep 'setup_link' | grep -oE 'http://localhost:8008/admin/setup#token=[A-Za-z0-9_-]+' | head -1 || true)"
if [ -z "$setup_link" ]; then
  fail "the first boot did not log a setup_link of http://localhost:8008/admin/setup#token=... (a pod with no publicBaseUrl should)"
fi
setup_token="${setup_link##*token=}"
echo "found a setup link rooted at http://localhost:8008, ${#setup_token} characters of token"

say "Port-forwarding svc/$STS to 127.0.0.1:$LOCAL_PORT"
k port-forward "svc/$STS" "$LOCAL_PORT:8008" --address 127.0.0.1 >/dev/null 2>&1 &
PF_PID=$!
base="http://127.0.0.1:$LOCAL_PORT"
for _ in $(seq 1 30); do
  if [ "$(http_code "$base/health/live")" = "200" ]; then break; fi
  sleep 1
done
[ "$(http_code "$base/health/live")" = "200" ] || fail "nothing answered /health/live through the port-forward within 30s"

say "Checks through the port-forward"
code="$(http_code "$base/health/ready")"
echo "GET /health/ready               $code"
[ "$code" = "200" ] || fail "/health/ready answered $code, not 200"

body="$(curl -sf "$base/_matrix/client/versions")" || fail "/_matrix/client/versions did not answer 200"
echo "GET /_matrix/client/versions    200 ${body:0:80}..."

admin="$(curl -sf "$base/admin/")" || fail "/admin/ did not answer 200"
if ! printf '%s' "$admin" | grep -q '<div id="root">'; then
  echo "/admin/ served:" >&2
  printf '%s\n' "$admin" | head -20 >&2
  fail "/admin/ is not the management interface (no <div id=\"root\">)"
fi
echo "GET /admin/                     200 and it is the interface (<div id=\"root\"> is in it)"

status="$(curl -sf "$base/api/v1/setup")" || fail "/api/v1/setup did not answer 200"
echo "GET /api/v1/setup               200 $status"
printf '%s' "$status" | grep -q '"needs_setup":true' || fail "a fresh server should say needs_setup:true"

say "Claiming the server through the setup link, as the operator would"
# The link the NOTES tell the operator to open. The interface POSTs the token from its fragment
# with a username and a password; this does the same and expects a signed-in session back.
created_body="$(curl -s -w '\n%{http_code}' -X POST "$base/api/v1/setup" \
  -H 'content-type: application/json' \
  -d "{\"setup_token\":\"$setup_token\",\"username\":\"smoke\",\"password\":\"smoke-$(date +%s)-passphrase\"}")"
code="${created_body##*$'\n'}"
created_json="${created_body%$'\n'*}"
echo "POST /api/v1/setup              $code $(printf '%s' "$created_json" | sed -E 's/"access_token":"[^"]*"/"access_token":"<redacted>"/')"
[ "$code" = "201" ] || fail "the setup link did not create the first administrator (expected 201)"
printf '%s' "$created_json" | grep -q '"user_id":"@smoke:smoke.invalid"' || fail "the session is not for @smoke:smoke.invalid"
status="$(curl -sf "$base/api/v1/setup")" || fail "/api/v1/setup did not answer 200 after setup"
echo "GET /api/v1/setup               200 $status"
printf '%s' "$status" | grep -q '"needs_setup":false' || fail "after setup the server should say needs_setup:false"

say "All checks passed"
