#!/usr/bin/env node
// A minimal same-origin reverse proxy used only to drive Element Web against `hs serve` for
// manual/browser-automation testing (track 16, "point Element Web at it").
//
// Why this exists: `hs serve`'s client-server API emits no `Access-Control-Allow-Origin` header
// at all (confirmed by direct OPTIONS/GET probes — see docs/status/16-management-web-interface.md).
// The spec requires the client-server API to be reachable cross-origin from any web client; this
// server does not do that yet, so a browser client hosted on a different origin/port than the
// homeserver (which is the normal case for a web client, including this test) cannot make any
// `/_matrix` request at all — the request is sent, but the browser blocks script from reading the
// response. This proxy puts Element's static assets and the homeserver's API behind one origin so
// the rest of the scenario (login, sync, rooms, messages) can actually be exercised; it is a test
// harness working around a real gap, not a fix for it.
//
// Usage: node scripts/element-proxy.mjs [port]
//   PROXY_PORT   (default 8090) - what the browser talks to
//   ELEMENT_TARGET (default http://127.0.0.1:8080) - the vectorim/element-web container
//   HS_TARGET     (default http://127.0.0.1:8098) - the running `hs serve`
//
// Routing: any path starting with /_matrix, /_synapse, or /.well-known goes to HS_TARGET;
// everything else (Element's static app) goes to ELEMENT_TARGET.

import http from "node:http";

const PROXY_PORT = Number(process.env.PROXY_PORT ?? process.argv[2] ?? 8090);
const ELEMENT_TARGET = new URL(process.env.ELEMENT_TARGET ?? "http://127.0.0.1:8080");
const HS_TARGET = new URL(process.env.HS_TARGET ?? "http://127.0.0.1:8098");

function isHomeserverPath(pathname) {
  return (
    pathname.startsWith("/_matrix") ||
    pathname.startsWith("/_synapse") ||
    pathname.startsWith("/.well-known")
  );
}

const server = http.createServer((req, res) => {
  const target = isHomeserverPath(req.url ?? "") ? HS_TARGET : ELEMENT_TARGET;
  const options = {
    hostname: target.hostname,
    port: target.port,
    path: req.url,
    method: req.method,
    headers: { ...req.headers, host: `${target.hostname}:${target.port}` },
  };
  const upstream = http.request(options, (upstreamRes) => {
    res.writeHead(upstreamRes.statusCode ?? 502, upstreamRes.headers);
    upstreamRes.pipe(res);
  });
  upstream.on("error", (err) => {
    res.writeHead(502, { "content-type": "text/plain" });
    res.end(`proxy error reaching ${target}: ${err.message}`);
  });
  req.pipe(upstream);
});

server.listen(PROXY_PORT, () => {
  console.log(`element-proxy: listening on http://127.0.0.1:${PROXY_PORT}`);
  console.log(`  /_matrix, /_synapse, /.well-known -> ${HS_TARGET}`);
  console.log(`  everything else                   -> ${ELEMENT_TARGET}`);
});
