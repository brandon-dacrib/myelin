# 0017. The server deploys its own bridges, one per person

Status: accepted 2026-09-26; built and wired into `hs serve` the same day; run end to end against
the real binary on 2026-09-27 (an offering, an instance from `requested` to `ready`, the front
door and the manager bot from a real client, heisenbridge for real); **not yet run against a
cluster** (section 8). Owner: track 12 (platform), `crates/hs-operator`,
and track 11 (appservices), `crates/hs-bridges`. Consuming tracks: 15 (admin API,
`crates/hs-admin`), 16 (management web interface, `web/`).

Companion artifacts: `crates/hs-operator/src/{crds/bridge.rs,bridge.rs,controller.rs,deploy.rs}`,
`crates/hs-bridges` (catalogue, render, offerings, instances, the manager and front doors), the
`/bridge-deployment-target` and `/bridge-offerings*` operations in
`crates/hs-admin/openapi/openapi.yaml`, `deploy/helm/hs/templates/bridges-*.yaml`, `PLAN.md`
section 8.4.

## 1. Why

On 2026-09-26 the owner added mautrix-whatsapp to the demo at `myelin.dacrib.net` through the
wizard and found one pod in the namespace: the homeserver's. The wizard had registered the
appservice and handed over two files; nothing ran them. The Created page said "apply the Bridge
resource and the operator does the rest", and the operator's `Bridge` reconciler was a stub that
logged "no workload created". The chart did not install it.

The owner's decisions, the same day:

1. The server deploys bridges itself (a **Deploy** button), and the files stay downloadable for a
   bridge that has to run elsewhere (mautrix-imessage runs on a Mac).
2. **One bridge instance per user**, Beeper's model, rather than one shared bridge per server.
3. A user gets theirs by messaging the bridge's familiar address (`@whatsappbot:server`), which
   says it is setting one up and invites them to it when it is running, or by asking a manager bot
   (`@bridges:server`). Administrators can also create and remove instances for a user. "Magic is
   a goal."

## 2. The model

**An offering** is a bridge type an administrator has switched on for the server: WhatsApp, with
an image tag, a runtime (deployed in this cluster, or run by someone elsewhere), and who may use
it. **An instance** is one user's bridge: its own appservice registration, its own ghosts, its own
process, database and volume.

- *Per user, not shared.* Isolation: one user's crash, ban or re-link touches nobody else. Least
  privilege: an instance's registration may double-puppet only its own user (a non-exclusive claim
  on `@alice:server`, not `@.*:server`), so a compromised instance acts as one person. Scale: more
  users is more pods, spread across nodes, with no single process as the ceiling. Removal: taking
  someone off a bridge deletes their pod and volume and nothing else.
- *The cost, accepted.* Ghosts are per instance (`@whatsapp_alice_<number>`), so two users in the
  same WhatsApp group each get their own portal room and see the other as a ghost, and relay mode
  across users is gone. Beeper has the same property. Each instance is a pod and a volume
  (30 to 80 MB resident for a mautrix bridge).
- *Some types stay shared.* heisenbridge, matrix-appservice-irc and hookshot bridge a network or
  a server, not a person's account. The catalogue marks each type `per_user` or `shared`; a shared
  offering has exactly one instance, owned by nobody.
- *Never in the homeserver's pod, never more than one replica.* A sidecar would be copied onto
  every homeserver replica, and every server rollout would sign everyone out of WhatsApp. A
  mautrix bridge owns its remote connections and its SQLite database; two copies overlapping in a
  rolling update would be two clients on one session. Instances run one replica with `Recreate`
  rollouts.

## 3. Names

For user `@alice:server` and type `mautrix-whatsapp` (ghost prefix `whatsapp_`, bot `whatsappbot`):

| | |
|---|---|
| front door (the manager, per offering) | `@whatsappbot:server` |
| manager bot | `@bridges:server` |
| instance's appservice id | `whatsapp-alice` (localpart encoded, below; `-<n>` if taken) |
| instance's bot | `@whatsappbot_alice:server` |
| instance's ghosts | `@whatsapp_alice_{{.}}:server` |
| instance's exclusive namespaces | `@whatsapp_alice_.*:server`, `@whatsappbot_alice:server`, `#whatsapp_alice_.*:server` |
| double puppeting | non-exclusive `@alice:server` only |
| Kubernetes objects | `Bridge`, Deployment and Service `bridge-<8 hex of sha256(appservice id)>`, labelled with the type and the appservice id; Secret `<that>-files`; PVC `<that>-data` |
| registration `url` | `http://bridge-<hash>.<namespace>.svc:<port>` |
| bridge `permissions` | `"@alice:server": admin` and nothing else |

