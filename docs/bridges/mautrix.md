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
replaces the keys. Each signature is one `INFO` line in the server's log, "cross-signed the
bridge bot's device with the server-held self-signing key", with `appservice`, `bot`, `device`,
`first_signing` and (after the first) `previous_device`; a look that signs nothing logs nothing
(since 2026-10-09; before that the line lacked the appservice id). `config.yaml` is unchanged (`self_sign` stays at its default, off); mautrix-go
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

## 2026-10-09: offerings pin a release tag

Every offering's image was `:latest`, and during the roll above the WhatsApp bridge pulled a
mautrix-whatsapp built 25 minutes earlier. The catalogue now names a release tag for each image
(decision 0037), and an offering whose row still says `latest` (every one made before this)
reads and deploys as the pin. The pins, each the newest release of its upstream whose manifest
the registry served on 2026-10-09 (checked with the registry's v2 API and an anonymous token):

| Offering | Image | Release | Where it came from |
| --- | --- | --- | --- |
| mautrix-whatsapp | `dock.mau.dev/mautrix/whatsapp:v0.2609.0` | 2026-09-16 | github.com/mautrix/whatsapp/releases; manifest on dock.mau.dev |
| mautrix-telegram | `dock.mau.dev/mautrix/telegram:v0.2609.0` | 2026-09-16 | github.com/mautrix/telegram/releases; manifest on dock.mau.dev |
| mautrix-signal | `dock.mau.dev/mautrix/signal:v0.2609.0` | 2026-09-16 | github.com/mautrix/signal/releases; manifest on dock.mau.dev |
| mautrix-gmessages | `dock.mau.dev/mautrix/gmessages:v0.2609.0` | 2026-09-16 | github.com/mautrix/gmessages/releases; manifest on dock.mau.dev |
| mautrix-gvoice | `dock.mau.dev/mautrix/gvoice:v0.2605.0` | 2026-05-16 | github.com/mautrix/gvoice/releases; manifest on dock.mau.dev |
| mautrix-meta | `dock.mau.dev/mautrix/meta:v0.2609.0` | 2026-09-16 | github.com/mautrix/meta/releases; manifest on dock.mau.dev |
| mautrix-discord | `dock.mau.dev/mautrix/discord:v0.7.7` | 2026-08-16 | github.com/mautrix/discord/releases; manifest on dock.mau.dev |
| mautrix-slack | `dock.mau.dev/mautrix/slack:v0.2609.1` | 2026-09-25 | github.com/mautrix/slack/releases; manifest on dock.mau.dev |
| mautrix-twitter | `dock.mau.dev/mautrix/twitter:v0.2609.0` | 2026-09-16 | github.com/mautrix/twitter/releases; manifest on dock.mau.dev |
| mautrix-linkedin | `dock.mau.dev/mautrix/linkedin:v0.2609.0` | 2026-09-16 | github.com/mautrix/linkedin/releases; manifest on dock.mau.dev |
| mautrix-bluesky | `dock.mau.dev/mautrix/bluesky:v0.2510.0` | 2025-10-16 | github.com/mautrix/bluesky/releases; manifest on dock.mau.dev |
| heisenbridge | `hif1/heisenbridge:1.15.4` | 2025-10-04 | github.com/hifi/heisenbridge/releases (v1.15.4); Docker Hub tag `1.15.4`, amd64 and arm64 |
| matrix-appservice-irc | `matrixdotorg/matrix-appservice-irc:release-4.0.0` | 2025-10-24 | github.com/matrix-org/matrix-appservice-irc/releases (4.0.0); Docker Hub tag `release-4.0.0`. Was `ghcr.io/...:release-3.0.0`, whose package refuses an anonymous pull token |
| matrix-hookshot | `ghcr.io/matrix-org/matrix-hookshot:7.5.0` | 2026-09-22 | github.com/matrix-org/matrix-hookshot/releases; manifest on ghcr.io. Docker Hub's `halfshot/matrix-hookshot` has no release tag after 7.3.2 (2026-01-30) |

mau.dev tags each release twice, `v0.2609.0` (the Go module version, the GitHub release's name)
and `v26.09`; the catalogue uses the release's name. The dock.mau.dev check:
`TOKEN=$(curl -s 'https://mau.dev/jwt/auth?service=container_registry&scope=repository:mautrix/whatsapp:pull' | jq -r .token)`,
then `curl -sI -H "Authorization: Bearer $TOKEN" -H 'Accept: application/vnd.oci.image.index.v1+json, application/vnd.docker.distribution.manifest.list.v2+json' https://dock.mau.dev/v2/mautrix/whatsapp/manifests/v0.2609.0`
answers 200. Docker Hub: `https://hub.docker.com/v2/repositories/hif1/heisenbridge/tags/1.15.4`.
ghcr.io: a token from `https://ghcr.io/token?scope=repository:matrix-org/matrix-hookshot:pull`.

To bump one: check the manifest the same way, change the catalogue, the web mock
(`web/src/mocks/data/bridge-types.ts`) and, for WhatsApp or Signal, the real-bridge story's
constant and `web/e2e-real/*.spec.ts`; run `cargo test -p hs-bridge-conformance --test
real_mautrix_login` with Docker (it fails before anything boots when the catalogue and the test
disagree); add the row here.

Verified 2026-10-09 with that story on this machine (Docker 29.4.0, the debug `hs` of this
branch): all five pass on the pins, WhatsApp `v0.2609.0` (`Initializing bridge
built_at=2026-09-16T11:28:18Z go_version=go1.27.1 name=mautrix-whatsapp version=v26.09`) in
the encrypted chat, in the clear, in a chat the bot started and repaired in place, and through
the roll that keeps its own pickle key; Signal `v0.2609.0` (`built_at=2026-09-16T11:22:20Z
name=mautrix-signal version=v26.09`) answering `login` with its linking code. 102 s for the
five.

## 2026-10-09: "The supplied account key is invalid": a roll lost the bridge's own pickle key

**What happened.** The demo's WhatsApp instance was registered before 2026-10-02, when the
server did not render `encryption.pickle_key`, so the bridge generated its own on its first
start and pickled its crypto store with it. On 2026-10-09 the roll of `a6f02c48` restarted its
pod with a files Secret rendered without a `pickle_key` line. The operator's init container
carried the bridge's key only into a rendered file that already had the line (and printed
`carried pickle_key into /data/config.yaml` either way), so the key was dropped, mautrix's config
upgrader generated a new random one, and the bridge crash-looped with
`FTL Failed to start bridge error="failed to start Matrix connector: the supplied account key is
invalid"`. The server had also minted a key for the old instance that day (`minted a pickle key
for a bridge instance registered before the manager kept one`), a second key that was not the
store's either.

**What changed** (branch `agent/crd-upgrade`, decision 0036):

- The init container (`COPY_FILES_SCRIPT`, `crates/hs-operator/src/bridge.rs`) carries the
  bridge's own `pickle_key`, `signing_key` and `server_key` into every new copy: replacing a
  rendered value, adding the key under its section when the rendered file lacks it, or
  appending the section. The bridge's own value always wins.
- The server never mints a key for an instance registered before it kept one; only a new
  instance gets one, with its tokens.
- A crash-looping bridge's last log line reaches its page: the operator sets
  `terminationMessagePolicy: FallbackToLogsOnError` and carries the line into the `Bridge`'s
  status, and the manager turns "the supplied account key is invalid" into "the bridge cannot
  read its encryption store ... the steps are in docs/bridges/mautrix.md".
- Verified with the real bridge: `crates/hs-bridge-conformance/tests/real_mautrix_login.rs`,
  `a_bridge_rolled_with_a_render_without_its_pickle_key_keeps_its_own`, starts
  `dock.mau.dev/mautrix/whatsapp:latest` on the old render, then runs the operator's script in
  that image twice (a render with no key line, then one with another key) and starts the bridge
  again: it keeps its key and answers the server. With the old script the same test reproduces
  the demo's `FTL ... the supplied account key is invalid`.

**The recovery, as done on the demo on 2026-10-09.** A store pickled with a lost key cannot be
read again; resetting it costs the bridge's encryption keys, not its WhatsApp sign-in. On the
demo it took one helper pod and no data loss beyond that:

1. Make sure nothing holds the database: the demo's bridge was crash-looping, so it held
   nothing; a running one is stopped first (`kubectl scale` its Deployment to 0, with the
   operator stopped so it does not scale it back).
2. Run a helper pod on the bridge's claim `<bridge name>-data`. `kubectl proxy` (how the session
   reached the cluster) refuses `exec`, so the helper does its work as its own command rather
   than in a shell opened into it. The database is `/data/<appservice id>.db`
   (`whatsapp-brandon.db` on the demo, not `wa.db`). A script of this shape (any image with
   `sqlite3`), with the claim mounted at `/data`:

   ```sh
   cp /data/whatsapp-brandon.db /data/whatsapp-brandon.db.bak-20261009
   for t in $(sqlite3 /data/whatsapp-brandon.db \
       "SELECT name FROM sqlite_master WHERE type='table' AND name LIKE 'crypto%' AND name != 'crypto_version'"); do
     sqlite3 /data/whatsapp-brandon.db "DELETE FROM $t"
   done
   ```

   Keep `crypto_version`: it is the schema version, and an empty one makes the bridge try to
   create tables that exist.
