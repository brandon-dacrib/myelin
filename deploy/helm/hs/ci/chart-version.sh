#!/usr/bin/env bash
#
# The version CD publishes the chart under, and the image tag it pulls (its appVersion). Run by
# the `chart` job in .github/workflows/cd.yml; a script so the rule is in one place and can be
# checked without a workflow run (`--self-test`).
#
# The rule:
#
# - A `v*` tag publishes the tag's version (`v0.2.0` -> chart 0.2.0, appVersion 0.2.0).
# - A push to `main` publishes a pre-release, `<base>-main.<run number>.g<short sha>`, pulling the
#   image `sha-<commit>`. `<base>` is Chart.yaml's `version` -- unless a `v*` tag already
#   released that version or a later one, in which case it is the patch after the newest tag.
#
# Why the second clause: SemVer sorts a pre-release below the release it precedes, so once
# `v0.1.0` is tagged, `0.1.0-main.N` sorts below it and `helm install --devel` stops picking up
# `main` until somebody remembers to raise Chart.yaml's version. The base moving past the newest
# tag by itself makes that reminder unnecessary: `main` after `v0.1.0` publishes
# `0.1.1-main.N`, which sorts above `0.1.0`. Raising Chart.yaml's version (to 0.2.0, say) still
# works and wins whenever it is ahead of every tag. The alternative, the release workflow
# committing a Chart.yaml bump to `main` itself, was not taken: `main` is written by the serial
# merge queue (tools/merge-queue.sh) under its lock, a workflow's push would race it, and a push
# made with the workflow's token starts no CI or CD run of its own.
#
# A chart from `main` still needs `--devel`: that is what a pre-release is for, so a plain
# `helm install` takes releases only.
#
# Usage:
#
#   deploy/helm/hs/ci/chart-version.sh REF RUN_NUMBER SHA CHART_VERSION [TAG...]
#   deploy/helm/hs/ci/chart-version.sh --self-test
#
#   REF            `refs/tags/v1.2.3` or `refs/heads/main` (GITHUB_REF).
#   RUN_NUMBER     GITHUB_RUN_NUMBER.
#   SHA            The full commit (GITHUB_SHA).
#   CHART_VERSION  Chart.yaml's `version`, X.Y.Z.
#   TAG...         Every `v*` tag in the repository (`git tag -l 'v*'`); others are ignored.
#
# Prints two lines, `version=...` and `app_version=...`, for "$GITHUB_OUTPUT". Exits non-zero,
# with the reason on stderr, for a version that is not SemVer.

set -euo pipefail

semver_core='^[0-9]+\.[0-9]+\.[0-9]+$'
semver_tag='^v([0-9]+\.[0-9]+\.[0-9]+)(-[0-9A-Za-z.-]+)?(\+[0-9A-Za-z.-]+)?$'

# The greater of two X.Y.Z versions, numerically per field.
max_core() {
  printf '%s\n%s\n' "$1" "$2" | sort -t. -k1,1n -k2,2n -k3,3n | tail -1
}

compute() {
  local ref="$1" run="$2" sha="$3" chart="$4"
  shift 4
  if [[ "$ref" == refs/tags/v* ]]; then
    local tag="${ref#refs/tags/}"
    if ! [[ "$tag" =~ $semver_tag ]]; then
      echo "the tag $tag is not v<SemVer>; helm cannot publish it as a chart version" >&2
      return 1
    fi
    echo "version=${tag#v}"
    echo "app_version=${tag#v}"
    return 0
  fi
  if ! [[ "$chart" =~ $semver_core ]]; then
    echo "Chart.yaml's version '$chart' is not X.Y.Z" >&2
    return 1
  fi
  local base="$chart" newest="" t core
  for t in "$@"; do
    [[ "$t" =~ $semver_tag ]] || continue
    core="${BASH_REMATCH[1]}"
    if [ -z "$newest" ]; then newest="$core"; else newest="$(max_core "$newest" "$core")"; fi
  done
  if [ -n "$newest" ] && [ "$(max_core "$newest" "$chart")" = "$newest" ]; then
    # A tag has released Chart.yaml's version or a later one: the patch after the newest tag.
    local major minor patch
    IFS=. read -r major minor patch <<<"$newest"
    base="$major.$minor.$((patch + 1))"
    echo "note: v$newest is tagged and Chart.yaml says $chart, so main publishes $base pre-releases (raise Chart.yaml's version to choose another)" >&2
  fi
  echo "version=${base}-main.${run}.g${sha:0:7}"
  echo "app_version=sha-${sha}"
}