The localpart is encoded so that one user's namespace can never contain another's: lowercase
letters, digits, `.`, `-` and `/` stay as they are and every other byte, including `_`, becomes
`=` and two hex digits (`alice_x` is `alice=5fx`). With `_` never inside an encoded localpart, the
`_` after it is an unambiguous end, and `@whatsapp_alice_.*` cannot match `alice.x`'s or
`alice=5fx`'s ghosts.

## 4. Who does what

```
 user ──DM──▶ @whatsappbot (front door) ─┐
 admin ─web─▶ PUT .../instances/{user} ──┤
                                         ▼
                              homeserver: the manager (hs-bridges)
                    registration ─▶ appservice registry (live at once)
                    files ─────────▶ Secret bridge-<h>-files
                    run ───────────▶ Bridge bridge-<h> ──▶ operator ──▶ PVC, Deployment, Service
                    watch ◀─────── Bridge.status + the registry's ping
                    ready ─────────▶ personal bot DMs the user; front door says so
```

### 4.1 The manager (homeserver, `crates/hs-bridges`)

Runs inside the homeserver. It owns the offerings and instances tables, renders an instance's
files from the catalogue with the instance's own tokens, creates and removes registrations
through the appservice registry, and asks the runtime (below) to run or stop them. Each instance
moves through a persisted state machine, so a restart or another replica picks up where it was:

`requested → registered → deploying → starting → ready`, or `failed` (with the reason), and
`removing → (gone)`.

`starting → ready` is the deployment reporting Ready **and** the registry's ping of the instance
succeeding: the pod being up is not enough, the bridge has to have answered this server. The
manager pings an instance every few seconds while it is starting, and gives up with `failed` after
ten minutes.

At `ready` the instance's bot (the manager holds its `as_token`) creates a direct chat with the
user (`is_direct`, `m.direct` updated for the user), and sends the sign-in steps from the
catalogue. mautrix treats a room with its bot and one user as that user's management room, so the
user's first message there (`login qr`) goes straight to their bridge.

### 4.2 The front doors and the manager bot

The manager is itself an appservice (`myelin-bridges`), registered by the server for itself, whose
exclusive namespace is every enabled offering's front-door bot plus `@bridges`. Its `url` is the
homeserver's own internal route (`/_myelin/bridges/v1/transactions/{txn}` on the client listener),
and it calls the client API over loopback with its own token, so it uses exactly the paths any
bridge does (and is tested by them). Transactions reach whichever replica the Service picks; all
state is in the store.

- **Front door** (`@whatsappbot:server`). On an invite from a local user it joins. On the first
  message, or on the join if the invite was a direct chat: if the user may use the offering and
  has no instance, it says *"Setting up your WhatsApp bridge. This takes a minute or two; I'll
  invite you to it as soon as it's running."* and starts one. When the instance is ready it says
  *"Your WhatsApp bridge is ready. I've invited you to a chat with @whatsappbot_alice: accept it and
  send `login qr`."* If they already have one it says where it is (and re-invites if they left it).
  If the offering runs elsewhere (iMessage) it explains that an administrator runs this bridge for
  them and has been told. If they may not use it, it says so, politely, once.
- **Manager bot** (`@bridges:server`): `help`, `list` (offerings this user may use, and which they
  have), `start <type>`, `stop <type>` (asks for `stop <type> confirm`: it deletes their sign-ins),
  `status`.

### 4.3 The runtime

A trait in `hs-bridges` with two implementations:

- **Kubernetes** (when the chart says so, 4.5): writes the Secret and the `Bridge`, reads the
  `Bridge`'s status, deletes the `Bridge` (the operator's owner references remove the pod, Service
  and volume).
