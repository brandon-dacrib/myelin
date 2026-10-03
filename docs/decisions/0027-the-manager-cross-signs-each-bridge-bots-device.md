# 0027: 2026-10-03: the bridge manager keeps each instance bot's cross-signing identity

Status: accepted (track 11; touches 08's routes as a consumer, 15's admin API and 16's pages).

## The problem

A client that excludes insecure devices (Element Web's Labs setting "Exclude insecure devices
when sending/receiving messages", Element X's invisible crypto, `matrix-sdk`'s
`CollectStrategy::IdentityBasedStrategy`, all MSC4153) shares a room's Megolm keys only with
devices signed by their owner's self-signing key, and with no device at all of a user who has
published no cross-signing identity. A mautrix bridge's bot has no identity unless its config
says `encryption.self_sign: true`, so such a client sends the bot `m.room_key.withheld`
(`m.unverified`) and the bridge answers "⚠️ Your message was not bridged: your client refused to
share decryption keys with the bridge". The owner saw exactly that on the demo on 2026-10-03.

## What was chosen

The manager (`hs-bridges`) owns the bot's identity rather than the bridge:

1. On an instance's first ready step it mints a master key and a self-signing key, stores the
   seeds on the instance row (`cross_signing_master_seed`, `cross_signing_self_signing_seed`,
   beside `pickle_key`), publishes the public keys with `POST /keys/device_signing/upload` as the
   appservice masquerading as the bot, and signs the bot's device with `POST
   /keys/signatures/upload`. The signed device is recorded (`signed_bot_device`) and shown by the
   admin API and the interface.
2. Every minute afterwards it looks again (`/keys/query`, loopback) and signs a device the bridge
   made since (a reset bridge database). An instance recreated for the same owner gets new seeds
   and replaces the keys on the server.
3. The server side needed no change: `hs-e2e` already skips user-interactive auth on
   `/keys/device_signing/upload` for an appservice requester even when keys exist (MSC4190, the
   same rule as Synapse), accepts a bare master key, verifies the self-signing key's signature,
   and records a device-list change on both uploads so a tracking client re-fetches.

## Why not mautrix's own `encryption.self_sign`

It would be one rendered line, but the bridge keeps its recovery key in its own database
(`kv_store.recovery_key`) and its startup is fatal (exit 34, "Server already has cross-signing
keys, but no key in database") whenever the server has the bot's keys and the database does not:
every instance recreated for the same owner (`remove` keeps the bot user and there is no client
API to delete cross-signing keys), every reset bridge database. The manager's identity survives
both, needs nothing from the bridge, and works for any appservice bot, not only mautrix.

## What other tracks see

- Track 15: `BridgeInstance.signed_bot_device`, `BridgeInstance.last_key_withheld`,
  `AppServiceHealth.last_key_withheld`, the `KeyWithheld` schema; OpenAPI 0.1.7.
- Track 11's `hs-appservice`: the scheduler records the last `m.room_key.withheld` it delivers
  in the appservice's health and counts `hs_appservice_key_withheld_total{appservice,code}`.
- Track 16: the person's instance page says the bot's device is cross-signed, and both it and the
  bridge page show the last refusal and what it means.
- Track 08: nothing to change; the appservice exemption on `/keys/device_signing/upload` is now
  relied on by a real bridge deployment and proven by `crates/hs-bridge-conformance/tests/
  real_mautrix_login.rs`.
