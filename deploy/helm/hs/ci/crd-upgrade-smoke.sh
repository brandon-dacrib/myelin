#!/usr/bin/env bash
#
# Prove that `helm upgrade` keeps the Bridge CRD current (docs/decisions/0036) on a real API
# server: a cluster holding the CRD as an install from `crds/` left it (no Helm ownership, no
# `.spec.owner`) refuses a Bridge that sets `.spec.owner` with the message the server's bridge
# client reads ("field not declared in schema"); the chart from this checkout, installed with
# `--take-ownership`, adopts and updates it; the same Bridge is then accepted; and
# `helm uninstall` leaves the CRD and the Bridge in place (`helm.sh/resource-policy: keep`).
#
# Usage: deploy/helm/hs/ci/crd-upgrade-smoke.sh [--kind NAME | --context CTX] [--set K=V ...]
#
#   kind create cluster --name crd-upgrade
#   deploy/helm/hs/ci/crd-upgrade-smoke.sh --kind crd-upgrade
#
#   --kind NAME     Use the kind cluster NAME's context, and pull the Bridge's image here (with
#                   retries and fallbacks) and load it into the cluster, so the kubelet never
#                   pulls it (helper-image.sh says why). The Bridge's pod is never waited for,
#                   but a kubelet pull of the same image the operator smoke loads next would
#                   race it on the node.
#   --context CTX   The kubectl context. Default: kind-NAME with --kind, else the current.
#   --set K=V       Passed to both `helm install`s; repeatable. CD uses it to point the release
#                   at the image already loaded on its kind node (`image.tag=smoke`,
#                   `image.pullPolicy=Never`) so the server pod this install starts, and which
#                   the checks never wait for, pulls nothing from a registry.
#   --bridge-image IMG  The image the Bridge names. Default: public.ecr.aws/docker/library/nginx:alpine.
#
# Refuses to run on a cluster that already has the Bridge CRD: it replaces it, and deleting a
# CRD deletes every Bridge on the cluster. Cleans up after itself, the CRD included, so a
# smoke that needs a cluster without it can follow. Needs kubectl, helm (3.17 or later, for
# --take-ownership; with Helm 4 also --force-conflicts) and jq. Exit status 0 only if every
# check passed.

set -euo pipefail

CONTEXT=""
KIND_CLUSTER=""
BRIDGE_IMAGE="public.ecr.aws/docker/library/nginx:alpine"
SET_ARGS=()
while [ $# -gt 0 ]; do
  case "$1" in
    --kind) KIND_CLUSTER="$2"; shift 2 ;;
    --context) CONTEXT="$2"; shift 2 ;;
    --set) SET_ARGS+=(--set "$2"); shift 2 ;;
    --bridge-image) BRIDGE_IMAGE="$2"; shift 2 ;;
    *) sed -n '2,/^$/p' "$0" | sed 's/^# \{0,1\}//'; exit 2 ;;
  esac
done
if [ -n "$KIND_CLUSTER" ]; then : "${CONTEXT:=kind-$KIND_CLUSTER}"; fi
if [ -z "$CONTEXT" ]; then CONTEXT="$(kubectl config current-context)"; fi
HERE="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source-path=SCRIPTDIR
# shellcheck source=helper-image.sh
. "$HERE/helper-image.sh"
ROOT="$(cd "$(dirname "$0")/../../../.." && pwd)"
NS="crd-upgrade-$RANDOM"
CRD=bridges.hs.matrix.org
kc() { kubectl --context "$CONTEXT" "$@"; }
say() { printf '\n== %s\n' "$*"; }
fail() { echo "FAIL: $*" >&2; exit 1; }

