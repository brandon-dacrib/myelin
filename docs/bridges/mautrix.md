# A mautrix bridge, added through the interface: mautrix-whatsapp, verified 2026-09-25

The first mautrix bridge ever pointed at this server, and the first bridge added the way an
operator is meant to add one: through the interface's wizard, started from the files the wizard
produced, with nothing generated or edited by hand. Two runs, the second on the binary with the
one server fix the first one found. About twelve seconds from "Create bridge" to the Created
page saying **Connected**.

The reproduction is a Playwright spec against the real binary,
`web/e2e-real/add-mautrix-bridge.spec.ts`; the screenshots it took are
`docs/design/screenshots/bridge-*-real-whatsapp.png`.

## What was verified

1. **The wizard, WhatsApp chosen.** Identity came from the catalogue (`whatsapp`, bot
   `whatsappbot`, ghosts `@whatsapp_.*:test.local`). Deployment was told where each side is,
   which is the one thing a first try on a laptop needs: the server as
   `http://host.docker.internal:8008` (the bridge is in Docker, the server is not) and the
   bridge as `http://127.0.0.1:29318` (its published port). Options: double puppeting,
   encryption and rate-limit exemption on, the operator prefilled as the bridge's administrator.
2. **Review** showed `registration.yaml` and `config.yaml`, both carrying the same two freshly
   minted tokens; **Create** registered the appservice with the server at once.
3. **The Created page's files** were saved into a directory exactly as shown, and
   `docker run -d -p 29318:29318 -v $DIR:/data dock.mau.dev/mautrix/whatsapp:latest` started.
   The bridge's own first-run script found both files and ran the bridge straight away (with
   either missing it generates one and exits, which is the manual path the mautrix
   documentation describes).
4. **The bridge accepted the wizard's config.** It completed `config.yaml` from its own defaults
   on first start, as its config upgrader is designed to: 40 lines in, 666 out, with
   `provisioning.shared_secret` and `encryption.pickle_key` generated and every section the
   wizard did not write (`matrix`, `provisioning`, `direct_media`, `public_media`, `logging`,
   `analytics`) filled in. So the wizard only has to write what ties the bridge to this server.
5. **The bridge reached the server and the server reached the bridge.** In its log, in order:
   `/versions` and `/whoami` as the bot; a ping of itself through the server (MSC2659,
   `POST /_matrix/client/v1/appservice/whatsapp/ping`, the server calling
   `/_matrix/app/v1/ping` on the bridge); "Homeserver -> appservice connection works"; its bot
   device created without a login (MSC4190, `PUT /_matrix/client/v3/devices/{id}` → 200); a key
   query with device masquerading (`org.matrix.msc3202.device_id`); "End-to-bridge encryption is
   in appservice mode, registering event listeners and not starting syncer"; `/media/config`;
   the bot's profile; "Bridge started".
6. **The server's side agrees.** The admin API reported the appservice `healthy` with a ping
   just now; the users list attributed `@whatsappbot:test.local` to the bridge
   (`appservice_id: whatsapp`); the bridge's page said Healthy, the Sign in tab said what to
   send to the bot and what to do on the phone, from the catalogue.

**Not verified:** signing in, which needs a phone with WhatsApp (the steps are on the page and
at <https://docs.mau.fi/bridges/go/whatsapp/authentication.html>), and therefore any message
in either direction. ~~The MSC3202 transaction fields and MSC4203 to-device delivery have been
*asked for* by a real bridge and set up on both sides; they have not yet carried an encrypted
conversation.~~ Since 2026-09-30 the bridge is sent them (below); an encrypted conversation
still needs the phone.

## 2026-09-30: the bridge receives device lists, key counts, ephemeral events and to-device messages

Until this day the server sent a bridge events only (`docs/next-steps.md`, "Appservice delivery
carries events only"). The same bridge image (`dock.mau.dev/mautrix/whatsapp:latest`,
`v26.09+dev.a0325e76`, mautrix-go `v0.31.0+dev.4aac2bbf`, pulled from mau.dev; Docker Hub is
not involved) was run again on the binary of branch `agent/as-ephemeral`, from the files of a
`mautrix-whatsapp` offering's instance for `@alice:test.local` (`PUT /api/v1/bridge-offerings/
mautrix-whatsapp` with `runtime: elsewhere`, `PUT .../instances/@alice:test.local`, `POST
.../files`, the registration's `url` patched to `http://127.0.0.1:29318`, `docker run -d -p
29318:29318 -v $DIR:/data`), the server at `public_baseurl: http://host.docker.internal:8018`.
The registration the offering rendered asks for `de.sorunome.msc2409.push_ephemeral`,
`org.matrix.msc3202` and `io.element.msc4190`; the config has `encryption.appservice: true`.
In the bridge's log, in order (`Starting handling of transaction content=...` is mautrix-go's
own count of what a transaction carried; `unstable_edu` is what it calls the legacy
`de.sorunome.msc2409.ephemeral` key, which is the one this registration asked for):

