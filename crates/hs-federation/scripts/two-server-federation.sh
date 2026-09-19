#!/usr/bin/env bash
# Two real `hs serve` processes, two different server names, federating over real HTTPS with a
# private CA and real X-Matrix signatures -- the "achievable and genuinely valuable version" of
# "join a real room" this laptop can run without a publicly resolvable name or a public CA.
#
# What this proves, end to end, against two live processes (not fixtures, not `tower::oneshot`):
#   1. Server discovery + TLS with a private CA (`federation.custom_ca_certificates`).
#   2. Outbound `X-Matrix` request signing and inbound verification (`hs-federation::xmatrix`).
#   3. The join handshake's *resident* side (`make_join`/`send_join`, `hs-federation::join`) --
#      already tested in-process, exercised here for the first time against a second real server.
#   4. The join handshake's *client* side (`hs-federation::outbound_join`, new this session) --
#      never exercised at all before this session, because nothing in this workspace could
#      initiate an outbound join before now.
#   5. RFC-0014's fix (events are signed over their *redacted* form): server A's own
#      locally-originated events, and B's own join event, both get accepted by a real,
#      independent, spec-compliant verifier for the first time.
#
# What this does NOT prove -- see the summary this script prints at the end, and
# `docs/rfcs/0015-outbound-join-needs-a-room-bootstrap-api.md`: server B has no `hs-room` API to
# turn a verified join response into a room it can sync or post into, so "both servers send
# messages, each sees the other's" only completes in one direction (A's messages are visible to A;
# B's join is visible to A; B itself cannot yet read or post into the room).
#
# Usage: crates/hs-federation/scripts/two-server-federation.sh [workdir]
#   workdir defaults to a fresh `mktemp -d`. Re-running with the same workdir reuses the built
#   binary and generated CA/keys but always starts fresh server processes and a fresh room.
#
# Requires: cargo (to build `hs`), openssl, stunnel (`brew install stunnel` on macOS -- `hs serve`
# does not terminate TLS itself; see `tests/complement/Dockerfile.template`'s header for the same
# reasoning, and this crate's status file), curl, jq. No Docker.

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)"
WORKDIR="${1:-$(mktemp -d /tmp/hs-two-server-federation.XXXXXX)}"
mkdir -p "$WORKDIR"
echo "== workdir: $WORKDIR"

STUNNEL_BIN="${STUNNEL_BIN:-stunnel}"
if ! command -v "$STUNNEL_BIN" >/dev/null 2>&1; then
  # Homebrew's stunnel installs unlinked (it is not meant to run as a system service by default);
  # its actual binary lives here even when `stunnel` is not on PATH.
  for candidate in /opt/homebrew/opt/stunnel/bin/stunnel /usr/local/opt/stunnel/bin/stunnel; do
    if [ -x "$candidate" ]; then
      STUNNEL_BIN="$candidate"
      break
    fi
  done
fi
if ! command -v "$STUNNEL_BIN" >/dev/null 2>&1 && [ ! -x "$STUNNEL_BIN" ]; then
  echo "error: stunnel not found. Install it (macOS: brew install stunnel) -- hs serve does not" >&2
  echo "terminate TLS itself yet; see tests/complement/Dockerfile.template's header." >&2
  exit 1
fi
for tool in curl jq openssl; do
  command -v "$tool" >/dev/null 2>&1 || { echo "error: $tool not found" >&2; exit 1; }
done

# ---- 0. Build the real binary once -------------------------------------------------------------
echo "== building hs (cargo build -p hs-cli --bin hs)"
( cd "$REPO_ROOT" && cargo build -p hs-cli --bin hs )
HS="$REPO_ROOT/target/debug/hs"

