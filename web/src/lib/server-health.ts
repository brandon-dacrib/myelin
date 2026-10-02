/**
 * Words for `GET /server/health` (`api/dashboard.ts::useServerHealth`): the server answers with
 * check names it chose (`audit`, `events`, `users`, ...) and one of `ok`, `unknown` or `down` for
 * each; the Overview says what each check is and what its state means.
 */

/** The checks the server reports today, in an operator's words; anything else is humanised. */
const CHECK_LABELS: Record<string, string> = {
  audit: "Audit log",
  events: "Event stream",
  users: "User directory",
  storage: "Storage",
  federation: "Federation",
  media: "Media store",
  search: "Search index",
  cluster: "Cluster",
};

/** `search_index` → "Search index"; a known key gets its label. */
export function checkLabel(key: string): string {
  const known = CHECK_LABELS[key];
  if (known) return known;
  const words = key.replace(/[_.-]+/g, " ").trim();
  return words ? words.charAt(0).toUpperCase() + words.slice(1) : key;
}

export type CheckStatus = "success" | "warning" | "danger" | "muted";

/** The badge status for a check's or the whole server's state. */
export function checkStatus(state: string | undefined): CheckStatus {
  switch (state) {
    case "ok":
      return "success";
    case "degraded":
    case "unknown":
      return "warning";
    case "down":
      return "danger";
    default:
      return "muted";
  }
}

/** The one-word badge for a check's state. */
export function checkWord(state: string | undefined): string {
  switch (state) {
    case "ok":
      return "Ok";
    case "unknown":
      return "Unknown";
    case "down":
      return "Down";
    case "degraded":
      return "Degraded";
    default:
      return state ?? "—";
  }
}

/** What a check's state means for the operator, in a sentence. */
export function checkMeaning(state: string | undefined): string {
  switch (state) {
    case "ok":
      return "Answering.";
    case "unknown":
      return "Cannot be checked: the part of the server that would answer is not wired up here.";
    case "down":
      return "Not answering. The server cannot do what depends on it.";
    default:
      return "The server reported a state this interface does not know.";
  }
}

/** The whole answer in one sentence, for the card's header and the Attention row. */
export function healthSummary(
  status: string | undefined,
  checks: Record<string, string> | undefined,
): string {
  const entries = Object.entries(checks ?? {});
  const notOk = entries.filter(([, state]) => state !== "ok");
  if (status === "ok") {
    return entries.length === 0
      ? "Every probe answered."
      : `Every probe answered: ${entries.map(([key]) => checkLabel(key).toLowerCase()).join(", ")}.`;
  }
  const named = notOk
    .map(([key, state]) => `${checkLabel(key).toLowerCase()} ${checkWord(state).toLowerCase()}`)
    .join(", ");
  if (status === "down") return `Server health is down: ${named || "a probe is down"}.`;
  if (status === "degraded") return `Server health is degraded: ${named || "a probe is not ok"}.`;
  return `Server health is ${status ?? "unknown"}.`;
}