1. Start as before: `/versions`, `/register` as the bot, the ping (the first lost the race
   with its own listener, the retry worked), "Creating bot device with MSC4190", `/keys/upload`
   with 51 one-time keys, "End-to-bridge encryption is in appservice mode, registering event
   listeners and not starting syncer", "Added listeners for encryption data coming from
   appservice transactions", "Bridge started".
2. **Transaction 3, 200 ms after its key upload:**
   `{"device_changes":1,"fallback_key_users":1,"otk_count_users":1,"pdu":0,...}`, then the
   crypto component: "Device list changes in /sync changes=["@whatsappbot_alice:test.local"]",
   "Finished handling device list changes". Its own device list change and its own one-time-key
   count and fallback key type, from the server's device-list stream and key store (MSC3202).
   From here every transaction carried `otk_count_users: 1, fallback_key_users: 1`.
3. Alice joined the bot's DM, typed, sent a message, sent a read receipt, set her presence
   and sent an `m.room_key_request` to the bot's device (`IEXNEKZESJ`) with `/sendToDevice`.
   **Transactions 4 to 11**: `unstable_edu: 1` for her presence (three times: her `/sync` put
   her online, her join restamped it, then `unavailable`), the typing and the receipt; `pdu: 1`
   for the join and the message; and **transaction 11 `{"to_device":1}`** followed by "Starting
   handling to-device event component=crypto sender=@alice:test.local type=m.room_key_request"
   and "Finished handling to-device event". The server's side of the same, from its log and
   `/metrics`: `delivered a transaction to an appservice appservice=whatsapp-alice txn_id=11
   ... to_device=1 ... one_time_key_counts=1 fallback_key_types=1`;
   `hs_appservice_transactions_total{appservice="whatsapp-alice",outcome="delivered"} 11`;
   `hs_appservice_delivered_items_total{...,kind="to_device"} 1`, `kind="typing"} 1`,
   `kind="receipts"} 1`, `kind="presence"} 3`, `kind="device_list_changes"} 1`,
   `kind="one_time_key_counts"} 9`, `kind="events"} 11`.

So the bridge's Olm machine now receives what appservice-mode encryption needs from this
server. What it has still not done is decrypt or encrypt a room message: that needs a signed-in
WhatsApp account, and a phone. The bridge's avatar fetch from `maunium.net` got a 502 (no
federation on that test server), which is unrelated and harmless.

## 2026-10-01: the server asks the bridge who has signed in

The render now writes a `provisioning.shared_secret` of its own into `config.yaml` and keeps the
same value in the registration (`io.myelin.provisioning_secret`), so `GET
/api/v1/appservices/{id}/logins?user_id=` can ask the bridge's provisioning API
(`GET /_matrix/provision/v3/whoami` on its appservice listener, bearer the secret) and the Sign in
tab can say "Signed in as +1 555… since …" or "not signed in". Branch `agent/bridge-logins`;
the design is in `docs/status/11-appservices-and-bridges.md` (2026-10-01).

