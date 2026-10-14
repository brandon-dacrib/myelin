# Tracing: where a slow or failing request spent its time

The `hs` binary exports OpenTelemetry traces over OTLP/gRPC to a collector (an OpenTelemetry
Collector, Grafana Alloy, Tempo's or Jaeger's own OTLP receiver). Every image, release binary
and `cargo build` of `hs-cli` has the exporter compiled in (`hs-cli`'s default `otlp` feature,
since 2026-10-14); it is off until it is configured.

## Turning it on

Three settings, `telemetry.tracing.*`:

| Setting | Meaning |
|---|---|
| `enabled` | Send traces. |
| `otlp_endpoint` | The collector's OTLP **gRPC** endpoint, `http://host:4317` (or `https://`). |
| `sample_ratio` | The share of traces kept, `0.0`–`1.0` (default `0.1`). |

They are read at start-up: a change takes effect at the next restart.

**Helm** (the chart pins them through the pod's environment when `enabled` is true, so they win
over the database and take effect on the next rollout of an installed release):

```yaml
telemetry:
  tracing:
    enabled: true
    otlpEndpoint: http://otel-collector.monitoring.svc:4317
    sampleRatio: 0.1
```

The demo's values (`deploy/demo/values-bridges.yaml`) send every trace to
`http://otel-gateway-collector.monitoring-system.svc:4317`, which forwards to Tempo.

**Environment** (any deployment; the environment outranks the database):

```sh
HS__TELEMETRY__TRACING__ENABLED=true
HS__TELEMETRY__TRACING__OTLP_ENDPOINT=http://127.0.0.1:4317
HS__TELEMETRY__TRACING__SAMPLE_RATIO=0.25
```

**Admin interface or API** (`PATCH /api/v1/config/telemetry`), then a restart. A config file's
`telemetry.tracing` only seeds the database on the first start (decision 0010): on a server
that has started before, change it in the interface, or through the environment.

