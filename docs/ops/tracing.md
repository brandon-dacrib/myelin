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

## Finding things

In Grafana's Tempo data source (or Tempo's API):

- slow requests: `{ resource.service.name = "myelin" && kind = server && duration > 2s }`;
- one request from a log line: `{ span.request_id = "<X-Request-Id>" }`;
- a user's requests: `{ span.matrix.user_id = "@alice:example.org" }`;
- a stuck or slow room load: `{ name = "room.load" && duration > 5s }`;
- a slow hub update, by step: `{ name = "hub.room_update" && duration > 1s }`, then its children;
- a destination that keeps failing: `{ name = "federation.transaction" && span.destination = "example.org" }`.

On the cluster, Tempo's search API answers through the API server's service proxy:
`/api/v1/namespaces/monitoring-system/services/tempo:3200/proxy/api/search?tags=service.name%3Dmyelin`.

## Trying it locally

```sh
docker run -d --name jaeger -p 4317:4317 -p 16686:16686 mirror.gcr.io/jaegertracing/all-in-one:1.62.0
HS__TELEMETRY__TRACING__ENABLED=true HS__TELEMETRY__TRACING__OTLP_ENDPOINT=http://127.0.0.1:4317 \
  HS__TELEMETRY__TRACING__SAMPLE_RATIO=1.0 hs serve --data-dir ./data --server-name example.test
# traces: http://127.0.0.1:16686, service "myelin"
```
