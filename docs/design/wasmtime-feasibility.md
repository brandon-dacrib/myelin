# `wasmtime` component-model feasibility: verdict

Date: 2026-09-18. Owner: track 15. Status: feasibility spike (design-only; no code). See `docs/workstreams/15-admin-api-and-modules.md` ("day-one work: ... a `wasmtime` component-model feasibility spike") and `crates/hs-modules`.

## 0. Why this is a document and not a branch

`wasmtime` is a large dependency: Cranelift (a full code generator), a C-API surface, and a build that compiles a meaningful amount of Rust and, on some configurations, invokes a C toolchain for its runtime support libraries. This machine is shared across sixteen concurrent tracks on ten cores and 16 GB of RAM (`docs/workstreams/README.md`, `.claude/agents/hs-15-admin.md` rule 4). Adding `wasmtime` to `[workspace.dependencies]` would force every other track's `cargo check` to build it too (workspace dependency graphs are shared; see `docs/decisions/0002-workspace-conventions.md`: "one shared `target/` directory"), which is a bad trade for a Phase 1/2 feature (`docs/workstreams/15-admin-api-and-modules.md`: "the WebAssembly host" is listed under "Phase 1 and 2 deliverables", not day-one work). This document does the feasibility analysis the spike calls for without paying that cost now; the verdict is **feasible, and the design below is buildable when Phase 1 starts**, not "we tried it and it broke."

## 1. What problem this solves

`crates/hs-modules`'s `ModuleHooks` trait (`crates/hs-modules/src/hooks.rs`) and its HTTP-callback implementation (`crates/hs-modules/src/client.rs`) cover the case where a module runs as a separate process reachable over HTTP. That is enough for anything that can tolerate network latency on the hot path (a few milliseconds per `check_event_for_spam` call) and that an operator is willing to deploy and keep alive as its own service. It is not enough for:

