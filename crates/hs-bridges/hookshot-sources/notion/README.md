# Notion

Notion's webhook subscriptions say *that* something changed, not *what*: each event carries the
object's id and type, the kind of change (`page.content_updated`, `database.schema_updated`,
`comment.created`, ...), when, and who (as ids). The notice renders exactly that, with a link
that opens the page or database: `Notion: page content changed · <id> · 2026-10-20 10:00 UTC ·
[open](...)`. Reading the content back would need Notion's API and an integration token, which
the webhook does not carry.

## In Notion

Settings → Connections → Develop or manage integrations → your integration (make an
internal one if there is none) → **Webhooks** → **Create subscription**:

- **Webhook URL**: `{{url}}`
- **Event types**: the ones wanted (`page.content_updated`, `page.created`,
  `page.properties_updated`, `database.*`, `comment.created`, ...). Fewer is calmer: a page
  being edited sends `page.content_updated` about once a minute while it is.
- Notion sends the subscription's **verification token** to the URL first. The room shows it
  (`Notion sent this subscription's verification token ...`); paste it into the subscription
  in Notion to activate it.

The integration must be connected to the pages whose events are wanted (the page's `...` menu
→ Connections). Notion signs deliveries with `X-Notion-Signature` (an HMAC of the body with the
verification token); Hookshot's generic webhook **does not verify it**: the URL's last part is
the only secret.

## What lands

One notice per event: the kind of object, the change, the id, the time, the number of people
involved when more than one, and the link; for a content change, how many blocks changed.
