# 0040: The server `.well-known` document is derived from the public base URL (2026-10-10)

Status: accepted (track 12; touches `crates/hs-cli/src/well_known.rs`, `hs-config`'s
`server.well_known_server` and the chart). Number taken against `origin/main` on 2026-10-10; the
coordinator renumbers on collision.

## Context

`GET /.well-known/matrix/server` was served only when `server.well_known_server` named a
`host[:port]`, on the reasoning (the module's own header, since 2026-09-2x) that a document
naming the server name itself is indistinguishable from no document, so serving one by default
adds a failure mode and no capability. Synapse's `serve_server_wellknown: false` default was
cited.

That reasoning assumes the server name itself answers on 8448. Ours never did: the demo
(`myelin.dacrib.net`) sets `server.public_baseurl` and publishes the client document, its
Ingress routes `/.well-known/matrix` on 443, and 8448 is closed. On 2026-10-10 the roll to
`87d57288` found that every signed request the demo made was answered
`401 Failed to find any key to satisfy ... ed25519:a_JBQV7r` (ten remote-media fetches in an
hour from maunium.net): no remote server could resolve `myelin.dacrib.net` to anything that
would hand over its signing key. Every federation number in the README came from servers that
could reach each other; the demo itself had never been reachable. The operator had set the one
setting that says where the server is (`public_baseurl`), and the server had the information
to publish the document, and chose not to.

The owner's instruction: "enable the .well-known/matrix setting by default".

## Decision

When `server.well_known_server` is unset and `server.public_baseurl` is an `https://` URL, the
server publishes `/.well-known/matrix/server` as `{"m.server": "<host>:<port>"}` with the base
URL's host and its explicit port or 443: `https://myelin.dacrib.net` publishes
`myelin.dacrib.net:443`. An operator who has said where this server is reachable over TLS has
said where federation is reachable too; a server nobody can discover is not federating.

- `server.well_known_server` set to a `host[:port]` wins: federation reached at a different
  host or port than clients use (`matrix.example.org:8448` while clients use
  `https://example.org`).
- `server.well_known_server` set to the empty string publishes no document (a reverse proxy
  that serves its own, or a deliberate choice). The empty string, rather than a `false`, because
  the field is `Option<String>` and the generic Configuration page renders a string; validation
  accepted nothing but a `host[:port]` before and now accepts the empty string as this value.
  Unset and the empty string are different values: unset derives, empty turns off.
- An `http://` base URL derives nothing: federation needs TLS, and a document naming an
  `http://` host would send remote servers to a port that cannot answer them. The client
  document is still published from it (a local or port-forwarded install).
- `federation.enabled: false` derives nothing: there is no federation to point at. An explicit
  `well_known_server` is still published, since the operator asked for exactly that document.
- The derived value is recomputed with every change to the `server` section, like the client
  document; `public_baseurl` and `well_known_server` are hot settings.
- The boot line `publishing .well-known discovery documents` carries `server_source`: "derived
  from server.public_baseurl", "set (server.well_known_server)", or why nothing is published
  and what to set; the reload line `the server settings are now in force` carries the same.

The client document's rule is unchanged (`public_baseurl` set, any scheme), as is the support
document's. A server with no `public_baseurl` still publishes nothing, for the original reason:
there is nothing to derive from but the server name, and that document would say nothing.

## Consequences

- The demo publishes `m.server: myelin.dacrib.net:443` on its next roll with no value change;
  the 401s stop once remote servers' negative caches expire.
- The chart's `publicBaseUrl` is enough for a federating install; `ci/install-smoke.sh` checks
  the derived document on every image CD builds (set through the admin API after setup, since
  the smoke installs without `publicBaseUrl` to check the port-forwarded setup link).
- A Synapse configuration translated with `public_baseurl` set and `serve_server_wellknown`
  unset or `false` (Synapse's default) publishes a server document after migration where
  Synapse did not. For a Synapse deployment that was federating, a reverse proxy already serves
  one and answers first; for one that was not, the document now makes it reachable, which is
  the intent. `docs/compat/synapse-config-table.md` says so on the row.
- The Configuration page's text control turns an emptied field into `null` on blur, so the
  "empty string" value cannot be typed there today; it can be set through the admin API, a
  file or `HS__SERVER__WELL_KNOWN_SERVER='""'`. Track 16's to fix if the page should offer it.
