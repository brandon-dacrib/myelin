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

## 2026-10-02: a person types `login qr` to their own bot, and the chat is encrypted

The first time anyone typed to a personal bot through this server, and it did nothing. The
reproduction is now a test, `crates/hs-bridge-conformance/tests/real_mautrix_login.rs`: the real
binary, this image (`dock.mau.dev/mautrix/whatsapp:latest`, `v26.09+dev.a0325e76`) from an
offering instance's files, and alice on `matrix-sdk` with encryption, twice (the chat encrypted,
the offering's default, and in the clear). Before the fix the bridge decrypted `login qr`
(`Event decrypted successfully decrypted_event_type="m.room.message (message)"`) and dropped it:
the chat had been started by its bot, and a mautrix bridge only takes bare commands in a room the
person invited the bot into (its management room). `!wa login qr` in the same chat got "⚠️ This
is not your management room. Entering login info must be prefixed with `!wa` like other
commands." and then the QR. The manager now starts the chat as the person (double puppeting)
with the bot invited; the bridge accepts (`Accepted invite to room as bot`) and marks it, and
`login qr` is `Received command mx_command=login`, `Received QR codes code_count=6`, "Scan the QR
code with the WhatsApp mobile app to log in" and an `m.image` of the code, all of it encrypted
and decrypted by alice's client in the encrypted chat. Details and the owner's way out of an
existing chat: `docs/status/11-appservices-and-bridges.md`, 2026-10-02.

Running it: `cargo build -p hs-cli --bin hs`, then `cargo test -p hs-bridge-conformance --test
real_mautrix_login -- --nocapture` with Docker reachable (it pulls the image from mau.dev if it
is missing, and skips, saying why, when it cannot). `HS_BRIDGE_LOGIN_LOG_DIR=<dir>` keeps the
server's and the bridge's logs; `HS_BRIDGE_LOGIN_TEXT='!wa login qr'` types something else.

## 2026-10-02, later: a chat the bot started is repaired in place

