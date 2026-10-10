# 0041: 2026-10-10: the bridge manager removes a bot's device the bridge left behind

Status: accepted (track 11; touches 07's auth middleware, 15's admin API and 16's bridge pages).

## The problem

A mautrix bridge whose crypto store is reset (or whose whole database is) makes its bot a new
device and never uses the old one again, but the old device stays registered with its keys. On
the demo on 2026-10-10 the WhatsApp bot had `BQBMQVR81T` (made by the 2026-10-09 reset) and
`BSLXZIVKIV` (from 2026-10-02). Clients went on encrypting room keys to `BSLXZIVKIV`, the bridge
dropped them ("Dropping to-device event targeted to someone else"), and a client that withheld
keys (`m.unverified`) withheld them from both devices, so the server's health line named the dead
one half the time. Nothing removed it: there is no client a person could log out of.

## What was chosen

1. **The server records when an appservice acts as a device.** A request with an appservice
   token and `device_id` (or `org.matrix.msc3202.device_id`) now writes that device's
   `last_seen_ts` and `last_seen_ip`, at most once a minute (`hs-auth` middleware,
   `APPSERVICE_DEVICE_SEEN_EVERY_MS`). Before, an appservice's device kept the time it was made
   for ever, so "last seen" could not tell the device a bridge uses from the one it left. (Synapse
   records appservice IPs only with `track_appservice_user_ips`; ours is always on, throttled,
   as an ordinary login's is on every request.)
2. **The device in use is the bot's device with keys seen last.** On each look at a ready
   instance's bot (`settle_bot_identity`, every minute once settled), after signing, the manager
   reads `GET /devices` as the bot. Among the devices with keys published (`/keys/query`), the
   one with the latest `last_seen_ts` is in use; it is never removed, however long the bridge has
   been quiet. When two share the latest time, or none has a time, nothing is removed.
3. **A device is left behind** when it was last seen before the device in use and at least a day
   ago (`STALE_BOT_DEVICE_MS`). A device the server has no time for is never removed. A device
   without keys counts as left behind on the same terms (a reset abandoned before uploading keys);
   a newer device without keys does not count as in use.
4. **Removal goes through the instance's own token**: `DELETE /devices/{id}?user_id=<bot>`, which
   MSC4190 lets an appservice do without user-interactive auth (every managed instance with
   encryption has `io.element.msc4190: true`). The server deletes the device's keys with it and
   records a device-list change, so clients stop encrypting to it. A failure is the instance's
   reason, like the identity's own failures.
5. **It is said where an operator looks**: one `INFO` line per removal ("removed a device the
   bridge bot no longer uses", with `bridge_type`, `owner`, `appservice`, `bot`, `device`,
   `last_seen`, `kept_device`); the instance row keeps the last five removals, which the admin API
   serves as `BridgeInstance.removed_bot_devices` (schema `RemovedBotDevice`, OpenAPI 0.1.13);
   the bridge offering page says which devices were removed and, for a withheld key, which device
   it was for and whether that is the device the bridge uses.
6. **`signed_bot_device` is the device in use** when there is one, so the instance names the
   bridge's current device whatever order `/keys/query` lists them in.

## Why not the other signals

- **Cross-signing alone.** The manager signs every device of the bot, the old one included, so
  "signed by our identity" does not separate them.
- **Device-id order or `/keys/query` order.** Arbitrary.
- **Whatever device the instance last named.** Before this change the name could be the old
  device after a look that signed both at once.
- **No grace period.** A bridge restored from a backup goes back to its old device; a day lets
  that device be seen again (and become the one in use) before anything is removed.

## Risks accepted

A second crypto client acting as the same bot (two bridges sharing one registration) would make
the quieter one's device removable once the other's is seen after it and it has been a day
unseen. That set-up is already broken (one registration, two crypto stores); the line in the log
names both devices.
