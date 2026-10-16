# shellcheck shell=bash
#
# The images the smokes' helper pods run (a backup helper with `sh` and `tar`, a traffic pod
# with `python3`, a PostgreSQL, the Bridge stand-in), resolved so that the kubelet never pulls
# them. Sourced by every smoke under deploy/helm/hs/ci/ and deploy/operator/ci/; not a program.
#
# Why: a pod spec naming `public.ecr.aws/docker/library/python:3.12-alpine` makes the kubelet
# pull it, anonymously, once, with no fallback. On CD run 38976232395 (2026-10-16) that single
# pull failed ("httpReadSeeker: failed open: unexpected stat", ErrImagePull, ImagePullBackOff)
# and the backup smoke timed out on `pod/backup-helper`; on the runs for v0.3.28 and v0.3.29 the
# same happened to the operator smoke's stand-in, and on the run for 0d4afb81 public.ecr.aws
# refused outright ("toomanyrequests: Data limit exceeded"), which no retry gets past. A kubelet
# pull is also what broke `pullPolicy: Never` for the server image on run 38016787579: bytes the
# kubelet pulled under one name made the names kind had imported for the same image ID
# ErrImageNeverPull. So every image a smoke pod runs goes through `helper_image` below: pulled
# on the runner with retries, from the mirror or Docker Hub when the primary registry refuses,
# loaded into the kind cluster with `kind load docker-image`, and run with
# `imagePullPolicy: IfNotPresent`. IfNotPresent, not Never, because the stand-in Bridge of the
# operator smoke must turn a tag that does not exist into ErrImagePull (a kubelet that may not
# pull answers ErrImageNeverPull instead), and because on a cluster that is not kind, where
# nothing is loaded, IfNotPresent lets the kubelet pull as before.
#
# Usage, in a script with `set -euo pipefail`:
#
#   HERE="$(cd "$(dirname "$0")" && pwd)"
#   # shellcheck source=helper-image.sh
#   . "$HERE/helper-image.sh"
#   HELPER_IMAGE="$(helper_image "$HELPER_IMAGE" "$KIND_CLUSTER")" || fail "no helper image"
#
#   helper_image IMAGE [KIND_CLUSTER]
#       With KIND_CLUSTER: pulls IMAGE here (helper_image_pull), loads it into that cluster
#       (helper_image_load) and prints the name to put in the pod spec, which is IMAGE itself.
#       Without one (a cluster that is not kind): prints IMAGE and does nothing else, since
#       there is no node to load it on; the kubelet pulls it. Progress goes to stderr, so the
#       name can be captured with `$(...)`. Returns 1, with the reason on stderr, if the image
#       could be had from nowhere or could not be loaded.
#   helper_image_pull IMAGE
#       Nothing if the local daemon already has IMAGE by name. Otherwise `docker pull`s it,
#       three attempts with a growing pause, and when that fails the same path on
#       mirror.gcr.io and then docker.io (IMAGE names a Docker Hub image: `public.ecr.aws/docker/
#       library/NAME` is Docker Hub's `library/NAME`), tagging whatever answered as IMAGE so the
#       pod spec's name does not change. Returns 1 if every source failed.
#   helper_image_load IMAGE KIND_CLUSTER
#       `kind load docker-image IMAGE` into that cluster, unless its control-plane node already
#       holds the image by that name (crictl), so a second smoke on the same cluster does not
#       export and import it again. Returns 1 if kind could not load it.
#   helper_image_on_node IMAGE KIND_CLUSTER
#       True if the cluster's control-plane node holds IMAGE by name.
#
# Needs docker; kind with a KIND_CLUSTER.

# The path of IMAGE on each registry that carries Docker Hub: `library/python:3.12-alpine`.
_helper_image_hub_path() {
  local ref="$1"
  case "$ref" in
    public.ecr.aws/docker/*) ref="${ref#public.ecr.aws/docker/}" ;;
    mirror.gcr.io/*) ref="${ref#mirror.gcr.io/}" ;;
    docker.io/*) ref="${ref#docker.io/}" ;;
    */*)
      # Another registry (ghcr.io/...): no Docker Hub path to fall back to.
      return 1 ;;
  esac
  case "$ref" in */*) ;; *) ref="library/$ref" ;; esac
  printf '%s\n' "$ref"
}

helper_image_pull() {
  local image="$1" path source attempt
  local -a sources=("$image")
  if docker image inspect "$image" >/dev/null 2>&1; then
    echo "$image is in the local daemon" >&2
    return 0
  fi
  if path="$(_helper_image_hub_path "$image")"; then
    for source in "public.ecr.aws/docker/$path" "mirror.gcr.io/$path" "docker.io/$path"; do
      [ "$source" = "$image" ] || sources+=("$source")
    done
  fi
  for source in "${sources[@]}"; do
    for attempt in 1 2 3; do
      if docker pull -q "$source" >/dev/null 2>&1; then
        [ "$source" = "$image" ] || docker tag "$source" "$image"
        echo "$image pulled from $source" >&2
        return 0
      fi
      echo "pulling $source failed (attempt $attempt of 3)" >&2
      [ "$attempt" = 3 ] || sleep $((attempt * 5))
    done
  done
  echo "could not pull $image from any of: ${sources[*]}" >&2
  return 1
}

helper_image_on_node() {
  docker exec "$2-control-plane" crictl inspecti "$1" >/dev/null 2>&1
}

helper_image_load() {
  local image="$1" cluster="$2"
  if helper_image_on_node "$image" "$cluster"; then
    echo "$image is on node $cluster-control-plane already" >&2
    return 0
  fi
  echo "\$ kind load docker-image $image --name $cluster" >&2
  if ! kind load docker-image "$image" --name "$cluster" >&2; then
    echo "kind could not load $image into cluster $cluster" >&2
    return 1
  fi
}

helper_image() {
  local image="$1" cluster="${2:-}"
  if [ -n "$cluster" ]; then
    helper_image_pull "$image" || return 1
    helper_image_load "$image" "$cluster" || return 1
  fi
  printf '%s\n' "$image"
}
