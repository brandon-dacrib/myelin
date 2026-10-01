/**
 * When a change to a setting takes effect, in the operator's words.
 *
 * The server classifies every setting once (`hs_config::reload::SETTINGS`, decision 0016's
 * 2026-10-01 amendment) and says so per setting (`ConfigSettingInfo.applies`): `hot` (the
 * running server re-reads it when it is saved), `restart` (stored at once, read at the next
 * start) or `bootstrap` (read before the database is open, so set per replica in its file or
 * environment and never stored here). The Configuration pages show one badge per setting from
 * it, explain each class once at the top of a section, and name, after a save, which of the
 * saved settings applied and which wait.
 */
import type { ConfigReloadReport } from "@/api/config";
import type { ConfigSettingInfo } from "@/api/config-schema";

/** `ConfigSettingInfo.applies`. */
export type Applies = "hot" | "restart" | "bootstrap";

export const APPLIES_ORDER: readonly Applies[] = ["hot", "restart", "bootstrap"];

type BadgeStatus = "success" | "neutral" | "muted";

/** The badge and the one-paragraph explanation of each class. */
export const APPLIES_COPY: Record<
  Applies,
  { label: string; status: BadgeStatus; explanation: string }
> = {
  hot: {
    label: "Applies on save",
    status: "success",
    explanation:
      "Saving changes the running server at once: nothing restarts and no one is disconnected. In a cluster the replica that takes the save applies it straight away and the others follow within ten seconds.",
  },
  restart: {
    label: "Needs a restart",
    status: "neutral",
    explanation:
      "Saving stores the new value at once, but the running server keeps using the old one until it next starts, because it is read once at startup. In a cluster, restart the replicas one at a time and nothing goes offline.",
  },
  bootstrap: {
    label: "Per replica (file or environment)",
    status: "muted",
    explanation:
      "Read before the database is open, so it cannot be stored here: each replica takes it from its bootstrap file, an HS__ environment variable or the Helm values, and reads it when it starts. It is shown so you can see what this replica runs with.",
  },
};

function isApplies(value: unknown): value is Applies {
  return value === "hot" || value === "restart" || value === "bootstrap";
}

/** A setting's class from what the server said about it, for a server older than `applies`. */
export function appliesOfInfo(
  info: Pick<ConfigSettingInfo, "applies" | "bootstrap" | "reloadable">,
): Applies {
  if (isApplies(info.applies)) return info.applies;
  if (info.bootstrap) return "bootstrap";
  return info.reloadable ? "hot" : "restart";
}

/**
 * When a change to the setting at `fullPath` (dotted, `rate_limits.login.per_second`) takes
 * effect. The server classifies leaves, and a setting edited as one form (a list of providers,
 * a bucket) may be a leaf or hold several: its own entry first, then the nearest entry above it,
 * then the entries beneath it if they agree. `fallback` is the section's answer, for a server
 * that sent no per-setting rows at all.
 */
export function appliesOf(
  settings: Record<string, ConfigSettingInfo> | undefined,
  fullPath: string,
  fallback: Applies,
): Applies {
  const all = settings ?? {};
  const own = all[fullPath];
  if (own) return appliesOfInfo(own);
  const parts = fullPath.split(".");
  for (let i = parts.length - 1; i > 0; i -= 1) {
    const ancestor = all[parts.slice(0, i).join(".")];
    if (ancestor) return appliesOfInfo(ancestor);
  }
  const beneath = Object.entries(all)
    .filter(([path]) => path.startsWith(`${fullPath}.`))
    .map(([, info]) => appliesOfInfo(info));
  if (beneath.length > 0 && beneath.every((a) => a === beneath[0])) return beneath[0];
  // Mixed beneath: a change may need a restart, so say the cautious thing.
  if (beneath.includes("restart")) return "restart";
  return fallback;
}

/** How many of `paths` fall in each class. */
export function countApplies(
  settings: Record<string, ConfigSettingInfo> | undefined,
  paths: readonly string[],
  fallback: Applies,
): Record<Applies, number> {
  const counts: Record<Applies, number> = { hot: 0, restart: 0, bootstrap: 0 };
  for (const path of paths) counts[appliesOf(settings, path, fallback)] += 1;
  return counts;
}

/** The legend's title: what most of the section does, in one line. */
export function appliesHeadline(counts: Record<Applies, number>): string {
  const administered = counts.hot + counts.restart;
  if (administered === 0) return "Every setting here is set per replica";
  if (counts.restart === 0) return "Every change here applies on save";
  if (counts.hot === 0) return "Changes here take effect at the next restart";
  return counts.hot >= counts.restart
    ? "Most changes here apply on save; some wait for a restart"
    : "Most changes here take effect at the next restart; some apply on save";
}

/** `["a", "b", "c"]` → `"a, b and c"`. */
export function joinLabels(labels: readonly string[]): string {
  if (labels.length <= 1) return labels.join("");
  return `${labels.slice(0, -1).join(", ")} and ${labels[labels.length - 1]}`;
}

/** One saved setting: what the form calls it, and when it applies. */
export interface SavedSetting {
  label: string;
  applies: Applies;
}

/**
 * What a save did, naming each setting: which applied to the running server, which wait for a
 * restart, and whether the running server refused the hot ones (the server's
 * `ConfigReloadReport.errors` at the section's pointer; it then keeps the old values, though
 * the new ones are stored).
 */
export function describeSaveOutcome(
  saved: readonly SavedSetting[],
  report: ConfigReloadReport | null | undefined,
  section: string,
): { description: string; failed: boolean } {
  const hot = saved.filter((s) => s.applies === "hot").map((s) => s.label);
  const later = saved.filter((s) => s.applies !== "hot").map((s) => s.label);
  const failure = report?.errors.find((e) => e.pointer === `/${section}`);
  const sentences: string[] = [];
  if (hot.length > 0) {
    sentences.push(
      failure
        ? `Stored, but the running server could not apply ${joinLabels(hot)} and keeps the old ${hot.length === 1 ? "value" : "values"}: ${failure.detail}`
        : `Applied to the running server: ${joinLabels(hot)}.`,
    );
  }
  if (later.length > 0) {
    sentences.push(`Stored, and waiting for the next restart: ${joinLabels(later)}.`);
  }
  if (sentences.length === 0)
    sentences.push("Stored. The running server already uses these values.");
  return { description: sentences.join(" "), failed: Boolean(failure) && hot.length > 0 };
}

/**
 * `ConfigSection.source`, the highest-precedence layer that sets anything in a section, in
 * words: where its values come from.
 */
export function describeSource(source: string): string {
  switch (source) {
    case "default":
      return "Defaults only";
    case "file":
      return "The bootstrap file";
    case "database":
      return "Saved here (the database)";
    case "environment":
      return "Environment variables";
    default:
      return source;
  }
}
