// The `alertmanager` webhook source: Prometheus Alertmanager's webhook receiver. One POST is a
// group of alerts (`status` firing or resolved, `alerts[]` each with labels and annotations);
// the notice heads with the group and lists every alert. `data` is Alertmanager's payload;
// `result` is what the room sees (Hookshot's v2 API). Markdown in `plain` is rendered.
const alerts = Array.isArray(data.alerts) ? data.alerts : [];
if (!alerts.length) {
  result = { version: "v2", empty: true };
} else {
  const firing = alerts.filter((a) => a.status === "firing").length;
  const resolved = alerts.length - firing;
  const groupStatus = String(data.status || (firing ? "firing" : "resolved")).toUpperCase();
  const groupLabels = data.groupLabels && typeof data.groupLabels === "object" ? data.groupLabels : {};
  const groupName = Object.keys(groupLabels).map((k) => k + "=" + groupLabels[k]).join(", ");
  const counts = [];
  if (firing) counts.push(firing + " firing");
  if (resolved) counts.push(resolved + " resolved");
  let head = "**" + groupStatus + "** " + counts.join(", ");
  if (groupName) head += " (" + groupName + ")";
  if (typeof data.externalURL === "string" && data.externalURL.startsWith("http")) {
    head = "[" + head + "](" + data.externalURL.replace(/\/$/, "") + "/#/alerts)";
  }
  const lines = [head, ""];
  for (const alert of alerts) {
    const labels = alert.labels || {};
    const ann = alert.annotations || {};
    const name = labels.alertname || "alert";
    const severity = labels.severity ? " `" + labels.severity + "`" : "";
    const status = alert.status === "resolved" ? " (resolved)" : "";
    const summary = ann.summary || ann.description || ann.message || "";
    const where = ["instance", "job", "namespace", "pod", "service"].filter((k) => labels[k]).map((k) => k + "=" + labels[k]).join(" ");
    let line = "- **" + name + "**" + severity + status;
    if (summary) line += ": " + summary;
    if (where) line += " — `" + where + "`";
    if (typeof alert.generatorURL === "string" && alert.generatorURL.startsWith("http")) {
      line += " [source](" + alert.generatorURL + ")";
    }
    if (typeof ann.runbook_url === "string" && ann.runbook_url.startsWith("http")) {
      line += " [runbook](" + ann.runbook_url + ")";
    }
    lines.push(line);
  }
  if (typeof data.truncatedAlerts === "number" && data.truncatedAlerts > 0) {
    lines.push("", "... and " + data.truncatedAlerts + " more");
  }
  result = { version: "v2", plain: lines.join("\n"), msgtype: "m.notice" };
}
