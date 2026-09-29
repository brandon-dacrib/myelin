/**
 * Which setting changed, who changed it, when — and a way back.
 *
 * Reads `GET /config/{section}/history` (`config.history.list`): every write
 * to the section, newest first, one row per setting it touched with what the
 * database held before and what the write left. Secrets arrive redacted on
 * both sides, so a row for one says only that it changed.
 *
 * Revert is `POST /config/{section}/history/{revision}/revert`: the server
 * puts the settings back from its own record — a secret included, which is
 * why the interface never needs to hold one — as a new revision. The dialog
 * says what will change before anything is sent; a `409` (a later change
 * wrote the same settings) is shown in the same dialog with what it would
 * also undo, and the operator may go ahead anyway.
 *
 * The page of history is in the URL (`?history=<cursor>`), so it survives a
 * reload and can be linked to.
 */
import { useMemo, useState } from "react";
import { ArrowRight, History, RotateCcw, TriangleAlert } from "lucide-react";
import {
  describeApplied,
  useConfigHistory,
  useRevertConfigChange,
  type ConfigChange,
  type ConfigSettingChange,
} from "@/api/config";
import { classifyError, type Problem } from "@/api/problem";
import { QueryProblemState } from "@/components/QueryProblemState";
import { RelativeTime } from "@/components/RelativeTime";
import { Badge } from "@/components/ui/badge/Badge";
import { Button } from "@/components/ui/button/Button";
import { Dialog, DialogClose, DialogContent } from "@/components/ui/dialog/Dialog";
import { SkeletonText } from "@/components/ui/skeleton/Skeleton";
import { toast } from "@/components/ui/toast/toast-store";
import {
  describeChange,
  describeRevert,
  labelFor,
  settingLabels,
  type ChangeWords,
} from "@/lib/config-history";
import type { SettingGroup } from "@/lib/config-model";

export interface ConfigHistoryProps {
  section: string;
  /** The section's form model, for naming settings the way the form does. */
  model: SettingGroup;
  /** The section's current `ETag`, sent as `If-Match` with a revert. */
  etag: string | null;
  canWrite: boolean;
  isHot: (path: string) => boolean;
  /** The page of history to show (`?history=`); newest when absent. */
  cursor?: string;
  onCursorChange: (cursor: string | undefined) => void;
}

export function ConfigHistory({
  section,
  model,
  etag,
  canWrite,
  isHot,
  cursor,
  onCursorChange,
}: ConfigHistoryProps) {
  const { data, isLoading, isError, error, refetch, isFetching } = useConfigHistory(
    section,
    cursor,
  );
  const labels = useMemo(() => settingLabels(model), [model]);
  const [reverting, setReverting] = useState<ConfigChange | null>(null);

  return (
    <section aria-labelledby="config-history-heading">
      <h2
        id="config-history-heading"
        className="flex items-center gap-2 text-md font-medium text-text"
      >
        <History size={16} aria-hidden="true" className="text-text-muted" />
        Change history
      </h2>

      {isError ? (
        <div className="mt-3">
          <QueryProblemState
            error={error}
            resource="this section's history"
            compact
            onRetry={() => refetch()}
          />
        </div>
      ) : isLoading ? (
        <div className="mt-3">
          <SkeletonText lines={3} />
        </div>
      ) : (data?.items.length ?? 0) === 0 ? (
        <p className="mt-3 text-sm text-text-muted">
          {cursor ? "No older changes." : "Nothing has changed this section yet."}
        </p>
      ) : (
        <ol className="mt-3 divide-y divide-border rounded-md border border-border">
          {data?.items.map((change) => (
            <li key={change.revision} className="px-4 py-3">
              <div className="flex flex-wrap items-center gap-x-3 gap-y-1">
                <span className="font-identifier text-sm text-text">
                  {change.actor ?? "unknown"}
                </span>
                <span className="text-xs text-text-muted">
                  <RelativeTime at={change.at} />
                </span>
                <span className="text-xs text-text-faint">revision {change.revision}</span>
                {change.reverts !== null && (
                  <Badge status="info" hideIcon>
                    Reverts revision {change.reverts}
                  </Badge>
                )}
                {canWrite && change.revertible && change.settings.length > 0 && (
                  <Button
                    variant="ghost"
                    size="sm"
                    className="ml-auto"
                    aria-label={`Revert revision ${change.revision}`}
                    onClick={() => setReverting(change)}
                  >
                    <RotateCcw size={14} aria-hidden="true" />
                    Revert
                  </Button>
                )}
                {!change.revertible && (
                  <span className="ml-auto text-xs text-text-faint">
                    Earlier values not recorded
                  </span>
                )}
              </div>
              <ul className="mt-1.5 flex flex-col gap-1">
                {change.settings.map((row) => (
                  <SettingLine
                    key={row.pointer}
                    label={labelFor(labels, row.path)}
                    words={describeChange(row)}
                    path={row.path}
                  />
                ))}
              </ul>
            </li>
          ))}
        </ol>
      )}

      {(cursor || data?.next_cursor) && (
        <nav aria-label="Change history pages" className="mt-3 flex justify-between gap-2">
          <Button
            variant="secondary"
            size="sm"
            disabled={!cursor || isFetching}
            onClick={() => onCursorChange(data?.prev_cursor ?? undefined)}
          >
            Newer changes
          </Button>
          <Button
            variant="secondary"
            size="sm"
            disabled={!data?.next_cursor || isFetching}
            onClick={() => onCursorChange(data?.next_cursor ?? undefined)}
          >
            Older changes
          </Button>
        </nav>
      )}

      {reverting && (
        <RevertDialog
          change={reverting}
          section={section}
          labels={labels}
          etag={etag}
          isHot={isHot}
          onClose={() => setReverting(null)}
          onReverted={() => onCursorChange(undefined)}
        />
      )}
    </section>
  );
}

