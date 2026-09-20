/**
 * The step between typing and saving: exactly what is about to change, what
 * the server thinks of it, and whether it takes effect now or at the next
 * restart.
 *
 * `POST /config/validate` is run from here rather than on every keystroke —
 * it is a server round trip, and the moment an operator wants it is the
 * moment before they commit.
 */
import { ArrowRight, CircleCheck, TriangleAlert } from "lucide-react";
import type { JsonValue } from "@/api/config-schema";
import type { ConfigValidateReport } from "@/api/config";
import { formatValue, type ChangeEntry } from "@/lib/config-model";
import { Button } from "@/components/ui/button/Button";
import { Dialog, DialogClose, DialogContent } from "@/components/ui/dialog/Dialog";

export interface ChangeReviewProps {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  sectionLabel: string;
  reloadable: boolean;
  changes: ChangeEntry[];
  patch: Record<string, JsonValue>;
  report?: ConfigValidateReport;
  validating: boolean;
  saving: boolean;
  onValidate: () => void;
  onSave: () => void;
}

export function ChangeReview({
  open,
  onOpenChange,
  sectionLabel,
  reloadable,
  changes,
  patch,
  report,
  validating,
  saving,
  onValidate,
  onSave,
}: ChangeReviewProps) {
  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent
        size="form"
        title={`Review ${changes.length} change${changes.length === 1 ? "" : "s"} to ${sectionLabel}`}
        description={
          reloadable
            ? "This section is reloadable: saving applies it to the running server straight away."
            : "This section is not reloadable. Saving stores the change; it takes effect the next time the server restarts."
        }
        footer={
          <>
            <DialogClose asChild>
              <Button variant="secondary">Keep editing</Button>
            </DialogClose>
            <Button variant="secondary" disabled={validating} onClick={onValidate}>
              {validating ? "Checking…" : "Check without saving"}
            </Button>
            <Button disabled={saving || changes.length === 0} onClick={onSave}>
              {saving ? "Saving…" : "Save changes"}
            </Button>
          </>
        }
      >
        <ChangeList changes={changes} />

        {report && (
          <div
            className={`mt-4 flex items-start gap-2 rounded-md border p-3 text-sm ${
              report.valid
                ? "border-success-border bg-success-bg text-success"
                : "border-danger-border bg-danger-bg text-danger"
            }`}
          >
            {report.valid ? (
              <CircleCheck size={16} aria-hidden="true" className="mt-0.5 shrink-0" />
            ) : (
              <TriangleAlert size={16} aria-hidden="true" className="mt-0.5 shrink-0" />
            )}
            <div>
              {report.valid ? (
                <>
                  <p>The server accepts this configuration.</p>
                  {report.requires_restart.length > 0 && (
                    <p className="mt-1">
                      It cannot be applied without a restart:{" "}
                      <span className="font-identifier">{report.requires_restart.join(", ")}</span>.
                    </p>
                  )}
                </>
              ) : (
                <>
                  <p>The server would reject this:</p>
                  <ul className="mt-1 list-disc pl-5">
                    {report.errors.map((e, i) => (
                      <li key={`${e.pointer}-${i}`}>
                        <span className="font-identifier">{e.pointer}</span> — {e.detail}
                      </li>
                    ))}
                  </ul>
                </>
              )}
            </div>
          </div>
        )}

        <details className="mt-4">
          <summary className="cursor-pointer text-xs text-accent hover:underline">
            Show the JSON Merge Patch this sends
          </summary>
          <pre className="mt-2 overflow-x-auto rounded-sm border border-border bg-surface-sunken p-3 font-mono text-xs text-text">
            {JSON.stringify(patch, null, 2)}
          </pre>
        </details>
      </DialogContent>
    </Dialog>
  );
}

/** The diff itself. Also used inline on the page, above the form, while edits are pending. */
export function ChangeList({ changes }: { changes: ChangeEntry[] }) {
  if (changes.length === 0) {
    return <p className="text-sm text-text-muted">Nothing has changed.</p>;
  }
  return (
    <ul className="divide-y divide-border rounded-md border border-border">
      {changes.map((change) => (
        <li key={change.path} className="px-3 py-2.5">
          <p className="text-sm font-medium text-text">{change.label}</p>
          <p className="font-identifier text-xs text-text-faint">{change.path}</p>
          <p className="mt-1.5 flex flex-wrap items-center gap-2 text-sm">
            <span className="font-identifier text-text-muted line-through">
              {formatValue(change.from)}
            </span>
            <ArrowRight size={14} aria-hidden="true" className="text-text-faint" />
            <span className="font-identifier text-text">
              {change.isReset
                ? `default${change.defaultValue === undefined ? "" : ` (${formatValue(change.defaultValue)})`}`
                : formatValue(change.to)}
            </span>
          </p>
        </li>
      ))}
    </ul>
  );
}
