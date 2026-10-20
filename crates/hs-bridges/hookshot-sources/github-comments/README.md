# GitHub comments

Comments on pull requests and issues, review comments on a diff line, and submitted reviews
(approved, changes requested, a review summary), one notice each. Everything else GitHub sends
is dropped; GitHub's ping when the webhook is made is answered with "connected". No GitHub App
is needed: this is a plain repository webhook.

## On GitHub

Repository → Settings → Webhooks → Add webhook:

- **Payload URL**: `{{url}}`
- **Content type**: `application/json`
- **Secret**: leave empty (the URL's last part is the secret; Hookshot does not check
  `X-Hub-Signature-256`)
- **Which events**: "Let me select individual events": **Issue comments**, **Pull request
  review comments**, **Pull request reviews**. Nothing else is rendered.

GitHub sends a ping when the webhook is saved; the room says "GitHub webhook for
`owner/repo` is connected." Any later comment is in the room within a second or two.

## What lands

`**login** commented on PR [#12 Title](link) on `path/to/file.rs`:` followed by the comment,
and `**login** approved PR [#12 Title](link)` for a review. Edited and deleted comments are
not reported.
