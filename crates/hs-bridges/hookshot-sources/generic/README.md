# Plain JSON

No transformation: whatever posts JSON to the URL is shown as Hookshot shows it. Use this for
a script, a cron job, or any service whose payload is already a message.

## On the sender

Post JSON to `{{url}}` (`POST`, `Content-Type: application/json`; `PUT` works too). The URL's
last part is the secret: anyone who has it can post into the room.

- `text` is the message. Markdown in it is rendered.
- `username`, when given, is shown in bold before the message.
- `html`, when given, is the message's HTML.
- Anything else is posted as data: "Received webhook data:" and the JSON, formatted.

```sh
curl -X POST '{{url}}' -H 'content-type: application/json' \
  -d '{"username": "deploy", "text": "myelin 0.4.3 is on the demo"}'
```

A form (`application/x-www-form-urlencoded`) or plain text posts work as well: Hookshot
turns the form into an object and wraps text as `{"text": ...}`.
