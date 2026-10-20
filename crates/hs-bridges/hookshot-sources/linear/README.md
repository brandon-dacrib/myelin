# Linear

Issues, comments and projects as they are created, changed and deleted, one notice each with
who did it: `**Ada** moved to **In Progress**: ENG-42 Receipts loop`, `**Ada** commented on
ENG-42 ...`, `**Ada** created project Q4 launch (started)`. Updates that change nothing the
room would show (a description edit, a label, a reorder) are dropped. Cycles, labels and the
rest of Linear's event types are dropped too.

## In Linear

Settings → API → Webhooks → **New webhook**:

- **URL**: `{{url}}`
- **Data change events**: **Issues**, **Comments**, **Projects** (the others are ignored).
- **Teams**: the teams whose work should post here, or all public teams.
- **Signing secret**: Linear signs every delivery (`Linear-Signature`, an HMAC-SHA256 of the
  body). Hookshot's generic webhook **does not verify it**: the URL's last part is the only
  secret, and anyone who has the URL can post. Keep the URL out of shared documents.

Linear retries a delivery it did not get a 2xx for; Hookshot answers 200 once it has the
payload, so a delivery is not posted twice.

## What lands

For an issue: created (with its state, assignee and priority), moved between states,
assigned or unassigned, renamed, its priority, due date or estimate changed, deleted. For a
comment: made, edited, deleted, with its text. For a project: created, its state, name, lead
or target date changed, deleted.
