# Installing and upgrading Myelin on Kubernetes

The Helm commands for each step, in order: install, upgrade, roll back, uninstall. What each
value means is documented in `deploy/helm/hs/values.yaml`; what has been run on real clusters is
in `docs/status/12-platform-and-kubernetes.md`.

Every command below names the release `myelin` in the namespace `myelin`. Substitute your own;
add `--kube-context <context>` if you have more than one cluster.

**Verified 2026-10-12** on a kind cluster (Kubernetes 1.33, Helm 4.3.0) with the published
charts: install 0.2.0 (Ready in 31 s), upgrade to 0.3.0 with the commands in
[Upgrade](#upgrade) (signing key unchanged, `/_matrix/federation/v1/version` 0.2.0 then 0.3.0,
no `ERROR` lines), `helm rollback` to 0.2.0 (key unchanged again), `helm uninstall` (the data
volume and the `Bridge` CRD left in place). The standing demo is upgraded the same way (its values
from `helm get values`, passed with `-f`).

## What you need

- Kubernetes with a default StorageClass (the single-node install claims one 10 GiB volume).
- Helm 3.17 or later, or Helm 4. Older Helm works for a new install; an upgrade from a chart
  before 2026-10-09 needs 3.17's `--take-ownership` (see [one-time steps](#one-time-steps)).
- For a server other people reach: a DNS name, an Ingress controller or Gateway, and a
  certificate (cert-manager or your own).

## Which chart

