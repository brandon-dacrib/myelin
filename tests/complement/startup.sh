#!/bin/sh
# Container entrypoint for the Complement image (see Dockerfile.template). Untested end to end
# (no `hs-server` binary exists yet to exec at the bottom of this script -- see that file's
# TODO markers), but the certificate-signing steps are exactly Complement's documented recipe
# (refs/complement/README.md, "Complement PKI") and can be exercised on their own once Docker is
# available, independent of the server binary.
set -eu

: "${SERVER_NAME:?SERVER_NAME must be set by Complement at container start}"

CERT_DIR=/data/tls
mkdir -p "$CERT_DIR"

if [ ! -f "$CERT_DIR/server.crt" ]; then
  echo "startup.sh: signing a federation TLS cert for SERVER_NAME=$SERVER_NAME" >&2
  openssl genrsa -out "$CERT_DIR/server.key" 2048
  openssl req -new -sha256 -key "$CERT_DIR/server.key" \
    -subj "/C=US/ST=CA/O=hs-reimplement complement/CN=$SERVER_NAME" \
    -out "$CERT_DIR/server.csr"
  openssl x509 -req -in "$CERT_DIR/server.csr" \
    -CA /complement/ca/ca.crt -CAkey /complement/ca/ca.key -CAcreateserial \
    -out "$CERT_DIR/server.crt" -days 1 -sha256
fi

# Complement's CA is what the *other* containers' certs are signed by; this server needs to trust
# it too, since Complement runs several homeservers that federate with each other in the same
# blueprint.
if [ -f /complement/ca/ca.crt ]; then
  cp /complement/ca/ca.crt /usr/local/share/ca-certificates/complement-ca.crt
  update-ca-certificates >/dev/null 2>&1 || true
fi

# TODO: replace with the real invocation once hs-server exists and its config surface (hs-config,
# track 13) is wired up. Expected shape: listen on 0.0.0.0:8008 (plain HTTP, client-server and
# appservice traffic) and 0.0.0.0:8448 (HTTPS federation traffic, using $CERT_DIR/server.{crt,key}),
# server name $SERVER_NAME, embedded hs-kv store rooted at /data, and the Synapse-compatible
# `/_synapse/admin/v1/register` shared secret hardcoded to `complement` (RFC 0004's Synapse
# compatibility surface) since that is Complement's own hardcoded expectation, not a real secret.
exec hs-server \
  --server-name "$SERVER_NAME" \
  --client-listen 0.0.0.0:8008 \
  --federation-listen 0.0.0.0:8448 \
  --federation-tls-cert "$CERT_DIR/server.crt" \
  --federation-tls-key "$CERT_DIR/server.key" \
  --data-dir /data \
  --registration-shared-secret complement
