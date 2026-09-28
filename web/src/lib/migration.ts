/** Words and small calculations for the Migration page (`pages/migration/MigrationPage.tsx`). */
import type { MigrationPhase, MigrationStream } from "@/api/migration";

type BadgeStatus = "success" | "warning" | "danger" | "info" | "muted" | "neutral";

/** How each status of `MigrationStatus.status` is shown. */
export const MIGRATION_STATUS_META: Record<MigrationPhase, { label: string; status: BadgeStatus }> =
  {
    idle: { label: "Not started", status: "neutral" },
    copying: { label: "Copying", status: "info" },
    paused: { label: "Paused", status: "muted" },
    ready_for_cutover: { label: "Ready for cutover", status: "success" },
    cutting_over: { label: "Cutting over", status: "info" },
    verifying: { label: "Verifying", status: "info" },
    completed: { label: "Completed", status: "success" },
    failed: { label: "Stopped on an error", status: "danger" },
    aborted: { label: "Aborted", status: "warning" },
  };

/** What each stream copies, in the operator's words. */
export const STREAM_LABELS: Record<string, string> = {
  users: "Accounts",
  devices: "Devices",
  access_tokens: "Sessions (access tokens)",
  account_data: "Account data and room tags",
  rooms: "Rooms",
  events: "Room events",
  media: "Media",
};

/** How far a stream has got, `null` before it has been counted. */
export function streamFraction(stream: MigrationStream): number | null {
  if (stream.done) return 1;
  const total = stream.total_count;
  if (total == null) return null;
  if (total === 0) return 1;
  const handled =
    (stream.copied_count ?? 0) + (stream.skipped_count ?? 0) + (stream.failed_count ?? 0);
  return Math.min(1, handled / total);
}

/** `93_000` → `"2 minutes"`; rounded the way a person would say it. */
export function formatDuration(ms: number): string {
  const seconds = Math.round(ms / 1000);
  if (seconds < 60) return seconds <= 1 ? "a second" : `${seconds} seconds`;
  const minutes = Math.round(seconds / 60);
  if (minutes < 60) return minutes === 1 ? "a minute" : `${minutes} minutes`;
  const hours = Math.round(minutes / 60);
  return hours === 1 ? "an hour" : `${hours} hours`;
}
