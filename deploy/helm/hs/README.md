# hs: the Myelin homeserver chart

```sh
helm install myelin oci://ghcr.io/brandon-dacrib/charts/hs --set serverName=example.org
```

One value installs a running server; `values.yaml` documents every other one. What has been
verified against a real cluster is recorded in `docs/status/12-platform-and-kubernetes.md`.

## The Bridge CRD

With `bridges.enabled` (the default) the chart renders the `Bridge` CustomResourceDefinition
(`bridges.hs.matrix.org`) from `templates/crds.yaml`, so every `helm upgrade` applies the CRD
the new server and operator expect. It is annotated `helm.sh/resource-policy: keep`:
`helm uninstall` leaves it, and every `Bridge` in the cluster, in place. Remove it by hand when
you mean to (`kubectl delete crd bridges.hs.matrix.org` deletes every bridge with it).

- `crds.enabled: false` for every release but one in a cluster: a cluster-scoped object can
  belong to one release only.
- `crds.keep: false` drops the annotation.
- The file is generated (`cargo run -p hs-operator --bin gen-crds` writes
  `files/crds/bridge.yaml` beside `deploy/crds/bridge.yaml`); do not edit it.

## Federation and discovery

With `publicBaseUrl` set to an `https://` address, the server publishes both discovery
documents by default: `/.well-known/matrix/client` names the base URL for clients, and
`/.well-known/matrix/server` names its host and port (`matrix.example.org:443` for
`https://matrix.example.org`) for other servers, which is how they fetch this server's signing
key (decision 0040; the demo spent a day answered `401 Failed to find any key` by every remote
server before this was the default). Route `/.well-known/matrix` to the server at the
`serverName` host, as the chart's Ingress and HTTPRoute do; no port 8448 is needed. When
federation is reached at a different host or port than clients use, set
`server.well_known_server` to that `host:port` under `extraConfig` (or on the Configuration
page; it is a hot setting); the empty string publishes no server document, for a deployment
whose reverse proxy serves its own. `ci/install-smoke.sh` checks the derived document on
every image CD builds.

## Upgrading

### From a chart before 2026-10-09: adopt the CRD once

Earlier charts shipped the CRD in `crds/`, which Helm installs once and never upgrades, so a
cluster kept the CRD of its first install. A newer server then logs, and shows on each bridge's
page, "the Bridge CRD in the cluster is older than this server ...: apply
deploy/crds/bridge.yaml". The first upgrade to this chart has to take that CRD over, because it
was not created as part of the release; without that Helm stops with "CustomResourceDefinition
"bridges.hs.matrix.org" ... exists and cannot be imported into the current release".

With Helm 3.17 or later:

```sh
helm upgrade myelin <chart> -n <namespace> --reuse-values --take-ownership
# Helm 4 applies server-side; the old CRD's fields belong to whoever created it:
helm upgrade myelin <chart> -n <namespace> --reuse-values --take-ownership --force-conflicts
```

With an older Helm, label and annotate the CRD first, then upgrade as usual:

```sh
kubectl label crd bridges.hs.matrix.org app.kubernetes.io/managed-by=Helm
kubectl annotate crd bridges.hs.matrix.org \
  meta.helm.sh/release-name=myelin meta.helm.sh/release-namespace=<namespace>
```

To fix the CRD without upgrading the release: `kubectl apply --server-side --force-conflicts -f
deploy/crds/bridge.yaml`.

`ci/crd-upgrade-smoke.sh` runs this whole path against a real API server (a kind cluster):
the old CRD refusing `.spec.owner`, the plain install stopping, `--take-ownership` adopting and
updating it, and `helm uninstall` keeping it. CD runs it on every push to `main`, on the kind cluster of the
image job, between the install smoke and the operator smoke (`--set K=V` points its release at
the image already on the node).

## Day two

Each procedure below has an executable form under `ci/`, run by CD on a kind cluster against
every image before it is tagged (`.github/workflows/cd.yml`), so the steps are known to work
on real pods, not only written down. `docs/status/12-platform-and-kubernetes.md` records what
each run measured.

### Alerts

