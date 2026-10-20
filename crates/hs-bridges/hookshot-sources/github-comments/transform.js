// The `github-comments` webhook source (matrix-hookshot generic webhooks,
// `transformationFunction` in each room's `uk.half-shot.matrix-hookshot.generic.hook` state
// event; README.md beside this file). GitHub posts every event the repository webhook
// subscribes to here; this turns the ones about comments into one notice each and drops the
// rest. `data` is the decoded payload; `result` is what the room sees (hookshot's v2 API).
const repo = data.repository ? data.repository.full_name : "a repository";
let plain = null;
if (data.zen) {
  // GitHub's ping when the webhook is created or tested.
  plain = "GitHub webhook for " + repo + " is connected.";
} else if (data.comment && (data.pull_request || data.issue)) {
  // issue_comment (a comment on a PR's or issue's conversation) and pull_request_review_comment
  // (a comment on a diff line). Edits and deletions are not reported.
  if (data.action === "created") {
    const target = data.pull_request || data.issue;
    const isPr = Boolean(data.pull_request || (data.issue && data.issue.pull_request));
    const where = data.comment.path ? " on `" + data.comment.path + "`" : "";
    plain =
      "**" + data.comment.user.login + "** commented on " + (isPr ? "PR" : "issue") +
      " [#" + target.number + " " + target.title + "](" + data.comment.html_url + ")" + where +
      ":\n\n" + (data.comment.body || "");
  }
} else if (data.review && data.pull_request && data.action === "submitted") {
  // pull_request_review: the review's summary. A review that only carries line comments
  // (state "commented", no body) is reported by those comments instead.
  const verbs = { approved: "approved", changes_requested: "requested changes on", commented: "reviewed" };
  const verb = verbs[data.review.state] || "reviewed";
  if (data.review.state !== "commented" || data.review.body) {
    plain =
      "**" + data.review.user.login + "** " + verb + " PR [#" + data.pull_request.number + " " +
      data.pull_request.title + "](" + data.review.html_url + ")" +
      (data.review.body ? ":\n\n" + data.review.body : "");
  }
}
if (plain === null) {
  result = { version: "v2", empty: true };
} else {
  result = { version: "v2", plain: plain, msgtype: "m.notice" };
}
