import { useState, type FormEvent, type ReactNode } from "react";
import { Link } from "@tanstack/react-router";
import {
  migrationIsRunning,
  useMigration,
  useMigrationControl,
  useMigrationLog,
  useMigrationSource,
  useMigrationTask,
  useSetMigrationSource,
  type MigrationStatus,
  type MigrationStream,
  type SynapseSource,
} from "@/api/migration";
import { MutationError } from "@/components/MutationError";
import { QueryProblemState } from "@/components/QueryProblemState";
import { RelativeTime } from "@/components/RelativeTime";
import { Badge } from "@/components/ui/badge/Badge";
import { Button } from "@/components/ui/button/Button";
import { Dialog, DialogClose, DialogContent } from "@/components/ui/dialog/Dialog";
import { ForbiddenState } from "@/components/ui/error-state/ErrorState";
import { Field, Input } from "@/components/ui/input/Input";
import { Skeleton } from "@/components/ui/skeleton/Skeleton";
import { toast } from "@/components/ui/toast/toast-store";
import { hasScope } from "@/lib/auth";
import {
  MIGRATION_STATUS_META,
  STREAM_LABELS,
  formatDuration,
  streamFraction,
} from "@/lib/migration";
import { TaskProgressBar } from "@/pages/tasks/TaskProgress";

/**
 * `/migration`: moving a Synapse deployment onto this server, from pointing at Synapse's
 * database to cutting over, without editing a file. Four steps, each unlocked by the one before:
 * the source, the copy (with pause, resume and abort), verification, and the cutover checklist.
 * The migration log is at the bottom. Synapse's database is only ever read, which is the whole
 * rollback story, and the page says so where it matters.
 */
export function MigrationPage() {
  const migration = useMigration();
  const source = useMigrationSource();

  if (!hasScope("admin:read")) {
    return (
      <div className="p-6">
        <h1 className="text-xl text-text">Migration from Synapse</h1>
        <ForbiddenState scope="admin:read" />
      </div>
    );
  }

  const status = migration.data;
  const phase = status?.status ?? "idle";
  const meta = MIGRATION_STATUS_META[phase];

  return (
    <div className="mx-auto max-w-[72rem] space-y-8 p-6">
      <div className="flex flex-wrap items-start justify-between gap-4">
        <div>
          <h1 className="text-xl text-text">Migration from Synapse</h1>
          <p className="mt-1 max-w-3xl text-sm text-text-muted">
            Copies a Synapse server&apos;s accounts, sessions, rooms, account data and media into
            this one, while Synapse keeps running. Synapse&apos;s database is only ever read: until
            you cut over, rolling back is simply carrying on with Synapse.
          </p>
        </div>
        {status && (
          <Badge status={meta.status} className="mt-1">
            {meta.label}
          </Badge>
        )}
      </div>

      {migration.isError ? (
        <QueryProblemState
          error={migration.error}
          resource="migration"
          scope="admin:read"
          onRetry={() => migration.refetch()}
        />
      ) : !status ? (
        <Skeleton className="h-40 w-full" />
      ) : (
        <>
          {status.errors && status.errors.length > 0 && phase !== "completed" && (
            <div
              role="alert"
              className="rounded-md border border-danger-border bg-danger-bg p-3 text-sm text-text"
            >
              <p className="font-medium">The last run stopped on an error</p>
              <ul className="mt-1 list-disc pl-5">
                {status.errors.slice(-3).map((e) => (
                  <li key={e}>{e}</li>
                ))}
              </ul>
            </div>
          )}
          <SourceStep status={status} source={source.data ?? null} loading={source.isLoading} />
          <CopyStep status={status} sourceSet={Boolean(source.data)} />
          <VerifyStep status={status} />
          <CutoverStep status={status} />
          <LogSection />
        </>
      )}
    </div>
  );
}

function Step({
  number,
  title,
  done,
  children,
  id,
}: {
  number: number;
  title: string;
  done?: boolean;
  children: ReactNode;
  id: string;
}) {
  return (
    <section
      aria-labelledby={`${id}-heading`}
      className="rounded-md border border-border bg-surface p-5"
    >
      <h2 id={`${id}-heading`} className="flex items-center gap-3 text-lg text-text">
        <span
          aria-hidden="true"
          className="flex h-7 w-7 items-center justify-center rounded-full bg-surface-sunken text-sm"
        >
          {number}
        </span>
        {title}
        {done && (
          <Badge status="success" className="ml-1">
            Done
          </Badge>
        )}
      </h2>
      <div className="mt-4 space-y-4">{children}</div>
    </section>
  );
}

