// The `datadog` webhook source: a Datadog monitor's alert, warning, recovery or "no data"
// notification, posted by Datadog's Webhooks integration with the payload template in the
// README beside this file. `data` is that payload; `result` is what the room sees (Hookshot's
// v2 API). Markdown in `plain` is rendered by Hookshot.
const title = typeof data.title === "string" ? data.title.trim() : "";
const text = typeof data.text === "string" ? data.text.trim() : "";
if (!title && !text) {
  // Nothing to say: Datadog's "Test" button, or a payload that is not the template.
  result = { version: "v2", empty: true };
} else {
  // $ALERT_TRANSITION: Triggered, Re-Triggered, Recovered, Warn, Re-Warn, No Data, Renotify...
  const transition = String(data.status || data.transition || "").trim();
  const words = {
    "triggered": "Alert",
    "re-triggered": "Alert (again)",
    "recovered": "Recovered",
    "warn": "Warning",
    "re-warn": "Warning (again)",
    "no data": "No data",
    "renotify": "Still alerting",
  };
  const what = words[transition.toLowerCase()] || (transition || "Alert");
  const priority = data.priority && data.priority !== "normal" ? " " + String(data.priority).toUpperCase() : "";
  const link = typeof data.link === "string" && data.link.startsWith("http") ? data.link : null;
  const head = "**" + what + "**" + priority + ": " + (link ? "[" + (title || "monitor") + "](" + link + ")" : title || "monitor");
  const lines = [head];
  if (text) {
    lines.push("", text);
  }
  const details = [];
  if (data.hostname) {
    details.push("host `" + data.hostname + "`");
  }
  if (data.scope && data.scope !== "n/a") {
    details.push("scope `" + data.scope + "`");
  }
  const tags = typeof data.tags === "string" ? data.tags.split(",").map((t) => t.trim()).filter(Boolean) : Array.isArray(data.tags) ? data.tags : [];
  if (tags.length) {
    details.push("tags " + tags.map((t) => "`" + t + "`").join(" "));
  }
  if (details.length) {
    lines.push("", details.join(" · "));
  }
  result = { version: "v2", plain: lines.join("\n"), msgtype: "m.notice" };
}
