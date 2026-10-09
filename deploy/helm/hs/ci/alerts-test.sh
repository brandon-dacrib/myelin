#!/usr/bin/env bash
#
# Prove the chart's alert rules are well-formed and fire on what they are written for: render
# templates/prometheusrule.yaml for a release that has every group (cluster mode on
# PostgreSQL, a local media claim), then `promtool check rules` on the rendered groups and
# `promtool test rules` with ci/alerts/tests.yaml, whose cases feed each alert the series it
# watches and assert the labels and summary an operator would see, and feed a healthy release
# and assert silence.
#
# promtool comes from a Prometheus image when it is not installed (quay.io/prometheus/prometheus;
# override with PROMETHEUS_IMAGE); CD's chart job runs this before publishing anything.
#
# Usage: deploy/helm/hs/ci/alerts-test.sh [--chart DIR]

set -euo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
CHART="$(cd "$HERE/.." && pwd)"
PROMETHEUS_IMAGE="${PROMETHEUS_IMAGE:-quay.io/prometheus/prometheus:v3.5.0}"
while [ $# -gt 0 ]; do
  case "$1" in
    --chart) CHART="$2"; shift 2 ;;
    *) echo "unknown option: $1" >&2; exit 2 ;;
  esac
done
command -v helm >/dev/null || { echo "helm is not installed" >&2; exit 2; }

WORK="$(mktemp -d "${TMPDIR:-/tmp}/alerts-test.XXXXXX")"
trap 'rm -rf "$WORK"' EXIT

echo "== Rendering the PrometheusRule (release myelin, namespace hs, cluster mode, PostgreSQL, media claim hs-media)"
helm template myelin "$CHART" --namespace hs --set fullnameOverride=myelin \
  --set serverName=example.org --set mode=cluster --set replicaCount=2 \
  --set storage.backend=postgres --set storage.postgres.host=db \
  --set media.storage.backend=local --set media.storage.local.existingClaim=hs-media \
  --set secrets.signingKey.existingSecret=example --set cluster.mesh.tls.existingSecret=example-mesh \
  --set prometheusRule.enabled=true \
  --show-only templates/prometheusrule.yaml > "$WORK/prometheusrule.yaml"
# promtool wants the rule file, which is the resource's `spec`.
python3 - "$WORK/prometheusrule.yaml" "$WORK/rules.yaml" <<'EOF'
import sys
lines = open(sys.argv[1]).read().splitlines()
start = next(i for i, l in enumerate(lines) if l.startswith("spec:"))
with open(sys.argv[2], "w") as out:
    for l in lines[start + 1:]:
        out.write(l[2:] + "\n" if l.startswith("  ") else l + "\n")
EOF
cp "$HERE/alerts/tests.yaml" "$WORK/tests.yaml"
echo "$(grep -c 'alert:' "$WORK/rules.yaml") alerts in $(grep -c '^  - name:' "$WORK/rules.yaml") groups"
cd "$WORK"

if command -v promtool >/dev/null; then
  promtool() { command promtool "$@"; }
  echo "promtool: $(command -v promtool)"
else
  echo "promtool: $PROMETHEUS_IMAGE"
  docker image inspect "$PROMETHEUS_IMAGE" >/dev/null 2>&1 || docker pull "$PROMETHEUS_IMAGE" >/dev/null
  promtool() { docker run --rm -v "$WORK:/work:ro" -w /work --entrypoint promtool "$PROMETHEUS_IMAGE" "$@"; }
fi

echo "== promtool check rules"
promtool check rules rules.yaml
echo "== promtool test rules"
promtool test rules tests.yaml
echo "== PASSED: the alert rules are well-formed and each fires on the series it watches"
