# 0036: The chart applies the Bridge CRD on every upgrade, and a bridge keeps its own pickle key (2026-10-09)

Status: accepted (tracks 12 and 11). Changes how `deploy/helm/hs` ships the `Bridge` CRD
(RFC 0017) and how the bridge manager and the operator treat a failing apply and a bridge's
generated secrets.

## Context

Rolling `a6f02c48` to the demo on 2026-10-09 broke its WhatsApp bridge three ways at once:

1. The cluster's `Bridge` CRD was the one from the release's first install. The chart shipped
   it in `crds/`, which Helm installs on `helm install` only and never upgrades, so the
   `.spec.owner` field added on 2026-10-02 was unknown to the API server, and every apply of
   the instance's changed deployment was refused: `failed to create typed patch object (...):
   .spec.owner: field not declared in schema`.
2. The manager asked again every 3 s, logging each time that it was "applying it, which
   restarts the pod". Nothing was applied (the refusal came first), but nothing slowed it down.
3. The bridge's pod rolled with a files Secret rendered without `encryption.pickle_key` (the
   instance predates the server rendering one). The init container carried the bridge's own
   key only into a rendered file that had the line, so the key was dropped, mautrix generated
   another, and the bridge crash-looped with "the supplied account key is invalid".

## Decision

- **The CRD is a template, kept on uninstall.** `templates/crds.yaml` renders the generated
  `files/crds/bridge.yaml` (read as data with `.Files.Get`, so its text is never templated) with
  the release's labels and `helm.sh/resource-policy: keep`. `helm upgrade` applies it like any
  object; `helm uninstall` leaves it and every `Bridge`. Values `crds.enabled` (one release per
  cluster owns it; others set false) and `crds.keep`. This is cert-manager's choice
  (`crds.enabled`, `crds.keep`); the alternatives were the operator or the server applying the
  CRD at startup (a ClusterRole on CRDs for a namespaced component, and two writers) and a
  pre-upgrade hook Job (the same ClusterRole, and a Job to debug). `crds/` is gone: with both,
  the first install creates the CRD outside the release and the template then fails on it.
- **Moving an existing release over takes one adoption.** A CRD installed from `crds/` has no
  Helm ownership metadata, so the first upgrade with this chart stops ("cannot be imported into
  the current release"). `helm upgrade --take-ownership` (Helm 3.17+; with Helm 4 also
  `--force-conflicts`, since it applies server-side and the old CRD's fields belong to whoever
  created it) adopts it, or label and annotate it first (`deploy/helm/hs/README.md`).
  `deploy/helm/hs/ci/crd-upgrade-smoke.sh` shows the whole path on a real API server.
- **A server ahead of the cluster's CRD degrades instead of failing.** The bridge client reads
  the undeclared fields from the API server's refusal. When all of them are informational
  (`INFORMATIONAL_FIELDS`: `.spec.owner`, which only labels objects), it applies the `Bridge`
  without them and carries on; otherwise it fails with "the Bridge CRD in the cluster is older
  than this server (...): apply deploy/crds/bridge.yaml". Either way it logs that sentence once
  per change and shows it on every deployed instance's page (`Runtime::warning`).
- **A failing step backs off.** Each instance's failing streak doubles its wait from 3 s up to
  5 minutes (`step_backoff`), logged with the count and the wait; asking for the instance again
  or changing its offering ends the wait. The "restarts the pod" line is logged after an apply
  went through, not before it is tried. The client writes an existing `Bridge`'s files Secret
  before the `Bridge` (whose files hash restarts the pod), so a failure between the two never
  restarts a bridge with old files.
- **A bridge's own secrets always win.** The init container carries `pickle_key`, `signing_key`
  and `server_key` from the bridge's file into every new copy: replacing, inserting under the
  section, or appending the section. The server never mints a pickle key for an instance
  registered before it kept one; only a new instance gets one, with its tokens.
- **A bridge that cannot read its store says so.** The bridge container's termination message
  falls back to its log; the operator carries the last line into the `Bridge`'s status; the
  manager explains "the supplied account key is invalid" on the instance with the recovery in
  `docs/bridges/mautrix.md`.

## Consequences

- An operator upgrading a release from before this decision runs one `helm upgrade` with
  `--take-ownership` (and `--force-conflicts` on Helm 4); later upgrades need nothing.
- Every bridge pod rolls once when the operator from this change starts (its init script and
  container spec changed); with the carry fixed, that roll keeps each bridge's keys.
- A second release of the chart in the same cluster must set `crds.enabled=false`.
