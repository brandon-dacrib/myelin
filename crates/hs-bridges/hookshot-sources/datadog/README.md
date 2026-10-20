# Datadog monitors

A monitor's alert, warning, recovery or "no data" notification, one notice each: the status,
the priority, the monitor's title linked to the event in Datadog, the message, the host and
scope, and the tags.

## In Datadog

Integrations → Webhooks (install it if it is not) → **New Webhook**:

- **Name**: something you will recognise in a monitor's message, `matrix-{{name}}`.
- **URL**: `{{url}}`
- **Payload**: replace the default with this template (Datadog fills the `$` variables;
  `$TEXT_ONLY_MSG` is the message with its JSON escaped, which the default `$EVENT_MSG` is not):

```json
{
  "title": "$EVENT_TITLE",
  "text": "$TEXT_ONLY_MSG",
  "status": "$ALERT_TRANSITION",
  "priority": "$PRIORITY",
  "alert_type": "$ALERT_TYPE",
  "hostname": "$HOSTNAME",
  "scope": "$ALERT_SCOPE",
  "tags": "$TAGS",
  "link": "$LINK",
  "id": "$ALERT_ID",
  "date": "$DATE"
}
```

- Leave **Custom Headers** empty; **Encode as form** off.

Then, in each monitor that should post here, add `@webhook-matrix-{{name}}` to its message
(Monitors → the monitor → Edit → "Notify your team"). Datadog's **Test** button on the
webhook sends a payload without the template's values, which the room ignores.

The URL's last part is the secret. Datadog signs nothing; anyone with the URL can post.

## What lands

`**Alert** P2: [CPU on web-1 is high](https://app.datadoghq.com/event/...)`, the monitor's
message, then `host web-1 · tags env:prod service:web`; `**Recovered**: ...` when it recovers.