const EDITABLE_PHASES = new Set(["idle", "failed", "aborted"]);

function SourceStep({
  status,
  source,
  loading,
}: {
  status: MigrationStatus;
  source: SynapseSource | null;
  loading: boolean;
}) {
  const [editing, setEditing] = useState(false);
  const editable = EDITABLE_PHASES.has(status.status ?? "idle") && hasScope("admin:write");
  return (
    <Step number={1} id="source" title="Point at Synapse" done={Boolean(source) && !editing}>
      {loading ? (
        <Skeleton className="h-24 w-full" />
      ) : source && !editing ? (
        <div className="flex flex-wrap items-start justify-between gap-4">
          <dl className="grid gap-x-8 gap-y-2 text-sm sm:grid-cols-2">
            <Fact label="Database">
              <code>
                postgresql://{source.user}@{source.host}:{source.port}/{source.database}
              </code>
            </Fact>
            <Fact label="Password">{source.passwordSet ? "Set, hidden" : "None"}</Fact>
            <Fact label="Media store">
              {source.mediaStorePath ? (
                <code>{source.mediaStorePath}</code>
              ) : (
                "Not mounted: media records are copied without their files"
              )}
            </Fact>
            <Fact label="Rows per batch">{source.batchSize.toLocaleString()}</Fact>
          </dl>
          {editable && (
            <Button variant="secondary" onClick={() => setEditing(true)}>
              Change source
            </Button>
          )}
        </div>
      ) : editable ? (
        <SourceForm source={source} onDone={() => setEditing(false)} />
      ) : (
        <p className="text-sm text-text-muted">
          No Synapse database is set, and you need <code>admin:write</code> to set one.
        </p>
      )}
    </Step>
  );
}

function Fact({ label, children }: { label: string; children: ReactNode }) {
  return (
    <div>
      <dt className="text-text-muted">{label}</dt>
      <dd className="text-text">{children}</dd>
    </div>
  );
}

function SourceForm({ source, onDone }: { source: SynapseSource | null; onDone: () => void }) {
  const save = useSetMigrationSource();
  const [host, setHost] = useState(source?.host ?? "");
  const [port, setPort] = useState(String(source?.port ?? 5432));
  const [database, setDatabase] = useState(source?.database ?? "synapse");
  const [user, setUser] = useState(source?.user ?? "synapse");
  const [password, setPassword] = useState("");
  const [mediaStorePath, setMediaStorePath] = useState(source?.mediaStorePath ?? "");
  const [batchSize, setBatchSize] = useState(String(source?.batchSize ?? 500));

  function submit(event: FormEvent) {
    event.preventDefault();
    save.mutate(
      {
        host: host.trim(),
        port: Number(port),
        database: database.trim(),
        user: user.trim(),
        password: password === "" && source?.passwordSet ? undefined : password,
        mediaStorePath: mediaStorePath.trim() === "" ? null : mediaStorePath.trim(),
        batchSize: Number(batchSize),
      },
      {
        onSuccess: () => {
          toast({ title: "Synapse source saved" });
          onDone();
        },
      },
    );
  }

  return (
    <form onSubmit={submit} className="space-y-4" aria-label="Synapse source">
      <p className="text-sm text-text-muted">
        Synapse&apos;s PostgreSQL database: the <code>database.args</code> of its{" "}
        <code>homeserver.yaml</code>. A read-only role is enough. A Synapse on SQLite is moved to
        PostgreSQL first, with Synapse&apos;s own <code>synapse_port_db</code>.
      </p>
      <div className="grid gap-4 sm:grid-cols-2">
        <Field label="Host" required>
          {(p) => <Input {...p} value={host} onChange={(e) => setHost(e.target.value)} />}
        </Field>
        <Field label="Port" required>
          {(p) => (
            <Input
              {...p}
              type="number"
              min={1}
              max={65535}
              value={port}
              onChange={(e) => setPort(e.target.value)}
            />
          )}
        </Field>
        <Field label="Database" required>
          {(p) => <Input {...p} value={database} onChange={(e) => setDatabase(e.target.value)} />}
        </Field>
        <Field label="User" required>
          {(p) => <Input {...p} value={user} onChange={(e) => setUser(e.target.value)} />}
        </Field>
        <Field
          label="Password"
          hint={
            source?.passwordSet
              ? "A password is stored. Leave this empty to keep it."
              : "Stored with the configuration and never shown again."
          }
        >
          {(p) => (
            <Input
              {...p}
              type="password"
              autoComplete="new-password"
              value={password}
              onChange={(e) => setPassword(e.target.value)}
            />
          )}
        </Field>
        <Field
          label="Media store path"
          hint="Synapse's media_store_path, as this server sees it (the same volume, mounted). Leave empty to copy media records without their files."
        >
          {(p) => (
            <Input
              {...p}
              value={mediaStorePath}
              placeholder="/var/lib/synapse/media_store"
              onChange={(e) => setMediaStorePath(e.target.value)}
            />
          )}
        </Field>
        <Field label="Rows per batch" hint="Each batch is a point the copy can pause at.">
          {(p) => (
            <Input
              {...p}
              type="number"
              min={1}
              max={100000}
              value={batchSize}
              onChange={(e) => setBatchSize(e.target.value)}
            />
          )}
        </Field>
      </div>
      {save.isError && <MutationError error={save.error} action="save the source" />}
      <div className="flex gap-2">
        <Button type="submit" disabled={save.isPending || !host || !database || !user}>
          Save source
        </Button>
        {source && (
          <Button type="button" variant="secondary" onClick={onDone}>
            Cancel
          </Button>
        )}
      </div>
    </form>
  );
}