`prometheusRule.enabled: true` renders a `PrometheusRule` (`templates/prometheusrule.yaml`)
beside the ServiceMonitor, scoped to this release: a replica down or not Ready, restarts, a
5xx rate over 5%, slow requests (long-polls excluded); in cluster mode a replica missing from
the registry, shards with no owner, a lease older than its TTL, fenced writes; slow PostgreSQL
commits; a data or media volume under 10% free; dropped outbound federation events; a bridge
that stopped accepting transactions or had them dead-lettered; memory over 90% of its limit.
Every annotation names the command or admin page to look at. `ci/alerts-test.sh` renders the
rules and runs `promtool check rules` and `promtool test rules` (`ci/alerts/tests.yaml`): each
alert fires on the series it is written for and a healthy release stays silent.

### Scaling, rolling, and what a disruption costs

In cluster mode a request that reaches a replica which does not own the room or user it is
for is forwarded to the owner, and a forward that lands while a shard is changing hands waits
for the new owner instead of failing (`docs/decisions/0017`). So these lose nothing, under
traffic:

- `helm upgrade ... --set replicaCount=N` up or down: a new replica takes its share of the
  shards within seconds of its first heartbeat; a leaving one hands its shards on before its
  listeners close (`cluster.terminationGracePeriodSeconds` must outlast that handoff, which
  takes up to twenty seconds).
- `helm upgrade` to a new image, `helm rollback`, `kubectl rollout restart`: the StatefulSet
  rolls one pod at a time, each handing its shards on and taking them back.
- `kubectl delete pod`: the same handoff.

What does cost requests: a replica that dies without a handoff (`--grace-period=0`, an
OOMKill, a node loss). Its shards have no owner until its lease (`cluster.leaseTtl`, 10 s)
lapses; forwards to them are retried until their own deadline (10 s) and then fail with
`503 M_HS_NOT_SHARD_OWNER`. Everything that arrives after the lease lapses is served by the
survivors. `ci/cluster-smoke.sh` runs all of the above with a traffic pod against the Service
and judges every request by these rules.

### Backup and restore

What to copy depends on the mode. `ci/backup-restore-smoke.sh` runs both procedures and
proves the restore is the same server: the same signing key, the same accounts, the messages
up to the backup and nothing after it.

**Single node** (`mode: singleNode`): everything is on the data claim, `data-<release>-0`
(`db/` the database, `keys/` the signing key, `media/` the media). The embedded engine has no
online snapshot, so stop the server for the copy (seconds), copy the volume, start it again:

```sh
kubectl -n <ns> scale statefulset/<release> --replicas=0
kubectl -n <ns> apply -f - <<EOF   # a helper pod that mounts the claim
apiVersion: v1
kind: Pod
metadata: {name: backup-helper}
spec:
  containers: [{name: h, image: public.ecr.aws/docker/library/python:3.12-alpine, command: [sleep, "3600"], volumeMounts: [{name: data, mountPath: /data}]}]
  volumes: [{name: data, persistentVolumeClaim: {claimName: data-<release>-0}}]

## Day two

Each procedure below has an executable form under `ci/`, run by CD on a kind cluster against
every image before it is tagged (`.github/workflows/cd.yml`), so the steps are known to work
on real pods, not only written down. `docs/status/12-platform-and-kubernetes.md` records what
each run measured.

### Alerts

`prometheusRule.enabled: true` renders a `PrometheusRule` (`templates/prometheusrule.yaml`)
beside the ServiceMonitor, scoped to this release: a replica down or not Ready, restarts, a
5xx rate over 5%, slow requests (long-polls excluded); in cluster mode a replica missing from
the registry, shards with no owner, a lease older than its TTL, fenced writes; slow PostgreSQL
commits; a data or media volume under 10% free; dropped outbound federation events; a bridge
that stopped accepting transactions or had them dead-lettered; memory over 90% of its limit.
Every annotation names the command or admin page to look at. `ci/alerts-test.sh` renders the
rules and runs `promtool check rules` and `promtool test rules` (`ci/alerts/tests.yaml`): each
alert fires on the series it is written for and a healthy release stays silent.

### Scaling, rolling, and what a disruption costs

In cluster mode a request that reaches a replica which does not own the room or user it is
for is forwarded to the owner, and a forward that lands while a shard is changing hands waits
for the new owner instead of failing (`docs/decisions/0017`). So these lose nothing, under
traffic:

- `helm upgrade ... --set replicaCount=N` up or down: a new replica takes its share of the
  shards within seconds of its first heartbeat; a leaving one hands its shards on before its
  listeners close (`cluster.terminationGracePeriodSeconds` must outlast that handoff, which
  takes up to twenty seconds).
- `helm upgrade` to a new image, `helm rollback`, `kubectl rollout restart`: the StatefulSet
  rolls one pod at a time, each handing its shards on and taking them back.
- `kubectl delete pod`: the same handoff.

What does cost requests: a replica that dies without a handoff (`--grace-period=0`, an
OOMKill, a node loss). Its shards have no owner until its lease (`cluster.leaseTtl`, 10 s)
lapses; forwards to them are retried until their own deadline (10 s) and then fail with
`503 M_HS_NOT_SHARD_OWNER`. Everything that arrives after the lease lapses is served by the
survivors. `ci/cluster-smoke.sh` runs all of the above with a traffic pod against the Service
and judges every request by these rules.

### Backup and restore

What to copy depends on the mode. `ci/backup-restore-smoke.sh` runs both procedures and
proves the restore is the same server: the same signing key, the same accounts, the messages
up to the backup and nothing after it.

**Single node** (`mode: singleNode`): everything is on the data claim, `data-<release>-0`
(`db/` the database, `keys/` the signing key, `media/` the media). The embedded engine has no
online snapshot, so stop the server for the copy (seconds), copy the volume, start it again:

```sh
kubectl -n <ns> scale statefulset/<release> --replicas=0
# A helper pod that mounts the claim (any image with sh and tar):
kubectl -n <ns> run backup-helper --image=public.ecr.aws/docker/library/python:3.12-alpine \
  --overrides='{"spec":{"containers":[{"name":"h","image":"public.ecr.aws/docker/library/python:3.12-alpine","command":["sleep","3600"],"volumeMounts":[{"name":"data","mountPath":"/data"}]}],"volumes":[{"name":"data","persistentVolumeClaim":{"claimName":"data-<release>-0"}}]}}'
kubectl -n <ns> wait --for=condition=Ready pod/backup-helper
kubectl -n <ns> exec backup-helper -- tar cf - -C /data . > hs-data.tar
kubectl -n <ns> delete pod backup-helper
kubectl -n <ns> scale statefulset/<release> --replicas=1
```

A storage-layer snapshot of the claim (a CSI `VolumeSnapshot`) taken while the server runs is
crash-consistent, as a power loss would be; the engine recovers from its journal on the next
start. To restore: install the chart as before (the pod comes up fresh, with a new key), scale
to zero, empty the claim and `tar xf - -C /data < hs-data.tar` through the same helper pod,
scale to one. The server is back as it was at the backup, key included.

**Cluster** (`mode: cluster`): three things, in this order.

1. The database: `pg_dump -Fc` (online, consistent; with CloudNativePG, its own backups and
   point-in-time recovery do this for you).
2. The media volume (`media.storage.local.existingClaim`): `tar` through a helper pod as above,
   or the bucket's own versioning and replication with `media.storage.backend: s3`.
3. The signing key Secret (`secrets.signingKey.existingSecret`):
   `kubectl get secret <name> -o yaml > hs-signing-key.yaml`. Without it the restored server
   is a different server to the rest of Matrix: every event it ever signed fails verification
   elsewhere.

Restore in the opposite order: apply the Secret, `pg_restore -d <db> --no-owner`, put the media
back, `helm install` with the same values. The replicas find the schema and the shard layout in
the database and start where the backup left off.

### Rotating the mesh certificates

The replicas load `cluster.mesh.tls.existingSecret` at start and do not watch it, so a new
certificate takes effect through a roll (`kubectl rollout restart statefulset/<release>`), which
loses nothing (above). A renewed leaf under the same CA is one Secret update and one roll; with
cert-manager, that is a `Certificate` renewal plus the roll. Rotating the CA takes three rolls,
so that at every moment every pod trusts the CA that signed every other pod's certificate:

1. `ca.crt` holds both the old and the new CA; `tls.crt` is still the old CA's. Roll.
2. `tls.crt` is issued by the new CA; `ca.crt` still holds both. Roll.
3. `ca.crt` holds only the new CA. Roll.

`ci/cluster-smoke.sh --rotate-certs` runs these three rolls under traffic.