3. Delete the helper and let the bridge start. It makes a new crypto account and a new bot
   device (`BQBMQVR81T` on the demo), the server cross-signs that device on its next look
   (within a minute: `signed_bot_device` on the instance names it, and the server's log has
   one line, `cross-signed the bridge bot's device with the server-held self-signing key
   ... appservice=whatsapp-brandon ... device=BQBMQVR81T first_signing=false
   previous_device=<the old device>`; `grep` the server's log for the appservice id or the new
   device id), and the WhatsApp login, kept in the bridge's own tables, survives. The `Bridge`
   went `Ready`.
4. People's clients see a new device for the bot; old encrypted messages to the bridge cannot
   be decrypted by it, new ones can.

## 2026-10-08: one bridge falling behind never holds up another, and each one's queue is a gauge

Every bridge has its own delivery worker and queue; since 2026-10-08 nothing a bridge does
(including never answering the server's "does this user exist" question) delays another bridge's
transactions (decision 0033). What to watch, per bridge (`appservice` is the registration id):

- `hs_appservice_queue_depth{appservice}`: queued entries waiting to be sent. Near 0 for a
  bridge that keeps up; climbing while the others stay flat is that bridge falling behind.
- `hs_appservice_queue_oldest_age_seconds{appservice}`: how long the oldest has waited. A
  starting alert: `max by (appservice) (hs_appservice_queue_oldest_age_seconds) > 300` for ten
  minutes.
- `hs_appservice_queue_dead_lettered{appservice}`: entries that ran out of attempts; replay them
  from the bridge's page (Backlog) once the bridge is back.
- `hs_appservice_transactions_total{appservice,outcome}` and
  `hs_appservice_queries_total{appservice,kind,outcome}`: failed deliveries, and questions the
  bridge did not answer (`outcome="error"`, logged at `WARN`: "an appservice did not answer the
  homeserver's question").

The bridges list (Bridges, Registrations) shows the same queue numbers in its "Waiting to send"
column (`AppService.queue` in the admin API), so the bridge that is behind is visible without
opening each one.

## 2026-10-04: the demo's shared WhatsApp registration becomes an offering

RFC 0017 section 6: the shared `mautrix-whatsapp` registration the demo got on 2026-09-25
(one bridge, registered by hand through the wizard, for everyone) is replaced by the WhatsApp
offering, where each person gets their own instance by messaging `@whatsappbot`. The demo has
both today: the offering, with brandon's instance (`whatsapp-brandon`, pod `bridge-d2854412-…`
or, once renamed, `bridge-whatsapp-brandon`), and the shared registration (appservice id
`whatsapp` or whatever the wizard was told; bot `@whatsappbot…`, ghosts `@whatsapp_.*`). Branch
`agent/bridge-offering-demo`.

**What the server does with both.** It keeps delivering to both. The shared registration's
exclusive `@whatsapp_.*` covers every instance's ghosts (`@whatsapp_brandon_.*`), so a message
for one of them reaches the shared bridge too. The registry allows that: neither claims the
other's bot, and the patterns are not identical. Since this branch the server says so instead
of letting it pass unnoticed:

- the shared bridge's page (Bridges → Registrations → it) has a warning line: "WhatsApp is
  offered on this server now: people get their own WhatsApp bridge by messaging
  @whatsappbot:… This bridge was registered by hand and …, so a message for a WhatsApp ghost
  user is delivered to it and to the person's own instance. Pause it here to stop delivering
  to it now; …". In the admin API it is `AppServiceHealth.overlaps_offering`;
- the WhatsApp offering's page has "Also registered by hand", naming it with a link
  (`BridgeOffering.overlapping_appservices`);
- the server's log says it once a minute at most when one appears (`WARN a bridge registered by
  hand overlaps the offering's instances: its page says what to do`, with `bridge_type` and
  `appservice`) and again when it is gone (`INFO … is gone`).

