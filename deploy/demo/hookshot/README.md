# Hookshot on the demo

matrix-hookshot 7.5.0 runs on the demo (`admin@dacrib0`, namespace `myelin`) as the shared
bridge `bridge-hookshot`, and posts GitHub comments on pull requests into Matrix rooms. Set up
by hand on 2026-10-20: the server's catalogue lists Hookshot but cannot deploy it itself yet
(`not_deployable_reason` in `crates/hs-admin/src/bridge_types.rs`; the branch
`agent/hookshot-offering` is making it an offering the server deploys like the others).

## What is where

| Thing | Where |
|---|---|
| The bridge | `Bridge/bridge-hookshot` (`bridge.yaml`): image `ghcr.io/matrix-org/matrix-hookshot:7.5.0`, appservice id `hookshot`, bot `@hookshot:myelin.dacrib.net`, 1 Gi volume |
| Its files | Secret `bridge-hookshot-files`: `config.yml`, `registration.yml`, `passkey.pem`. Made from `config.template.yml` and `registration.template.yml`; the tokens and the passkey are only in the Secret |
| Webhooks listener | Service `bridge-hookshot-webhooks` (port 9000) and the Funnel Ingress `hookshot-funnel`: **https://hookshot.longhair-tet.ts.net** (`bridge.yaml`) |
| Appservice registration | `registration.yml` registered with the server through the admin interface (Bridges → add a bridge registered by hand) or `POST /api/v1/appservices` with `registration_yaml` |
| Rooms | one room per repository, with a generic webhook connection whose transformation function is `github-comments.js` |
| GitHub | a repository webhook on `brandon-dacrib/myelin` and on `mosaic-avantos/avantos`, events `issue_comment`, `pull_request_review_comment`, `pull_request_review`, URL `https://hookshot.longhair-tet.ts.net/webhook/<hook id>` |

Hookshot 7.x has no provisioning API (its config refuses a `provisioning` section), so Myelin's
`hookshot_v1` provisioning kind reads nothing from it; connections are room state.

## How a room's webhook works

A generic webhook connection is a state event in the room, type
`uk.half-shot.matrix-hookshot.generic.hook`, state key = the connection's name, content
`{"name": ..., "transformationFunction": ...}`, plus the bridge's room account data of the same
type mapping `{"<hook id>": "<name>"}`; the hook id is the secret part of the URL, so it is in
the account data and the GitHub webhook, not in git. The bot creates the room and writes both
(it is the room's creator, so it may), then invites the people who want the notices. A room
made this way is exactly what `!hookshot webhook <name>` makes, with the hook id chosen first
so that the GitHub webhook can be created before the bridge is even registered.

`github-comments.js` turns `issue_comment` (PR conversation and issue comments),
`pull_request_review_comment` (comments on a diff line) and `pull_request_review` (approve,
request changes, a review summary) into one `m.notice` each and drops everything else,
including the ping GitHub sends on creation, which it answers with "connected". Edit the file
and write it into the state event's `transformationFunction` to change the format.

## Doing it again, or for another repository

1. Make the files from the templates: `<as-token>` and `<hs-token>` are `openssl rand -hex 32`,
   the passkey is `openssl genpkey -out passkey.pem -outform PEM -algorithm RSA -pkeyopt
   rsa_keygen_bits:4096`. Put them in the Secret (`kubectl create secret generic
   bridge-hookshot-files --from-file=...`), set `myelin.dev/files-hash` in `bridge.yaml` to
   `sha256(config.yml registration.yml passkey.pem)` and apply `bridge.yaml`.
2. Register `registration.yml` with the server (table above). The bridge crash-loops with
   `M_UNKNOWN_TOKEN` until then and recovers on its own.
3. Pick a hook id (`uuidgen`), create the room as the bot (`createRoom`, then the state event
   and the account data above, then the invites), and create the repository webhook with
   `gh api -X POST repos/<owner>/<repo>/hooks` and the URL
   `https://hookshot.longhair-tet.ts.net/webhook/<hook id>`, events as in the table.
4. Test: `gh api -X POST repos/<owner>/<repo>/hooks/<id>/pings` posts the "connected" notice.
   A comment on any PR of the repository is in the room within a second or two.
