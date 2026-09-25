import { Link, useParams, useSearch } from "@tanstack/react-router";
import { useAuditEntry } from "@/api/audit";
import { CopyBlock } from "@/components/CopyBlock";
import { CopyableId } from "@/components/CopyableId";
import { QueryProblemState } from "@/components/QueryProblemState";
import { ResourceLink } from "@/components/ResourceLink";
import { Badge } from "@/components/ui/badge/Badge";
import { ForbiddenState } from "@/components/ui/error-state/ErrorState";
import { SkeletonText } from "@/components/ui/skeleton/Skeleton";
import { hasScope } from "@/lib/auth";
import { actorKindLabel, describeAction, succeeded, targetTypeLabel } from "@/lib/audit";

export function AuditEntryPage() {
  const { entryId } = useParams({ from: "/audit/$entryId" });
  const search = useSearch({ from: "/audit/$entryId" });
  const query = useAuditEntry(entryId);
  const entry = query.data;

  return (
    <div className="mx-auto max-w-5xl space-y-5 p-6">
      <Link to="/audit" search={search} className="text-sm text-accent hover:underline">
        Back to audit log
      </Link>
      {!hasScope("admin:read") ? (
        <ForbiddenState scope="admin:read" />
      ) : query.isLoading ? (
        <SkeletonText lines={6} />
      ) : query.isError || !entry ? (
        <QueryProblemState
          error={query.error}
          resource="audit entry"
          onRetry={() => query.refetch()}
        />
      ) : (
        <>
          <header>
            <h1 className="text-xl text-text">{describeAction(entry.action)}</h1>
            <p className="mt-1 break-all font-identifier text-sm text-text-muted">{entry.action}</p>
            <div className="mt-3 flex flex-wrap gap-2">
              <Badge status={succeeded(entry) ? "success" : "danger"}>
                {succeeded(entry) ? "Succeeded" : "Failed"}
                {entry.outcome.status ? ` · HTTP ${entry.outcome.status}` : ""}
              </Badge>
              {entry.replayed && <Badge status="info">Replayed request</Badge>}
            </div>
          </header>
          {entry.outcome.problem && (
            <div role="alert" className="rounded-md border border-danger p-4 text-sm text-text">
              {entry.outcome.problem.detail ?? entry.outcome.problem.title}
            </div>
          )}
          <dl className="grid gap-4 rounded-md border border-border bg-surface p-4 text-sm sm:grid-cols-2 [&_dt]:text-text-muted [&_dd]:mt-1 [&_dd]:break-all [&_dd]:text-text">
            <div>
              <dt>Recorded at</dt>
              <dd>
                <time dateTime={entry.recorded_at}>{entry.recorded_at}</time>
              </dd>
            </div>
            <div>
              <dt>Entry ID</dt>
              <dd>
                <CopyableId value={entry.id} />
              </dd>
            </div>
            <div>
              <dt>{actorKindLabel(entry.actor.kind)}</dt>
              <dd>
                {entry.actor.kind === "user" ? (
                  <ResourceLink target={{ type: "user", id: entry.actor.id }} />
                ) : (
                  entry.actor.id
                )}
                {entry.actor.display_name && (
                  <span className="block">{entry.actor.display_name}</span>
                )}
              </dd>
            </div>
            <div>
              <dt>{targetTypeLabel(entry.target.type)}</dt>
              <dd>
                <ResourceLink target={entry.target} />
              </dd>
            </div>
            {entry.actor.ip && (
              <div>
                <dt>IP address</dt>
                <dd>{entry.actor.ip}</dd>
              </div>
            )}
            {entry.actor.user_agent && (
              <div>
                <dt>Client</dt>
                <dd>{entry.actor.user_agent}</dd>
              </div>
            )}
            {entry.actor.token_id && (
              <div>
                <dt>Token ID</dt>
                <dd>{entry.actor.token_id}</dd>
              </div>
            )}
            {entry.request?.request_id && (
              <div>
                <dt>Request ID</dt>
                <dd>
                  <CopyableId value={entry.request.request_id} />
                </dd>
              </div>
            )}
            {entry.request?.path && (
              <div>
                <dt>Request</dt>
                <dd>
                  {entry.request.method} {entry.request.path}
                </dd>
              </div>
            )}
            {entry.replay_of && (
              <div>
                <dt>Original request</dt>
                <dd>
                  <Link
                    to="/audit/$entryId"
                    params={{ entryId: entry.replay_of }}
                    search={search}
                    className="text-accent hover:underline"
                  >
                    {entry.replay_of}
                  </Link>
                </dd>
              </div>
            )}
          </dl>
          <section aria-labelledby="audit-changes-heading">
            <h2 id="audit-changes-heading" className="mb-3 text-md font-medium text-text">
              Changes
            </h2>
            {entry.changes?.length ? (
              <ul className="space-y-3">
                {entry.changes.map((change, index) => (
                  <li key={index} className="rounded-md border border-border bg-surface p-4">
                    <h3 className="break-all font-identifier text-sm text-text">
                      {change.pointer ?? "Value"}
                    </h3>
                    <dl className="mt-2 grid gap-3 text-sm sm:grid-cols-2">
                      <div>
                        <dt className="text-text-muted">Before</dt>
                        <dd>
                          <pre className="whitespace-pre-wrap break-all text-text">
                            {JSON.stringify(change.from, null, 2) ?? "Not recorded"}
                          </pre>
                        </dd>
                      </div>
                      <div>
                        <dt className="text-text-muted">After</dt>
                        <dd>
                          <pre className="whitespace-pre-wrap break-all text-text">
                            {JSON.stringify(change.to, null, 2) ?? "Not recorded"}
                          </pre>
                        </dd>
                      </div>
                    </dl>
                  </li>
                ))}
              </ul>
            ) : (
              <p className="text-sm text-text-muted">
                No field changes were recorded for this action.
              </p>
            )}
          </section>
          <CopyBlock
            label="Full audit entry (JSON)"
            content={JSON.stringify(entry, null, 2)}
            filename={`audit-${entry.id}.json`}
          />
        </>
      )}
    </div>
  );
}
