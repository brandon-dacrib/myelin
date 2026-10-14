# 0048. Traces are exported, and how a span is written so that its trace is useful

Status: accepted, 2026-10-14. Owner: track 12. Extends decision 0004 ("Span naming"); every
track that opens spans is a consumer.

## Context

`hs` was built without `hs-telemetry`'s `otlp` feature, so `telemetry.tracing` did nothing,
and the binary had two spans in all (`request`, and Ruma's push-rule `flatten_value`). After the
outages of 2026-10-13/14 (a `/metrics` scrape starving the runtime, the session hub wedged on
one room, the federation boot freeze) the owner asked for traces that say where a slow or
failing request spent its time. Turning export on showed three ways a span can make its trace
useless; the rules below are what avoids them.

## Decision

1. **The shipped binary exports traces.** `hs-cli` has a default feature `otlp` that turns on
   `hs-telemetry/otlp`; the image, CD binaries and releases all have it. Export is still off
   until `telemetry.tracing.enabled`. `service.name` is `myelin`.
2. **The request span is the root.** `hs_telemetry::RequestIdLayer` opens `request` with
   `parent: None` (or the caller's W3C `traceparent`), kind `server`, named `METHOD route` once
   the router knows the route (`hs_telemetry::record_route`). Fields further in are recorded on
   it with `tracing::Span::current().record(..)` while it is current, and must be declared in
   `request_id.rs` first (`hs_telemetry::request_id::fields`): `matrix.user_id` (the client
   API's authentication records it), `matrix.origin` (the `X-Matrix` layer).
3. **Only `hs_*` spans are exported.** A library's span (Ruma's, `hyper`'s, `tonic`'s) is not;
   its events still land on our span. A span must therefore be opened from a `hs-*` crate to be
   seen.
4. **No orphan leaf spans.** A span for a small, frequent operation (a room actor operation,
   `room <op>`) is opened only when something is already traced (`!Span::current().is_none()`);
   otherwise the search indexer and the pumps make thousands of one-span traces. Work that is a
   unit of its own (a session hub update, an outbound transaction attempt, a room load) opens a
   root span (`parent: None`) deliberately.
5. **A span around a long wait is a span of its own.** Waiting for a blocking thread, a lock, a
   slot or another caller's load is a child span (`*.blocking_wait`, `*.lock_wait`,
   `*.slot_wait`, `room.load.wait`), so a trace separates waiting from working. `busy_ns` and
   `idle_ns` on every span say the same within a span.
6. **Outbound HTTP runs outside every span: `hs_telemetry::outside_spans(future)`.**
   `kube-client` turns on `hyper-util`'s `tracing` feature for the whole binary, and with it
   every new connection's task is spawned `in_current_span()`: the task, and with it the
   caller's span and every span above it, lives as long as the pooled connection. A request
   that opened a connection was never exported (seen: an inbound `/send` whose key lookup asked
   a notary). `outside_spans` runs the future in a root span of its own that is never exported
   (target `hs_telemetry::detached`), which the connection task holds instead; log lines inside
   still reach the log. Time an exchange with a span held around the call, not entered in it.
7. **Errors.** A failed unit records `otel.status_code = "error"` and `error.message` (not
   `error`, which Jaeger and others treat as a boolean). A `5xx` marks the request span failed;
   an `error!` event inside a span marks it failed too. A `4xx` is not a failed span.
8. **Names and attributes from outside are bounded.** A span's name never carries a value a
   remote chose (an EDU's type is one of the spec's or `other`; a federation request is named
   by its endpoint, not its path). No span carries a message body, a token or a key.

## Consequences

- Done in the first stage (branch `agent/otlp-traces`): the feature, the chart, the demo, the
  request root, auth, `/sync`, the room actor, room loads, the session hub, inbound `/send`
  (per PDU and EDU), the inbound slot wait, outbound transactions and federation requests.
- Left for the next stage (`docs/next-steps.md`): `outside_spans` on the other outbound HTTP
  call sites (appservice transactions and queries, push gateways, URL previews, media scanning,
  the identity server, modules); KV and `spawn_blocking` spans; state resolution, event auth and
  persistence; the device-list announcer; appservice and background-loop passes; links from a
  background pass to the request that caused it.