Run against the same image (`dock.mau.dev/mautrix/whatsapp:latest`, `v26.09+dev.a0325e76`) on
that branch's debug binary: the files of a `POST /bridge-types/mautrix-whatsapp/render`
(`bridgeAddress: http://127.0.0.1:29399`, the server at `http://host.docker.internal:18731`),
registered with `POST /appservices`, the container started with `-p 29399:29318`. The bridge's
config upgrader completed `config.yaml` (40 lines to 666) and **kept the rendered secret**
(line 439, the same 64 hex characters as the registration's). Nobody signed in, so:

```
GET /api/v1/appservices/whatsapp/logins?user_id=@ops:test.local
{"appservice_id":"whatsapp","bridge_type":"mautrix-whatsapp","provisioning_api":"mautrix_v3",
 "supported":true,"reason":null,"user_id":"@ops:test.local","signed_in":false,"logins":[],
 "checked_at":"2026-10-01T06:51:55.727Z","cached":false,"error":null}
```

Asked again at once, `cached: true`; `/metrics` had
`hs_admin_bridge_login_queries_total{type="mautrix-whatsapp",outcome="answered"} 1` and
`outcome="cached"} 1`. With the container stopped, the same question about another user was a
`200` with `error: {status: 502, reason: "unreachable", ...}` and the server logged `WARN ...
could not ask a bridge who has signed in appservice=whatsapp ... reason=unreachable`. A login
with a phone (and so the `logins[]` entries of a real WhatsApp account) has not been seen; the
shape is mautrix-go's `RespWhoami`, which `hs_admin::bridge_logins::answered` reads and the
stand-ins in the tests serve.

A bridge registered before this (the demo's, from 2026-09-25) has a secret the bridge generated
itself; the answer says so, and copying `provisioning.shared_secret` from its `config.yaml` into
the registration with `PATCH /api/v1/appservices/{id}` `{"io.myelin.provisioning_secret": "..."}`
makes it answer.

## What the bridge's first minute found

- **The first ping lost a race, and the server kept the loss.** A mautrix bridge pings the
  moment its listener starts. The first attempt in the second run arrived before the port was
  open: the server answered 502 (`M_CONNECTION_FAILED`, connection refused), the bridge logged
  "Homeserver -> appservice connection is not working, retrying in 5 seconds", retried, and the
  retry worked. Ordinary, and Synapse sees the same. What was not ordinary: the server then
  reported the appservice `healthy` *and* kept the failed attempt in `last_error`, so the
  bridge's page would have shown a healthy bridge under a red error banner. A ping that works
  now clears the error (`hs_appservice::ping::PingService::record`, with a test:
  `a_ping_that_works_clears_the_error_from_the_one_before`).
- `host.docker.internal` reaches a server on the host from OrbStack and Docker Desktop; on
  Linux it needs `--add-host host.docker.internal:host-gateway`. The wizard's hint says so in
  fewer words.

## What the mautrix documentation taught the wizard

Read on 2026-09-25 from <https://docs.mau.fi/bridges/> and the `bridgev2` example config in
`mautrix/go`; this is the part of running a bridge the documentation spends most of its pages
on, and the wizard now does or says it.

- **A bridge needs two files, and they must agree.** The registration (for the server) and
  `config.yaml` (for the bridge) share `id`, the two tokens, the bot's localpart, the
  ephemeral-events flag and the encryption features. The manual flow is: run once to generate
  the config, edit six fields, run again to generate the registration, register it, run. The
  wizard writes both from one set of choices, so there is nothing to keep in step.
- **What the config has to say.** `homeserver.address` and `.domain`; `appservice.address`
  (the registration's `url`), `.hostname`, `.port`, `.id`, `.bot.username`,
  `.username_template` (`whatsapp_{{.}}`), `.ephemeral_events`, the tokens; `database`
  (SQLite in the bridge's volume for Compose, Postgres for Kubernetes); `bridge.permissions`
  (`*: relay`, the server's domain as `user`, the administrator as `admin`);
  `backfill.enabled`; `double_puppet.secrets`; `encryption.allow/default/appservice/msc4190`.
  Everything else keeps the bridge's default and the bridge writes it in.
- **Double puppeting through the bridge's own token.** The documentation's shape is a separate
  `doublepuppet.yaml` registration with `url: null` and a non-exclusive `@.*:domain` claim,
  its `as_token` placed in every bridge's `double_puppet.secrets`. The wizard does the same
  with one registration instead of two: the bridge's own registration gets the non-exclusive
  claim, and its own `as_token` goes into its `double_puppet.secrets`. The server allows
  `m.login.application_service` as any user a non-exclusive namespace covers, which is all the
  shared registration provided. One fewer secret to rotate; the Options step says exactly this
  rather than promising a shared registration nothing creates.
- **Encryption in appservice mode.** With `encryption.appservice: true` and `msc4190: true`
  the bridge expects device lists and to-device messages inside its transactions (MSC3202,
  MSC4203) and creates its device with a `PUT` instead of a login (MSC4190); that is what the
  registration's `org.matrix.msc3202` and `io.element.msc4190` flags ask this server for, and
  the wizard only sets `appservice: true` when it set those. Synapse needs experimental flags
  for the same; here they are on whenever a registration asks.
- **Signing in is a conversation with the bot, and it differs per network.** WhatsApp:
  `login qr` or `login phone`, then Linked devices on the phone. Signal: `login`, then Linked
  devices. Telegram: `login phone +number`, a code from another Telegram client, an optional
  two-factor password; or `login qr`; or `login bot <token>`. Discord: `login-qr`, or
  `login-token user|bot <token>`. Slack: `login token <xoxc-…> <xoxd-…>` or `login app`.
  Google Messages: `login google` with a request copied as cURL, then an emoji tapped on the
  phone (QR pairing is gone). Messenger and Instagram: `login messenger|facebook|instagram`
  with a `graphql` request copied as cURL. The catalogue carries each as numbered steps with
  the caveats the documentation gives (a phone offline for two weeks loses WhatsApp; Discord
  may flag accounts that look automated; Meta may ask for a checkpoint), and the interface
  substitutes the bot's real Matrix ID. The interface does not try to do the login itself:
  `PLAN.md` section 8.4 keeps it where the bridges put it.

## Reproducing it

From the repository root, with Docker running and `cargo build -p hs-cli --bin hs` done.
About a minute, most of it the image pull. The file below is the bootstrap a test server needs
(decision 0010); its `media` and `rate_limits` lines only seed the database on the first start,
and on a real server they are set in the interface's Configuration section. The bridge itself is
added through the interface, never in the file.

```sh
D=$(mktemp -d); mkdir -p $D/wa && chmod 777 $D/wa
cat > $D/homeserver.yaml <<YAML
server: { server_name: test.local, public_baseurl: http://127.0.0.1:8008 }
listeners: { listeners: [ { port: 8008, bind_addresses: ["0.0.0.0"], resources: [client, admin, health, metrics] } ] }
storage: { backend: embedded, data_dir: "$D/data" }
media: { storage: { backend: local, path: "$D/media" } }
rate_limits: { enabled: false }
YAML
(cd $D && target/debug/hs serve -c $D/homeserver.yaml > $D/hs.log 2>&1 &); sleep 6
TOKEN=$(grep -o 'setup_link=[^ ]*' $D/hs.log | tail -1 | sed 's/.*#token=//')
ADMIN=$(curl -s -X POST http://127.0.0.1:8008/api/v1/setup -H 'content-type: application/json' \
  -d "{\"setup_token\":\"$TOKEN\",\"username\":\"ops\",\"password\":\"opspassword123\"}" \
  | python3 -c 'import sys,json; print(json.load(sys.stdin)["access_token"])')
cd web && HS_REAL_SERVER_URL=http://127.0.0.1:8008 HS_REAL_ADMIN_TOKEN=$ADMIN \
  HS_REAL_BRIDGE_DIR=$D/wa HS_REAL_BRIDGE_RUN=1 \
  npx playwright test --config playwright.real.config.ts e2e-real/add-mautrix-bridge.spec.ts
```

The spec walks the wizard in a browser, saves the two files, runs the container, waits for the
Created page to turn green, opens the bridge and its Sign in tab, and removes the container.
Its screenshots land in `web/test-results/`. To keep the bridge running and sign in for real,
run the same `docker run` it prints, then follow the Sign in tab.
