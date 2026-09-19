#!/bin/sh
# Container entrypoint for the Complement image (see Dockerfile.template). Every step here is
# idempotent (`if [ ! -f ... ]` guards) because Complement's image contract requires tolerating
# CMD/ENTRYPOINT being invoked more than once per container lifetime.
set -eu

: "${SERVER_NAME:?SERVER_NAME must be set by Complement at container start}"

# ---- 1. Federation TLS certificate, signed against Complement's mounted CA --------------------
# Exact recipe from refs/complement/README.md's "Complement PKI" section.
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

# Complement runs several homeservers in one blueprint that federate with each other, so this
# server also needs to trust the same CA (for outbound federation requests it makes as a client).
if [ -f /complement/ca/ca.crt ]; then
  cp /complement/ca/ca.crt /usr/local/share/ca-certificates/complement-ca.crt
  update-ca-certificates >/dev/null 2>&1 || true
fi

# ---- 2. This server's own signing key, persisted across ENTRYPOINT re-invocations -------------
SIGNING_KEY_DIR=/data/signing-keys
mkdir -p "$SIGNING_KEY_DIR"
if [ -z "$(find "$SIGNING_KEY_DIR" -type f 2>/dev/null)" ]; then
  echo "startup.sh: generating a signing key at $SIGNING_KEY_DIR" >&2
  hs generate-signing-key -o "$SIGNING_KEY_DIR/hs.signing.key"
fi

# ---- 3. Native hs-config, written fresh each start (cheap, and SERVER_NAME can differ across
#         containers reusing the same image even though /data itself is not reused across runs).
DB_DIR=/data/db
mkdir -p "$DB_DIR"
CONFIG_PATH=/data/config.yaml
cat >"$CONFIG_PATH" <<EOF
server:
  server_name: "$SERVER_NAME"
  signing_key_path: $SIGNING_KEY_DIR
storage:
  backend: embedded
  data_dir: $DB_DIR
listeners:
  listeners:
    - port: 8008
      bind_addresses: ["0.0.0.0"]
      resources: [client, federation, media, health]
auth:
  enable_registration: true
  enable_legacy_login: true
federation:
  # Complement's containers and synthetic federation-test doubles present certificates signed by
  # its own generated CA (/complement/ca/ca.crt, trusted into the OS store above) or are reached
  # over private Docker/host-internal addresses. Neither is trusted by hs-federation's outbound
  # `reqwest` client as built (rustls-tls's webpki-roots backend never reads the OS trust store,
  # so step 1's update-ca-certificates is a no-op for it; there is also no config surface yet for
  # an extra trusted-CA list, unlike Synapse's federation_custom_ca_list). Synapse's own Complement
  # config (refs/synapse/docker/complement/conf/workers-shared-extra.yaml.j2) resolves the
  # equivalent two problems with federation_custom_ca_list + federation_ip_range_blacklist: []; the
  # first has no equivalent here yet (see docs/status/14-test-and-conformance.md), so
  # verify_certificates: false is the closest available substitute for this harness only -- a real
  # deployment should keep the default `true` and add proper CA trust instead.
  verify_certificates: false
  ip_range_blocklist: []
EOF

# ---- 4. TLS termination in front of the plaintext hs listener ---------------------------------
# hs's single router already answers both client and federation resources on 8008 (see
# Dockerfile.template's header), so stunnel only needs to forward 8448 -> 8008.
mkdir -p /etc/stunnel
sed -e "s#__CERT_DIR__#$CERT_DIR#g" /stunnel.conf.template >/etc/stunnel/stunnel.conf
stunnel4 /etc/stunnel/stunnel.conf &

# ---- 5. The real server, as PID 1's foreground child ------------------------------------------
# `wait -n` (bash) would let us notice either process dying; this script is `/bin/sh` (dash on
# Debian, no `wait -n`), so it just waits on `hs` -- the process Complement's healthcheck and
# every test actually depend on. If `hs` dies the container exits and Complement notices; if
# stunnel alone dies, federation-over-TLS tests fail loudly instead of hanging, which is an
# equally clear signal.
exec hs serve -c "$CONFIG_PATH"