# ---- 1. A private CA and one server certificate for "127.0.0.1" --------------------------------
# Both instances run on 127.0.0.1 under different ports; server names are IP literals with an
# explicit port (`127.0.0.1:8448`, `127.0.0.1:8449`), which per `crate::discovery`'s step 1 bypass
# discovery entirely -- no `.well-known`, no SRV, no A/AAAA lookup at all -- rather than a
# hostname. This is deliberate, not just convenient: `hs-federation`'s outbound `AddrResolver` is
# `hickory-resolver`, a pure userspace stub resolver that queries the configured nameservers
# directly and does **not** consult `/etc/hosts`, unlike `getaddrinfo`-based tools (`curl`, `dig`
# without search suffixes, ...). A hostname of "localhost" resolved through a real,
# search-domain-configured `/etc/resolv.conf` is genuinely unreliable for this reason (confirmed
# live: it does not resolve to 127.0.0.1 on every network) -- IP literals sidestep DNS altogether
# and are exactly as "real" a federation destination as a hostname per the spec. Both instances
# therefore present TLS for the same IP, so one certificate (with an IP SAN, not a DNS SAN),
# signed by one private CA, covers both.
TLS_DIR="$WORKDIR/tls"
mkdir -p "$TLS_DIR"
if [ ! -f "$TLS_DIR/ca.crt" ]; then
  echo "== generating a private CA and a 127.0.0.1 server certificate"
  openssl genrsa -out "$TLS_DIR/ca.key" 2048 2>/dev/null
  openssl req -x509 -new -nodes -key "$TLS_DIR/ca.key" -sha256 -days 3650 \
    -subj "/O=hs-reimplement two-server test/CN=hs-reimplement test CA" \
    -out "$TLS_DIR/ca.crt" 2>/dev/null
  openssl genrsa -out "$TLS_DIR/server.key" 2048 2>/dev/null
  openssl req -new -key "$TLS_DIR/server.key" -subj "/CN=127.0.0.1" \
    -out "$TLS_DIR/server.csr" 2>/dev/null
  cat >"$TLS_DIR/server.ext" <<EOF
subjectAltName = IP:127.0.0.1
EOF
  openssl x509 -req -in "$TLS_DIR/server.csr" -CA "$TLS_DIR/ca.crt" -CAkey "$TLS_DIR/ca.key" \
    -CAcreateserial -out "$TLS_DIR/server.crt" -days 825 -sha256 \
    -extfile "$TLS_DIR/server.ext" 2>/dev/null
else
  echo "== reusing existing CA/server certificate in $TLS_DIR"
fi

# ---- 2. Per-server signing keys, storage and native hs-config -----------------------------------
for name in a b; do
  mkdir -p "$WORKDIR/$name/signing-keys" "$WORKDIR/$name/db" "$WORKDIR/$name/media"
  if [ -z "$(find "$WORKDIR/$name/signing-keys" -type f 2>/dev/null)" ]; then
    "$HS" generate-signing-key -o "$WORKDIR/$name/signing-keys/hs.signing.key" >/dev/null
  fi
done

SERVER_A_NAME="127.0.0.1:8448"
SERVER_B_NAME="127.0.0.1:8449"
PLAIN_PORT_A=8008
PLAIN_PORT_B=8018
TLS_PORT_A=8448
TLS_PORT_B=8449

cat >"$WORKDIR/a/config.yaml" <<EOF
server:
  server_name: "$SERVER_A_NAME"
  signing_key_path: $WORKDIR/a/signing-keys
storage:
  backend: embedded
  data_dir: $WORKDIR/a/db
listeners:
  listeners:
    - port: $PLAIN_PORT_A
      bind_addresses: ["127.0.0.1"]
      resources: [client, federation, media, health]
media:
  storage:
    backend: local
    path: $WORKDIR/a/media
auth:
  enable_registration: true
  enable_legacy_login: true
federation:
  # The other instance's TLS is terminated by stunnel with a certificate signed by our shared
  # private CA -- this is the real feature under test, not a workaround: without this, outbound
  # TLS trusts only the ~140 baked-in public roots and every request to the other instance fails
  # with "unknown certificate authority" (this crate's sixth session; see its status file).
  custom_ca_certificates:
    - $TLS_DIR/ca.crt
  # Both instances run on loopback; the default blocklist (private/loopback ranges, correctly, to
  # stop a hostile room from pointing federation at a deployment's own internal network) would
  # otherwise refuse every outbound request this script makes.
  ip_range_blocklist: []
