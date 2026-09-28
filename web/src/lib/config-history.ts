/**
 * Reading a configuration section's history (`GET /config/{section}/history`)
 * as sentences: "Login · Per second: 0.1 → 0.17".
 *
 * The server sends one row per setting a change touched, with what the
 * database held before and what the change left (`ConfigSettingChange`).
 * Everything here turns those rows into words without knowing any setting by
 * name: labels come from the same schema-built form model the section page
 * renders (`lib/config-model.ts`), so a setting added to the server is named
 * in its history the moment it is named in its form.
 */
import type { ConfigSettingChange, ConfigSettingValue } from "@/api/config";
import { formatValue, humanizeKey, type SettingGroup } from "./config-model";

/**
 * A readable name for every setting in a section's form, keyed by its dotted
 * whole-configuration path (`rate_limits.login.per_second`): the labels of the
 * groups it sits in and its own, joined (`Login · Per second`). The section's
 * own label is left out — the history is on the section's page.
 */
export function settingLabels(model: SettingGroup): Map<string, string> {
  const labels = new Map<string, string>();
  const walk = (group: SettingGroup, trail: string[]) => {
    for (const field of group.fields) {
      labels.set(field.fullPath, [...trail, field.label].join(" · "));
    }
    for (const child of group.groups) walk(child, [...trail, child.label]);
  };
  walk(model, []);
  return labels;
}

/**
 * The label for one changed setting: the form's, or — for a setting the form
 * does not show (a newer server, a section with no schema) — its path made
 * readable, section left out.
 */
export function labelFor(labels: Map<string, string>, path: string): string {
  const known = labels.get(path);
  if (known) return known;
  const [, ...rest] = path.split(".");
  return (rest.length > 0 ? rest : [path]).map(humanizeKey).join(" · ");
}

/** Not stored in the database: the value came from the bootstrap file or the schema default. */
export const UNSET_WORDS = "file or default";

/** One side of a change, in words. `null` is a change recorded before prior values were kept. */
export function describeSide(side: ConfigSettingValue | null, secret: boolean): string {
  if (side === null) return "not recorded";
  if (!side.set) return UNSET_WORDS;
  if (secret) return "set, hidden";
  return formatValue(side.value);
}

export interface ChangeWords {
  from: string;
  to: string;
}

/** What a change did to one setting, as the history shows it. */
export function describeChange(row: ConfigSettingChange): ChangeWords {
  return { from: describeSide(row.from, row.secret), to: describeSide(row.to, row.secret) };
}

/**
 * What *reverting* a change will do to one setting: from what the change left
 * to what was there before it. A secret goes back to its earlier value, which
 * the server restores from its own record — the interface never sees it.
 */
export function describeRevert(row: ConfigSettingChange): ChangeWords {
  if (row.secret && row.from?.set) {
    return { from: describeSide(row.to, true), to: "its earlier value, hidden" };
  }
  return { from: describeSide(row.to, row.secret), to: describeSide(row.from, row.secret) };
}