function CopyStep({ status, sourceSet }: { status: MigrationStatus; sourceSet: boolean }) {
  const start = useMigrationControl("start");
  const pause = useMigrationControl("pause");
  const resume = useMigrationControl("resume");
  const [aborting, setAborting] = useState(false);
  const phase = status.status ?? "idle";
  const canWrite = hasScope("admin:write");
  const streams = status.streams ?? [];
  const copied = streams.reduce((n, s) => n + (s.copied_count ?? 0), 0);
  const total = streams.reduce((n, s) => n + (s.total_count ?? 0), 0);
  const failed = start.isError ? start : pause.isError ? pause : resume.isError ? resume : null;
  const done = ["ready_for_cutover", "verifying", "cutting_over", "completed"].includes(phase);

  return (
    <Step number={2} id="copy" title="Copy, while Synapse keeps running" done={done}>
      {phase === "idle" || phase === "failed" || phase === "aborted" ? (
        <p className="text-sm text-text-muted">
          {phase === "idle"
            ? "Accounts with their password hashes, devices and access tokens (so nobody has to sign in again), account data, rooms with their whole history, and media. Nothing is written to Synapse."
            : phase === "aborted"
              ? "The migration was aborted. What was copied stays; starting again carries on from it."
              : "The copy stopped on an error. Starting it again carries on from where it stopped."}
        </p>
      ) : (
        <p className="text-sm text-text-muted">
          From <code>{status.source}</code>
          {status.started_at && (
            <>
              , started <RelativeTime at={status.started_at} />
              {status.started_by ? ` by ${status.started_by}` : ""}
            </>
          )}
          .
          {phase === "copying" && status.estimated_remaining_ms != null && (
            <> About {formatDuration(status.estimated_remaining_ms)} left.</>
          )}
        </p>
      )}
      {streams.length > 0 && (
        <>
          {phase === "copying" && (
            <TaskProgressBar
              fraction={total > 0 ? Math.min(1, copied / total) : null}
              label={`${copied.toLocaleString()} of ${total.toLocaleString()} rows copied`}
            />
          )}
          <StreamsTable streams={streams} />
        </>
      )}
      {canWrite && (
        <div className="flex flex-wrap gap-2">
          {(phase === "idle" || phase === "failed" || phase === "aborted") && (
            <Button
              disabled={!sourceSet || start.isPending}
              onClick={() =>
                start.mutate(undefined, {
                  onSuccess: () => toast({ title: "Copying from Synapse" }),
                })
              }
            >
              {phase === "idle" ? "Start copying" : "Start again"}
            </Button>
          )}
          {phase === "copying" && (
            <Button variant="secondary" disabled={pause.isPending} onClick={() => pause.mutate()}>
              Pause
            </Button>
          )}
          {phase === "paused" && (
            <Button disabled={resume.isPending} onClick={() => resume.mutate()}>
              Resume
            </Button>
          )}
          {!["idle", "aborted", "completed"].includes(phase) && (
            <Button variant="danger" onClick={() => setAborting(true)}>
              Abort migration
            </Button>
          )}
        </div>
      )}
      {!sourceSet && phase === "idle" && (
        <p className="text-sm text-text-muted">Point at Synapse first (step 1).</p>
      )}
      {failed && <MutationError error={failed.error} action="do that" />}
      <AbortDialog open={aborting} onClose={() => setAborting(false)} />
    </Step>
  );
}

