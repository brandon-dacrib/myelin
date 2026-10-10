/**
 * The Federation page's panel for the destinations this server shares no room with
 * (decision 0042): what the hourly sweep does with them and when
 * (`federation.forget_unused_destinations_after`), and a prune by hand
 * (`POST /federation/destinations/prune`): a preview first (`dry_run`), then forgetting what
 * the preview named. The sweep's own result is one log line, not kept, so the preview is how
 * an operator sees what would go now.
 */
import { useState, type ReactNode } from "react";
import { Link } from "@tanstack/react-router";
import { Eraser } from "lucide-react";
import {
  useForgetAfterSetting,
  usePruneDestinations,
  type DestinationPruneEntry,
  type DestinationPruneReport,
} from "@/api/federation";
import { MutationError } from "@/components/MutationError";
import { Button } from "@/components/ui/button/Button";
import { Select } from "@/components/ui/select/Select";
import { toast } from "@/components/ui/toast/toast-store";
import { hasScope } from "@/lib/auth";
import { settingRowId } from "@/lib/config-model";
import { FORGET_AFTER_SETTING, pruneReasonLabel } from "@/lib/federation";
import { formatCount } from "@/lib/format";

/** The `failing_for` choices: none (destinations with a queue are kept), or a duration. */
const FAILING_FOR_OPTIONS = [
  { value: "none", label: "Keep every server that has a queue" },
  { value: "1d", label: "Failing for a day" },
  { value: "7d", label: "Failing for a week" },
  { value: "30d", label: "Failing for a month" },
];

export function PrunePanel() {
  const canWrite = hasScope("admin:write");
  const { after, configured } = useForgetAfterSetting();
  const prune = usePruneDestinations();
  const [failingFor, setFailingFor] = useState("none");
  const [preview, setPreview] = useState<DestinationPruneReport | null>(null);
  const [done, setDone] = useState<DestinationPruneReport | null>(null);
  const options = { failingFor: failingFor === "none" ? undefined : failingFor };

  function runPreview() {
    setDone(null);
    prune.mutate({ dryRun: true, ...options }, { onSuccess: (report) => setPreview(report) });
  }

  function runPrune() {
    prune.mutate(
      { dryRun: false, ...options },
      {
        onSuccess: (report) => {
          setPreview(null);
          setDone(report);
          toast({
            title: `Forgot ${formatCount(report.forgotten.count)} ${
              report.forgotten.count === 1 ? "server" : "servers"
            }`,
          });
        },
      },
    );
  }

  return (
    <section aria-labelledby="prune-destinations" className="mt-8">
      <h2 id="prune-destinations" className="text-lg text-text">
        Servers this one shares no room with
      </h2>
      <p className="mt-1 max-w-3xl text-sm text-text-muted">
        Every server this one ever sent to stays in the list with its retry state. Once no room has
        users from both, it is only a record: nothing will be sent to it until a room brings the two
        together again, and then it is learned from nothing. Such a server can be forgotten from its
        row, or all of them at once here.{" "}
        {after === null ? (
          <>
            The hourly sweep is <strong className="font-medium text-text">off</strong> (the setting{" "}
            <SettingLink /> is 0), so only a prune here forgets them.
          </>
        ) : (
          <>
            The hourly sweep forgets each one once it has had nothing queued and nothing happen for{" "}
            <strong className="font-medium text-text">{after}</strong>
            {configured ? "" : " (the default)"}, and each one failing that long whose queue is only
            for rooms this server has left; change that with <SettingLink />, or set it to 0 to turn
            the sweep off.
          </>
        )}{" "}
        A server that shares a room is never swept. The sweep logs what it forgets; it keeps no
        report, so preview a prune to see what would go now.
      </p>

      <div className="mt-4 flex flex-wrap items-end gap-3">
        <div className="flex min-w-[16rem] flex-col gap-1.5">
          <label htmlFor="prune-failing-for" className="text-sm font-medium text-text">
            Servers with a queue
          </label>
          <Select
            id="prune-failing-for"
            value={failingFor}
            onValueChange={(value) => {
              setFailingFor(value);
              setPreview(null);
              setDone(null);
            }}
            options={FAILING_FOR_OPTIONS}
            aria-describedby="prune-failing-for-hint"
          />
          <p id="prune-failing-for-hint" className="text-xs text-text-muted">
            A server with events queued is kept, since they will be delivered when it answers,
            unless it has been failing this long and the events are only for rooms this server has
            since left.
          </p>
        </div>
        <Button
          type="button"
          variant="secondary"
          leadingIcon={<Eraser size={16} aria-hidden="true" />}
          disabled={!canWrite || prune.isPending}
          title={!canWrite ? "Needs admin:write" : undefined}
          onClick={runPreview}
        >
          {prune.isPending && prune.variables?.dryRun ? "Looking…" : "Preview prune"}
        </Button>
      </div>

      {prune.error != null && (
        <MutationError
          error={prune.error}
          action={prune.variables?.dryRun ? "preview the prune" : "prune the destinations"}
          className="mt-4 max-w-3xl"
        />
      )}

      {preview && (
        <PruneReportView report={preview} heading="If you prune now">
          {preview.forgotten.count > 0 && (
            <Button
              type="button"
              variant="danger"
              disabled={!canWrite || prune.isPending}
              onClick={runPrune}
            >
              {prune.isPending && !prune.variables?.dryRun
                ? "Forgetting…"
                : `Forget ${formatCount(preview.forgotten.count)} ${
                    preview.forgotten.count === 1 ? "server" : "servers"
                  }`}
            </Button>
          )}
        </PruneReportView>
      )}

      {done && <PruneReportView report={done} heading="Pruned" />}
    </section>
  );
}

