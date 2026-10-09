#!/usr/bin/env bash
# Myelin <-> Synapse interop: a real Synapse in Docker next to an `hs` built from this tree,
# federating over TLS with a private CA, driven through both servers' client APIs.
#
# Topology (every container, network and volume is named `fed-synapse*`):
#   fed-synapse-synapse   ghcr.io/element-hq/synapse, server_name 127.0.0.1:8448; client API
#                         published on host port 8408, federation (TLS) on host port 8448.
#   fed-synapse-myelin    nginx terminating TLS for Myelin at fed-synapse-myelin:8449 (inside the
#                         `fed-synapse` network; `hs serve` does not terminate TLS itself) and
#                         proxying to the host's plaintext listener.
#   hs (host process)     server_name fed-synapse-myelin:8449, plaintext on 0.0.0.0:8449.
# Server names carry ports, so both sides connect directly: no .well-known, no SRV.
#
# Exit 0 with "SKIP" when Docker is not usable, so CI without Docker stays green.
# Usage: tests/federation-synapse/run.sh [workdir]; HS_BINARY=... skips the cargo build;
#        SYNAPSE_IMAGE overrides the image; KEEP=1 leaves the containers up for inspection.
set -uo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$HERE/../.." && pwd)"
WORKDIR="${1:-$(mktemp -d /tmp/fed-synapse.XXXXXX)}"
# Docker Hub's `matrixdotorg/synapse` through Google's mirror: an agent session cannot pull from
# Docker Hub or ghcr.io (the keychain credential helper), and the mirror needs no credentials.
SYNAPSE_IMAGE="${SYNAPSE_IMAGE:-mirror.gcr.io/matrixdotorg/synapse:latest}"
NGINX_IMAGE="${NGINX_IMAGE:-public.ecr.aws/docker/library/nginx:alpine}"
SYN_NAME="127.0.0.1:8448"
SYN_CLIENT="http://127.0.0.1:8408"
MY_NAME="fed-synapse-myelin:8449"
MY_CLIENT="http://127.0.0.1:8449"
RESULTS="$WORKDIR/results.tsv"
mkdir -p "$WORKDIR"
: > "$RESULTS"
HS_PID=""

command -v docker >/dev/null 2>&1 || { echo "SKIP: docker is not installed; the Myelin<->Synapse interop run needs it."; exit 0; }
# OrbStack's socket, and a Docker config with no credential store: the owner's config names the
# macOS keychain, which an agent session cannot open, and every pull would fail on it.
if [ -z "${DOCKER_HOST:-}" ] && [ -S "$HOME/.orbstack/run/docker.sock" ]; then
  export DOCKER_HOST="unix://$HOME/.orbstack/run/docker.sock"
fi
if [ -z "${DOCKER_CONFIG:-}" ]; then
  DOCKER_CONFIG="$(mktemp -d)"
  echo '{"auths":{}}' >"$DOCKER_CONFIG/config.json"
  export DOCKER_CONFIG
fi
if ! docker info >/dev/null 2>&1; then
  echo "SKIP: Docker is not available; the Myelin<->Synapse interop run needs it."
  exit 0
fi
for tool in curl jq openssl; do
  command -v "$tool" >/dev/null 2>&1 || { echo "SKIP: $tool not found"; exit 0; }
done

cleanup() {
  [ "${KEEP:-0}" = "1" ] && { echo "== KEEP=1: leaving containers and $WORKDIR"; return; }
  [ -n "$HS_PID" ] && kill "$HS_PID" 2>/dev/null && wait "$HS_PID" 2>/dev/null
  docker rm -f fed-synapse-synapse fed-synapse-myelin >/dev/null 2>&1 || true
  docker network rm fed-synapse >/dev/null 2>&1 || true
}
trap cleanup EXIT

record() { # record <step> <PASS|FAIL> <note>
  printf '%s\t%s\t%s\n' "$1" "$2" "$3" >> "$RESULTS"
  echo "== [$2] $1: $3"
}
expect() { # expect <step> <description> <actual> <expected substring>
  if [[ "$3" == *"$4"* ]]; then record "$1" PASS "$2"; else record "$1" FAIL "$2 -- expected '$4' in: ${3:0:400}"; fi
}

# ---- 0. Binary, CA, certificates ------------------------------------------------------------
if [ -z "${HS_BINARY:-}" ]; then
  echo "== building hs"
  (cd "$REPO_ROOT" && CARGO_PROFILE_DEV_DEBUG=0 cargo build -p hs-cli --bin hs >/dev/null) || { echo "build failed"; exit 1; }
  HS_BINARY="$REPO_ROOT/target/debug/hs"
