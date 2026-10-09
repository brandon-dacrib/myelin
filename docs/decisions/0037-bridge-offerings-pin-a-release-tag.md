# 0037: Bridge offerings pin a release tag, and bumping one is a release step (2026-10-09)

Status: accepted (track 11; tracks 12 and 16 consume). Changes what the bridge catalogue
(`crates/hs-admin/src/bridge_types.rs`) names as each offering's image and what an offering's
`image_tag` means.

## Context

Every offering's image was `:latest`: `dock.mau.dev/mautrix/<network>:latest`,
`hif1/heisenbridge:latest`, `halfshot/matrix-hookshot:latest`. mau.dev's `latest` is rebuilt on
every push to the bridge's main branch. During the demo roll of 2026-10-09 (decision 0036) the
WhatsApp bridge's pod was recreated and pulled a mautrix-whatsapp built 25 minutes earlier; it
was a suspect in the outage, and nothing could say which bridge had run before the roll. An
operator reading the offering's page saw `latest`, which names nothing.

## Decision

- **The catalogue names a release tag for every image, never `latest`.** Each pin is the newest
  release of its upstream whose manifest exists in its registry when the pin is set
  (`docs/bridges/mautrix.md`, "2026-10-09: offerings pin a release tag", has the table and where
  each came from). A tag, not a digest: the tag is what the operator reads and what the upstream's
  release notes name. A test fails if any entry's tag is `latest` or not shaped like a release.
- **An offering's `image_tag` is the catalogue's pin unless the operator names another release.**
  `bridge_types::image_tag(type, requested)`: blank and `latest` both resolve to the pin; any
  other tag is kept as given. The bridge manager (`hs-bridges`) and the admin API's in-memory
  source apply it when an offering is put and when one is read, so an offering recorded before
  this decision, whose row says `latest`, reads and deploys as the pin from now on. `latest` is
  not a pin and cannot be chosen; an operator who wants a moving tag names the registry's branch
  tag for it.
- **A moved pin is a deployment change, applied once.** The manager's deploy fingerprint includes
  the tag and the operator's pod-template hash includes the image, so each `cluster` instance of
  the offering rolls once after a release that bumps its pin, with the back-off of decision 0036
  if the apply fails. The server logs the apply; the instance's page shows the image it runs.
- **Bumping a pin is a release step, not a chore.** Check the registry for the manifest, run the
  real-bridge story (`crates/hs-bridge-conformance/tests/real_mautrix_login.rs`) for a mautrix
  bridge, which fails when the catalogue's pin and the test's differ, and add the row to the
  table in `docs/bridges/mautrix.md`. The web mocks (`web/src/mocks/data/bridge-types.ts`) and
  the real-binary Playwright specs under `web/e2e-real/` name the WhatsApp pin too.
- **Hookshot and matrix-appservice-irc move registries.** Docker Hub's `halfshot/matrix-hookshot`
  stopped receiving release tags after 7.3.2 (2026-01-30); releases since are on
  `ghcr.io/matrix-org/matrix-hookshot`, so the offering names `ghcr.io/matrix-org/matrix-hookshot:7.5.0`.
  `ghcr.io/matrix-org/matrix-appservice-irc` refuses an anonymous pull token, so the offering
  names Docker Hub's maintained `matrixdotorg/matrix-appservice-irc:release-4.0.0` instead.

## Consequences

- The demo's WhatsApp offering (row `image_tag: latest`) rolls once to `v0.2609.0` at the next
  roll of this server; after that it stays there until a release bumps the pin.
- The real-bridge CI story pulls a release tag, so it stops exercising mautrix main. Running it
  against a candidate pin before bumping is how a pin earns its place.
- `MYELIN_BRIDGES_OFFERINGS` (the chart's `bridges.offerings`) may still name `image_tag`; omitted
  or `latest`, it is the pin.