function StreamsTable({ streams }: { streams: MigrationStream[] }) {
  return (
    <table className="w-full text-sm" aria-label="What has been copied">
      <thead>
        <tr className="border-b border-border text-left text-text-muted">
          <th scope="col" className="py-2 font-medium">
            What
          </th>
          <th scope="col" className="py-2 text-right font-medium">
            Copied
          </th>
          <th scope="col" className="hidden py-2 text-right font-medium sm:table-cell">
            Not copied
          </th>
          <th scope="col" className="hidden py-2 text-right font-medium sm:table-cell">
            Failed
          </th>
          <th scope="col" className="hidden py-2 text-right font-medium md:table-cell">
            Rate
          </th>
        </tr>
      </thead>
      <tbody>
        {streams.map((s) => {
          const fraction = streamFraction(s);
          return (
            <tr key={s.name} className="border-b border-border last:border-0">
              <th scope="row" className="py-2 text-left font-normal text-text">
                <div className="flex items-center gap-2">
                  {STREAM_LABELS[s.name ?? ""] ?? s.name}
                  {s.done && (
                    <Badge status="success" hideIcon>
                      Done
                    </Badge>
                  )}
                </div>
                {!s.done && fraction != null && (
                  <div className="mt-1 max-w-48">
                    <TaskProgressBar fraction={fraction} label={`${s.name} progress`} compact />
                  </div>
                )}
              </th>
              <td className="py-2 text-right tabular-nums">
                {(s.copied_count ?? 0).toLocaleString()}
                {s.total_count != null && (
                  <span className="text-text-muted"> of {s.total_count.toLocaleString()}</span>
                )}
              </td>
              <td className="hidden py-2 text-right tabular-nums sm:table-cell">
                {(s.skipped_count ?? 0).toLocaleString()}
              </td>
              <td
                className={
                  "hidden py-2 text-right tabular-nums sm:table-cell" +
                  ((s.failed_count ?? 0) > 0 ? " text-danger" : "")
                }
              >
                {(s.failed_count ?? 0).toLocaleString()}
              </td>
              <td className="hidden py-2 text-right tabular-nums text-text-muted md:table-cell">
                {s.rate_per_second ? `${Math.round(s.rate_per_second).toLocaleString()}/s` : "—"}
              </td>
            </tr>
          );
        })}
      </tbody>
    </table>
  );
}

function AbortDialog({ open, onClose }: { open: boolean; onClose: () => void }) {
  const abort = useMigrationControl("abort");
  function close() {
    abort.reset();
    onClose();
  }
  return (
    <Dialog open={open} onOpenChange={(o) => !o && close()}>
      {open && (
        <DialogContent
          title="Abort the migration?"
          description="The copy stops, and the migration is marked aborted."
          footer={
            <>
              <DialogClose asChild>
                <Button variant="secondary">Keep going</Button>
              </DialogClose>
              <Button
                variant="danger"
                disabled={abort.isPending}
                onClick={() =>
                  abort.mutate(undefined, {
                    onSuccess: () => {
                      toast({ title: "Migration aborted" });
                      close();
                    },
                  })
                }
              >
                Abort migration
              </Button>
            </>
          }
        >
          <ul className="list-disc space-y-2 pl-5 text-sm text-text">
            <li>Synapse was only ever read, so it is exactly as it was: keep running it.</li>
            <li>
              What was already copied stays here. Starting again later carries on from it rather
              than copying everything twice.
            </li>
          </ul>
          {abort.isError && <MutationError error={abort.error} action="abort" />}
        </DialogContent>
      )}
    </Dialog>
  );
}

