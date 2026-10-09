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
