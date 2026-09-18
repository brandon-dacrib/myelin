# 0007. Use existing projects wherever it makes sense; build as little as is reasonable

Status: accepted, 2026-09-18. Author: integration lead, at the user's direction. Applies to every track.

This governs the other decisions. Where a maintained project already does a job well, we use it and contribute upstream rather than reimplementing it. We build only what is genuinely ours to build: the homeserver's own behaviour.

## The test a track applies before writing something

1. Does a maintained implementation exist with a compatible licence? If so, use it. Apache-2.0 and MIT are freely usable; AGPL-3.0 is a behavioural reference only, never a code source, while this project is Apache-2.0 (decision 0001).
2. If it exists but is missing something we need, is contributing that upstream cheaper than owning a reimplementation forever? Usually yes, and the answer is a pull request, not a fork.
3. If it exists but is unmaintained or unsuitable, say so explicitly in the track's status file, with the reason, so the judgement can be revisited.
4. Build only what is left.

"Reasonable" carries weight in both directions. A dependency that is abandoned, unlicensed for our use, or so thin that vendoring it costs less than tracking it is not a saving. Nor is adopting a heavy framework to avoid writing forty lines.

## What we already reuse, and keep reusing

Ruma for Matrix types, endpoints, signatures and state resolution v2 and v2.1. Fjall for the embedded store. PostgreSQL with CloudNativePG for the clustered store. `object_store` for media. axum, hyper, tokio and rustls for the HTTP and TLS stack. `tantivy` for search. the `image` crate for thumbnails. `hickory-resolver` for federation discovery. `minijinja` for templates, chosen because it is Jinja2-compatible so operators' existing Synapse email templates keep working. `kube-rs` for the operator. Complement, Sytest and `matrix-rust-sdk` for testing. mautrix for bridges, which we integrate with and never reimplement.

## Decisions this principle forces today

### Dropped: the push gateway component

Sygnal is the reference Matrix push gateway, Apache-2.0, maintained by the Matrix.org Foundation, and already deployed by operators who need push. Our push gateway would be a second implementation of a solved problem with no advantage.

`hs-pushgw` is removed. The homeserver keeps its *pusher* side, which is genuinely ours: evaluating push rules, maintaining notification counts and posting to a gateway. It posts to Sygnal, or to any gateway speaking the Push Gateway API. Documentation points operators at Sygnal.

### Dropped: the identity service component

Sydent is the reference identity server. The Matrix.org Foundation has stated it cannot resource its maintenance, Element is forking it under AGPL-3.0, and the Foundation's copy is being archived. The v1 identity API is already deprecated in favour of v2. In practice most homeservers use the matrix.org instance or no identity server at all.

Shipping our own would mean taking on an unloved component in a declining area of the protocol. `hs-identity` is removed. We keep the *client* side, binding and unbinding third-party identifiers against whatever identity server an operator configures, which lives in `hs-auth` and is required by the client-server specification.

### Narrowed hard: content scanning is one ICAP client and nothing else

c-icap is a mature ICAP server whose `virus_scan` service drives ClamAV, with packaged container images. It owns talking to engines; we own talking to it. So the scanning feature ships **one ICAP client**, plus an HTTP provider for cloud APIs that have no ICAP fronting, and nothing else. No clamd client, no command runner, no per-engine adapters: each would be a second path to a problem c-icap already solves, carrying its own maintenance, tests and failure modes forever.

`icap-rs`, a tokio-based Rust ICAP/1.0 library following RFC 3507, is evaluated before any protocol code is written. Use it if it covers `docs/rfcs/0008-content-scanning.md` section 3.3; contribute the gaps upstream if it is close; write our own only with a recorded reason.

The reference deployment, c-icap plus ClamAV beside the homeserver, ships as chart and compose configuration, so enabling virus scanning is a configuration change rather than an integration project.

### Confirmed: things we do still build

- The homeserver itself, which is the point.
- The management interface and its admin API, because they are built on our own API and the user asked for them specifically.
- The native OAuth 2.0 issuer, because the alternative is making every deployment run a second service; but its grant mechanics lean on established crates rather than being written from first principles, and delegation to Matrix Authentication Service stays a first-class supported mode for operators who already run it.
- The storage abstraction and the state representation, because no existing library models Matrix state the way the protocol needs.

## Standing obligation

Every track records in its status file, under a "Reuse considered" heading, anything substantial it built rather than adopted, and why. The integration lead treats a missing or unconvincing entry as a review finding.