- A module an operator wants to *drop in* (a single `.wasm` file) without running a second process.
- A module that needs to run inside the request path of every event with sub-millisecond overhead (Synapse's in-process Python modules have this property; the HTTP-callback protocol does not).
- Sandboxing an untrusted third-party module's CPU and memory use, which an out-of-process HTTP module already gets "for free" from the OS process boundary, but which an in-process option needs some other mechanism for.

WebAssembly with the component model is the candidate for "in-process, sandboxed, drop-in". This document assesses whether `wasmtime` specifically is the right way to get there.

## 2. What would be built

### 2.1 The interface (WIT)

The component model's contract language is WIT (WebAssembly Interface Types). `ModuleHooks`'s eleven categories map onto a WIT `world` with one `import` (host functions the module calls back into, e.g. logging, a key-value scratch store) and one `export` per hook category the module implements:

```wit
package hs:modules@0.1.0;

interface types {
  record event-for-check { event-id: string, room-id: string, sender: string, event-type: string, state-key: option<string>, content-json: string }
  variant check-result { allow, deny(deny-reason) }
  record deny-reason { errcode: option<string>, reason: option<string> }
  // ... the rest of hooks.rs's types, JSON-encoded at the boundary where a type is open-ended
  // (event content) and as native WIT records where it is not (CheckResult, AuthResult, ...).
}

interface spam-checker {
  use types.{event-for-check, check-result};
  check-event-for-spam: func(event: event-for-check) -> check-result;
  user-may-invite: func(inviter: string, invitee: string, room-id: string) -> check-result;
}

// ... one interface per category, mirroring hooks.rs's eleven groups exactly, so the WIT world
// and the Rust trait stay a single source of truth in practice (see section 4).

world hs-module {
  import host: interface { log: func(level: string, message: string); }
  export spam-checker;
  export third-party-rules;
  export presence-router;
  // ... a component implements whichever interfaces it wants; unimplemented ones are simply not
  // exported, which `wasmtime`'s component linking surfaces as "this component has no export
  // named X" at load time, not a runtime error per call (an improvement on the HTTP protocol's
  // per-call 404, section 3 of `crates/hs-modules/src/callback.rs`).
}
```

### 2.2 The host

`hs-modules` would gain a `wasm` module (feature-gated, `cfg(feature = "wasm")`, off by default) with:

- `WasmModuleHost`: loads a `.wasm` component with `wasmtime::component::Component::from_file`, instantiates it with a `wasmtime::Store` per module instance (not per call — instantiation is not free; see section 3), and implements `ModuleHooks` by calling the generated bindings (`wasmtime::component::bindgen!` from the WIT above).
- Resource limits per `Store`: `wasmtime::ResourceLimiter` capping linear memory (default: 64 MiB) and `wasmtime::Store::epoch_deadline_trap` or fuel metering (`Config::consume_fuel`) capping CPU per call, so a misbehaving module traps instead of hanging the request that triggered it.
- No WASI beyond what a spam/rule checker plausibly needs (no filesystem, no network): the whole point is a sandboxed drop-in, so the host exposes only the `host` import above (logging) and nothing else. This is stricter than Synapse's Python modules, which run with full process privileges; that is a feature, not a gap.

### 2.3 Fit with the existing trait

`WasmModuleHost` implements `hs_modules::hooks::ModuleHooks` exactly like `HttpCallbackClient` does, so `ModuleChain` (`crates/hs-modules/src/noop.rs`) composes WASM and HTTP-callback modules interchangeably without any other track's code caring which kind a given hook is backed by. No changes are needed to `hooks.rs`, `noop.rs`, or any consumer of `ModuleHooks` to add this later; that was a design goal, not an accident (the trait was written with an in-process option in mind from the start).

## 3. Risks and open questions, ranked by how much they could change the design above

1. **Per-call latency of the sync bridge.** `wasmtime`'s host-to-guest calls are synchronous; `ModuleHooks`'s methods are `async fn`. The host wraps each call in `tokio::task::spawn_blocking` (Cranelift-compiled WASM execution is CPU-bound, not I/O-bound, so it belongs on the blocking pool, not the async executor). This is a known, common pattern (`wasmtime`'s own embedding examples do this) and is not expected to be a blocker, but the actual overhead (spawn_blocking's context switch, `Store` locking if instances are shared) needs measuring, not assuming, once real code exists.
2. **Instance lifecycle.** A `wasmtime::Store` is not `Send + Sync` shareable across concurrent calls; either the host pools one instance per worker thread (more memory, no lock contention) or serializes calls through one instance behind a mutex (less memory, a bottleneck under load). Which one is right depends on how many modules a real deployment runs and how hot the hottest hook (`check_event_for_spam`, on every locally-created event) actually is. This is a capacity-planning question that needs a benchmark, not a guess.
3. **Build cost on contributors' and CI machines**, not just this shared spike machine: `wasmtime` plus Cranelift adds real minutes to a clean build. Feature-gating it (`cfg(feature = "wasm")`, not compiled by default, not in `[workspace.dependencies]` until a track actually needs it at build time) defers this cost to whoever opts in, which is the right default for an optional capability.
4. **WIT/Rust drift.** The WIT world in section 2.1 and the `ModuleHooks` trait in `hooks.rs` need to describe the same eleven categories without a generator keeping them in sync (unlike `openapi/operations.json` and `openapi/openapi.yaml`, which this track's OpenAPI generator keeps honest by construction, section 15 of `docs/rfcs/0004-admin-api.md`'s spirit). A contract test comparing the WIT world's export names against `ModuleHooks`'s method names (a simple string-set diff, no `wasmtime` dependency needed to write it) is cheap insurance and should exist before the host does.
5. **Versioning a `.wasm` module across `hs-modules` releases.** WIT's own package versioning (`hs:modules@0.1.0` above) gives a natural answer (additive changes bump the minor version; breaking ones bump the major and old modules simply fail to link, a clear failure mode), but the operational story (how does an operator know their module needs rebuilding against a new `hs-modules` WIT version) is unresolved and should be designed alongside 13's compatibility and deprecation conventions (RFC 0004 section 3.8's `Deprecation`/`Sunset` pattern is a reasonable model to reuse).

None of these are "this doesn't work"; they are "this needs measurement and a prototype," which is exactly what a feasibility spike is supposed to conclude.

## 4. Verdict

**Feasible.** The component model's WIT interfaces map cleanly onto `ModuleHooks`'s existing shape (section 2.1), `wasmtime`'s resource-limiting primitives (fuel, memory limits, epoch deadlines) give the sandboxing a drop-in module needs that an HTTP-callback module gets from the OS instead, and the trait was already designed so a WASM-backed implementation slots in next to the HTTP one without disturbing any consumer. The open questions in section 3 are normal engineering unknowns for a from-scratch host, not red flags.

**Recommendation: build it in Phase 1, behind a `wasm` cargo feature, starting from the WIT world in section 2.1 and a benchmark answering risk 1 and 2 before committing to an instance-pooling strategy.** Do not add `wasmtime` to `[workspace.dependencies]` until that work starts, so the other fifteen tracks' builds are unaffected until this is real.

## 5. What Phase 1 needs to do differently from this document

This document is a design, not a decision record binding future implementers to every detail above (the WIT field names, the resource limit defaults, the pooling choice) — those are starting points for the person who actually writes `crates/hs-modules/src/wasm.rs`, to be revised once real code and a real benchmark exist. What should not change without an RFC: that `WasmModuleHost` implements the same `ModuleHooks` trait as `HttpCallbackClient`, and that adding it does not require changing `hooks.rs`.
