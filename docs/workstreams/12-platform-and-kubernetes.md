# 12. Platform and Kubernetes

Wave 1, starts day one. Owns the CI every track runs on and the deployment surface operators see.

**Expert profile.** Kubernetes operators (`kube-rs`), Helm, CI and multi-arch builds, observability, SRE practice, supply-chain security.

**Mission.** Images, CI on both architectures, the Helm chart, the operator with its custom resources, probes and rollout behavior, observability, packaging and the small-ARM benchmark rig. See `PLAN.md` section 7 and 8.4.

**Owns.** Container images (distroless, non-root, read-only root, static `musl`, `linux/amd64` and `linux/arm64`, signed with cosign, SBOMs), CI runners including native arm64, the Helm chart, `hs-operator` with the `Homeserver`, `AppService`, `Bridge`, `PushGateway` and `IdentityService` resources, probe and lifecycle wiring with 03, HPA custom metrics, PodDisruptionBudgets and anti-affinity, CloudNativePG integration, object-store provisioning hooks, cert-manager mutual TLS for the mesh, `hs-telemetry` (OpenTelemetry, Prometheus conventions, structured logs, Sentry), Grafana dashboards and alert rules, Debian and RPM packages and a Nix flake, Element Server Suite integration testing, the release train, the arm64 benchmark rig.

**Provides.** CI for everyone; the probe contract; metric and span naming conventions; the chart values schema.

**Consumes.** 03 (leases, probes, handoff), 11 (registry API for the `Bridge` resource), 13 (config schema).

**Day-one work.** Repository CI (build, test, clippy, fmt, cross builds, arm64 runners); the image pipeline; `hs-telemetry` conventions; a `kind`-based end-to-end skeleton; chart and operator skeletons with CRD schemas; dashboard skeletons; a study of the ESS Helm values so the chart is a plausible swap for its Synapse component.

**Phase 0 deliverables.** All skeletons working; the arm64 benchmark rig producing numbers; the chaos infrastructure with 03; the docs site scaffold.

**Phase 1 and 2 deliverables.** Full chart and operator; the `Bridge` flow end to end with `mautrix-irc`; HPA metrics; dashboards; packaging; release automation; an ESS-style deployment with our chart; hardening (seccomp, capabilities), signed images.

**Definition of done.** Chart and operator end-to-end tests on `kind` in CI; an ESS-style stack runs against us; arm64 budgets from `PLAN.md` section 7.4 measured in CI; dashboards render from live metrics; images signed and SBOMs published.

**References.** `element-hq/ess-helm` values and templates; the community mautrix charts (`mautrix-go-base`, `wrenix/mautrix-bridge`); CloudNativePG docs; `kube-rs` controller docs; the FoundationDB operator as an example of a mature operator design.

**Open questions to settle first.** Operator scope (how much of bridge lifecycle it owns versus the chart); Gateway API versus Ingress; chart naming compatibility with ESS.

**Risks.** Operator scope creep; arm64 runner availability (self-hosted if needed).
