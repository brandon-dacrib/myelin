# Plan: a modern Matrix homeserver in Rust

Status: proposal v2, written 2026-09-17. Supersedes the schema-compatible draft from earlier the same day.
Project name: **Myelin** (decided 2026-09-19). Crate names below still use the `hs-` prefix; renaming the crates is mechanical follow-up work, deliberately not done while parallel tracks are mid-flight.

Companion files:

- `docs/synapse-inventory.md`: the generated behavioral checklist for Synapse 1.161.0 (every route, `unstable_features` flag, room version, config option, replication stream, module callback registry). We do not copy Synapse's design, but clients, bridges and admin tools are written against Synapse's behavior, so this is the parity list.
- `tools/synapse_inventory.py`: regenerates that checklist from any Synapse checkout.

---

## Table of contents

0. Summary and what changed from v1
1. What you asked for, restated as requirements
2. Research findings
3. Where Synapse burns resources, and what this design does about each
4. Design decisions
5. Architecture
6. Storage and state: the redesign
7. Kubernetes-native operation, and the small-ARM-host mode
8. Bridges: making them easy to run
9. Compatibility with Synapse and the migration path
10. Full-spec policy
11. Workstreams
12. Test strategy
13. Performance targets and benchmarking
14. Roadmap, milestones, effort
15. Risks
16. Decisions needed from you
Appendix A. Crate and dependency choices
Appendix B. What the mautrix bridges call on a homeserver
Appendix C. Sources

---

## 0. Summary and what changed from v1

The first draft treated "drop-in" literally: run on Synapse's PostgreSQL schema, collapse its workers, port its code. Your notes are right about the consequence: that yields a faster Synapse, not a different one. State-group storage and state resolution are algorithmic and structural problems; a language change fixes JSON, signatures, push rules and worker sprawl, but not the state-store blowup or the DB round trips baked into the data model. With the added requirements (Kubernetes-native HA and scale-out, a single small ARM host, the full specification, bridges that are easy to manage, and no contribution to an existing product), the right shape is different:

