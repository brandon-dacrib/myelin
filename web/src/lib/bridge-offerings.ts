import type {
  BridgeInstance,
  BridgeInstanceState,
  BridgeOffering,
  BridgeOfferingRequest,
  BridgeType,
} from "@/api/bridges";
import { BRIDGE_INSTANCE_STATES } from "@/api/bridges";
import type { BadgeProps } from "@/components/ui/badge/Badge";

/**
 * What the interface makes of RFC 0017's offerings and instances: a label and a badge for each
 * instance state, a sentence for an offering's instance counts, the runtime in words, the
 * defaults a new offering starts from, and the one line an administrator can tell their users.
 * Pure functions; `bridge-offerings.test.ts`.
 */

export const instanceStateMeta: Record<
  BridgeInstanceState,
  { status: NonNullable<BadgeProps["status"]>; label: string; description: string }
> = {
  requested: {
    status: "info",
    label: "Requested",
    description: "Asked for; the server is about to register it.",
  },
  registered: {
    status: "info",
    label: "Registered",
    description: "Its registration is live; the server is about to run it.",
  },
  deploying: {
    status: "info",
    label: "Deploying",
    description: "The cluster is pulling the image and starting the pod.",
  },
  starting: {
    status: "info",
    label: "Starting",
    description: "Waiting for the bridge to answer this server's ping.",
  },
  ready: { status: "success", label: "Ready", description: "Running and answering this server." },
  failed: { status: "danger", label: "Failed", description: "It stopped on the way; see why." },
  removing: {
    status: "muted",
    label: "Removing",
    description: "Its pod, volume and registration are being removed.",
  },
};

/** The label for a state the server sends that this build does not know yet. */
export function instanceStateLabel(state: string): string {
  return state in instanceStateMeta
    ? instanceStateMeta[state as BridgeInstanceState].label
    : state.charAt(0).toUpperCase() + state.slice(1);
}

export function instanceStateBadge(state: string): NonNullable<BadgeProps["status"]> {
  return state in instanceStateMeta
    ? instanceStateMeta[state as BridgeInstanceState].status
    : "neutral";
}

/** An offering's counts in the order an instance moves, zeroes and unknown states left out. */
export function instanceCountList(
  counts: BridgeOffering["instances"],
): { state: string; count: number }[] {
  const entries = Object.entries(counts ?? {}).filter(([, n]) => n > 0);
  const rank = (s: string) => {
    const i = BRIDGE_INSTANCE_STATES.indexOf(s as BridgeInstanceState);
    return i === -1 ? BRIDGE_INSTANCE_STATES.length : i;
  };
  return entries
    .sort(([a], [b]) => rank(a) - rank(b) || a.localeCompare(b))
    .map(([state, count]) => ({ state, count }));
}

export function totalInstances(counts: BridgeOffering["instances"]): number {
  return Object.values(counts ?? {}).reduce((sum, n) => sum + n, 0);
}

export const runtimeMeta: Record<
  BridgeOffering["runtime"],
  { label: string; description: string }
> = {
  cluster: {
    label: "Runs in this cluster",
    description: "This server deploys each instance as its own pod, with its own volume.",
  },
  elsewhere: {
    label: "Runs elsewhere",
    description:
      "An administrator runs each instance from its files, on a machine that can run it (a Mac, for iMessage).",
  },
};

/** `dock.mau.dev/mautrix/whatsapp:v0.12.1` → `v0.12.1`; `latest` when the image names no tag. */
export function imageTag(image: string | undefined): string {
  if (!image) return "latest";
  const withoutDigest = image.split("@")[0];
  const lastSlash = withoutDigest.lastIndexOf("/");
  const colon = withoutDigest.indexOf(":", lastSlash + 1);
  return colon === -1 ? "latest" : withoutDigest.slice(colon + 1) || "latest";
}

/**
 * Why this server cannot deploy a type itself, when it cannot: the catalogue's own reason
 * (`not_deployable_reason`), or, from a server that gives none, the interface's wording.
 */
export function notDeployableReason(
  type: Pick<BridgeType, "name" | "id" | "not_deployable_reason"> | undefined,
): string {
  if (type?.not_deployable_reason) return type.not_deployable_reason;
  const name = type?.name ?? "This bridge";
  if (type?.id === "mautrix-imessage") {
    return `${name} has to run on a Mac signed in to iMessage, so it always runs elsewhere.`;
  }
  return `${name} needs more than a rendered config to run, so this server can't deploy it; it runs elsewhere.`;
}

/**
 * The options a new offering of `type` starts with: whatever the catalogue says the bridge can
 * do, switched on. A bridge that cannot double-puppet does not pretend to.
 */
export function defaultOfferingOptions(
  type: Pick<BridgeType, "supports_double_puppeting" | "required_features" | "renders_config">,
): Required<NonNullable<BridgeOfferingRequest["options"]>> {
  const e2ee = (type.required_features ?? []).includes("org.matrix.msc3202");
  return {
    encryption: e2ee,
    double_puppeting: Boolean(type.supports_double_puppeting),
    backfill: Boolean(type.renders_config),
  };
}

/** The request that would leave `offering` exactly as it is, for an edit to change one part of. */
export function requestFromOffering(offering: BridgeOffering): Required<BridgeOfferingRequest> {
  return {
    enabled: offering.enabled,
    runtime: offering.runtime,
    // The server says the tag itself; an older one only the image, which names it at the end.
    image_tag: offering.image_tag || imageTag(offering.image),
    access: {
      all_local_users: offering.access?.all_local_users ?? true,
      users: offering.access?.users ?? [],
    },
    options: {
      encryption: offering.options?.encryption ?? false,
      double_puppeting: offering.options?.double_puppeting ?? false,
      backfill: offering.options?.backfill ?? false,
    },
  };
}