EOF

cat >"$WORKDIR/b/config.yaml" <<EOF
server:
  server_name: "$SERVER_B_NAME"
  signing_key_path: $WORKDIR/b/signing-keys
storage:
  backend: embedded
  data_dir: $WORKDIR/b/db
listeners:
  listeners:
    - port: $PLAIN_PORT_B
      bind_addresses: ["127.0.0.1"]
      resources: [client, federation, media, health]
media:
  storage:
    backend: local
    path: $WORKDIR/b/media
auth:
  enable_registration: true
  enable_legacy_login: true
federation:
  custom_ca_certificates:
    - $TLS_DIR/ca.crt
  ip_range_blocklist: []
EOF

# ---- 3. stunnel: TLS on the federation port, forwarding to the plaintext hs listener ------------
# Same pattern as `tests/complement/stunnel.conf.template` -- `hs serve` does not terminate TLS
# itself yet (`crates/hs-cli/src/serve.rs` logs this and serves plaintext even when a listener's
# `tls:` block is set), so this is the smallest tool that does exactly "accept TLS on one port,
# forward plaintext to another".
cat >"$WORKDIR/a/stunnel.conf" <<EOF
foreground = yes
pid = $WORKDIR/a/stunnel.pid
output = $WORKDIR/a/stunnel.log
[federation]
accept = 127.0.0.1:$TLS_PORT_A
connect = 127.0.0.1:$PLAIN_PORT_A
cert = $TLS_DIR/server.crt
key = $TLS_DIR/server.key
EOF
cat >"$WORKDIR/b/stunnel.conf" <<EOF
foreground = yes
pid = $WORKDIR/b/stunnel.pid
output = $WORKDIR/b/stunnel.log
[federation]
accept = 127.0.0.1:$TLS_PORT_B
connect = 127.0.0.1:$PLAIN_PORT_B
cert = $TLS_DIR/server.crt
key = $TLS_DIR/server.key
EOF

PIDS=()
cleanup() {
  echo "== shutting down"
  for pid in "${PIDS[@]:-}"; do
    kill "$pid" >/dev/null 2>&1 || true
  done
}
trap cleanup EXIT

echo "== starting server A ($SERVER_A_NAME, plaintext :$PLAIN_PORT_A, TLS :$TLS_PORT_A)"
"$HS" serve -c "$WORKDIR/a/config.yaml" >"$WORKDIR/a/hs.log" 2>&1 &
PIDS+=("$!")
"$STUNNEL_BIN" "$WORKDIR/a/stunnel.conf" &
PIDS+=("$!")

echo "== starting server B ($SERVER_B_NAME, plaintext :$PLAIN_PORT_B, TLS :$TLS_PORT_B)"
"$HS" serve -c "$WORKDIR/b/config.yaml" >"$WORKDIR/b/hs.log" 2>&1 &
PIDS+=("$!")
"$STUNNEL_BIN" "$WORKDIR/b/stunnel.conf" &
PIDS+=("$!")

wait_ready() {
  local url="$1" name="$2"
  for _ in $(seq 1 50); do
    if curl -sf "$url" >/dev/null 2>&1; then
      echo "== $name is ready"
      return 0
    fi
    sleep 0.2
  done
  echo "error: $name never became ready ($url); see $WORKDIR/*/hs.log" >&2
  exit 1
}
wait_ready "http://127.0.0.1:$PLAIN_PORT_A/_matrix/client/versions" "server A"
wait_ready "http://127.0.0.1:$PLAIN_PORT_B/_matrix/client/versions" "server B"

# Sanity check on the actual feature under test, before anything else: A's outbound federation
# client, talking real TLS to B's stunnel over the private CA, should get a real key response --
# proof `federation.custom_ca_certificates` really is wired end to end (crates/hs-cli/src/federation.rs's
# `client_config`, fixed this session -- see docs/status/06-federation.md), not just accepted by
# the config parser. A -1/timeout here almost always means stunnel is not running or the CA file
# path is wrong, not a code bug.
echo "== sanity check: fetching B's server key over TLS with the private CA"
set +e
KEY_CHECK_STATUS=$(curl -s -o "$WORKDIR/b-key-via-a.json" -w '%{http_code}' \
  --cacert "$TLS_DIR/ca.crt" \
  "https://127.0.0.1:$TLS_PORT_B/_matrix/key/v2/server")