The demo cluster's bridge for `@brandon` was made before the fix above, and its chat was the
bot's: eight messages typed there were each decrypted by the bridge (`Event decrypted
successfully`) and dropped, with the bridge healthy, its keys fine and every transaction
delivered. Nothing in the server could see that; the bridge's log could. So the manager now
settles every owned instance's chat (`hs_bridges::manager::BridgeManager::settle_chat`): it
reads who created the room, and where the bot did and the instance may act as the owner
(double puppeting), **the bot leaves and the owner re-invites it**, which is the one thing
`bridgev2` takes as "mark this room as the person's management room"
(`handleBotInvite`: an invitation for the bot from someone allowed commands, accepted, two
members). The bridge says "This room has been marked as your management room", the bot says
why it had been silent and lists the steps again, and `login qr` works in the same chat. Where
the instance cannot act as the owner, the bot says once that commands there need its prefix
(`!wa login qr`), or a chat the person starts; the bridge page says the same, and the admin
API carries `chat_room`, `chat_started_by` and the type's `command_prefix`.

The third story in `crates/hs-bridge-conformance/tests/real_mautrix_login.rs` is that chat:
made with `double_puppeting: false` so the bot starts it, then the instance allowed to act as
alice (the registration's claim on her, and the option), the repair, the bridge's own
"management room" line decrypted by her client, `Accepted invite to room as bot` in the
bridge's log, and the QR. Status 11 has the cluster's evidence and what the owner types.

The command prefixes the catalogue knows (from each bridge's `pkg/connector/connector.go`):
WhatsApp `!wa`, Telegram `!tg`, Google Messages `!gm`, Google Voice `!gv`, Messenger and
Instagram `!fb`, Discord `!discord`, Bluesky `!bsky`. Signal, Slack and X set none in their
connector, so none is claimed for them.

## 2026-10-02, evening: the bridge has a name in WhatsApp's Linked devices

The owner's bridge showed up on the phone as "other device", platform "Other device". Branch
`agent/bridge-device-names`.

**Where the two strings come from.** In mautrix-whatsapp (the `bridgev2` connector the
catalogue's image `dock.mau.dev/mautrix/whatsapp:latest` runs) they are the config's
`network.os_name` and `network.browser_name` (`pkg/connector/config.go`, `OSName`/`BrowserName`;
the example config says "Device name that's shown in the 'WhatsApp Web' section" and "Browser
name that determines the logo"; defaults `Mautrix-WhatsApp bridge` and `unknown`).
`connector.go` copies them into whatsmeow's `store.DeviceProps.Os` and, after uppercasing and a
lookup in `DeviceProps_PlatformType_value`, `.PlatformType`. whatsmeow marshals `DeviceProps`
into the client payload's `DevicePairingRegistrationData` **only when the device has no ID yet**
(`store/clientpayload.go`, `getRegistrationPayload`); a linked device's `getLoginPayload`
carries nothing of it. So WhatsApp learns both strings when the bridge is linked and never
again. The pairing-code route is unaffected by either: `login.go` calls `PairPhone(...,
whatsmeow.PairClientChrome, "Chrome (Linux)")`, which is what the phone's "link with phone
number" prompt says before the link exists; the linked device afterwards is named from
`DeviceProps`.

**What each platform shows.** `PlatformType` (whatsmeow `proto/waCompanionReg/WACompanionReg.proto`)
is `UNKNOWN`, `CHROME`, `FIREFOX`, `IE`, `OPERA`, `SAFARI`, `EDGE`, `DESKTOP`, `IPAD`,
`ANDROID_TABLET`, `OHANA`, `ALOHA`, `CATALINA`, `TCL_TV`, `IOS_PHONE`, `IOS_CATALYST`,
`ANDROID_PHONE`, `ANDROID_AMBIGUOUS`, `WEAR_OS`, `AR_WRIST`, `AR_DEVICE`, `UWP`, `VR`, `CLOUD_API`.
What the phone makes of them is not in any source; it is in whatsmeow's own discussion #469 and
issue #89, which match what the owner saw: with `UNKNOWN` the phone **ignores the os name** and
shows "Other device"; with a browser it shows "Google Chrome (os name)" and that browser's logo;
with `DESKTOP` it shows **the os name by itself**, with the desktop app's icon. `DESKTOP` is
therefore the one constant, `hs_admin::bridge_types::WHATSAPP_PLATFORM`. The QR code's client
type does not follow it (`pair.go`, `getQRClientType` has no `DESKTOP` arm and falls through to
"other web client"), which changes nothing visible. Neither whatsmeow nor the bridge limits the
name's length; the phone's list cuts long names short, which is why the name below puts the
person before the server.

**The name, in one place.** `hs_admin::bridge_types::device_name(type, server, owner)`:
`Myelin WhatsApp bridge for brandon (myelin.dacrib.net)` for a person's instance,
`Myelin WhatsApp bridge (myelin.dacrib.net)` for a shared one (and for the wizard's hand-run
bridge). Every mautrix instance's `config.yaml` now carries a `network:` section where the
connector has a setting for it (read from each bridge's `pkg/connector/config.go` and
`example-config.yaml` the same day):

| Bridge | Key | Sent to the network |
|---|---|---|
| WhatsApp | `network.os_name`, `network.browser_name: DESKTOP` | at linking only |
| Signal | `network.device_name` ("Default device name that shows up in the Signal app") | at linking only (`signalmeow/provisioning.go`, encrypted) |
| Telegram | `network.device_info.device_model` (its Devices list) | on every connection |
| Google Messages | `network.device_meta.os` ("the name that shows up in the paired devices list"; `browser` and `type` keep the defaults) | at pairing only |
| Messenger and Instagram, Discord, Slack, X, LinkedIn, Bluesky, Google Voice | none: they sign in with cookies, tokens or an app password and list no device the bridge names | — |

The admin API's `BridgeInstance` says the name (`device_name`, null for a kind without one),
and the person's next-steps box in the interface shows it with one line saying it is what the
network's own device list will call the bridge, and that an earlier link keeps its old name.

**The Matrix side is the bridge's own.** The bot's device is named by mautrix-go, not by the
config: `bridgev2/matrix/crypto.go` builds `"<network> bridge"` ("WhatsApp bridge") and passes it
both to `CreateDeviceMSC4190` and to the login it falls back to. Nothing in `config.yaml` sets
it. The double-puppet intent in `as_token` mode (`bridgev2/matrix/doublepuppet.go`) logs nothing
in and so names no device. The manager could rename the bot's device through
`PUT /_matrix/client/v3/devices/{id}` with the instance's own token once it has read the device
ID from the bot's device list; it does not, since "WhatsApp bridge" already says what it is and
a client lists it under the bot, whose name already carries the person.

**A changed config now reaches a running bridge.** Until this day nothing did: an offering's new
image tag or options were rendered for new instances only, and the operator's init container
never replaced a file already in `/data` (the bridge rewrites `config.yaml` with a generated
`encryption.pickle_key`; losing it makes its crypto store unreadable). Now:

1. The manager mints `pickle_key` with the instance's tokens and renders it into
   `encryption.pickle_key`, so the rendered file is complete; an instance from before gets one
   on its next step.
2. The manager keeps a fingerprint of what it last applied (image, port, arguments, every
   rendered file) on the instance row and, on every step of a deploying, starting or ready
   instance, applies the deployment again when the fingerprint differs, moving the instance to
   `deploying` with "its configuration changed: restarting the pod with it" and watching it
   come back. An instance from before this (no fingerprint) is applied once.
3. `hs-operator`'s client annotates the `Bridge` with `myelin.dev/files-hash`; the operator folds
   it into the pod template's `myelin.dev/spec-hash`, so new files in the Secret roll the pod.
4. The init container writes every file from the Secret over `/data`'s copy, carrying the
   values of `pickle_key`, `signing_key` and `server_key` from the bridge's copy into the new
   one (a `generate` placeholder is not carried). From here the Secret is the config: an edit
   made by hand inside the pod is gone on the next roll.

Verified with the unit tests named in status 11 and the operator's test that runs the real
script with `/bin/sh`; on a cluster this has not run yet (unreachable from sessions today).

**What the owner will see.** The demo instance rolls once when this ships (its pod restarts
with the new config, its pickle key carried over, its WhatsApp session and its chat with the bot
untouched). The phone keeps calling the existing link "Other device": WhatsApp learnt that name
at pairing and is never told again. To see `Myelin WhatsApp bridge for brandon
(myelin.dacrib.net)` with a desktop icon, log the bridge out on the phone (Linked devices, the
device, Log out) or with `logout` in the bot's chat, then `login qr` again; chats carry on under
the new link. The page and this document say the same.

## 2026-10-03: the owner's Element refused to share keys with the bridge, so the bot is now cross-signed

In the repaired chat on the demo, the owner typed and the bot answered "⚠️ Your message was
not bridged: your client refused to share decryption keys with the bridge". That is
mautrix-go's wording (`bridgev2/matrix/cryptoerror.go`, `errorToHumanMessage`) for an
`m.room_key.withheld` from the person's client, whatever the code but `m.unverified` is the one
here: the bot's device had no cross-signing identity behind it, and a client that excludes
insecure devices shares a room's keys only with devices signed by their owner's self-signing key
(`matrix-sdk-crypto`'s `CollectStrategy::IdentityBasedStrategy`: "if a user has no published
identity he will not receive any room keys"; Element Web's Labs flag "Exclude insecure devices
when sending/receiving messages", off by default in Element Web; Element X's invisible crypto;
MSC4153). Branch `agent/bridge-bot-verified`.

**What mautrix does by itself.** `encryption.self_sign: true` (`bridgev2/matrix/crypto.go`,
`doSelfSign`) has the bot generate a recovery key and SSSS, upload its master, self-signing and
user-signing keys with `POST /keys/device_signing/upload` and no UIA callback (a `401` is fatal),
and sign its own device and master key. It keeps the recovery key in its own database
(`kv_store.recovery_key`) and exits (34: "Server already has cross-signing keys, but no key in
database") whenever the server has the bot's keys and the database does not: an instance
recreated for the same owner (the manager's `remove` keeps the bot user, and no client API
deletes cross-signing keys), a reset bridge database. The example config's note says as much.

**What this server does instead.** The manager keeps the bot's identity
(`hs_bridges::cross_signing`, decision `docs/decisions/0027-the-manager-cross-signs-each-bridge-bots-device.md`):
on an instance's first ready step it mints master and self-signing keys, stores the seeds on the
instance row beside the pickle key, publishes the public keys with
`POST /keys/device_signing/upload` as the appservice masquerading as the bot (no user-interactive
auth for an appservice, MSC4190; `hs-e2e` already did this, the same rule as Synapse's), and
signs the bot's device (`POST /keys/signatures/upload`) with the self-signing key. It looks again
every minute and signs a device the bridge made since; an instance recreated for the same owner
replaces the keys. `config.yaml` is unchanged (`self_sign` stays at its default, off); mautrix-go
tolerates its own user's keys on the server when it does not self-sign (`crypto/devicelist.go`
only compares them). The server announces a device-list change on both uploads, so a client
already tracking the bot (the owner's) re-fetches the identity and the signature.

**What the server now says about a refusal.** The scheduler keeps the last
`m.room_key.withheld` it delivers to an appservice's device in the appservice's health
(`AppServiceHealth.last_key_withheld`, `BridgeInstance.last_key_withheld`: when, who, the code,
the reason, the room, the device), logs it at `WARN` ("a client withheld a room's keys from an
appservice's device") and counts `hs_appservice_key_withheld_total{appservice,code}`. The bridge
page and the person's instance page show it beside what the bridge said in the chat, with what it
means and what happens next; the instance page also says which bot device is cross-signed.

**Proof.** `crates/hs-bridge-conformance/tests/real_mautrix_login.rs`: alice's `matrix-sdk`
client is built with `with_room_key_recipient_strategy(CollectStrategy::IdentityBasedStrategy)`,
the exact rule the owner's client applied; `bot_is_cross_signed` waits for the instance's
`signed_bot_device`, then reads `/keys/query` as alice and asserts the bot's master key, its
self-signing key signed by the master key, and the signed device carrying the self-signing key's
signature beside its own; the encrypted stories then assert `login qr` is answered with a QR (a
"refused to share decryption keys" answer is named as such) and that the appservice's health has
no `last_key_withheld`. Run on 2026-10-03: 3 of 3 in 31 s, the bot's devices `X3HXCGJKPK` and
`I36QDLYXYI` cross-signed, `login qr` decrypted and answered with the QR in both encrypted
stories, nothing withheld. Alice's own cross-signing has to be bootstrapped first: that
strategy refuses to send at all otherwise ("Encryption failed because cross-signing is not set
up on your account"), as the owner's Element has it. (No run against a binary without the
manager change was made from this session; the rule that client applies is the documented one,
"no published identity, no room keys", which is what the owner's client did to the demo's bot.)

**The owner's way out until this ships, in Element's own words.** The warning comes from one of
two settings, and which one decides what to do:

- Settings → Labs → Encryption → **"Exclude insecure devices when sending/receiving messages"**
  (`feature_exclude_insecure_devices`; a per-device Labs flag, off unless turned on). This one
  ignores manual verification of a single device (`IdentityBasedStrategy` wants the bot's own
  identity), so the only relief is to turn it off, which puts the client back to Element's
  default, or wait for this fix. Element X's equivalent is its invisible-crypto setting under
  Advanced settings.
- Settings → Security & Privacy → **"Only send messages to verified users"**
  (`blacklistUnverifiedDevices`, `OnlyTrustedDevices`; also per room under the room's Settings
  → Security & Privacy with the same name). This one honours a manually verified device: open the
  bot's profile, its session "WhatsApp bridge" → **"Manually verify by text"**, compare the
  session key with the one shown on the bridge page's appservice (`/keys/query` for the bot) and
  confirm. That is the safer workaround, since it relaxes nothing; turning the setting off for
  the bot's chat only (the room override) is the next safest, as that room holds the person and
  their own bot and nobody else.

Running it: `cargo build -p hs-cli --bin hs`, then `cargo test -p hs-bridge-conformance --test
real_mautrix_login -- --nocapture` with Docker reachable.

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
