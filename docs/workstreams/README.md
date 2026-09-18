# Workstreams: splitting the plan across experts

This directory turns `PLAN.md` into sixteen expert-owned tracks that can run in parallel. Each brief is self-contained: an agent or an engineer can pick it up with only `PLAN.md`, this file and the reference checkouts (`tools/fetch-refs.sh` clones them into `refs/`).

## How the split was derived

The plan's crates were sorted by what they provide and what they consume. Anything that only consumes a small, freezable interface can start on day one against a stub. Anything that consumes a protocol that does not exist yet (the room actor, the user session) waits for that protocol to be frozen. That gives three waves.

## The seams: interfaces that must be frozen for parallel work to be safe

| Week | Interface | Owner track | Consumers |
|---|---|---|---|
| 2 | `hs-kv` trait v0 (transactions, snapshots, range scans, multi-get, watches) plus the in-memory backend | 01 Storage | everyone |
| 2 | `hs-model` event and identifier types, room-version capability table | 02 State and model | everyone |
| 4 | `hs-tables` key encoding, declarative index API, migration API, interning API | 01 Storage | everyone that stores anything |
| 4 | Native config schema (`hs-config`) and the environment and file-secret override rules | 13 Config, compat and migration | 12 Platform, all |
| 6 | `hs-state` API: `state_at`, `diff`, `apply`, `resolve`, chain-cover queries | 02 State and model | 04 Room, 06 Federation |
| 6 | Ownership API (`owner_of(shard)`, `is_mine`, forwarding), mesh RPC envelope, lease manager hooks | 03 Cluster | 04, 05, 06, 11, 12 |
| 6 | Authentication middleware: request to `Requester` (user, device, appservice identity assertion, admin flag) | 07 Auth and identity | every HTTP handler |
| 8 | Room actor command protocol and its publish stream (room position updates with the changed-state summary) | 04 Room and events | 05, 06, 10, 11, 15 |
| 8 | User session protocol (subscribe, publish, cursors) | 05 Sync, jointly with 04 | 08, 10, 11 |
| 4 | Admin API design document and OpenAPI draft (mocked by 16) | 15 Admin API and modules | 16, 13 |
| 8 | Admin API v1 contract, module hook trait, admin model | 15 Admin API and modules | 16, 13, all tracks calling hooks |
| 12 | State representation decision (bake-off) and backend conformance sign-off: Phase 0 exit | 02, 01 | all |

Interface changes after freeze go through a short RFC in `docs/rfcs/` reviewed by the owning and consuming tracks. Nothing else needs coordination.

## Waves

**Wave 1, day one (10 tracks):** 01 Storage engine, 02 State and model, 03 Cluster, 07 Auth and identity, 09 Media, 12 Platform and Kubernetes, 13 Config, compat and migration, 14 Test and conformance, 15 Admin API and modules (design, OpenAPI draft, hook trait), 16 Management web interface (product design, design system, scaffold against the mock API).

**Wave 1.5, week 4 (2 tracks):** 04 Room and events (on the week-2 KV and model interfaces, stubbing state), 06 Federation (discovery, keys, transport server, signing, and the federation client can start; inbound event processing waits for the room protocol).

**Wave 2, week 8 (4 tracks plus the page work of 15 and 16):** 05 Sync, 08 E2EE, 10 Push, 11 Appservices and bridges; 15's endpoints and 16's pages land on the frozen API contract.

Wave 2 tracks are not idle before week 8: each brief lists the design and test-corpus work that starts on day one.

## Dependency graph

```
01 Storage ──────────┬──────────────┬────────────┬─────────────┐
02 State/model ──────┤              │            │             │
                     ▼              ▼            ▼             ▼
03 Cluster ───► 04 Room/events ─► 05 Sync    06 Federation   07 Auth
                     │              ▲            │             │
                     ├──► 10 Push   │            │             │
                     ├──► 11 Appservices/bridges ◄┘ (key proxies, AS federation) │
                     └──► 15 Admin API/modules ──► 16 Management web interface     │
08 E2EE ◄── 07 Auth, 06 Federation, 05 Sync cursors, 11 MSC3202 fields ◄──────────┘
09 Media ◄── 07 Auth, 06 Federation client (remote media), 11 direct media
12 Platform ◄── 03 Cluster (probes, leases), 11 (Bridge CRD → registry API)
13 Config/compat/migration ◄── everything's data model; feeds 12 and 15
14 Test/conformance ◄── everything; owns the harnesses everyone uses
```

## Staffing the tracks

| Team size | Mapping |
|---|---|
| 16 (one expert per track, or 16 agents) | As listed. |
| 6 engineers | A: 01 and 09. B: 02 and 06. C: 03 and 12. D: 04, 05 and 10. E: 07, 08 and 11. F: 13, 14, 15 and 16 (16 needs a designer at least half time). |
| 4 engineers | A: 01, 02. B: 03, 04, 05, 06. C: 07, 08, 09, 10, 11. D: 12, 13, 14, 15, 16. Expect the plan's month numbers to stretch by roughly half. |

Whoever holds 14 Test and conformance is also the integration lead: they own `hs-testkit`, the CI, the parity dashboard and the weekly interface review.

## Rules of engagement

1. Each track owns its crates and directories (listed in its brief) and may change anything inside them without asking. Cross-track changes are interface RFCs.
2. Every track writes its own tests against the shared `hs-testkit` and the shared in-memory KV backend; nothing merges without them.
3. No endpoint code merges before the Phase 0 exit (week 12). Wave 1.5 and 2 tracks build behind a `phase0` feature flag until then.
4. Reference code is read, not copied: Synapse is AGPL-3.0 and is a behavioral reference only; Ruma (MIT), Palpo, Conduit, Complement and the spec (Apache-2.0) may be quarried with attribution.
5. Every brief's "definition of done" is the acceptance test; the parity dashboard (owned by 14) is the only status report.
6. Decisions that change `PLAN.md` are recorded in `docs/decisions/` as dated entries.

## Brief format

Every brief has the same sections: expert profile, mission, owns, provides, consumes, day-one work, Phase 0 deliverables (weeks 1 to 12), Phase 1 and 2 deliverables, definition of done, references, open questions, risks.