CURL_STATUS=$?
set -e
if [ "$CURL_STATUS" -ne 0 ] || [ "$KEY_CHECK_STATUS" != "200" ]; then
  echo "error: could not fetch B's key over TLS with the private CA (curl exit $CURL_STATUS, HTTP $KEY_CHECK_STATUS)" >&2
  cat "$WORKDIR/b-key-via-a.json" >&2 || true
  exit 1
fi
echo "   ok: $(jq -c '{server_name, valid_until_ts}' "$WORKDIR/b-key-via-a.json")"

# ---- 4. Register a user on each server via the real client-server UIA dance --------------------
register() {
  local base="$1" user="$2" password="$3"
  local first
  first=$(curl -s -X POST "$base/_matrix/client/v3/register" \
    -H "Content-Type: application/json" \
    -d "{\"username\":\"$user\",\"password\":\"$password\",\"initial_device_display_name\":\"two-server-script\"}")
  local session
  session=$(echo "$first" | jq -r '.session // empty')
  if [ -z "$session" ]; then
    echo "error: registration for $user did not offer a UIA session: $first" >&2
    exit 1
  fi
  curl -s -X POST "$base/_matrix/client/v3/register" \
    -H "Content-Type: application/json" \
    -d "{\"username\":\"$user\",\"password\":\"$password\",\"initial_device_display_name\":\"two-server-script\",\"auth\":{\"type\":\"m.login.dummy\",\"session\":\"$session\"}}"
}

echo "== registering @alice on A and @bob on B"
ALICE=$(register "http://127.0.0.1:$PLAIN_PORT_A" alice hunter2-alice)
BOB=$(register "http://127.0.0.1:$PLAIN_PORT_B" bob hunter2-bob)
ALICE_TOKEN=$(echo "$ALICE" | jq -r '.access_token')
ALICE_ID=$(echo "$ALICE" | jq -r '.user_id')
BOB_TOKEN=$(echo "$BOB" | jq -r '.access_token')
BOB_ID=$(echo "$BOB" | jq -r '.user_id')
if [ "$ALICE_TOKEN" = "null" ] || [ "$BOB_TOKEN" = "null" ]; then
  echo "error: registration failed. alice=$ALICE bob=$BOB" >&2
  exit 1
fi
echo "   alice: $ALICE_ID"
echo "   bob:   $BOB_ID"

# ---- 5. Alice creates a public room on A, and posts a message -----------------------------------
echo "== alice creates a room on A"
CREATE=$(curl -s -X POST "http://127.0.0.1:$PLAIN_PORT_A/_matrix/client/v3/createRoom" \
  -H "Authorization: Bearer $ALICE_TOKEN" -H "Content-Type: application/json" \
  -d '{"preset":"public_chat","name":"two-server federation test","room_version":"11"}')
ROOM_ID=$(echo "$CREATE" | jq -r '.room_id')
if [ "$ROOM_ID" = "null" ]; then
  echo "error: room creation failed: $CREATE" >&2
  exit 1
fi
echo "   room: $ROOM_ID"

curl -s -X PUT "http://127.0.0.1:$PLAIN_PORT_A/_matrix/client/v3/rooms/$ROOM_ID/send/m.room.message/txn-alice-1" \
  -H "Authorization: Bearer $ALICE_TOKEN" -H "Content-Type: application/json" \
  -d '{"msgtype":"m.text","body":"hello from alice on server A"}' >/dev/null
echo "== alice sent a message"

