/** What the Media page says about its bulk deletions, which run as tasks on the server. */
import { purgeResult, type Task } from "@/api/media";
import { toast } from "@/components/ui/toast/toast-store";
import { formatBytes } from "@/lib/format";
import { describeProgress, describeTaskAction } from "@/lib/tasks";

/** The two bulk deletions the Media page starts; both run as tasks on the server. */
export const BULK_MEDIA_ACTIONS = ["media.delete", "media.purge_remote_cache"] as const;

function plural(n: number, one: string, many: string): string {
  return `${n.toLocaleString()} ${n === 1 ? one : many}`;
}

/** The toast an ended bulk deletion earns: what went, what was kept on purpose, or where it stopped. */
export function announceBulkTask(task: Task) {
  if (task.status === "cancelled") {
    const words = describeProgress(task);
    toast({
      title: `${describeTaskAction(task.action)} stopped`,
      description: words
        ? `It had got through ${words}. What it deleted stays deleted.`
        : "What it deleted stays deleted.",
    });
    return;
  }
  if (task.status === "failed") {
    toast({
      title: `${describeTaskAction(task.action)} failed`,
      description: task.error?.detail ?? undefined,
      variant: "danger",
    });
    return;
  }
  const r = purgeResult(task);
  const kept = [
    r.skipped_protected > 0 && plural(r.skipped_protected, "protected item", "protected items"),
    r.skipped_quarantined > 0 &&
      plural(r.skipped_quarantined, "quarantined copy", "quarantined copies"),
  ].filter(Boolean);
  const notes = [
    kept.length > 0 && `Kept ${kept.join(" and ")}.`,
    r.failed.length > 0 && `${plural(r.failed.length, "item", "items")} could not be deleted.`,
  ].filter(Boolean);
  toast({
    title:
      r.deleted_count === 0
        ? "Nothing matched"
        : `Deleted ${plural(r.deleted_count, "item", "items")} (${formatBytes(r.deleted_bytes)})`,
    description: notes.length > 0 ? notes.join(" ") : undefined,
  });
}
