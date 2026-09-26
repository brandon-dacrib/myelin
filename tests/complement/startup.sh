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
  # Complement's recipe stops at a CN. A certificate with no subject alternative name is
  # rejected by rustls, which is what this server's own outbound client is built on, so the
  # federation requests between two homeservers in one blueprint (hs1 joining hs2's room) all
  # failed at the TLS handshake -- invisibly, until the client's error started reporting its
  # cause. A real deployment's certificate always carries a SAN; this one now does too.
  case "$SERVER_NAME" in
    *[!0-9.]*) SAN="DNS:${SERVER_NAME%%:*}" ;;
    *) SAN="IP:${SERVER_NAME%%:*}" ;;
  esac
  printf 'subjectAltName = %s\n' "$SAN" > "$CERT_DIR/server.ext"
  openssl x509 -req -in "$CERT_DIR/server.csr" \
    -CA /complement/ca/ca.crt -CAkey /complement/ca/ca.key -CAcreateserial \
    -out "$CERT_DIR/server.crt" -days 1 -sha256 -extfile "$CERT_DIR/server.ext"
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

# ---- 2b. Federation CA trust mode --------------------------------------------------------------
# Retired workaround, kept only as an opt-in escape hatch.
#
# Until 2026-09-19 this harness unconditionally set `federation.verify_certificates: false`,
# because setting `federation.custom_ca_certificates` in a real config file had never had any
# effect on a running server: `hs-config`/`hs-federation` parsed, validated and unit-tested the
# option, but `crates/hs-cli/src/federation.rs::client_config` -- the one site that actually boots
# a `FederationClient` for `hs serve` -- built its `ClientConfig` with `..ClientConfig::default()`
# for everything not explicitly listed, silently dropping the configured paths on the floor.
#
# That is now fixed (see that function's own doc comment for the two-server federation test that
# proved it), and this session verified it end to end: two back-to-back full runs of the
# `refs/complement` `tests` package (top-level, federation-heavy) -- one with the old
# `verify_certificates: false`, one with `custom_ca_certificates: ["/complement/ca/ca.crt"]` and
# `verify_certificates` left at its real default `true` -- produced byte-for-byte identical
# top-level and leaf pass/fail/skip results (leaf 212: 52/153/7; top 88: 6/81/1; see
# docs/status/14-test-and-conformance.md) and zero TLS/certificate errors in either log. There is
# no remaining reason to run this harness with certificate verification off, so `trust_ca` -- the
# configuration a real deployment would actually use -- is now the default.
#
# HS_COMPLEMENT_CA_MODE=insecure (via Complement's env passthrough:
# `COMPLEMENT_SHARE_ENV_PREFIX=PASS_ PASS_HS_COMPLEMENT_CA_MODE=insecure go test ...`, Complement
# strips the `PASS_` prefix before it reaches this container -- see refs/complement/README.md's
# "pass environment variables to the image under test" section) restores the old
# `verify_certificates: false` behaviour, kept only in case a future test needs to isolate TLS
# verification as a variable again; nothing in this project's own CI/README instructions should
# ever need to set it.
CA_MODE="${HS_COMPLEMENT_CA_MODE:-trust_ca}"
if [ "$CA_MODE" = "insecure" ]; then
  echo "startup.sh: federation CA mode = insecure (verify_certificates: false) -- the retired workaround, opted back in" >&2
  FEDERATION_CA_CONFIG='  verify_certificates: false'
else
  echo "startup.sh: federation CA mode = trust_ca (verify_certificates stays at its default true;" >&2
  echo "  custom_ca_certificates: [/complement/ca/ca.crt])" >&2
  FEDERATION_CA_CONFIG='  custom_ca_certificates: ["/complement/ca/ca.crt"]'
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
  # Complement registers its admin accounts through Synapse's shared-secret protocol, with this
  # fixed secret (refs/complement/client/auth.go: `SharedSecret = "complement"`). Without it
  # /_synapse/admin/v1/register answers 404 and Complement reports that the image "does not
  # support shared secret registration" -- three assertions this server could pass all along.
  registration_shared_secret: complement
federation:
$FEDERATION_CA_CONFIG
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
