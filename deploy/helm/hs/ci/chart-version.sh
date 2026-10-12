#!/usr/bin/env bash
#
# The version CD releases: the image tag, the chart version and its appVersion, and the `v*`
# tag the run creates at its end. Run by the `version` job in .github/workflows/cd.yml; a script
# so the rule is in one place and can be checked without a workflow run (`--self-test`).
#
# The rule (since 2026-10-12, every merge to `main` is a release; before, `main` published
# `<base>-main.<run>.g<sha>` pre-releases and only a hand-pushed tag released):
#
# - A `v*` tag releases the tag's version (`v0.2.0` -> 0.2.0).
# - A push to `main` releases the next patch of the line Chart.yaml names: Chart.yaml's
#   `version` itself when it is ahead of every `v*` tag (somebody raised it: a new line starts at
#   `.0`), else the patch after the newest tag. So with Chart.yaml at 0.3.0 and v0.3.0 tagged,
#   merges release 0.3.1, 0.3.2, ...; raising Chart.yaml (and Cargo.toml and web/package.json,
#   which name the same line) to 0.4.0 makes the next merge 0.4.0.
#
# The repository's own version files therefore name the *line* (`X.Y.0`), and CD derives the
# patch. The alternative, the release workflow committing a bump to `main` itself, was not
# taken: `main` is written by the serial merge queue (tools/merge-queue.sh) under its lock, a
# workflow's push would race it, and a push made with the workflow's token starts no CI or CD
# run of its own. The tag the run pushes at its end is made with that token too, so it starts
# no second run; the run that built the release is the run that tags it, once everything in
# it has succeeded. Runs on `main` are serialized (the workflow's concurrency group), so each
# run sees the tag the previous one made.
#
# Usage:
#
#   deploy/helm/hs/ci/chart-version.sh REF RUN_NUMBER SHA CHART_VERSION [TAG...]
#   deploy/helm/hs/ci/chart-version.sh --self-test
#
#   REF            `refs/tags/v1.2.3` or `refs/heads/main` (GITHUB_REF).
#   RUN_NUMBER     GITHUB_RUN_NUMBER (kept for the call site; unused by the rule now).
#   SHA            The full commit (GITHUB_SHA; unused by the rule now).
#   CHART_VERSION  Chart.yaml's `version`, X.Y.Z.
#   TAG...         Every `v*` tag in the repository (`git tag -l 'v*'`); others are ignored.
#
# Prints `version=X.Y.Z`, `app_version=X.Y.Z`, `line=X.Y` and `tag=vX.Y.Z` for "$GITHUB_OUTPUT".
# Exits non-zero, with the reason on stderr, for a version that is not SemVer.

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
    emit "${tag#v}"
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
    echo "note: v$newest is tagged and Chart.yaml says $chart, so this merge releases $base (raise Chart.yaml's version to start a new line)" >&2
  fi
  emit "$base"
}

# The four output lines for a release version (X.Y.Z, or X.Y.Z-pre from a hand-pushed tag).
emit() {
  local v="$1" major minor rest
  IFS=. read -r major minor rest <<<"$v"
  echo "version=$v"
  echo "app_version=$v"
  echo "line=$major.$minor"
  echo "tag=v$v"
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
  # No tags yet: Chart.yaml's version is the first release.
  check "0.1.0" refs/heads/main 7 "$sha" 0.1.0
  # A hand-pushed tag releases its version ...
  check "0.1.0" refs/tags/v0.1.0 8 "$sha" 0.1.0 v0.1.0
  # ... and the next merge is the patch after it.
  check "0.1.1" refs/heads/main 9 "$sha" 0.1.0 v0.1.0
  check "0.1.3" refs/heads/main 9 "$sha" 0.1.0 v0.1.0 v0.1.1 v0.1.2
  # Somebody raised Chart.yaml ahead of every tag: a new line starts at its .0.
  check "0.2.0" refs/heads/main 10 "$sha" 0.2.0 v0.1.0 v0.1.9
  # A tag later than Chart.yaml (Chart.yaml forgotten for two releases).
  check "0.3.1" refs/heads/main 11 "$sha" 0.1.0 v0.1.0 v0.3.0 v0.2.0
  # Numeric, not lexical: v0.10.0 is newer than v0.9.0.
  check "0.10.1" refs/heads/main 12 "$sha" 0.1.0 v0.9.0 v0.10.0
  # A release candidate counts by its core.
  check "0.2.1" refs/heads/main 13 "$sha" 0.2.0 v0.1.0 v0.2.0-rc.1
  check "0.2.0-rc.1" refs/tags/v0.2.0-rc.1 14 "$sha" 0.2.0 v0.2.0-rc.1
  # Tags that are not v<SemVer> are not releases.
  check "0.1.0" refs/heads/main 15 "$sha" 0.1.0 vnext v1 release-1
  # A tag helm could not publish fails rather than producing a broken chart.
  check "(error)" refs/tags/vnext 16 "$sha" 0.1.0
  check "(error)" refs/heads/main 17 "$sha" 0.1
  # The image the chart pulls is the release itself, and the tag names it.
  local out
  out="$(compute refs/heads/main 18 "$sha" 0.3.0 v0.3.0 2>/dev/null)"
  if [ "$out" = $'version=0.3.1\napp_version=0.3.1\nline=0.3\ntag=v0.3.1' ]; then
    echo "ok    app_version, line and tag follow the version"
  else
    echo "FAIL  outputs: $out"; failed=1
  fi
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