Traces carry `service.name = myelin`, `service.version` (the binary's version) and
`service.instance.id` (the pod's name, from `HOSTNAME`).

## What it costs, and what a collector that is down costs

- Export runs on a thread of its own. A finished span goes into a queue of at most 4,096 with a
  non-blocking send; when the queue is full (a collector down, slow or unreachable) the span is
  dropped, never waited for. Batches of up to 512 go out every 2 s, each abandoned after 5 s.
  So a request never waits on the collector: with the collector refused and with it
  black-holed, the median of 10,000 requests stayed within 0.06 ms of tracing off
  (`docs/status/12-platform-and-kubernetes.md`, 2026-10-14). While export fails the server logs
  `trace export to the OTLP collector is failing` once, then at most every five minutes with the
  count of lost spans, and `trace export works again` when it recovers.
- The sampler is parent-based over the trace id: a request that arrives with a sampled W3C
  `traceparent` is traced whatever the ratio (its caller chose), one with an unsampled
  `traceparent` is not, and anything else is kept with probability `sample_ratio`. An unsampled
  span is not recorded or exported; it still costs building the span, a few microseconds.
- Only this project's spans (targets `hs_*`) are exported. A library's would be a trace of its
  own with nothing around it; its log events still land on our span they happened in.

## What a trace shows

Every HTTP request (client, federation, admin, media, metrics) is a **root span** of kind
`server`, named `METHOD route` from the matched route template
(`PUT /_matrix/client/v3/rooms/{roomId}/send/{eventType}/{txnId}`; `{unmatched}` for a 404),
with `http.request.method`, `http.route`, `http.response.status_code`, `url.path`,
`request_id` (the `X-Request-Id` the response carries, also on every log line), and
`matrix.user_id` for a signed-in client or `matrix.origin` for a federation request. A `5xx`
marks the span failed; an `error`-level log line inside a span marks it failed too and is
attached to it as an event. A request with a `traceparent` header continues its caller's trace.

Every span also carries `busy_ns` and `idle_ns`: time spent running versus waiting. A span with
a large `idle_ns` waited (for a lock, a thread, the network); one with a large `busy_ns` worked.

Inside requests and background work:

| Span | Where | What it says |
|---|---|---|
| `auth.authenticate` | client API | The access-token lookup. |
| `federation.authenticate`, `federation.origin_key` | inbound federation | `X-Matrix` verification and the origin's key (a fetch from it or a notary when not cached). |
| `federation.inbound_slot_wait` | inbound federation | Waiting for one of the 64 inbound slots. |
| `federation.pdu` (`event_id`, `room_id`) | inbound `/send` | One PDU: its verification, its room's work and any missing events fetched. |
| `federation.edu` (`edu_type`) | inbound `/send` | One EDU's handling (device lists, typing, receipts, to-device). |
| `sync.settle`, `sync.wait_for_news`, `sync.build_batch` | `/sync` | Waiting for the session hub to catch up with what came before the request; long-polling for news (expected to be long); building the response (`joined_rooms`, `initial`). |
| `room <op>` (`room_id`) | any room operation | One operation on a room's actor, named by the function that asked (`send_event_txn`, `build_batch`, `apply_room_update`). Children `room.actor.blocking_wait` (a blocking thread) and `room.actor.lock_wait` (another operation on the same room). Only inside something already traced. |
| `room.load` (`room_id`) | first touch of a room not in memory | Children `room.load.slot_wait` (one of 8 load slots), `room.load.blocking_wait`, `room.load.from_store` (`timeline_events`, `replayed`, `state_records_fed`). A load started at boot is a trace of its own. |
| `room.load.wait` | a room another caller is loading | Waiting for that load. |
| `hub.room_update` (`room_id`, `global_seq`) | session hub, a trace per update | One room update fanned out to its members. Each step is a child span named by the step (`reading the room's members and directory entry`, `waiting for the feed writer`, `writing membership records and feed entries`, …), the same names the hub's stuck-update warning uses. |
| `federation.transaction` (`destination`, `txn_id`, `pdus`, `edus`, `outcome`) | federation sender, a trace per attempt | One attempt at an outbound transaction. |
| `federation.request` (`destination`, status) | every outbound federation call | Kind `client`, named by endpoint (`PUT /_matrix/federation/v1/send`). Children `federation.request.slot_wait` (the per-destination limit), `federation.resolve` (well-known, SRV, DNS) and `federation.http` (the exchange). |
| `kv <Type::function>` (`attempts`, `mutations`, `keyspaces`) | every store transaction in a recorded trace | One `hs_kv::transact`, named after the code that asked (`kv RoomActor::persist_with`, `kv TablesAuthStore::upsert_device`): how many tries it took (conflicts retry), how many puts and deletes, which keyspaces. Child `kv.commit` (on PostgreSQL, where the writes are sent). Failed when the transaction failed. |
| `<name>` with child `blocking_wait` | work moved to a blocking thread (`hs_telemetry::spans::spawn_blocking`) | `federation.store <op>`, `federation.destination_store`, `appservice.store`, `announcer.*`, `federation.device_list_answer`, ...: the wait for a thread (`blocking_wait`) apart from the work. |
| `room.build_and_authorize` | a local event | Building, hashing and signing the event and checking it against the auth rules. |
| `room.event_auth` (`event_id`, `outcome`) | a remote event | The auth rules against its `auth_events` and the state before it: `allowed`, `refused`, `failed`. |
| `room.state_resolution` (`forks`) | an event whose state is more than one fork | State resolution proper (one fork is a lookup, not traced). |
| `room.persist` (`event_id`, `kind`) | every event written | The event's transaction and the in-memory indexes; the store transaction is its child. |
| `federation.key_fetch` (`server_name`, `outcome`), `federation.notary_query` (`notary`, `keys`, `outcome`) | a server key not cached | The origin's `/_matrix/key/v2/server`, or a notary's `/_matrix/key/v2/query`, that a request waited for. |
| `federation.user_devices` (`cached`, `devices`) | `GET /_matrix/federation/v1/user/devices/{userId}` | The answer, and whether it was the one kept for the list as it stands. |
| `appservice.query` | a user or alias query to a bridge | Kind `client`, named by endpoint (`GET /_matrix/app/v1/users`). |
| `mesh.forward` (`shard`, `route`, `status`) | a request this replica sends to a room's owner | Kind `client`; its retries and waits. The owner's `mesh.handle` and request span are its children (see "Across replicas"). |

### Background work

Each pass of a background loop is a trace of its own (a root span), made only when the pass has
something to do. A pass caused by a request **links** to it (Tempo and Jaeger show a link,
"follows from", to the other trace), so a slow delivery can be followed back to the send:

| Root span | What it is | Links to |
|---|---|---|
| `federation.transaction` (`destination`, `txn_id`, `pdus`, `edus`, `outcome`) | one attempt at an outbound transaction | the requests that wrote its PDUs (a client's send, an inbound PDU), the last 1,024 PDUs' |
| `federation.device_list_announce` (`from`, `to`, `stream`, `shards`, `users`) | one step of the device-list announcer; a child `federation.device_list_announce_user` (`user_id`, `edus`, `destinations`) per user, with the store hops `announcer.read_records`, `announcer.enqueue_edu`, `announcer.store_record`, `announcer.store_positions` | |
| `hub.room_update` | one room update fanned out to its members (stage 1) | the request that wrote the event |
| `appservice.transaction` (`appservice`, `txn_id`, `entries`, `events`, `ephemeral`, `to_device`, `outcome`) | one delivery attempt to a bridge: the user queries made first, `PUT /_matrix/app/v1/transactions` (kind `client`), the queue's bookkeeping. Failed when the bridge did not take it | |
| `appservice.pump_room` (`room_id`, `listeners`, `appservices`) | queueing a room's new events for the bridges (only with a bridge registered) | the request that wrote the event |
| `appservice.ephemeral_pass` (`typing_rooms`, `receipts`, `presence`, `to_device`, `appservices`) | queueing typing, receipts, presence, to-device and device lists for bridges; the ticks that find nothing are not traced | the requests that rang its doorbell (typing, receipts) |
| `push.notify` (`app_id`, `outcome`) | one notification to a push gateway, each attempt a `push.http` child | |
| `admin.task` (`action`, `task_id`) | an admin task's work | the request that started it |
| `media.retention_pass` (`selected`, `deleted`, `failed`) | the hourly remote-media retention pass | |
| `federation.destination_sweep`, `statistics.sample`, `search.catch_up` (`why`, `rooms`, `events`) | the destination sweep, the quarter-hourly statistics sample, a search-index catch-up with rooms behind | |
| `room.load` | a room load started at boot (stage 1) | |

### Across replicas

A request a replica forwards to the room's owner (the shard gate) is **one trace**: the
forwarding replica's request span (named `METHOD {forwarded to the room's owner}`), its
`mesh.forward`, the owner's `mesh.handle` (kind `server`, continuing the forwarder's
`traceparent`) and the owner's own request span named by its route, all with the same
`request_id`. A peer message sent from inside a trace (`mesh.peer` on the receiver) joins it the
same way. A client's own `traceparent` makes the forwarding replica's request span its child,
and the owner sees this replica's, not the client's.

### Who called

A client request records `matrix.user_id`; a federation request `matrix.origin`; an admin API
request `admin.principal` (a user id, a client's or a service account's id) and
`admin.principal_kind`, and `matrix.user_id` for a user.

### What the small spans cost

Store transactions, blocking hops, state resolution, event auth, persistence, key fetches and
the other small spans above are made only when the trace is **sampled**
(`hs_telemetry::current_is_sampled`): an unsampled request still has its request span (the log
uses it) but nothing under it is built. The check costs 80-200 ns; a span costs about 1 µs
(`hs-telemetry`'s `otlp::tests::cost_of_a_guarded_span`, release build). With the collector up,
10,000 sequential `whoami` on a debug build: tracing off p50 0.202 ms / p99 0.257 ms, ratio 0
0.226 / 0.289, ratio 1 0.230 / 0.304 (2026-10-14, `docs/status/12-platform-and-kubernetes.md`).

### Export health on `/metrics`

| Metric | Meaning |
|---|---|
| `hs_telemetry_spans_exported_total` | Spans the collector accepted. |
| `hs_telemetry_spans_dropped_total` | Spans finished while 4,096 already waited (a collector down, slow or unreachable): never queued. |
| `hs_telemetry_spans_failed_total` | Spans in batches the collector refused or that timed out. |
| `hs_telemetry_span_export_queue` | Spans waiting for export. |

`dropped` or `failed` rising while `exported` does not is a collector that is not taking spans
(alert on `rate(hs_telemetry_spans_dropped_total[10m]) > 0`). They exist at zero when tracing is
off.

## Finding things

In Grafana's Tempo data source (or Tempo's API):

- slow requests: `{ resource.service.name = "myelin" && kind = server && duration > 2s }`;
- one request from a log line: `{ span.request_id = "<X-Request-Id>" }`;
- a user's requests: `{ span.matrix.user_id = "@alice:example.org" }`;
- a stuck or slow room load: `{ name = "room.load" && duration > 5s }`;
- a slow hub update, by step: `{ name = "hub.room_update" && duration > 1s }`, then its children;
- a destination that keeps failing: `{ name = "federation.transaction" && span.destination = "example.org" }`;
- a slow store transaction: `{ name =~ "kv .*" && duration > 100ms }`, or one that retried:
  `{ name =~ "kv .*" && span.attempts > 1 }`;
- a bridge that is slow to take transactions: `{ name = "appservice.transaction" && span.appservice = "<id>" && duration > 1s }`;
- what a client's message did after the request answered: open its trace, note the trace id,
  then `{ name = "federation.transaction" }` and look for the link to it (Tempo's "Links" tab);
- a forwarded request: `{ name = "mesh.handle" }`, or the request id on either replica;
- who did that through the admin API: `{ span.admin.principal = "<id>" }`.

## Writing spans

The rules are decision 0048's. In short: name spans `<subsystem>.<operation>`; a span around a
small, frequent operation is made only when `hs_telemetry::current_is_sampled()`; a unit of
background work is a root span (`parent: None`) made only when there is work, linked to what
caused it with `hs_telemetry::TraceLink` (capture `TraceLink::current()` where the work is
queued, `add_to` the pass's span); outbound HTTP runs in `hs_telemetry::outside_spans` (the test
`crates/hs-telemetry/tests/outbound_http_outside_spans.rs` fails on a `reqwest` send or `hyper`
handshake without it); a blocking hop is `hs_telemetry::spans::spawn_blocking`; a call to
another replica sends `hs_telemetry::current_traceparent()` and the receiver calls
`hs_telemetry::continue_trace`. To test a span, turn on `hs-telemetry`'s `testing` feature as a
dev-dependency and run the code in `hs_telemetry::testing::trace` (or read a background pass's
trace with `traces_rooted_at`). A span that closes after the code returns (a background
task's, another replica's) is waited for with a deadline, never slept or yielded for:
`testing::trace_until` for the test's own trace, `testing::wait_for_trace_rooted_at` for a
background pass's. The recorder rebuilds `tracing`'s callsite interest cache when recording
starts, because a callsite first reached by another test while the recorder was being
installed could otherwise stay disabled for the rest of the process (it did:
`crates/hs-telemetry/tests/testing_install_race.rs`).

On the cluster, Tempo's search API answers through the API server's service proxy:
`/api/v1/namespaces/monitoring-system/services/tempo:3200/proxy/api/search?tags=service.name%3Dmyelin`.

## Trying it locally

```sh
docker run -d --name jaeger -p 4317:4317 -p 16686:16686 mirror.gcr.io/jaegertracing/all-in-one:1.62.0
HS__TELEMETRY__TRACING__ENABLED=true HS__TELEMETRY__TRACING__OTLP_ENDPOINT=http://127.0.0.1:4317 \
  HS__TELEMETRY__TRACING__SAMPLE_RATIO=1.0 hs serve --data-dir ./data --server-name example.test
# traces: http://127.0.0.1:16686, service "myelin"
```
