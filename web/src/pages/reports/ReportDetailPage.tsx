import { useState, type ReactNode } from "react";
import { ChevronLeft } from "lucide-react";
import { Link, useNavigate, useParams, useSearch } from "@tanstack/react-router";
import {
  useDeleteReport,
  useReport,
  useReports,
  useResolveReport,
  type Report,
  type ReportResolution,
} from "@/api/reports";
import { CopyableId } from "@/components/CopyableId";
import { MutationError } from "@/components/MutationError";
import { QueryProblemState } from "@/components/QueryProblemState";
import { RelativeTime } from "@/components/RelativeTime";
import { ResourceLink } from "@/components/ResourceLink";
import { Badge } from "@/components/ui/badge/Badge";
import { Button } from "@/components/ui/button/Button";
import { Dialog, DialogClose, DialogContent, DialogTrigger } from "@/components/ui/dialog/Dialog";
import { ForbiddenState } from "@/components/ui/error-state/ErrorState";
import { Field, Textarea } from "@/components/ui/input/Input";
import { SkeletonText } from "@/components/ui/skeleton/Skeleton";
import { toast } from "@/components/ui/toast/toast-store";
import { hasScope } from "@/lib/auth";
import {
  describeScore,
  REPORT_KIND_LABELS,
  REPORT_STATUS_META,
  reportSubject,
  resolutionLabel,
  RESOLUTIONS,
} from "@/lib/reports";

/**
 * `/reports/$reportId`: one report, the reported message as the server holds it now, who is
 * involved (each a link to the page where something can be done about them), and the decision:
 * a resolution and a note while it is open, the record of what was decided once it is closed.
 */
export function ReportDetailPage() {
  const { reportId } = useParams({ from: "/reports/$reportId" });
  const search = useSearch({ from: "/reports/$reportId" });
  const query = useReport(reportId);
  const report = query.data;

  return (
    <div className="mx-auto max-w-5xl space-y-6 p-6">
      <Link
        to="/reports"
        search={search}
        className="inline-flex items-center gap-1 text-sm text-text-muted hover:text-text"
      >
        <ChevronLeft size={14} aria-hidden="true" />
        Reports
      </Link>
      {!hasScope("moderation:read") ? (
        <ForbiddenState scope="moderation:read" />
      ) : query.isLoading ? (
        <SkeletonText lines={6} />
      ) : query.isError || !report ? (
        <QueryProblemState
          error={query.error}
          resource="this report"
          scope="moderation:read"
          onRetry={() => query.refetch()}
        />
      ) : (
        <ReportView report={report} />
      )}
    </div>
  );
}

function ReportView({ report }: { report: Report }) {
  const statusMeta = REPORT_STATUS_META[report.status];
  const canWrite = hasScope("moderation:write");
  return (
    <>
      <header className="flex flex-wrap items-start justify-between gap-4">
        <div>
          <h1 className="break-all text-xl text-text">{reportSubject(report)}</h1>
          <div className="mt-2 flex flex-wrap gap-2">
            <Badge status={statusMeta.status}>{statusMeta.label}</Badge>
            <Badge status="neutral" hideIcon>
              {REPORT_KIND_LABELS[report.kind]} report
            </Badge>
          </div>
        </div>
        {canWrite && <DeleteReportButton report={report} />}
      </header>

      <section aria-labelledby="report-reason-heading" className="space-y-2">
        <h2 id="report-reason-heading" className="text-md font-medium text-text">
          What the reporter said
        </h2>
        {report.reason ? (
          <blockquote className="whitespace-pre-wrap rounded-md border-l-4 border-border-strong bg-surface p-4 text-base text-text">
            {report.reason}
          </blockquote>
        ) : (
          <p className="text-sm text-text-muted">They gave no reason.</p>
        )}
      </section>

      {report.kind === "event" && <ReportedEvent report={report} />}

      <dl className="grid gap-4 rounded-md border border-border bg-surface p-4 text-sm sm:grid-cols-2 [&_dd]:mt-1 [&_dd]:break-all [&_dd]:text-text [&_dt]:text-text-muted">
        <Fact label="Reported by">
          <ResourceLink target={{ type: "user", id: report.reporter_id }} />
          <Link
            to="/reports"
            search={{ status: "all", reporter_id: report.reporter_id }}
            className="mt-1 block text-accent hover:underline"
          >
            Every report they filed
          </Link>
        </Fact>
        {report.reported_user_id && (
          <Fact label={report.kind === "user" ? "Reported user" : "Sender"}>
            <ResourceLink target={{ type: "user", id: report.reported_user_id }} />
          </Fact>
        )}
        {report.room_id && (
          <Fact label="Room">
            <ResourceLink target={{ type: "room", id: report.room_id }} />
            <Link
              to="/reports"
              search={{ status: "all", room_id: report.room_id }}
              className="mt-1 block text-accent hover:underline"
            >
              Every report about this room
            </Link>
          </Fact>
        )}
        <Fact label="Received">
          <RelativeTime at={report.received_at} />
        </Fact>
        {report.kind === "event" && <Fact label="Score">{describeScore(report.score)}</Fact>}
        {report.event_id && (
          <Fact label="Event ID">
            <CopyableId value={report.event_id} />
          </Fact>
        )}
        <Fact label="Report ID">
          <CopyableId value={report.id} />
        </Fact>
      </dl>

      {report.reported_user_id && (
        <OtherReports userId={report.reported_user_id} currentId={report.id} />
      )}

      {report.status === "open" ? (
        canWrite ? (
          <ResolveForm report={report} />
        ) : (
          <p className="rounded-md border border-border bg-surface p-4 text-sm text-text-muted">
            Deciding a report needs the <span className="font-identifier">moderation:write</span>{" "}
            scope.
          </p>
        )
      ) : (
        <Decision report={report} />
      )}
    </>
  );
}

