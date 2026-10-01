# `Homeserver` (hs.matrix.org/v1alpha1)

A `Homeserver` is one Myelin server, single-node or clustered, run by the operator
(`hs operator --homeservers`). It becomes the same objects `deploy/helm/hs` renders for the
equivalent values, and the operator scales it and rolls it with every departing replica drained
through the admin API first.

Schema: `deploy/crds/homeserver.yaml` (generated from `crates/hs-operator/src/crds/homeserver.rs`
by `cargo run -p hs-operator --bin gen-crds`). Examples: `deploy/operator/examples/`. Install:
`deploy/operator/` (below).

Status as of 2026-10-01: tested against an in-memory cluster and against `helm template`, and
**run on a kind cluster in single-node mode** by `deploy/operator/ci/kind-smoke.sh --homeserver`
(CD runs it before tagging an image): the objects below, owned by the resource, `Ready` once the
pod is, an image change rolled through the partition (1 → 0 → 1, pod replaced, `Ready` again)
and deletion removing all but the data claim. **Cluster mode has not run on a cluster yet**:
replicas, the PodDisruptionBudget and draining through the admin API are tested only against
the in-memory cluster; the steps are in `docs/status/12-platform-and-kubernetes.md`.

## What it becomes

| Object | Name | Notes |
| --- | --- | --- |
| ServiceAccount | `<name>` | |
| ConfigMap | `<name>-config` | `homeserver.yaml`, the file layer of the config (RFC 0016) |
| Service | `<name>` | ClusterIP: client 8008, federation 8448, metrics 9090 |
| Service | `<name>-headless` | `publishNotReadyAddresses`; client, and mesh in cluster mode |
| StatefulSet | `<name>` | probes, security contexts, anti-affinity and grace period as the chart |
| PodDisruptionBudget | `<name>` | cluster mode with more than one replica; `minAvailable: 1` |

Every object is owned by the `Homeserver` (deleting it deletes them; the embedded data claim is
a StatefulSet claim and outlives it, as with the chart). Labels: `app.kubernetes.io/name: hs`,
`app.kubernetes.io/instance: <name>`, `app.kubernetes.io/managed-by: myelin-operator`.

**One design with the chart.** `hs_operator::homeserver::chart_values` maps a `Homeserver` to
chart values, and a test (`crates/hs-operator/src/homeserver/helm_equivalence.rs`) renders the
chart with them and compares every object field by field. The deliberate differences are:
the chart's Helm labels (`helm.sh/chart`, `app.kubernetes.io/version`, `managed-by: Helm`); how
`checksum/config` is computed; owner references; the chart's `helm.sh/resource-policy` on the
data claim; and the StatefulSet's `updateStrategy`, which the operator sets to `RollingUpdate`
with a `partition` it controls (see "Draining"). Use the chart or the operator for a given
server, not both.

## Spec