function VerifyStep({ status }: { status: MigrationStatus }) {
  const verify = useMigrationTask("verify");
  const phase = status.status ?? "idle";
  const report = status.verification;
  const allowed = ["ready_for_cutover", "paused", "failed", "completed"].includes(phase);
  return (
    <Step
      number={3}
      id="verify"
      title="Verify"
      done={Boolean(report?.passed) && phase !== "verifying"}
    >
      <p className="text-sm text-text-muted">
        Counts every row in Synapse and checks each one is here, then compares samples field by
        field: password hashes, profiles, which account each token signs in, every room&apos;s
        current state, and media files byte for byte. Run it as often as you like.
      </p>
      {phase === "verifying" && <TaskProgressBar fraction={null} label="Verifying" />}
      {report && (
        <div className="space-y-2">
          <p className="text-sm text-text">
            <Badge status={report.passed ? "success" : "danger"}>
              {report.passed ? "Everything matches" : "Differences found"}
            </Badge>{" "}
            <span className="text-text-muted">
              checked <RelativeTime at={report.checked_at ?? ""} />
            </span>
          </p>
          <table className="w-full text-sm" aria-label="Verification">
            <thead>
              <tr className="border-b border-border text-left text-text-muted">
                <th scope="col" className="py-2 font-medium">
                  What
                </th>
                <th scope="col" className="py-2 text-right font-medium">
                  Here / in Synapse
                </th>
                <th scope="col" className="hidden py-2 text-right font-medium sm:table-cell">
                  Not copied on purpose
                </th>
                <th scope="col" className="py-2 pl-4 font-medium">
                  Differences
                </th>
              </tr>
            </thead>
            <tbody>
              {(report.streams ?? []).map((s) => {
                const ok = s.source_count === s.target_count && (s.mismatches ?? []).length === 0;
                return (
                  <tr key={s.name} className="border-b border-border align-top last:border-0">
                    <th scope="row" className="py-2 text-left font-normal">
                      {STREAM_LABELS[s.name ?? ""] ?? s.name}
                    </th>
                    <td className={"py-2 text-right tabular-nums" + (ok ? "" : " text-danger")}>
                      {(s.target_count ?? 0).toLocaleString()} /{" "}
                      {(s.source_count ?? 0).toLocaleString()}
                    </td>
                    <td className="hidden py-2 text-right tabular-nums sm:table-cell">
                      {(s.skipped_count ?? 0).toLocaleString()}
                    </td>
                    <td className="py-2 pl-4">
                      {(s.mismatches ?? []).length === 0 ? (
                        <span className="text-text-muted">None</span>
                      ) : (
                        <ul className="list-disc pl-4 text-danger">
                          {(s.mismatches ?? []).map((m) => (
                            <li key={m}>{m}</li>
                          ))}
                        </ul>
                      )}
                    </td>
                  </tr>
                );
              })}
            </tbody>
          </table>
        </div>
      )}
      {hasScope("admin:write") && (
        <Button
          variant={report?.passed ? "secondary" : "primary"}
          disabled={!allowed || verify.isPending}
          onClick={() =>
            verify.mutate(undefined, {
              onSuccess: () => toast({ title: "Verifying the copy" }),
            })
          }
        >
          {report ? "Verify again" : "Verify"}
        </Button>
      )}
      {verify.isError && <MutationError error={verify.error} action="verify" />}
    </Step>
  );
}

const CHECKLIST = [
  {
    id: "stopped",
    label: "Synapse is stopped",
    detail:
      "Stop every Synapse process and worker (for example `systemctl stop matrix-synapse`, or scale its deployment to zero). The cutover copies whatever changed since the copy, so nothing written after it would be carried over.",
  },
  {
    id: "routing",
    label: "Clients and other servers will reach this server",
    detail:
      "Point the DNS name or ingress that served Synapse (and its .well-known delegation) at this server once the cutover has finished.",
  },
] as const;