/** How many of a person's other reports the report page lists before linking to the rest. */
const OTHER_REPORTS_SHOWN = 10;

/**
 * The reported person's other reports (`GET /reports?reported_user_id=`), open or closed, newest
 * first: whether this is a first report about them or the latest of many, and what was decided
 * the times before.
 */
function OtherReports({ userId, currentId }: { userId: string; currentId: string }) {
  const query = useReports({ reported_user_id: userId, limit: OTHER_REPORTS_SHOWN + 1 });
  const others = (query.data?.items ?? []).filter((r) => r.id !== currentId);
  const more = others.length > OTHER_REPORTS_SHOWN || Boolean(query.data?.next_cursor);
  return (
    <section aria-labelledby="other-reports-heading" className="space-y-2">
      <h2 id="other-reports-heading" className="text-md font-medium text-text">
        Other reports about <span className="break-all font-identifier">{userId}</span>
      </h2>
      {query.isLoading ? (
        <SkeletonText lines={2} />
      ) : query.isError ? (
        <QueryProblemState
          error={query.error}
          resource="their other reports"
          scope="moderation:read"
          onRetry={() => query.refetch()}
        />
      ) : others.length === 0 ? (
        <p className="text-sm text-text-muted">This is the only report about them.</p>
      ) : (
        <>
          <ul className="divide-y divide-border rounded-md border border-border bg-surface">
            {others.slice(0, OTHER_REPORTS_SHOWN).map((other) => (
              <li
                key={other.id}
                className="flex flex-wrap items-center justify-between gap-3 p-3 text-sm"
              >
                <Link
                  to="/reports/$reportId"
                  params={{ reportId: other.id }}
                  className="min-w-0 break-all font-medium text-text hover:text-accent hover:underline"
                >
                  {reportSubject(other)}
                </Link>
                <span className="flex items-center gap-3 text-text-muted">
                  <RelativeTime at={other.received_at} />
                  <Badge status={REPORT_STATUS_META[other.status].status}>
                    {other.status === "open"
                      ? REPORT_STATUS_META.open.label
                      : `${REPORT_STATUS_META[other.status].label}: ${resolutionLabel(other.resolution)}`}
                  </Badge>
                </span>
              </li>
            ))}
          </ul>
          {more && (
            <Link
              to="/reports"
              search={{ status: "all", reported_user_id: userId }}
              className="inline-block text-sm text-accent hover:underline"
            >
              Every report about them
            </Link>
          )}
        </>
      )}
    </section>
  );
}

function Fact({ label, children }: { label: string; children: ReactNode }) {
  return (
    <div>
      <dt>{label}</dt>
      <dd>{children}</dd>
    </div>
  );
}

function ReportedEvent({ report }: { report: Report }) {
  const event = report.event;
  const content = (event?.content ?? {}) as Record<string, unknown>;
  const body = typeof content.body === "string" ? content.body : null;
  return (
    <section aria-labelledby="reported-event-heading" className="space-y-2">
      <h2 id="reported-event-heading" className="text-md font-medium text-text">
        The reported message
      </h2>
      {!event ? (
        <p className="rounded-md border border-border bg-surface p-4 text-sm text-text-muted">
          This server does not hold the message, so it cannot show it. It may have come from a
          server this one no longer talks to, or have been purged.
        </p>
      ) : (
        <div className="rounded-md border border-border bg-surface p-4">
          <p className="flex flex-wrap items-center gap-x-3 gap-y-1 text-sm text-text-muted">
            {event.sender && <span className="font-identifier text-text">{event.sender}</span>}
            {event.origin_server_ts != null && (
              <RelativeTime at={new Date(event.origin_server_ts).toISOString()} />
            )}
            {event.type && <span className="font-identifier">{event.type}</span>}
          </p>
          {event.redacted ? (
            <p className="mt-2 flex items-center gap-2 text-sm text-text-muted">
              <Badge status="muted">Redacted</Badge>
              Its content has been removed.
            </p>
          ) : body !== null ? (
            <p className="mt-2 whitespace-pre-wrap break-words text-base text-text">{body}</p>
          ) : (
            <p className="mt-2 text-sm text-text-muted">This message has no text to show.</p>
          )}
        </div>
      )}
    </section>
  );
}