/** What the administrator tells people, in one line. */
export function frontDoorSentence(
  offering: Pick<BridgeOffering, "front_door" | "access" | "mode" | "name" | "type">,
): string | null {
  if (offering.mode !== "per_user" || !offering.front_door) return null;
  const who = offering.access?.all_local_users === false ? "Anyone allowed" : "Anyone here";
  const name = offering.name ?? offering.type;
  return `${who} can message ${offering.front_door} to get their own ${name} bridge.`;
}

/** Matrix user IDs, one per line or separated by commas or spaces, deduplicated. */
export function parseUserList(text: string): string[] {
  const ids = text
    .split(/[\s,]+/)
    .map((s) => s.trim())
    .filter(Boolean);
  return [...new Set(ids)];
}

/** Rough shape check for a Matrix user ID; the server has the last word. */
export function looksLikeUserId(value: string): boolean {
  return /^@[^:\s]+:[^\s]+$/.test(value.trim());
}

/** Whether an access choice can be sent as it is: everyone, or at least one well-formed ID. */
export function accessIsValid(allLocalUsers: boolean, users: readonly string[]): boolean {
  if (allLocalUsers) return true;
  return users.length > 0 && users.every(looksLikeUserId);
}

/** The label an instance row goes by: its owner, or the offering itself for a shared one. */
export function instanceOwnerLabel(instance: Pick<BridgeInstance, "user_id">): string {
  return instance.user_id ?? "Everyone (shared)";
}

/**
 * A bridge deployment's phase (`BridgeDeployment.phase`, what the operator reports about the
 * pod) in words, with a badge status. `Ready` is the one that means the bridge is up.
 */
export function deploymentPhaseMeta(phase: string): {
  label: string;
  status: NonNullable<BadgeProps["status"]>;
} {
  switch (phase) {
    case "Ready":
      return { label: "Running", status: "success" };
    case "Pending":
      return { label: "Starting", status: "info" };
    case "Degraded":
      return { label: "Not running properly", status: "danger" };
    default:
      return { label: phase, status: "neutral" };
  }
}

/**
 * What saving an offering's settings does to the bridges people already have, by section of the
 * settings dialog, as `hs_bridges::manager` does it (checked against its tick, 2026-10-08):
 *
 * - **Who can have one** is asked when somebody requests one; nobody's bridge is removed when
 *   they leave the list.
 * - **The image tag and the options** are rendered into every instance's files on the next pass:
 *   a bridge the cluster runs is redeployed and restarts once; one run elsewhere keeps running
 *   its old files until somebody downloads the new ones and restarts it.
 * - **The runtime** is the offering's, read on every pass, not each bridge's: from elsewhere to
 *   the cluster, every bridge people have is deployed into the cluster on the next pass (a copy
 *   still run by hand then runs twice); from the cluster to elsewhere, the pods keep running and
 *   later changes no longer reach them.
 *
 * `existing` is how many bridges people have. `runtimeChange` is `null` when the runtime is
 * unchanged or nobody has a bridge yet.
 */
export interface SettingsEffects {
  access: string;
  imageAndOptions: string;
  runtimeChange: string | null;
}

export function settingsEffects(
  savedRuntime: BridgeOfferingRequest["runtime"],
  draftRuntime: BridgeOfferingRequest["runtime"],
  existing: number,
): SettingsEffects {
  const access =
    "Applies to people who ask from now on. Somebody you take off the list keeps the bridge they have; remove it from the table on this page.";
  const imageAndOptions =
    existing === 0
      ? "Applies to every bridge created after saving, and to any people already have."
      : savedRuntime === "cluster"
        ? `Applies to ${bridgesPeopleHave(existing)} as well as to new ones: the server redeploys each with its new files, and it restarts once.`
        : `Applies to new bridges at once. ${capitalise(bridgesPeopleHave(existing))} run elsewhere: each keeps its old settings until its files are downloaded again (Files on its row) and it is restarted with them.`;
  let runtimeChange: string | null = null;
  if (existing > 0 && savedRuntime !== draftRuntime) {
    runtimeChange =
      draftRuntime === "cluster"
        ? `Saving moves ${bridgesPeopleHave(existing)} too: the server deploys each into the cluster on its next pass. Stop any copy run by hand first, or it runs twice with the same registration.`
        : `Saving does not move ${bridgesPeopleHave(existing)}: the ones in the cluster keep running there, and later changes to the image or options no longer reach them. To move one, remove it and add it again.`;
  }
  return { access, imageAndOptions, runtimeChange };
}

function bridgesPeopleHave(n: number): string {
  return n === 1 ? "the 1 bridge people already have" : `the ${n} bridges people already have`;
}

function capitalise(s: string): string {
  return s.charAt(0).toUpperCase() + s.slice(1);
}

/**
 * The server's reason it cannot deploy bridges, as a sentence that follows "This server can't run
 * bridges itself.": the reason starts with a capital and ends with a full stop, and the sentence
 * about running them elsewhere is added only when the reason did not already say so (the
 * server's own reason does, and the page used to say it twice).
 */
export function cannotRunBridgesWhy(reason: string | null | undefined): string {
  const given = (reason ?? "").trim();
  if (!given) {
    return "It isn't running in Kubernetes with the chart's bridges enabled, so each bridge offered here runs elsewhere, from the files on its page.";
  }
  const sentence =
    given.charAt(0).toUpperCase() + given.slice(1) + (/[.!?]$/.test(given) ? "" : ".");
  return /elsewhere/.test(given)
    ? sentence
    : `${sentence} Each bridge offered here runs elsewhere, from the files on its page.`;
}