| Field | Default | Chart value | Meaning |
| --- | --- | --- | --- |
| `serverName` | required | `serverName` | The Matrix server name. Do not change it after rooms exist. |
| `publicBaseUrl` | | `publicBaseUrl` | The external base URL. |
| `replicas` | 1 | `replicaCount` | Replicas; always 1 with embedded storage. |
| `image.repository`, `.tag`, `.digest`, `.pullPolicy` | | `image.*` | A digest wins over a tag; pull policy `IfNotPresent` for a digest, else `Always`. |
| `storage.backend` | required | `storage.backend`, `mode` | `embedded` (the chart's `singleNode`), `postgres` or `slatedb` (both `cluster`). |
| `storage.embedded.size`, `.storageClassName` | | `storage.embedded.*` | The data claim. |
| `storage.postgres.cloudNativePgCluster` | | `cloudNativePG.clusterName` | Connection from `<cluster>-app`. |
| `storage.postgres.host`, `.port` (5432), `.database` (hs), `.user` (hs), `.passwordSecretRef` | | `storage.postgres.*` | An external database. |
| `storage.postgres.sslMode` (prefer), `.sslRootCert` | | `storage.postgres.sslMode`, `.sslRootCert` | libpq's `sslmode`; the root certificate is rendered for `verify-ca` and `verify-full` only. |
| `storage.slatedb.bucketUrl`, `.shardCount` (256) | | `storage.objectStorage.*` | |
| `signingKeySecretRef` | required | `secrets.signingKey` | Mounted whole as a directory. |
| `registrationSharedSecretRef`, `sessionSecretRef` | | `secrets.*` | |
| `cluster.meshTlsSecret` | | `cluster.mesh.tls.existingSecret` | `kubernetes.io/tls` with `ca.crt`, for `*.<name>-headless.<ns>.svc.<domain>`. |
| `cluster.meshSharedSecretRef` | | `cluster.mesh.sharedSecret` | Instead of TLS, trusted networks only. One of the two is required in cluster mode. |
| `cluster.meshPort` | 8449 | `cluster.mesh.port` | |
| `cluster.roomShards`, `.userShards` | 256 | `cluster.*` | |
| `cluster.heartbeatInterval`, `.leaseTtl` | 2s, 10s | `cluster.*` | |
| `cluster.clusterDomain` | cluster.local | `cluster.clusterDomain` | |
| `cluster.terminationGracePeriodSeconds` | 40 | `cluster.*` | Must outlast the server's 20 s shutdown handoff. |
| `cluster.antiAffinity` | `Soft` | `affinity.podAntiAffinity` | `Soft`, `Hard` or `None`, by node. |
| `media.backend` | `local` | `media.storage.backend` | `local` or `s3`. |
| `media.localClaim` | | `media.storage.local.existingClaim` | Required for local media in cluster mode (ReadWriteMany). |
| `media.s3.bucket`, `.region`, `.endpoint`, `.accessKeyId`, `.secretAccessKeyRef` | | `media.storage.s3.*` | |
| `resources` | the chart's | `resources` | The server container's resources. |
| `extraConfig` | | `extraConfig` | Native config merged into `homeserver.yaml`, winning over generated keys. |
| `drain.timeoutSeconds` | 600 | | How long a replica may take to hand off its shards. |
| `drain.onTimeout` | `Proceed` | | `Proceed`: let the pod go anyway; `Hold`: keep waiting. |
| `adminApi.tokenSecretRef` | | | A bearer token with `admin:write`. Without `adminApi`, pods go undrained. |
| `adminApi.url` | `http://<name>.<ns>.svc:8008` | | The server's base URL. |

`storage.embedded.size` and `.storageClassName` are fixed once the StatefulSet exists (its claim
templates are immutable, as with the chart): a change makes every reconcile fail with the API
server's `Forbidden` until it is reverted, counted in `hs_operator_reconcile_errors_total`.

An invalid spec (the same checks as the chart's `hs.validate`) applies nothing: the phase is
`Degraded`, `SpecValid` is `False` with the reason in its message, and a Warning event says so.

The resource has the `scale` subresource, so `kubectl scale homeserver/<name> --replicas=N` works.

## Draining

In cluster mode with `adminApi` set, no pod is removed or replaced while its replica owns
shards (decision 0012):

1. The StatefulSet runs `RollingUpdate` with a `partition` the operator owns. In steady state
   it equals the replica count, so a new template replaces nothing by itself.
2. For each pod that must go, highest ordinal first, the operator calls
   `POST /api/v1/cluster/replicas/{id}/drain` (the replica id is the pod's mesh address,
   `<pod>.<name>-headless.<ns>.svc.<domain>:<meshPort>`), records it in `status.drain`, and
   reads the replica back every five seconds until it owns no shards, with the drain task's
   status alongside.
3. Then the pod goes: a scale-down lowers `replicas` by one; a rolling update lowers
   `partition` to that ordinal and the StatefulSet controller replaces the pod.
4. The replica is undrained (`POST .../undrain`): after a scale-down once the pod is gone, so a
   later scale-up does not bring that ordinal back drained; after a rolling update once the
   replacement (same name, same id, so it inherits the drain) runs the new template and is
   ready. The next pod is drained only after that, so the operator never has two replicas
   drained at once.

A drain past `drain.timeoutSeconds` raises a `DrainTimedOut` Warning event and then either lets
the pod go (`Proceed`; its own `SIGTERM` handoff releases the rest within twenty seconds, and
the others take anything left when its lease expires) or holds it (`Hold`; phase `Degraded`)
until the drain completes or the change is reverted. A drain the spec no longer needs (replicas
raised back mid scale-down, a template reverted mid rollout) is aborted: the replica is undrained
and keeps its pod (`DrainAborted`). A drain the server refuses (`409`: no other active replica)
is reported (`DrainRefused`) and retried. A replica someone undrains by hand while the operator
still needs it gone is drained again (`DrainReissued`).

The operator adds the finalizer `hs.matrix.org/undrain` to a clustered `Homeserver` with
`adminApi`; deleting the resource first undrains every replica the operator drained (a drain
request lives in the database and would otherwise outlive the resource).

Scaling to zero, one replica, and embedded storage have nothing to drain through; pods go as
the StatefulSet controller removes them. The token needs `admin:write`; a server
administrator's access token is accepted as that (register an operator account with
`hs register -a`, log in with `/_matrix/client/v3/login`, and keep the token for this purpose
only), stored as `kubectl create secret generic hs-admin --from-literal=token=<token>`.

## Status

| Field | Meaning |
| --- | --- |
| `phase` | `Pending`, `Ready` or `Degraded` |
| `observedGeneration` | The spec generation this status describes |
| `replicas`, `readyReplicas`, `updatedReplicas` | From the StatefulSet |
| `drain` | The drain in flight: `pod`, `replicaId`, `reason` (`ScaleDown`, `RollingUpdate`), `startedAt`, `taskId`, `taskStatus`, `shardsRemaining`, `timedOut` |
| `pendingUndrains` | Replicas to undrain, and `when` (`AfterRemoval`, `WhenReady`) |
| `conditions` | Below |

| Condition | True means | Reasons |
| --- | --- | --- |
| `Ready` | Every desired replica runs the current template and is ready | `AllReplicasReady`, `ReplicasNotReady` |
| `Progressing` | Creating, scaling or rolling | `Creating`, `ScalingUp`, `ScalingDown`, `RollingUpdate`, `Converging`; `Stable` when false |
| `Draining` | A drain is in flight | `DrainInProgress`, `DrainTimedOut`; `NoDrain` when false |
| `DrainAvailable` | The operator can drain through the admin API | `AdminApiConfigured`; `SingleNode`, `NoAdminApi`, `TokenSecretMissing` when false |
| `SpecValid` | The spec can be built | `Valid`; `InvalidSpec` when false |

`kubectl wait --for=condition=Ready homeserver/<name>` waits for a rollout.

## Events

Normal: `Created`, `ScalingUp`, `Draining`, `Drained`, `Undrained`, `DrainAborted`.
Warning: `InvalidSpec`, `DrainRefused`, `DrainTimedOut`, `DrainReissued`,
`ScaledDownWithoutDrain`, `UndrainSkipped`, `DrainAbandoned`.

## Metrics

`hs operator --metrics-address 0.0.0.0:9090` serves `/metrics` (and `/healthz`):

| Metric | Type | Labels |
| --- | --- | --- |
| `hs_operator_reconcile_duration_seconds` | histogram | `kind` (`Homeserver`, `Bridge`), `result` (`ok`, `error`) |
| `hs_operator_reconcile_errors_total` | counter | `kind`, `reason` (`kube`, `admin_api`, `build`, `missing`) |
| `hs_operator_drains_in_flight` | gauge | `kind` |
| `hs_operator_drains_total` | counter | `outcome` (`started`, `completed`, `timed_out`, `aborted`, `refused`, `undrained`) |

## Install

```sh
kubectl apply -f deploy/crds/homeserver.yaml -f deploy/crds/bridge.yaml
kubectl apply -k deploy/operator            # namespace `myelin`; edit kustomization.yaml
kubectl -n myelin apply -f deploy/operator/examples/homeserver-single-node.yaml
kubectl -n myelin wait --for=condition=Ready homeserver/hs --timeout=5m
```

The operator watches its own namespace only, and also runs the `Bridge` controller: do not
install it beside the chart's bridge operator in the same namespace.