fi
mkdir -p "$WORKDIR/pki" "$WORKDIR/synapse" "$WORKDIR/myelin"
cd "$WORKDIR/pki"
if [ ! -f ca.crt ]; then
  openssl req -x509 -newkey rsa:2048 -nodes -keyout ca.key -out ca.crt -days 7 -subj "/CN=fed-synapse CA" >/dev/null 2>&1
  for who in synapse myelin; do
    if [ "$who" = synapse ]; then san="IP:127.0.0.1,DNS:localhost"; else san="DNS:fed-synapse-myelin"; fi
    openssl req -newkey rsa:2048 -nodes -keyout $who.key -out $who.csr -subj "/CN=$who" >/dev/null 2>&1
    printf 'subjectAltName=%s\n' "$san" > $who.ext
    openssl x509 -req -in $who.csr -CA ca.crt -CAkey ca.key -CAcreateserial -out $who.crt -days 7 -extfile $who.ext >/dev/null 2>&1
  done
fi
chmod 644 "$WORKDIR"/pki/*

# ---- 1. Synapse -----------------------------------------------------------------------------
docker network create fed-synapse >/dev/null 2>&1 || true
cp "$WORKDIR"/pki/{ca.crt,synapse.crt,synapse.key} "$WORKDIR/synapse/"
if [ ! -f "$WORKDIR/synapse/signing.key" ]; then
  # `generate` writes homeserver.yaml and the signing key; we keep only the key.
  docker run --rm -v "$WORKDIR/synapse:/data" -e SYNAPSE_SERVER_NAME="$SYN_NAME" -e SYNAPSE_REPORT_STATS=no \
    "$SYNAPSE_IMAGE" generate >/dev/null 2>&1
  mv "$WORKDIR/synapse"/*.signing.key "$WORKDIR/synapse/signing.key" 2>/dev/null || true
fi
cat > "$WORKDIR/synapse/homeserver.yaml" <<YAML
server_name: "$SYN_NAME"
pid_file: /data/homeserver.pid
report_stats: false
signing_key_path: /data/signing.key
media_store_path: /data/media
database: { name: sqlite3, args: { database: /data/homeserver.db } }
log_config: /data/log.yaml
listeners:
  - port: 8008
    tls: false
    type: http
    x_forwarded: false
    resources: [{ names: [client] }]
  - port: 8448
    tls: true
    type: http
    resources: [{ names: [federation] }]
tls_certificate_path: /data/synapse.crt
tls_private_key_path: /data/synapse.key
federation_custom_ca_list: [/data/ca.crt]
federation_ip_range_blacklist: []
trusted_key_servers: []
suppress_key_server_warning: true
enable_registration: true
enable_registration_without_verification: true
allow_public_rooms_over_federation: true
allow_device_name_lookup_over_federation: true
# Synapse forbids publishing to the room directory by default since 1.126; the interop run
# publishes rooms to list them over federation, so allow it.
room_list_publication_rules: [{action: allow}]
rc_message: { per_second: 1000, burst_count: 1000 }
rc_registration: { per_second: 1000, burst_count: 1000 }
rc_login: { address: { per_second: 1000, burst_count: 1000 }, account: { per_second: 1000, burst_count: 1000 }, failed_attempts: { per_second: 1000, burst_count: 1000 } }
rc_joins: { local: { per_second: 1000, burst_count: 1000 }, remote: { per_second: 1000, burst_count: 1000 } }
rc_invites: { per_room: { per_second: 1000, burst_count: 1000 }, per_user: { per_second: 1000, burst_count: 1000 } }
rc_federation: { window_size: 1000, sleep_limit: 1000, sleep_delay: 1, reject_limit: 1000, concurrent: 1000 }
YAML
cat > "$WORKDIR/synapse/log.yaml" <<'YAML'
version: 1
formatters: { precise: { format: '%(asctime)s - %(name)s - %(levelname)s - %(message)s' } }
handlers: { console: { class: logging.StreamHandler, formatter: precise } }
loggers: { synapse.federation: { level: DEBUG }, synapse.handlers.federation: { level: DEBUG } }
root: { level: INFO, handlers: [console] }
YAML
docker rm -f fed-synapse-synapse >/dev/null 2>&1 || true
docker run -d --name fed-synapse-synapse --network fed-synapse \
  --add-host host.docker.internal:host-gateway \
  -p 127.0.0.1:8408:8008 -p 127.0.0.1:8448:8448 \
  -v "$WORKDIR/synapse:/data" "$SYNAPSE_IMAGE" >/dev/null || { echo "synapse did not start"; exit 1; }

# ---- 2. TLS in front of Myelin ---------------------------------------------------------------
cat > "$WORKDIR/myelin/nginx.conf" <<'NGINX'
events {}
http {
  server {
    listen 8449 ssl;
    server_name fed-synapse-myelin;
    ssl_certificate /pki/myelin.crt;
    ssl_certificate_key /pki/myelin.key;
    client_max_body_size 100m;
    location / {
      proxy_pass http://host.docker.internal:8449;
      proxy_set_header Host $host;
      proxy_set_header X-Forwarded-For $remote_addr;
      proxy_set_header X-Forwarded-Proto https;
    }
  }
}
NGINX
docker rm -f fed-synapse-myelin >/dev/null 2>&1 || true
docker run -d --name fed-synapse-myelin --network fed-synapse \
  --add-host host.docker.internal:host-gateway \
  -v "$WORKDIR/pki:/pki:ro" -v "$WORKDIR/myelin/nginx.conf:/etc/nginx/nginx.conf:ro" \
  "$NGINX_IMAGE" >/dev/null || { echo "nginx did not start"; exit 1; }

# ---- 3. Myelin ------------------------------------------------------------------------------
cat > "$WORKDIR/myelin/hs.yaml" <<YAML
server:
  server_name: "$MY_NAME"
  signing_key_path: "$WORKDIR/myelin/signing-keys"
listeners:
  listeners:
    - port: 8449
      bind_addresses: ["0.0.0.0"]
      resources: [client, federation, media, health]
      x_forwarded: true
storage: { backend: embedded, data_dir: "$WORKDIR/myelin/data" }
media: { storage: { backend: local, path: "$WORKDIR/myelin/media" } }
auth: { enable_registration: true }
federation:
  ip_range_blocklist: []
  custom_ca_certificates: ["$WORKDIR/pki/ca.crt"]
  allow_public_rooms_over_federation: true
  allow_device_name_lookup_over_federation: true
rate_limits: { enabled: false }
YAML
RUST_LOG="${RUST_LOG:-info,hs_federation=debug}" "$HS_BINARY" serve --config "$WORKDIR/myelin/hs.yaml" > "$WORKDIR/myelin/hs.log" 2>&1 &
HS_PID=$!

wait_for() { for _ in $(seq 1 120); do curl -fsS "$1" >/dev/null 2>&1 && return 0; sleep 1; done; return 1; }
wait_for "$MY_CLIENT/_matrix/client/versions" || { echo "hs did not come up; see $WORKDIR/myelin/hs.log"; exit 1; }
wait_for "$SYN_CLIENT/_matrix/client/versions" || { echo "synapse did not come up: docker logs fed-synapse-synapse"; exit 1; }

# ---- helpers ---------------------------------------------------------------------------------
reg() { # reg <base> <user> -> "<token> <device_id>"
  curl -s -X POST "$1/_matrix/client/v3/register" -H 'content-type: application/json' \
    -d "{\"username\":\"$2\",\"password\":\"fed-synapse-pw\",\"auth\":{\"type\":\"m.login.dummy\"}}" | jq -r '"\(.access_token) \(.device_id)"'
}
api() { # api <base> <token> <method> <path> [json]
  curl -s -X "$3" "$1/_matrix/client/v3$4" -H "authorization: Bearer $2" -H 'content-type: application/json' ${5:+-d "$5"}
}
# wait until <base>/<token> sees <event type with content substring> in <room> via /messages
wait_event() { # wait_event <base> <token> <room> <jq filter> -> 0/1
  for _ in $(seq 1 60); do
    if api "$1" "$2" GET "/rooms/$3/messages?dir=b&limit=100" | jq -e "$4" >/dev/null 2>&1; then return 0; fi
    sleep 1
  done; return 1
}
enc() { jq -rn --arg v "$1" '$v|@uri'; }
# poll until <base>/<token> sees <user>'s membership in <room> equal <membership>, up to ~30s;
# reads the member state event, which a server serves even to the member once they have left/been
# banned (so a banned user can confirm their own ban).
wait_membership() { # wait_membership <base> <token> <room> <user> <membership> -> 0/1
  for _ in $(seq 1 30); do
    local m; m=$(api "$1" "$2" GET "/rooms/$3/state/m.room.member/$(enc "$4")" | jq -r '.membership // empty' 2>/dev/null)
    [ "$m" = "$5" ] && { echo "$m"; return 0; }
    sleep 1
  done; return 1
}
# poll <base> <token> <jq filter over a full /sync body> until it matches, up to ~20s
wait_sync() { # wait_sync <base> <token> <jq filter> -> prints the first match, or empty
  for _ in $(seq 1 20); do
    local out; out=$(api "$1" "$2" GET "/sync?timeout=1000" | jq -c "$3" 2>/dev/null)
    if [ -n "$out" ] && [ "$out" != null ] && [ "$out" != '""' ]; then echo "$out"; return 0; fi
    sleep 1
  done; return 1
}

read SYN_TOK SYN_DEV <<<"$(reg "$SYN_CLIENT" synalice)"; read MY_TOK MY_DEV <<<"$(reg "$MY_CLIENT" mybob)"
SYN_USER="@synalice:$SYN_NAME"; MY_USER="@mybob:$MY_NAME"
[ "$SYN_TOK" != null ] && [ "$MY_TOK" != null ] || { echo "registration failed: syn=$SYN_TOK my=$MY_TOK"; exit 1; }

# ---- Step 1: keys ----------------------------------------------------------------------------
expect "1 keys" "Myelin serves /_matrix/key/v2/server" "$(curl -s "$MY_CLIENT/_matrix/key/v2/server")" '"server_name":"'"$MY_NAME"'"'
expect "1 keys" "Synapse serves /_matrix/key/v2/server over TLS" "$(curl -s --cacert "$WORKDIR/pki/ca.crt" "https://$SYN_NAME/_matrix/key/v2/server")" '"server_name":"'"$SYN_NAME"'"'
expect "1 keys" "Myelin fetches Synapse's keys through its notary" "$(curl -s "$MY_CLIENT/_matrix/key/v2/query/$SYN_NAME")" '"server_name":"'"$SYN_NAME"'"'
# The reverse -- that Synapse can serve Myelin's keys through its own notary -- is asserted at the
# end of step 4, once the two servers have federated and Synapse holds Myelin's keys (a fresh
# Synapse's notary cache is empty and the deprecated GET form does not fetch on demand).

# ---- Step 2: Myelin joins a Synapse room ----------------------------------------------------
ROOM_S=$(api "$SYN_CLIENT" "$SYN_TOK" POST /createRoom '{"preset":"public_chat","name":"on synapse","room_alias_name":"onsynapse"}' | jq -r .room_id)
api "$SYN_CLIENT" "$SYN_TOK" PUT "/rooms/$ROOM_S/send/m.room.message/h1" '{"msgtype":"m.text","body":"before the join"}' >/dev/null
JOIN=$(api "$MY_CLIENT" "$MY_TOK" POST "/join/$(enc "#onsynapse:$SYN_NAME")?server_name=$SYN_NAME" '{}')
expect "2 join" "Myelin user joins a Synapse room by alias" "$JOIN" "$ROOM_S"
if [[ "$JOIN" == *"$ROOM_S"* ]]; then
  api "$MY_CLIENT" "$MY_TOK" PUT "/rooms/$ROOM_S/send/m.room.message/m1" '{"msgtype":"m.text","body":"hello from myelin"}' >/dev/null
  wait_event "$SYN_CLIENT" "$SYN_TOK" "$ROOM_S" '.chunk[]|select(.content.body=="hello from myelin")' && record "2 join" PASS "Myelin->Synapse message" || record "2 join" FAIL "Myelin->Synapse message not seen on Synapse"
  api "$SYN_CLIENT" "$SYN_TOK" PUT "/rooms/$ROOM_S/send/m.room.message/s1" '{"msgtype":"m.text","body":"hello from synapse"}' >/dev/null
  wait_event "$MY_CLIENT" "$MY_TOK" "$ROOM_S" '.chunk[]|select(.content.body=="hello from synapse")' && record "2 join" PASS "Synapse->Myelin message" || record "2 join" FAIL "Synapse->Myelin message not seen on Myelin"
  wait_event "$MY_CLIENT" "$MY_TOK" "$ROOM_S" '.chunk[]|select(.content.body=="before the join")' && record "2 join" PASS "history before the join backfilled" || record "2 join" FAIL "pre-join history not on Myelin"
  A=$(api "$SYN_CLIENT" "$SYN_TOK" GET "/rooms/$ROOM_S/joined_members" | jq -c '.joined|keys'); B=$(api "$MY_CLIENT" "$MY_TOK" GET "/rooms/$ROOM_S/joined_members" | jq -c '.joined|keys')
  [ "$A" = "$B" ] && [ "$A" != null ] && record "2 join" PASS "member lists agree: $A" || record "2 join" FAIL "member lists differ: synapse=$A myelin=$B"
fi

# ---- Step 3: Synapse joins a Myelin room ----------------------------------------------------
ROOM_M=$(api "$MY_CLIENT" "$MY_TOK" POST /createRoom '{"preset":"public_chat","name":"on myelin","room_alias_name":"onmyelin"}' | jq -r .room_id)
api "$MY_CLIENT" "$MY_TOK" PUT "/rooms/$ROOM_M/send/m.room.message/h1" '{"msgtype":"m.text","body":"before the join"}' >/dev/null
JOIN=$(api "$SYN_CLIENT" "$SYN_TOK" POST "/join/$(enc "#onmyelin:$MY_NAME")?server_name=$(enc "$MY_NAME")" '{}')
expect "3 join" "Synapse user joins a Myelin room by alias" "$JOIN" "$ROOM_M"
if [[ "$JOIN" == *"$ROOM_M"* ]]; then
  api "$SYN_CLIENT" "$SYN_TOK" PUT "/rooms/$ROOM_M/send/m.room.message/s1" '{"msgtype":"m.text","body":"hello from synapse"}' >/dev/null
  wait_event "$MY_CLIENT" "$MY_TOK" "$ROOM_M" '.chunk[]|select(.content.body=="hello from synapse")' && record "3 join" PASS "Synapse->Myelin message" || record "3 join" FAIL "Synapse->Myelin message not seen on Myelin"
  api "$MY_CLIENT" "$MY_TOK" PUT "/rooms/$ROOM_M/send/m.room.message/m1" '{"msgtype":"m.text","body":"hello from myelin"}' >/dev/null
  wait_event "$SYN_CLIENT" "$SYN_TOK" "$ROOM_M" '.chunk[]|select(.content.body=="hello from myelin")' && record "3 join" PASS "Myelin->Synapse message" || record "3 join" FAIL "Myelin->Synapse message not seen on Synapse"
  wait_event "$SYN_CLIENT" "$SYN_TOK" "$ROOM_M" '.chunk[]|select(.content.body=="before the join")' && record "3 join" PASS "history before the join backfilled" || record "3 join" FAIL "pre-join history not on Synapse"
  A=$(api "$SYN_CLIENT" "$SYN_TOK" GET "/rooms/$ROOM_M/joined_members" | jq -c '.joined|keys'); B=$(api "$MY_CLIENT" "$MY_TOK" GET "/rooms/$ROOM_M/joined_members" | jq -c '.joined|keys')
  [ "$A" = "$B" ] && [ "$A" != null ] && record "3 join" PASS "member lists agree: $A" || record "3 join" FAIL "member lists differ: synapse=$A myelin=$B"
fi

# ---- Step 4: membership, redaction, EDUs, queries, media, directory --------------------------
# Invite Myelin->Synapse (accepted), then leave and rejoin, kick, ban.
read SYN_TOK2 _ <<<"$(reg "$SYN_CLIENT" syncarol)"; read MY_TOK2 _ <<<"$(reg "$MY_CLIENT" mydave)"
SYN_USER2="@syncarol:$SYN_NAME"; MY_USER2="@mydave:$MY_NAME"
ROOM_I=$(api "$MY_CLIENT" "$MY_TOK" POST /createRoom '{"preset":"private_chat"}' | jq -r .room_id)
api "$MY_CLIENT" "$MY_TOK" POST "/rooms/$ROOM_I/invite" "{\"user_id\":\"$SYN_USER2\"}" >/dev/null
R=$(api "$SYN_CLIENT" "$SYN_TOK2" POST "/join/$ROOM_I?server_name=$(enc "$MY_NAME")" '{}'); expect "4 invite" "Myelin invites a Synapse user, who accepts" "$R" "$ROOM_I"
# Invite into Synapse's invite-only room, accepted (rejoin-after-leave of an invite-only room is
# not allowed without a new invite, so leave/rejoin is tested on the public room below).
ROOM_J=$(api "$SYN_CLIENT" "$SYN_TOK" POST /createRoom '{"preset":"private_chat"}' | jq -r .room_id)
api "$SYN_CLIENT" "$SYN_TOK" POST "/rooms/$ROOM_J/invite" "{\"user_id\":\"$MY_USER2\"}" >/dev/null
R=$(api "$MY_CLIENT" "$MY_TOK2" POST "/join/$ROOM_J?server_name=$SYN_NAME" '{}'); expect "4 invite" "Synapse invites a Myelin user, who accepts" "$R" "$ROOM_J"
# Leave, rejoin and kick on a PUBLIC Synapse room, settling each membership across federation
# before the next step so the servers do not resolve state over concurrent branches.
ROOM_P=$(api "$SYN_CLIENT" "$SYN_TOK" POST /createRoom '{"preset":"public_chat","room_alias_name":"leaverejoin"}' | jq -r .room_id)
R=$(api "$MY_CLIENT" "$MY_TOK2" POST "/join/$(enc "#leaverejoin:$SYN_NAME")?server_name=$SYN_NAME" '{}'); expect "4 join-public" "Myelin user joins a public Synapse room" "$R" "$ROOM_P"
wait_membership "$SYN_CLIENT" "$SYN_TOK" "$ROOM_P" "$MY_USER2" join >/dev/null
R=$(api "$MY_CLIENT" "$MY_TOK2" POST "/rooms/$ROOM_P/leave" '{}'); expect "4 leave" "Myelin user leaves the public Synapse room" "$R" "{}"
wait_membership "$SYN_CLIENT" "$SYN_TOK" "$ROOM_P" "$MY_USER2" leave >/dev/null && record "4 leave" PASS "the leave reached Synapse's state" || record "4 leave" FAIL "the leave did not reach Synapse's state"
R=$(api "$MY_CLIENT" "$MY_TOK2" POST "/join/$ROOM_P?server_name=$SYN_NAME" '{}'); expect "4 rejoin" "Myelin user rejoins the public Synapse room" "$R" "$ROOM_P"
wait_membership "$SYN_CLIENT" "$SYN_TOK" "$ROOM_P" "$MY_USER2" join >/dev/null && record "4 rejoin" PASS "the rejoin reached Synapse's state" || record "4 rejoin" FAIL "the rejoin did not reach Synapse's state"
R=$(api "$SYN_CLIENT" "$SYN_TOK" POST "/rooms/$ROOM_P/kick" "{\"user_id\":\"$MY_USER2\"}"); expect "4 kick" "Synapse kicks the Myelin user" "$R" "{}"
wait_membership "$MY_CLIENT" "$MY_TOK2" "$ROOM_P" "$MY_USER2" leave >/dev/null && record "4 kick" PASS "the kick reached Myelin's state" || record "4 kick" FAIL "the kick did not reach Myelin's state"
# Ban the Synapse user from Myelin's room; verify via Synapse's room state (a banned user's own
# /sync is not a reliable place to see the ban).
R=$(api "$MY_CLIENT" "$MY_TOK" POST "/rooms/$ROOM_I/ban" "{\"user_id\":\"$SYN_USER2\"}"); expect "4 ban" "Myelin bans the Synapse user" "$R" "{}"
# A banned user cannot read the room's state endpoint, so look for the ban in the room's leave
# section of the banned user's own /sync.
BAN_OK=0
for _ in $(seq 1 30); do
  M=$(api "$SYN_CLIENT" "$SYN_TOK2" GET "/sync?timeout=0" | jq -r --arg r "$ROOM_I" --arg u "$SYN_USER2" '[.rooms.leave[$r].timeline.events[]?|select(.type=="m.room.member" and .state_key==$u)|.content.membership]|last // empty')
  [ "$M" = ban ] && { BAN_OK=1; break; }
  sleep 1
done
[ "$BAN_OK" = 1 ] && record "4 ban" PASS "the ban reached Synapse (the banned user's leave sync)" || record "4 ban" FAIL "the ban did not reach Synapse"
# Redaction: Synapse redacts its own message in the Myelin room.
EV=$(api "$SYN_CLIENT" "$SYN_TOK" PUT "/rooms/$ROOM_M/send/m.room.message/r1" '{"msgtype":"m.text","body":"to be redacted"}' | jq -r .event_id)
api "$SYN_CLIENT" "$SYN_TOK" PUT "/rooms/$ROOM_M/redact/$(enc "$EV")/rd1" '{"reason":"interop"}' >/dev/null
wait_event "$MY_CLIENT" "$MY_TOK" "$ROOM_M" ".chunk[]|select(.event_id==\"$EV\" and (.content|length)==0)" && record "4 redaction" PASS "Synapse's redaction applied on Myelin" || record "4 redaction" FAIL "redaction of $EV not applied on Myelin"
# Typing and receipts: Myelin -> Synapse.
api "$MY_CLIENT" "$MY_TOK" PUT "/rooms/$ROOM_M/typing/$(enc "$MY_USER")" '{"typing":true,"timeout":30000}' >/dev/null
T=$(wait_sync "$SYN_CLIENT" "$SYN_TOK" ".rooms.join[\"$ROOM_M\"].ephemeral.events[]?|select(.type==\"m.typing\")|.content.user_ids")
expect "4 typing" "Myelin's typing EDU shows in Synapse's /sync" "$T" "$MY_USER"
LAST=$(api "$MY_CLIENT" "$MY_TOK" GET "/rooms/$ROOM_M/messages?dir=b&limit=1" | jq -r '.chunk[0].event_id')
RC_SINCE=$(api "$SYN_CLIENT" "$SYN_TOK" GET "/sync?timeout=0" | jq -r .next_batch)
api "$MY_CLIENT" "$MY_TOK" POST "/rooms/$ROOM_M/receipt/m.read/$(enc "$LAST")" '{}' >/dev/null
T=""
for _ in $(seq 1 20); do
  RESP=$(api "$SYN_CLIENT" "$SYN_TOK" GET "/sync?since=$RC_SINCE&timeout=2000")
  T=$(echo "$RESP" | jq -c ".rooms.join[\"$ROOM_M\"].ephemeral.events[]?|select(.type==\"m.receipt\")|.content" 2>/dev/null)
  [ -n "$T" ] && [[ "$T" == *"$MY_USER"* ]] && break
  RC_SINCE=$(echo "$RESP" | jq -r .next_batch)
done
expect "4 receipt" "Myelin's read receipt shows in Synapse's /sync" "$T" "$MY_USER"
# Device keys: Synapse queries the Myelin user's keys over federation. The keys are uploaded
# under this session's real device id (an upload for any other device id is M_BAD_JSON). Device
# keys over federation are track 08's; this step is here so the whole story runs in one place.
C43=$(printf 'c%.0s' $(seq 1 43)); D43=$(printf 'd%.0s' $(seq 1 43)); E86=$(printf 'e%.0s' $(seq 1 86))
api "$MY_CLIENT" "$MY_TOK" POST /keys/upload "{\"device_keys\":{\"user_id\":\"$MY_USER\",\"device_id\":\"$MY_DEV\",\"algorithms\":[\"m.olm.v1.curve25519-aes-sha2\",\"m.megolm.v1.aes-sha2\"],\"keys\":{\"curve25519:$MY_DEV\":\"$C43\",\"ed25519:$MY_DEV\":\"$D43\"},\"signatures\":{\"$MY_USER\":{\"ed25519:$MY_DEV\":\"$E86\"}}}}" >/dev/null
R=$(api "$SYN_CLIENT" "$SYN_TOK" POST /keys/query "{\"device_keys\":{\"$MY_USER\":[]}}"); expect "4 keys/query" "Synapse's /keys/query of the Myelin user (device keys over federation; track 08)" "$R" '"ed25519:'
# A to-device message is delivered only in an incremental (since-based) /sync, so take a token
# before sending and poll from it.
TD_SINCE=$(api "$SYN_CLIENT" "$SYN_TOK" GET "/sync?timeout=0" | jq -r .next_batch)
R=$(api "$MY_CLIENT" "$MY_TOK" PUT "/sendToDevice/m.fed.test/td1" "{\"messages\":{\"$SYN_USER\":{\"*\":{\"hello\":\"synapse\"}}}}"); expect "4 to-device" "Myelin sends a to-device message to a Synapse user" "$R" "{}"
T=""
for _ in $(seq 1 20); do
  RESP=$(api "$SYN_CLIENT" "$SYN_TOK" GET "/sync?since=$TD_SINCE&timeout=2000")
  T=$(echo "$RESP" | jq -c '.to_device.events[]?|select(.type=="m.fed.test")' 2>/dev/null)
  [ -n "$T" ] && break
  TD_SINCE=$(echo "$RESP" | jq -r .next_batch)
done
expect "4 to-device" "the to-device message is in Synapse's /sync" "$T" "synapse"
# Media uploaded on Myelin, viewed from Synapse (and the other way).
MXC=$(curl -s -X POST "$MY_CLIENT/_matrix/media/v3/upload?filename=a.txt" -H "authorization: Bearer $MY_TOK" -H 'content-type: text/plain' -d 'media from myelin' | jq -r .content_uri)
R=$(curl -s "$SYN_CLIENT/_matrix/client/v1/media/download/${MXC#mxc://}" -H "authorization: Bearer $SYN_TOK"); expect "4 media" "Synapse fetches media uploaded on Myelin" "$R" "media from myelin"
MXC=$(curl -s -X POST "$SYN_CLIENT/_matrix/media/v3/upload?filename=b.txt" -H "authorization: Bearer $SYN_TOK" -H 'content-type: text/plain' -d 'media from synapse' | jq -r .content_uri)
R=$(curl -s "$MY_CLIENT/_matrix/client/v1/media/download/${MXC#mxc://}" -H "authorization: Bearer $MY_TOK"); expect "4 media" "Myelin fetches media uploaded on Synapse" "$R" "media from synapse"
# Public rooms over federation, profile and directory queries.
api "$SYN_CLIENT" "$SYN_TOK" PUT "/directory/list/room/$ROOM_S" '{"visibility":"public"}' >/dev/null
api "$MY_CLIENT" "$MY_TOK" PUT "/directory/list/room/$ROOM_M" '{"visibility":"public"}' >/dev/null
R=$(api "$MY_CLIENT" "$MY_TOK" GET "/publicRooms?server=$SYN_NAME"); expect "4 publicRooms" "Myelin lists Synapse's public rooms" "$R" "$ROOM_S"
R=$(api "$SYN_CLIENT" "$SYN_TOK" GET "/publicRooms?server=$(enc "$MY_NAME")"); expect "4 publicRooms" "Synapse lists Myelin's public rooms" "$R" "$ROOM_M"
api "$MY_CLIENT" "$MY_TOK" PUT "/profile/$(enc "$MY_USER")/displayname" '{"displayname":"Bob of Myelin"}' >/dev/null
R=$(api "$SYN_CLIENT" "$SYN_TOK" GET "/profile/$(enc "$MY_USER")"); expect "4 query/profile" "Synapse reads the Myelin user's profile" "$R" "Bob of Myelin"
api "$SYN_CLIENT" "$SYN_TOK" PUT "/profile/$(enc "$SYN_USER")/displayname" '{"displayname":"Alice of Synapse"}' >/dev/null
R=$(api "$MY_CLIENT" "$MY_TOK" GET "/profile/$(enc "$SYN_USER")"); expect "4 query/profile" "Myelin reads the Synapse user's profile" "$R" "Alice of Synapse"
R=$(api "$SYN_CLIENT" "$SYN_TOK" GET "/directory/room/$(enc "#onmyelin:$MY_NAME")"); expect "4 query/directory" "Synapse resolves a Myelin alias" "$R" "$ROOM_M"
R=$(api "$MY_CLIENT" "$MY_TOK" GET "/directory/room/$(enc "#onsynapse:$SYN_NAME")"); expect "4 query/directory" "Myelin resolves a Synapse alias" "$R" "$ROOM_S"
# Now that the servers have federated, Synapse holds Myelin's keys: its notary serves them (the
# reverse of the step-1 Myelin-notary check). Synapse serves the notary on its TLS federation
# listener, and may need one on-demand fetch, so poll briefly.
NOTARY=""
for _ in $(seq 1 10); do
  NOTARY=$(curl -s --cacert "$WORKDIR/pki/ca.crt" "https://$SYN_NAME/_matrix/key/v2/query/$(enc "$MY_NAME")")
  [[ "$NOTARY" == *'"server_name":"'"$MY_NAME"'"'* ]] && break
  sleep 1
done
expect "4 keys/notary" "Synapse serves Myelin's keys through its notary" "$NOTARY" '"server_name":"'"$MY_NAME"'"'

# ---- Step 5: room versions 10, 11, 12 and a restricted join authorised by Synapse ----------
for v in 10 11 12; do
  RV=$(api "$SYN_CLIENT" "$SYN_TOK" POST /createRoom "{\"preset\":\"public_chat\",\"room_version\":\"$v\",\"room_alias_name\":\"v$v\"}" | jq -r .room_id)
  R=$(api "$MY_CLIENT" "$MY_TOK" POST "/join/$(enc "#v$v:$SYN_NAME")?server_name=$SYN_NAME" '{}'); expect "5 v$v" "Myelin joins a version $v Synapse room" "$R" "$RV"
  RV=$(api "$MY_CLIENT" "$MY_TOK" POST /createRoom "{\"preset\":\"public_chat\",\"room_version\":\"$v\",\"room_alias_name\":\"mv$v\"}" | jq -r .room_id)
  R=$(api "$SYN_CLIENT" "$SYN_TOK" POST "/join/$(enc "#mv$v:$MY_NAME")?server_name=$(enc "$MY_NAME")" '{}'); expect "5 v$v" "Synapse joins a version $v Myelin room" "$R" "$RV"
done
# Restricted: a Synapse space, a restricted room allowing its members; the Myelin user is in the
# space (via Synapse) and joins the restricted room with Synapse as the authorising server.
SPACE=$(api "$SYN_CLIENT" "$SYN_TOK" POST /createRoom '{"preset":"public_chat","creation_content":{"type":"m.space"},"room_alias_name":"space"}' | jq -r .room_id)
RESTRICTED=$(api "$SYN_CLIENT" "$SYN_TOK" POST /createRoom "{\"room_version\":\"10\",\"initial_state\":[{\"type\":\"m.room.join_rules\",\"state_key\":\"\",\"content\":{\"join_rule\":\"restricted\",\"allow\":[{\"type\":\"m.room_membership\",\"room_id\":\"$SPACE\"}]}}],\"room_alias_name\":\"restricted\"}" | jq -r .room_id)
api "$MY_CLIENT" "$MY_TOK" POST "/join/$(enc "#space:$SYN_NAME")?server_name=$SYN_NAME" '{}' >/dev/null
R=$(api "$MY_CLIENT" "$MY_TOK" POST "/join/$(enc "#restricted:$SYN_NAME")?server_name=$SYN_NAME" '{}'); expect "5 restricted" "Myelin user joins a restricted Synapse room via the space" "$R" "$RESTRICTED"

echo; echo "== results ($RESULTS)"; column -t -s $'\t' "$RESULTS" 2>/dev/null || cat "$RESULTS"
echo "== logs: $WORKDIR/myelin/hs.log, docker logs fed-synapse-synapse"
grep -q $'\tFAIL\t' "$RESULTS" && exit 1 || exit 0
