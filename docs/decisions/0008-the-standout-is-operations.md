# 0008. The standout is operations: Kubernetes-native, one-value install, scale by replica count, administration in a browser

Status: accepted, 2026-09-26. Author: integration lead, at the user's direction. Applies to every track.

## Decision

Myelin's distinguishing feature, the thing it is for, is that it is the easiest Matrix
homeserver to install, scale and administer, and that it is native to Kubernetes and the cloud
from birth. Specification coverage, federation and bridges are necessary and are measured, but
they are the price of entry. The standout is operations.

Concretely, the properties this project holds itself to, in priority order:

1. **Install is one value.** `docker run` with a server name; `helm install` with a server
   name; `hs serve --data-dir --server-name` with nothing else. No configuration file, no
   pre-made secrets, no generated YAML to edit. The first administrator comes from a one-time
   link in the log, not from a shared secret and a `curl`.
2. **Scale is a replica count.** `replicas: N` behind one Service, no worker types, no
   path-routing map at the ingress. Rooms and users are owned by lease, forwarded over a mesh,
   handed off before a pod stops. A rolling update drops no request. The store is the only
   stateful dependency.
3. **Administration is a page in a browser, on a public API.** Everything an operator does
   day to day is in the management interface, and everything the interface does is an
   operation in the admin API with OpenAPI, scopes and an audit entry. Configuration lives in
   the database and is edited there; the deployment pins what it owns through the environment.
4. **It is a good Kubernetes citizen without being only that.** Distroless, non-root,
   read-only root, multi-arch; probes that mean something; graceful shutdown inside the
   default grace period; a chart that fits the Element Server Suite stack; an operator with
   `Homeserver`, `AppService` and `Bridge` resources. And the same binary on a small ARM host
   with an embedded store, because "cloud-native" that needs a cloud is a tax.

## Why

`docs/landscape.md` has the field as of today. The Rust homeservers people run, continuwuity
and tuwunel, are excellent single-process servers and say in their own Kubernetes documentation
that they do not scale horizontally; tuwunel is funded and moving quickly, so competing with it
on single-node throughput or spec breadth would be building a second implementation of a solved
problem (decision 0007). Synapse scales, but through hand-assigned worker types and a routing
table, which is the part of running Synapse that operators get wrong. Nobody offers the
combination above. That is the gap, and it is the gap `PLAN.md` requirements R6, R7, R8 and R9
were written for; this decision names it as the product rather than one requirement among ten.

## Consequences

- **Priority order changes.** `docs/next-steps.md` puts the operations work first: the chart
  installed for real with the published image, the cluster path carrying real client and
  bridge traffic on two replicas, the operator creating workloads, a rolling update measured.
  Federation and conformance keep their measurements and their place in the table, second.
- **Every feature answers "how does the operator turn it on".** A capability that exists only
  as a config key nobody can reach from the interface, or only as a CLI flag, is not done. The
  bridge wizard is the model: the feature is the page, the runbook and the green tick.
- **The gates grow.** A `helm install` from an empty namespace to a working server, with the
  published image, becomes a CD check alongside the `docker run` boot; a two-replica test that
  drives real client traffic through both replicas joins the cluster suite; a rolling update
  under load that drops no request is a performance gate (`PLAN.md` section 13).
- **What we do not chase.** Headline single-node throughput against RocksDB-tuned servers;
  niche MSCs ahead of operational completeness; packaging for every distribution (tuwunel has
  that covered, and a static binary plus an image is enough for the people this is for).
- **Language.** The README leads with this. "A modern Matrix homeserver in Rust" is true of
  four projects; "the one that installs with one value and scales with one number" is true of
  this one, and it is stated only to the extent it has been verified by running it.
