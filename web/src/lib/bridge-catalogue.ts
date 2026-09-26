import {
  deriveDisplayName,
  deriveKindLabel,
  type AppService,
  type AppServiceHealthStatus,
  type BridgeType,
} from "@/api/bridges";

/**
 * What the interface makes of the bridge catalogue (`GET /bridge-types`) and of an appservice's
 * `bridge_type` link back into it: grouping for the wizard, a title and a kind for a row, the
 * bot's Matrix ID, the sign-in steps with the bot's name filled in, and the order a list should
 * be read in (what needs attention first). Pure functions; `bridge-catalogue.test.ts`.
 */

export const CATEGORY_ORDER = ["messaging", "social", "irc", "integrations"] as const;
export type BridgeCategory = (typeof CATEGORY_ORDER)[number];

export const CATEGORY_META: Record<BridgeCategory, { label: string; blurb: string }> = {
  messaging: { label: "Messaging", blurb: "Personal chats tied to a phone or an account." },
  social: { label: "Communities and work", blurb: "Servers, workspaces and direct messages." },
  irc: { label: "IRC", blurb: "Channels and networks, in two styles." },
  integrations: { label: "Integrations", blurb: "Services that post into rooms." },
};

const OTHER = { label: "Other", blurb: "" };

function isCategory(value: unknown): value is BridgeCategory {
  return typeof value === "string" && (CATEGORY_ORDER as readonly string[]).includes(value);
}

export interface BridgeTypeGroup {
  category: BridgeCategory | "other";
  label: string;
  blurb: string;
  types: BridgeType[];
}

function matches(type: BridgeType, needle: string): boolean {
  if (!needle) return true;
  const haystack = [type.name, type.description, type.id, type.upstream_project]
    .filter((s): s is string => typeof s === "string")
    .join(" ")
    .toLowerCase();
  return haystack.includes(needle);
}

/** The catalogue in the wizard's order: by category, empty groups dropped, a free-text filter applied. */
export function groupByCategory(types: BridgeType[], query = ""): BridgeTypeGroup[] {
  const needle = query.trim().toLowerCase();
  const groups: BridgeTypeGroup[] = CATEGORY_ORDER.map((category) => ({
    category,
    ...CATEGORY_META[category],
    types: [],
  }));
  const other: BridgeTypeGroup = { category: "other", ...OTHER, types: [] };
  for (const type of types) {
    if (!matches(type, needle)) continue;
    const group = isCategory(type.category)
      ? groups.find((g) => g.category === type.category)
      : undefined;
    (group ?? other).types.push(type);
  }
  return [...groups, other].filter((g) => g.types.length > 0);
}

/** The catalogue entry an appservice was created from, if it was and the catalogue is loaded. */
export function bridgeTypeOf(
  appservice: Pick<AppService, "bridge_type">,
  types: BridgeType[] | undefined,
): BridgeType | undefined {
  if (!appservice.bridge_type || !types) return undefined;
  return types.find((t) => t.id === appservice.bridge_type);
}

/** `mautrix-whatsapp` → `whatsapp`, `matrix-hookshot` → `hookshot`. */
export function shortTypeId(typeId: string): string {
  return typeId.replace(/^mautrix-/, "").replace(/^matrix-/, "");
}

/**
 * What to call a bridge in a heading. The catalogue's name when the operator kept the default
 * id (`whatsapp` for `mautrix-whatsapp`), the humanised id otherwise (`Work Whatsapp` for
 * `work-whatsapp`) with the kind shown beside it.
 */
export function bridgeTitle(
  appservice: Pick<AppService, "id" | "bridge_type">,
  type: Pick<BridgeType, "id" | "name"> | undefined,
): string {
  const id = appservice.id ?? "";
  if (type?.id && type.name && (id === type.id || id === shortTypeId(type.id))) return type.name;
  return deriveDisplayName(appservice);
}

/** The kind column: the catalogue's name, or what the registration's `protocols` say. */
export function bridgeKind(
  appservice: Pick<AppService, "protocols" | "bridge_type">,
  type: Pick<BridgeType, "name"> | undefined,
): string {
  return type?.name ?? deriveKindLabel(appservice);
}

/** `@whatsappbot:example.org`, or `@whatsappbot` while the server's name is not known yet. */
export function botMatrixId(senderLocalpart: string | undefined, serverName: string | undefined) {
  const localpart = senderLocalpart ?? "";
  return serverName ? `@${localpart}:${serverName}` : `@${localpart}`;
}

/** The catalogue's sign-in steps with `{bot}` replaced by the bridge bot's Matrix ID. */
export function signInSteps(type: Pick<BridgeType, "sign_in"> | undefined, botId: string) {
  return (type?.sign_in?.steps ?? []).map((step) => step.replaceAll("{bot}", botId));
}

/** Lower reads first: what is broken, then what is unsure, then what is fine, then what was set aside. */
export const ATTENTION_RANK: Record<AppServiceHealthStatus, number> = {
  down: 0,
  degraded: 1,
  unknown: 2,
  healthy: 3,
  paused: 4,
};

function rankOf(appservice: Pick<AppService, "health" | "paused">): number {
  if (appservice.paused) return ATTENTION_RANK.paused;
  return ATTENTION_RANK[appservice.health ?? "unknown"];
}

/** The list's default order (information-architecture.md, Bridges: "attention first"), ties by id. */
export function sortByAttention<T extends Pick<AppService, "health" | "paused" | "id">>(
  items: T[],
): T[] {
  return [...items].sort((a, b) => rankOf(a) - rankOf(b) || (a.id ?? "").localeCompare(b.id ?? ""));
}

/** How many bridges are in each state, `paused` counted as paused whatever its stale `health` says. */
export function healthCounts(
  items: Pick<AppService, "health" | "paused">[],
): Record<AppServiceHealthStatus, number> {
  const counts: Record<AppServiceHealthStatus, number> = {
    healthy: 0,
    degraded: 0,
    down: 0,
    paused: 0,
    unknown: 0,
  };
  for (const item of items) {
    if (item.paused) counts.paused += 1;
    else counts[item.health ?? "unknown"] += 1;
  }
  return counts;
}