function CutoverStep({ status }: { status: MigrationStatus }) {
  const cutover = useMigrationTask("cutover");
  const [ticked, setTicked] = useState<Record<string, boolean>>({});
  const [confirming, setConfirming] = useState(false);
  const phase = status.status ?? "idle";
  const allTicked = CHECKLIST.every((c) => ticked[c.id]);

  if (phase === "completed") {
    return (
      <Step number={4} id="cutover" title="Cut over" done>
        <p className="text-sm text-text">
          Cut over{status.cutover_by ? ` by ${status.cutover_by}` : ""}
          {status.completed_at && (
            <>
              {" "}
              <RelativeTime at={status.completed_at} />
            </>
          )}
          . This server is the one in service. Keep Synapse stopped: starting it again would mean
          two servers answering for the same name.
        </p>
        <p className="text-sm text-text-muted">
          Not carried over by this version: end-to-end encryption keys and key backups (clients
          upload them again), push rules and pushers, read receipts, cached remote media, and rooms
          your users had joined on other servers (they rejoin). See the{" "}
          <Link to="/users" className="text-accent underline underline-offset-2">
            users
          </Link>{" "}
          and{" "}
          <Link to="/rooms" className="text-accent underline underline-offset-2">
            rooms
          </Link>{" "}
          that were copied.
        </p>
      </Step>
    );
  }

  return (
    <Step number={4} id="cutover" title="Cut over">
      <p className="text-sm text-text-muted">
        Once everything has been copied: stop Synapse, then cut over. The cutover makes a final pass
        over Synapse&apos;s database (picking up what changed since the copy), verifies, and only
        finishes if verification passes. If it does not, nothing is cut over, and Synapse can be
        started again.
      </p>
      {phase === "cutting_over" && (
        <TaskProgressBar fraction={null} label="Cutting over: final pass, then verification" />
      )}
      <fieldset className="space-y-3" disabled={phase !== "ready_for_cutover"}>
        <legend className="text-sm font-medium text-text">Before you cut over</legend>
        {CHECKLIST.map((item) => (
          <div key={item.id} className="flex items-start gap-3 text-sm">
            <input
              id={`cutover-${item.id}`}
              type="checkbox"
              className="mt-1"
              aria-describedby={`cutover-${item.id}-detail`}
              checked={Boolean(ticked[item.id])}
              onChange={(e) => setTicked({ ...ticked, [item.id]: e.target.checked })}
            />
            <div>
              <label htmlFor={`cutover-${item.id}`} className="text-text">
                {item.label}
              </label>
              <p id={`cutover-${item.id}-detail`} className="text-text-muted">
                {item.detail}
              </p>
            </div>
          </div>
        ))}
      </fieldset>
      {hasScope("admin:write") && (
        <Button
          disabled={phase !== "ready_for_cutover" || !allTicked || cutover.isPending}
          onClick={() => setConfirming(true)}
        >
          Cut over
        </Button>
      )}
      {cutover.isError && <MutationError error={cutover.error} action="cut over" />}
      <Dialog open={confirming} onOpenChange={(o) => !o && setConfirming(false)}>
        {confirming && (
          <DialogContent
            title="Cut over to this server?"
            description="A final pass over Synapse's database, then verification. If verification passes, the migration is finished."
            footer={
              <>
                <DialogClose asChild>
                  <Button variant="secondary">Not yet</Button>
                </DialogClose>
                <Button
                  onClick={() =>
                    cutover.mutate(undefined, {
                      onSuccess: () => {
                        toast({
                          title: "Cutting over",
                          description: "This page follows it; it ends in a verification.",
                        });
                        setConfirming(false);
                      },
                    })
                  }
                  disabled={cutover.isPending}
                >
                  Cut over now
                </Button>
              </>
            }
          >
            <p className="text-sm text-text">
              After this, keep Synapse stopped. Anything written to Synapse after the cutover is not
              carried over.
            </p>
          </DialogContent>
        )}
      </Dialog>
    </Step>
  );
}

const LOG_LIMIT = 100;

function LogSection() {
  const log = useMigrationLog(LOG_LIMIT);
  const running = migrationIsRunning(useMigration().data);
  return (
    <section aria-labelledby="log-heading" className="space-y-3">
      <h2 id="log-heading" className="text-lg text-text">
        Log
      </h2>
      {log.isError ? (
        <QueryProblemState
          error={log.error}
          resource="migration log"
          scope="admin:read"
          onRetry={() => log.refetch()}
        />
      ) : !log.data ? (
        <Skeleton className="h-24 w-full" />
      ) : log.data.items.length === 0 ? (
        <p className="text-sm text-text-muted">Nothing has happened yet.</p>
      ) : (
        <>
          {log.data.total > LOG_LIMIT && (
            <p className="text-sm text-text-muted">
              The most recent {LOG_LIMIT} of {log.data.total.toLocaleString()} entries
              {running ? ", following along" : ""}.
            </p>
          )}
          <ol className="divide-y divide-border rounded-md border border-border bg-surface text-sm">
            {log.data.items.map((entry, i) => (
              <li key={`${entry.recorded_at}-${i}`} className="flex gap-3 p-2">
                <Badge
                  status={
                    entry.level === "error"
                      ? "danger"
                      : entry.level === "warning"
                        ? "warning"
                        : "info"
                  }
                  className="shrink-0 self-start"
                >
                  {entry.stream}
                </Badge>
                <span className="flex-1 text-text">{entry.message}</span>
                <span className="shrink-0 text-text-muted">
                  <RelativeTime at={entry.recorded_at ?? ""} />
                </span>
              </li>
            ))}
          </ol>
        </>
      )}
    </section>
  );
}