Since 2026-10-12 every merge to `main` is a release `X.Y.Z`: the chart, the image
`ghcr.io/brandon-dacrib/myelin:X.Y.Z` (the chart's default), the tag `vX.Y.Z` and a GitHub
release all come from that one commit. A release is ready once its tag and GitHub release
exist (`gh release view vX.Y.Z`; CD makes both last, after the chart and image are published).

**Install and upgrade from a tagged release, always.** Every command below sets the version
once and uses it for everything:

```sh
V=0.3.0        # the release: gh release list, or https://github.com/brandon-dacrib/myelin/releases
```

| What | From the release `$V` |
|---|---|
| The chart | `oci://ghcr.io/brandon-dacrib/charts/hs --version "$V"` |
| The image | `ghcr.io/brandon-dacrib/myelin:$V`, the chart's default (its `appVersion`) |
| A values file shipped in the repository (`deploy/demo/values-bridges.yaml`, …) | `-f "https://raw.githubusercontent.com/brandon-dacrib/myelin/v$V/<path>"` |

Not the chart from a checkout (`deploy/helm/hs`): its `appVersion` is `main`, a moving image, so
an upgrade that changes nothing but what `main` points at does not restart the pod, and a
checkout that is not the tag runs templates the release did not test. A checkout's chart is for
working on the chart. Without `--version`, Helm takes the newest release, which is not the same
twice.

**If your values pin `image.tag`**, which installs from a checkout did, that pin outlives every
upgrade: the new chart runs the old image. Check with
`helm -n myelin get values myelin -o yaml | grep -A3 '^image:'`. If `tag:` is there, pass
`--set image.tag="$V"` on every upgrade, as below, or drop the pin from your values file once.

`CHANGELOG.md` says what each line changes; a release's own notes are on its GitHub release.

## Install

### The smallest install

```sh
V=0.3.0
helm install myelin oci://ghcr.io/brandon-dacrib/charts/hs --version "$V" \
  -n myelin --create-namespace \
  --set serverName=example.org \
  --wait --timeout 10m
```

One replica, one volume holding the database, the signing key (generated on the first start) and
media, a Service, and the bridge operator. `serverName` is the part after the colon in user IDs
(`@alice:example.org`) and **cannot be changed later**.

Make the first administrator. The server logs a one-time setup link at every start until
someone uses it:

```sh
kubectl -n myelin logs statefulset/myelin-hs | grep setup_link
kubectl -n myelin port-forward svc/myelin-hs 8008:8008   # without publicBaseUrl the link is rooted at localhost:8008
```

Open the link, choose a username and password, and you are in the admin interface at `/admin/`.

### A server on the internet

Put the values in a file, which you keep: every upgrade starts from the release's values.

```yaml
# myelin-values.yaml
serverName: example.org
publicBaseUrl: https://matrix.example.org
ingress:
  enabled: true
  className: traefik            # your controller's IngressClass
  clientHost: matrix.example.org
  annotations:
    cert-manager.io/cluster-issuer: letsencrypt
  tlsSecret: matrix-example-org-tls
serviceMonitor:
  enabled: true                 # if you run kube-prometheus-stack
prometheusRule:
  enabled: true
```

```sh
V=0.3.0
helm install myelin oci://ghcr.io/brandon-dacrib/charts/hs --version "$V" \
  -n myelin --create-namespace -f myelin-values.yaml --wait --timeout 10m
```

With `publicBaseUrl` set, the server publishes both `/.well-known/matrix/client` and
`/.well-known/matrix/server`, and the Ingress routes `/.well-known/matrix`, `/_matrix`, `/admin`,
`/api/v1`, `/_synapse` and `/`. When `serverName` (`example.org`) is a different host from
`publicBaseUrl` (`matrix.example.org`), `https://example.org/.well-known/matrix/*` has to reach
the server too, or be served by whatever answers on `example.org`; otherwise other servers cannot
find yours. Check from outside:

```sh
curl https://example.org/.well-known/matrix/server     # {"m.server":"matrix.example.org:443"}
curl https://matrix.example.org/_matrix/federation/v1/version
```

### Cluster mode (several replicas, PostgreSQL)

Read the cluster section of `values.yaml` and `docs/scaling.md` first. Three things have to
exist before the install, because replicas share no volume:

```sh
# 1. One signing key for every replica. Back this Secret up: without it the server is a different server.
hs generate-signing-key -o signing.key        # the hs binary from the release, or a checkout's cargo run -p hs-cli --bin hs --
kubectl -n myelin create secret generic myelin-signing-key --from-file=signing.key=signing.key

# 2. Mesh authentication between replicas: a TLS Secret (tls.crt, tls.key, ca.crt) valid for
#    *.myelin-hs-headless.myelin.svc.cluster.local. A cert-manager Certificate writes exactly this;
#    deploy/helm/hs/values-two-replica-experiment.yaml has the three openssl commands without it.

# 3. A PostgreSQL database, and media on S3 or a ReadWriteMany claim.
```

```yaml
# myelin-values.yaml (cluster mode)
serverName: example.org
publicBaseUrl: https://matrix.example.org
mode: cluster
replicaCount: 2
storage:
  backend: postgres
  postgres:
    host: myelin-pg-rw.myelin.svc
    sslMode: require
    password: { secret: myelin-pg-app, secretKey: password }
# or, with CloudNativePG:  cloudNativePG: { enabled: true, clusterName: myelin-pg }
media:
  storage:
    backend: s3
    s3: { bucket: myelin-media, region: us-east-1, accessKeyId: AKIA..., existingSecret: myelin-s3 }
secrets:
  signingKey: { existingSecret: myelin-signing-key }
cluster:
  mesh:
    tls: { existingSecret: myelin-mesh-tls }
```

Install with the same `helm install ... -f myelin-values.yaml` as above. Known limit (status 12,
2026-10-09): scaling from one replica back up to two has broken `/sync` for some accounts; pick
the replica count at install and change it only in steps you can roll back.

### A second release in the same cluster

The `Bridge` CRD is cluster-wide and belongs to one release. Every other release sets
`--set crds.enabled=false`.

## Upgrade

### Before

1. Read the releases between yours and the target in `CHANGELOG.md`, and the
   [one-time steps](#one-time-steps) below.
2. Back up (`deploy/helm/hs/README.md`, "Backup and restore"). An upgrade keeps the volume and the
   key, but a rollback across a format change is not guaranteed to be clean.
3. Note where you are:

   ```sh
   helm -n myelin history myelin
   helm -n myelin list                       # CHART and APP VERSION columns
   ```

### The upgrade

```sh
V=0.3.1        # the release you are going to
helm -n myelin get values myelin -o yaml > myelin-values.yaml
helm upgrade myelin oci://ghcr.io/brandon-dacrib/charts/hs --version "$V" \
  -n myelin -f myelin-values.yaml \
  --set image.tag="$V" \
  --wait --timeout 10m
```

`--set image.tag="$V"` matters only if your values pin the tag ([Which chart](#which-chart)).
It is harmless otherwise, since it names the image the chart would pick anyway. If you keep your
values file in git (you should), use it instead of `helm get values`, and diff the two first.

A values file from the repository goes in after yours, read from the same tag. The standing
demo's upgrade, for example:

```sh
V=0.3.1
helm --kube-context admin@dacrib0 -n myelin get values myelin -o yaml > /tmp/myelin-values.yaml
helm --kube-context admin@dacrib0 upgrade myelin oci://ghcr.io/brandon-dacrib/charts/hs --version "$V" \
  -n myelin -f /tmp/myelin-values.yaml \
  -f "https://raw.githubusercontent.com/brandon-dacrib/myelin/v$V/deploy/demo/values-bridges.yaml" \
  --set image.tag="$V" \
  --wait --timeout 10m
```

**Do not use `--reuse-values`.** It reuses the old release's merged values in place of the new
chart's defaults, so a value a new chart added has no default: the 2026-09-30 upgrade of the
demo failed rendering `statefulset.yaml` that way when `bridges.enabled` was new. `helm get
values` returns only what you set, so `-f` with it gives you your values on top of the new
chart's defaults. (`--reset-then-reuse-values`, Helm 3.14+, does the same.)

A single-node server restarts once (seconds; clients reconnect). In cluster mode the StatefulSet
rolls one pod at a time and each hands its rooms to the others first, so nothing is lost.
Bridge pods roll by themselves when the new server renders their files differently.

### After

```sh
kubectl -n myelin rollout status statefulset/myelin-hs
helm -n myelin list                                                      # CHART hs-$V, APP VERSION $V
kubectl -n myelin get pods,bridges                                       # all Running
kubectl -n myelin get pod myelin-hs-0 -o jsonpath='{.spec.containers[0].image}'   # ghcr.io/brandon-dacrib/myelin:$V
kubectl -n myelin logs statefulset/myelin-hs | grep -E 'WARN|ERROR'
curl https://matrix.example.org/_matrix/federation/v1/version             # the new version
curl https://matrix.example.org/_matrix/key/v2/server                     # the same ed25519 key as before
```

The signing key must be the same before and after. If it is not, stop: the volume or the key
Secret did not come along, and other servers will reject what this one signs.

### One-time steps

Only for an install made from a chart older than these dates. Installs from a released chart
(0.2.0 or later) need none of them.

| Your install was made from | The upgrade fails with | Do this once |
|---|---|---|
| a chart before 2026-09-26 (pre-release `0.1.0-main.61` or earlier) | `StatefulSet.apps "myelin-hs" is invalid: spec: Forbidden: updates to statefulset spec for fields other than 'replicas', ...` | `kubectl -n myelin delete statefulset myelin-hs --cascade=orphan` (the pod and its volume stay), then the upgrade again. It adopts the pod and rolls it once. |
| a chart before 2026-10-09 (the CRD in `crds/`) | `CustomResourceDefinition "bridges.hs.matrix.org" ... exists and cannot be imported into the current release` | Add `--take-ownership` to the upgrade, and on Helm 4 also `--force-conflicts`. On Helm before 3.17, label and annotate the CRD first (`deploy/helm/hs/README.md`, "Upgrading"). |

## Roll back

```sh
helm -n myelin history myelin
helm -n myelin rollback myelin <revision> --wait --timeout 10m
```

This puts back the previous chart, values and image; the volume and key are untouched. What a
rollback to an older server costs: sync tokens the newer version handed out may be refused
(`400 invalid_since_token`) and clients do one initial sync again. A rollback across a change in
how data is stored is not proven safe; when the release notes say storage changed, restore the
backup instead.

If an upgrade was interrupted and Helm says `another operation (install/upgrade/rollback) is in
progress`, `helm history` shows the revision as `pending-upgrade`: roll back to the last
`deployed` revision, then upgrade again.

## Uninstall

```sh
helm -n myelin uninstall myelin
```

This leaves the data volume (`data-myelin-hs-0`, with the database and the signing key) and the
`Bridge` CRD with every bridge, so `helm install` with the same values brings the same server
back. To remove everything, which cannot be undone:

```sh
kubectl -n myelin delete pvc data-myelin-hs-0
kubectl delete crd bridges.hs.matrix.org      # deletes every Bridge in the cluster
```

## Other problems

| You see | It means | Do this |
|---|---|---|
| A bridge's page, or the log: "the Bridge CRD in the cluster is older than this server" | The CRD was not upgraded with the chart (`crds.enabled=false` here, or the one-time step above was skipped) | Upgrade the release that owns the CRD, or `kubectl apply --server-side --force-conflicts -f deploy/crds/bridge.yaml` from the matching tag |
| The pod still runs the old image after an upgrade | Your values pin `image.tag` (an install from a checkout), or the chart came from a checkout, whose image is `main` | Upgrade from the release with `--set image.tag="$V"` ([Which chart](#which-chart)) |
| Locked out of the admin interface | | `kubectl -n myelin exec myelin-hs-0 -- hs recover`, and open the link it prints |
| `401 Failed to find any key` from other servers | They cannot fetch this server's key | `/.well-known/matrix/server` at the `serverName` host (see [A server on the internet](#a-server-on-the-internet)) |
