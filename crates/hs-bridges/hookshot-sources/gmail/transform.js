// The `gmail` webhook source: a Google Apps Script (Code.gs beside this file) posts each new
// thread under one Gmail label as `{from, subject, snippet, link, labels, date}`; the notice is
// the sender, the subject linked to the thread, and the snippet. `{test: true}` is the script's
// own check that the URL works. `data` is the payload; `result` is what the room sees
// (Hookshot's v2 API). Markdown in `plain` is rendered.
const subject = typeof data.subject === "string" ? data.subject.trim() : "";
const from = typeof data.from === "string" ? data.from.trim() : "";
if (data.test === true) {
  result = {
    version: "v2",
    plain: "Gmail script connected" + (data.label ? " (label `" + data.label + "`)" : "") + ".",
    msgtype: "m.notice",
  };
} else if (!subject && !from) {
  result = { version: "v2", empty: true };
} else {
  // "Ada Lovelace <ada@example.org>" reads better as just the name when there is one.
  const name = from.replace(/\s*<[^>]*>\s*$/, "").replace(/^"|"$/g, "") || from;
  const link = typeof data.link === "string" && data.link.startsWith("http") ? data.link : null;
  const title = subject || "(no subject)";
  let plain = "**" + name + "**: " + (link ? "[" + title + "](" + link + ")" : title);
  const snippet = typeof data.snippet === "string" ? data.snippet.trim() : "";
  if (snippet) {
    plain += "\n\n" + snippet;
  }
  const labels = Array.isArray(data.labels) ? data.labels.filter((l) => typeof l === "string") : [];
  if (labels.length) {
    plain += "\n\n" + labels.map((l) => "`" + l + "`").join(" ");
  }
  result = { version: "v2", plain: plain, msgtype: "m.notice" };
}
