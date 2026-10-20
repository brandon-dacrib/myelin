# Gmail (through Apps Script)

Gmail has no outgoing webhooks, so a small Google Apps Script (`Code.gs` beside this file;
the create answer and the offering's page give it with the URL filled in) runs on a timer in
the mailbox, posts each new thread under one label once, and labels it as posted. The room gets
`**Ada Lovelace**: [Subject](link to the thread)` and the first 240 characters of the latest
message. The mail itself stays in Gmail; only the sender, subject, snippet and link leave it.

## In Gmail and Apps Script

1. In Gmail, make a label (`to-matrix`) and a filter that applies it to the mail that should
   post here (Settings → Filters and Blocked Addresses → Create a new filter → "Apply the
   label").
2. Open <https://script.google.com>, **New project**, replace `Code.gs` with the script below
   (or the file; `WEBHOOK_URL` is already `{{url}}`; `LABEL` is the label from step 1).
3. Run `testWebhook` once from the editor: Google asks for permission to read Gmail and to
   connect to an external service; the room then says "Gmail script connected".
4. Run `install` once: it makes a time-based trigger that runs `postNewThreads` every five
   minutes (`EVERY_MINUTES`).

Each thread is posted once (it gets the `to-matrix/posted` label). A thread that keeps
getting replies is not posted again; remove the posted label to post it again. The script
runs as the Google account that made it and sees that account's mail only.

The URL's last part is the secret; it is in the script, which only that account can read.

## What lands

One notice per thread: the sender's name in bold, the subject linked to the thread in Gmail,
the snippet, and the thread's other labels.