function ResolveForm({ report }: { report: Report }) {
  const resolve = useResolveReport();
  const [resolution, setResolution] = useState<ReportResolution | "">("");
  const [note, setNote] = useState("");
  const [error, setError] = useState("");
  const dismissing = resolution === "no_action";

  return (
    <section aria-labelledby="decide-heading" className="space-y-3">
      <h2 id="decide-heading" className="text-md font-medium text-text">
        Decide
      </h2>
      <form
        className="space-y-4 rounded-md border border-border bg-surface p-4"
        onSubmit={(event) => {
          event.preventDefault();
          if (!resolution) {
            setError("Choose what was done about this report.");
            return;
          }
          if (resolution === "other" && !note.trim()) {
            setError("Say in the note what was done.");
            return;
          }
          setError("");
          resolve.mutate(
            { id: report.id, resolution, note: note.trim() || undefined },
            {
              onSuccess: (closed) =>
                toast({
                  title: closed.status === "dismissed" ? "Report dismissed" : "Report resolved",
                }),
            },
          );
        }}
      >
        <fieldset>
          <legend className="text-sm font-medium text-text">What was done</legend>
          <div className="mt-2 grid gap-2 sm:grid-cols-2">
            {RESOLUTIONS.map((option) => (
              <label
                key={option.value}
                className="grid cursor-pointer grid-cols-[auto_1fr] gap-x-3 rounded-sm border border-border p-3 has-[:checked]:border-accent has-[:checked]:bg-accent-muted"
              >
                <input
                  type="radio"
                  name="resolution"
                  value={option.value}
                  checked={resolution === option.value}
                  onChange={() => setResolution(option.value)}
                  className="row-span-2 mt-1 accent-[var(--color-accent)]"
                />
                <span className="text-sm font-medium text-text">{option.label}</span>
                <span className="col-start-2 text-xs text-text-muted">{option.hint}</span>
              </label>
            ))}
          </div>
        </fieldset>
        <Field
          label="Note"
          hint="Kept on the report and in the audit log. Other moderators will read it."
        >
          {(props) => (
            <Textarea
              {...props}
              value={note}
              maxLength={2000}
              onChange={(event) => setNote(event.target.value)}
            />
          )}
        </Field>
        <p className="text-sm text-text-muted">
          Recording a decision does not act on its own. Lock or deactivate a person from their page,
          and block a room from its page, using the links above. A decided report is closed for
          good.
        </p>
        {error && (
          <p role="alert" className="text-sm text-danger">
            {error}
          </p>
        )}
        {resolve.isError && <MutationError error={resolve.error} action="save the decision" />}
        <Button type="submit" disabled={resolve.isPending}>
          {resolve.isPending ? "Saving…" : dismissing ? "Dismiss report" : "Resolve report"}
        </Button>
      </form>
    </section>
  );
}

function Decision({ report }: { report: Report }) {
  return (
    <section aria-labelledby="decision-heading" className="space-y-2">
      <h2 id="decision-heading" className="text-md font-medium text-text">
        Decision
      </h2>
      <dl className="grid gap-4 rounded-md border border-border bg-surface p-4 text-sm sm:grid-cols-2 [&_dd]:mt-1 [&_dd]:break-all [&_dd]:text-text [&_dt]:text-text-muted">
        <Fact label="What was done">{resolutionLabel(report.resolution)}</Fact>
        <Fact label="Decided">
          <RelativeTime at={report.resolved_at} />
        </Fact>
        {report.resolved_by && (
          <Fact label="Decided by">
            <ResourceLink target={{ type: "user", id: report.resolved_by }} />
          </Fact>
        )}
        <div className="sm:col-span-2">
          <dt>Note</dt>
          <dd className="whitespace-pre-wrap">
            {report.resolution_note ?? <span className="text-text-faint">No note.</span>}
          </dd>
        </div>
      </dl>
    </section>
  );
}

function DeleteReportButton({ report }: { report: Report }) {
  const remove = useDeleteReport();
  const navigate = useNavigate();
  return (
    <Dialog>
      <DialogTrigger asChild>
        <Button variant="secondary">Delete report</Button>
      </DialogTrigger>
      <DialogContent
        title="Delete this report?"
        description="The report is removed for good, as if it had never been filed. Use this for a report filed by mistake or to harass; to close one, decide it instead."
        footer={
          <>
            <DialogClose asChild>
              <Button variant="secondary">Cancel</Button>
            </DialogClose>
            <Button
              variant="danger"
              disabled={remove.isPending}
              onClick={() =>
                remove.mutate(report.id, {
                  onSuccess: () => {
                    toast({ title: "Report deleted" });
                    navigate({ to: "/reports" });
                  },
                })
              }
            >
              Delete report
            </Button>
          </>
        }
      >
        {remove.isError && <MutationError error={remove.error} action="delete the report" />}
      </DialogContent>
    </Dialog>
  );
}
