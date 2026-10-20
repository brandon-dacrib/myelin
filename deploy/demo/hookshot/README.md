# Hookshot on the demo

matrix-hookshot 7.5.0 runs on the demo (`admin@dacrib0`, namespace `myelin`) as the
`matrix-hookshot` offering (`../values-bridges.yaml`; docs/bridges/hookshot.md): the server
renders its config, registration and passkey, the operator runs it as `Bridge/bridge-hookshot`
with the bot `@hookshot:myelin.dacrib.net`, and it posts GitHub comments on pull requests into
Matrix rooms. First set up by hand on 2026-10-20 (a Bridge, a files Secret and a registration
made from templates, while the server could not deploy Hookshot itself); switched to the
offering the same evening, once v0.4.11 (the chart whose Bridge CRD has `extraPorts`) was on
the demo. The rooms, their connections and the GitHub webhooks survived the switch unchanged:
a connection is room state and the bot's room account data, and the bot kept its name.

## What is where

| Thing | Where |
|---|---|
| The bridge | the offering: Bridges → Hookshot in the admin interface, or `GET /api/v1/bridge-offerings/matrix-hookshot`; runtime `cluster`, public URL `https://hookshot.longhair-tet.ts.net`, everyone on the server may connect things |
| Its objects | `Bridge/bridge-hookshot` and the Secret `bridge-hookshot-files`, written by the server; Deployment, Service `bridge-hookshot` (ports `appservice` 9993, `webhooks` 9000, `metrics` 9001) and a 1 Gi volume, by the operator |
| Public hostname | the Funnel Ingress `hookshot-funnel` (`ingress.yaml`): **https://hookshot.longhair-tet.ts.net** → `bridge-hookshot:webhooks`. The one object made by hand; `kubectl apply -f ingress.yaml` |
| Appservice registration | `hookshot`, made by the server with the instance; the offering's page shows its health |
| Rooms | one room per repository, with a generic webhook connection whose transformation function is `github-comments.js`; the offering's page lists them under Connections |
| GitHub | a repository webhook on `brandon-dacrib/myelin` and on `mosaic-avantos/avantos`, events `issue_comment`, `pull_request_review_comment`, `pull_request_review`, URL `https://hookshot.longhair-tet.ts.net/webhook/<hook id>` |

Hookshot 7.x has no provisioning API (its config refuses a `provisioning` section), so the
server reads the connections from the rooms' state
(`GET /api/v1/bridge-offerings/matrix-hookshot/connections`). Since 2026-10-20 the server
also makes a webhook itself, as the bot, from the offering's page or
`POST /api/v1/bridge-offerings/matrix-hookshot/connections` (`docs/bridges/hookshot.md`,
"Making a webhook from the server"): the steps below are what it does, by hand.

## How a room's webhook works

A generic webhook connection is a state event in the room, type
`uk.half-shot.matrix-hookshot.generic.hook`, state key = the connection's name, content
`{"name": ..., "transformationFunction": ...}`, plus the bridge's room account data of the same
type mapping `{"<hook id>": "<name>"}`; the hook id is the secret part of the URL, so it is in
the account data and the GitHub webhook, not in git. `!hookshot webhook <name>` in a room the
bot is in makes exactly this; the demo's two rooms were made by the bot with the hook id chosen
first, so that the GitHub webhook could be created before the bridge was even registered.

`github-comments.js` (since 2026-10-20 the server's `github-comments` webhook source,
`crates/hs-bridges/hookshot-sources/github-comments/transform.js`; the file here points there)
turns `issue_comment` (PR conversation and issue comments),
`pull_request_review_comment` (comments on a diff line) and `pull_request_review` (approve,
request changes, a review summary) into one `m.notice` each and drops everything else,
including the ping GitHub sends on creation, which it answers with "connected". Edit the file
and write it into the state event's `transformationFunction` to change the format.

## Another repository

1. In a room the bot is in (invite `@hookshot:myelin.dacrib.net`, give it Moderator so it may
   send state events): `!hookshot webhook <name>`. The bot answers with the URL, under
   `https://hookshot.longhair-tet.ts.net/webhook/`.
2. Put `github-comments.js` in the connection: write the room's
   `uk.half-shot.matrix-hookshot.generic.hook` state event (state key = the name) with
   `transformationFunction` set to the file's text (the bot's own account can; so can any room
   moderator).
3. `gh api -X POST repos/<owner>/<repo>/hooks` with that URL, content type `json`, the events
   in the table.
4. Test: `gh api -X POST repos/<owner>/<repo>/hooks/<id>/pings` posts the "connected" notice.
   A comment on any PR of the repository is in the room within a second or two.

## Doing it again from nothing

Install the chart with `../values-bridges.yaml` (the offering is declared there and the server
creates it on first start), apply `ingress.yaml`, then the steps above per repository. Nothing
else: the server mints the tokens and the passkey, and keeps the passkey on the instance so a
roll never loses what the bridge encrypted with it.
