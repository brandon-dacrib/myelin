# The other homeservers, and where this one stands

Checked 2026-09-26 from each project's own repository, documentation site and release notes;
sources at the end. `PLAN.md` section 2.3 has the design lessons taken from each; this file is
the operator's view: what each server is, who is behind it, how it is installed, how it scales
and how it is administered. It exists so the claim in `docs/decisions/0008-the-standout-is-
operations.md` is made against the field as it actually is, not as it was remembered.

## In one table

| Server | Language, store | Backing | Install | Scale-out | Administration |
|---|---|---|---|---|---|
| Synapse | Python (and Rust), PostgreSQL | Element, commercial | pip, Docker, Ansible, ESS Helm | worker types behind a path-routing proxy, Redis replication | Synapse Admin API, Element Admin, `synapse-admin` |
| Dendrite | Go, PostgreSQL or SQLite | Matrix.org Foundation, maintenance mode | Docker, binary | one process (the function-split "polylith" was abandoned) | admin API subset |
| Conduit | Rust, RocksDB or SQLite | Famedly, beta | Docker, binary, distro packages | one process | admin room commands |
| continuwuity | Rust, RocksDB | community (conduwuit's successor) | Docker, NixOS, binary, packages; a sample StatefulSet | one process, by its own documentation | `!admin` commands in an admin room; an admin API is a stated work item |
| tuwunel | Rust, RocksDB | Matrix Construct, sponsored by the Swiss government, full-time staff | static binary, Docker, deb/rpm/apt/COPR/AUR/Nix/Alpine/Gentoo, Ansible; community Helm charts | one process, by its own documentation | admin commands, `--execute` at startup, Synapse-compatible admin API since 1.8.1 |
| Palpo | Rust, PostgreSQL | small team, not production-proven | Docker, binary | a "cluster support" migration exists | web admin, Synapse admin-API tables |
| **Myelin** | Rust, embedded (Fjall) or PostgreSQL | this project | `docker run` with one variable; `helm install` with one value | `replicas: N`, one Service, rooms owned by lease and forwarded over a mesh | admin API with OpenAPI and audit log, management web interface, `hs config` |

The Conduit lineage, which is where the Rust energy in this ecosystem has gone, has optimised
for one fast process on one machine and says so plainly: both continuwuity's and tuwunel's
Kubernetes pages open with "doesn't support horizontal scalability or distributed loading
natively". Synapse scales, but by an operator hand-assigning worker types and maintaining a
routing map at the proxy, which is the part of running Synapse at scale that goes wrong. Nobody
in the field offers a homeserver where scale is a replica count, install is one value, and
administration is a page in a browser. That gap is the product.

## Synapse

The reference implementation and the behavioural target for this project (`PLAN.md` section
2.1, `docs/synapse-inventory.md`). `element-hq/synapse`, AGPL-3.0 with a commercial licence,
1.161.0 at the time of the plan. Python with Rust for the hot paths; PostgreSQL. Scale-out is
worker processes of named types (`synchrotron`, federation sender, event persister, ...) fed by
Redis replication and reached through a reverse proxy configured with a per-path routing table
that the operator maintains by hand. The Element Server Suite Community Helm charts (AGPL)
deploy it with Matrix Authentication Service, Element Web, Element Call's backend and Hookshot;
our chart is modelled on that one's Synapse values so it can take that component's place.
Administration is the Synapse Admin API, which Element Admin and `synapse-admin` sit on. The
known operational pathology is `state_groups_state`, routinely most of the database, needing an
external compressor. The migration path from it is ours to build (`PLAN.md` section 9).

## Dendrite

Go, `matrix-org/dendrite`, in maintenance mode. Its lasting contribution is a lesson: it split
by *function* into micro-services (roomserver, syncapi, federationapi, ...) connected by Kafka or
NATS, found that operationally painful, and retreated to a monolith. This project splits by
*data* (rooms, users) into identical replicas instead (`PLAN.md` section 5).

## Conduit, and the two servers that came out of conduwuit

**Conduit** (`gitlab.com/famedly/conduit`, Apache-2.0, Rust, RocksDB or SQLite behind a small
ordered-KV abstraction, axum) is the proof that a whole homeserver can be written against an
ordered key-value store with hand-maintained indexes. It is still beta. **conduwuit** was the
performance-minded fork that most people running "Conduit" actually ran; it was archived, and
two projects claim its succession.

### continuwuity

- `forgejo.ellis.link/continuwuation/continuwuity`, docs at `continuwuity.org`. Apache-2.0,
  Rust, RocksDB. Describes itself as "the official community continuation of the conduwuit
  homeserver" and "a community-driven Matrix homeserver in Rust".
- Releases: v26.9.0 on 2026-09-16, v26.8.1 on 2026-08-22, v26.7.3 on 2026-08-11. v26.9.0
  removed "challengeless registration" (a server now needs tokens, CAPTCHA or email
  verification), turned unauthenticated legacy media off by default, added OIDC provider
  name and icon options and `connection_uri_file`, advertised MatrixRTC on `/versions`, and
  fixed a v26.8.0 regression that stalled outbound federation.
- Install: a TOML file with environment-variable overrides; Docker images for amd64 and arm64,
  a NixOS flake, a generic binary, "traditional package managers". The Kubernetes page gives a
  sample StatefulSet with `replicas: 1` and a PVC, tells the operator to write the Service,
  Ingress and PVC themselves, points at a community Helm chart written for conduwuit that the
  project does not maintain, and states: "Continuwuity doesn't support horizontal scalability
  or distributed loading natively."
- Administration: `!admin` commands typed into an admin room in a Matrix client, in eleven
  groups (appservices, users, token, oidc, rooms, federation, server, media, check, debug,
  query), optionally a console. No web interface. "Admin API" is listed among the development
  priorities as work in progress.
- Stated goals: stability, bug fixes, missing features, documentation, sustainable
  development, and "a lightweight, efficient codebase that can run on modest hardware".

### tuwunel

- `github.com/matrix-construct/tuwunel`. Apache-2.0, Rust, RocksDB. "The official successor
  to conduwuit after it reached stability." Sponsorship is stated on the front page: "primarily
  sponsored by the government of Switzerland, where it is currently deployed for citizens",
  and "used by many companies with a vested interest in its continued development by full-time
  staff". Positions itself as "a scalable, low-cost, enterprise-ready, community-driven
  alternative" to Synapse that implements "the Matrix Specification for all but the most niche
  uses".
- Releases: v1.9.3 on 2026-09-25, v1.9.2 on 2026-09-20, v1.9.1 on 2026-09-12, v1.9.0 on
  2026-08-18, v1.8.3 on 2026-08-05. Recent work: room version 12, sliding sync with
  required-state delivery, user status through sync, animated and video thumbnails, URL
  previews, QR-code login, MSC3664 push rules, aws-lc-rs TLS, an AppArmor profile, systemd
  socket activation and configuration reload, journald logging, RocksDB checkpoint exports and
  column-family backups.
- Install: the widest menu in the field. Static binaries, Docker on Docker Hub and GHCR, deb
  and rpm with an apt repository and a COPR, Arch, Alpine, Gentoo, NixOS with a binary cache,
  and the `matrix-docker-ansible-deploy` playbook. A TOML file, `TUWUNEL_` environment variables
  with `__` nesting, `-O` for single options, and `--execute` to run admin commands at startup.
  The Kubernetes page says the same sentence continuwuity's does, "Tuwunel doesn't support
  horizontal scalability or distributed loading natively", recommends `tuwunel --health-check`
  for the three probes, and warns to raise `terminationGracePeriodSeconds` past the longest
  database migration. Helm charts exist (`AreYouLoco/tuwunel-helm`, `Arsolitt/tuwunel-helm`
  with LiveKit for Element Call) and are community-maintained, not the project's.
- Administration: admin commands, and since v1.8.1 a Synapse-compatible admin API covering
  users, rooms, media and devices, which is the first of the Conduit lineage to offer one. No
  web interface of its own. Migration *into* tuwunel from Conduit, conduwuit and their forks is
  an in-place database reconciliation; from Synapse, "not yet, but this is planned and an
  important issue".

Between them these two are the state of the art for a small, fast, single-process Rust
homeserver, and tuwunel in particular is moving quickly, with money and staff. Competing with
them on that ground would be a second implementation of a solved problem (`docs/decisions/
0007-build-less-reuse-more.md`). What neither does, and neither says it intends to do, is run as
more than one process.

## Palpo

`palpo-im/palpo`, Apache-2.0, Rust on PostgreSQL through diesel-async, 174k lines. The closest
existing design to a Postgres-backed Rust server: interned state, deduplicated state frames with
deltas, an auth-chain index, sliding sync, the appservice MSCs the bridges need, a web admin
and Synapse admin-API tables, and a "cluster support" migration that moves in-memory state into
the database. Self-reported Complement 672 pass, 0 fail; self-described as not production-proven.
Its storage design informed `PLAN.md` section 6 and the state bake-off
(`docs/decisions/0005`, `0006`).

## Where Myelin stands against that, honestly

What is real today, verified by running it (`docs/next-steps.md` has the basis for every line):

- **Install is one command or one value.** `docker run -e HS__SERVER__SERVER_NAME=example.org
  -v myelin:/data ...` and `helm install myelin <chart> --set serverName=example.org` each
  produce a server with its signing key, database and media on one volume, no configuration
  file, and a one-time setup link in the log that makes the first administrator.
- **Configuration lives in the database** and is edited in a browser, with the environment
  pinning what the deployment owns and the admin API refusing a write the environment would
  shadow (`docs/rfcs/0016`).
- **Administration is a web interface on a public admin API** with OpenAPI, scopes, an audit
  log and an event stream: users, rooms, bridges (a catalogue of fourteen, a wizard that writes
  the bridge's own config, a runbook that turns green when it connects), federation, the
  configuration itself. 58 of 145 operations are real; the rest answer 501 rather than pretend.
- **Scale is a replica count, in design and in one experiment.** Rooms are owned by lease and
  requests for a room are forwarded to its owner over a mesh; two `hs serve` processes on one
  PostgreSQL took concurrent writes to one room without forking its history. The image is
  distroless and non-root on amd64 and arm64; readiness reflects cluster ownership and is
  withdrawn the moment a shutdown begins; a shutdown answers waiting long-polls rather than
  waiting out their timeout; the chart has probes, a PodDisruptionBudget, anti-affinity, an HPA
  and a ServiceMonitor.

What is not, and should temper any comparison:

- The cluster path has carried no real traffic beyond that experiment. Two replicas have never
  served Element, a bridge or federation together, and `/createRoom` is not shard-gated.
- The operator reconciles against a real API server and creates nothing yet.
- Federation is about 30% (`docs/next-steps.md`): a user here joins rooms elsewhere and
  messages flow between two instances of this server, but no EDUs cross, the outbound queue is
  in memory, and it has not been pointed at Synapse.
- Both conduwuit successors have far more of the client-server surface and years of real
  users; tuwunel has a Synapse-compatible admin API and this project's `/_synapse/admin`
  surface is thinner.

The bet, recorded in decision 0008, is that operations is the axis nobody else is on, and that
it is worth more to a person choosing a homeserver than another few percent of a spec table.

## Sources

- continuwuity: <https://forgejo.ellis.link/continuwuation/continuwuity>,
  <https://forgejo.ellis.link/continuwuation/continuwuity/releases>,
  <https://continuwuity.org/deploying/kubernetes>,
  <https://continuwuity.org/reference/admin/>.
- tuwunel: <https://github.com/matrix-construct/tuwunel>,
  <https://github.com/matrix-construct/tuwunel/releases>,
  `docs/deploying/kubernetes.md` and `docs/configuration.md` in that repository,
  <https://github.com/Arsolitt/tuwunel-helm>.
- Synapse, Dendrite, Conduit, Palpo: `PLAN.md` Appendix C, checked 2026-09-17.
