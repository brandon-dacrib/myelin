// The `linear` webhook source: Linear's webhooks for issues, comments and projects, one notice
// per create, update or remove, with who did it. `data` is Linear's payload (`action`, `type`,
// `data`, `updatedFrom`, `actor`, `url`); `result` is what the room sees (Hookshot's v2 API).
// Markdown in `plain` is rendered by Hookshot.
const actor = data.actor && data.actor.name ? "**" + data.actor.name + "**" : "Someone";
const action = String(data.action || "");
const item = data.data || {};
const url = typeof data.url === "string" ? data.url : item.url;
const link = (label) => (url ? "[" + label + "](" + url + ")" : label);
const from = data.updatedFrom || {};
let plain = null;
if (data.type === "Issue") {
  const label = (item.identifier ? item.identifier + " " : "") + (item.title || "an issue");
  const state = item.state && item.state.name ? item.state.name : null;
  const assignee = item.assignee && item.assignee.name ? item.assignee.name : null;
  if (action === "create") {
    plain = actor + " created " + link(label);
    const bits = [];
    if (state) bits.push("in " + state);
    if (assignee) bits.push("assigned to " + assignee);
    if (item.priorityLabel && item.priorityLabel !== "No priority") bits.push(item.priorityLabel);
    if (bits.length) plain += " (" + bits.join(", ") + ")";
  } else if (action === "update") {
    const changes = [];
    if ("stateId" in from && state) changes.push("moved to **" + state + "**");
    if ("assigneeId" in from) changes.push(assignee ? "assigned to " + assignee : "unassigned");
    if ("title" in from) changes.push("renamed");
    if ("priority" in from && item.priorityLabel) changes.push("priority " + item.priorityLabel);
    if ("dueDate" in from) changes.push(item.dueDate ? "due " + item.dueDate : "due date removed");
    if ("estimate" in from) changes.push("estimate " + (item.estimate == null ? "removed" : item.estimate));
    if (changes.length) {
      plain = actor + " " + changes.join(", ") + ": " + link(label);
    }
    // An update that changed nothing this shows (a description edit, a label, a sort order) is dropped.
  } else if (action === "remove") {
    plain = actor + " deleted " + label;
  }
} else if (data.type === "Comment") {
  const issue = item.issue || {};
  const label = (issue.identifier ? issue.identifier + " " : "") + (issue.title || "an issue");
  if (action === "create") {
    plain = actor + " commented on " + link(label) + ":\n\n" + (item.body || "");
  } else if (action === "update" && "body" in from) {
    plain = actor + " edited a comment on " + link(label) + ":\n\n" + (item.body || "");
  } else if (action === "remove") {
    plain = actor + " deleted a comment on " + label;
  }
} else if (data.type === "Project") {
  const label = item.name || "a project";
  if (action === "create") {
    plain = actor + " created project " + link(label) + (item.state ? " (" + item.state + ")" : "");
  } else if (action === "update") {
    const changes = [];
    if ("state" in from && item.state) changes.push("now **" + item.state + "**");
    if ("name" in from) changes.push("renamed");
    if ("leadId" in from) changes.push(item.lead && item.lead.name ? "led by " + item.lead.name : "lead removed");
    if ("targetDate" in from) changes.push(item.targetDate ? "target " + item.targetDate : "target date removed");
    if (changes.length) {
      plain = actor + " updated project " + link(label) + ": " + changes.join(", ");
    }
  } else if (action === "remove") {
    plain = actor + " deleted project " + label;
  }
}
if (plain === null) {
  result = { version: "v2", empty: true };
} else {
  result = { version: "v2", plain: plain, msgtype: "m.notice" };
}