kc get crd "$CRD" >/dev/null 2>&1 && fail "$CRD already exists on $CONTEXT; this script replaces it"
BRIDGE_IMAGE="$(helper_image "$BRIDGE_IMAGE" "$KIND_CLUSTER")" || fail "could not pull the Bridge's image $BRIDGE_IMAGE"
BRIDGE_REPOSITORY="${BRIDGE_IMAGE%:*}"
BRIDGE_TAG="${BRIDGE_IMAGE##*:}"
cleanup() {
  helm --kube-context "$CONTEXT" -n "$NS" uninstall myelin >/dev/null 2>&1 || true
  kc delete namespace "$NS" --ignore-not-found --wait=false >/dev/null 2>&1 || true
  kc delete crd "$CRD" --ignore-not-found >/dev/null 2>&1 || true
}
trap cleanup EXIT

bridge() {
  cat <<YAML
apiVersion: hs.matrix.org/v1alpha1
kind: Bridge
metadata: { name: bridge-smoke, namespace: $NS }
spec:
  bridgeType: heisenbridge
  appserviceId: heisenbridge
  owner: "@alice:example.org"
  image: { repository: $BRIDGE_REPOSITORY, tag: "$BRIDGE_TAG", pullPolicy: IfNotPresent }
  port: 80
  filesSecret: bridge-smoke-files
YAML
}

say "an old CRD, as a first install from crds/ left it: no Helm ownership, no .spec.owner"
kc create --dry-run=client -o json -f "$ROOT/deploy/crds/bridge.yaml" \
  | jq 'del(.spec.versions[0].schema.openAPIV3Schema.properties.spec.properties.owner)
        | .spec.versions[0].additionalPrinterColumns |= map(select(.name != "Owner"))' \
  | kc create -f -
kc wait --for condition=established "crd/$CRD" --timeout=60s
kc create namespace "$NS"

say "the old CRD refuses a Bridge with .spec.owner, as the demo's did"
if out="$(bridge | kc apply --server-side --field-manager myelin-homeserver -f - 2>&1)"; then
  fail "the old CRD accepted .spec.owner: $out"
fi
echo "$out"
grep -q '.spec.owner: field not declared in schema' <<<"$out" || fail "unexpected refusal: $out"

say "helm install without --take-ownership stops on the unowned CRD"
if out="$(helm --kube-context "$CONTEXT" -n "$NS" install myelin "$ROOT/deploy/helm/hs" \
    --set serverName=example.org "${SET_ARGS[@]}" 2>&1)"; then
  fail "helm installed over an unowned CRD: $out"
fi
echo "$out" | tail -2

say "helm install --take-ownership adopts it and brings it up to date"
# Helm 4 applies server-side, and the old CRD's fields belong to whoever created it
# (`kubectl-create` here, Helm 3's own manager on a real install): --force-conflicts takes them.
force=()
helm version --short | grep '^v4' >/dev/null && force=(--force-conflicts)
helm --kube-context "$CONTEXT" -n "$NS" install myelin "$ROOT/deploy/helm/hs" \
  --set serverName=example.org "${SET_ARGS[@]}" --take-ownership "${force[@]}" >/dev/null
kc get crd "$CRD" -o json | jq -e '
  .spec.versions[0].schema.openAPIV3Schema.properties.spec.properties.owner != null
  and .metadata.annotations["helm.sh/resource-policy"] == "keep"
  and .metadata.annotations["meta.helm.sh/release-name"] == "myelin"
  and .metadata.labels["app.kubernetes.io/managed-by"] == "Helm"' >/dev/null \
  || fail "the CRD was not updated and adopted: $(kc get crd "$CRD" -o yaml | head -30)"
echo "the CRD declares .spec.owner, is the release's, and is kept on uninstall"
kc wait --for condition=established "crd/$CRD" --timeout=60s
bridge | kc apply --server-side --field-manager myelin-homeserver -f -

say "helm uninstall leaves the CRD and the Bridge"
helm --kube-context "$CONTEXT" -n "$NS" uninstall myelin >/dev/null
kc get crd "$CRD" >/dev/null || fail "helm uninstall deleted the CRD"
kc -n "$NS" get bridge bridge-smoke >/dev/null || fail "helm uninstall deleted the Bridge"
echo "both still there"

say "PASS"
