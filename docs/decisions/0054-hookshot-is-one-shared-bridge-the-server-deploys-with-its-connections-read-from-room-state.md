# 0054: Hookshot is one shared bridge the server deploys, with its connections read from room state

Date: 2026-10-20. Track 11 (appservices and bridges); touches tracks 12 (the `Bridge` CRD and
the operator), 15 (the admin API) and 16 (the interface).

## Context

matrix-hookshot (`ghcr.io/matrix-org/matrix-hookshot:7.5.0`) was in the catalogue with a
`not_deployable_reason`: "its config names the networks or services it connects, which only
an administrator can write". That was true of the IRC bridge and was assumed of Hookshot. It is
not: Hookshot's config names nothing it connects. Everything it connects is decided room by room
after it runs (`!hookshot webhook`, `!hookshot feed`, `!hookshot github repo`), and the config
needs exactly one thing an administrator has: a public URL that reaches its webhooks listener,
because every webhook URL it hands out starts with it. A GitHub App is optional.

Three more facts, from the 7.5.0 image and the hand-made bridge on the demo
(`deploy/demo/hookshot/`), shape the design:

- Hookshot does not generate its `passFile` (`passkey.pem`, the RSA key it encrypts stored
  tokens with): `UserTokenStore.fromKeyPath` reads the file and the bridge stops without it.
- Hookshot 7 has no provisioning API: a `provisioning` config section is refused, and
  `provisioning` is not a listener resource (only `webhooks`, `widgets`, `metrics`). Myelin's
  `hookshot_v1` provisioning kind read nothing. Each connection is a state event in its room
  (`uk.half-shot.matrix-hookshot.<service>.<kind>`, state key = the connection's name), emptied
  when removed; a generic webhook's hook id is in the bot's room account data, not the state.
- It is one bridge for the whole server with many connections, each landing in its own room,
  not one bridge per person with one account each. Nobody signs in to it.

## Decision

1. **Hookshot is `deployable`.** The catalogue renders its `config.yml` whole from the offering:
   `bridge`, `passFile: /data/passkey.pem`, `permissions` (everyone on the server, or the
   offering's `access.users`, at `admin`), `generic` with `urlPrefix` = the offering's
   `public_url` + `/webhook/` and `userIdPrefix: _webhooks_`, `feeds`, `github` when the
   offering carries a complete App, `metrics`, and listeners for webhooks (9000) and metrics
   (9001). No `encryption` (needs Redis), no `provisioning`. The files carry the names the
   image's command reads (`config.yml`, `registration.yml`), the passkey and the GitHub key.
2. **`BridgeOfferingOptions` gains `public_url` and `github {app_id, private_key,
   webhook_secret}`.** A type whose catalogue entry needs `public_url` cannot be offered without
   one that starts with `http(s)://`; half a GitHub App is refused. The private key is echoed by
   `GET`: `PUT` replaces the options whole, and `bridges:read` is the scope that reads an
   offering, as `bridges:write` reads the tokens in an instance's files.
3. **The passkey is minted by the server and kept on the instance's row** (`passkey_pem`), the
   way `pickle_key` is: 4096-bit RSA as PKCS#8 PEM (what `openssl genpkey` writes), made off
   the runtime with the instance's tokens, written into every render, so a roll never changes
   it and a bridge run elsewhere gets it in its files. 2048 bits under test.
4. **The `Bridge` CRD gains `extraPorts: [{name, port}]`** and the operator puts each on the
   container and on the Service by name, after the `appservice` port; nothing probes them. The
   field is left out when empty, so an older CRD still takes every other bridge, and refuses a
   `Bridge` with ports (`OutdatedCrd`: apply the CRD). The Ingress for the public URL is the
   administrator's and is documented, not made: which ingress controller and host a deployment
   has is not the operator's to know.
5. **Hookshot's provisioning kind is `none`**, and **`GET /bridge-offerings/{type}/connections`**
   lists what a shared integrations bridge has connected by reading, as the bot with the
   instance's token, every `uk.half-shot.matrix-hookshot.*` state event in the rooms the bot is
   in: `service`, `event_type`, `state_key`, `room_id`, `room_name`, `name`, `state`
   (`active`/`removed`), `content`. The hook id is not in it. The offering's page shows the
   table and the catalogue copy says what Hookshot is. Read live on each call: a server has one
   Hookshot and a handful of rooms.
6. **The registration claims every ghost prefix Hookshot's services use**, as its own sample
   registration does: `@hookshot_.*`, `@_github_.*`, `@_gitlab_.*`, `@_jira_.*`,
   `@_webhooks_.*`, `@feeds:server`, all exclusive.

## Consequences

- An administrator offers Hookshot from the wizard with a public URL (and optionally a GitHub
  App), points an Ingress at `bridge-hookshot:webhooks`, and people connect things in rooms. The
  demo's hand-made setup (`deploy/demo/hookshot/`) can become an offering; this decision does
  not change the demo.
- `BridgeInstanceFiles` gains `files` (every file by name), since a Hookshot run elsewhere needs
  more than a config and a registration.
- The CRD in a cluster has to be current for a Hookshot `Bridge` to apply; the chart keeps it
  current, and the instance's page says so otherwise.
- The real-image story (`crates/hs-bridge-conformance/tests/real_hookshot.rs`) passed against
  the 7.5.0 image on 2026-10-20: a webhook made in a room, posted to, landing, and listed. It
  skips, saying why, where the image cannot be had (pulling from ghcr.io in a session on the
  owner's desktop needs a `DOCKER_CONFIG` without a credential store and a `PATH` without the
  keychain helper; `docs/bridges/hookshot.md`).
- Not done: GitLab and Jira sections, the GitHub App's OAuth, adding or removing connections
  from the page (they are made in rooms by design), encryption (Redis).
