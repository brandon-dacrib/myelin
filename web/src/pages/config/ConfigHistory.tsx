/**
 * Who changed this section, and when.
 *
 * `crates/hs-config/src/store.rs` keeps a `history/<revision>` record per
 * write "so the web interface can show what changed and when", but no
 * operation in `crates/hs-admin/openapi/openapi.yaml` exposes it — there is
 * no `GET /config/{section}/history`. What the API does expose is the audit
 * log, which records configuration writes under the `config_section`
 * resource type, so that is what this reads. The trade is that the audit
 * entry says *that* the section changed and by whom, not *which settings*;
 * the per-setting diff would need the store's own history endpoint.
 */
import { History } from "lucide-react";
import { useConfigHistory } from "@/api/config";
import { QueryProblemState } from "@/components/QueryProblemState";
import { RelativeTime } from "@/components/RelativeTime";
import { Badge } from "@/components/ui/badge/Badge";
import { SkeletonText } from "@/components/ui/skeleton/Skeleton";

export function ConfigHistory({ section }: { section: string }) {
  const { data, isLoading, isError, error, refetch } = useConfigHistory(section);

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
      ) : (data?.length ?? 0) === 0 ? (
        <p className="mt-3 text-sm text-text-muted">
          Nothing has changed this section since the audit log started.
        </p>
      ) : (
        <ul className="mt-3 divide-y divide-border rounded-md border border-border">
          {data?.map((entry) => {
            const failed = (entry.outcome?.status ?? 200) >= 400;
            return (
              <li key={entry.id} className="flex flex-wrap items-center gap-x-3 gap-y-1 px-4 py-3">
                <span className="font-identifier text-sm text-text">
                  {entry.actor?.display_name ?? entry.actor?.id ?? "unknown"}
                </span>
                <span className="text-sm text-text-muted">{entry.action}</span>
                {failed && <Badge status="danger">Rejected</Badge>}
                <span className="ml-auto text-xs text-text-muted">
                  <RelativeTime at={entry.recorded_at} />
                </span>
              </li>
            );
          })}
        </ul>
      )}
    </section>
  );
}