- **A greenfield Rust homeserver** designed around today's protocol (room version 12 with state resolution v2.1, simplified sliding sync accepted into the spec in July 2026, OAuth 2.0 client auth in the spec since v1.15, authenticated media, MSC4242 state DAGs as the Foundation's direction for state) and today's infrastructure (Kubernetes, object storage, multi-arch containers).
- **One binary, three deployment sizes.** The same code runs as a single static binary with an embedded store on a Raspberry Pi, as a small HA cluster on Kubernetes with PostgreSQL, and as a sharded cluster with a distributed store for very large deployments. The storage layer is an ordered, transactional key-value abstraction with pluggable backends, which is what makes all three real.
- **State storage is redesigned before code is written**, with a Phase 0 bake-off between three representations on real room histories (section 6).
- **Rooms are sharded across identical replicas with leases**, so state resolution and event authorization run against memory on the room's owner instead of against the database. This is the Kubernetes-native answer to "fetch auth chain" and "state resolution" costs: not a database feature, but a compute-placement decision.
- **Synapse compatibility moves to the edges**: the same client, federation, appservice and admin API behavior (Synapse's 236 routes and 37 `unstable_features` are the parity list), Synapse's `homeserver.yaml` accepted through a translator, Synapse's admin API and metric names available for existing tooling, and an online importer for Synapse databases and media stores. Not the schema, not the workers, not Python modules.
- **Bridges are a first-class managed object**: a database-backed appservice registry with hot registration, per-bridge health and backlog, the encryption MSCs on by default, a Kubernetes operator with a `Bridge` custom resource that deploys mautrix bridges with generated registrations and double puppeting wired up, and a management web interface whose marquee page is bridges.

Honest scale: 100 to 150 engineer-months to a production-grade server that a typical Synapse operator can migrate to, with a usable single-node server with bridges at roughly month 9 to 12 for a team of four. Section 14 has the phases.

---

## 1. What you asked for, restated as requirements

| # | Requirement | How this plan reads it |
|---|---|---|
| R1 | Re-implementation of Synapse in Rust with a focus on performance | A new server, not a port. Performance targets in section 13, measured against Synapse on the same hardware. |
| R2 | Fully compatible, including tests for all functionality | Behavioral compatibility with what clients, bridges, federation peers and admin tools observe. Compatibility is defined by test suites: Complement, Sytest, a spec-coverage tool driven by the spec's OpenAPI files, differential tests against Synapse, bridge tests, client tests. Section 12. |
| R3 | Drop-in binary replacement | Same external behavior, Synapse config accepted, Synapse admin API and metrics served, one command to import a Synapse database and media store. Not the same schema. Section 9. |
| R4 | All bridges, especially mautrix, fully functional | Every homeserver feature the mautrix frameworks use (inventoried in Appendix B) plus first-class management. Section 8. |
| R5 | Full specification implemented | All five spec APIs at v1.19, all twelve room versions, plus the MSCs that Element X, Element Web, Element Call and the bridges require. Section 10. |
| R6 | Bridges easy to manage | Registry, operator, console, sane defaults. Section 8. |
| R7 | Cloud-native, runs on Kubernetes from birth | Identical stateless-by-design replicas, leases, probes, graceful shutdown, object storage, operator and Helm chart, multi-arch images. Section 7. |
| R8 | Kubernetes-native answers to state resolution and state-group storage | Room ownership by lease with in-memory hot state; a redesigned, backend-independent state store; object-storage-backed store option. Sections 5 and 6. |
| R9 | HA and scalable, but runs on a small ARM host | Embedded store backend, static `aarch64` binary, memory budget; the cluster backends are optional. Section 7.4. |
| R10 | Modern rebuild, no contribution to existing products | Synapse, Conduit, Palpo, Dendrite and the Conduit forks are references and, where their license allows, quarries. No fork. Section 2.3. |

---

## 2. Research findings

Everything here was checked on 2026-09-17 from fresh checkouts or the live sites; sources are in Appendix C.

### 2.1 Synapse 1.161.0 (the behavioral reference)

- Active repository is `element-hq/synapse`, AGPL-3.0 with a commercial license. The `matrix-org/synapse` repository you linked is archived and read-only (its last releases were Apache-2.0).
- 249k lines of Python plus 14.9k lines of Rust (push rules, canonical JSON, event metadata and redaction, room versions, identifiers, server ACLs, rendezvous). 4,177 tests. Schema version 94 with about 227 tables created over its history and 147 registered background updates. 19 replication streams over Redis, 36 replication HTTP endpoints, 117 worker-routable path patterns.
- 236 route patterns: 109 client, 31 federation, 77 admin, 19 media, keys and other. 229 documented config options plus 51 `experimental_features` flags. 37 `unstable_features` advertised on `/versions`. Room versions 1 to 12 plus six unstable versions (`org.matrix.hydra.11`, `org.matrix.msc1767.10`, `org.matrix.msc3389.10`, `org.matrix.msc3757.10`, `org.matrix.msc3757.11`, `org.matrix.msc4242.12`). Default room version 12.
- Recent work is dominated by MSC4140 delayed events, MSC4354 sticky events, MSC4242 state DAGs, MSC4186 simplified sliding sync, MSC3814 dehydrated devices, MSC4388 rendezvous, MSC4284 policy servers, MSC3266 room summaries, MSC4512 appservice federation proxy.
- Synapse still runs both Complement and Sytest in CI; Complement's own README still tracks Sytest-parity conversion as incomplete, so both suites matter.
- The `state_groups_state` table is the best known operational pathology: it is routinely the majority of a Synapse database, needs an external compressor, and the compressor skips backfilled and heavily-resolved regions.

### 2.2 The specification and the protocol direction

- Spec v1.19 (2026-07-08): mutual rooms endpoint (MSC2666), image packs (MSC2545), `m.key_backup` account data (MSC4287), encrypted history sharing (MSC4268), server-defined room directory ordering (MSC4423). OAuth 2.0 client authentication has been in the spec since v1.15. Authenticated media since v1.11. Async uploads and appservice ping since v1.7. Extended profile fields since v1.16.
- Room version 12 ("Hydra"): MSC4289 creators with infinite power and multiple creators, MSC4291 room IDs as the hash of the create event, MSC4297 state resolution v2.1 (conflicted-state subgraph replay to end state resets). All twelve versions are stable; v12 is the recommended default.
- MSC4186 simplified sliding sync: the core MSC was accepted by the Spec Core Team on 2026-07-01; extensions are still in review. Element X has required it since January 2025 and no longer supports the older sliding sync.
- MSC4242 state DAGs: events carry `prev_state_events`, making state causality explicit so that receiving servers reconstruct state structurally instead of resolving algorithmically. Synapse has experimental support merged (storage, receiving, serving, `/make_join` and `/send_join` changes); Ruma has draft definitions; Complement tests are in progress. It requires a new room version and is explicitly experimental, with open questions on anti-entropy, deep-partition reconciliation, redactions and high-churn rooms. It is the Foundation's stated direction for state handling and this design accommodates it from the start.
- Ruma 0.15 (MIT) covers all events and endpoints of Matrix 1.18, one minor version behind the spec, and includes `ruma-state-res` with the v2.1 rules (Palpo's code references them).

### 2.3 Prior art in Rust (references and quarries, not bases)

| Server | Stack | What is worth learning from it |
|---|---|---|
| Conduit (upstream, `gitlab.com/famedly/conduit`, Apache-2.0, 42.8k lines) | Ruma, RocksDB or SQLite behind a small ordered-KV `Tree` abstraction, axum | Proof that a full homeserver can be written against an ordered-KV interface with hand-maintained indexes; short-ID interning for state keys and events; layered state diffs; appservice registration by admin command instead of config files; still Beta and supports room versions 3 to 12 only. |
| conduwuit (archived), continuwuity (community, v26.8.1 in August 2026), tuwunel (company-backed) | Conduit lineage, RocksDB tuned hard | RocksDB tuning, federation hardening, admin commands. Embedded single-node only, which conflicts with R7 and R9 together. |
| Palpo (`palpo-im/palpo`, Apache-2.0, 174k lines, last commit 2026-09-09) | Ruma-derived core, Salvo, PostgreSQL 16 via diesel-async | The closest existing design to a Postgres-backed Rust server: 73 tables, interned state fields, `room_state_frames` deduplicated by hash with parent deltas (`appended` and `disposed` byte sets), an auth-chain index, sliding sync, MSC2409, MSC3202, MSC4190 and MSC4203 in the appservice code, a "cluster support" migration that moves in-memory state into the database, a web admin, and Synapse admin-API tables. Self-reported Complement 672 pass, 0 fail, 14 skip. Self-described as not production-proven. |
| Dendrite (Go, matrix-org, maintenance mode) | Function-split micro-services (roomserver, syncapi, federationapi, ...) with Kafka, or a monolith | The lesson: splitting by function was operationally painful and was abandoned in favor of the monolith. Split by data (rooms, users), not by function. |

### 2.4 Storage engines and Kubernetes infrastructure

| Component | State in 2026 | Fit |
|---|---|---|
| Fjall 3.0 (pure Rust LSM, MIT, released January 2026) | Transactions with serializable snapshot isolation, MVCC snapshots, keyspaces, per-level compression, key-value separation, write batches; author reports parity with RocksDB across workloads and states that feature work winds down in 2026 (stable); MSRV 1.91 | Default embedded backend for single-node and small ARM hosts. |
| RocksDB via `rust-rocksdb` | Battle-tested in the Conduit lineage | Fallback embedded backend if the bake-off shows gaps. C++ dependency complicates static `aarch64` builds. |
| redb (pure Rust B-tree, stable file format) | Mature, LMDB-like | Candidate for small read-heavy keyspaces; not the primary engine. |
| PostgreSQL with CloudNativePG (1.29 in 2026, multi-arch images, automated failover, streaming replication) | The boring HA store on Kubernetes | Default cluster backend. As far as is publicly documented, matrix.org runs Synapse against one PostgreSQL primary with read replicas, so the write capacity of a single primary is demonstrated at the largest deployment in existence. |
| SlateDB (Rust, Apache-2.0, LSM on object storage, local cache, single-writer with a formally verified manifest-fencing protocol, transactions, snapshots) | Young but purpose-built for "diskless" stateful services | The Kubernetes-native option for shard-per-writer storage on S3 with lease-fenced failover (section 6.5). Experimental tier. |
| FoundationDB 7.3 with the FDB Kubernetes operator (v2.24, March 2026, arm64 samples) and `foundationdb-rs` | Transactional, ordered, distributed KV with watches | Third backend for very large deployments; its API is almost exactly the abstraction we define. Later phase. |
| openraft (Databend's Raft, pre-1.0 API) | Usable, not stable | Not used in v1; embedded replication is not worth building when PostgreSQL and object storage already give HA. |
| Element Server Suite Community Helm charts (AGPL) | Deploys Synapse, MAS, Element Web, Element Admin, Element Call's RTC backend, Hookshot, HAProxy, optional PostgreSQL | The reference Kubernetes deployment of Synapse; our Helm chart must be a plausible swap for the Synapse component in that stack. |
| Community mautrix Helm charts (`mautrix-go-base`, `wrenix/mautrix-bridge`) | Per-bridge charts, registration files templated, shared double-puppet registration | No operator or CRD exists in the ecosystem; that is the gap section 8 fills. |
| Matrix Authentication Service | AGPL-3.0 since 0.12 under Element; only supports Synapse through the delegation feature and the `/_synapse/mas/*` internal API | We implement the spec's OAuth 2.0 API natively (section 4, D5) and optionally delegate to an external MAS for operators who already run one. |

### 2.5 The bridges

All maintained mautrix bridges are Go programs on `mautrix-go`'s `bridgev2` framework in 2026 (WhatsApp, Telegram, Signal, Discord, Slack, Google Messages, Meta, Instagram, Twitter, LinkedIn, Google Voice, IRC, Zulip, iMessage). `mautrix-python` still exists for legacy bridges. Neither framework calls the Synapse admin API from bridge code. What they need from a homeserver is inventoried in Appendix B. IRC and Zulip bridge networks can be self-hosted in CI, which enables real end-to-end bridge tests without third-party accounts.

---

## 3. Where Synapse burns resources, and what this design does about each

Your diagram splits Synapse's per-event cost into six boxes. Here is what each one is, and what fixes it.

| Box | Nature | What actually fixes it | In this design |
|---|---|---|---|
| Parse JSON (object churn, GC) | Language | Native parsing, borrowed decoding, cached serialized bytes | `serde_json` with borrowed parsing; events stored as canonical bytes and served without re-encoding; `Arc<Event>` shared across sync, push and federation. |
| Verify signatures (ed25519 per event) | Language, plus parallelism | Native crypto and batch verification on all cores | `ed25519-dalek` batch verification, key cache, parallel verification of `send_join` payloads. |
| Fetch auth chain (DB round trips) | Schema and placement | An index that answers "auth chain difference" in O(chains) rather than a graph walk, and hot data in memory instead of behind a socket | A chain-cover index like Synapse's (it is the best known structure and Palpo adopted it too), maintained per room by the room's owner and cached in memory; batched multi-get on the store; no per-hop round trips. |
| State resolution (state group blowup) | Algorithm and structure | Interned state, structurally shared snapshots, cheap state diffs, and a room owner that already has the state in memory; MSC4242 to make causality explicit | Section 6: interned IDs, content-addressed persistent state maps (or one of the two alternatives if the bake-off says so), room-owner actors, `ruma-state-res` v2.1 with cross-checks, MSC4242 tables from day one. |
| Evaluate push rules (per recipient) | Language | Native evaluator, precompiled rules per user, evaluation once per event with per-user deltas | Native evaluator; per-user compiled rule sets cached and invalidated by the push-rules stream; evaluation batched per event on the room owner. |
| Fan out to syncs (cache RAM per worker) | Architecture | One process per replica with shared caches, and per-user change trackers instead of per-room scans | No worker types; identical replicas each with one cache set; per-user "rooms changed since position" tracking fed by the room owner. |

Language solves three boxes. The other three are solved by data-structure and placement decisions, which is why section 6 comes before any endpoint work in the roadmap.

---

## 4. Design decisions

**D0. Use existing projects wherever it makes sense; build as little as is reasonable.** This governs every decision below. Where a maintained project already does a job well we use it and contribute upstream rather than reimplementing. It has already removed two components from the plan, the push gateway and the identity service, in favour of Sygnal and of configuring whatever identity server an operator uses; and it reduced content scanning to a single ICAP client in front of c-icap. Full audit and the standing obligation on every track in `docs/decisions/0007-build-less-reuse-more.md`.

**D1. Storage is an ordered, transactional key-value abstraction with pluggable backends.** Snapshot reads, serializable transactions with retry, range scans, multi-get, atomic counters and watches. Backends: embedded (Fjall, with RocksDB as fallback) for single-node; PostgreSQL for HA clusters; SlateDB on object storage as the experimental diskless cluster option; FoundationDB later. Above the KV layer sits a typed table layer with a tuple-style key encoding, declarative secondary indexes maintained inside the same transaction, and schema migrations, so hand-maintained indexes (the Conduit lineage's chronic bug source) are declared once rather than written per call site. Full-text search is a per-shard `tantivy` index, backend-independent. Consequence: SQL is not the query language, and admin reporting queries are served from purpose-built indexes and a read-only export to Parquet rather than ad-hoc SQL.

**D2. Identical replicas; data-sharded ownership by lease.** Every replica runs every function. Each room and each user's sync session has exactly one owner replica at a time, chosen by rendezvous hashing over live replicas registered with heartbeats in the store (a Kubernetes Lease is used for the cluster membership list when available; the store-based mechanism works everywhere). Requests that arrive at a non-owner are forwarded over an internal mesh. The owner keeps a room's hot state in memory: current state map, forward extremities, chain-cover cache, recent timeline, member list. One replica means one owner for everything and no forwarding. Failover is lease expiry plus a cold load from the store.

**D3. State storage is designed first and chosen by measurement.** Section 6 defines the three candidate representations, the benchmark corpus and the exit criteria for the Phase 0 bake-off. No endpoint code lands before that decision.

**D4. State resolution uses `ruma-state-res` (v2 and v2.1) with an independent implementation as a test oracle; v1 is implemented in-house; MSC4242 state DAGs are supported experimentally from the first federation milestone.** Room versions 1 to 12 are all supported, including the auth rules, redaction algorithms and event formats of the old versions, because federation with existing rooms requires them.

**D5. The homeserver is its own OAuth 2.0 authorization server.** The spec's next-generation auth API (MSC3861 family: auth metadata discovery, dynamic client registration, authorization code with PKCE, device authorization grant, token refresh and revocation, account management URL) is implemented natively, alongside the legacy `/login` and user-interactive auth flows that older clients and bridges use (`m.login.password`, `m.login.token`, `m.login.application_service`, SSO redirects). Upstream identity providers (OIDC, SAML, LDAP) are login methods behind the native issuer. Operators who already run Matrix Authentication Service can instead configure delegation (introspection plus the internal provisioning endpoints MAS expects). This removes a mandatory second service from most deployments.

**D6. Media lives on object storage first.** The `object_store` crate abstracts S3-compatible, GCS, Azure and local filesystem. Thumbnails are generated on demand and cached. Authenticated media endpoints are primary; the legacy unauthenticated endpoints are served behind a config flag with the same freeze semantics Synapse has. Synapse's on-disk media layout is understood by the importer.

**D6b. Media content adaptation is a swappable ICAP service, not a built-in.** ICAP is a content adaptation protocol, not an antivirus protocol: a service may return a verdict or return modified content, so one good client buys antivirus, data-loss prevention, metadata stripping, classification and transcoding rather than a single-purpose scanner. Operators choose their scanner and change it without changing the server. We ship one ICAP client and no antivirus integration of our own. c-icap already hosts scanning services and drives ClamAV, so it owns talking to engines and we own talking to it. Commercial engines speak ICAP natively and cloud APIs reach it through gateways such as ICAPeg; an HTTP provider covers cloud services with no ICAP fronting, such as CrowdStrike Falcon. Effort goes into one good client, including preview negotiation and using the protocol's own ISTag as the verdict cache's engine version. The reference c-icap and ClamAV deployment ships as chart and compose configuration. Scanning applies to local uploads, asynchronous upload completion, remote media fetched over federation, and bridge uploads, with verdicts cached by content hash and engine version. Modes are block, defer, quarantine or off, and enabling scanning without choosing fail-open or fail-closed is a configuration error, because that is a policy decision. Encrypted media is reported unscannable rather than clean, and can never be rewritten, since the server holds ciphertext and no key; the documentation says so, because an operator who believes a scanner covers encrypted rooms has bought a false assurance. Content replacement is off by default, applies at upload only, and is audited with both content hashes. Design in `docs/rfcs/0008-content-scanning.md`.

**D7. Appservices are first-class managed objects** (section 8): registry in the store, hot registration through the admin API, health and backlog per appservice, replay and pause, the encryption-related MSCs on by default, an operator with a `Bridge` custom resource, and a console.

**D8. Synapse compatibility lives at the edges** (section 9): behavior, config translation, admin API, metric names, importer. Not schema, workers or Python modules.

**D9. License: Apache-2.0.** The design is derived from the specification and from MIT and Apache-2.0 code (Ruma, Fjall, Palpo, Conduit, Complement). Synapse (AGPL-3.0) and MAS (AGPL-3.0) are behavioral references only; no code is copied from them. Their tests are read for intent and re-expressed. If you would rather have AGPL-3.0 to be able to port Synapse's Rust push evaluator and canonical JSON verbatim, that is a one-line change to this decision with a modest saving; the recommendation is Apache-2.0 because it keeps the project usable by the widest set of operators and vendors.

**D10. Full-spec policy** (section 10): every endpoint in the five OpenAPI trees of the spec at the pinned version is registered and tested, enforced by a coverage tool; Identity Service and Push Gateway are shipped as optional components of the same binary.

**D11. Kubernetes-native operation with a single-node mode** (section 7): multi-arch static images, probes, graceful drain, lease-based ownership, object storage, Helm chart and operator, and `hs serve --single-node` on a small ARM host with no external dependencies.

**D12. A public admin API and a management web interface are products, not afterthoughts.** The admin API under `/api/v1` is designed from operator tasks: consistent resources, OpenAPI 3.1 generated from the handlers and shipped with the binary, cursor pagination, RFC 9457 errors, idempotent mutations, OAuth scopes, an audit log, a server-sent-events stream for live interfaces, and generated TypeScript and Rust clients. The management interface is built only on that API, is served by the homeserver, and is held to a product bar: information architecture designed around real operator flows, a design system, accessibility conformance, end-to-end tests for every flow, and bridges as its marquee page. Synapse's admin API is served alongside as a compatibility surface, not as the model.

---

## 5. Architecture

### 5.1 Process model

One binary, `hs`. `hs serve` runs a replica: HTTP listeners (client, federation, media, metrics, internal mesh), the ownership manager, the room and user actors it owns, the federation sender shards it owns, the appservice sender shards it owns, background jobs it has leased. `hs serve --single-node` is the same process owning everything with the embedded store and no mesh. Subcommands cover the CLI surface (section 9.1), the importer, the identity service and push gateway components, and admin operations.

### 5.2 Request flow

1. A client request authenticates (native OAuth tokens, legacy access tokens, appservice tokens with identity assertion) and rate-limits at the edge of any replica.
2. Reads that only need durable data (profile, media, key queries, most admin endpoints) execute locally against the store through the replica's caches.
3. Writes to a room, and reads that need the room's hot state (send, state, join, `/messages` with recent events, `/context`, `/members` of a live room) are routed to the room owner. Locally if this replica owns it, else forwarded over the mesh with the authenticated user context.
4. `/sync` and simplified sliding sync attach to the user's session owner, which holds the per-user change tracker and the connection state, and is woken by room owners through the mesh.
5. Federation inbound `/send` transactions are split by room and dispatched to room owners; EDUs to user owners or global handlers.
6. Federation outbound: per-destination queues sharded by destination hash across replicas; each shard persists its queue state so failover resumes.

### 5.3 Room actor

The room actor is the unit of consistency. It serializes all writes for one room: event creation, authorization against current state, state resolution when a new event forks the DAG, persistence of the event and its state, chain-cover index updates, push-action computation, and publication of the room's new position to interested user owners, the federation sender and the appservice sender. It holds in memory: current state map (interned), forward and backward extremities, the recent timeline window, the member index, the chain-cover cache, and the persistent state maps for recent events. Cold rooms are evicted after inactivity and reloaded on demand. Everything it holds is derivable from the store, so a crash loses nothing but cache.

### 5.4 User session actor

Owns one user's sync state: the set of rooms the user is in, the last position published per room, invite and knock state, account data, to-device queue cursor, device-list changes, the sliding sync connection state (lists, subscriptions, extensions), presence. Serves `/sync` and sliding sync from memory plus targeted store reads. Wakes long-polls when a room owner publishes.

### 5.5 Crate layout

| Crate | Responsibility |
|---|---|
| `hs-kv` | The ordered transactional KV trait, watches, the Fjall, PostgreSQL and SlateDB backends, the test double. |
| `hs-tables` | Typed keyspaces, tuple key encoding, declarative secondary indexes, migrations, interning (short IDs for rooms, users, event IDs, state keys, servers). |
| `hs-model` | Domain types over Ruma: events, room versions, canonical JSON, hashing, signing, redaction per version. |
| `hs-state` | Event auth for every room version, state resolution (v1 in-house, v2 and v2.1 via `ruma-state-res`, oracle implementation for tests), the state store representation chosen in section 6, chain-cover index, MSC4242 DAG tables. |
| `hs-room` | The room actor and its store access: timeline, extremities, membership, relations and threads, receipts per room, room stats, retention and purge, upgrades, spaces and summaries. |
| `hs-user` | The user session actor: sync v2, simplified sliding sync, account data, tags, filters, presence, to-device, device lists, thread subscriptions. |
| `hs-cluster` | Replica registry, leases, rendezvous hashing, the internal mesh (HTTP/2 with mTLS, `tonic` or plain hyper), forwarding, failover. |
| `hs-federation` | Server discovery, key server and notary, request signing and verification, transport server (31 routes at parity), federation client, inbound transaction dispatch, backfill and missing events, joins including faster joins, sender shards with batching and backoff, EDUs, server ACLs, policy servers. |
| `hs-auth` | Native OAuth 2.0 authorization server and OIDC issuer, legacy login and UIA, registration and tokens, 3PIDs, password policy, upstream IdPs (OIDC, SAML, LDAP), MAS delegation mode, account validity, consent, suspension, locking, deactivation and erasure. |
| `hs-e2e` | Device keys, one-time and fallback keys, cross-signing, key backups, device-list tracking and outbound pokes, dehydrated devices, appservice key proxies. |
| `hs-media` | Object-store repository, uploads and async uploads, authenticated and legacy downloads, thumbnails, URL previews and oEmbed, quarantine, retention, remote cache, and pluggable content scanning (`docs/rfcs/0008-content-scanning.md`). |
| `hs-push` | Push rules engine and per-user compiled rules, notification counts, HTTP pushers, email pushers and templates. |
| `hs-appservice` | Registry, namespaces, transaction scheduler with MSC2409, MSC3202 and MSC4203, identity assertion and device masquerading, MSC4190, ping, third-party lookups, MSC3983 and MSC3984, health and backlog reporting. |
| `hs-search` | `tantivy` per shard for room events and the user directory. |
| `hs-admin` | The public admin API (`/api/v1`, OpenAPI, scopes, audit log, event stream, generated clients), scheduled tasks, server notices, reports; serves the management interface's built assets. The Synapse-compatible admin surface lives in `hs-compat`. |
| `web/` | The management web interface: TypeScript application, design system, generated API client, Storybook, Playwright tests; built assets embedded in the binary. |
| `hs-compat` | Synapse `homeserver.yaml` translator, Synapse database and media importer, Synapse metric-name exporter, `register_new_matrix_user` protocol, worker-config collapse. |
| `hs-modules` | Native module points, HTTP-callback modules, WebAssembly modules (component model) for in-process extensions without Python. |
| `hs-http` | Router, request parsing with Synapse's leniency, error mapping, CORS, rate limiters, listeners (TCP, TLS, unix), `X-Matrix` auth. |
| `hs-telemetry` | Prometheus (`hs_*` names plus the Synapse-name exporter), OpenTelemetry tracing and logs, structured JSON logs, Sentry. |
| `hs-operator` | Kubernetes operator (`kube-rs`) with the `Homeserver`, `AppService` and `Bridge` CRDs. |
| `hs-cli` | The `hs` binary and Synapse-named shims. |
| `hs-testkit`, `hs-loadgen`, `hs-bridge-conformance`, `hs-spec-coverage` | Test infrastructure (section 12). |

---

## 6. Storage and state: the redesign

This section is the answer to "treat state storage as a redesign, not a translation". It is written to be decided by measurement in Phase 0, not by taste.

### 6.1 Interning

Every identifier that appears in hot paths gets a compact integer the moment it is first seen, and the integer is what indexes and in-memory structures carry:

| Identifier | Short ID | Notes |
|---|---|---|
| Room ID | `room_sn` (u32) | Assigned on creation or first sight. |
| User ID | `user_sn` (u32) | Local and remote. |
| Server name | `server_sn` (u32) | For membership-by-server and ACLs. |
| Event ID | `event_sn` (u64) | Assigned at persist time, store-wide monotonic; the primary key of the event record. |
| `(type, state_key)` pair | `state_key_id` (u32) | The Conduit and Palpo trick; membership state keys dominate. |
| Event type | `type_id` (u32) | For filtering without string compares. |

A state map in memory is `Map<state_key_id, event_sn>`. A state map on disk is a sequence of `(u32, u64)` pairs. Reverse lookups are one indexed read each and cached.

### 6.2 Events

- `event/{event_sn}`: fixed header (`room_sn`, `type_id`, `sender_sn`, optional `state_key_id`, `origin_server_ts`, `depth`, `room_pos`, flags for rejected, soft-failed, redacted, outlier, partial-state) followed by the canonical JSON bytes. Zstd with a trained dictionary is evaluated in the bake-off; the gain on event JSON is typically 3x to 5x, which matters on a Pi and on object storage.
- `event_id/{event_id} -> event_sn`, `room/{room_sn}/timeline/{room_pos} -> event_sn` (the room-local, monotonic timeline position, negative for backfilled events), `room/{room_sn}/state_events/{room_pos}`, relations, redactions, and edges (`prev_events`, `auth_events`) stored in interned form, forward and backward extremities.
- The event's resolved state is referenced from the header by a state root (section 6.3); most events share their predecessor's root.

### 6.3 State representation: three candidates and a bake-off

| Candidate | Structure | Strengths | Known failure modes |
|---|---|---|---|
| A. Snapshot plus delta chains (Synapse's model) | A state group per state change, storing a delta from its parent; full snapshots every N hops | Simple; append-only | Group explosion on forks and backfill; needs an external compactor; every lookup chases the chain. This is what your notes rightly call the state-group blowup. |
| B. Deduplicated frames with layered diffs (Conduit's and Palpo's model) | State as a sorted set of `(state_key_id, event_sn)` pairs, delta-varint compressed; frames deduplicated by content hash; each frame stores `parent`, `appended`, `disposed` | Compact; dedup is automatic; set algebra on integers is fast | Diff between two arbitrary states still walks layers; layer depth must be bounded by periodic re-basing; a frame cannot share storage with a sibling on another fork. |
| C. Content-addressed persistent map | A 32-way hash array mapped trie (or a persistent B-tree) whose nodes are stored by content hash; a state is its root hash; a change writes O(log32 n) new nodes and shares the rest | Structural sharing across forks and across time; diff between any two states skips identical subtrees (this is the operation state resolution, `/sync` state deltas and `state_after` need); dedup is intrinsic; the on-disk and in-memory shapes are the same persistent map, so the room owner's cache is literally the store's nodes; cold nodes tier to object storage naturally | More small records; needs garbage collection (per-room reference counting or periodic mark-and-sweep); node fan-in makes hot nodes (the root of a large room) cache-critical |

Prior, before the bake-off ran: C, because its cost model matches the operations the protocol actually performs. **The bake-off refuted this.** On the published weights (`docs/decisions/0005-state-bakeoff-methodology.md`), B scored 0.819, A 0.766 and C 0.513, so tracks 04 and 06 build against B. Two caveats belong in the same breath as the result: B's 5.3-point margin over A clears the decisiveness bar by a hair and is driven by two correlated rows that reward the same property, so a reader weighting hot-path latency over footprint would reasonably pick A; and C's defeat is implementation-specific rather than structural, since its one undiluted structural advantage did show up, a deep-distance diff costing 0.041 microseconds against about 26 for both rivals. Revisit with real-room corpora, a wider-fan-out C, and a PostgreSQL or SlateDB backend. Full numbers in `docs/decisions/0006-state-bakeoff-results.md`.

**Bake-off corpus** (collected once, versioned, reused by CI forever):

1. A very large public room's full state history obtained by joining it from a throwaway server (on the order of 100k members and years of membership churn).
2. A high-churn support room of the kind the MSC4242 discussion cites (hundreds of thousands of membership changes).
3. A synthetic moderation-policy room with 100k policy state events (Draupnir-style lists).
4. Synthetic fork and backfill scenarios: N concurrent forward extremities with periodic merges; deep backfill after a long partition; a faster join with partial state that later un-partials.
5. A thousand small rooms with ordinary chatter, for the common case.

**Measurements** per candidate and per backend: bytes on disk per state event; state-at-event lookup p50 and p99 cold and warm; diff between two states 1, 100 and 10,000 state events apart; time to compute the conflicted state for a fork and run resolution; room-owner resident memory; write amplification; compaction or GC cost; behavior under MSC4242 where the DAG is explicit.

**Exit criterion:** choose C if it is within 20 percent of the best candidate on size and lookup and wins on diff; otherwise choose the candidate that wins on the weighted score, with the weights published before the run. Either way, the `hs-state` API (`state_at(event_sn)`, `diff(a, b)`, `apply(root, changes)`, `resolve(forks)`) is fixed now, so the decision is contained.

### 6.4 Auth chains

Auth chain difference is the expensive primitive under state resolution v2. The chain-cover index (each event assigned a `(chain_id, sequence_number)` such that an event's auth chain is a set of chain prefixes, with a small table of links between chains) answers it with a handful of range reads instead of a graph walk, and it is the best known structure; Synapse introduced it and Palpo adopted it. We build it per room in interned form, maintain it in the room owner's persistence transaction, keep the hot part in the owner's memory, and rebuild it in the background for rooms imported from Synapse.

### 6.5 Mapping onto backends

| Backend | Keyspaces | Transactions | Watches | Sharding | Use |
|---|---|---|---|---|---|
| Fjall (embedded) | One keyspace per table | Serializable snapshot isolation, single process | In-process | All shards local | Single node, small ARM host, tests |
| PostgreSQL | One table per keyspace: `k bytea primary key, v bytea`, plus a few typed tables where SQL earns its keep (appservice registry, admin listings, audit log) | `SERIALIZABLE` with retry, pipelined multi-get via `= ANY($1)` and range scans on the primary key | `LISTEN` / `NOTIFY` | Logical shards; one database; CloudNativePG for HA | Default cluster backend |
| SlateDB on object storage | One SlateDB per shard (rooms and users are hashed into a fixed number of virtual shards, for example 256, plus one global shard) | Per-shard transactions; each shard has one writer, the shard's owner, fenced by SlateDB's manifest protocol | In-process on the owner, mesh to others | Physical: each shard is its own object-store prefix; failover opens the shard elsewhere after fencing | Diskless clusters; also a single node that wants S3 durability for free |
| FoundationDB | Subspaces | Native | Native | Native | Very large deployments, later |

The shard count is fixed at cluster creation and is the unit of ownership. With PostgreSQL the shards are only an ownership concept; with SlateDB they are also storage boundaries, which is exactly what makes lease-fenced failover safe: a new owner cannot write to a shard until the old writer is fenced out.

### 6.6 Sync positions without a global sequence

A single global stream ordering is the coordination point that Synapse's stream writers exist to protect. This design does not have one:

- Each room has a local monotonic position assigned by its owner.
- Each user session keeps a durable, coalesced feed: `(feed_seq, room_sn, room_pos)` entries appended when a room the user is in publishes a new position, with one entry per room per unsynced window (an update to a room already in the unsynced window replaces the entry). Sync tokens are opaque and encode `feed_seq` plus the small independent cursors (to-device, device lists, account data, presence, receipts).
- Fan-out on write to local members is cheap on any server except at matrix.org scale; rooms above a configurable local-member threshold switch to fan-out on read (the user session checks those rooms' positions at sync time), the same hybrid large systems use for high-fan-out sources.
- Everything the user session holds is derivable from room positions and the feed, so failover reloads it.

### 6.7 What else is stored, and where

Global keyspaces (small): server config snapshot, signing keys, server key cache, appservice registry, replica registry and leases, schema version. User shards: profiles, devices, keys, tokens, account data, push rules, pushers, filters, feeds, sliding sync connections, to-device queues, receipts by user. Room shards: everything in 6.2 to 6.4, membership, receipts by room, room account of stats, aliases, retention. Media metadata: by media ID in the global shard, bytes on object storage. Search: `tantivy` indexes per shard, rebuilt from the store if lost.

---

## 7. Kubernetes-native operation, and the small-ARM-host mode

### 7.1 Container and configuration

- Distroless, non-root, read-only root filesystem, multi-arch images for `linux/amd64` and `linux/arm64`, static `musl` binaries (the embedded backend is pure Rust so cross-compiling stays simple; RocksDB, if ever chosen, breaks this and that is a mark against it).
- Configuration is a single YAML document with environment overrides (`HS__section__key`) and file-backed secrets (`*_file` variants for every secret), which is how ConfigMaps and Secrets are meant to be consumed. Reloadable sections (appservices, rate limits, log levels, federation allow and deny lists) are reloaded on SIGHUP or on file change without a restart.
- Synapse's `homeserver.yaml` is accepted through the translator (section 9) and the image entrypoint honors the `SYNAPSE_*` environment variables Synapse's image uses, so existing Helm values and Compose files keep working at the edge.

### 7.2 Lifecycle and probes

- `/health/live`: process is up. `/health/ready`: store reachable, migrations applied, mesh joined, ownership converged for this replica. `/health/startup`: cold-load progress.
- Graceful shutdown on SIGTERM: stop accepting new connections; finish in-flight requests; announce release of owned shards so peers take them over before the lease expires; drain long-polls by answering them so clients reconnect through the Service to another replica; flush; exit inside `terminationGracePeriodSeconds`.
- Rolling updates hand ownership off before a pod stops, which keeps p99 flat instead of paying lease-expiry latency on every rollout.

### 7.3 Scaling and high availability

- Scale-out is `replicas: N`. No worker types, no path-based routing map at the ingress; one Service. This alone removes the most error-prone part of running Synapse at scale.
- Horizontal Pod Autoscaler on CPU and on custom metrics (sync connections, actor queue depth, federation backlog); PodDisruptionBudget; pod anti-affinity; topology spread across zones.
- Storage HA: CloudNativePG with synchronous replication for the PostgreSQL backend; object storage for media and for the SlateDB backend; the store is the only stateful dependency in the default cluster mode.
- Federation: port 8448 on the same Service, or `.well-known` delegation served by the homeserver itself.
- Internal mesh: headless Service for peer discovery, HTTP/2 with mutual TLS (certificates from cert-manager or self-issued and rotated), bounded forwarding fan-out, retries with ownership refresh.
- Cluster membership: a Kubernetes Lease per replica when running with the Kubernetes API available, otherwise heartbeat rows in the store. Ownership is rendezvous hashing of shard ID over live replicas; changes are announced over the mesh and confirmed through the store so two replicas never both believe they own a shard.

### 7.4 The small ARM host

`hs serve --single-node` on a Raspberry Pi 4 or 5 class machine (4 GB) is a supported configuration, not a demo:

- Embedded Fjall store on local disk, media on local disk (or S3 if wanted), no PostgreSQL, no Redis, no mesh.
- Budget: under 150 MB resident at idle with 100 MB of store cache, comfortable at 50 users, 500 rooms and a handful of bridges. These budgets are benchmarked in CI on real `aarch64` runners, not emulated.
- Same binary, same config, same admin API, same console. Moving to a cluster later is an export and import of the store, or, with the SlateDB backend, a change of ownership.
- Also runs on k3s on the same box for people who want the Kubernetes surface at home.

### 7.5 Observability

OpenTelemetry for traces, metrics and logs over OTLP; Prometheus `/metrics` with `hs_*` names plus an optional exporter that emits Synapse's metric names for existing dashboards; structured JSON logs with request IDs, shard and actor context; shipped Grafana dashboards and alert rules; a `ServiceMonitor` in the chart.

### 7.6 Helm chart and operator

- The Helm chart deploys the homeserver, optionally CloudNativePG and an object store, and integrates with the Element Server Suite Community stack as the homeserver component (Element Web, Element Call's backend and Hookshot keep working against it).
- The operator (`kube-rs`) reconciles three custom resources: `Homeserver` (server name, backend, replicas, media, auth, federation policy; status with readiness, schema version, shard map), `AppService` (a registration; status with connectivity, backlog, last successful transaction) and `Bridge` (section 8.4). The operator is optional; the chart alone is enough for a static deployment.

---

## 8. Bridges: making them easy to run

### 8.1 What a bridge needs from the homeserver

The mautrix inventory (Appendix B) reduces to this list, all of which is in scope and on by default:

1. The Application Service API with transactions carrying `events`, `ephemeral` (stable, plus the `de.sorunome.msc2409.ephemeral` legacy key), to-device messages (MSC4203, `de.sorunome.msc2409.to_device`), and the MSC3202 fields for device lists, one-time-key counts (both spellings Synapse sends) and unused fallback key types.
2. Identity assertion with `user_id`, device masquerading with `org.matrix.msc3202.device_id`, timestamp massaging with `ts`, `m.login.application_service` for both exclusive and non-exclusive namespaces (the latter is how double puppeting works), `/register` with `inhibit_login`, MSC4190 device creation and deletion without login, MSC3983 and MSC3984 key proxies when the registration declares them.
3. `/_matrix/client/v1/appservice/{id}/ping` and the outbound `/_matrix/app/v1/ping`.
4. Media: synchronous and async uploads, authenticated downloads and thumbnails, `/media/config`, URL previews, and remote media fetching over federation that copes with a bridge acting as a tiny federation server for direct media (`.well-known`, key server, signed requests, multipart responses with redirects).
5. Everything in the ordinary client API that a puppet uses: profiles including extended fields, room creation with `initial_state` and power-level overrides, membership, state, `/messages`, `/context`, `/relations`, receipts with threads, read markers, typing, presence, account data, tags, aliases, search, user directory, mutual rooms, `/sync` for bots that run in sync mode, key upload and query for encrypted bridges, room key backups.
6. A `/versions` response whose `unstable_features` matches Synapse's for every feature we gate the same way, so bridges take the same code paths they take on Synapse. Beeper's `com.beeper.*` features are not advertised because we do not implement them, exactly as Synapse does not.
7. Rate-limit exemption for `rate_limited: false`, and generous default limits for appservice senders.

### 8.2 The registry

Appservices are rows in the store, not lines in a config file:

- `hs appservice add|show|list|update|pause|resume|remove|rotate-tokens|replay`, the same operations in the admin API, and the same in the console.
- Registration files are still accepted (`app_service_config_files`) and imported into the registry on start, so existing bridges need no change; a bridge added through the registry can export its registration file for the bridge's own config.
- Hot registration: a new appservice is live immediately, with namespace conflict checks against existing registrations and existing users.
- Per-appservice health: last ping round-trip, last successful transaction, backlog depth and age, error rate, and a dead-letter view with replay. Exposed as metrics (`hs_appservice_backlog`, `hs_appservice_txn_latency_seconds`, `hs_appservice_last_success_timestamp`), in the admin API and in the `AppService` resource status.
- `url: null` registrations (double puppeting) are first-class and never pushed to.

### 8.3 Defaults

Ephemeral events, MSC3202 transaction extensions, MSC4203 to-device delivery, MSC4190 device management and the key proxies are enabled whenever the registration asks for them, with no server-side experimental flags to discover. Encrypted bridging works out of the box in both appservice-transaction mode and bot-sync mode.

### 8.4 The `Bridge` custom resource and the console

A `Bridge` resource names a bridge type (`mautrix-whatsapp`, `mautrix-telegram`, `mautrix-signal`, `mautrix-discord`, `mautrix-slack`, `mautrix-gmessages`, `mautrix-meta`, `mautrix-instagram`, `mautrix-twitter`, `mautrix-linkedin`, `mautrix-gvoice`, `mautrix-irc`, `mautrix-zulip`, plus `matrix-hookshot`, `heisenbridge`, `matrix-appservice-irc` and a generic type), an image tag, bridge config values and a reference to a Secret. The operator then: generates the registration and tokens; registers it in the homeserver; provisions the bridge's database (its own CloudNativePG database, or a schema in a shared cluster); deploys the bridge with the right probes and a network policy that only allows the homeserver and the bridge's network; wires up double puppeting with a shared non-exclusive registration; enables the encryption MSCs; and surfaces the bridge's status (`/ping`, backlog, last error) on the resource. `kubectl get bridges` is the operational view.

The management web interface's bridges page is its marquee: bridges and their health, an "add bridge" wizard that produces either a `Bridge` resource or a plain registration and Compose snippet, backlog and replay, per-bridge logs link, token rotation, and deep links to the bridge's own login flow. Bridge login remains where mautrix puts it (bot commands and provisioning APIs); the interface does not duplicate it.

### 8.5 Bridge tests

- `hs-bridge-conformance`: a synthetic appservice written in Rust that replays the exact mautrix call patterns in Appendix B and asserts transaction contents, MSC field spellings, masquerading, `ts`, ping, MSC4190 device flow, async media, direct media through an embedded mini federation server, double-puppet login, rate-limit exemption.
- Real bridges in CI: `mautrix-irc` against an `ergo` IRC server; `mautrix-zulip` against a Zulip container; one `mautrix-python` legacy bridge to cover that framework; `matrix-hookshot`, `heisenbridge` and `matrix-appservice-irc` as non-mautrix sanity; an encrypted-bridging soak in both appservice mode and sync mode.
- The operator's `Bridge` flow tested end to end on a `kind` cluster with `mautrix-irc`.

---

## 9. Compatibility with Synapse and the migration path

### 9.1 Command line and image

| Synapse | Here |
|---|---|
| `synapse_homeserver -c homeserver.yaml` | Shim that runs `hs serve --synapse-config homeserver.yaml`, translating on the fly and printing a translation report. |
| `register_new_matrix_user`, `hash_password`, `generate_signing_key` | Shims with the same flags; the shared-secret registration endpoint and its HMAC protocol are implemented so existing scripts work. |
| `synapse_worker`, `synctl`, `synapse_port_db`, `update_synapse_database`, manhole | Not provided; worker configs are read only to collapse them. |
| Docker `SYNAPSE_SERVER_NAME`, `SYNAPSE_CONFIG_PATH`, `SYNAPSE_REPORT_STATS`, `SYNAPSE_WORKER_TYPES`, ... | Honored by the image entrypoint; worker types are ignored with a notice. |

### 9.2 Configuration translation

Every one of Synapse's 229 documented options and 51 experimental flags has one of three fates in the translation table: mapped (with the same default), mapped with a documented semantic difference, or unsupported with a reason. Unsupported keys fail the start unless `--allow-unsupported-synapse-config` is passed; nothing is silently ignored. Password pepper, signing keys, `macaroon_secret_key`, registration shared secret, appservice registration files, listeners, TLS paths, federation allow and deny lists, rate limits, retention, URL preview settings, email and templates, OIDC and SAML providers, push, MAS delegation, and the media store path are all mapped.

### 9.3 API surface

- Client, federation, appservice, media and key routes: the 236 route patterns in `docs/synapse-inventory.md` are the parity checklist, including legacy `r0` and Synapse-specific unstable paths that clients still hit.
- `/versions`: the same spec versions and the same `unstable_features` gated the same way.
- Admin API: the 77 `/_synapse/admin` routes are served with the same JSON shapes on top of our admin model so `synapse-admin`, Draupnir, Mjolnir and existing scripts keep working; our own versioned admin API sits beside it.
- `/_synapse/client/*` pages (password reset, SSO pages, consent, unsubscribe) at the same routes with our own templates; `/_synapse/mas/*` only in MAS-delegation mode; `/health`; `/_synapse/metrics` through the compatibility exporter.
- Behavior details clients depend on: 404 `M_UNRECOGNIZED` for unknown paths and 405 for wrong methods, `M_LIMIT_EXCEEDED` with `retry_after_ms` and `Retry-After`, `soft_logout`, lenient JSON body parsing regardless of `Content-Type`, the same `unsigned` fields on events, the same bundled aggregations.

### 9.4 The importer

`hs import synapse --postgres <url> --media-dir <path>` is online and incremental:

1. Validates the source schema version (pinned per release; 94 today).
2. Bulk-copies while Synapse keeps running: users and password hashes (bcrypt verifies unchanged; the pepper comes from the config), devices, access and refresh tokens (sessions survive the cutover), device keys, cross-signing keys, key backups, account data, push rules, pushers, filters, profiles, 3PIDs, room directory entries, appservice state, server signing keys, then every room's events and state. State is recomputed by our engine from the events and cross-checked against Synapse's `event_to_state_groups` mapping for every event; a mismatch is a bug report, not a silent difference.
3. Media: either copies from Synapse's `local_content` and `remote_content` layout into the object store, or mounts the existing directory through the local `object_store` backend with a layout adapter and copies lazily.
4. Cutover: stop Synapse, run the final delta (bounded by `stream_ordering` and the per-stream IDs), start `hs`, verify with the differential test harness against a Synapse read replica.
5. Rollback: Synapse's database is never written to, so rolling back is starting Synapse again on it. Activity since the cutover is lost; a reverse exporter is a later item if operators need a longer safety window.

Sync tokens issued by Synapse are accepted after import and treated as "since the import point" for the streams that do not map, so clients do not need to log in again.

### 9.5 What is not carried over

Python modules. The common ones are provided natively (shared-secret authentication for legacy mautrix double puppeting, LDAP, the REST password provider, the S3 storage provider is moot) and the rest are reachable through HTTP-callback modules for each of Synapse's eleven callback categories, or WebAssembly modules for in-process extensions. Redis, workers, `synapse_port_db`, the manhole, and Beeper-specific extensions are not carried over.

---

## 10. Full-spec policy

1. **Coverage is mechanical.** `hs-spec-coverage` reads the spec's OpenAPI trees (`data/api/client-server`, `server-server`, `application-service`, `identity`, `push-gateway` in `matrix-org/matrix-spec` at the pinned tag) and asserts that every path and method is registered in the router, that every test response validates against the spec's response schema, and that every error code in the spec's enumerations is producible. The report is published per release. Today's target is v1.19.
2. **All twelve room versions**, with their event formats, auth rules, redaction algorithms and state resolution variants, because federation with the existing network requires them.
3. **All five APIs, but we implement three.** Client-Server, Server-Server and Application Service are the homeserver and are ours. The Identity Service and Push Gateway APIs are consumed rather than served: we talk to whatever identity server an operator configures, and we post notifications to Sygnal or any compatible gateway. Serving those two is out of scope per decision D0, and the coverage tool reports them as consumed rather than counting them against us.
4. **MSCs are chosen by who needs them**, not by novelty. The required set at first release: simplified sliding sync (MSC4186) and its extensions as they are accepted; delayed events (MSC4140), MatrixRTC transports and call membership (MSC4143), room summary (MSC3266) and `state_after` (MSC4222) for Element Call and Element X; dehydrated devices (MSC3814) and QR login rendezvous (MSC4108) for Element X; the appservice family (MSC2409, MSC3202, MSC4203, MSC4190, MSC3983, MSC3984) for bridges; view-redacted-content (MSC2815), user redaction (MSC4194), mutual rooms, extended profiles and account moderation as the bridges probe for them; push for encrypted events (MSC4028), pusher enablement (MSC3881), account-data deletion (MSC3391), invite filtering (MSC4155), thread subscriptions (MSC4306 and MSC4308), sticky events (MSC4354), policy servers (MSC4284), MSC4242 state DAGs as experimental. Everything else on Synapse's 51-flag list is tracked and added on demand.
5. **Spec releases are tracked within thirty days**: regenerate coverage, ticket gaps, ship them. Ruma lags the spec by a version; we contribute the missing types upstream rather than forking.

---

## 11. Workstreams

Each workstream names its scope, the definition of done, and the tests that define it. Synapse's test tree is cited as a reference for intent; its tests are not copied.

| WS | Scope | Done when |
|---|---|---|
| WS0 Foundations | Workspace, CI (Linux amd64 and arm64 runners, macOS build), images, `hs-testkit`, fuzz and bench scaffolds, `hs-spec-coverage`, licensing and contribution rules | A PR runs unit tests, coverage report and micro-benchmarks on both architectures. |
| WS1 KV and tables | `hs-kv` trait and the Fjall and PostgreSQL backends, `hs-tables` with declarative indexes and migrations, interning | Backend conformance suite (transactions, isolation anomalies, range scans, watches, crash recovery) passes on both backends; property tests for index maintenance. |
| WS2 State | The bake-off (section 6.3), the chosen representation, chain-cover index, event auth for versions 1 to 12, `ruma-state-res` integration, the in-house oracle, MSC4242 tables | Bake-off report published; cross-implementation property tests (ours vs oracle vs Synapse via a Python harness) pass on 10k random DAGs; corpus benchmarks tracked. |
| WS3 Room actor and events | Event creation and validation, persistence, extremities, timeline, relations and threads, redactions, retention and purge, upgrades, spaces, summaries | Complement `csapi` room tests pass in single-node mode; differential tests against Synapse on the scripted room workload are clean. |
| WS4 Client API and auth (legacy) | Legacy login and UIA, registration, tokens, devices, profiles, account data, filters, presence, receipts, typing, read markers, search, directory, user directory, capabilities, `/versions` | Coverage tool at 100 percent for the client OpenAPI tree minus E2EE and sliding sync; Complement `csapi` green. |
| WS5 Sync | `/sync` v2, `initialSync`, simplified sliding sync with extensions, the user session actor and feeds, hybrid fan-out | Element Web and Element X (via `matrix-rust-sdk`) work against a single node; sync latency targets met. |
| WS6 Federation | Everything in section 5.5's federation row; faster joins; MSC4242 experimental; sender shards | Complement federation tests and Sytest federation suites at Synapse's pass level; fuzz targets for every inbound path; external security review scheduled. |
| WS7 E2EE support | Keys, cross-signing, backups, device lists, to-device, dehydrated devices, rendezvous, appservice key proxies | `matrix-rust-sdk` E2EE suite (session establishment, verification, backup and recovery, bridge bot with MSC4190) passes. |
| WS8 Media | Object-store repository, uploads, downloads, thumbnails, previews, quarantine, retention, remote cache, direct media compatibility | Complement media tests; decoder fuzzing; bridge direct-media conformance. |
| WS9 Push | Rules engine, compiled per-user rules, counts, HTTP and email pushers, templates, the optional push gateway component | Complement push tests; golden template rendering; gateway tested with a fake APNs and FCM. |
| WS10 Appservices | Registry, scheduler, all the MSCs, ping, third-party lookups, health, console backend | `hs-bridge-conformance` green; real bridges green in CI. |
| WS11 Native OAuth 2.0 and IdPs | Authorization server and OIDC issuer per the spec's next-gen auth API, account management UI, upstream OIDC, SAML, LDAP, MAS delegation mode | Element X logs in through the native issuer; `matrix-rust-sdk` OIDC flow tests; conformance against the spec's OAuth requirements. |
| WS12 Cluster | Replica registry, leases, rendezvous hashing, mesh, forwarding, failover, graceful handoff, SlateDB backend | Chaos suite on `kind` (pod kills, partitions, rolling updates) shows no lost or duplicated events, bounded failover time, flat p99 on rollouts. |
| WS13 Admin API | Public admin API (`/api/v1`), OpenAPI, scopes, audit log, event stream, generated clients, scheduled tasks, server notices | Contract tests between OpenAPI and handlers; every resource tested for pagination and authorization; audit log complete. |
| WS13b Compat and migration | Synapse admin API surface, `/_synapse/client` pages, config translator, importer, metric-name exporter, CLI shims | `synapse-admin` and Draupnir work; importer round-trip test from a Synapse 1.161 database passes differential reads. |
| WS18 Management web interface | Product design, design system, the application on the admin API, embedded build | Playwright tests for every flow; accessibility checks clean; usability pass recorded. |
| WS14 Kubernetes | Helm chart, operator with the three CRDs, probes, HPA metrics, dashboards, ESS integration | Chart and operator tested on `kind` in CI; `Bridge` flow green with `mautrix-irc`. |
| WS15 Modules | HTTP-callback modules for the eleven callback categories, WebAssembly module host, native ports of the common Synapse modules | Each category exercised by a test module; the shared-secret and LDAP ports pass their upstream test intent. |
| WS16 Identity and search | `tantivy` search and user directory; the optional identity service component | Search results match Synapse's on the differential corpus; identity API coverage at 100 percent. |
| WS17 Docs and packaging | Documentation site, migration guide, operator guide, Debian and RPM packages, Nix flake, release train | A new operator can deploy single-node and cluster modes from the docs alone. |

---

## 12. Test strategy

"Tests for all functionality" is met by layering suites so that every feature is covered by at least two of them.

| Layer | What | Notes |
|---|---|---|
| L0 Unit and property tests | Every crate; property tests for canonical JSON, redaction, token codecs, index maintenance, state map operations; the state-resolution oracle | `proptest`, `cargo test` on both architectures. |
| L1 In-process harness (`hs-testkit`) | Fake clock, in-memory KV, fake federation peers, fake appservice, fake SMTP, fake push gateway, fake identity server, `make_request` helpers, scripted multi-user scenarios | Mirrors the intent of Synapse's `HomeserverTestCase` without its internals. |
| L2 Spec coverage | OpenAPI-driven route registration and response-schema validation for all five APIs | Fails the build on any unregistered path. |
| L3 Complement | Upstream Complement with no blacklist as the goal (Synapse's blacklist as the interim bar), plus Synapse's in-repo Go tests, run in single-node and cluster modes | Palpo's self-reported 672 pass, 0 fail, 14 skip is the bar for a new Rust server. |
| L4 Sytest | A Sytest homeserver plugin for our binary; run with Synapse's expectations | Kept until Complement reaches Sytest parity. |
| L5 Differential against Synapse | The same scripted client and federation workloads against Synapse 1.161 and us; normalized response diffs; state and auth cross-checks through a Python harness that drives Synapse's own implementations | Clean diffs are the definition of behavioral compatibility. |
| L6 Client end-to-end | `matrix-rust-sdk` flows (login, OAuth, E2EE, verification, backups, sliding sync, calls signaling), Element Web smoke tests with Playwright | The SDK is the same code Element X runs. |
| L7 Bridges | Section 8.5 | |
| L8 Cluster and chaos | `kind` clusters; pod kills, partitions, slow stores, rolling updates; a per-room linearizability checker over recorded histories | Jepsen-style, room by room. |
| L9 Migration | Synapse 1.161 database and media fixtures imported and verified by L5 reads; token survival | Per supported Synapse version. |
| L10 Fuzzing | `cargo-fuzz` targets for PDUs, EDUs, canonical JSON, `X-Matrix` headers, `.well-known`, multipart media, URL preview HTML, images, appservice registration regexes, sync filters, OAuth requests | Run continuously. |
| L11 Performance | `criterion` micro-benchmarks gated at 5 percent; nightly macro runs with `hs-loadgen` publishing section 13's table; the state corpus benchmarks | On amd64 and arm64. |
| L12 Upgrades | Store migrations across releases on every backend; rolling cluster upgrades with mixed versions for one release step | |

Coverage is reported as a single parity dashboard: spec coverage percentage, Complement and Sytest pass counts, Synapse route checklist completion, bridge conformance, and the performance table.

---

## 13. Performance targets and benchmarking

Targets are measured against monolithic Synapse 1.161 on the same hardware, same PostgreSQL where applicable, and published from the nightly run. They are targets, not promises, until the numbers exist.

| Metric | Target |
|---|---|
| Events persisted per second per core, local senders across many rooms | 10x Synapse |
| Incremental `/sync` and sliding sync p99 with 10k connected users on 8 cores | under 30 ms |
| Federation inbound `/send` transactions per second per core | 10x Synapse |
| Join a 100k-member room (`send_join` processing, excluding network) | under 20 s single node |
| State store bytes per state event on the corpus | at most half of Synapse's `state_groups_state` bytes for the same history |
| Resident memory at steady state, 10k active users | at most one quarter of an equivalent Synapse worker fleet |
| Single node on a 4 GB ARM host, 50 users, 500 rooms, three bridges | under 400 MB resident, p99 sync under 100 ms |
| Failover of a room shard in cluster mode | under 5 s to first successful write on the new owner |
| Cold start to ready | under 5 s single node; under 30 s per replica in a large cluster |

Infrastructure: `hs-loadgen` (`matrix-rust-sdk` clients, real E2EE, sliding sync); a federation peer simulator replaying recorded transactions; the state corpus of section 6.3; synthetic stores at 1M, 10M and 100M events for every backend; dedicated amd64 and arm64 runners.

---

## 14. Roadmap, milestones, effort

Assumes four to six senior engineers with Rust and Matrix experience. Months are cumulative from the start.

| Phase | Months | Deliverable | Exit criteria |
|---|---|---|---|
| 0. Foundations and bake-off | 0 to 3 | `hs-kv` with Fjall and PostgreSQL; `hs-tables`; the state bake-off on the corpus; a cluster ownership prototype (leases, mesh, forwarding) on `kind`; `hs-spec-coverage`; CI on both architectures | Bake-off decision published; ownership prototype survives chaos tests; both backends pass the conformance suite. |
| 1. Single-node client server | 3 to 7 | Legacy auth, rooms, sync v2 and simplified sliding sync, media, push, search, user directory, admin API v1 core, management interface skeleton on the API | Complement `csapi` green; Element Web and Element X usable against one node; spec coverage 100 percent for the client tree minus federation-dependent parts. |
| 2. Federation | 6 to 11 | Full federation, room versions 1 to 12, faster joins, sender shards, MSC4242 experimental, server ACLs, policy servers | Complement federation and Sytest at Synapse's level; fuzzing running; joins Matrix HQ from a single node inside the target. |
| 3. Encryption, bridges, early adopter release | 9 to 13 | E2EE support, appservice registry and scheduler with all MSCs, bridge conformance, real bridges in CI, importer for users and rooms | **0.x release for single-node early adopters with bridges.** Encrypted bridging works in both modes. |
| 4. Cluster and Kubernetes | 12 to 18 | PostgreSQL cluster mode with leases and failover, Helm chart, operator with the three CRDs, `Bridge` flow, native OAuth 2.0 issuer, Synapse admin API surface, config translator, full importer, metric exporter | Chaos suite green; ESS-style deployment works with our chart; Synapse migration rehearsed on a real deployment. |
| 5. Hardening and 1.0 | 17 to 23 | External security review of federation and auth, performance validation against section 13, SlateDB backend, packaging, docs, migration guide, release train | **1.0.** All parity dashboards at target; two production deployments migrated from Synapse and running for 90 days. |
| 6. Beyond | after 1.0 | FoundationDB backend, identity and push gateway components, WebAssembly module ecosystem, reverse exporter, MSC4242 as it stabilizes | |

Effort: roughly 100 to 150 engineer-months to 1.0. A four-person team reaches the early adopter release around month 12 and 1.0 around month 24; six people compress that by about a third but not more, because federation and cluster work are hard to parallelize beyond a point. A one- or two-person effort should expect four or more years and should scope down to single-node first.

---

## 15. Risks

| Risk | Impact | Mitigation |
|---|---|---|
| The state bake-off favors none of the candidates decisively, or the winner has an unforeseen pathology on real rooms | Rework in the most central component | The `hs-state` API is fixed before the bake-off; the corpus is real; the decision is reversible until Phase 2 because nothing else depends on the representation's internals. |
| Hand-maintained indexes over a KV store drift (the Conduit lineage's chronic bug class) | Data inconsistencies | Declarative indexes in `hs-tables`, property tests, and a background index verifier that samples and repairs. |
| Sharded ownership has split-brain or handoff bugs | Lost or duplicated events, the worst failure a homeserver can have | Ownership confirmed through the store, fenced writers on SlateDB, per-room linearizability checker in chaos CI, conservative lease timing, single-node mode has none of this code active. |
| PostgreSQL as a KV store is slower than expected | Cluster performance short of target | Pipelined multi-get and range scans, room-owner memory absorbs the hot path, and the FoundationDB and SlateDB backends exist precisely as escape hatches. |
| Federation correctness and security | Room takeovers, state resets, denial of service | Twelve room versions implemented from the spec with cross-implementation tests, fuzzing, differential tests against Synapse on recorded traffic, an external review before 1.0. |
| Protocol velocity (sliding sync extensions, MSC4242, new room versions) | Perpetual catch-up | Thirty-day spec tracking, Ruma contributions, the coverage tool, and a design that keeps MSC4242's DAG tables ready. |
| Native OAuth 2.0 issuer is a large surface with security consequences | Account compromise | Scope it after legacy auth ships; use well-reviewed crates for JOSE and OIDC client parts; include it in the external review; keep the MAS delegation mode as an alternative. |
| Bridge behavior depends on Synapse quirks not captured in Appendix B | Bridge breakage after migration | Real bridges in CI, the conformance suite, and differential tests of the appservice transaction stream against Synapse. |
| Small-host and cluster requirements pull the design in opposite directions | Neither is good | The KV abstraction and the ownership layer are the only places the two modes differ; single-node is a strict subset with the cluster code inactive, and both are benchmarked in CI. |
| Team and knowledge | Schedule | Hire from the Ruma, Conduit-lineage and Synapse communities; publish the design early to attract reviewers. |

---

## 16. Decisions needed from you

1. **License:** Apache-2.0 (recommended) or AGPL-3.0 (allows porting Synapse's Rust code).
2. **Default cluster backend:** PostgreSQL via CloudNativePG (recommended) or SlateDB on object storage as the primary with PostgreSQL secondary.
3. **Native OAuth 2.0 issuer in scope for 1.0** (recommended) or MAS delegation only at 1.0 with the native issuer after.
4. **Room-version floor:** support all twelve (recommended, needed for federation with old rooms) or start at 6 and add 1 to 5 later.
5. **Optional components at 1.0:** ship the identity service and push gateway components (recommended for a complete self-hosted stack) or defer.
6. **Team size and timeline appetite**, which sets the phase dates.
7. **Project name**, and whether the Synapse-named CLI shims should be part of the product or a separate compatibility package.
8. **Management interface stack** (recommended: TypeScript, React, Vite, Tailwind, Radix, embedded in the binary) and whether it may also be deployed separately in the cluster.

---

## Appendix A. Crate and dependency choices

| Concern | Choice | Alternatives considered |
|---|---|---|
| Runtime and HTTP | `tokio`, `hyper` 1.x, `axum`, `rustls`, `tower` | Salvo (used by Palpo; smaller ecosystem) |
| Matrix types | `ruma` (identifiers, events, API types, signatures, state resolution) | Own types (rejected; Ruma is the ecosystem standard and MIT) |
| JSON | `serde_json` with borrowed parsing; canonical JSON via `ruma-signatures` plus in-house tests against Synapse's behavior | `simd-json` for hot parsing paths if profiling justifies it |
| Crypto | `ed25519-dalek` (batch verification), `sha2`, `hmac`, `bcrypt` (Synapse hash compatibility), `argon2` (native default), `josekit` or `jsonwebtoken` | |
| Embedded store | Fjall 3 | RocksDB (fallback), redb (small keyspaces) |
| Cluster store | PostgreSQL via `tokio-postgres` and `deadpool-postgres` | SlateDB (diskless), FoundationDB (`foundationdb-rs`, later) |
| Object storage | `object_store` | |
| Search | `tantivy` | PostgreSQL full text (backend-specific, rejected) |
| Caches | `moka` or `quick_cache`; persistent maps via `imbl` or in-house HAMT matching the store's node format | |
| Federation DNS | `hickory-resolver` | |
| Templates | `minijinja` (Jinja2-compatible, so Synapse-customized email templates can be reused) | |
| Email | `lettre` | |
| Images | `image`, `libwebp` bindings if needed, `kamadak-exif` | |
| OAuth and OIDC client side, SAML, LDAP | `openidconnect`, `oauth2`, `samael`, `ldap3` | |
| Push gateway | `a2` (APNs), FCM HTTP v1 via `reqwest`, `web-push` | |
| Telemetry | `tracing`, `opentelemetry`, `prometheus-client`, `sentry` | |
| Kubernetes | `kube-rs` for the operator; Helm for the chart | |
| Mesh | HTTP/2 over `hyper` with mutual TLS; `tonic` if a typed RPC layer proves worth it | |
| Testing | `proptest`, `cargo-fuzz`, `criterion`, `insta` snapshots, Complement, Sytest, `matrix-rust-sdk`, Playwright | |
| WebAssembly modules | `wasmtime` with the component model | |

## Appendix B. What the mautrix bridges call on a homeserver

Measured from `mautrix/go` (the `bridgev2` framework every maintained bridge uses) and `mautrix/python` on 2026-09-17.

**Registration fields read by the frameworks:** `id`, `url` (may be null), `as_token`, `hs_token`, `sender_localpart`, `rate_limited`, `namespaces` (`users`, `aliases`, `rooms` with `regex` and `exclusive`), `protocols`, `receive_ephemeral`, `de.sorunome.msc2409.push_ephemeral`, `org.matrix.msc3202`, `io.element.msc4190`.

**Transaction fields parsed (stable name first, legacy fallback):** `events`; `ephemeral` or `de.sorunome.msc2409.ephemeral`; `to_device` or `de.sorunome.msc2409.to_device`; `device_lists` or `org.matrix.msc3202.device_lists`; `device_one_time_keys_count` or `org.matrix.msc3202.device_one_time_keys_count` (Synapse also sends the older `org.matrix.msc3202.device_one_time_key_counts`); `device_unused_fallback_key_types` or `org.matrix.msc3202.device_unused_fallback_key_types`.

**Query parameters used:** `user_id` (identity assertion), `org.matrix.msc3202.device_id` (device masquerading), `ts` (timestamp massaging).

**Feature flags probed on `/versions`, with the spec version that supersedes each:** `fi.mau.msc2246.stable` (async uploads, v1.7), `fi.mau.msc2659.stable` (appservice ping, v1.7), `org.matrix.msc3916.stable` (authenticated media, v1.11), `uk.half-shot.msc2666.query_mutual_rooms` and `.stable` (mutual rooms, v1.19), `org.matrix.msc4194` (user redaction), `fi.mau.msc2815` (view redacted content), `uk.timedout.msc4323` and `.stable` (account moderation, v1.18), `uk.tcpip.msc4133` and `.stable` (extended profiles, v1.16), `com.beeper.msc4169` (redact send-as event), `com.beeper.msc4437` and `.stable` (replace whole profile), `com.beeper.msc4446` (fully-read backward), `org.matrix.msc4143` and `.stable` (MatrixRTC), plus Beeper-only flags (`com.beeper.hungry`, `batch_sending`, `room_yeeting`, `room_create_autojoin_invites`, `arbitrary_profile_meta`, `account_data_mute`, `inbox_state`, `arbitrary_member_change`) that Synapse does not advertise and we do not either. The minimum spec version `bridgev2` accepts is v1.4.

**Login types:** `m.login.application_service` (with `/register` `inhibit_login`), `m.login.password`, `m.login.token`, `m.login.sso`, `org.matrix.login.jwt`, and `com.devture.shared_secret_auth` (legacy double puppeting through the shared-secret module).

**Client endpoints called** (`/_matrix/client/...`): `versions`; `v3/login`, `logout`, `logout/all`, `register`, `register/available`, `account/whoami`, `capabilities`, `devices`, `devices/{id}` (including MSC4190 `PUT` to create a device), `delete_devices`, `user/{id}/filter`, `user/{id}/account_data/{type}`, `user/{id}/rooms/{room}/account_data/{type}`, `user/{id}/rooms/{room}/tags`, `user/{id}/openid/request_token`, `profile/{id}` and `profile/{id}/{field}`, `presence/{id}/status`, `createRoom`, `join/{room}`, `knock/{room}`, `joined_rooms`, `publicRooms`, `directory/room/{alias}`, `rooms/{room}/{invite,join,leave,kick,ban,unban,forget}`, `rooms/{room}/send/{type}/{txn}`, `rooms/{room}/state` and `state/{type}/{key}`, `rooms/{room}/event/{id}`, `rooms/{room}/context/{id}`, `rooms/{room}/messages`, `rooms/{room}/members`, `rooms/{room}/joined_members`, `rooms/{room}/aliases`, `rooms/{room}/redact/{id}/{txn}`, `rooms/{room}/receipt/{type}/{id}`, `rooms/{room}/read_markers`, `rooms/{room}/typing/{id}`, `rooms/{room}/report` and `report/{id}`, `search`, `sync`, `user_directory/search`, `pushrules/...` (`actions`, `enabled`), `pushers`, `pushers/set`, `keys/{upload,query,claim,changes}`, `keys/device_signing/upload`, `keys/signatures/upload`, `room_keys/version`, `room_keys/keys[/{room}[/{session}]]`, `sendToDevice/{type}/{txn}`, `voip/turnServer`, `admin/whois/{id}`; `v1/appservice/{id}/ping`, `v1/rooms/{room}/relations/{id}[/{type}[/{eventType}]]`, `v1/rooms/{room}/hierarchy`, `v1/rooms/{room}/timestamp_to_event`, `v1/room_summary/{room}`, `v1/mutual_rooms`, `v1/media/{download,thumbnail,config,preview_url}`, `v1/auth_metadata`, `v1/rtc/transports`, `v1/username_available`, `v1/admin/{action}/{target}`; unstable `im.nheko.summary`, `org.matrix.msc4140/delayed_events`, `org.matrix.msc4143/rtc/transports`, `org.matrix.msc4174/pushers/ack`, `org.matrix.msc4194/rooms/{room}/redact/user/{user}`, `uk.half-shot.msc2666/user/mutual_rooms`, `uk.tcpip.msc4133/profile/{id}/{key}`, `uk.timedout.msc4323/admin/{action}/{target}`, and the Beeper-only `com.beeper.*` paths that are gated off by `/versions`.

**Media endpoints called:** `POST /_matrix/media/v3/upload`, `POST /_matrix/media/v1/create`, `PUT /_matrix/media/v3/upload/{server}/{id}`, `GET /_matrix/media/v3/config`, `preview_url`, and the authenticated `client/v1/media/*` equivalents.

**Federation endpoints the bridge itself serves for direct media** (so our federation client must handle them correctly): `/.well-known/matrix/server`, `/_matrix/key/v2/server`, `/_matrix/key/v2/query`, `/_matrix/federation/v1/version`, `/_matrix/federation/v1/media/download/{id}`, `/_matrix/federation/v1/media/thumbnail/{id}`.

**Federation client endpoints in `mautrix-go`** (used by tooling, not by bridges at runtime): `v1/version`, `v1/query/{type}`, `v2/query`, `v2/server`, `v1/event/{id}`, `v1/state/{room}`, `v1/state_ids/{room}`, `v1/event_auth`, `v1/backfill`, `v1/get_missing_events`, `v1/make_join`, `v2/send_join`, `v1/make_leave`, `v2/send_leave`, `v1/make_knock`, `v1/send_knock`, `v2/invite`, `v1/send/{txn}`, `v1/publicRooms`, `v1/openid/userinfo`, `v1/timestamp_to_event`, `v1/media/download`.

**Content-level conventions** (no server support needed, listed so nobody "fixes" them): `fi.mau.will_auto_accept` in invites, `com.beeper.exclude_from_timeline`, `com.beeper.linkpreviews`, `fi.mau.gif`, `fi.mau.event_id`, `org.matrix.msc3381.poll.*`, `org.matrix.msc4354.sticky_duration_ms`, `io.element.functional_members`, Mjolnir policy event types.

## Appendix C. Sources

- Synapse 1.161.0 checkout of `element-hq/synapse` (2026-09-15): `pyproject.toml`, `CHANGES.md`, `synapse/rest/**`, `synapse/federation/transport/server/**`, `rust/src/handlers/versions.rs`, `rust/src/room_versions.rs`, `synapse/config/experimental.py`, `docs/usage/configuration/config_documentation.md`, `synapse/storage/schema/__init__.py`, `synapse/replication/**`, `synapse/module_api/**`, `docs/workers.md`, `docker/`, `.github/workflows/tests.yml`, `synapse/types/__init__.py`, `synapse/handlers/auth.py`, `synapse/appservice/api.py`, `synapse/media/filepath.py`.
- `matrix-org/synapse` README: archived, development moved to `element-hq/synapse`.
- Matrix specification v1.19 index, room versions page, and the v1.19 changelog (spec.matrix.org).
- Matrix.org blog: "Project Hydra: Improving state resolution in Matrix" (2025-08); "Matrix v1.16 release" (2025-09-17); "Sunsetting the Sliding Sync Proxy" (2024-11-14); This Week in Matrix 2026-07-03 (MSC4186 acceptance).
- MSC4242 State DAGs (matrix-org/matrix-spec-proposals #4242) and the Synapse serving PR (element-hq/synapse #20133).
- Ruma README (0.15, Matrix 1.18, MIT, MSRV 1.89).
- `mautrix/go` and `mautrix/python` checkouts (2026-09-17); mautrix docs on end-to-bridge encryption and double puppeting; mau.fi August 2026 release post; the mautrix GitHub organization listing.
- Palpo (`palpo-im/palpo`, checkout 2026-09-09 head) README and `crates/data/migrations`; upstream Conduit (`gitlab.com/famedly/conduit`, head 2026-08-26); continuwuity.org and matrix-construct/tuwunel for lineage status; Dendrite's Component Design wiki.
- Fjall 3.0 release post; SlateDB site; redb README; FoundationDB Kubernetes operator releases (v2.24, arm64); openraft README; CloudNativePG releases (1.29); Element Server Suite Community Helm charts; community mautrix Helm charts (`mautrix-go-base`, `wrenix/mautrix-bridge`).
- Complement README (Docker interface, Sytest parity tracking); Synapse's `scripts-dev/complement.sh` and in-repo `complement/` suite.
- Matrix Authentication Service repository and release notes (AGPL since 0.12; Synapse-only delegation).
- Synapse state-groups documentation and `rust-synapse-compress-state` for the state-store pathology.