- **Elsewhere**: nothing to run; the instance goes from `registered` to `starting` and waits for
  the ping, and the administrator downloads the files for it (and the iMessage user's Mac runs it).

An offering's runtime is `cluster` (only when the server can deploy) or `elsewhere`.

### 4.4 The operator and the `Bridge` resource (`hs.matrix.org/v1alpha1`)

`hs operator`, the same image, a one-replica Deployment the chart installs, watching its own
namespace. It knows nothing about users: one `Bridge` is one bridge process.

```yaml
apiVersion: hs.matrix.org/v1alpha1
kind: Bridge
metadata: { name: bridge-1a2b3c4d, namespace: myelin, labels: {...} }
spec:
  bridgeType: mautrix-whatsapp
  appserviceId: whatsapp-alice
  image: { repository: dock.mau.dev/mautrix/whatsapp, tag: latest }
  port: 29318
  filesSecret: bridge-1a2b3c4d-files   # every key becomes a file in /data on first start
  args: []                             # heisenbridge takes its flags here
  storage: { size: 1Gi }
  resources: {}
status: { phase: Pending|Ready|Degraded, readyReplicas, observedGeneration, conditions }
```

It reconciles each `Bridge` into a PersistentVolumeClaim, a one-replica `Recreate` Deployment
and a ClusterIP Service, all owned by the `Bridge`, and writes the Deployment's state back
(`Degraded` with the reason for an image pull failure or a crash loop). An init container (the
bridge's own image) copies each file from the Secret into `/data` **only if it is not there yet**:
a mautrix bridge completes and rewrites its `config.yaml` on first start and generates secrets it
is not told (`encryption.pickle_key`); overwriting the file on every start would regenerate the
pickle key and make its crypto store unreadable. The container runs the image's own entrypoint,
has a TCP readiness probe on `port` and `/data` on the volume. SQLite in the volume is the
database.

The draft schema (`appServiceRef`, `replicas`, inline `config`) was never reconciled by anything
and is replaced; `v1alpha1` allows it.

### 4.5 How the server knows it can deploy

With `bridges.enabled` (the chart's default) the chart sets `MYELIN_BRIDGES_NAMESPACE` (the
release namespace) and `MYELIN_BRIDGES_HOMESERVER_URL` (`http://<fullname>.<ns>.svc:<port>`, how a
bridge in the cluster reaches the server) on the homeserver, and gives its service account, in
that namespace only, `bridges` (all verbs) and `secrets` (get, create, update, patch, delete). With
both variables set the server builds a Kubernetes client from its service account. Otherwise
`GET /bridge-deployment-target` says `available: false` with the reason and offerings can only run
elsewhere.

## 5. Admin API

| Operation | id | |
|---|---|---|
| `GET /bridge-deployment-target` | `bridge_deployments.target` | `{available, namespace, homeserver_url, reason}` |
| `GET /bridge-offerings` | `bridge_offerings.list` | Offerings, each with its instance counts by state. |
| `PUT /bridge-offerings/{type}` | `bridge_offerings.put` | Enable or change an offering: `{enabled, runtime, image_tag, access: {all_local_users, users[]}, options: {encryption, double_puppeting, backfill}}`. Registers the front door. A shared type creates its one instance. |
| `GET /bridge-offerings/{type}` | `bridge_offerings.get` | |
| `DELETE /bridge-offerings/{type}` | `bridge_offerings.delete` | Only with no instances left, or with `?remove_instances=true`. |
| `GET /bridge-offerings/{type}/instances` | `bridge_instances.list` | |
| `GET /bridge-offerings/{type}/instances/{user_id}` | `bridge_instances.get` | State, reason, appservice id, bot, deployment phase, ping health. `_` is the user id of a shared type's instance. |
| `PUT /bridge-offerings/{type}/instances/{user_id}` | `bridge_instances.put` | Create one for a user (what the front door does); idempotent. |
| `DELETE /bridge-offerings/{type}/instances/{user_id}` | `bridge_instances.delete` | Stop it and remove its registration, pod and volume. |
| `POST /bridge-offerings/{type}/instances/{user_id}/files` | `bridge_instances.files` | `config_yaml`, `registration_yaml`, `compose_yaml`, `manifest_yaml` (Secret + `Bridge`) with the instance's tokens, to run it elsewhere. Creates nothing. |

Reads need `bridges:read`; everything else `bridges:write` (the files carry tokens). (This said
`admin:read`/`admin:write` when accepted; the OpenAPI document, which is what the router enforces
and the interface follows, uses the bridge scopes, and the text was corrected to it on 2026-09-27.) Every write is
audited and published on the event stream. `BridgeType` gains `mode` (`per_user` or `shared`) and
`deployable`. The `appservices.*` operations are unchanged and list every instance's registration
(each tagged `io.myelin.bridge_instance`), so the registry stays the one place delivery, health and
backlog are looked at.

## 6. Affected tracks and migration

- Track 12: the chart installs the `Bridge` CRD (`crds/`), the operator and the RBAC, on by
  default; `bridges.enabled: false` removes all of it.
- Track 11: many more registrations (users × offerings). Event routing must not scan every
  namespace regex per event linearly once there are hundreds; `hs-bridges` measures it with 500
  registrations and the registry indexes by literal prefix if it has to.
- Track 15: the operations above.
- Track 16: Bridges becomes offerings (enable WhatsApp for this server) with each offering's
  instances (who has one, its state, remove, download files, create for a user). The wizard is
  "offer a bridge". A custom registration is still added through `appservices.create`.
- The demo's shared WhatsApp registration from 2026-09-25 is removed and replaced by an offering;
  its owner gets an instance by messaging `@whatsappbot`.

## 7. Not in this RFC

Reserving the ghost prefixes against local registration before an instance exists; per-user
quotas beyond "may use it"; a Docker runtime for single-node installs (instances there run
elsewhere); moving an instance between users; shared portal rooms between instances.

## 8. Where it stands (2026-09-27)

Everything sections 4 and 5 describe exists on `main` (`1c38b5e` to `52649d2`, 2026-09-26) and,
apart from the cluster runtime, has now been run:

| Piece | Where | Verified by |
|---|---|---|
| The manager, store, state machine, front doors, manager bot, Matrix client, runtime seam | `crates/hs-bridges` | 9 unit tests (6 drive the manager over the in-memory store and directory); **three tests through the real binary** (`crates/hs-cli/tests/bridge_offerings.rs`): an offering made over the admin API, an instance walked `requested → registered → starting → ready` with an axum stand-in answering the ping, the owner invited to a direct chat with the sign-in steps, `m.direct` set through double puppeting, the instance's registration delivered its room, the instance and the offering removed; `@whatsappbot` invited by a real client (the server delivers to itself over loopback), setting one up, saying where it stands, saying it is ready, refusing a user it is not open to once; `@bridges` answering `help`, `list`, `status`, `stop`, `stop confirm`, `start` |
| The ten admin operations, `BridgeType.mode`, `deployable` and `not_deployable_reason`, `BridgeOffering.image_tag`, `BridgeInstance.last_ping_at`/`last_error`, instance rendering | `crates/hs-admin` | router tests over the in-memory source; the real-binary tests above through the router |
| A `shared` offering: heisenbridge | `crates/hs-admin` (catalogue), `crates/hs-bridges` | its one instance (`_`) created by the offering's `PUT`; **heisenbridge 1.15.4 run from the rendered `registration.yaml` reached `ready`** (the same test, when `heisenbridge` is installed; a stand-in otherwise) |
| The `Bridge` CRD, the reconciler, the status, `KubeBridgeClient`, `hs operator` | `crates/hs-operator`, `crates/hs-cli` | 52 unit tests, generated CRDs checked for drift |
| The Kubernetes runtime from the chart's two variables | `crates/hs-cli/src/bridges.rs` | compiles; a half-set pair fails startup |
| The operator Deployment, RBAC, CRD and the server's variables in the chart | `deploy/helm/hs` | `helm lint`, both mode renders, `bridges.enabled=false` renders none of it |
| Offerings first in the interface, the Offer wizard, the offering page, files, registrations tab | `web/` | unit tests and Playwright against MSW mocks; **Playwright against the real binary** (`web/e2e-real/bridge-offerings.spec.ts`: offer WhatsApp elsewhere with the server's own reason quoted, the offering page, a refused non-local user, an instance row reaching Starting, its files with the server's bound address, remove, stop offering; `docs/design/screenshots/bridge-offerings-*-real.png`) |

Found and fixed on the way (2026-09-27): an offering on a server with no `public_baseurl`
rendered an empty homeserver address into the files (now the bound address); the manager only
noticed a bridge through its own half-minute ping, though a mautrix bridge pings itself through
the server on start and an administrator can press Ping (the registry's health is read first,
and an instance run elsewhere is pinged every tick for its first two minutes); a front door
repeated its refusal on every message (once per room now); the bots' `m.direct` write replaced
the account data event (merged now, and set for the owner through double puppeting); the
catalogue said heisenbridge was per user (it is a bouncer many local users share, so it is
`shared`, and a shared instance names no owner: the first local user to talk to it claims it);
the two list operations were documented as `{data}` while the router answered pages
(`{items, next_cursor, prev_cursor}`, like every other list: the document and the interface now
say so).

Not yet done, in the order it should happen: a `Bridge` applied by hand on a kind cluster and
watched to `Ready`, and a `cluster` offering driven through `deploying`; the demo's shared
WhatsApp registration replaced by an offering (section 6); the scale measurement in section 6.
Until the first of those, the cluster runtime is a design, not a capability, and `CHANGELOG.md`
says so.