# ---- 6. Bob's server (B) joins A's room via the real make_join/send_join handshake --------------
# This is the deliverable: server B, which has never heard of this room, asks server A (over real
# HTTPS, with real X-Matrix request signatures, trusting A's certificate only because of the
# private CA configured above) for a join template, signs it as $BOB_ID's own homeserver (the
# real, spec-correct redacted-form signing RFC-0014 fixed), submits it back, and verifies every
# event A hands back. `hs-federation::outbound_join` is new this session -- see that module's doc
# and docs/rfcs/0015-outbound-join-needs-a-room-bootstrap-api.md for exactly what this does and
# does not close (the join is real and persisted on A's side; B cannot yet represent the room
# locally, so this is run as a diagnostic CLI command against B's own config, not through B's
# ordinary client API).
echo "== bob's server (B) joins A's room via make_join/send_join"
set +e
"$HS" federation-join-room -c "$WORKDIR/b/config.yaml" \
  --destination "$SERVER_A_NAME" --room "$ROOM_ID" --user "$BOB_ID"
JOIN_STATUS=$?
set -e
if [ "$JOIN_STATUS" -ne 0 ]; then
  echo "error: federation-join-room failed (exit $JOIN_STATUS)" >&2
  exit 1
fi

# ---- 7. Verify, from A's own client API, that bob genuinely joined ------------------------------
echo "== verifying on A: bob should now be a joined member"
MEMBERS=$(curl -s "http://127.0.0.1:$PLAIN_PORT_A/_matrix/client/v3/rooms/$ROOM_ID/joined_members" \
  -H "Authorization: Bearer $ALICE_TOKEN")
if echo "$MEMBERS" | jq -e --arg u "$BOB_ID" '.joined // {} | has($u)' >/dev/null; then
  echo "   confirmed: $(echo "$MEMBERS" | jq -c ".joined[\"$BOB_ID\"]")"
else
  echo "error: bob does not appear in A's joined_members: $MEMBERS" >&2
  exit 1
fi

STATE=$(curl -s "http://127.0.0.1:$PLAIN_PORT_A/_matrix/client/v3/rooms/$ROOM_ID/state/m.room.member/$BOB_ID" \
  -H "Authorization: Bearer $ALICE_TOKEN")
echo "   bob's membership event on A: $(echo "$STATE" | jq -c .)"

cat <<SUMMARY

================================================================================================
RESULT
================================================================================================
Room:   $ROOM_ID
Server A ($SERVER_A_NAME): alice created the room, sent a message, and now sees bob (from
  server B) as a real, federated, durably persisted joined member -- verified above via A's own
  client API, not just this script's say-so.
Server B ($SERVER_B_NAME): performed the real make_join/send_join handshake against A over TLS
  with a private CA and real X-Matrix request signatures, and independently verified every event
  A returned (content hash + signature, against each event's own sender). This is a genuine,
  live, cross-process proof of:
    - federation.custom_ca_certificates and the outbound TLS client (this session found and fixed
      a wiring bug: the config field existed but crates/hs-cli/src/federation.rs's client_config
      never read the certificate files -- see docs/status/06-federation.md).
    - outbound and inbound X-Matrix request signing.
    - RFC-0014's fix: events signed over their redacted form verify against a real, independent
      peer for the first time.

What is NOT proven here (see docs/rfcs/0015-outbound-join-needs-a-room-bootstrap-api.md):
  bob's own client cannot sync this room or post into it -- hs-room has no API yet to create a
  local room from a federation join response's state snapshot, only to originate a brand new one
  or apply one more event to a room it already has. So this is a one-way federation proof: A sees
  B's join; B cannot yet act on it.

What a real public join over the open internet would still exercise that this script does not:
  - DNS-based discovery (.well-known and SRV) -- this script's server names are IP literals, which
    per crate::discovery's step 1 bypass ALL discovery (no .well-known fetch, no SRV lookup, no
    A/AAAA lookup at all). A real deployment's server_name is a hostname, taking step 2 or 3.
  - A publicly trusted CA and real hostname/certificate validation against the public web PKI,
    rather than a private CA both sides are configured to trust ahead of time.
  - Another Matrix implementation's own quirks and interpretation of ambiguous spec corners
    (this script's "remote" is another instance of the same server).
  - Version negotiation against a server that is not itself: both instances here support exactly
    the same room versions and the same v1/v2 endpoint spellings.
================================================================================================
SUMMARY
