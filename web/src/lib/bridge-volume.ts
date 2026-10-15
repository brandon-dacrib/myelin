import type { BridgeVolume, BridgeVolumeState } from "@/api/bridges";
import type { BadgeProps } from "@/components/ui/badge/Badge";
import { formatBytes } from "@/lib/format";

/**
 * How the pages word a deployed bridge's volume (`BridgeVolume`, OpenAPI 0.1.19). The server
 * judges the state (`nearly_full` from 85% used, `full` from 95% or when the bridge stopped saying
 * it could not write) and words what to do in `message`; this only picks the badge and the
 * numbers.
 */

/** The size a volume gets when nobody sets one (`hs_bridges`' default since 2026-10-15). */
export const DEFAULT_VOLUME_SIZE = "5Gi";

/** The largest size the server takes for a bridge's volume. */
export const MAX_VOLUME_SIZE = "1Ti";

/** Each state's badge, with a label that says what it is about when read on its own. */
export const volumeStateMeta: Record<
  BridgeVolumeState,
  { status: NonNullable<BadgeProps["status"]>; label: string }
> = {
  ok: { status: "success", label: "Volume has room" },
  nearly_full: { status: "warning", label: "Volume nearly full" },
  full: { status: "danger", label: "Volume full" },
  unknown: { status: "muted", label: "Volume not measured yet" },
};

/** Whether a volume is nearly full or full: what a list flags at a glance. */
export function volumeNeedsAttention(
  volume: Pick<BridgeVolume, "state"> | null | undefined,
): volume is BridgeVolume {
  return volume?.state === "full" || volume?.state === "nearly_full";
}

/**
 * What a nearly full or full volume means, in the server's words where it gave some (it always
 * does for those states; the fallback is for a server that did not).
 */
export function volumeMessage(volume: Pick<BridgeVolume, "state" | "message">): string | null {
  if (volume.message) return volume.message;
  if (volume.state === "full")
    return "The bridge's volume is full, so the bridge cannot write and stops. Give it a larger size.";
  if (volume.state === "nearly_full")
    return "The bridge's volume is nearly full; when it fills, the bridge stops. Give it a larger size.";
  return null;
}

/** The share of the volume in use, 0 to 1, or null when it has not been measured. */
export function volumeUsedRatio(
  volume: Pick<BridgeVolume, "used_ratio" | "used_bytes" | "capacity_bytes">,
): number | null {
  if (volume.used_ratio != null) return Math.min(1, Math.max(0, volume.used_ratio));
  if (volume.used_bytes != null && volume.capacity_bytes)
    return Math.min(1, Math.max(0, volume.used_bytes / volume.capacity_bytes));
  return null;
}

/** "1003.5 MiB of 1.0 GiB used (98%)", or null before the first measurement. */
export function describeVolumeUsage(
  volume: Pick<BridgeVolume, "used_ratio" | "used_bytes" | "capacity_bytes">,
): string | null {
  const ratio = volumeUsedRatio(volume);
  if (volume.used_bytes == null || volume.capacity_bytes == null || ratio == null) return null;
  return `${formatBytes(volume.used_bytes)} of ${formatBytes(volume.capacity_bytes)} used (${Math.round(ratio * 100)}%)`;
}

/** The size the volume has now: what the cluster asks for when someone raised it by hand. */
export function currentVolumeSize(volume: Pick<BridgeVolume, "size" | "cluster_size">): string {
  return volume.cluster_size ?? volume.size;
}

const UNIT_BYTES: Record<string, number> = {
  Ki: 2 ** 10,
  Mi: 2 ** 20,
  Gi: 2 ** 30,
  Ti: 2 ** 40,
  Pi: 2 ** 50,
  k: 1e3,
  M: 1e6,
  G: 1e9,
  T: 1e12,
  P: 1e15,
  "": 1,
};

/**
 * A Kubernetes quantity (`5Gi`, `1500M`, `1073741824`) in bytes, or null for one this does not
 * read. Only for comparing sizes on the page; the server decides what it accepts.
 */
export function quantityBytes(quantity: string): number | null {
  const match = /^(\d+(?:\.\d+)?)(Ki|Mi|Gi|Ti|Pi|k|M|G|T|P)?$/.exec(quantity.trim());
  if (!match) return null;
  return Number(match[1]) * UNIT_BYTES[match[2] ?? ""];
}

/**
 * What the size field says before anything is sent: a size written the way the server takes it (a
 * whole number of `Mi`, `Gi` or `Ti`), or why not. Whether it is in range and no smaller than the
 * volume is the server's to say (a 400 at `/volume_size`, shown beside the field); this only saves
 * a round trip for a typo.
 */
export function checkVolumeSize(raw: string): { size: string } | { error: string } {
  const value = raw.trim();
  if (!value) return { error: "Enter a size, such as 10Gi." };
  if (!/^\d+(Mi|Gi|Ti)$/.test(value))
    return { error: "A size is a whole number of Mi, Gi or Ti, such as 10Gi." };
  return { size: value };
}

/** A size one step up from the volume's current one, to suggest in the field: double it. */
export function suggestedVolumeSize(volume: Pick<BridgeVolume, "size" | "cluster_size">): string {
  const current = currentVolumeSize(volume);
  const bytes = quantityBytes(current);
  if (bytes == null) return DEFAULT_VOLUME_SIZE;
  const gib = Math.max(1, Math.ceil((bytes * 2) / 2 ** 30));
  return gib >= 1024 ? MAX_VOLUME_SIZE : `${gib}Gi`;
}
