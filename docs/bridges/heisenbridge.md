# A real bridge: heisenbridge, verified 2026-09-22

The first real bridge ever pointed at this server. It is the IRC bouncer bridge most people
run, it is one container, and it needs nothing external until you tell it which IRC network to
join -- so it exercises the whole of the appservice surface a bridge touches on its first day,
with nothing to sign up for.

What was verified, against the real binary, with a local IRC server (`ergo`) in a second
container:

1. The bridge registers its bot with `POST /register` (`type: m.login.application_service`,
   `inhibit_login`), on a server with registration closed. **This did not work when the bridge
   was first started**: `/register` had no appservice branch and answered "Registration is
   disabled". A bridge could not create its own bot on a default server. Fixed the same hour.
2. It writes and reads its configuration as the bot's account data, lists the bot's joined rooms,
   creates a control room, invites the owner, joins it, and posts a welcome. Every request is
   masqueraded (`?user_id=`) with the `as_token`.
3. Every event in every room the bot is in reaches it as a transaction
   (`PUT /_matrix/app/v1/transactions/{txnId}` with the `hs_token`), and it answers commands:
   `HELP`, `ADDNETWORK`, `ADDSERVER`, `OPEN`, `CONNECT`, `JOIN #channel`.
4. A message on IRC arrives in Matrix from a ghost the bridge registered in its `@irc_.*`
   namespace and joined via masquerade (the ghost's first join of an invite-only room is refused,
   the bot invites it, it joins -- the bridge's ordinary flow). A message in Matrix arrives on IRC
   as the owner's nick.
5. The admin API's user list attributes the bot and the ghost to the bridge (`appservice_id`).
6. The *server* restarted underneath the running bridge: a new IRC user then arrived in Matrix as
   a new ghost, and a Matrix message reached IRC. (This is what found the two restart bugs
   described in `docs/next-steps.md`; it now works.)

Nothing else in the bridge's log was a server problem: a repeated bot registration is answered
`M_USER_IN_USE`, which every mautrix bridge expects; a probe for Synapse's own admin endpoint
is a 404; the first read of not-yet-written account data is a 404.

Not verified: the bridge's media path (`--media-proxy`), identd, a second Matrix user in a
bridged channel, and a restart of the *bridge itself* (its state lives in the bot's account
data, which the server keeps, so it should come back; it has not been watched doing so).

## Reproducing it

From the repository root, with Docker running. About five minutes.

```sh
cargo build -p hs-cli --bin hs
D=$(mktemp -d); mkdir -p $D/hb && chmod 777 $D/hb

# 1. The bridge's registration. The homeserver URL is how the bridge reaches the server from
#    inside Docker; the owner is the one Matrix account allowed to drive it.
docker run --rm -v $D/hb:/data hif1/heisenbridge -c /data/heisenbridge.yaml --generate \
  -l 0.0.0.0 -p 9898 -o @brandon:test.local http://host.docker.internal:8008
# The server's copy points at the port Docker maps; the bridge's own copy keeps 0.0.0.0.
sed 's#url: http://0.0.0.0:9898#url: http://127.0.0.1:9898#' $D/hb/heisenbridge.yaml > $D/heisenbridge-for-hs.yaml

# 2. The server, with the registration in its configuration.
cat > $D/homeserver.yaml <<YAML
server:
  server_name: test.local
  public_baseurl: http://127.0.0.1:8008
listeners:
  listeners:
    - port: 8008
      bind_addresses: ["0.0.0.0"]
      resources: [client, admin, health, metrics]
storage:
  backend: embedded
  data_dir: "$D/data"
media:
  storage:
    backend: local
    path: "$D/media"
rate_limits:
  enabled: false
appservices:
  registration_files: ["$D/heisenbridge-for-hs.yaml"]
YAML
target/debug/hs serve -c $D/homeserver.yaml > $D/hs.log 2>&1 &
sleep 5

# 3. The first administrator, from the setup link the server logged, and the owner account.
TOKEN=$(grep -o 'setup_link=[^ ]*' $D/hs.log | tail -1 | sed 's/.*#token=//')
ADMIN=$(curl -s -X POST http://127.0.0.1:8008/api/v1/setup -H 'content-type: application/json' \
  -d "{\"setup_token\":\"$TOKEN\",\"username\":\"ops\",\"password\":\"opspassword123\"}" \
  | python3 -c 'import sys,json; print(json.load(sys.stdin)["access_token"])')
curl -s -X POST http://127.0.0.1:8008/api/v1/users -H "authorization: Bearer $ADMIN" \
  -H 'content-type: application/json' \
  -d '{"localpart":"brandon","password":"brandonpassword123","display_name":"Brandon"}'

# 4. The bridge, and an IRC server for it to talk to.
docker run -d --name heisenbridge -p 9898:9898 -v $D/hb:/data hif1/heisenbridge \
  -c /data/heisenbridge.yaml -l 0.0.0.0 -p 9898 -o @brandon:test.local -vv http://host.docker.internal:8008
docker run -d --name ergo -p 6667:6667 ghcr.io/ergochat/ergo:stable
docker logs heisenbridge 2>&1 | grep -E "bridge is now running|ERROR"
```

Then, as `brandon` (Element against `http://127.0.0.1:8008`, or `curl` with a login token):
accept the invitation from `@heisenbridge:test.local`, and in that room say `ADDNETWORK ergo`,
`ADDSERVER ergo host.docker.internal 6667`, `OPEN ergo`; accept the new invitation; there,
`CONNECT` and `JOIN #myelin`; accept the channel room's invitation. Anybody on IRC
(`nc 127.0.0.1 6667`, then `NICK carol`, `USER carol 0 * :carol`, `JOIN #myelin`,
`PRIVMSG #myelin :hello`) now appears in that room as `@irc_ergo_carol:test.local`, and what
you say there reaches them.

Afterwards: `docker rm -f heisenbridge ergo`, stop the server, `rm -rf $D`.
