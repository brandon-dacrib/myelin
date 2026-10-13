# 0046: 2026-10-12: double puppeting is a registration of its own, with no url, one per instance

Status: accepted (track 11; touches 15's admin API (OpenAPI 0.1.16) and 16's bridge pages).

## The problem

Five of the demo's six bridges (mautrix signal, whatsapp, meta, linkedin, discord; instances
`<type>-brandon`) stopped taking transactions and were shown "Degraded". Their pods were Running
and Ready, answered at once, and had not restarted.

Each instance's own registration (the one with the `url` the server pushes to) claimed its owner
non-exclusively, `@brandon:myelin\.dacrib\.net`, so that the bridge could double puppet with its
own token; the wizard's server-wide render claimed `@.*:<server>` the same way. Under the spec's
interest rules a registration is interested in every room one of its namespace's users is a
member of, exclusive or not. So the server sent each bridge every event of every room brandon was
in, his encrypted chats with other people among them, rooms the bridge's bot has never been in.

For each event it could not decrypt, mautrix waited 3 s and then 22 s for the keys (asking
brandon's devices for them), gave up, tried to send a "couldn't decrypt" notice, tried to join
the room and got `403 M_FORBIDDEN`. It handles events one at a time, so its event channel filled
("Event channel is full", thousands of times), its transaction handler blocked, and the server's
30 s request timeout fired. In 75 s the five bridges took no transactions; in 15 minutes meta
waited on keys 299 times, was refused 598 notices, and took 20 transactions. Slack, given the same
flood, drops such events at once and stayed healthy. The server's log said only `error sending
request for url (...)`: reqwest's top line, without the timeout under it.

## What was chosen

1. **A bridge's own registration claims its ghosts and its bot, and nothing else.** No owner, no
   `@.*`. The server then sends it the rooms its bot or ghosts are in (its portals and its
   management room), which are the ones it bridges.
2. **Double puppeting is a registration of its own**, mautrix's "double puppeting via appservice"
   (<https://docs.mau.fi/bridges/general/double-puppeting.html>): `url: null`, a non-exclusive
   claim on the people the bridge may act as, tokens of its own, and `io.myelin.double_puppet_for:
   <the bridge's appservice id>`. The server never sends a registration with no `url` anything
   (`hs_appservice::scheduler::wire_body`, `pump::listeners`). The bridge's config carries its
   `as_token` as `as_token:<token>`: `double_puppet.secrets` (bridgev2) and
   `bridge.login_shared_secret_map` (mautrix-discord's older framework, which takes `as_token:`
   since mautrix-go v0.15). The bridge acts as the person with it and `?user_id=`, which the
   server's masquerading allows for any user a registration's namespace covers.
   mautrix-imessage's build knows only `login_shared_secret: appservice`, an appservice login
   with its own token, and takes `as_token:` as an HMAC secret. **Amended 2026-10-12, late (the
   owner wants iMessage to double puppet):** the server allows an appservice login by an
   appservice as a user its own registration does not claim when that appservice's
   double-puppeting registration (`io.myelin.double_puppet_for` naming it) does
   (`AppserviceRegistry::double_puppet_record_for`, `hs-auth`'s `routes::login`). The bridge's
   own registration still claims only its ghosts and bot, so it is not flooded, and its config
   says `appservice` again. This widens only the login: masquerading with `?user_id=` and
   transaction delivery still follow the bridge's own registration. Checked with the real
   build: `Successfully automatically enabled custom puppet` with the double-puppeting
   registration, and `403` without it.
3. **One per instance, not one shared.** The manager mints `<instance id>-doublepuppet` for each
   instance that double puppets, claiming that instance's owner alone (`@brandon:server`), and
   keeps its tokens on the instance's row (`InstanceRow::double_puppet`). The wizard's
   server-wide bridge gets `<id>-doublepuppet` claiming `@.*:server`, since it serves everyone,
   rendered as `BridgeTypeRenderResult.double_puppet_registration` and created by the wizard
   beside the bridge's own.
4. **The manager acts as the owner with that token** (starting the owner's chat as them,
   re-inviting the bot when it repairs a chat), not the instance's own.
5. **Existing instances move by themselves.** On the first step after an upgrade, the manager
   mints and registers the double-puppeting registration, then (its existing mechanism, the
   registration fingerprint) patches the instance's own registration in place, same id and
   tokens, to stop claiming the owner, and the changed files roll the pod once through the
   deployment fingerprint, as the 0.3.0 Discord layout fix did. The pickle key, the provisioning
   secret and the instance's tokens are the row's and are written as they were (decision 0036).
   It is said once at INFO: "moved the bridge instance's double puppeting to a registration of
   its own". An instance run elsewhere is told to download its files again.
6. **A registration with no url is never a fault.** Its health is `unknown` whatever was recorded
   (`Registry::health`), and a ping of one answers `M_URL_NOT_SET` without recording a failure.
   The admin interface lists a double-puppeting registration as "Built in" ("Double puppeting for
   Signal Brandon"), any other url-less one as "Sends nothing", and neither under Attention.
   **Amended 2026-10-13:** listing it as a row of its own doubled the Registrations tab (the owner
   read "Slack Brandon" beside "Double puppeting for Slack Brandon" as a duplicate). The tab now
   folds it into its bridge's row ("Double puppeting on"); the bridge's page names and links it.
   It keeps a row of its own only when its bridge is not on the loaded page.
7. **Delivery errors say why.** The transaction sender, the ping, the user and alias queries and
   the provisioning call report the error's whole source chain (`hs_appservice::error::error_chain`):
   `error sending request for url (...): operation timed out`, not the top line alone.

## Why one per instance

mautrix's documentation shows one shared registration claiming `@.*` whose token goes into every
bridge. Here that token would let any bridge, and anyone who reads any bridge's config, act as
everyone on the server. A managed instance already could act as one person only (its claim was
its owner); one double-puppeting registration per instance keeps that: a leaked token acts as one
person, removing an instance removes its registration, and switching double puppeting off for an
offering removes each of its instances' registrations without touching anyone else's. The cost is
one more registration per instance in the registry, which the interface labels as the server's
own. The wizard's server-wide bridge already served everyone, so its one registration claims
everyone; that is the documentation's shape, applied to one bridge.

## Why not the other ways

- **Keep the claim and filter on the server.** The spec makes a namespace user's rooms the
  registration's business; sending a bridge less than the spec says would surprise every other
  appservice, and the bridge would still be told about rooms it cannot read.
- **The shared-secret login (`m.login.application_service` with the bridge's own token).** It
  needs the same namespace claim, so it has the same flood.
- **Fix it in mautrix.** The bridges behave as documented for the registration they were given.