function SettingLink() {
  return (
    <Link
      to="/configuration/$section"
      params={{ section: "federation" }}
      hash={settingRowId(FORGET_AFTER_SETTING)}
      className="text-accent underline hover:no-underline"
    >
      Forget unused destinations after
    </Link>
  );
}

/** Both sides of a prune report: how many and why, and the servers named. */
export function PruneReportView({
  report,
  heading,
  children,
}: {
  report: DestinationPruneReport;
  heading: string;
  children?: ReactNode;
}) {
  const forgotten = report.forgotten.count;
  const kept = report.kept.count;
  const verb = report.dry_run
    ? "would be forgotten"
    : report.forgotten.count === 1
      ? "was forgotten"
      : "were forgotten";
  return (
    <div
      role="region"
      aria-label={heading}
      className="mt-4 max-w-3xl rounded-md border border-border bg-surface-raised p-4"
    >
      <h3 className="text-sm font-medium text-text">{heading}</h3>
      <p className="mt-1 text-sm text-text-muted" aria-live="polite">
        {forgotten === 0
          ? report.dry_run
            ? "Nothing would be forgotten: every server either shares a room, has a queue that will be delivered, or has not been quiet for long enough."
            : "Nothing was forgotten."
          : `${formatCount(forgotten)} ${forgotten === 1 ? "server" : "servers"} ${verb}; ${formatCount(kept)} kept.`}
      </p>
      <div className="mt-3 grid gap-4 sm:grid-cols-2">
        <PruneGroupView
          title={report.dry_run ? "Would be forgotten" : "Forgotten"}
          byReason={report.forgotten.by_reason}
          servers={report.forgotten.servers}
          count={forgotten}
        />
        <PruneGroupView
          title="Kept"
          byReason={report.kept.by_reason}
          servers={report.kept.servers}
          count={kept}
        />
      </div>
      {children && <div className="mt-4 flex justify-end">{children}</div>}
    </div>
  );
}

function PruneGroupView({
  title,
  byReason,
  servers,
  count,
}: {
  title: string;
  byReason: Record<string, number>;
  servers: DestinationPruneEntry[];
  count: number;
}) {
  const reasons = Object.entries(byReason).sort((a, b) => b[1] - a[1]);
  return (
    <div>
      <h4 className="text-xs font-medium uppercase tracking-wide text-text-muted">
        {title} ({formatCount(count)})
      </h4>
      {reasons.length === 0 ? (
        <p className="mt-1 text-sm text-text-muted">None.</p>
      ) : (
        <ul className="mt-1 space-y-0.5 text-sm text-text">
          {reasons.map(([reason, n]) => (
            <li key={reason}>
              {formatCount(n)}: {pruneReasonLabel(reason)}
            </li>
          ))}
        </ul>
      )}
      {servers.length > 0 && (
        <details className="mt-2 text-sm">
          <summary className="cursor-pointer text-accent hover:underline">
            {servers.length < count
              ? `The first ${servers.length} of ${formatCount(count)}`
              : `Which ${servers.length === 1 ? "server" : "servers"}`}
          </summary>
          <ul className="mt-1 space-y-1">
            {servers.map((entry) => (
              <li key={entry.server_name} className="text-text-muted">
                <span className="font-identifier text-text">{entry.server_name}</span>:{" "}
                {entry.detail}
              </li>
            ))}
          </ul>
        </details>
      )}
    </div>
  );
}