A registration counts as the offering's network when it was created from the same catalogue
entry (`io.myelin.bridge_type`), uses the catalogue's bot name, or has an exclusive user
namespace that covers the catalogue's ghosts (`crates/hs-bridges/src/overlap.rs`). It is not
paused or removed automatically: someone may still be signed in through it, and cutting it off
silently would lose their messages.

**The offering is declared in the deployment.** `deploy/demo/values-bridges.yaml` sets the
chart's new `bridges.offerings` (the server reads it from `MYELIN_BRIDGES_OFFERINGS`): WhatsApp
on the cluster runtime, everyone allowed, encryption, double puppeting and backfill. The server
creates a declared offering once, the first time it runs with it; one that exists already (the
demo's) is kept exactly as the admin interface has it; one an administrator removes later stays
removed. Nothing in `deploy/` registers a shared bridge.

**The roll, step by step** (a desk item: the session cannot reach the cluster).

1. Before anything, save the shared registration in case it has to come back: Bridges →
   Registrations → the shared WhatsApp bridge → Registration tab → download (or
   `curl -H "authorization: Bearer $ADMIN" https://myelin.dacrib.net/api/v1/appservices/<id>/registration`).
   Note its `url`: that is where the shared bridge runs.
2. Roll the image with the overlay:

   ```sh
   helm --kube-context admin@dacrib0 get values myelin -n myelin -o yaml > /tmp/myelin-values.yaml
   helm --kube-context admin@dacrib0 upgrade myelin /Users/brandon/myelin/deploy/helm/hs -n myelin \
     -f /tmp/myelin-values.yaml -f /Users/brandon/myelin/deploy/demo/values-bridges.yaml \
     --set image.tag=sha-<commit> --wait --timeout 10m
   ```

   The server's log says `the deployment declares bridge offerings` and then `the deployment
   declares a bridge offering that already exists: keeping it as the admin API has it`.
3. Open the shared bridge's page: the warning line is there. The WhatsApp offering's page says
   "Also registered by hand".
4. Press **Pause** on the shared bridge's page. Delivery to it stops at once and its queue keeps
   growing, so nothing is lost if it has to be resumed.
5. Anyone who still used the shared bridge messages `@whatsappbot:myelin.dacrib.net`, gets their
   own bridge, and signs in to it (`login qr`). Then they log the shared bridge out on their
   phone: WhatsApp → Linked devices → the old device → Log out.
6. Stop the shared bridge where it runs, from its `url` in step 1. If it is in the cluster,
   `kubectl -n myelin get deploy,svc | grep -i whatsapp` shows it beside the instances'
   `bridge-…` objects, which belong to the operator and stay. Scale it to zero, then delete it.
7. Remove the registration: **Remove** on its page (or
   `DELETE /api/v1/appservices/<id>`). The warning line and "Also registered by hand" go, and
   the log says `the bridge registered by hand that overlapped the offering's instances is
   gone`.
8. Check: brandon's chat with `@whatsappbot_brandon` still answers `help`, and a WhatsApp
   message still arrives through his instance.

To undo: re-add the saved registration through Bridges → Add (or `POST /api/v1/appservices`),
resume, and start the shared bridge again.

**Also in this branch.**

- An offering's options changed after instances exist reach their registrations: the manager
  renders each registered instance on every step and, when what its registration claims
  differs from what it last wrote (`registered_fingerprint`), patches the namespaces and the
  feature flags on the server. Switching double puppeting off drops the owner's non-exclusive
  claim at once; the files change with it, so a deployed instance rolls once, as a changed image
  tag already did. An instance run elsewhere keeps running with its old files and is told
  "its registration changed: download its files again and restart it with them". The offering
  settings dialog says what saving a changed double puppeting will do, for how many bridges.
- `command_prefix` for Signal, Slack, X and LinkedIn: their connectors set no
  `DefaultCommandPrefix`, and mautrix-go's example config
  (`bridgev2/matrix/mxmain/example-config.yaml`) falls back to `!` and the network id:
  `!signal`, `!slack`, `!twitter` (the network id stays `twitter` for X), `!linkedin`. The
  offering page shows the prefix.

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
