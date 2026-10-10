# The demo behind a Tailscale Funnel

Written 2026-10-10. `myelin.dacrib.net` has no public DNS: it resolves only on the owner's
LAN, to the cluster's ingress (192.168.115.100), and port 8448 is closed. So no other server
could fetch its signing key (maunium.net answered every signed request from the demo
`401 Failed to find any key to satisfy ... ed25519:a_JBQV7r`), and only LAN clients could use
it. The server **keeps its name**: renaming a Matrix server is a new identity. Instead, a
Tailscale Funnel gives it a public hostname, `myelin.longhair-tet.ts.net`, and the two
`.well-known` documents send federation and clients there.

## 1. Tailnet (owner, admin console)

ACL (`https://login.tailscale.com/admin/acls`), merged into the policy:

```jsonc
"tagOwners": {
  "tag:k8s-operator": [],
  "tag:k8s": ["tag:k8s-operator"],
},
"nodeAttrs": [
  { "target": ["tag:k8s"], "attr": ["funnel"] },
],
```

OAuth client (`https://login.tailscale.com/admin/settings/oauth`): scopes **Devices: Core
(write)** and **Auth Keys (write)**, tag `tag:k8s-operator`. Keep the id and secret for step 2.
HTTPS certificates and MagicDNS must be on for the tailnet (they are: `silver` has `funnel`
and `https` in its capabilities).

## 2. The operator (through the owner's `kubectl proxy`)

```sh
helm repo add tailscale https://pkgs.tailscale.com/helmcharts && helm repo update
kubectl --server=http://127.0.0.1:8001 create namespace tailscale
kubectl --server=http://127.0.0.1:8001 -n tailscale create secret generic operator-oauth \
  --from-literal=client_id=<id> --from-literal=client_secret=<secret>
helm --kube-apiserver http://127.0.0.1:8001 upgrade --install tailscale-operator \
  tailscale/tailscale-operator -n tailscale --set oauth.clientId=<id> \
  --set oauth.clientSecret=<secret> --wait
kubectl --server=http://127.0.0.1:8001 apply -f deploy/demo/tailscale-funnel-ingress.yaml
kubectl --server=http://127.0.0.1:8001 -n myelin get ingress myelin-funnel   # ADDRESS = the ts.net name
```

The owner creates the Secret from their own shell if they would rather the secret not pass
through a session (then `--set oauth.existingSecret`-style values; see the chart's values).

## 3. The server (Configuration page, server section; or the release's values)

- `server.public_baseurl`: `https://myelin.longhair-tet.ts.net` (chart value `publicBaseUrl`).
- `server.well_known_server`: `myelin.longhair-tet.ts.net:443`. From decision 0040 on, this is
  derived from `public_baseurl` when unset; until that is rolled, set it.

Both are hot. The LAN ingress for `myelin.dacrib.net` stays; it serves the same documents.

## 4. Cloudflare (dacrib.net's DNS is there: cert-manager's issuer uses it)

Other servers fetch `https://myelin.dacrib.net/.well-known/matrix/server`, and clients
`/.well-known/matrix/client`, from the **name**, so the name needs a public answer. A proxied
DNS record and a redirect rule at the edge do it without exposing anything else:

1. DNS: `myelin.dacrib.net` A `192.0.2.1`, proxied (orange cloud). The address is never
   connected to; the rule below answers first.
2. Redirect Rule (Rules → Redirect Rules): when hostname equals `myelin.dacrib.net` and URI
   path starts with `/.well-known/matrix/`, redirect (301) to
   `concat("https://myelin.longhair-tet.ts.net", http.request.uri.path)`, preserving the query.
   Synapse, Dendrite and Conduit follow redirects on the well-known fetch; Element does too.

Later, with the LAN DNS pointing at the private address, LAN clients keep reaching the ingress
directly and public ones come in through the Funnel.

## 5. Verify

```sh
curl -si https://myelin.dacrib.net/.well-known/matrix/server | head -5     # 301 to the ts.net name
curl -s https://myelin.longhair-tet.ts.net/.well-known/matrix/server         # {"m.server":"myelin.longhair-tet.ts.net:443"}
curl -s https://myelin.longhair-tet.ts.net/_matrix/key/v2/server | head -c 200
curl -s https://myelin.longhair-tet.ts.net/_matrix/federation/v1/version
curl -s "https://federationtester.matrix.org/api/report?server_name=myelin.dacrib.net" | python3 -c 'import json,sys;print(json.load(sys.stdin)["FederationOK"])'
```

Then, from the Federation page, join a room on another server, and the remote-media warnings
for maunium.net stop.
