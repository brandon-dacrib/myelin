# 14. Test and conformance (integration lead)

Wave 1, starts day one. Owns the harnesses every other track uses and the only status report the project publishes.

**Expert profile.** Test infrastructure, Go (Complement), Perl (Sytest plugin), Python (driving Synapse's own implementations as oracles), fuzzing, performance engineering, CI.

**Mission.** Make "fully compatible, including tests for all functionality" mechanical: twelve test layers, a spec-coverage tool, a differential harness against Synapse, real-client and real-bridge tests, chaos and migration tests, performance gates, and the parity dashboard. See `PLAN.md` sections 12 and 13.

**Owns.** `hs-testkit` (fake clock, in-memory KV double with 01, fake federation peers, fake appservice, fake SMTP, fake push gateway, fake identity server, request helpers, a scenario DSL); `hs-spec-coverage` (OpenAPI trees to router assertions and response-schema validation for all five APIs); Complement integration (image contract, blacklist management, single-node and cluster modes, Synapse's in-repo Go suite); the Sytest homeserver plugin; the differential harness (recorder and replayer, normalizers, scripted workloads, the Python oracle harness that drives Synapse's state resolution, auth and push evaluation); fuzzing infrastructure; `hs-loadgen` on `matrix-rust-sdk`; performance CI and regression gates; the parity dashboard; the chaos infrastructure with 03 and 12; migration fixtures with 13; Element Web smoke tests with Playwright; corpus management (state corpus with 02, image corpus with 09, sync recordings with 05); the flake policy; the weekly interface review.

**Provides.** All of the above, to everyone.

**Consumes.** Everything.

**Day-one work.** Testkit skeleton and the KV double; the spec-coverage tool (can be complete within Phase 0); the Complement runner validated against Synapse's own image before ours exists; the Sytest plugin skeleton; the differential recorder capturing a baseline from Synapse 1.161 on a scripted workload; the loadgen skeleton; performance runners with 12; the dashboard skeleton.

**Phase 0 deliverables.** All skeletons functional; coverage tool complete; baseline recordings from Synapse; the oracle harness ready for 02's bake-off; loadgen v0; the Synapse 1.161 fixture database for 13.

**Phase 1 and 2 deliverables.** Every layer live in CI; the dashboard published per PR and nightly; continuous fuzzing; the chaos suite; migration fixtures per supported Synapse version; Element Web smoke; flake rate under 1 percent.

**Definition of done.** Layers L0 to L12 from `PLAN.md` section 12 exist and run; the dashboard is the project's status page; every other track's definition of done is executable through this track's harnesses.

**References.** `refs/complement/` (README image contract, `runtime/`, the `synapse_blacklist` tag), `refs/synapse/scripts-dev/complement.sh`, `refs/synapse/complement/`, `refs/synapse/docker/complement/` (structure only); `refs/sytest/lib/SyTest/Homeserver/` for the plugin shape; `refs/matrix-spec/data/api/`; `refs/palpo/tests/complement/` and its results file for a Rust server's Complement rig; `matrix-rust-sdk` integration tests.

**Open questions to settle first.** Running Synapse's Python oracle in CI (a container with Synapse installed, calling functions directly); normalization rules for differential diffs; performance runner hardware stability.

**Risks.** Flaky integration suites erode trust; the flake policy and quarantine lane exist from day one.
