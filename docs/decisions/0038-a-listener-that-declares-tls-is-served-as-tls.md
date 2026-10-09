# 0038: A listener that declares TLS is served as TLS (2026-10-09)

Status: accepted (track 12; touches `hs serve` in `hs-cli`, which every track's real-binary
tests start). Number taken against `origin/main` on 2026-10-09; the coordinator renumbers on
collision.

## Context

`hs_config::listeners::TlsConfig` (`tls: {certificate_path, private_key_path}` on a listener)
has existed since the configuration schema was written, and `hs serve` logged "listener
declares TLS but hs serve does not terminate TLS yet; serving plaintext" and served plaintext
on it. Every deployment therefore had something in front: the chart's Ingress, the demo's
Traefik, the Myelin<->Synapse interop harness's nginx (`tests/federation-synapse/run.sh`,
whose header says why). A small host serving federation on 8448 with nothing in front, the
PLAN's small-ARM target, could not.

## Decision

A listener with `tls:` is served as HTTPS by the server itself (`crates/hs-cli/src/tls_listener.rs`):

- The PEM chain and key are read once at start. A missing file, an empty one or a key that
  does not match the certificate stops the start with `ServeError::ListenerTls`, naming the
  listener and the file, exactly as a port that cannot be bound does. There is no warning-and-
  plaintext path any more: a configuration that asks for TLS gets TLS or nothing.
- The listener offers `h2` and `http/1.1` through ALPN; hyper's automatic server behind
  `axum::serve` speaks both.
- Handshakes run on their own tasks (`TlsListener`, an `axum::serve::Listener`): a client that
  stalls in its handshake, or speaks plaintext to the port, costs the next client nothing.
- Handlers see the client's address through `ConnectInfo<SocketAddr>` as behind a plaintext
  listener (`WithConnectInfo`), so per-address rate limits and `x_forwarded` work the same.
- Rotation is a restart, as it is for the mesh's certificate (`hs_cluster::mesh::tls`): the
  files are not watched. A deployment that wants zero-restart renewal keeps its proxy.

## Consequences

- `tests/federation-synapse/run.sh` can drop its nginx and point Synapse at the `hs` listener
  directly (track 06/14's file; not changed here).
- The chart is unchanged: in Kubernetes the Ingress or Gateway terminates TLS, and the pod
  listens in plaintext behind it. Nothing in `values.yaml` renders a `tls:` block.
- `crates/hs-cli/tests/tls_listener.rs` is the proof against the real binary: HTTPS answers
  with a private CA, ALPN negotiates `h2`, an untrusted CA and plaintext are refused without
  disturbing the next request, the plaintext listener beside it still answers, eight
  handshakes at once are served, and a mismatched key stops the start naming the listener.
- `rustls`, `tokio-rustls`, `rustls-pki-types` move from `hs-cli`'s dev-dependencies to its
  dependencies, and `rustls-pemfile` (already a workspace dependency) joins them; `tower`
  gains its `util` feature in `hs-cli`. No new crate in the workspace.