function SettingLine({ label, words, path }: { label: string; words: ChangeWords; path: string }) {
  return (
    <li className="flex flex-wrap items-baseline gap-x-2 text-sm" title={path}>
      <span className="text-text">{label}:</span>
      <span className="font-identifier text-text-muted">{words.from}</span>
      <ArrowRight size={12} aria-hidden="true" className="self-center text-text-faint" />
      <span className="sr-only">to</span>
      <span className="font-identifier text-text">{words.to}</span>
    </li>
  );
}

interface RevertDialogProps {
  change: ConfigChange;
  section: string;
  labels: Map<string, string>;
  etag: string | null;
  isHot: (path: string) => boolean;
  onClose: () => void;
  onReverted: () => void;
}

function RevertDialog({
  change,
  section,
  labels,
  etag,
  isHot,
  onClose,
  onReverted,
}: RevertDialogProps) {
  const revert = useRevertConfigChange();
  const hotCount = change.settings.filter((row) => isHot(row.path)).length;
  const allHot = hotCount === change.settings.length;
  // A 409 from the first attempt: later changes wrote the same settings.
  const [conflict, setConflict] = useState<Problem | null>(null);

  function submit() {
    revert.mutate(
      { section, revision: change.revision, etag, force: conflict !== null },
      {
        onSuccess: (result) => {
          const outcome = describeApplied(result.section.applied, section, allHot);
          toast({
            title: `Revision ${change.revision} reverted`,
            description: outcome.description,
            variant: outcome.failed ? "danger" : undefined,
          });
          onClose();
          onReverted();
        },
        onError: (err) => {
          const { kind, problem } = classifyError(err);
          if (problem?.status === 409 && conflict === null && (problem.errors?.length ?? 0) > 0) {
            setConflict(problem);
            return;
          }
          onClose();
          if (problem?.status === 412) {
            toast({
              title: "Someone else changed this section",
              description: "The page now shows their change. Look again before reverting.",
              variant: "danger",
            });
            return;
          }
          toast({
            title:
              kind === "forbidden"
                ? "Not allowed to change this section"
                : (problem?.title ?? "Could not revert"),
            description: problem?.detail,
            variant: "danger",
          });
        },
      },
    );
  }

  return (
    <Dialog open onOpenChange={(open) => !open && onClose()}>
      <DialogContent
        size="form"
        title={`Revert revision ${change.revision}?`}
        description={
          allHot
            ? "These settings go back to what they were before this change, on the running server straight away."
            : hotCount > 0
              ? "These settings go back to what they were before this change. Some apply now; the rest take effect the next time the server restarts."
              : "These settings go back to what they were before this change. It takes effect the next time the server restarts."
        }
        footer={
          <>
            <DialogClose asChild>
              <Button variant="secondary">Keep as it is</Button>
            </DialogClose>
            <Button
              variant={conflict ? "danger" : "primary"}
              disabled={revert.isPending}
              onClick={submit}
            >
              {revert.isPending ? "Reverting…" : conflict ? "Revert anyway" : "Revert"}
            </Button>
          </>
        }
      >
        <ul className="flex flex-col gap-1 rounded-md border border-border px-3 py-2.5">
          {change.settings.map((row: ConfigSettingChange) => (
            <SettingLine
              key={row.pointer}
              label={labelFor(labels, row.path)}
              words={describeRevert(row)}
              path={row.path}
            />
          ))}
        </ul>
        {conflict && (
          <div
            role="alert"
            className="mt-4 flex items-start gap-2 rounded-md border border-warning-border bg-warning-bg p-3 text-sm text-warning"
          >
            <TriangleAlert size={16} aria-hidden="true" className="mt-0.5 shrink-0" />
            <div>
              <p>Later changes wrote some of the same settings. Reverting undoes them too:</p>
              <ul className="mt-1 list-disc pl-5">
                {conflict.errors?.map((e, i) => (
                  <li key={`${e.pointer}-${i}`}>
                    {labelFor(labels, pointerToPath(e.pointer))} — {e.detail}
                  </li>
                ))}
              </ul>
            </div>
          </div>
        )}
      </DialogContent>
    </Dialog>
  );
}

/** `/rate_limits/login/per_second` → `rate_limits.login.per_second`. */
function pointerToPath(pointer: string): string {
  return pointer
    .split("/")
    .slice(1)
    .map((t) => t.replace(/~1/g, "/").replace(/~0/g, "~"))
    .join(".");
}