self_test() {
  local failed=0 sha=0123456789abcdef0123456789abcdef01234567
  check() {
    local want="$1" got
    shift
    got="$(compute "$@" 2>/dev/null | sed -n 's/^version=//p')" || got="(error)"
    if [ "$got" = "$want" ]; then
      printf 'ok    %-28s <- %s\n' "$want" "$*"
    else
      printf 'FAIL  want %s, got %s <- %s\n' "$want" "$got" "$*"
      failed=1
    fi
  }
  # No tags yet: Chart.yaml's version, as before.
  check "0.1.0-main.7.g0123456" refs/heads/main 7 "$sha" 0.1.0
  # The first release: v0.1.0 publishes 0.1.0 ...
  check "0.1.0" refs/tags/v0.1.0 8 "$sha" 0.1.0 v0.1.0
  # ... and main after it moves past it by itself, so `--devel` still picks main.
  check "0.1.1-main.9.g0123456" refs/heads/main 9 "$sha" 0.1.0 v0.1.0
  # Somebody raised Chart.yaml ahead of every tag: that wins.
  check "0.2.0-main.10.g0123456" refs/heads/main 10 "$sha" 0.2.0 v0.1.0
  # A tag later than Chart.yaml (Chart.yaml forgotten for two releases).
  check "0.3.1-main.11.g0123456" refs/heads/main 11 "$sha" 0.1.0 v0.1.0 v0.3.0 v0.2.0
  # Numeric, not lexical: v0.10.0 is newer than v0.9.0.
  check "0.10.1-main.12.g0123456" refs/heads/main 12 "$sha" 0.1.0 v0.9.0 v0.10.0
  # A release candidate counts by its core: main sorts above 0.2.0-rc.1 too.
  check "0.2.1-main.13.g0123456" refs/heads/main 13 "$sha" 0.2.0 v0.1.0 v0.2.0-rc.1
  check "0.2.0-rc.1" refs/tags/v0.2.0-rc.1 14 "$sha" 0.2.0 v0.2.0-rc.1
  # Tags that are not v<SemVer> are not releases.
  check "0.1.0-main.15.g0123456" refs/heads/main 15 "$sha" 0.1.0 vnext v1 release-1
  # A tag helm could not publish fails rather than producing a broken chart.
  check "(error)" refs/tags/vnext 16 "$sha" 0.1.0
  check "(error)" refs/heads/main 17 "$sha" 0.1
  # The appVersion of a main build is the image tag CD pushed for that commit.
  local app
  app="$(compute refs/heads/main 18 "$sha" 0.1.0 | sed -n 's/^app_version=//p')"
  if [ "$app" = "sha-$sha" ]; then echo "ok    app_version=sha-<commit> on main"; else echo "FAIL  app_version $app"; failed=1; fi
  if [ "$failed" -eq 0 ]; then echo "self-test passed"; else echo "self-test FAILED"; fi
  return "$failed"
}

if [ "${1:-}" = "--self-test" ]; then
  self_test
  exit $?
fi
if [ $# -lt 4 ]; then
  sed -n '2,/^$/p' "$0" | sed 's/^# \{0,1\}//' >&2
  exit 2
fi
compute "$@"
