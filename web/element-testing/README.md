# Pointing Element Web at `hs serve`

Reproducible setup for track 16's "point a real Matrix client at this server" session
(`docs/next-steps.md` item 2). Everything here is a test harness, not shipped product code.

## Why a reverse proxy is needed

`hs serve`'s client-server API (`/_matrix/client/*`) emits **no CORS headers at all** — confirmed
by direct probe (see `docs/status/16-management-web-interface.md`). Per the Matrix spec, the
client-server API must be reachable cross-origin from any web client (this is exactly why Synapse
and every other homeserver answer `OPTIONS` with `Access-Control-Allow-Origin: *` on these routes).
Without it, a browser client hosted on a different origin/port than the homeserver — which is the
normal deployment shape for a web client — cannot make a single `/_matrix` request: preflighted
requests (any JSON POST/PUT, which is almost every Matrix call) fail before they are even sent,
and even simple GETs are blocked from being read by script.

`scripts/element-proxy.mjs` (in `web/scripts/`) works around this for testing purposes only: it
serves Element's static assets and proxies `/_matrix`, `/_synapse`, `/.well-known` to `hs serve`,
all from one origin, so the browser never needs cross-origin `Access-Control-*` headers at all.
**This masks the CORS gap rather than fixing it — the gap is real and is reported separately.**

## One-time setup

```sh
cd web/element-testing   # this directory; config.yaml and element-config.json already here

# 1. Build the server binary (from the repo root)
cargo build -p hs-cli --bin hs

# 2. Generate + patch config (already done for this session; regenerate with:)
../../target/debug/hs generate-config --server-name test.local -o config.yaml
# then hand-patch: port 8098, add `admin` to listener resources, enable_registration: true,
# registration_shared_secret: elementtestsecret, public_baseurl: http://127.0.0.1:8098,
# rate_limits.enabled: false (so Element's normal usage doesn't trip the default
# 1-message-per-5-seconds limit while testing — a config choice for testing convenience, not a
# recommendation for production).
```

## Every run

Three processes, in order:

```sh
# 1. The homeserver
../../target/debug/hs serve -c config.yaml
# listens on 127.0.0.1:8098

# 2. Register test users (once per fresh ./data)
../../target/debug/hs register http://127.0.0.1:8098 -u ops -p opspassword123 -k elementtestsecret --admin -v
../../target/debug/hs register http://127.0.0.1:8098 -u alice -p alicepassword123 -k elementtestsecret -v
../../target/debug/hs register http://127.0.0.1:8098 -u bob -p bobpassword123 -k elementtestsecret -v

# 3. Element Web, pulled (not built) per this session's Docker instructions
docker run -d --name element-web-test -p 8080:80 \
  -v "$PWD/element-config.json:/app/config.json:ro" \
  vectorim/element-web:latest

# 4. The same-origin proxy (from web/)
cd ../..
PROXY_PORT=8090 ELEMENT_TARGET=http://127.0.0.1:8080 HS_TARGET=http://127.0.0.1:8098 \
  node scripts/element-proxy.mjs
```

Open **http://localhost:8090/** — this is Element Web, same-origin-proxied to the real server.
Log in as `alice` / `alicepassword123` (or `bob`, or register a new account — registration is
enabled in this config). Server name `test.local` is baked into `element-config.json`'s
`default_server_config.m.homeserver.base_url` (pointed at the proxy's own origin), so Element
skips `.well-known` discovery entirely (see below for why that document was still verified
separately).

## `.well-known` client discovery, verified separately

`GET http://127.0.0.1:8098/.well-known/matrix/client` returns the correct document
(`{"m.homeserver":{"base_url":"http://127.0.0.1:8098"}}`, confirmed by direct curl) once
`server.public_baseurl` is set. This was not exercised through Element's own discovery flow (that
needs a real domain name and TLS to resolve `test.local` from the browser, which this local setup
does not have) — `default_server_config` is used instead, a normal thing for a self-hosted Element
deployment to do. The document itself is real and correct; only Element's own fetch-on-domain-entry
step was not driven end-to-end.

## Known-broken without the proxy

`curl -i -X OPTIONS http://127.0.0.1:8098/_matrix/client/v3/login -H 'Origin: http://x' -H 'Access-Control-Request-Method: POST'`
returns `405 Method Not Allowed` with no `Access-Control-*` headers at all — confirming the gap
above from the wire, independent of any browser.
