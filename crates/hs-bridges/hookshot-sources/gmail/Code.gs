// Posts each new Gmail thread under one label to a Matrix room through matrix-hookshot's
// generic webhook (the server's `gmail` webhook source; README.md beside this file). Runs on a
// timed trigger: every thread under LABEL that is not yet under DONE_LABEL is posted once, as
// {from, subject, snippet, link, labels, date}, and then labelled done. Nothing is deleted or
// read elsewhere; Gmail's own filters decide what gets LABEL.
//
// Setup: script.google.com → New project → paste this file; set WEBHOOK_URL and LABEL; run
// `testWebhook` once (it asks for Gmail and "connect to an external service" permissions and
// posts a "connected" notice); then run `install` once to make the trigger.

// The webhook URL the server gave for this room (its last part is the secret).
const WEBHOOK_URL = "{{url}}";
// The Gmail label whose threads are posted; a Gmail filter puts the wanted mail under it.
const LABEL = "to-matrix";
// Threads already posted carry this label too, so each is posted once.
const DONE_LABEL = "to-matrix/posted";
// How often the trigger runs, in minutes (1, 5, 10, 15 or 30: what time-based triggers allow).
const EVERY_MINUTES = 5;

function postNewThreads() {
  const label = GmailApp.getUserLabelByName(LABEL);
  if (!label) {
    throw new Error("Gmail has no label named " + LABEL + "; make it, and a filter that applies it");
  }
  const done = GmailApp.getUserLabelByName(DONE_LABEL) || GmailApp.createLabel(DONE_LABEL);
  // Newest first, fifty at a time; a backlog bigger than that drains over a few runs.
  const threads = label.getThreads(0, 50);
  for (const thread of threads) {
    const names = thread.getLabels().map((l) => l.getName());
    if (names.indexOf(DONE_LABEL) !== -1) {
      continue;
    }
    const messages = thread.getMessages();
    const latest = messages[messages.length - 1];
    const payload = {
      from: latest.getFrom(),
      subject: thread.getFirstMessageSubject(),
      snippet: latest.getPlainBody().replace(/\s+/g, " ").trim().slice(0, 240),
      link: thread.getPermalink(),
      labels: names.filter((n) => n !== LABEL && n !== DONE_LABEL),
      date: latest.getDate().toISOString(),
    };
    const response = UrlFetchApp.fetch(WEBHOOK_URL, {
      method: "post",
      contentType: "application/json",
      payload: JSON.stringify(payload),
      muteHttpExceptions: true,
    });
    if (response.getResponseCode() >= 300) {
      // Left unlabelled: the next run tries again.
      console.warn("webhook answered " + response.getResponseCode() + " for " + payload.subject);
      continue;
    }
    thread.addLabel(done);
  }
}

// Posts a "connected" notice to the room: run this once by hand to grant the permissions and
// see the URL works.
function testWebhook() {
  const response = UrlFetchApp.fetch(WEBHOOK_URL, {
    method: "post",
    contentType: "application/json",
    payload: JSON.stringify({ test: true, label: LABEL }),
    muteHttpExceptions: true,
  });
  console.log("webhook answered " + response.getResponseCode());
}

// Makes the timed trigger (once; running it again replaces the trigger).
function install() {
  for (const trigger of ScriptApp.getProjectTriggers()) {
    if (trigger.getHandlerFunction() === "postNewThreads") {
      ScriptApp.deleteTrigger(trigger);
    }
  }
  ScriptApp.newTrigger("postNewThreads").timeBased().everyMinutes(EVERY_MINUTES).create();
}
