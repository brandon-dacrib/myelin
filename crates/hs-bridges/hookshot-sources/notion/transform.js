// The `notion` webhook source: Notion's webhook subscriptions. Notion's events carry ids and a
// type, not content (`entity: {id, type}`, `type: "page.content_updated"`, `timestamp`), so the
// notice says what kind of thing changed and how, with a link to open it. The subscription's
// first delivery is a verification token, which the room shows so it can be pasted back into
// Notion. `data` is the payload; `result` is what the room sees (Hookshot's v2 API).
if (typeof data.verification_token === "string") {
  result = {
    version: "v2",
    plain: "Notion sent this subscription's verification token; paste it into the subscription in Notion to finish:\n\n`" + data.verification_token + "`",
    msgtype: "m.notice",
  };
} else if (typeof data.type !== "string") {
  result = { version: "v2", empty: true };
} else {
  const entity = data.entity || {};
  const kind = entity.type || data.type.split(".")[0] || "object";
  const verbs = {
    "created": "created",
    "deleted": "deleted",
    "undeleted": "restored",
    "moved": "moved",
    "content_updated": "content changed",
    "properties_updated": "properties changed",
    "schema_updated": "schema changed",
    "locked": "locked",
    "unlocked": "unlocked",
    "updated": "updated",
  };
  const verb = verbs[data.type.split(".").slice(1).join(".")] || data.type;
  const id = typeof entity.id === "string" ? entity.id : "";
  // A page or database opens at notion.so/<id without dashes>; a comment opens at its page.
  const target = kind === "comment" && data.data && data.data.parent && typeof data.data.parent.id === "string" ? data.data.parent.id : id;
  const link = target ? "https://www.notion.so/" + target.replace(/-/g, "") : null;
  const when = typeof data.timestamp === "string" ? data.timestamp.replace("T", " ").replace(/\.\d+Z$/, " UTC") : "";
  const authors = Array.isArray(data.authors) ? data.authors.filter((a) => a && a.type === "person").length : 0;
  let plain = "Notion: " + kind + " " + verb;
  if (id) {
    plain += " · `" + id + "`";
  }
  if (when) {
    plain += " · " + when;
  }
  if (authors > 1) {
    plain += " · " + authors + " people";
  }
  if (link) {
    plain += " · [open](" + link + ")";
  }
  if (data.data && Array.isArray(data.data.updated_blocks) && data.data.updated_blocks.length) {
    plain += "\n\n" + data.data.updated_blocks.length + " block" + (data.data.updated_blocks.length === 1 ? "" : "s") + " changed";
  }
  result = { version: "v2", plain: plain, msgtype: "m.notice" };
}
